//! Advisories: rules the operator programs at runtime that tell agents what to
//! do, delivered on every blocking return and by `vigil advice` (SPEC
//! "Advisories"). This module is the pure model: ids, specs, the condition
//! and message languages, number extraction and the validated `Advisory`. The
//! file (`advisory_store.rs`) and the evaluation with its cache
//! (`advisory_eval.rs`) sit on top.
//!
//! Reason it exists: an agent cannot see Boat's remaining hours or the host's
//! memory; an operator can write down what to do about them once, and every
//! agent reads it where it already looks (the keepalive envelope).
//!
//! Invariants (CLAIM(advisory-valid)): an `Advisory` that exists has a probe
//! that can yield text, an extraction that compiles, a condition and two
//! messages whose names are all known, and bounded sizes. Everything an
//! untrusted caller can send goes through `Advisory::new`.
//!
//! Mating parts: `AdvisorySpec` is the wire/TOML form; `Advisory::spec()` is
//! its canonical copy (parity: `advisory_store` round trip).

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::Value;

use serde::{Deserialize, Serialize};

use crate::inputs::Reading;
use crate::watch::Input;

pub const MAX_ADVISORIES: usize = 32;
pub const MAX_PARAMS: usize = 16;
pub const MAX_MESSAGE_BYTES: usize = 512;
pub const MAX_CONDITION_BYTES: usize = 128;
pub const MAX_REGEX_BYTES: usize = 256;
pub const DEFAULT_CACHE_S: u64 = 600;
pub const MAX_CACHE_S: u64 = 86_400;
/// A failed probe is remembered this long at most, so a dead probe is not
/// retried (and waited for) on every call, yet the failure is re-checked soon.
pub const FAILURE_CACHE_S: u64 = 30;
/// Setting names that `advisory set` treats as fields, so a parameter cannot
/// be called any of them.
pub const RESERVED_NAMES: [&str; 4] = ["cache_s", "condition", "message_when_true", "message_when_false"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdvisoryError {
    #[error("advisories are turned off (VIGIL_ADVISORIES_FILE=off)")]
    Disabled,
    #[error("bad advisory id {0:?}: use 1 to 64 characters from A-Z a-z 0-9 _ -")]
    BadId(String),
    #[error("no advisory named {id:?}; known: {known}")]
    Unknown { id: String, known: String },
    #[error("advisory {0:?} already exists; change it with `advisory set`, or add it again with replace")]
    Exists(String),
    #[error("invalid {field}: {reason}")]
    Invalid { field: String, reason: String },
    #[error("advisory {id:?} has no parameter {name:?}; its parameters: {known}")]
    UnknownParam { id: String, name: String, known: String },
    #[error("too many advisories (at most {0})")]
    TooMany(usize),
    #[error("advisories file {path}: {reason}")]
    File { path: String, reason: String },
}

impl AdvisoryError {
    pub fn invalid(field: &str, reason: impl Into<String>) -> AdvisoryError {
        AdvisoryError::Invalid { field: field.to_string(), reason: reason.into() }
    }
}

/// `[A-Za-z0-9_-]{1,64}`, parsed once at the edge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdvisoryId(String);

impl AdvisoryId {
    pub fn parse(raw: &str) -> Result<AdvisoryId, AdvisoryError> {
        let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if (1..=64).contains(&raw.len()) && raw.chars().all(allowed) {
            Ok(AdvisoryId(raw.to_string()))
        } else {
            Err(AdvisoryError::BadId(raw.to_string()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AdvisoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How the number is taken out of the probe's text. Exactly one of `json` or
/// `regex`; `of` and `scale` go only with `json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractSpec {
    /// Name the number goes by in the condition and the messages (default `value`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Dotted path into the probe's JSON, e.g. `planIncluded.vmHoursRemaining`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<String>,
    /// With `json`: a second path; the result is `json / of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    /// With `json`: multiplier applied last (100 turns a ratio into a percent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// A regular expression; the number is its first group, else the whole match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// The advisory as written in the file and sent over MCP and the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorySpec {
    pub probe: Input,
    pub extract: ExtractSpec,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, f64>,
    pub condition: String,
    pub message_when_true: String,
    pub message_when_false: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_s: Option<u64>,
}

/// The two possible meanings of a probed value.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub condition_true: bool,
    pub message: String,
}

/// Names a condition or message may use: parameters, the extracted number's
/// name, and `value`.
type Scope = BTreeMap<String, f64>;

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && text.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn known_names(scope: &Scope) -> String {
    scope.keys().cloned().collect::<Vec<_>>().join(", ")
}

// ---- extraction -----------------------------------------------------------

#[derive(Debug, Clone)]
enum Extract {
    Json { path: String, of: Option<String>, scale: f64 },
    Pattern(Regex),
}

impl Extract {
    fn compile(spec: &ExtractSpec) -> Result<Extract, AdvisoryError> {
        let bad = |reason: &str| AdvisoryError::invalid("extract", reason);
        match (&spec.json, &spec.regex) {
            (Some(_), Some(_)) => Err(bad("give either json or regex, not both")),
            (None, None) => Err(bad("give json (a dotted path) or regex")),
            (Some(path), None) => {
                for (label, text) in [("json", Some(path)), ("of", spec.of.as_ref())] {
                    if let Some(text) = text
                        && (text.is_empty() || text.split('.').any(str::is_empty))
                    {
                        return Err(bad(&format!("{label} must be a dotted path like a.b.0.c, got {text:?}")));
                    }
                }
                let scale = spec.scale.unwrap_or(1.0);
                if !scale.is_finite() {
                    return Err(bad("scale must be a finite number"));
                }
                Ok(Extract::Json { path: path.clone(), of: spec.of.clone(), scale })
            }
            (None, Some(pattern)) => {
                if spec.of.is_some() || spec.scale.is_some() {
                    return Err(bad("of and scale go only with json"));
                }
                if pattern.len() > MAX_REGEX_BYTES {
                    return Err(bad(&format!("regex is longer than {MAX_REGEX_BYTES} bytes")));
                }
                regex::RegexBuilder::new(pattern)
                    .size_limit(1 << 20)
                    .build()
                    .map(Extract::Pattern)
                    .map_err(|error| bad(&format!("regex does not compile: {error}")))
            }
        }
    }

    fn apply(&self, text: &str) -> Result<f64, String> {
        let number = match self {
            Extract::Json { path, of, scale } => {
                let root: Value = serde_json::from_str(text).map_err(|error| format!("probe output is not JSON: {error}"))?;
                let numerator = number_at(&root, path)?;
                let ratio = match of {
                    None => numerator,
                    Some(of) => {
                        let denominator = number_at(&root, of)?;
                        if denominator == 0.0 {
                            return Err(format!("{of} is 0, cannot divide {path} by it"));
                        }
                        numerator / denominator
                    }
                };
                ratio * scale
            }
            Extract::Pattern(pattern) => {
                let captures = pattern.captures(text).ok_or_else(|| format!("regex {} did not match the probe output", pattern.as_str()))?;
                let matched = if captures.len() > 1 { captures.get(1) } else { captures.get(0) }
                    .ok_or_else(|| format!("regex {}: group 1 took no part in the match", pattern.as_str()))?
                    .as_str();
                matched.trim().parse::<f64>().map_err(|_| format!("regex matched {matched:?}, which is not a number"))?
            }
        };
        if number.is_finite() { Ok(number) } else { Err("the extracted value is not a finite number".to_string()) }
    }
}

fn number_at(root: &Value, path: &str) -> Result<f64, String> {
    let mut at = root;
    for segment in path.split('.') {
        let next = match at {
            Value::Object(map) => map.get(segment),
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|index| items.get(index)),
            _ => None,
        };
        at = next.ok_or_else(|| format!("{path} not found in the probe output"))?;
    }
    let number = match at {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    number.filter(|number| number.is_finite()).ok_or_else(|| {
        let found = match at {
            Value::Null => "null".to_string(),
            Value::Bool(_) => "a boolean".to_string(),
            Value::String(text) => format!("the text {text:?}"),
            Value::Array(_) => "an array".to_string(),
            Value::Object(_) => "an object".to_string(),
            Value::Number(_) => "an unusable number".to_string(),
        };
        format!("{path} is not a number (found {found})")
    })
}

// ---- condition ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Number(f64),
    Name(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    AtMost,
    Less,
    AtLeast,
    Greater,
    Equal,
    NotEqual,
}

#[derive(Debug, Clone, PartialEq)]
struct Condition {
    left: Operand,
    comparison: Comparison,
    right: Operand,
}

impl Condition {
    fn parse(text: &str, scope: &Scope) -> Result<Condition, AdvisoryError> {
        let bad = |reason: String| AdvisoryError::invalid("condition", reason);
        if text.len() > MAX_CONDITION_BYTES {
            return Err(bad(format!("longer than {MAX_CONDITION_BYTES} bytes")));
        }
        let shape = || bad(format!("{text:?} is not `<name or number> <op> <name or number>`; ops: <= < >= > == !="));
        let start = text.find(['<', '>', '=', '!']).ok_or_else(shape)?;
        let rest = &text[start..];
        let (comparison, length) = [("<=", Comparison::AtMost), (">=", Comparison::AtLeast), ("==", Comparison::Equal), ("!=", Comparison::NotEqual), ("<", Comparison::Less), (">", Comparison::Greater)]
            .into_iter()
            .find_map(|(symbol, comparison)| rest.starts_with(symbol).then_some((comparison, symbol.len())))
            .ok_or_else(shape)?;
        let operand = |raw: &str| -> Result<Operand, AdvisoryError> {
            let raw = raw.trim();
            if raw.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '+' | '.')) {
                return raw.parse::<f64>().ok().filter(|n| n.is_finite()).map(Operand::Number).ok_or_else(shape);
            }
            if !is_identifier(raw) {
                return Err(shape());
            }
            if !scope.contains_key(raw) {
                return Err(bad(format!("unknown name {raw:?}; known names: {}", known_names(scope))));
            }
            Ok(Operand::Name(raw.to_string()))
        };
        let (left, right) = (operand(&text[..start])?, operand(&rest[length..])?);
        if matches!((&left, &right), (Operand::Number(_), Operand::Number(_))) {
            return Err(bad("compares two numbers; use a name on one side".to_string()));
        }
        Ok(Condition { left, comparison, right })
    }

    fn holds(&self, scope: &Scope) -> bool {
        let resolve = |operand: &Operand| match operand {
            Operand::Number(number) => *number,
            Operand::Name(name) => scope[name],
        };
        let (left, right) = (resolve(&self.left), resolve(&self.right));
        match self.comparison {
            Comparison::AtMost => left <= right,
            Comparison::Less => left < right,
            Comparison::AtLeast => left >= right,
            Comparison::Greater => left > right,
            Comparison::Equal => left == right,
            Comparison::NotEqual => left != right,
        }
    }
}

// ---- messages -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    Name(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Template(Vec<Piece>);

impl Template {
    fn parse(field: &str, text: &str, scope: &Scope) -> Result<Template, AdvisoryError> {
        let bad = |reason: String| AdvisoryError::invalid(field, reason);
        if text.is_empty() {
            return Err(bad("is empty".to_string()));
        }
        if text.len() > MAX_MESSAGE_BYTES {
            return Err(bad(format!("longer than {MAX_MESSAGE_BYTES} bytes")));
        }
        if text.chars().any(char::is_control) {
            return Err(bad("must be one line of plain text".to_string()));
        }
        let (mut pieces, mut literal) = (Vec::new(), String::new());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '}' => return Err(bad("has a single `}`; write `}}` for a literal brace".to_string())),
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        name.push(c);
                    }
                    if !closed {
                        return Err(bad("has a `{` with no closing `}`; write `{{` for a literal brace".to_string()));
                    }
                    if !scope.contains_key(&name) {
                        return Err(bad(format!("uses unknown name {{{name}}}; known names: {}", known_names(scope))));
                    }
                    if !literal.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut literal)));
                    }
                    pieces.push(Piece::Name(name));
                }
                other => literal.push(other),
            }
        }
        if !literal.is_empty() {
            pieces.push(Piece::Text(literal));
        }
        Ok(Template(pieces))
    }

    fn render(&self, scope: &Scope) -> String {
        self.0
            .iter()
            .map(|piece| match piece {
                Piece::Text(text) => text.clone(),
                Piece::Name(name) => format_number(scope[name]),
            })
            .collect()
    }
}

