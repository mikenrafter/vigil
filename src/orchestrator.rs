//! Orchestrator confirmation (owner 2026-11-14). Orchestrator mode (the long
//! window and credit checks) is only for the main session that speaks with the
//! user. A call that asks for it explicitly without the day's token does not
//! block: it fails with a challenge, and only the challenge prints the token.
//! Once a call confirms, the watch is confirmed for its life, so the main
//! session answers once per watch. A watch that stores the flag by default (a
//! hold) but was never confirmed gets the plain window, without a challenge.

use std::io::Read;
use std::path::PathBuf;

use crate::engine::Vigil;
use crate::error::VigilError;
use crate::watch::{WatchFile, WatchId};

impl Vigil {
    fn orchestrator_dir(&self) -> PathBuf {
        self.store.root().join("orchestrator")
    }

    /// The token for today (UTC), created on first use each day. Kept in the
    /// state root, never in docs or skills: the challenge is the one place that
    /// shows it.
    fn orchestrator_token(&self) -> Result<String, VigilError> {
        let path = self.orchestrator_dir().join("token.json");
        let today = self.clock.now().0.format("%Y-%m-%d").to_string();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok((day, token)) = serde_json::from_slice::<(String, String)>(&bytes) {
                if day == today {
                    return Ok(token);
                }
            }
        }
        let mut raw = [0u8; 4];
        std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut raw)).map_err(crate::error::StoreError::io)?;
        let token = raw.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        std::fs::create_dir_all(self.orchestrator_dir()).map_err(crate::error::StoreError::io)?;
        crate::store::write_atomic(&path, &(today, token.clone()))?;
        Ok(token)
    }

    fn orchestrator_marker(&self, id: &WatchId) -> PathBuf {
        self.orchestrator_dir().join(format!("{}.json", id.as_str()))
    }

    /// Confirmed for THIS watch: the marker names its `created_at`, so a later
    /// watch reusing the id is not.
    pub(crate) fn orchestrator_confirmed(&self, watch: &WatchFile) -> bool {
        std::fs::read(self.orchestrator_marker(&watch.spec.id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::clock::Timestamp>(&bytes).ok())
            .is_some_and(|created_at| created_at == watch.created_at)
    }

    pub(crate) fn mark_orchestrator_confirmed(&self, watch: &WatchFile) -> Result<(), VigilError> {
        std::fs::create_dir_all(self.orchestrator_dir()).map_err(crate::error::StoreError::io)?;
        crate::store::write_atomic(&self.orchestrator_marker(&watch.spec.id), &watch.created_at)?;
        Ok(())
    }

    /// `Ok(())` when `confirm` is today's token; else the challenge error.
    pub(crate) fn check_orchestrator_token(&self, confirm: Option<&str>) -> Result<(), VigilError> {
        let token = self.orchestrator_token()?;
        if confirm.is_some_and(|given| given.trim() == token) {
            return Ok(());
        }
        Err(VigilError::OrchestratorUnconfirmed(format!(
            "orchestrator mode is only for the main session that speaks directly with the user. Have you spoken directly with the user in this session? If not (you are a subagent or a delegated implementer), make this call again without orchestrator. If yes, repeat the call with confirm \"{token}\" (CLI: --confirm {token}).{}",
            if confirm.is_some() { " The confirm given is not today's." } else { "" }
        )))
    }

    /// Keepalive: what `orchestrator` this call really gets. An explicit `true`
    /// needs every covered pending watch confirmed, or today's token (which
    /// confirms them). A stored default counts only for confirmed watches.
    pub(crate) fn gate_orchestrator(&self, requested: Option<bool>, confirm: Option<&str>, covered: &[WatchId]) -> Result<Option<bool>, VigilError> {
        if !self.config.orchestrator_confirm || requested == Some(false) {
            return Ok(requested);
        }
        let pending: Vec<WatchFile> = covered.iter().filter_map(|id| self.store.load(id).ok()).filter(|watch| !watch.status.is_finished()).collect();
        let relevant: Vec<&WatchFile> = pending.iter().filter(|watch| requested == Some(true) || watch.spec.orchestrator).collect();
        if relevant.iter().all(|watch| self.orchestrator_confirmed(watch)) {
            return Ok(requested);
        }
        match self.check_orchestrator_token(confirm) {
            Ok(()) => {
                for watch in relevant {
                    self.mark_orchestrator_confirmed(watch)?;
                }
                Ok(Some(true))
            }
            Err(challenge) if requested == Some(true) => Err(challenge),
            // Stored default, never confirmed: the plain window, no challenge.
            Err(_) => Ok(Some(pending.iter().any(|watch| watch.spec.orchestrator && self.orchestrator_confirmed(watch)))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::error::VigilError;
    use crate::hold::HoldRequest;
    use crate::keepalive::KeepaliveRequest;
    use crate::testing::{default_config, spec_instant, Env, TokioClock};
    use crate::watch::WatchId;

    fn env() -> Env {
        let config = crate::config::Config { orchestrator_confirm: true, ..default_config() };
        Env::with_clock_and_config(Arc::new(TokioClock::new(spec_instant())), config)
    }

    fn id(text: &str) -> WatchId {
        WatchId::parse(text).unwrap()
    }

    async fn bare_hold(env: &Env, name: &str, orchestrator: Option<bool>, confirm: Option<&str>) -> Result<(), VigilError> {
        let request = HoldRequest { id: Some(name.into()), orchestrator, confirm: confirm.map(String::from), ..HoldRequest::default() };
        env.vigil.hold(request).await.map(|_| ())
    }

    async fn keepalive(env: &Env, name: &str, orchestrator: Option<bool>, confirm: Option<&str>) -> Result<serde_json::Value, VigilError> {
        let request = KeepaliveRequest { watch_id: Some(id(name)), max_s: Some(30), orchestrator, confirm: confirm.map(String::from) };
        env.vigil.keepalive(request).await.map(|envelope| serde_json::to_value(envelope).unwrap())
    }

    /// The token, read the way an agent would: from the challenge text.
    fn token_in(error: &VigilError) -> String {
        let text = error.to_string();
        let start = text.find("confirm \"").expect("the challenge names the token") + "confirm \"".len();
        text[start..start + 8].to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn an_explicit_orchestrator_call_is_challenged_then_confirmed_once_per_watch() {
        // why: owner 2026-11-14: orchestrator mode only for the main session; the
        //      challenge asks, and only it shows the token; the main session
        //      answers once per watch, not on every call.
        // catches: blocking without the token; a challenge that does not ask or
        //      does not name the token; asking again after the watch is confirmed.
        let env = env();
        bare_hold(&env, "h", None, None).await.unwrap();
        let challenge = keepalive(&env, "h", Some(true), None).await.expect_err("challenged");
        assert!(matches!(challenge, VigilError::OrchestratorUnconfirmed(_)), "{challenge}");
        let text = challenge.to_string();
        assert!(text.contains("spoken directly with the user") && text.contains("without orchestrator") && text.contains("--confirm"), "{text}");
        let token = token_in(&challenge);
        keepalive(&env, "h", Some(true), Some(&token)).await.expect("the token confirms");
        keepalive(&env, "h", Some(true), None).await.expect("confirmed for this watch");
    }

    #[tokio::test(start_paused = true)]
    async fn a_wrong_or_yesterdays_token_is_refused() {
        // why: the token is per UTC day, so one seen in an old transcript stops working.
        // catches: accepting any confirm; a token that never rotates.
        let env = env();
        bare_hold(&env, "h", None, None).await.unwrap();
        let token = token_in(&keepalive(&env, "h", Some(true), None).await.unwrap_err());
        let wrong = keepalive(&env, "h", Some(true), Some("00000000")).await.unwrap_err().to_string();
        assert!(wrong.contains("not today's"), "{wrong}");
        tokio::time::advance(std::time::Duration::from_secs(86_400)).await;
        bare_hold(&env, "next", None, None).await.unwrap();
        assert!(keepalive(&env, "next", Some(true), Some(&token)).await.is_err(), "yesterday's token");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_hold_gets_the_plain_window_without_a_challenge() {
        // why: subagents may hold and call keepalive with nothing extra; a hold
        //      stores orchestrator by default, which must not count until confirmed.
        // catches: challenging a caller that never asked; the stored default
        //      still granting orchestrator mode.
        let env = env();
        bare_hold(&env, "sub", None, None).await.unwrap();
        assert_eq!(env.vigil.gate_orchestrator(None, None, &[id("sub")]).unwrap(), Some(false));
        keepalive(&env, "sub", None, None).await.expect("no challenge without the flag");
    }

    #[tokio::test(start_paused = true)]
    async fn an_explicit_orchestrator_hold_needs_the_token_before_anything_is_written() {
        // why: `hold --orchestrator true` / watch_create with orchestrator true is
        //      the same request as the keepalive flag.
        // catches: writing the watch before the check; a confirmed create that
        //      still challenges its first keepalive.
        let env = env();
        let challenge = bare_hold(&env, "h", Some(true), None).await.expect_err("challenged");
        assert!(env.vigil.store().load(&id("h")).is_err(), "nothing was written");
        bare_hold(&env, "h", Some(true), Some(&token_in(&challenge))).await.expect("the token confirms");
        assert_eq!(env.vigil.gate_orchestrator(None, None, &[id("h")]).unwrap(), None, "defers to the stored, confirmed flag");
        keepalive(&env, "h", Some(true), None).await.expect("confirmed at creation");
    }
}
