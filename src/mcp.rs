//! stdio MCP front: the five watch tools, the hold tools (`hold`, `hold_add`,
//! `hold_done`) and the advisory tools over `Vigil`.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    service::RequestContext,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, MetaObject, ProgressNotificationParam, ProtocolVersion,
        ServerCapabilities, ServerConfig,
    },
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::engine::Vigil;
use crate::error::VigilError;
use crate::keepalive::{KeepaliveRequest, ProgressSink, ProgressUpdate};
use crate::watch::{CreateRequest, WatchId};

/// Server `instructions`: under 512 characters; says to create a watch, then
/// call `keepalive` repeatedly, and (V20) to stop calling when `next` is not
/// exactly "call keepalive again" ("act on events" means act on them; the
/// ended text means create a new watch only if still needed).
pub fn server_instructions() -> String {
    "vigil keeps your turn open while a cheap judge watches things. watch_create says what to watch; then call \
     keepalive again and again. A main session with background shells runs \
     `vigil keepalive <id> --orchestrator` in the background instead. Read `next`: while it is exactly \"call keepalive \
     again\", call it again; otherwise stop (\"act on events\": act on them; \"watch ended; create a new watch if still \
     needed\"). Results carry `advice`. When done, call `cold`, post an inline BabelTele summary, end the turn."
        .to_string()
}

/// Serve on stdin/stdout until the client disconnects.
pub async fn serve_stdio(vigil: Arc<Vigil>) -> anyhow::Result<()> {
    let service = VigilServer::new(vigil).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// `watch_create` arguments: the spec's watch object. Nested parts stay loose
/// here and are checked by `CreateRequest`, so a bad one is a tool error with
/// vigil's own message, not a protocol error.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateArgs {
    /// Optional stable id, `[A-Za-z0-9_-]{1,64}`; generated when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// What the judge should look for (at most 1024 bytes).
    #[serde(default)]
    pub prompt: String,
    /// Inputs: {kind: file|glob|cmd|url|port|process, ...} (at most 16). The
    /// work kinds {kind: qex, job}, {kind: qex_idle, dir}, {kind: path_exists,
    /// path} and {kind: until, at} each end by themselves, so `condition` and
    /// `hold` take them without a `want`; `hold` takes only these (and a
    /// process with want = exited).
    /// Typed as objects so clients do not send each input as a JSON string.
    #[schemars(with = "Vec<serde_json::Map<String, Value>>")]
    pub inputs: Vec<Value>,
    /// Seconds between ticks, clamped to 1..=3600 (default 20).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<u64>,
    /// Seconds until the watch expires, 1..=604800 (default 3600).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_s: Option<u64>,
    /// "judge", "change", "condition" or "hold" (fires when every input, all
    /// work, has ended; no judge fields, no stall_s; deadline defaults to
    /// 604800 and orchestrator to true). The `hold` tool is the easy way.
    pub fire: String,
    /// How long the job should take, in seconds (at least 1). vigil raises one
    /// `attention` event when the watch has run 1.5x this long (or 9 minutes
    /// over, whichever comes first).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_s: Option<u64>,
    /// Seconds the inputs may stay unchanged while a `process` input still
    /// runs (at least 1) before vigil raises one `attention` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_s: Option<u64>,
    /// True when keepalive calls for this watch are the main orchestrator
    /// session's (the same as passing `orchestrator: true` to keepalive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<bool>,
    /// The token an orchestrator challenge gave; only with orchestrator: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// fire = judge: Laya questions keyed by id, types noul, choice, score.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Map<String, Value>>")]
    pub judge_questions: Option<Value>,
    /// fire = judge: {question, noul_gte | choice_in | score_gte, min_confidence?}.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Map<String, Value>>")]
    pub judge_rule: Option<Value>,
}

