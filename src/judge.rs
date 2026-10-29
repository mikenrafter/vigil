//! The judge: a POST to Laya's `/v1/predict` (FINDINGS section 1), the rule
//! that turns its answers into fire/no-fire, and the `state` trimming.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::watch::ChangedInput;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One typed answer (FINDINGS section 1). Extra Laya fields (`action`,
/// `probabilities`, `legend`) are ignored on read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul { noul: f64, confidence: f64 },
    Choice { choice: String, confidence: f64 },
    Score { score: f64, confidence: f64 },
}

impl Answer {
    pub fn confidence(&self) -> f64 {
        match self {
            Answer::Noul { confidence, .. }
            | Answer::Choice { confidence, .. }
            | Answer::Score { confidence, .. } => *confidence,
        }
    }
}

pub type Answers = BTreeMap<String, Answer>;

/// Body of `POST /v1/predict`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeRequest {
    pub state: String,
    pub questions: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JudgeError {
    #[error("judge unreachable: {0}")]
    Unreachable(String),
    #[error("judge timed out")]
    Timeout,
    #[error("judge returned HTTP {0}")]
    Status(u16),
    #[error("judge returned an unreadable answer: {0}")]
    Malformed(String),
}

/// Mating parts: `HttpJudge` (real) and `testing::ScriptedJudge` (fake).
pub trait Judge: Send + Sync {
    fn predict<'a>(&'a self, request: &'a JudgeRequest) -> BoxFuture<'a, Result<Answers, JudgeError>>;
    /// `GET /health` with the health timeout; false on any failure.
    fn healthy(&self) -> BoxFuture<'_, bool>;
}

pub struct HttpJudge {
    base_url: String,
    predict_timeout: Duration,
    health_timeout: Duration,
}

impl HttpJudge {
    /// `base_url` may or may not end in `/`; requests go to `/v1/predict`
    /// and `/health` under it without a doubled slash.
    pub fn new(base_url: &str, predict_timeout: Duration, health_timeout: Duration) -> HttpJudge {
        HttpJudge {
            base_url: base_url.to_string(),
            predict_timeout,
            health_timeout,
        }
    }
}

impl Judge for HttpJudge {
    fn predict<'a>(&'a self, request: &'a JudgeRequest) -> BoxFuture<'a, Result<Answers, JudgeError>> {
        Box::pin(async move { todo!() })
    }
    fn healthy(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { todo!() })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleTest {
    /// P(true) >= p, p in 0..=1.
    NoulGte(f64),
    /// The chosen label is one of these.
    ChoiceIn(Vec<String>),
    /// Expected level >= n.
    ScoreGte(f64),
}

impl RuleTest {
    /// `"noul"`, `"choice"` or `"score"`: the question type this test needs.
    pub fn question_type(&self) -> &'static str {
        match self {
            RuleTest::NoulGte(_) => "noul",
            RuleTest::ChoiceIn(_) => "choice",
            RuleTest::ScoreGte(_) => "score",
        }
    }
}

/// `{"question", "noul_gte" | "choice_in" | "score_gte", "min_confidence"?}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeRule {
    pub question: String,
    #[serde(flatten)]
    pub test: RuleTest,
    #[serde(default)]
    pub min_confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("answer for question {0:?} is missing")]
    MissingAnswer(String),
    #[error("answer for question {0:?} has the wrong type")]
    WrongAnswerType(String),
}

impl JudgeRule {
    /// Does the judge's answer satisfy the rule? A missing or wrongly typed
    /// answer is an error (the caller treats it as a judge failure).
    pub fn evaluate(&self, answers: &Answers) -> Result<bool, RuleError> {
        todo!()
    }
}

/// V15: the observation part of `state` is never smaller than this, however
/// small `VIGIL_JUDGE_STATE_BYTES` is or however long the prompt is.
pub const MIN_OBSERVATION_BYTES: usize = 1536;

