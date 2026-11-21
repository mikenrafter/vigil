//! `vigil` CLI. Reads the environment here, at the edge, and hands explicit
//! values (state root, config) to the library.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use vigil::judge::HttpJudge;
use vigil::store::state_root;
use vigil::{Config, CreateRequest, Envelope, KeepaliveRequest, PersistNote, SystemClock, Vigil, WatchId};

#[derive(Debug, Parser)]
#[command(
    name = "vigil",
    version,
    about = "Keep an agent's turn open while a cheap judge watches things",
    long_about = "Keep an agent's turn open while a cheap judge watches builds, logs, commands, ports, processes and URLs.\n\n\
                  Only watches with fire = judge use the judge, a Jev-style typed-decision endpoint (POST /v1/predict; \
                  Laya is one implementation) at VIGIL_JUDGE_URL (default http://127.0.0.1:4010). Optional tools: qex for \
                  qex: and idle: hold items, boat for the first-use boat-budget advisory, uw for the credit check. \
                  VIGIL_COLD_SUMMARY sets what `cold` tells the agent to post."
)]
struct Cli {
    /// State root; default `$XDG_STATE_HOME/vigil`, else `~/.local/state/vigil`.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Project reminders, delivered only inside vigil calls.
    Remind { #[command(subcommand)] command: RemindCommand },
    /// One-time announcements delivered once per session.
    /// For this release, restart long-lived `vigil mcp` servers after installing.
    Notice { #[command(subcommand)] command: NoticeCommand },
    /// Print the notices not yet shown to this agent session and record them as
    /// delivered (for SessionStart / UserPromptSubmit hooks). Text form: one
    /// `id: message` line each, nothing when none. With `--session`, uses only
    /// that id; otherwise checks and records every session name the call sees
    /// (all present identity environment variables plus hook payload ids with
    /// `--from-hook`, falling back to an agent ancestor only when none exist).
    /// Unidentified hooks fail; other unidentified calls use one run of the
    /// calling process and say so.
    Notices {
        #[arg(long)]
        session: Option<String>,
        /// Directory that decides which scoped notices apply (default: the cwd).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Read the hook's JSON payload from stdin for the session id and cwd.
        #[arg(long)]
        from_hook: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run the stdio MCP server.
    Mcp,
    /// Create a watch from one JSON object (argument, or stdin if absent).
    Create { json: Option<String> },
    Status { id: String },
    List,
    Cancel { id: String },
    /// Going cold (the work is done): cancel the named watches and holds, then
    /// print the instruction to post a summary of the session and end the turn
    /// (VIGIL_COLD_SUMMARY changes what to post), and the ids still pending. With no ids it cancels nothing: the CLI cannot
    /// tell which watches are this session's.
    Cold { ids: Vec<String> },
    /// Read or set the expiring limits on counted Claude runs (default work/review: 1/1).
    Cap {
        #[command(subcommand)]
        command: Option<CapCommand>,
    },
    /// Wait for a watch or all session holds. JSON by default; --brief prints text.
    Keepalive {
        #[arg(required_unless_present = "session", conflicts_with = "session")]
        id: Option<String>,
        /// Cover this session's holds across CLI/MCP processes, including new holds.
        /// Each hold must be confirmed separately before --orchestrator can cover it.
        #[arg(long)]
        session: bool,
        /// Two plain lines with actions, notes, next step, and every advisory message.
        #[arg(long)]
        brief: bool,
        #[arg(long)]
        max_s: Option<u64>,
        /// This call is the main orchestrator session's (AI1').
        #[arg(long)]
        orchestrator: bool,
        /// The token an orchestrator challenge printed.
        #[arg(long, value_name = "TOKEN")]
        confirm: Option<String>,
    },
    /// Hold the session: a watch on the work itself. Each `--for` is one item
    /// of work: `qex:JOB`, `pid:N`, `file:PATH` (ends when it exists),
    /// `until:RFC3339`, `idle:DIR` (no qex job at or under DIR is running), or
    /// `none` (a delegated run with no pid or file to watch, such as a T3 Code
    /// delegate task: needs --label, ended by `vigil hold done`). `qex:` and
    /// `idle:` need qex on PATH (https://github.com/stephenc/qex). Each
    /// autonomous end wakes keepalive; `hold done` queues a quiet note.
    /// Up to 16 live items; ended history retains 64. The hold releases
    /// when every item has ended. With no work it is a bare hold, live until
    /// cancelled (7 days at most); `vigil hold add` gives it items. Then
    /// `keepalive ID` (main session: add --orchestrator, a hold stores it by
    /// default). One `keepalive --session --brief` covers all owned holds.
    #[command(args_conflicts_with_subcommands = true)]
    Hold {
        #[command(subcommand)]
        item: Option<HoldItemCommand>,
        #[command(flatten)]
        create: Box<HoldArgs>,
    },
    /// Print the current advice (operator rules for agents), for hooks and
    /// scripts: one `id: message` line per advisory, or JSON with --json.
    Advice {
        #[arg(long)]
        json: bool,
    },
    /// Program advisories: rules that tell agents what to do (SPEC "Advisories").
    Advisory {
        #[command(subcommand)]
        command: AdvisoryCommand,
    },
}

#[derive(Debug, Subcommand)]
enum NoticeCommand {
    /// Add a notice. Delivered once to every agent session until it expires or is removed.
    Add {
        id: String,
        #[arg(long)]
        message: String,
        /// RFC 3339 instant after which the notice is never shown.
        #[arg(long)]
        expires: Option<chrono::DateTime<chrono::FixedOffset>>,
        /// Only sessions whose directory is under one of these (absolute) directories get it.
        #[arg(long, num_args = 1..)]
        scope: Vec<PathBuf>,
    },
    /// Every notice, oldest first, with `expired`.
    List,
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
enum RemindCommand {
    List,
    Show { id: String },
    Add(Box<RemindAddArgs>),
    Done { id: String },
    Snooze { id: String, duration: String },
    Rm { id: String },
}

#[derive(Debug, clap::Args)]
struct RemindAddArgs {
    id: String,
    #[arg(long, required = true, num_args = 1..)]
    scope: Vec<PathBuf>,
    #[arg(long, value_delimiter = ',', required = true)]
    days: Vec<String>,
    #[arg(long)]
    window: Option<String>,
    #[arg(long)]
    tz: Option<String>,
    /// JSON argv array; exits 0 due, 1 not due, others unavailable.
    #[arg(long)]
    probe: Option<String>,
    /// Numeric comparison, e.g. value >= 10 (default extraction: number).
    #[arg(long)]
    condition: Option<String>,
    /// Advisory extraction object as JSON; requires --condition.
    #[arg(long)]
    extract: Option<String>,
    #[arg(long)]
    message: String,
    #[arg(long, default_value = "week", value_parser = ["day", "week"])]
    period: String,
    #[arg(long, default_value_t = 600)]
    cache_s: u64,
    #[arg(long, default_value_t = 10)]
    timeout_s: u64,
}

#[derive(Debug, clap::Args)]
struct HoldArgs {
    /// Opaque item metadata, repeatable (model, kind, brief path, items).
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_meta)]
    meta: Vec<(String, String)>,
    /// Count every initial item as a Claude work or review run; omit for uncounted work.
    #[arg(long, value_enum)]
    cap: Option<vigil::cap::CapClass>,
    #[arg(long)]
    id: Option<String>,
    /// One item of work to wait on (repeatable): qex:JOB, pid:N, file:PATH,
    /// until:RFC3339, idle:DIR, or none.
    #[arg(long = "for", value_name = "SPEC")]
    work: Vec<String>,
    /// Name of the item (with one --for only; default: its spec). Unique per hold.
    #[arg(long)]
    label: Option<String>,
    /// Also wait for every qex job at or under the current directory (an item).
    #[arg(long)]
    until_idle: bool,
    /// Seconds until the hold expires, 1..=604800 (default 604800).
    #[arg(long)]
    deadline_s: Option<u64>,
    /// Seconds each item should take. Quiet overdue note after 1.5x (or
    /// 9 minutes over, whichever comes first); interrupt once at 2x.
    #[arg(long)]
    expect_s: Option<u64>,
    /// Store whether keepalive calls are the main session's (default true).
    #[arg(long, value_name = "BOOL")]
    orchestrator: Option<bool>,
    /// The token an orchestrator challenge printed.
    #[arg(long, value_name = "TOKEN")]
    confirm: Option<String>,
}

/// `vigil hold add` / `vigil hold done` (W42): change the items of a live hold.
#[derive(Debug, Subcommand)]
enum HoldItemCommand {
    /// Add one item of work to the live hold ID; one keepalive on ID covers it.
    Add {
        /// Opaque item metadata, repeatable.
        #[arg(long, value_name = "KEY=VALUE", value_parser = parse_meta)]
        meta: Vec<(String, String)>,
        id: String,
        /// The work: qex:JOB, pid:N, file:PATH, until:RFC3339, idle:DIR, or
        /// none (a delegated run with no pid or file to watch; needs --label).
        #[arg(long = "for", value_name = "SPEC")]
        work: String,
        /// Name of the item (default: its spec). Unique per hold.
        #[arg(long)]
        label: Option<String>,
        /// Expected seconds: overdue note after 1.5x (capped at +9m), interrupt at 2x.
        #[arg(long)]
        expect_s: Option<u64>,
        /// Count this item as a Claude work or review run; omit for uncounted work.
        #[arg(long, value_enum)]
        cap: Option<vigil::cap::CapClass>,
    },
    /// End the `none` item LABEL. Queues a durable note without waking keepalive.
    Done { id: String, label: String },
}

#[derive(Debug, Subcommand)]
enum CapCommand {
    /// Set positive limits until a required future RFC3339 time.
    Set {
        #[arg(long)]
        work: std::num::NonZeroUsize,
        #[arg(long)]
        review: Option<std::num::NonZeroUsize>,
        #[arg(long)]
        until: chrono::DateTime<chrono::FixedOffset>,
    },
    /// Restore work/review limits to 1/1.
    Reset,
}

#[derive(Debug, Subcommand)]
enum AdvisoryCommand {
    /// List advisories (id, status, params, condition, cache time).
    List,
    /// One advisory: its spec and its current evaluation.
    Show { id: String },
    /// Add an advisory from one JSON object (argument, or stdin if absent).
    Add {
        id: String,
        json: Option<String>,
        /// Replace an existing advisory of that id.
        #[arg(long)]
        replace: bool,
    },
    /// Change settings: `NAME=VALUE` for a parameter (e.g. threshold=15), or
    /// for cache_s, condition, message_when_true, message_when_false.
    Set {
        id: String,
        #[arg(required = true, num_args = 1..)]
        settings: Vec<String>,
    },
    /// Remove an advisory.
    Rm { id: String },
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("cannot start the async runtime");
    let code = runtime.block_on(async_main(cli));
    // C9: shutting the runtime down drops every task that is still running,
    // and with them the reads whose drop SIGKILLs a command's process group.
    // The timeout only bounds threads stuck in blocking I/O (stdin); nothing
    // needs them.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}

/// Run the command; SIGINT or SIGTERM cancels it (C9): the command's future is
/// dropped here, `main` then shuts the runtime down so command cleanup runs,
/// and the process exits 130 (SIGINT) or 143 (SIGTERM).
async fn async_main(cli: Cli) -> i32 {
    use tokio::signal::unix::{signal, SignalKind};
    let (mut interrupt, mut terminate) = match (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("error: cannot handle signals: {error}");
            return 1;
        }
    };
    tokio::select! {
        result = run(cli) => match result {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        _ = interrupt.recv() => 130,
        _ = terminate.recv() => 143,
    }
}

/// Environment is read here and nowhere else (the library takes explicit
/// values): `XDG_STATE_HOME`, `HOME` and the `VIGIL_*` variables (including the
/// test-only `VIGIL_TEST_FAULT`).
fn build_vigil(root: Option<PathBuf>) -> anyhow::Result<Vigil> {
    let root = match root {
        Some(root) => root,
        None => state_root(std::env::var("XDG_STATE_HOME").ok().as_deref(), std::env::var("HOME").ok().as_deref())
            .ok_or_else(|| anyhow::anyhow!("cannot find a state directory: set XDG_STATE_HOME or HOME, or pass --root"))?,
    };
    let config = Config::from_lookup(|name| std::env::var(name).ok());
    let judge = HttpJudge::new(&config.judge_url, config.judge_timeout, config.health_timeout);
    // Test seam (fault.rs): unset in production, so the plan injects nothing.
    let faults = vigil::FaultPlan::from_env_value(std::env::var(vigil::fault::ENV_VAR).ok().as_deref())
        .map_err(|error| anyhow::anyhow!("invalid {}: {error}", vigil::fault::ENV_VAR))?;
    // A3, S4: who is calling is read here, once, from the whole environment.
    // agy gives the vigil mcp it spawns no ANTIGRAVITY_* variable: its parent name is the signal.
    let agent = vigil::cache::AgentEnv::from_vars(std::env::vars())
        .with_antigravity_signal(vigil::peers::antigravity_in_ancestry(std::path::Path::new("/proc"), std::process::id()));
    let session = vigil::peers::session_key_from_lookup(|name| std::env::var(name).ok());
    let notice_sessions = vigil::notice_ops::session_keys_from_lookup::<&str>(|name| std::env::var(name).ok(), &[]);
    Ok(Vigil::new(root, Arc::new(SystemClock), Arc::new(judge), config).with_write_faults(faults).with_agent_env(agent).with_session_key(session).with_notice_session_keys(notice_sessions))
}

fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// N3: write an envelope to the process's real standard output and say whether
/// it got there. Rust's own `stdout()` ignores a closed descriptor (EBADF) and
/// `println!` panics on any other failure, so a delivery committed after it
/// could be committed for an envelope nobody received. This writes through a
/// duplicate of descriptor 1 (the duplication itself fails when it is closed),
/// reports every error, and flushes.
fn deliver_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let text = serde_json::to_string(value)?;
    deliver_text(&text)
}

fn deliver_text(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::fd::AsFd;
    let mut line = text.as_bytes().to_vec();
    line.push(b'\n');
    let mut stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned().map_err(|error| anyhow::anyhow!("cannot write the envelope to stdout: {error}"))?);
    stdout.write_all(&line).and_then(|()| stdout.flush()).map_err(|error| anyhow::anyhow!("cannot write the envelope to stdout: {error}"))
}

/// DF2: the envelope on stdout is complete even when its save failed; stderr
/// gets one line per failed save naming the state root, for the person
/// watching the terminal. The envelope's `persist` note is what an agent reads.
fn warn_about_unsaved(root: &std::path::Path, envelope: &Envelope) {
    if let Some(PersistNote::NotSaved { failure, .. }) = &envelope.persist {
        eprintln!(
            "vigil: warning: {}: could not save {} (state root {}): {}; the result above is complete, free some space and keep calling",
            failure.kind.label(),
            failure.path.display(),
            root.display(),
            failure.error
        );
    }
    for other in &envelope.persist_failures {
        eprintln!(
            "vigil: warning: {}: could not save {} (state root {}): {}",
            other.kind.label(),
            other.path.display(),
            root.display(),
            other.error
        );
    }
}

/// The process exit code on success.
async fn run(cli: Cli) -> anyhow::Result<i32> {
    let vigil = build_vigil(cli.root)?;
    // D4: finished watches are deleted 7 days after they finish, by any run.
    // Housekeeping must not turn a working command into a failing one.
    let _ = vigil.prune_async().await;
    match cli.command {
        Command::Create { json } => {
            let raw = match json {
                Some(raw) => raw,
                None => {
                    let mut raw = String::new();
                    // C14: async, so the signal branches stay live while stdin is idle.
                    tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut raw).await?;
                    raw
                }
            };
            let request: CreateRequest = serde_json::from_str(&raw).map_err(|error| anyhow::anyhow!("invalid watch JSON: {error}"))?;
            print_json(&vigil.create(request).await?)?;
            Ok(0)
        }
        Command::Hold { item: None, create } => {
            let HoldArgs { id, work, label, until_idle, deadline_s, expect_s, orchestrator, cap, confirm, meta } = *create;
            print_json(&vigil.hold(vigil::HoldRequest { id, work, until_idle, deadline_s, expect_s, orchestrator, label, cap, confirm, meta: meta.into_iter().collect() }).await?)?;
            Ok(0)
        }
        Command::Hold { item: Some(HoldItemCommand::Add { id, work, label, expect_s, cap, meta }), .. } => {
            print_json(&vigil.hold_add(&WatchId::parse(&id)?, vigil::HoldItemRequest { work, expect_s, label, cap, meta: meta.into_iter().collect() }).await?)?;
            Ok(0)
        }
        Command::Hold { item: Some(HoldItemCommand::Done { id, label }), .. } => {
            print_json(&vigil.hold_done(&WatchId::parse(&id)?, &label).await?)?;
            Ok(0)
        }
        Command::Status { id } => print_json(&vigil.status(&WatchId::parse(&id)?).await?).map(|()| 0),
        Command::List => print_json(&vigil.list().await?).map(|()| 0),
        Command::Cancel { id } => {
            let envelope = vigil.cancel(&WatchId::parse(&id)?).await?;
            print_json(&envelope)?;
            warn_about_unsaved(vigil.store().root(), &envelope);
            Ok(0)
        }
        Command::Cold { ids } => {
            let ids = ids.iter().map(|id| WatchId::parse(id)).collect::<Result<Vec<_>, _>>()?;
            let report = vigil.cold(Some(ids)).await?;
            print_json(&report)?;
            for warning in &report.warnings { eprintln!("vigil: warning: {warning}"); }
            for error in &report.errors { eprintln!("vigil: {error}"); }
            Ok(if report.errors.is_empty() { 0 } else { 1 })
        }
        Command::Cap { command } => {
            let limits = match command {
                None => vigil.cap_limits()?,
                Some(CapCommand::Reset) => vigil.cap_reset().await?,
                Some(CapCommand::Set { work, review, until }) => {
                    match vigil.cap_set(work, review, vigil::Timestamp(until.with_timezone(&chrono::Utc))).await {
                        Ok(limits) => limits,
                        Err(vigil::VigilError::Item(vigil::ItemError::CapUntil)) => {
                            eprintln!("vigil: cap --until must be a future RFC3339 time");
                            return Ok(2);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            };
            print_json(&limits)?;
            Ok(0)
        }
        Command::Keepalive { id, session, brief, max_s, orchestrator, confirm } => {
            let request = KeepaliveRequest { watch_id: id.as_deref().map(WatchId::parse).transpose()?, max_s, orchestrator: orchestrator.then_some(true), confirm, session };
            let (envelope, delivery) = vigil
                .keepalive_uncommitted(request, &vigil::keepalive::NoProgress, std::future::pending::<()>())
                .await?
                .expect("a call that cannot be cancelled always finishes");
            // N3: print, THEN commit. A failed write leaves the delivery
            // uncommitted (the error ends the process), so the next call replays it.
            if brief { deliver_text(&envelope.brief())?; } else { deliver_json(&envelope)?; }
            vigil.commit_delivery(delivery);
            warn_about_unsaved(vigil.store().root(), &envelope);
            Ok(0)
        }
        Command::Remind { command } => {
            let result = match command {
                RemindCommand::List => vigil.remind_list()?,
                RemindCommand::Show { id } => vigil.remind_show(&id).await?,
                RemindCommand::Done { id } => vigil.remind_done(&id)?,
                RemindCommand::Snooze { id, duration } => vigil.remind_snooze(&id, &duration)?,
                RemindCommand::Rm { id } => vigil.remind_rm(&id)?,
                RemindCommand::Add(args) => {
                    let RemindAddArgs { id, scope, days, window, tz, probe, condition, extract, message, period, cache_s, timeout_s } = *args;
                    let probe = probe.map(|raw| serde_json::from_str::<Vec<String>>(&raw)).transpose()?;
                    let extract = extract.map(|raw| serde_json::from_str::<serde_json::Value>(&raw)).transpose()?;
                    vigil.remind_add(&id, serde_json::json!({
                        "scope": scope, "days": days, "window": window, "tz": tz,
                        "probe": probe, "condition": condition, "extract": extract,
                        "message": message, "period": period, "cache_s": cache_s, "timeout_s": timeout_s,
                    }))?
                },
            };
            print_json(&result)?;
            Ok(0)
        }
        Command::Notice { command } => {
            let result = match command {
                NoticeCommand::Add { id, message, expires, scope } => {
                    let expires_at = expires.map(|at| vigil::Timestamp(at.with_timezone(&chrono::Utc)));
                    serde_json::to_value(vigil.notice_add(vigil::notice::NoticeRequest { id, message, expires_at, scope })?)?
                }
                NoticeCommand::List => vigil.notice_list()?,
                NoticeCommand::Rm { id } => vigil.notice_rm(&id)?,
            };
            print_json(&result)?;
            Ok(0)
        }
        Command::Notices { session, mut cwd, from_hook, json } => {
            let mut vigil = vigil;
            if from_hook {
                let mut raw = String::new();
                tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut raw).await?;
                let payload: serde_json::Value = serde_json::from_str(&raw).map_err(|error| anyhow::anyhow!("hook payload on stdin is not JSON: {error}"))?;
                if !payload.is_object() {
                    anyhow::bail!("hook payload on stdin is not a JSON object");
                }
                if session.is_none() {
                    let payload_ids = vigil::notice_ops::hook_session_ids(&payload);
                    let names = vigil::notice_ops::session_keys_from_lookup(|name| std::env::var(name).ok(), &payload_ids);
                    if names.is_empty() {
                        anyhow::bail!("hook has no session identity in payload, environment or ancestry");
                    }
                    vigil = vigil.with_notice_session_keys(names);
                }
                if cwd.is_none() {
                    cwd = payload["cwd"].as_str().filter(|dir| !dir.is_empty()).map(PathBuf::from);
                }
            }
            let claim = vigil.claim_notices(session.as_deref(), cwd.as_deref())?;
            let delivery = claim.delivery().clone();
            let printed = if json {
                deliver_json(&delivery)
            } else if delivery.notices.is_empty() {
                Ok(())
            } else {
                let mut lines: Vec<String> = delivery.notices.iter().map(|notice| format!("{}: {}", notice.id, notice.message)).collect();
                lines.extend(delivery.note.iter().map(|note| format!("note: {note}")));
                deliver_text(&lines.join("\n"))
            };
            // Recorded before printing so racing callers cannot both print it;
            // released when printing failed so the notice is not lost.
            match printed {
                Ok(()) => {
                    claim.keep();
                    Ok(0)
                }
                Err(error) => {
                    claim.release();
                    Err(error)
                }
            }
        }
        Command::Advice { json } => {
            let report = vigil.advice_report().await?;
            if json {
                print_json(&report)?;
            } else {
                if let Some(error) = report["reminders_error"].as_str() { println!("{error}"); }
                for entry in ["advice", "reminders"].iter().flat_map(|key| report[key].as_array().into_iter().flatten()) {
                    println!("{}: {}", entry["id"].as_str().unwrap_or("?"), entry["message"].as_str().unwrap_or(""));
                }
            }
            Ok(0)
        }
        Command::Advisory { command } => {
            let shown = match command {
                AdvisoryCommand::List => vigil.advisory_list()?,
                AdvisoryCommand::Show { id } => vigil.advisory_show(&id).await?,
                AdvisoryCommand::Add { id, json, replace } => {
                    let raw = match json {
                        Some(raw) => raw,
                        None => {
                            let mut raw = String::new();
                            tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut raw).await?;
                            raw
                        }
                    };
                    let spec = serde_json::from_str(&raw).map_err(|error| anyhow::anyhow!("invalid advisory JSON: {error}"));
                    // The id is checked before the body, so a bad id is named first.
                    vigil::AdvisoryId::parse(&id)?;
                    vigil.advisory_add(&id, spec?, replace)?
                }
                AdvisoryCommand::Set { id, settings } => vigil.advisory_set(&id, &settings)?,
                AdvisoryCommand::Rm { id } => vigil.advisory_remove(&id)?,
            };
            print_json(&shown)?;
            Ok(0)
        }
        Command::Mcp => vigil::mcp::serve_stdio(Arc::new(vigil.with_transport(vigil::peers::CallTransport::Mcp))).await.map(|()| 0),
    }
}

fn parse_meta(text: &str) -> Result<(String, String), String> {
    let (key, value) = text.split_once('=').ok_or("expected KEY=VALUE")?;
    if key.is_empty() { return Err("metadata key must not be empty".into()); }
    Ok((key.into(), value.into()))
}