// ---- the advisory ---------------------------------------------------------

/// A checked advisory.
#[derive(Debug, Clone)]
pub struct Advisory {
    id: AdvisoryId,
    spec: AdvisorySpec,
    name: String,
    extract: Extract,
    condition: Condition,
    when_true: Template,
    when_false: Template,
}

impl Advisory {
    /// Parse, don't validate: every check lives here.
    pub fn new(id: AdvisoryId, spec: AdvisorySpec) -> Result<Advisory, AdvisoryError> {
        // CLAIM(advisory-valid) ↳ every rule below
        match &spec.probe {
            Input::Cmd { .. } | Input::Url { .. } | Input::File { .. } => {}
            other => {
                return Err(AdvisoryError::invalid("probe", format!("{} cannot give text to read a number from; use cmd, url or file", other.label())));
            }
        }
        crate::watch::validate_input(&spec.probe).map_err(|error| AdvisoryError::invalid("probe", error.to_string()))?;

        let name = spec.extract.name.clone().unwrap_or_else(|| "value".to_string());
        if !is_identifier(&name) {
            return Err(AdvisoryError::invalid("extract.name", format!("{name:?} must be letters, digits and _ (not starting with a digit), at most 32")));
        }
        let extract = Extract::compile(&spec.extract)?;

        if spec.params.len() > MAX_PARAMS {
            return Err(AdvisoryError::invalid("params", format!("at most {MAX_PARAMS} parameters")));
        }
        for (param, value) in &spec.params {
            let reason = if !is_identifier(param) {
                Some("must be letters, digits and _ (not starting with a digit), at most 32")
            } else if RESERVED_NAMES.contains(&param.as_str()) {
                Some("is the name of a setting (cache_s, condition, message_when_true, message_when_false)")
            } else if *param == name || param == "value" {
                Some("is already the name of the extracted number")
            } else if !value.is_finite() {
                Some("must be a finite number")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(AdvisoryError::invalid("params", format!("{param:?} {reason}")));
            }
        }

        if spec.cache_s.is_some_and(|seconds| seconds > MAX_CACHE_S) {
            return Err(AdvisoryError::invalid("cache_s", format!("at most {MAX_CACHE_S} (one day); 0 means probe on every call")));
        }

        let mut scope: Scope = spec.params.clone();
        scope.insert(name.clone(), 0.0);
        scope.insert("value".to_string(), 0.0);
        let condition = Condition::parse(&spec.condition, &scope)?;
        let when_true = Template::parse("message_when_true", &spec.message_when_true, &scope)?;
        let when_false = Template::parse("message_when_false", &spec.message_when_false, &scope)?;
        Ok(Advisory { id, spec, name, extract, condition, when_true, when_false })
    }
    pub fn id(&self) -> &AdvisoryId {
        &self.id
    }
    pub fn spec(&self) -> &AdvisorySpec {
        &self.spec
    }
    pub fn cache_s(&self) -> u64 {
        self.spec.cache_s.unwrap_or(DEFAULT_CACHE_S)
    }
    pub fn probe(&self) -> &Input {
        &self.spec.probe
    }
    /// Hash of what the cached probe result depends on: the probe and the
    /// extraction. Params, condition and messages are not part of it.
    pub fn fingerprint(&self) -> String {
        let basis = serde_json::json!({"probe": self.spec.probe, "extract": {"json": self.spec.extract.json, "of": self.spec.extract.of, "scale": self.spec.extract.scale, "regex": self.spec.extract.regex}});
        crate::inputs::hash_hex(&basis.to_string())
    }
    /// The extracted number's name (`value` unless the spec names it).
    pub fn value_name(&self) -> &str {
        &self.name
    }
    /// Take the number out of the probe's text; `Err` is the reason.
    pub fn extract_value(&self, probe_text: &str) -> Result<f64, String> {
        self.extract.apply(probe_text)
    }
    /// Evaluate the condition and render the message for `value`.
    pub fn judge(&self, value: f64) -> Verdict {
        let mut scope: Scope = self.spec.params.clone();
        scope.insert(self.name.clone(), value);
        scope.insert("value".to_string(), value);
        let condition_true = self.condition.holds(&scope);
        let template = if condition_true { &self.when_true } else { &self.when_false };
        Verdict { condition_true, message: template.render(&scope) }
    }
    /// A copy with the settings applied (`name=value` strings: a parameter, or
    /// one of `RESERVED_NAMES`), validated as a whole; `self` is untouched.
    pub fn with_settings(&self, assignments: &[String]) -> Result<Advisory, AdvisoryError> {
        // CLAIM(advisory-set) ↳ all-or-nothing: the copy is built fully before it is returned
        let mut spec = self.spec.clone();
        for assignment in assignments {
            let (name, value) = assignment
                .split_once('=')
                .map(|(name, value)| (name.trim(), value.trim()))
                .filter(|(name, _)| !name.is_empty())
                .ok_or_else(|| AdvisoryError::invalid("setting", format!("{assignment:?} is not name=value")))?;
            match name {
                "cache_s" => {
                    spec.cache_s = Some(value.parse::<u64>().map_err(|_| AdvisoryError::invalid("cache_s", format!("{value:?} is not a whole number of seconds")))?);
                }
                "condition" => spec.condition = value.to_string(),
                "message_when_true" => spec.message_when_true = value.to_string(),
                "message_when_false" => spec.message_when_false = value.to_string(),
                param => {
                    if !spec.params.contains_key(param) {
                        let known = spec.params.keys().cloned().collect::<Vec<_>>().join(", ");
                        let known = if known.is_empty() { "none".to_string() } else { known };
                        return Err(AdvisoryError::UnknownParam { id: self.id.to_string(), name: param.to_string(), known });
                    }
                    let number = value.parse::<f64>().ok().filter(|n| n.is_finite());
                    let number = number.ok_or_else(|| AdvisoryError::invalid(&format!("params.{param}"), format!("{value:?} is not a number")))?;
                    spec.params.insert(param.to_string(), number);
                }
            }
        }
        Advisory::new(self.id.clone(), spec)
    }
}

