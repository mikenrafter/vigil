//! Best-effort completion history. Filesystem work runs outside keepalive's executor.
use std::io::{self, Write};
use std::path::Path;

use crate::engine::Vigil;
use crate::watch::{Input, WatchFile};

impl Vigil {
    /// Called under the watch lock before history compaction. The saved flag
    /// prevents later ticks, retries and cancellation from recording an item again.
    pub(crate) fn record_items(&self, watch: &mut WatchFile, terminal: Option<&str>) -> Batch {
        let mut batch = Batch { path: self.store.root().join("items.jsonl"), rows: Vec::new() };
        let session = watch.owner_session.clone().or_else(|| self.agent.claude_session_id.clone());
        let Some(items) = watch.items.as_mut() else { return batch };
        for item in items {
            if item.ledger_recorded || (item.ended_at.is_none() && terminal.is_none()) { continue; }
            let input = item.input.and_then(|index| watch.spec.inputs.get(index));
            let kind = match input {
                Some(Input::Process { .. }) => "pid",
                Some(Input::PathExists { .. }) => "file",
                Some(Input::Qex { .. }) => "qex",
                Some(Input::QexIdle { .. }) => "idle",
                Some(Input::Until { .. }) => "until",
                None => "none",
                Some(_) => "other",
            };
            let outcome = if item.ended_at.is_none() {
                terminal.unwrap_or("released")
            } else if item.end_reading.as_deref() == Some("done") {
                "done"
            } else {
                match kind { "file" => "report", "pid" | "qex" => "exited", _ => "released" }
            };
            let row = serde_json::json!({
                "v": 1, "hold": watch.spec.id, "label": item.label,
                "spec_kind": kind, "expect_s": item.expect_s,
                "started_at": item.added_at,
                "ended_at": item.ended_at.or(watch.finished_at).unwrap_or_else(|| self.clock.now()),
                "outcome": outcome, "overdue_noted": item.overdue_raised,
                "session_id": session, "cwd": item.cwd.as_ref().unwrap_or(&self.cwd), "meta": item.meta,
            });
            item.ledger_recorded = true;
            batch.rows.push(row);
        }
        batch
    }
}

pub(crate) struct Batch { path: std::path::PathBuf, rows: Vec<serde_json::Value> }

impl Batch {
    pub(crate) fn publish(self) {
        if self.rows.is_empty() { return; }
        tokio::task::spawn_blocking(move || {
            for row in self.rows {
                if let Err(error) = append(&self.path, &row) {
                    eprintln!("vigil: item ledger {}: {error}", self.path.display());
                }
            }
        });
    }
}

fn append(path: &Path, row: &serde_json::Value) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = serde_json::to_vec(row)?;
    bytes.push(b'\n');
    const O_NONBLOCK: i32 = 0o4000;
    // O_NONBLOCK keeps a misplaced FIFO from holding a blocking-pool worker.
    let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600)
        .custom_flags(O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() { return Err(io::Error::other("ledger is not a regular file")); }
    // One O_APPEND write per record; never retry a partial write as a second fragment.
    if file.write(&bytes)? != bytes.len() { return Err(io::Error::from(io::ErrorKind::WriteZero)); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hold::HoldRequest;
    use crate::hold_tests::{rig, watch_file};
    use crate::testing::at;

    #[tokio::test(start_paused = true)]
    async fn first_end_survives_serialization_and_sensed_retry() {
        // why: sensed retries clear ended_at; that must not clear ledger identity or overdue attribution.
        // catches: using ended_at alone for deduplication or dropping the flag on reload.
        let env = rig();
        env.vigil.hold(HoldRequest { id: Some("h".into()), work: vec!["pid:1".into()], label: Some("build".into()), expect_s: Some(60), ..Default::default() }).await.unwrap();
        let mut watch = watch_file(&env.vigil, "h");
        watch.owner_session = Some("session-1".into());
        let item = &mut watch.items.as_mut().unwrap()[0];
        item.overdue_raised = true;
        item.ended_at = Some(at("2026-10-29T14:05:11Z"));
        item.end_reading = Some("exited".into());
        let first = env.vigil.record_items(&mut watch, None);
        assert_eq!(first.rows.len(), 1);
        assert_eq!(first.rows[0]["overdue_noted"], true);
        assert_eq!(first.rows[0]["session_id"], "session-1");
        assert_eq!(first.rows[0]["started_at"], "2026-10-29T14:03:11Z");
        assert_eq!(first.rows[0]["ended_at"], "2026-10-29T14:05:11Z");
        let mut reloaded: WatchFile = serde_json::from_slice(&serde_json::to_vec(&watch).unwrap()).unwrap();
        reloaded.items.as_mut().unwrap()[0].ended_at = None;
        assert!(env.vigil.record_items(&mut reloaded, Some("cancelled")).rows.is_empty());
        reloaded.items.as_mut().unwrap()[0].ended_at = Some(at("2026-10-29T14:06:11Z"));
        assert!(env.vigil.record_items(&mut reloaded, None).rows.is_empty());
    }

    #[test]
    fn concurrent_appenders_keep_large_escaped_rows_intact() {
        // why: separate file handles model concurrent vigil processes; rows must remain whole JSON lines.
        // catches: splitting newline/body writes or opening without O_APPEND.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.jsonl");
        let threads: Vec<_> = (0..12).map(|id| {
            let path = path.clone();
            std::thread::spawn(move || {
                for sequence in 0..10 {
                    append(&path, &serde_json::json!({"id": id, "sequence": sequence, "meta": {"escaped": "a\nb\"c".repeat(4096)}})).unwrap();
                }
            })
        }).collect();
        for thread in threads { thread.join().unwrap(); }
        let text = std::fs::read_to_string(path).unwrap();
        let rows: Vec<serde_json::Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.len(), 120);
        let mut identities = std::collections::BTreeSet::new();
        for row in rows {
            assert_eq!(row["meta"]["escaped"], "a\nb\"c".repeat(4096));
            assert!(identities.insert((row["id"].as_u64().unwrap(), row["sequence"].as_u64().unwrap())));
        }
    }
}