/// Build the `state` string (SPEC "Tick logic" step 4, V15): the prompt first
/// (at most 1024 bytes, V12), then the observations. The observation budget is
/// `max(limit_bytes - prompt.len(), MIN_OBSERVATION_BYTES)`; everything after
/// the prompt, labels and separators included, fits in it, so the whole state
/// is at most `prompt.len() + budget` bytes. The budget is split evenly
/// (`budget / changed.len()`) across the changed inputs, each input's lines
/// newest (last) first, each section cut on a UTF-8 boundary.
pub fn build_state(prompt: &str, changed: &[ChangedInput], limit_bytes: usize) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{laya_noul_body, Behavior, FakeHttpServer};
    use serde_json::json;

    fn answers(question: &str, answer: Answer) -> Answers {
        let mut map = Answers::new();
        map.insert(question.to_string(), answer);
        map
    }

    fn rule(test: RuleTest, min_confidence: f64) -> JudgeRule {
        JudgeRule { question: "q".into(), test, min_confidence }
    }

    // ---- rule evaluation (hand-derived) ----

    #[test]
    fn noul_gte_is_inclusive_at_the_threshold() {
        // why: SPEC `{"question","noul_gte": p}` fires when P(true) >= p.
        //      0.8 vs 0.8 -> true; 0.79 -> false; 0.81 -> true.
        // proves: CLAIM(rule-noul)
        let r = rule(RuleTest::NoulGte(0.8), 0.0);
        let at = |p: f64| answers("q", Answer::Noul { noul: p, confidence: 1.0 });
        assert_eq!(r.evaluate(&at(0.8)), Ok(true));
        assert_eq!(r.evaluate(&at(0.79)), Ok(false));
        assert_eq!(r.evaluate(&at(0.81)), Ok(true));
    }

    #[test]
    fn min_confidence_gates_the_rule_inclusively_and_defaults_to_zero() {
        // why: SPEC: optional "min_confidence" (default 0). noul 0.9 passes
        //      the noul test, but confidence 0.59 < 0.6 blocks it; 0.6 passes.
        //      Default 0 lets confidence 0.0 through.
        // proves: CLAIM(rule-min-confidence)
        let strict = rule(RuleTest::NoulGte(0.5), 0.6);
        let with_confidence = |c: f64| answers("q", Answer::Noul { noul: 0.9, confidence: c });
        assert_eq!(strict.evaluate(&with_confidence(0.59)), Ok(false));
        assert_eq!(strict.evaluate(&with_confidence(0.6)), Ok(true));
        let lax = rule(RuleTest::NoulGte(0.5), 0.0);
        assert_eq!(lax.evaluate(&with_confidence(0.0)), Ok(true));
    }

    #[test]
    fn choice_in_matches_only_listed_labels() {
        // why: SPEC `{"question","choice_in": [labels]}`. "done" is listed,
        //      "wait" is not, an empty list matches nothing.
        let r = rule(RuleTest::ChoiceIn(vec!["ready".into(), "done".into()]), 0.0);
        let chose = |label: &str| answers("q", Answer::Choice { choice: label.into(), confidence: 1.0 });
        assert_eq!(r.evaluate(&chose("done")), Ok(true));
        assert_eq!(r.evaluate(&chose("wait")), Ok(false));
        assert_eq!(rule(RuleTest::ChoiceIn(vec![]), 0.0).evaluate(&chose("done")), Ok(false));
    }

    #[test]
    fn score_gte_is_inclusive_at_the_threshold() {
        // why: SPEC `{"question","score_gte": n}`; expected level 3.0 vs 3 -> true.
        let r = rule(RuleTest::ScoreGte(3.0), 0.0);
        let scored = |s: f64| answers("q", Answer::Score { score: s, confidence: 1.0 });
        assert_eq!(r.evaluate(&scored(2.99)), Ok(false));
        assert_eq!(r.evaluate(&scored(3.0)), Ok(true));
        assert_eq!(r.evaluate(&scored(3.5)), Ok(true));
    }

    #[test]
    fn missing_or_mistyped_answers_are_errors_not_false() {
        // why: SPEC "Failure rules": "an answer missing the rule's question"
        //      is a judge failure (must not silently mean "no match").
        // proves: CLAIM(missing-answer-is-failure)
        let r = rule(RuleTest::NoulGte(0.5), 0.0);
        assert_eq!(
            r.evaluate(&answers("other", Answer::Noul { noul: 1.0, confidence: 1.0 })),
            Err(RuleError::MissingAnswer("q".into()))
        );
        assert_eq!(
            r.evaluate(&answers("q", Answer::Choice { choice: "x".into(), confidence: 1.0 })),
            Err(RuleError::WrongAnswerType("q".into()))
        );
    }

    #[test]
    fn rule_json_shapes_match_the_spec() {
        // why: parity with SPEC "watch_create": exactly one of noul_gte /
        //      choice_in / score_gte beside "question", optional min_confidence
        //      default 0; zero or two kinds are rejected. (Serde-derived, so this
        //      is a wire-format control that may already pass in the red phase.)
        let noul: JudgeRule = serde_json::from_value(json!({"question": "failed", "noul_gte": 0.8})).unwrap();
        assert_eq!(noul, JudgeRule { question: "failed".into(), test: RuleTest::NoulGte(0.8), min_confidence: 0.0 });
        let choice: JudgeRule =
            serde_json::from_value(json!({"question": "q", "choice_in": ["a", "b"], "min_confidence": 0.5})).unwrap();
        assert_eq!(choice.test, RuleTest::ChoiceIn(vec!["a".into(), "b".into()]));
        assert_eq!(choice.min_confidence, 0.5);
        assert!(serde_json::from_value::<JudgeRule>(json!({"question": "q"})).is_err(), "no kind");
        assert!(
            serde_json::from_value::<JudgeRule>(json!({"question": "q", "noul_gte": 0.5, "score_gte": 1})).is_err(),
            "two kinds"
        );
    }

    #[test]
    fn answers_parse_from_the_findings_literals_ignoring_extra_fields() {
        // why: parity with FINDINGS section 1 (Laya 0.3.6 wheel): noul has
        //      noul/confidence/action, choice has choice/probabilities, score
        //      has score/legend/probabilities. Extras must be ignored.
        let noul: Answer = serde_json::from_value(
            json!({"type": "noul", "noul": 0.93, "confidence": 0.93, "action": {"act_probability": 0.71}}),
        )
        .unwrap();
        assert_eq!(noul, Answer::Noul { noul: 0.93, confidence: 0.93 });
        let choice: Answer = serde_json::from_value(
            json!({"type": "choice", "choice": "ready", "probabilities": {"ready": 0.7, "wait": 0.3}, "confidence": 0.7}),
        )
        .unwrap();
        assert_eq!(choice, Answer::Choice { choice: "ready".into(), confidence: 0.7 });
        let score: Answer = serde_json::from_value(
            json!({"type": "score", "score": 2.4, "legend": ["low", "mid", "high"], "probabilities": [0.1, 0.3, 0.6], "confidence": 0.5}),
        )
        .unwrap();
        assert_eq!(score, Answer::Score { score: 2.4, confidence: 0.5 });
    }

    // ---- state building ----

    fn changed(label: &str, text: &str) -> ChangedInput {
        ChangedInput { label: label.into(), text: text.into() }
    }

    fn numbered(prefix: &str, n: usize) -> String {
        (0..n).map(|i| format!("{prefix}-line-{i:04}\n")).collect()
    }

    fn kept_lines(state: &str, prefix: &str) -> usize {
        state.matches(&format!("{prefix}-line-")).count()
    }

    #[test]
    fn state_starts_with_the_prompt() {
        // why: SPEC step 4: "`state` is the prompt, then the changed inputs'
        //      new content". With no changed inputs it is just the prompt.
        let state = build_state("Is the build broken?", &[changed("file:/x", "a\nb\n")], 2560);
        assert!(state.starts_with("Is the build broken?"), "{state:?}");
        assert_eq!(build_state("only prompt", &[], 2560), "only prompt");
    }

    #[test]
    fn state_lists_the_newest_lines_first() {
        // why: SPEC step 4 + FINDINGS: Laya keeps only the START of state, so
        //      the newest (last) lines must come first. Each line appears once.
        // proves: CLAIM(newest-first)
        let state = build_state("P", &[changed("file:/x", "a\nb\nc\n")], 2560);
        let (a, b, c) = (state.find("\na").unwrap(), state.find("\nb").unwrap(), state.find("\nc").unwrap());
        assert!(c < b && b < a, "expected c, b, a order in {state:?}");
        assert_eq!(state.matches('c').count(), 1);
    }

    #[test]
    fn state_reverses_lines_within_each_changed_input() {
        // why: with two changed inputs, each input's own lines are newest-first
        //      and both labels reach the judge.
        let state = build_state("P", &[changed("file:/one", "a1\na2\n"), changed("file:/two", "b1\nb2\n")], 2560);
        assert!(state.contains("file:/one") && state.contains("file:/two"));
        assert!(state.find("a2").unwrap() < state.find("a1").unwrap());
        assert!(state.find("b2").unwrap() < state.find("b1").unwrap());
    }

    #[test]
    fn state_cut_keeps_the_newest_lines_and_drops_the_oldest() {
        // why: when over budget the END of the state is cut; because newest is
        //      first, line-199 must survive and line-000 must not. 200 lines
        //      of "line-NNN\n" (9 bytes) = 1800 bytes > the 1536-byte
        //      observation floor, so the cut is real even with limit 100.
        // proves: CLAIM(newest-first)
        let text: String = (0..200).map(|i| format!("line-{i:03}\n")).collect();
        let state = build_state("P", &[changed("file:/x", &text)], 100);
        assert!(state.len() <= 1 + MIN_OBSERVATION_BYTES);
        assert!(state.contains("line-199"));
        assert!(!state.contains("line-000"));
    }

    #[test]
    fn the_observation_budget_has_a_floor_of_1536_bytes() {
        // why: V15 (S15): "(VIGIL_JUDGE_STATE_BYTES minus the prompt, at
        //      least 1536)". With a 100-byte limit and prompt "P" (1 byte) the
        //      observations still get 1536 bytes: the state is far longer than
        //      100 yet at most 1 + 1536. Needs 4000 bytes of input so the
        //      floor, not the input size, is what binds. NOTE (flagged in
        //      .red-report.md): this means a configured limit below 1537 has
        //      no effect.
        // proves: CLAIM(judge-budget)
        let input = [changed("file:/x", &numbered("n", 400))]; // 400 * 12 = 4800 bytes
        let state = build_state("P", &input, 100);
        assert!(state.len() > 1000, "floor not honoured: {} bytes", state.len());
        assert!(state.len() <= 1 + MIN_OBSERVATION_BYTES, "{} bytes", state.len());
    }

    #[test]
    fn a_maximal_prompt_still_leaves_room_for_observations() {
        // why: V15 (S15: "a prompt occupying the entire state budget leaves
        //      the judge no observations"). Prompt of 1024 bytes (the V12
        //      maximum) with the default limit 2560: budget = max(2560-1024,
        //      1536) = 1536, total <= 2560, and the newest line is present.
        //      With a limit (1100) smaller than prompt + floor the floor wins:
        //      budget 1536, total up to 2560, still more than the limit.
        // proves: CLAIM(judge-budget)
        let prompt = "p".repeat(1024);
        let input = [changed("file:/x", &numbered("n", 400))];
        let normal = build_state(&prompt, &input, 2560);
        assert!(normal.starts_with(&prompt));
        assert!(normal.len() <= 2560, "{} bytes", normal.len());
        assert!(normal.contains("n-line-0399"), "newest observation must be present");
        assert!(normal.len() >= 1024 + 1400, "observations were starved: {} bytes", normal.len());
        let tight = build_state(&prompt, &input, 1100);
        assert!(tight.len() <= 1024 + MIN_OBSERVATION_BYTES && tight.len() > 1100, "{} bytes", tight.len());
        assert!(tight.contains("n-line-0399"));
    }

    #[test]
    fn the_budget_is_split_evenly_across_changed_inputs_each_newest_first() {
        // why: V15 (S15: "the first noisy input can consume the budget before
        //      another input's relevant change"). Two inputs of 300 lines of
        //      12 bytes each (3600 bytes, far over budget), prompt "P",
        //      limit 2560: budget = 2559, share = 2559 / 2 = 1279 bytes per
        //      input = at most 106 whole 12-byte lines each (1279 / 12), at
        //      least ~100 after a label and a few separator bytes. The old
        //      "first input eats everything" gives ~213 / 0. Both newest
        //      lines are present, both oldest are not, and the shares differ
        //      by at most 2 lines. Control: ONE noisy input keeps the whole
        //      budget (>= 200 lines), so the split is what limits each here.
        // proves: CLAIM(judge-budget)
        let two = build_state(
            "P",
            &[changed("file:/a", &numbered("a", 300)), changed("file:/b", &numbered("b", 300))],
            2560,
        );
        assert!(two.len() <= 2560, "{} bytes", two.len());
        let (kept_a, kept_b) = (kept_lines(&two, "a"), kept_lines(&two, "b"));
        assert!((95..=106).contains(&kept_a), "a kept {kept_a}");
        assert!((95..=106).contains(&kept_b), "b kept {kept_b}");
        assert!(kept_a.abs_diff(kept_b) <= 2, "{kept_a} vs {kept_b}");
        for newest in ["a-line-0299", "b-line-0299"] {
            assert!(two.contains(newest), "{newest} missing");
        }
        for oldest in ["a-line-0000", "b-line-0000"] {
            assert!(!two.contains(oldest), "{oldest} should be cut");
        }

        let one = build_state("P", &[changed("file:/a", &numbered("a", 300))], 2560);
        assert!(kept_lines(&one, "a") >= 200, "a lone input keeps the whole budget, kept {}", kept_lines(&one, "a"));
    }

    #[test]
    fn state_is_cut_to_the_byte_limit_on_a_utf8_boundary() {
        // why: SPEC step 4: "cut ... on a UTF-8 boundary". Oracle: the cut
        //      state is the longest prefix of the uncut state that fits the
        //      total limit and ends on a char boundary. With prompt "P" the
        //      observation budget is limit - 1 >= 1536 for limits 1540..=1547,
        //      so the floor does not interfere; 3-byte chars make the limits
        //      sweep every alignment. Input: 400 lines of "日日日\n" (10 bytes).
        // proves: CLAIM(utf8-cut)
        let text = "日日日\n".repeat(400);
        let input = [changed("file:/x", &text)];
        let uncut = build_state("P", &input, usize::MAX);
        assert!(uncut.len() > 3900);
        for limit in 1540..=1547 {
            let cut = build_state("P", &input, limit);
            assert!(cut.len() <= limit, "limit {limit}: got {} bytes", cut.len());
            assert!(uncut.starts_with(&cut), "limit {limit}: cut must be a prefix of the uncut state");
            let next_char_len = uncut[cut.len()..].chars().next().unwrap().len_utf8();
            assert!(cut.len() + next_char_len > limit, "limit {limit}: cut too short ({} bytes)", cut.len());
        }
    }

    // ---- HTTP client against the fake server ----

    fn request() -> JudgeRequest {
        JudgeRequest {
            state: "build log tail".into(),
            questions: json!({"failed": {"type": "noul", "instructions": "Did the build fail?",
                "criteria": {"true": "it failed", "false": "fine"}, "future_field": [1, {"k": null}]}})
            .as_object()
            .unwrap()
            .clone(),
        }
    }

    fn client(server: &FakeHttpServer) -> HttpJudge {
        HttpJudge::new(&server.url(), Duration::from_millis(300), Duration::from_millis(300))
    }

    #[tokio::test]
    async fn predict_posts_state_and_questions_unchanged_and_parses_the_answer() {
        // why: SPEC step 4 + FINDINGS section 1: POST $URL/v1/predict with
        //      exactly {state, questions}; questions pass through unchanged
        //      (including unknown nested fields); the literal Laya response
        //      parses to typed answers. Positive control: the server counter moved.
        // proves: CLAIM(judge-request-shape)
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: laya_noul_body("failed", 0.93, 0.93) });
        let got = client(&server).predict(&request()).await.unwrap();
        assert_eq!(got, answers("failed", Answer::Noul { noul: 0.93, confidence: 0.93 }));
        assert_eq!(server.request_count(), 1);
        let recorded = &server.requests()[0];
        assert_eq!(recorded.method, "POST");
        assert_eq!(recorded.path, "/v1/predict");
        let body: serde_json::Value = serde_json::from_str(&recorded.body).unwrap();
        assert_eq!(
            body,
            json!({"state": "build log tail", "questions": serde_json::Value::Object(request().questions)})
        );
    }

    #[tokio::test]
    async fn a_trailing_slash_in_the_base_url_does_not_double_the_slash() {
        // why: VIGIL_JUDGE_URL is user-supplied; "http://h:4010/" must still hit /v1/predict.
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: laya_noul_body("failed", 0.1, 0.9) });
        let judge = HttpJudge::new(&format!("{}/", server.url()), Duration::from_millis(300), Duration::from_millis(300));
        judge.predict(&request()).await.unwrap();
        assert_eq!(server.requests()[0].path, "/v1/predict");
    }

    #[tokio::test]
    async fn non_2xx_responses_map_to_status_errors() {
        // why: SPEC "Failure rules": non-2xx is a judge failure. Laya returns
        //      400/500 with {"error": ...} (FINDINGS section 1).
        let server = FakeHttpServer::start(Behavior::Respond { status: 500, body: r#"{"error":"boom"}"#.into() });
        assert_eq!(client(&server).predict(&request()).await, Err(JudgeError::Status(500)));
        server.set_behavior(Behavior::Respond { status: 400, body: r#"{"error":"bad"}"#.into() });
        assert_eq!(client(&server).predict(&request()).await, Err(JudgeError::Status(400)));
        assert_eq!(server.request_count(), 2, "control: both calls reached the server");
    }

    #[tokio::test]
    async fn unreadable_200_bodies_are_malformed() {
        // why: an answer vigil cannot read is a failure, never an empty answer set.
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "not json".into() });
        assert!(matches!(client(&server).predict(&request()).await, Err(JudgeError::Malformed(_))));
        server.set_behavior(Behavior::Respond { status: 200, body: r#"{"model":"x"}"#.into() });
        assert!(matches!(client(&server).predict(&request()).await, Err(JudgeError::Malformed(_))));
    }

    #[tokio::test]
    async fn connection_refused_is_unreachable() {
        // why: SPEC "Failure rules": judge down.
        let mut server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "{}".into() });
        let judge = client(&server);
        server.stop();
        assert!(matches!(judge.predict(&request()).await, Err(JudgeError::Unreachable(_))));
    }

    #[tokio::test]
    async fn a_server_that_never_answers_times_out() {
        // why: SPEC "Failure rules": judge timed out (default 10 s; this test
        //      configures 300 ms real time because a socket timeout cannot be
        //      driven by a fake clock). Must return, not hang.
        let server = FakeHttpServer::start(Behavior::Hang);
        let outcome = tokio::time::timeout(Duration::from_secs(5), client(&server).predict(&request()))
            .await
            .expect("predict must honour its own timeout");
        assert_eq!(outcome, Err(JudgeError::Timeout));
    }

    #[tokio::test]
    async fn health_is_a_get_that_is_true_only_for_a_responding_judge() {
        // why: SPEC `watch_status` reports judge reachability "a /health GET
        //      with a 2 s timeout" (timeout injected here as 300 ms). Refused
        //      and hung servers are unreachable.
        let mut server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "{}".into() });
        let judge = client(&server);
        assert!(judge.healthy().await);
        assert_eq!((server.requests()[0].method.as_str(), server.requests()[0].path.as_str()), ("GET", "/health"));

        server.set_behavior(Behavior::Hang);
        assert!(!judge.healthy().await, "hung health endpoint");
        server.stop();
        assert!(!judge.healthy().await, "refused");
    }
}