/// `7.3` -> `7.3`, `10.0` -> `10`, `4.04` -> `4`: one decimal, no trailing `.0`.
pub fn format_number(number: f64) -> String {
    let rounded = (number * 10.0).round() / 10.0;
    if rounded == 0.0 {
        "0".to_string()
    } else if rounded == rounded.trunc() {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

/// Turn a reading of the probe into the text to extract from, or why the
/// probe failed. A `cmd` must exit 0 and a `url` must answer 2xx; their
/// header line (`exit=0`, `HTTP 200`) is removed.
pub fn probe_body(probe: &Input, reading: &Reading) -> Result<String, String> {
    if reading.is_error {
        return Err(reading.text.clone());
    }
    let shorten = |text: &str| -> String {
        let text = text.trim();
        let cut: String = text.chars().take(200).collect();
        if cut.len() < text.len() { format!("{cut}...") } else { cut }
    };
    let (header, body) = reading.text.split_once('\n').unwrap_or((reading.text.as_str(), ""));
    match probe {
        Input::Cmd { .. } => match header.strip_prefix("exit=") {
            Some("0") => Ok(body.to_string()),
            Some(code) if body.trim().is_empty() => Err(format!("probe exited with {code}")),
            Some(code) => Err(format!("probe exited with {code}: {}", shorten(body))),
            None => Err(format!("probe gave an unreadable result: {}", shorten(&reading.text))),
        },
        Input::Url { .. } => match header.strip_prefix("HTTP ").and_then(|code| code.trim().parse::<u16>().ok()) {
            Some(200..=299) => Ok(body.to_string()),
            Some(code) => Err(format!("probe answered HTTP {code}: {}", shorten(body))),
            None => Err(format!("probe gave an unreadable result: {}", shorten(&reading.text))),
        },
        Input::File { .. } => Ok(reading.text.clone()),
        other => Err(format!("{} cannot be a probe", other.label())),
    }
}

/// The advisory seeded into a fresh file (SPEC "Default advisory").
pub fn default_boat_budget(home: Option<&str>) -> (AdvisoryId, AdvisorySpec) {
    let boat = home.filter(|home| !home.is_empty()).map_or_else(|| "boat".to_string(), |home| format!("{home}/.local/bin/boat"));
    let spec = AdvisorySpec {
        probe: Input::Cmd { argv: ["env", "BOAT_NO_UPDATE=1", boat.as_str(), "limits", "--json"].map(String::from).to_vec(), timeout_s: Some(15.0), cwd: None },
        extract: ExtractSpec {
            name: Some("pct".to_string()),
            json: Some("planIncluded.vmHoursRemaining".to_string()),
            of: Some("planIncluded.vmHours".to_string()),
            scale: Some(100.0),
            regex: None,
        },
        params: BTreeMap::from([("threshold".to_string(), 10.0)]),
        condition: "pct <= threshold".to_string(),
        message_when_true: "Boat has {pct}% of its hours left (at or below the {threshold}% threshold): run heavy jobs (builds, test suites) through qex on this host instead of Boat.".to_string(),
        message_when_false: "Boat has {pct}% of its hours left (above the {threshold}% threshold): prefer Boat for heavy work (builds, test runs) via edge.research/bin/boat-run; use qex locally only for one or two quick runs.".to_string(),
        cache_s: Some(DEFAULT_CACHE_S),
    };
    (AdvisoryId("boat-budget".to_string()), spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(value: serde_json::Value) -> AdvisorySpec {
        serde_json::from_value(value).expect("test spec parses")
    }

    fn id(raw: &str) -> AdvisoryId {
        AdvisoryId::parse(raw).unwrap()
    }

    /// A small advisory: `echo` probe, percent of two JSON numbers.
    fn budget_json() -> serde_json::Value {
        json!({
            "probe": {"kind": "cmd", "argv": ["probe-cmd"]},
            "extract": {"name": "pct", "json": "plan.left", "of": "plan.total", "scale": 100},
            "params": {"threshold": 10},
            "condition": "pct <= threshold",
            "message_when_true": "Low at {pct}% (<= {threshold}%)",
            "message_when_false": "Fine at {pct}% (> {threshold}%)"
        })
    }

    fn budget() -> Advisory {
        Advisory::new(id("budget"), spec(budget_json())).unwrap()
    }

    fn rejected(mut changes: serde_json::Value) -> String {
        let mut base = budget_json();
        for (key, value) in changes.as_object_mut().unwrap().iter() {
            base[key] = value.clone();
        }
        Advisory::new(id("x"), spec(base)).expect_err("must be rejected").to_string()
    }

    #[test]
    fn ids_follow_the_watch_id_alphabet_and_length() {
        // why: the id names a file key and appears in CLI arguments and
        //      messages; it must stay in [A-Za-z0-9_-]{1,64} like a watch id.
        //      Boundaries: 64 ok, 65 not, empty not, path-like and spaced not.
        // proves: CLAIM(advisory-valid)
        assert!(AdvisoryId::parse("boat-budget").is_ok());
        assert!(AdvisoryId::parse("A_9-z").is_ok());
        assert!(AdvisoryId::parse(&"a".repeat(64)).is_ok());
        for bad in ["", &"a".repeat(65), "a b", "a/b", "..", "a.b", "é", "a\n"] {
            assert_eq!(AdvisoryId::parse(bad), Err(AdvisoryError::BadId(bad.to_string())), "{bad:?}");
        }
    }

    #[test]
    fn numbers_print_with_at_most_one_decimal_and_no_trailing_zero() {
        // why: messages are read by agents; "10.0%" and "86.21999%" are noise.
        //      Hand cases: 10 -> "10", 7.3 -> "7.3", 4.04 -> "4" (rounds to
        //      4.0, trailing .0 dropped), 86.26 -> "86.3", 0 -> "0",
        //      -0.04 -> "0" (not "-0"), 1234.5 -> "1234.5".
        // proves: CLAIM(advisory-message)
        for (number, text) in [(10.0, "10"), (7.3, "7.3"), (4.04, "4"), (86.26, "86.3"), (0.0, "0"), (-0.04, "0"), (1234.5, "1234.5"), (-2.5, "-2.5")] {
            assert_eq!(format_number(number), text, "{number}");
        }
    }

    #[test]
    fn extraction_by_json_path_takes_a_ratio_scaled_to_a_percent() {
        // why: the default advisory needs remaining / total * 100. 50 of 200
        //      is 25 by hand. A plain path returns the number as is; array
        //      indices and numeric strings work, so other CLIs' output fits.
        // proves: CLAIM(advisory-extract)
        let advisory = budget();
        assert_eq!(advisory.extract_value(r#"{"plan":{"left":50,"total":200}}"#), Ok(25.0));
        assert_eq!(advisory.extract_value(r#"{"plan":{"left":0,"total":200}}"#), Ok(0.0));
        let plain = Advisory::new(id("p"), spec(json!({
            "probe": {"kind": "cmd", "argv": ["x"]}, "extract": {"json": "a.1.b"},
            "condition": "value > 1", "message_when_true": "t", "message_when_false": "f"
        }))).unwrap();
        assert_eq!(plain.extract_value(r#"{"a":[{"b":1},{"b":12.5}]}"#), Ok(12.5));
        assert_eq!(plain.extract_value(r#"{"a":[{"b":1},{"b":"7.5"}]}"#), Ok(7.5), "a numeric string is a number");
    }

    #[test]
    fn extraction_failures_say_what_is_wrong() {
        // why: "never silence": every way the probe text can fail to hold a
        //      number must produce a specific reason that ends up in the
        //      "advisory <id> unavailable: <reason>" entry.
        // proves: CLAIM(advisory-extract)
        let advisory = budget();
        let cases = [
            (r#"{"plan":{"total":200}}"#, "plan.left"),           // path missing
            (r#"{"plan":{"left":"lots","total":200}}"#, "not a number"),
            (r#"{"plan":{"left":5,"total":0}}"#, "plan.total is 0"), // zero denominator
            ("not json at all", "not JSON"),
            ("", "not JSON"),
            (r#"{"plan":{"left":null,"total":1}}"#, "not a number"),
        ];
        for (text, needle) in cases {
            let error = advisory.extract_value(text).expect_err(text);
            assert!(error.contains(needle), "{text:?} -> {error:?} should mention {needle:?}");
        }
    }

    #[test]
    fn extraction_by_regex_uses_group_one_else_the_whole_match() {
        // why: probes that print text (not JSON) need a regex. Group 1 wins so
        //      surrounding words are not part of the number.
        // proves: CLAIM(advisory-extract)
        let with_group = Advisory::new(id("r"), spec(json!({
            "probe": {"kind": "cmd", "argv": ["x"]}, "extract": {"name": "left", "regex": "(\\d+(?:\\.\\d+)?)% left"},
            "condition": "left < 5", "message_when_true": "t {left}", "message_when_false": "f {left}"
        }))).unwrap();
        assert_eq!(with_group.extract_value("Boat: 42.5% left of plan"), Ok(42.5));
        assert!(with_group.extract_value("nothing here").unwrap_err().contains("did not match"));
        let bare = Advisory::new(id("r"), spec(json!({
            "probe": {"kind": "cmd", "argv": ["x"]}, "extract": {"regex": "\\d+"},
            "condition": "value < 5", "message_when_true": "t", "message_when_false": "f"
        }))).unwrap();
        assert_eq!(bare.extract_value("abc 17 def"), Ok(17.0));
    }

    #[test]
    fn conditions_compare_with_every_operator_and_boundary() {
        // why: the Boat rule is "<= threshold": 10 must be TRUE (at or below).
        //      All six operators, each at below / equal / above the threshold
        //      (values 9, 10, 11), derived by hand from the operator meaning.
        // proves: CLAIM(advisory-condition)
        let table: [(&str, [bool; 3]); 6] = [
            ("<=", [true, true, false]),
            ("<", [true, false, false]),
            (">=", [false, true, true]),
            (">", [false, false, true]),
            ("==", [false, true, false]),
            ("!=", [true, false, true]),
        ];
        for (op, expected) in table {
            for (value, want) in [9.0, 10.0, 11.0].into_iter().zip(expected) {
                let mut base = budget_json();
                base["condition"] = json!(format!("pct {op} threshold"));
                let advisory = Advisory::new(id("c"), spec(base)).unwrap();
                assert_eq!(advisory.judge(value).condition_true, want, "{value} {op} 10");
            }
        }
    }

    #[test]
    fn conditions_accept_number_literals_the_alias_and_no_spaces() {
        // why: operators typed fast: `pct<=5`, `value>threshold`, `3 < pct`.
        // proves: CLAIM(advisory-condition)
        for (condition, value, want) in [("pct<=5", 5.0, true), ("pct<=5", 5.1, false), ("value>threshold", 11.0, true), ("3 < pct", 2.0, false), ("3 < pct", 4.0, true)] {
            let mut base = budget_json();
            base["condition"] = json!(condition);
            let advisory = Advisory::new(id("c"), spec(base)).unwrap();
            assert_eq!(advisory.judge(value).condition_true, want, "{condition} at {value}");
        }
    }

    #[test]
    fn bad_conditions_are_errors_that_name_the_problem() {
        // why: the model is an untrusted caller; a typo must be refused with a
        //      message that says which name is unknown or what shape is wrong,
        //      not silently evaluate to false.
        // proves: CLAIM(advisory-condition)
        let unknown = rejected(json!({"condition": "pct <= treshold"}));
        assert!(unknown.contains("treshold") && unknown.contains("threshold"), "names the typo and the known names: {unknown}");
        for bad in ["", "pct", "pct <=", "<= 5", "pct <= threshold <= 3", "pct = 5", "pct <=> 5", "pct and threshold", "5 < 6"] {
            let error = rejected(json!({"condition": bad}));
            assert!(error.contains("condition"), "{bad:?} -> {error}");
        }
        assert!(rejected(json!({"condition": "x".repeat(MAX_CONDITION_BYTES + 1)})).contains("condition"));
    }

    #[test]
    fn messages_fill_the_value_the_alias_and_every_param() {
        // why: the owner's example: "Boat at {pct}% (<= {threshold}%): ...".
        //      pct 7.3 and threshold 10 render by hand as below; `{value}` is
        //      an alias of the named number; `{{`/`}}` print braces.
        // proves: CLAIM(advisory-message)
        let advisory = budget();
        assert_eq!(advisory.judge(7.3), Verdict { condition_true: true, message: "Low at 7.3% (<= 10%)".to_string() });
        assert_eq!(advisory.judge(86.26), Verdict { condition_true: false, message: "Fine at 86.3% (> 10%)".to_string() });
        let mut base = budget_json();
        base["message_when_true"] = json!("v={value} p={pct} {{literal}}");
        let advisory = Advisory::new(id("m"), spec(base)).unwrap();
        assert_eq!(advisory.judge(1.0).message, "v=1 p=1 {literal}");
    }

    #[test]
    fn bad_templates_are_errors() {
        // why: an unknown placeholder would print "{treshold}" to every agent
        //      forever; refuse it when it is written. Unbalanced braces too.
        // proves: CLAIM(advisory-message)
        for field in ["message_when_true", "message_when_false"] {
            for bad in ["at {nope}%", "open {pct", "close pct}", "empty {}", ""] {
                let error = rejected(json!({ field: bad }));
                assert!(error.contains(field), "{field} {bad:?} -> {error}");
            }
            assert!(rejected(json!({ field: "x".repeat(MAX_MESSAGE_BYTES + 1) })).contains(field));
        }
    }

    #[test]
    fn a_spec_is_checked_as_a_whole() {
        // why: CLAIM(advisory-valid): each rejected input below is one the
        //      model could send; each must fail with the field named.
        // proves: CLAIM(advisory-valid)
        assert!(rejected(json!({"probe": {"kind": "port", "host": "h", "port": 1}})).contains("probe"));
        assert!(rejected(json!({"probe": {"kind": "process", "pid": 1}})).contains("probe"));
        assert!(rejected(json!({"probe": {"kind": "glob", "pattern": "*"}})).contains("probe"));
        assert!(rejected(json!({"probe": {"kind": "cmd", "argv": []}})).contains("probe"));
        assert!(rejected(json!({"probe": {"kind": "cmd", "argv": ["x"], "timeout_s": 25}})).contains("probe"));
        assert!(rejected(json!({"probe": {"kind": "url", "url": "https://x"}})).contains("probe"));
        assert!(rejected(json!({"extract": {"json": "a", "regex": "b"}})).contains("extract"), "json and regex together");
        assert!(rejected(json!({"extract": {}})).contains("extract"), "neither");
        assert!(rejected(json!({"extract": {"regex": "a", "of": "b"}})).contains("extract"), "of needs json");
        assert!(rejected(json!({"extract": {"regex": "a", "scale": 2}})).contains("extract"), "scale needs json");
        assert!(rejected(json!({"extract": {"regex": "("}})).contains("extract"), "regex must compile");
        assert!(rejected(json!({"extract": {"regex": "a".repeat(MAX_REGEX_BYTES + 1)}})).contains("extract"));
        assert!(rejected(json!({"extract": {"json": "a", "name": "1bad"}})).contains("extract.name"));
        assert!(rejected(json!({"cache_s": MAX_CACHE_S + 1})).contains("cache_s"));
        assert!(rejected(json!({"params": {"cache_s": 1}})).contains("params"), "reserved name");
        assert!(rejected(json!({"params": {"pct": 1}})).contains("params"), "same as the extract name");
        assert!(rejected(json!({"params": {"value": 1}})).contains("params"), "the alias");
        assert!(rejected(json!({"params": {"bad name": 1}})).contains("params"));
        let many: serde_json::Map<String, serde_json::Value> = (0..=MAX_PARAMS).map(|n| (format!("p{n}"), json!(n))).collect();
        assert!(rejected(json!({"params": many})).contains("params"));
    }

    #[test]
    fn non_finite_numbers_are_refused_where_json_cannot_carry_them() {
        // why: TOML can say `scale = inf` and `floor = nan`; JSON tests cannot,
        //      so build the spec in Rust. Either would make every message "NaN".
        // proves: CLAIM(advisory-valid)
        let mut infinite_scale = spec(budget_json());
        infinite_scale.extract.scale = Some(f64::INFINITY);
        assert!(Advisory::new(id("x"), infinite_scale).unwrap_err().to_string().contains("extract"));
        let mut nan_param = spec(budget_json());
        nan_param.params.insert("threshold".to_string(), f64::NAN);
        assert!(Advisory::new(id("x"), nan_param).unwrap_err().to_string().contains("params"));
    }

    #[test]
    fn unknown_fields_and_missing_fields_do_not_parse() {
        // why: `{"treshold": 15}` or `cache: 5` must fail loudly instead of
        //      being ignored (deny_unknown_fields), and a spec without a
        //      condition is not a spec.
        // proves: CLAIM(advisory-valid)
        let mut typo = budget_json();
        typo["cach_s"] = json!(5);
        assert!(serde_json::from_value::<AdvisorySpec>(typo).unwrap_err().to_string().contains("cach_s"));
        let mut missing = budget_json();
        missing.as_object_mut().unwrap().remove("condition");
        assert!(serde_json::from_value::<AdvisorySpec>(missing).unwrap_err().to_string().contains("condition"));
    }

    #[test]
    fn cache_s_defaults_to_600_and_accepts_zero_to_a_day() {
        // why: SPEC: cache_s default 600; 0 means "probe every call"; a day
        //      is the ceiling.
        // proves: CLAIM(advisory-cache)
        assert_eq!(budget().cache_s(), 600);
        for seconds in [0, 1, MAX_CACHE_S] {
            let mut base = budget_json();
            base["cache_s"] = json!(seconds);
            assert_eq!(Advisory::new(id("c"), spec(base)).unwrap().cache_s(), seconds);
        }
    }

    #[test]
    fn the_fingerprint_follows_the_probe_and_extraction_but_not_params_or_messages() {
        // why: CLAIM(advisory-immediate): changing the threshold or a message
        //      must not throw the probe result away (no new probe), but
        //      changing what is probed or extracted must (a cached value of
        //      the old probe would be presented as the new one's).
        // proves: CLAIM(advisory-immediate), CLAIM(advisory-cache)
        let base = budget();
        let print = base.fingerprint();
        assert!(!print.is_empty());
        let derive = |key: &str, value: serde_json::Value| {
            let mut changed = budget_json();
            changed[key] = value;
            Advisory::new(id("budget"), spec(changed)).unwrap().fingerprint()
        };
        assert_eq!(derive("params", json!({"threshold": 99})), print);
        assert_eq!(derive("condition", json!("pct < threshold")), print);
        assert_eq!(derive("message_when_true", json!("other {pct}")), print);
        assert_eq!(derive("cache_s", json!(5)), print);
        assert_ne!(derive("probe", json!({"kind": "cmd", "argv": ["other"]})), print);
        assert_ne!(derive("extract", json!({"name": "pct", "json": "plan.left", "of": "plan.total", "scale": 50})), print);
    }

    #[test]
    fn settings_change_params_and_fields_atomically() {
        // why: `advisory set boat-budget threshold=15` is the runtime knob.
        //      The result is a new advisory (the old one is untouched); an
        //      unknown name or a bad value refuses the WHOLE set; a field that
        //      breaks the whole (condition naming a missing param) is refused.
        // proves: CLAIM(advisory-set)
        let base = budget();
        let changed = base.with_settings(&["threshold=15".to_string()]).unwrap();
        assert_eq!(changed.spec().params["threshold"], 15.0);
        assert_eq!(base.spec().params["threshold"], 10.0, "the original is untouched");
        assert!(changed.judge(12.0).condition_true, "12 <= 15 now");
        assert!(!base.judge(12.0).condition_true);

        let fields = base.with_settings(&["cache_s=60".into(), "condition=pct < threshold".into(), "message_when_true=Low {pct}".into(), "threshold=7.5".into()]).unwrap();
        assert_eq!(fields.cache_s(), 60);
        assert_eq!(fields.spec().condition, "pct < threshold");
        assert_eq!(fields.spec().params["threshold"], 7.5);
        assert_eq!(fields.judge(1.0).message, "Low 1");

        let unknown = base.with_settings(&["threshold=15".into(), "treshold=3".into()]).unwrap_err();
        assert!(matches!(&unknown, AdvisoryError::UnknownParam { name, known, .. } if name == "treshold" && known.contains("threshold")), "{unknown}");
        for bad in ["threshold", "threshold=", "threshold=abc", "threshold=NaN", "threshold=inf", "=5", "cache_s=-1", "cache_s=1.5", "cache_s=999999"] {
            let error = base.with_settings(&[bad.to_string()]).expect_err(bad);
            assert!(!error.to_string().is_empty(), "{bad}");
        }
        assert!(base.with_settings(&["condition=pct <= gone".into()]).unwrap_err().to_string().contains("gone"));
    }

    #[test]
    fn a_cmd_probe_must_exit_zero_and_a_url_probe_must_answer_2xx() {
        // why: R3 reading formats: cmd is `exit=<code>\n<output>`, url is
        //      `HTTP <status>\n<body>`. A failed probe must be a reason, never
        //      a value parsed out of an error page.
        // proves: CLAIM(advisory-unavailable)
        let reading = |text: &str, is_error: bool| Reading { label: "l".into(), text: text.into(), changed: false, is_error, want_met: None, state: Default::default() };
        let cmd = Input::Cmd { argv: vec!["x".into()], timeout_s: None, cwd: None };
        assert_eq!(probe_body(&cmd, &reading("exit=0\n{\"a\":1}", false)), Ok("{\"a\":1}".to_string()));
        assert_eq!(probe_body(&cmd, &reading("exit=0\n", false)), Ok(String::new()));
        let failed = probe_body(&cmd, &reading("exit=3\nboom: no network", false)).unwrap_err();
        assert!(failed.contains("exited with 3") && failed.contains("boom: no network"), "{failed}");
        assert!(probe_body(&cmd, &reading("exit=signal-9\n", false)).unwrap_err().contains("signal-9"));
        assert_eq!(probe_body(&cmd, &reading("cannot run x: No such file", true)).unwrap_err(), "cannot run x: No such file");
        assert_eq!(probe_body(&cmd, &reading("timeout after 10s", true)).unwrap_err(), "timeout after 10s");
        let url = Input::Url { url: "http://h/".into() };
        assert_eq!(probe_body(&url, &reading("HTTP 200\n{\"a\":1}", false)), Ok("{\"a\":1}".to_string()));
        assert!(probe_body(&url, &reading("HTTP 503\nbusy", false)).unwrap_err().contains("503"));
        let file = Input::File { path: "/x".into(), tail_bytes: None };
        assert_eq!(probe_body(&file, &reading("12.5\n", false)), Ok("12.5\n".to_string()));
    }

    /// A trimmed copy of a real `boat limits --json` (captured 2026-11-12) with
    /// the two fields the default advisory reads.
    fn boat_sample(remaining: f64) -> String {
        format!(r#"{{"accessTier":"standard","planIncluded":{{"periodEnd":"2026-12-11T05:35:10.000Z","vmHours":555.56,"vmHoursRemaining":{remaining}}},"starts":{{"day":{{"limit":200}}}}}}"#)
    }

    #[test]
    fn the_default_boat_budget_advisory_follows_the_owner_rule() {
        // why: owner request 2026-11-12: "prefer Boat; when Boat's remaining
        //      hours are at or below 10%, use qex locally". Total 555.56 h:
        //      10% is 55.556 h. 55.556 -> exactly 10 -> TRUE (at or below);
        //      55.5 -> 9.99 -> TRUE; 56 -> 10.08 -> FALSE; 479.01 (the live
        //      sample) -> 86.22 -> FALSE; 0 -> TRUE.
        // proves: CLAIM(advisory-boat-budget)
        let (name, default) = default_boat_budget(Some("/home/u"));
        assert_eq!(name.as_str(), "boat-budget");
        let advisory = Advisory::new(name, default.clone()).unwrap();
        assert_eq!(advisory.spec().params["threshold"], 10.0);
        assert_eq!(advisory.cache_s(), 600);
        let Input::Cmd { argv, .. } = advisory.probe() else { panic!("the probe is a cmd") };
        assert!(argv.windows(2).any(|pair| pair == ["limits", "--json"]) && argv.iter().any(|a| a.ends_with("boat")), "{argv:?}");
        let judge = |remaining: f64| {
            let value = advisory.extract_value(&boat_sample(remaining)).expect("the sample has the fields");
            (value, advisory.judge(value))
        };
        let (value, verdict) = judge(55.556);
        assert!((value - 10.0).abs() < 1e-9, "{value}");
        assert!(verdict.condition_true, "10% is at the threshold");
        assert!(judge(55.5).1.condition_true);
        assert!(!judge(56.0).1.condition_true);
        let (value, verdict) = judge(479.01);
        assert!((value - 86.2).abs() < 0.05, "479.01 / 555.56 = 0.8622 {value}");
        assert!(!verdict.condition_true);
        assert!(verdict.message.contains("86.2") && verdict.message.contains("boat-run"), "prefer Boat via boat-run: {}", verdict.message);
        let (_, low) = judge(0.0);
        assert!(low.condition_true);
        assert!(low.message.contains("qex") && low.message.contains('0') && low.message.contains("10"), "use qex locally: {}", low.message);
    }

    #[test]
    fn the_default_probe_is_found_under_home_when_boat_lives_there() {
        // why: hooks and MCP servers may start with a PATH lacking
        //      ~/.local/bin (where `boat` lives on this host). The seed uses
        //      `env BOAT_NO_UPDATE=1 boat ...` (as boat-run does, so the CLI
        //      does not self-update during a probe) and an absolute boat path
        //      when HOME is known.
        // proves: CLAIM(advisory-boat-budget)
        let argv = |home: Option<&str>| match default_boat_budget(home).1.probe {
            Input::Cmd { argv, .. } => argv,
            other => panic!("{other:?}"),
        };
        assert_eq!(argv(Some("/home/u")), ["env", "BOAT_NO_UPDATE=1", "/home/u/.local/bin/boat", "limits", "--json"]);
        assert_eq!(argv(None), ["env", "BOAT_NO_UPDATE=1", "boat", "limits", "--json"]);
    }
}
