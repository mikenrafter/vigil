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
        self.orchestrator_dir().join("watches").join(format!("{}.json", id.as_str()))
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
        std::fs::create_dir_all(self.orchestrator_dir().join("watches")).map_err(crate::error::StoreError::io)?;
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

    /// Only a named watch can gain confirmation. A no-id call requires every
    /// covered watch that can keep waiting already confirmed, even with a token.
    /// This includes quiet terminal holds; other terminal results return at once.
    /// Implicit mode also requires confirmation and a stored flag.
    pub(crate) fn gate_orchestrator(&self, requested: Option<bool>, confirm: Option<&str>, covered: &[WatchId], named_watch: Option<&WatchId>) -> Result<Option<bool>, VigilError> {
        if !self.config.orchestrator_confirm || requested == Some(false) {
            return Ok(requested);
        }
        let pending: Vec<WatchFile> = covered.iter().filter_map(|id| self.store.load(id).ok()).filter(|watch| !watch.wakes_wait()).collect();
        let all_confirmed = pending.iter().all(|watch| self.orchestrator_confirmed(watch));
        // CLAIM(confirmation-scope): plain callers never touch the token, and
        // mixed no-id calls stay plain [SPEC Orchestrator confirmation;
        // fix1_plain_calls_do_not_create_or_read_the_token;
        // fix1_probe_a_mixed_no_id_call_registers_plain_and_gets_plain_window].
        if requested.is_none() && confirm.is_none() {
            return Ok(if all_confirmed { None } else { Some(false) });
        }
        if all_confirmed {
            return Ok(requested);
        }
        let Some(named_watch) = named_watch else {
            return Err(VigilError::OrchestratorUnconfirmed(
                "orchestrator keepalive without watch_id requires every covered waiting watch already confirmed. Confirm each watch by naming its watch_id; a no-id call confirms nothing.".into()
            ));
        };
        self.check_orchestrator_token(confirm)?;
        for watch in pending.iter().filter(|watch| &watch.spec.id == named_watch) {
            self.mark_orchestrator_confirmed(watch)?;
        }
        Ok(Some(true))
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
        let request = KeepaliveRequest { watch_id: Some(id(name)), max_s: Some(30), orchestrator, confirm: confirm.map(String::from), session: false };
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
        let wrong_token = if token == "00000000" { "11111111" } else { "00000000" };
        let wrong = keepalive(&env, "h", Some(true), Some(wrong_token)).await.unwrap_err();
        assert!(matches!(wrong, VigilError::OrchestratorUnconfirmed(_)), "{wrong}");
        assert!(wrong.to_string().contains("not today's"), "{wrong}");
        tokio::time::advance(std::time::Duration::from_secs(86_400)).await;
        bare_hold(&env, "next", None, None).await.unwrap();
        let yesterday = keepalive(&env, "next", Some(true), Some(&token)).await.unwrap_err();
        assert!(matches!(yesterday, VigilError::OrchestratorUnconfirmed(_)), "{yesterday}");
        assert!(yesterday.to_string().contains("not today's"), "{yesterday}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_hold_gets_the_plain_window_without_a_challenge() {
        // why: subagents may hold and call keepalive with nothing extra; a hold
        //      stores orchestrator by default, which must not count until confirmed.
        // catches: challenging a caller that never asked; the stored default
        //      still granting orchestrator mode.
        let env = env();
        bare_hold(&env, "sub", None, None).await.unwrap();
        assert_eq!(env.vigil.gate_orchestrator(None, None, &[id("sub")], Some(&id("sub"))).unwrap(), Some(false));
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
        assert_eq!(env.vigil.gate_orchestrator(None, None, &[id("h")], Some(&id("h"))).unwrap(), None, "defers to the stored, confirmed flag");
        keepalive(&env, "h", Some(true), None).await.expect("confirmed at creation");
    }

    #[tokio::test(start_paused = true)]
    async fn fix1_no_id_cannot_confirm_new_watches_even_with_the_token() {
        // why: a no-id call on a shared server must not confirm another agent's watches.
        // catches: checking the token then marking every covered pending watch.
        let env = env();
        bare_hold(&env, "main", None, None).await.unwrap();
        bare_hold(&env, "sub", None, None).await.unwrap();
        let token = token_in(&keepalive(&env, "main", Some(true), None).await.unwrap_err());
        keepalive(&env, "main", Some(true), Some(&token)).await.unwrap();
        let result = env.vigil.keepalive(KeepaliveRequest {
            watch_id: None, max_s: Some(30), orchestrator: Some(true), confirm: Some(token.clone()), session: false }).await;
        assert!(matches!(result, Err(VigilError::OrchestratorUnconfirmed(_))), "{result:?}");
        assert!(!env.vigil.orchestrator_confirmed(&env.vigil.store().load(&id("sub")).unwrap()));
        keepalive(&env, "sub", Some(true), Some(&token)).await.unwrap();
        env.vigil.keepalive(KeepaliveRequest { watch_id: None, max_s: Some(30), orchestrator: Some(true), confirm: None, session: false }).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn fix1_plain_calls_do_not_create_or_read_the_token() {
        // why: a stored hold default must have no token side effects on plain callers.
        // catches: implicit token creation or accepting a stored token without a request.
        let env = env();
        bare_hold(&env, "sub", None, None).await.unwrap();
        keepalive(&env, "sub", None, None).await.unwrap();
        assert!(!env.root().join("orchestrator/token.json").exists());
        std::fs::create_dir_all(env.root().join("orchestrator/token.json")).unwrap();
        keepalive(&env, "sub", None, None).await.expect("plain call ignores broken token storage");
    }

    #[tokio::test(start_paused = true)]
    async fn fix1_token_named_watch_does_not_overwrite_the_daily_token() {
        // why: valid watch id 'token' used to collide with orchestrator/token.json.
        // catches: keeping watch markers in the token's directory.
        let env = env();
        bare_hold(&env, "token", None, None).await.unwrap();
        let token = token_in(&keepalive(&env, "token", Some(true), None).await.unwrap_err());
        keepalive(&env, "token", Some(true), Some(&token)).await.unwrap();
        let bytes = std::fs::read(env.root().join("orchestrator/token.json")).unwrap();
        let (day, stored): (String, String) = serde_json::from_slice(&bytes).expect("token stays a daily token");
        assert_eq!(day, spec_instant().0.format("%Y-%m-%d").to_string());
        assert_eq!(stored, token);
        assert!(env.root().join("orchestrator/watches/token.json").is_file());
    }

    #[tokio::test(start_paused = true)]
    async fn fix1_later_watch_reusing_an_id_is_not_confirmed() {
        // why: confirmation belongs to the watch lifetime, not its reusable id.
        // catches: considering a marker's existence sufficient.
        let env = env();
        bare_hold(&env, "reused", None, None).await.unwrap();
        let first = env.vigil.store().load(&id("reused")).unwrap();
        env.vigil.mark_orchestrator_confirmed(&first).unwrap();
        assert!(env.vigil.orchestrator_confirmed(&first));
        env.vigil.cancel(&id("reused")).await.unwrap();
        std::fs::remove_file(env.vigil.store().watch_path(&id("reused"))).unwrap();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        bare_hold(&env, "reused", None, None).await.unwrap();
        let later = env.vigil.store().load(&id("reused")).unwrap();
        assert_ne!(first.created_at, later.created_at);
        assert!(!env.vigil.orchestrator_confirmed(&later));
        assert!(matches!(keepalive(&env, "reused", Some(true), None).await, Err(VigilError::OrchestratorUnconfirmed(_))));
    }

}