/// `hold` arguments (SPEC "Holds", H11).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HoldArgs {
    /// Optional stable id, `[A-Za-z0-9_-]{1,64}`; generated when absent.
    pub id: Option<String>,
    /// Work to wait on, one item each: `qex:JOB`, `pid:N`, `file:PATH` (ends
    /// when it exists), `until:RFC3339`, `idle:DIR` (no qex job at or under
    /// DIR is running), or `none` (a T3 delegate: needs `label`, ended by
    /// `hold_done`). Omit for a bare hold that lives until cancelled.
    #[serde(default, rename = "for")]
    #[schemars(rename = "for")]
    pub work: Vec<String>,
    /// Name of the item (with one `for` only; default: its spec). Unique per hold.
    pub label: Option<String>,
    /// Also wait for every qex job at or under the server's working directory.
    #[serde(default)]
    pub until_idle: bool,
    /// Seconds until the hold expires, 1..=604800 (default 604800).
    pub deadline_s: Option<u64>,
    /// Seconds each item should take; one `item overdue` attention event after 1.5x.
    pub expect_s: Option<u64>,
    /// Whether keepalive calls for this hold are the main session's (default true).
    pub orchestrator: Option<bool>,
    /// The token an orchestrator challenge gave; only with orchestrator: true.
    pub confirm: Option<String>,
    /// Count each initial item as a working or reviewing Claude run; omit for uncounted work.
    pub cap: Option<crate::cap::CapClass>,
}

/// `hold_add` arguments (W42-H2): one item per call.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HoldAddArgs {
    /// The live hold to add to.
    pub watch_id: String,
    /// The work: `qex:JOB`, `pid:N`, `file:PATH`, `until:RFC3339`, `idle:DIR`,
    /// or `none` (a T3 delegate: needs `label`, ended by `hold_done`).
    #[serde(rename = "for")]
    #[schemars(rename = "for")]
    pub work: String,
    /// Seconds this item should take; one `item overdue` attention event after 1.5x.
    pub expect_s: Option<u64>,
    /// Name of the item (default: its spec). Unique per hold.
    pub label: Option<String>,
    /// Count this item as a working or reviewing Claude run; omit for uncounted work.
    pub cap: Option<crate::cap::CapClass>,
}

/// `hold_done` arguments (W42-H5, D7).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HoldDoneArgs {
    /// The live hold.
    pub watch_id: String,
    /// The `none` item that has finished.
    pub label: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeepaliveArgs {
    /// Watch to cover; without it, the watches this server created.
    pub watch_id: Option<String>,
    /// Leave this out: vigil chooses how long the call blocks. Only to force a
    /// shorter wait (30 or more).
    pub max_s: Option<u64>,
    /// True only for the main orchestrator session; a subagent never sets it.
    pub orchestrator: Option<bool>,
    /// The token an orchestrator challenge gave; only with orchestrator: true.
    pub confirm: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdvisoryIdArgs {
    /// The advisory's id, e.g. `boat-budget`.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdvisorySetArgs {
    /// The advisory's id, e.g. `boat-budget`.
    pub id: String,
    /// Names and new values: a parameter by name (`{"threshold": 15}`), or
    /// `cache_s`, `condition`, `message_when_true`, `message_when_false`.
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub settings: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdvisoryAddArgs {
    /// New advisory id, `[A-Za-z0-9_-]{1,64}`.
    pub id: String,
    /// The advisory: {probe: {kind: cmd|url|file, ...}, extract: {name?, json?, of?, scale?, regex?},
    /// params?: {name: number}, condition: "<name> <op> <name or number>", message_when_true,
    /// message_when_false (use {name} for the value and params), cache_s?}.
    #[schemars(with = "serde_json::Map<String, Value>")]
    pub spec: Value,
    /// Replace an advisory that already has this id.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ColdArgs {
    /// Watches or holds to cancel; without it, the watches this server created.
    pub watch_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WatchIdArgs {
    pub watch_id: String,
}

/// V18: forwards each tick as a progress notification. `progress` is the
/// call-local counter; the watch and its tick ride in the message.
struct ChannelProgress(mpsc::UnboundedSender<ProgressUpdate>);

impl ProgressSink for ChannelProgress {
    fn progress(&self, update: &ProgressUpdate) {
        let _ = self.0.send(update.clone());
    }
}

/// How long queued progress may still be flushed once the engine has
/// returned, and only as far as the call's own `max_s` allows.
const PROGRESS_FLUSH: std::time::Duration = std::time::Duration::from_millis(250);

/// Run a keepalive call, handing each progress update to `notify`, and stop
/// when `cancel` completes (`Ok(None)`, C2). Progress is best effort (C11): a
/// notification that cannot be delivered never holds up the result. When the
/// engine returns, forwarding is dropped after a short flush of what is
/// already queued, which never reaches past the call's `max_s`.
async fn run_keepalive<N, F>(
    vigil: &Vigil,
    request: KeepaliveRequest,
    cancel: impl std::future::Future<Output = ()>,
    mut notify: N,
) -> Result<Option<crate::envelope::Envelope>, VigilError>
where
    N: FnMut(ProgressUpdate) -> F,
    F: std::future::Future<Output = ()>,
{
    let started = tokio::time::Instant::now();
    let (sender, mut updates) = mpsc::unbounded_channel::<ProgressUpdate>();
    let mut cancel = std::pin::pin!(cancel);
    let outcome = {
        let sink = ChannelProgress(sender);
        let run = vigil.keepalive_uncommitted(request, &sink, &mut cancel);
        let forward = async {
            while let Some(update) = updates.recv().await {
                notify(update).await;
            }
            std::future::pending::<()>().await;
        };
        tokio::select! {
            biased;
            outcome = run => outcome,
            () = forward => unreachable!("the forwarder never completes"),
        }
    };
    // The sender went with `run`; what is queued can still be sent, briefly. A20: the
    // bound is the window the call really used (the envelope's `max_s`), not
    // the 270 s ceiling of an explicit request.
    let window_s = outcome.as_ref().ok().and_then(|done| done.as_ref()).and_then(|(envelope, _)| envelope.max_s).unwrap_or(0);
    let call_end = started + std::time::Duration::from_secs(window_s);
    let flush = PROGRESS_FLUSH.min(call_end.saturating_duration_since(tokio::time::Instant::now()));
    // C2: the results count as returned only once the flush is over and no
    // cancel has arrived; a cancel during the flush makes the transport
    // discard the response, so the results must stay for the next call.
    let done = outcome?;
    let cancelled_in_flush = tokio::select! {
        biased;
        () = &mut cancel, if done.is_some() => true,
        _ = tokio::time::timeout(flush, async {
            while let Some(update) = updates.recv().await {
                notify(update).await;
            }
        }) => false,
    };
    let Some((envelope, delivery)) = done else { return Ok(None) };
    let cancelled = cancelled_in_flush
        || tokio::select! {
            biased;
            () = &mut cancel => true,
            () = std::future::ready(()) => false,
        };
    if cancelled {
        return Ok(None);
    }
    vigil.commit_delivery(delivery);
    Ok(Some(envelope))
}

fn cancelled_result() -> CallToolResult {
    text_result(true, "cancelled".to_string())
}

#[derive(Clone)]
pub struct VigilServer {
    vigil: Arc<Vigil>,
    tool_router: ToolRouter<Self>,
}

/// Claude Code defers MCP tools unless they say otherwise; the agent must
/// always see `keepalive`.
fn always_load() -> MetaObject {
    let mut meta = MetaObject::new();
    meta.0.insert("anthropic/alwaysLoad".into(), Value::Bool(true));
    meta
}

fn text_result(failed: bool, text: String) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if failed { CallToolResult::error(content) } else { CallToolResult::success(content) }
}

/// A JSON text block for success; `isError` with the message for a failure.
fn reply<T: Serialize>(outcome: Result<T, VigilError>) -> CallToolResult {
    match outcome {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(text) => text_result(false, text),
            Err(error) => text_result(true, format!("cannot encode the result: {error}")),
        },
        Err(VigilError::NothingToCover) => {
            text_result(true, "no watches to cover: pass watch_id, or call watch_create first".to_string())
        }
        Err(error) => text_result(true, error.to_string()),
    }
}

/// Advisory results: the JSON value, or `isError` with the message.
fn advisory_reply(outcome: Result<Value, crate::advisory::AdvisoryError>) -> CallToolResult {
    reply(outcome.map_err(VigilError::from))
}

/// `{"threshold": 15, "condition": "pct < threshold"}` -> `["condition=pct < threshold", "threshold=15"]`.
fn assignments_of(settings: &serde_json::Map<String, Value>) -> Result<Vec<String>, crate::advisory::AdvisoryError> {
    settings
        .iter()
        .map(|(name, value)| match value {
            Value::Number(number) => Ok(format!("{name}={number}")),
            Value::String(text) => Ok(format!("{name}={text}")),
            other => Err(crate::advisory::AdvisoryError::invalid(&format!("settings.{name}"), format!("must be a number or a string, got {other}"))),
        })
        .collect()
}

fn parse_id(raw: &str) -> Result<WatchId, VigilError> {
    Ok(WatchId::parse(raw)?)
}

#[tool_router]
impl VigilServer {
    pub fn new(vigil: Arc<Vigil>) -> VigilServer {
        VigilServer { vigil, tool_router: Self::tool_router() }
    }

    #[tool(description = "Create a watch: inputs to sense, an interval, a deadline, and a fire rule (judge, change, condition or hold). No tick runs until keepalive.")]
    async fn watch_create(&self, Parameters(args): Parameters<CreateArgs>, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, ErrorData> {
        let request = serde_json::to_value(&args)
            .and_then(serde_json::from_value::<CreateRequest>)
            .map_err(|error| format!("invalid watch: {error}"));
        Ok(match request {
            // C2: a cancelled create stops reading its baseline.
            Ok(request) => tokio::select! {
                biased;
                () = ctx.ct.cancelled() => cancelled_result(),
                created = self.vigil.create(request) => reply(created),
            },
            Err(message) => text_result(true, message),
        })
    }

    #[tool(description = "Going cold (your work is done): cancels the named watch_ids (an empty list cancels nothing), or with watch_ids omitted the watches this server created, and returns what to do next (post an inline BabelTele summary, end your turn) plus the ids still pending.")]
    async fn cold(&self, Parameters(args): Parameters<ColdArgs>) -> Result<CallToolResult, ErrorData> {
        let ids = match args.watch_ids {
            Some(ids) => ids.iter().map(|raw| parse_id(raw)).collect::<Result<Vec<_>, _>>().map(Some),
            _ => Ok(None),
        };
        Ok(match ids {
            Ok(ids) => match self.vigil.cold(ids).await {
                Ok(report) => {
                    let is_error = !report.errors.is_empty();
                    let mut result = reply(Ok(report));
                    result.is_error = Some(is_error);
                    result
                }
                Err(error) => reply::<()>(Err(error)),
            },
            Err(error) => reply::<()>(Err(error)),
        })
    }

    #[tool(description = "Hold the session in cache by watching the work itself, one item per spec: qex jobs, pids, marker files, times, a qex queue, or none for a T3 delegate (for: [\"qex:ID\", \"pid:N\", \"file:PATH\", \"until:RFC3339\", \"idle:DIR\", \"none\"]). Each item that ends raises \"item ended: LABEL: WHY\"; when all have ended keepalive returns \"hold released: ...\". With no work it lives until cancelled. Then call keepalive.")]
    async fn hold(&self, Parameters(args): Parameters<HoldArgs>, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, ErrorData> {
        let request = crate::hold::HoldRequest {
            id: args.id,
            work: args.work,
            until_idle: args.until_idle,
            deadline_s: args.deadline_s,
            expect_s: args.expect_s,
            orchestrator: args.orchestrator,
            label: args.label,
            cap: args.cap,
            confirm: args.confirm,
        };
        Ok(tokio::select! {
            biased;
            () = ctx.ct.cancelled() => cancelled_result(),
            created = self.vigil.hold(request) => reply(created),
        })
    }

    #[tool(description = "Add one item of work to a live hold (for: one spec, or \"none\" with a label for a T3 delegate). The same keepalive on the hold covers it; it raises \"item ended\" when it ends and \"item overdue\" past 1.5x expect_s.")]
    async fn hold_add(&self, Parameters(args): Parameters<HoldAddArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(match parse_id(&args.watch_id) {
            Ok(id) => reply(self.vigil.hold_add(&id, crate::hold::HoldItemRequest { work: args.work, expect_s: args.expect_s, label: args.label, cap: args.cap }).await),
            Err(error) => reply::<()>(Err(error)),
        })
    }

    #[tool(description = "End the none item `label` of a live hold (a T3 delegate finished). The next tick reports \"item ended: LABEL: done\" and releases the hold if it was the last item.")]
    async fn hold_done(&self, Parameters(args): Parameters<HoldDoneArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(match parse_id(&args.watch_id) {
            Ok(id) => reply(self.vigil.hold_done(&id, &args.label).await),
            Err(error) => reply::<()>(Err(error)),
        })
    }

    #[tool(
        description = "Block while watches tick (vigil chooses how long; leave max_s out) and return when one fires, ends or needs attention. Pass orchestrator: true only from the main session. Call again while next is \"call keepalive again\".",
        meta = always_load()
    )]
    async fn keepalive(
        &self,
        Parameters(args): Parameters<KeepaliveArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let watch_id = match args.watch_id.as_deref().map(parse_id).transpose() {
            Ok(id) => id,
            Err(error) => return Ok(reply::<()>(Err(error))),
        };
        let request = KeepaliveRequest { watch_id, max_s: args.max_s, orchestrator: args.orchestrator, confirm: args.confirm };
        let token = ctx.meta.get_progress_token();
        let outcome = run_keepalive(&self.vigil, request, ctx.ct.cancelled(), |update| {
            let peer = ctx.peer.clone();
            let token = token.clone();
            async move {
                if let Some(token) = token {
                    let notice = ProgressNotificationParam::new(token, update.progress as f64)
                        .with_message(format!("{} tick {}", update.watch_id, update.tick));
                    let _ = peer.notify_progress(notice).await;
                }
            }
        })
        .await;
        Ok(match outcome {
            Ok(None) => cancelled_result(),
            Ok(Some(envelope)) => reply(Ok(envelope)),
            Err(error) => reply::<()>(Err(error)),
        })
    }

    #[tool(description = "The current advice: operator rules for agents, each with an id and a message to follow. Same list every keepalive result carries.")]
    async fn advice(&self) -> Result<CallToolResult, ErrorData> {
        Ok(advisory_reply(self.vigil.advice_report().await))
    }

    #[tool(description = "List advisories: id, status, parameters (e.g. threshold), condition and cache time.")]
    async fn advisory_list(&self) -> Result<CallToolResult, ErrorData> {
        Ok(advisory_reply(self.vigil.advisory_list()))
    }

    #[tool(description = "One advisory: its full spec and its current evaluation.")]
    async fn advisory_show(&self, Parameters(args): Parameters<AdvisoryIdArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(advisory_reply(self.vigil.advisory_show(&args.id).await))
    }

    #[tool(description = "Add an advisory (a probe command, how to extract a number, params, a condition and two messages). Unknown or malformed fields are errors.")]
    async fn advisory_add(&self, Parameters(args): Parameters<AdvisoryAddArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(advisory_reply(self.vigil.advisory_add(&args.id, args.spec, args.replace)))
    }

    #[tool(description = "Change an advisory at runtime, e.g. id boat-budget, settings {\"threshold\": 15}. Applies at once. Unknown parameters and bad values are errors and change nothing.")]
    async fn advisory_set(&self, Parameters(args): Parameters<AdvisorySetArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(advisory_reply(assignments_of(&args.settings).and_then(|assignments| self.vigil.advisory_set(&args.id, &assignments))))
    }

    #[tool(description = "Remove an advisory.")]
    async fn advisory_rm(&self, Parameters(args): Parameters<AdvisoryIdArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(advisory_reply(self.vigil.advisory_remove(&args.id)))
    }

    #[tool(description = "Non-blocking: the current envelope of one watch, plus judge reachability. Does not tick.")]
    async fn watch_status(&self, Parameters(args): Parameters<WatchIdArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(match parse_id(&args.watch_id) {
            Ok(id) => reply(self.vigil.status(&id).await),
            Err(error) => reply::<()>(Err(error)),
        })
    }

    #[tool(description = "List every watch (any session) with status, tick and deadline.")]
    async fn watch_list(&self) -> Result<CallToolResult, ErrorData> {
        Ok(reply(self.vigil.list().await))
    }

    #[tool(description = "Cancel a pending watch. A watch that already finished returns its stored result unchanged.")]
    async fn watch_cancel(&self, Parameters(args): Parameters<WatchIdArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(match parse_id(&args.watch_id) {
            Ok(id) => reply(self.vigil.cancel(&id).await),
            Err(error) => reply::<()>(Err(error)),
        })
    }
}

#[tool_handler]
impl ServerHandler for VigilServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(server_instructions())
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }

    // Claude Code does not register a server that negotiates 2026-07-28
    // (FINDINGS section 2), so never offer it.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_fit_codex_and_teach_the_loop_and_every_stop_condition() {
        // why: SPEC "Packaging": instructions under 512 characters (Codex
        //      reads the first 512) and must say: create a watch, then call
        //      keepalive repeatedly. V20 (S20): the old text said "until act on
        //      events", so an expired or cancelled watch would loop forever.
        //      The text must name the continue value verbatim and tell the
        //      agent to stop when `next` is anything else, and mention both
        //      other values. The e2e test checks the same text over the wire.
        // proves: CLAIM(instructions)
        let text = server_instructions();
        assert!(text.chars().count() < 512, "{} chars", text.chars().count());
        for needle in ["watch_create", "keepalive", "call keepalive again", "act on events", "watch ended"] {
            assert!(text.contains(needle), "instructions must mention {needle:?}: {text}");
        }
        assert!(text.to_lowercase().contains("stop"), "must say when to stop: {text}");
    }

    #[tokio::test(start_paused = true)]
    async fn progress_that_never_delivers_cannot_delay_the_result_past_max_s() {
        // why: code review C11: the handler joined the engine call with the
        //      progress forwarder, and `notify_progress` awaits transport
        //      completion, so a stalled client (backpressure) held the result
        //      past max_s. Progress is best effort: when the engine returns,
        //      forwarding is stopped and dropped. The notifier here never
        //      completes; a quiet watch ticks at 0 s and 20 s, so progress is
        //      produced and blocks. The call must still return at exactly 30 s
        //      (virtual time; the outer 60 s timeout only turns a regression
        //      into a failure instead of a hang).
        // proves: CLAIM(call-budget)
        use crate::testing::*;
        let env = Env::with_clock(std::sync::Arc::new(TokioClock::new(spec_instant())));
        let path = env.input_path("w.log");
        write_file(&path, "old\n");
        env.vigil.create(change_request("w", &path, 20)).await.unwrap();
        let started = tokio::time::Instant::now();
        let call = run_keepalive(&env.vigil, KeepaliveRequest { watch_id: None, max_s: Some(30), orchestrator: None, confirm: None }, std::future::pending::<()>(), |_update| std::future::pending::<()>());
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(60), call)
            .await
            .expect("the result was held back by a progress notification that never completes")
            .unwrap()
            .expect("not cancelled");
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(30));
        assert_eq!(envelope.status, crate::watch::WatchStatus::Pending);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_during_the_progress_flush_keeps_the_result_for_the_next_call() {
        // why: code review C2 (still open after round 1): terminal ids were
        //      marked returned when the engine finished, before the progress
        //      flush. A cancel arriving during the flush makes the transport
        //      discard the response, and the following no-id keepalive then
        //      found nothing to cover. The watch fires at once; its progress
        //      notice cannot be delivered (the flush runs its full 250 ms);
        //      the cancel lands 100 ms in. That call is cancelled; the next
        //      one must still return the fired result.
        // proves: CLAIM(call-budget)
        use crate::testing::*;
        let env = Env::with_clock(std::sync::Arc::new(TokioClock::new(spec_instant())));
        let path = env.input_path("w.log");
        write_file(&path, "old\n");
        env.vigil.create(change_request("w", &path, 20)).await.unwrap();
        append_file(&path, "new\n");
        let cancel = async { tokio::time::sleep(std::time::Duration::from_millis(100)).await };
        let first = run_keepalive(&env.vigil, KeepaliveRequest { watch_id: None, max_s: Some(30), orchestrator: None, confirm: None }, cancel, |_update| std::future::pending::<()>())
            .await
            .unwrap();
        assert!(first.is_none(), "the call was cancelled");
        let second = run_keepalive(&env.vigil, KeepaliveRequest { watch_id: None, max_s: Some(30), orchestrator: None, confirm: None }, std::future::pending::<()>(), |_update| std::future::ready(()))
            .await
            .expect("the result was marked returned by the cancelled call")
            .expect("not cancelled");
        assert_eq!(second.status, crate::watch::WatchStatus::Fired);
    }

    #[tokio::test(start_paused = true)]
    async fn the_mcp_call_is_held_one_interval_after_a_quiet_failed_save_and_a_cancel_ends_the_hold() {
        // why: DF1a (revised, T1): MCP and CLI keepalive share one
        //      pacing path. `run_keepalive` is the MCP server's call. Quiet
        //      watch, interval 20 s, every write fails: the call is held 20 s
        //      (virtual) and returns pending with persist.ok=false. A cancel
        //      5 s into the hold ends it at once (`Ok(None)`, 5 s spent): the
        //      hold is part of the cancellable call, not a sleep after it.
        // proves: CLAIM(DF1a), CLAIM(call-budget)
        use crate::fault::{FaultPlan, FaultRule, When, WriteKind};
        use crate::testing::*;
        let env = Env::with_clock(std::sync::Arc::new(TokioClock::new(spec_instant())));
        let path = env.input_path("w.log");
        write_file(&path, "old\n");
        let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
        let plan = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 28, When::Every), FaultRule::new(WriteKind::Reserve, 28, When::Every)]);
        let vigil = env.another_process().with_write_faults(plan);

        let started = tokio::time::Instant::now();
        let envelope = run_keepalive(&vigil, KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(30), orchestrator: None, confirm: None }, std::future::pending::<()>(), |_update| std::future::ready(()))
            .await
            .unwrap()
            .expect("not cancelled");
        assert!(envelope.save_failed(), "precondition: the save failed");
        // 20 s of hold; the progress flush after it adds at most its 250 ms.
        assert_eq!(envelope.waited_s, Some(20));
        assert!(started.elapsed() >= std::time::Duration::from_secs(20) && started.elapsed() < std::time::Duration::from_secs(21), "{:?}", started.elapsed());

        let started = tokio::time::Instant::now();
        let cancel = async { tokio::time::sleep(std::time::Duration::from_secs(5)).await };
        let cancelled = run_keepalive(&vigil, KeepaliveRequest { watch_id: Some(id), max_s: Some(30), orchestrator: None, confirm: None }, cancel, |_update| std::future::ready(())).await.unwrap();
        assert!(cancelled.is_none(), "the cancel ended the hold");
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(5));
    }
}
