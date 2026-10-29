//! Worker digest and lineage glance (cb-digest): read-only projections that
//! let a waking agent or the owner's phone read one bounded summary per
//! worker instead of a whole journal.
//!
//! * `GET /v1/workers/{id}/digest` — one worker, always valid JSON of at most
//!   [`WORKER_DIGEST_MAX_BYTES`] bytes. Over budget, TEXT fields shrink; the
//!   document is never cut mid-string.
//! * `GET /v1/lineages/{id}/glance` — the lineage's waiting and working
//!   workers (at most [`LINEAGE_GLANCE_ROW_CAP`] rows each) and today's DONE
//!   count.
//!
//! Both are projections of rows the Hub already holds (roster, instance,
//! journal tail, interactions, commands, provider supply): no Node call, no
//! new table. Authorization is the lineage read predicate: an Agent reads a
//! worker only when `lineage_owns` holds for the worker's instance (the same
//! edge `GET /v1/instances/{id}/journal` uses), and a lineage only when it is
//! the Agent's own — `restrict_agent_routes` admits exactly these two shapes
//! and nothing else under `/v1/workers/*`.
//!
//! Every string that came from a worker, a tool or a screen is untrusted
//! data. The pending-tool argument summary is quoted as such, withholds
//! secrets and input-handoff values, and is bounded.

use crate::AppState;
use crate::agent_scope::{caller, origin, owns};
use crate::auth::require_origin;
use crate::error::HubError;
use crate::http::map_store;
use crate::store::{CommandRecord, InstanceRecord, InteractionRecord, ProviderRecord, Store};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use remuda_protocol::{
    DigestConfigure, DigestPair, DigestPending, DigestState, DigestSupply, DigestText, DigestTurn,
    DigestWorker, GlanceRow, InputOrigin, LINEAGE_GLANCE_ROW_CAP, LineageGlance,
    WORKER_DIGEST_MAX_BYTES, WorkerDigest, WorkerRoster, WorkerState, WorkerWatchStatus,
};
use rusqlite::params;
use serde_json::Value;
use std::collections::HashMap;
use time::OffsetDateTime;

/// Journal events scanned for the digest. One turn's tail is far smaller; the
/// cap bounds work on long-lived instances.
const JOURNAL_TAIL_EVENTS: i64 = 256;
/// Recent commands scanned for the queued count and a pending configure.
const COMMAND_WINDOW: usize = 200;
/// Workers a glance loads before classifying (a lineage fans out to a handful;
/// this bounds a pathological roster).
const GLANCE_LOAD_CAP: i64 = 500;
/// Per-value cap inside the pending-tool argument summary.
const ARG_VALUE_CHARS: usize = 160;
const REDACTED: &str = "[redacted]";
const WITHHELD: &str = "[withheld]";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workers/{id}/digest", get(worker_digest))
        .route("/v1/lineages/{id}/glance", get(lineage_glance))
}

// ── secret scrubbing ───────────────────────────────────────────────────────

/// Whether a key / flag / header name denotes a credential or an input-handoff
/// value (a value the owner or another process handed to this one).
fn is_secret_name(name: &str) -> bool {
    let lower = name
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.')
        .to_ascii_lowercase();
    if lower.is_empty() || lower.contains("author") {
        return false;
    }
    const NEEDLES: &[&str] = &[
        "token",
        "secret",
        "passw",
        "pwd",
        "credential",
        "apikey",
        "api_key",
        "api-key",
        "auth",
        "cookie",
        "private",
        "bearer",
        "handoff",
        "hand_off",
        "hand-off",
        "signature",
        "accesskey",
        "access_key",
        "access-key",
        "stdin",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
        || lower == "key"
        || lower.ends_with("_key")
        || lower.ends_with("-key")
}

/// Whether a bare word has the shape of a credential: a well-known provider
/// prefix, a JWT, a PEM marker, a long hex string or a long opaque run.
fn is_secret_shaped(token: &str) -> bool {
    let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "sk_test_",
        "pk_live_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "AKIA",
        "ASIA",
        "AIza",
        "npm_",
        "pypi-",
        "hf_",
        "shpat_",
        "dop_v1_",
    ];
    if PREFIXES
        .iter()
        .any(|prefix| token.starts_with(prefix) && token.len() >= prefix.len() + 8)
    {
        return true;
    }
    if token.starts_with("eyJ") && token.len() >= 30 {
        return true;
    }
    let all_hex = !token.is_empty() && token.bytes().all(|b| b.is_ascii_hexdigit());
    // A 40-char hex run is a git sha-1: useful evidence, not a credential.
    if all_hex && token.len() >= 32 && token.len() != 40 {
        return true;
    }
    let opaque = token.len() >= 32
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'='));
    opaque
        && token.bytes().any(|b| b.is_ascii_digit())
        && token.bytes().any(|b| b.is_ascii_uppercase())
        && token.bytes().any(|b| b.is_ascii_lowercase())
}

#[derive(Default)]
struct Scrub {
    /// The next word is the value of a secret flag / header.
    next_secret: bool,
    /// Inside a quoted secret that spans words; the closing quote char.
    quote: Option<char>,
    in_pem: bool,
}

/// Mask `value`, keeping a surrounding quote so the shell shape survives, and
/// remember an unterminated opening quote so following words are swallowed.
fn mask_value(value: &str, scrub: &mut Scrub) -> String {
    let Some(first) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
        return REDACTED.to_string();
    };
    let closed = value.len() > 1 && value.ends_with(first);
    if !closed {
        scrub.quote = Some(first);
        return format!("{first}{REDACTED}{first}");
    }
    format!("{first}{REDACTED}{first}")
}

fn looks_like_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Redact the credential-shaped pieces of one word that carries no secret
/// name: URL userinfo and bare credential-shaped tokens.
fn scrub_plain(word: &str) -> String {
    let mut word = word.to_string();
    if let Some(scheme_end) = word.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = word[authority_start..]
            .find(['/', '?', '#'])
            .map_or(word.len(), |offset| authority_start + offset);
        if let Some(at) = word[authority_start..authority_end].rfind('@') {
            let userinfo_end = authority_start + at;
            let userinfo = &word[authority_start..userinfo_end];
            let masked = match userinfo.split_once(':') {
                Some((user, _password)) => format!("{user}:{REDACTED}"),
                None if is_secret_shaped(userinfo) => REDACTED.to_string(),
                None => userinfo.to_string(),
            };
            word.replace_range(authority_start..userinfo_end, &masked);
        }
    }
    let trimmed = word
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_string();
    if !trimmed.is_empty() && is_secret_shaped(&trimmed) {
        return word.replacen(&trimmed, REDACTED, 1);
    }
    word
}

/// One word through the redactor; `None` swallows a word that continues an
/// already-masked quoted secret.
fn scrub_word(word: &str, scrub: &mut Scrub) -> Option<String> {
    if scrub.in_pem {
        if word.contains("-----END") {
            scrub.in_pem = false;
        }
        return None;
    }
    if let Some(quote) = scrub.quote {
        if word.ends_with(quote) {
            scrub.quote = None;
        }
        return None;
    }
    if word.contains("-----BEGIN") {
        scrub.in_pem = !word.contains("-----END");
        return Some(REDACTED.to_string());
    }
    let bare = word.trim_matches(|c: char| matches!(c, '"' | '\''));
    if scrub.next_secret {
        scrub.next_secret = false;
        // `Authorization: Bearer <token>` — the scheme word is not the secret.
        if bare.eq_ignore_ascii_case("bearer") || bare.eq_ignore_ascii_case("basic") {
            scrub.next_secret = true;
            return Some(word.to_string());
        }
        return Some(mask_value(word, scrub));
    }
    // NAME=value / --flag=value.
    if let Some((name, value)) = word.split_once('=') {
        let name_bare = name.trim_start_matches(['-', '"', '\'']);
        if looks_like_name(name_bare) {
            if is_secret_name(name_bare) {
                if value.is_empty() {
                    return Some(word.to_string());
                }
                return Some(format!("{name}={}", mask_value(value, scrub)));
            }
            return Some(format!("{name}={}", scrub_plain(value)));
        }
    }
    // `Header-Name:value` (not a URL).
    if let Some((name, value)) = word.split_once(':')
        && !value.starts_with("//")
        && !value.is_empty()
        && looks_like_name(name.trim_start_matches(['"', '\'']))
        && is_secret_name(name)
    {
        return Some(format!("{name}:{}", mask_value(value, scrub)));
    }
    // `Authorization:` / `X-Api-Key:` alone: the value is the next word.
    if let Some(name) = word.strip_suffix(':')
        && looks_like_name(name.trim_start_matches(['"', '\'']))
        && is_secret_name(name)
    {
        scrub.next_secret = true;
        return Some(word.to_string());
    }
    // `--password VALUE`.
    if word.starts_with('-') && is_secret_name(word) {
        scrub.next_secret = true;
        return Some(word.to_string());
    }
    if bare.eq_ignore_ascii_case("bearer") || bare.eq_ignore_ascii_case("basic") {
        scrub.next_secret = true;
        return Some(word.to_string());
    }
    Some(scrub_plain(word))
}

/// Collapse whitespace to single spaces and redact credential-shaped content.
/// The result is one line, which also stops quoted data from forging
/// structure in whatever prompt eventually embeds it.
pub(crate) fn redact_secrets(text: &str) -> String {
    let mut scrub = Scrub::default();
    let words: Vec<String> = text
        .split_whitespace()
        .filter_map(|word| scrub_word(word, &mut scrub))
        .collect();
    words.join(" ")
}

/// Unwrap a `Knowledge`-shaped value (`{"state":"known","value":…}`) or pass a
/// bare value through.
fn known(value: &Value) -> Option<&Value> {
    match value.get("state").and_then(Value::as_str) {
        Some("known") => value.get("value"),
        Some(_) => None,
        None => Some(value),
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn clip_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    if cap == 0 {
        return String::new();
    }
    let mut clipped: String = text.chars().take(cap - 1).collect();
    clipped.push('…');
    clipped
}

/// Bounded one-line summary of a tool's arguments. Secret-named keys keep
/// their name but not their value; nested containers collapse to a count.
pub(crate) fn summarize_args(input: &Value) -> String {
    let input = known(input).unwrap_or(input);
    let Some(object) = input.as_object() else {
        let text = scalar_text(input).unwrap_or_else(|| input.to_string());
        return clip_chars(&redact_secrets(&text), ARG_VALUE_CHARS);
    };
    // Shell tools lead with the command: that is what an approval is about.
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort_by_key(|key| (!matches!(key.as_str(), "command" | "cmd"), (*key).clone()));
    let mut parts = Vec::new();
    for key in keys {
        let value = &object[key];
        if is_secret_name(key) {
            parts.push(format!("{key}={WITHHELD}"));
            continue;
        }
        let rendered = match value {
            Value::Array(items) => format!("[{} items]", items.len()),
            Value::Object(map) => format!("{{{} keys}}", map.len()),
            Value::Null => "null".to_string(),
            other => {
                let text = scalar_text(other).unwrap_or_default();
                clip_chars(&redact_secrets(&text), ARG_VALUE_CHARS)
            }
        };
        parts.push(format!("{key}={rendered}"));
    }
    parts.join("; ")
}

// ── journal tail ───────────────────────────────────────────────────────────

/// A tool call that has started and not yet produced its final result.
#[derive(Debug, Clone)]
struct OpenTool {
    id: String,
    name: String,
    input: Value,
}

#[derive(Debug, Default)]
struct Tail {
    /// Text of the last assistant message of the root conversation.
    last_text: Option<String>,
    /// `(status, observedAt)` of the last root turn result.
    turn: Option<(String, Option<String>)>,
    open_tools: Vec<OpenTool>,
    /// `observedAt` of the final journal event.
    last_event_at: Option<String>,
}

fn blocks_text(blocks: &Value) -> String {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn event_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| known(value).and_then(Value::as_str).map(str::to_string))
}

/// Fold the journal tail (ascending seq) into the facts the digest needs.
fn scan_tail(events: &[Value]) -> Tail {
    let mut tail = Tail::default();
    // messageId -> accumulated text, in first-seen order.
    let mut messages: Vec<(String, String)> = Vec::new();
    for event in events {
        if let Some(at) = event.get("observedAt").and_then(Value::as_str) {
            tail.last_event_at = Some(at.to_string());
        }
        let Some(payload) = event.get("payload") else {
            continue;
        };
        match event.get("kind").and_then(Value::as_str) {
            Some("message") => {
                if payload.get("role").and_then(Value::as_str) != Some("assistant")
                    || payload
                        .get("parentToolCallId")
                        .is_some_and(|parent| !parent.is_null())
                {
                    continue;
                }
                let id = payload
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let text = payload.get("blocks").map(blocks_text).unwrap_or_default();
                let operation = payload
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("open");
                match messages.iter_mut().find(|(known_id, _)| *known_id == id) {
                    Some((_, existing)) if operation == "append" => existing.push_str(&text),
                    Some((_, existing)) if operation != "close" => *existing = text,
                    Some(_) => {}
                    None => messages.push((id, text)),
                }
            }
            Some("tool_call") => {
                let Some(id) = payload.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                let name = event_text(payload.get("toolName")).unwrap_or_default();
                let input = payload.get("input").cloned().unwrap_or(Value::Null);
                match tail.open_tools.iter_mut().find(|tool| tool.id == id) {
                    Some(tool) => {
                        if !name.is_empty() {
                            tool.name = name;
                        }
                        if !input.is_null() {
                            tool.input = input;
                        }
                    }
                    None => tail.open_tools.push(OpenTool {
                        id: id.to_string(),
                        name,
                        input,
                    }),
                }
            }
            Some("tool_result") => {
                let final_stage = payload
                    .get("stage")
                    .and_then(Value::as_str)
                    .unwrap_or("final")
                    == "final";
                if final_stage && let Some(id) = payload.get("toolCallId").and_then(Value::as_str) {
                    tail.open_tools.retain(|tool| tool.id != id);
                }
            }
            Some("lifecycle")
                if payload.get("type").and_then(Value::as_str) == Some("native")
                    && payload.get("topic").and_then(Value::as_str) == Some("turn")
                    && payload.get("nativeName").and_then(Value::as_str) == Some("result") =>
            {
                let status = event_text(payload.get("status")).unwrap_or_else(|| "unknown".into());
                tail.turn = Some((
                    status,
                    event
                        .get("observedAt")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                ));
                // The turn is over: nothing it started is still running.
                tail.open_tools.clear();
            }
            _ => {}
        }
    }
    tail.last_text = messages
        .into_iter()
        .map(|(_, text)| text)
        .rfind(|text| !text.trim().is_empty());
    tail
}

// ── pending interaction ────────────────────────────────────────────────────

struct PendingParts {
    kind: String,
    id: String,
    question: String,
    tool: Option<String>,
    args: Option<String>,
    age_secs: i64,
}

fn parse_unix(stamp: &str) -> Option<i64> {
    OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .map(|at| at.unix_timestamp())
        .ok()
}

fn pending_parts(record: &InteractionRecord, open: &[OpenTool], now: i64) -> PendingParts {
    let interaction = record
        .payload
        .pointer("/payload/interaction")
        .or_else(|| record.payload.get("interaction"))
        .unwrap_or(&record.payload);
    let request = interaction.get("request").unwrap_or(&Value::Null);
    let kind = request
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| interaction.get("kind").and_then(Value::as_str))
        .unwrap_or(&record.kind);
    let kind = if kind == "permission" {
        "approval"
    } else {
        kind
    };
    let title = request
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = request
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_call_id = request.get("toolCallId").and_then(Value::as_str);
    let matched = tool_call_id
        .and_then(|id| open.iter().find(|tool| tool.id == id))
        // An approval journalled without a tool id still names its tool in the
        // title; match the newest open call of that name.
        .or_else(|| {
            (kind == "approval")
                .then(|| {
                    open.iter()
                        .rev()
                        .find(|tool| !tool.name.is_empty() && tool.name == title)
                })
                .flatten()
        });
    let (question, tool, args) = match kind {
        "question" => {
            let fields: Vec<String> = request
                .get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|field| {
                    let field_title = field.get("title").and_then(Value::as_str).unwrap_or("?");
                    if field.get("sensitive").and_then(Value::as_bool) == Some(true) {
                        // A sensitive field's prompt may itself name the secret.
                        format!("{} (sensitive)", clip_chars(field_title, 48))
                    } else {
                        field_title.to_string()
                    }
                })
                .collect();
            let joined = if fields.is_empty() {
                title.to_string()
            } else {
                fields.join(" | ")
            };
            (joined, None, None)
        }
        "approval" => {
            let tool = matched
                .map(|tool| tool.name.clone())
                .filter(|name| !name.is_empty())
                .or_else(|| (!title.is_empty()).then(|| title.to_string()));
            let args = matched
                .map(|tool| &tool.input)
                .filter(|input| !input.is_null())
                .or_else(|| request.get("input"))
                .map(summarize_args);
            let question = if description.is_empty() {
                title
            } else {
                description
            };
            (question.to_string(), tool, args)
        }
        _ => (title.to_string(), None, None),
    };
    PendingParts {
        kind: kind.to_string(),
        id: record.interaction_id.clone(),
        question: redact_secrets(&question),
        tool,
        // Every value in the summary was redacted as it was rendered; a second
        // pass would rewrite the `[withheld]` markers.
        args: args.filter(|args| !args.is_empty()),
        age_secs: parse_unix(&record.created_at).map_or(0, |at| (now - at).max(0)),
    }
}

// ── supply ─────────────────────────────────────────────────────────────────

struct SupplyFacts {
    state: &'static str,
    resets_at: Option<i64>,
    profile: String,
}

/// Parked supply for `model` on `profile`: the account is cooling/exhausted,
/// or a window declared on the model itself is cooling.
fn supply_facts(profile: &ProviderRecord, model: &str, now: i64) -> Option<SupplyFacts> {
    let model_windows: Vec<_> = profile
        .models
        .iter()
        .filter(|entry| entry.id == model)
        .flat_map(|entry| entry.windows.iter().cloned())
        .collect();
    let mut windows = profile.supply.windows.clone();
    windows.extend(model_windows.iter().cloned());
    let state = crate::supply::refresh_state(&mut windows, Some(profile.supply.state), now);
    let label = match state {
        remuda_protocol::SupplyState::Cooling => "cooling",
        remuda_protocol::SupplyState::Exhausted => "exhausted",
        _ if model_windows.iter().any(|w| w.is_cooling_at(now)) => "cooling",
        _ => return None,
    };
    // `refresh_state` cleared elapsed cooldowns on `windows`; the survivors
    // are the windows actually parking admissions now.
    let resets_at = windows
        .iter()
        .filter(|window| window.is_cooling_at(now))
        .filter_map(|window| window.resets_at.or(window.cooldown_until))
        .max();
    Some(SupplyFacts {
        state: label,
        resets_at,
        profile: profile.name.clone(),
    })
}

// ── rendering under the byte budget ────────────────────────────────────────

/// Raw (already redacted) text slots and their current character caps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Head,
    Tail,
    Question,
    Args,
    WatchDetail,
    TurnError,
    Reason,
    Branch,
    Name,
    Tool,
    ModelReq,
    ModelEff,
    EffortReq,
    EffortEff,
    Profile,
    Task,
    Sha,
    WorkerId,
    InstanceId,
    PendingId,
    CommandId,
    RequestId,
    Driver,
}

const SLOTS: [(Slot, usize, usize); 23] = [
    // (slot, initial char cap, shrink weight): prose shrinks before ids.
    (Slot::Head, 420, 3),
    (Slot::Tail, 260, 3),
    (Slot::Question, 240, 3),
    (Slot::Args, 320, 2),
    (Slot::WatchDetail, 120, 3),
    (Slot::TurnError, 140, 3),
    (Slot::Reason, 120, 3),
    (Slot::Branch, 96, 1),
    (Slot::Name, 64, 1),
    (Slot::Tool, 48, 1),
    (Slot::ModelReq, 64, 1),
    (Slot::ModelEff, 64, 1),
    (Slot::EffortReq, 24, 1),
    (Slot::EffortEff, 24, 1),
    (Slot::Profile, 48, 1),
    (Slot::Task, 64, 1),
    (Slot::Sha, 64, 1),
    (Slot::WorkerId, 64, 1),
    (Slot::InstanceId, 64, 1),
    (Slot::PendingId, 64, 1),
    (Slot::CommandId, 64, 1),
    (Slot::RequestId, 64, 1),
    (Slot::Driver, 32, 1),
];

/// Everything a digest says, before the byte budget is applied.
struct Parts {
    worker_id: String,
    name: String,
    instance_id: Option<String>,
    harness: String,
    driver: Option<String>,
    roster: String,
    reason: Option<String>,
    lifecycle: Option<String>,
    activity: Option<String>,
    watch: Option<String>,
    watch_detail: Option<String>,
    watch_age_secs: Option<i64>,
    turn: Option<(String, Option<String>, Option<String>)>,
    last_text: Option<String>,
    pending: Option<PendingParts>,
    model_req: Option<String>,
    model_eff: Option<String>,
    effort_req: Option<String>,
    effort_eff: Option<String>,
    configure: Option<(String, String, Option<String>)>,
    supply: Option<SupplyFacts>,
    queued: u32,
    task: Option<String>,
    branch: String,
    sha: Option<String>,
    idle_secs: Option<i64>,
    tool_in_flight: bool,
}

impl Parts {
    fn raw(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Head | Slot::Tail => self.last_text.as_deref(),
            Slot::Question => self.pending.as_ref().map(|p| p.question.as_str()),
            Slot::Args => self.pending.as_ref().and_then(|p| p.args.as_deref()),
            Slot::WatchDetail => self.watch_detail.as_deref(),
            Slot::TurnError => self.turn.as_ref().and_then(|t| t.2.as_deref()),
            Slot::Reason => self.reason.as_deref(),
            Slot::Branch => Some(&self.branch),
            Slot::Name => Some(&self.name),
            Slot::Tool => self.pending.as_ref().and_then(|p| p.tool.as_deref()),
            Slot::ModelReq => self.model_req.as_deref(),
            Slot::ModelEff => self.model_eff.as_deref(),
            Slot::EffortReq => self.effort_req.as_deref(),
            Slot::EffortEff => self.effort_eff.as_deref(),
            Slot::Profile => self.supply.as_ref().map(|s| s.profile.as_str()),
            Slot::Task => self.task.as_deref(),
            Slot::Sha => self.sha.as_deref(),
            Slot::WorkerId => Some(&self.worker_id),
            Slot::InstanceId => self.instance_id.as_deref(),
            Slot::PendingId => self.pending.as_ref().map(|p| p.id.as_str()),
            Slot::CommandId => self.configure.as_ref().map(|c| c.1.as_str()),
            Slot::RequestId => self.configure.as_ref().and_then(|c| c.2.as_deref()),
            Slot::Driver => self.driver.as_deref(),
        }
    }

    /// Characters a slot occupies at `cap` (excerpts count their own share).
    fn used(&self, slot: Slot, cap: usize) -> usize {
        match slot {
            // Head and tail are excerpts of one text; they never overlap, so
            // each is bounded by its own cap and by the text length.
            Slot::Head | Slot::Tail => self
                .raw(slot)
                .map_or(0, |text| text.chars().count().min(cap)),
            _ => self
                .raw(slot)
                .map_or(0, |text| text.chars().count().min(cap)),
        }
    }
}

struct Caps([usize; SLOTS.len()]);

impl Caps {
    fn full() -> Self {
        Self(SLOTS.map(|(_, cap, _)| cap))
    }

    fn get(&self, slot: Slot) -> usize {
        self.0[SLOTS.iter().position(|(s, _, _)| *s == slot).unwrap_or(0)]
    }
}

fn opt_clip(parts: &Parts, caps: &Caps, slot: Slot) -> Option<String> {
    let cap = caps.get(slot);
    if cap == 0 {
        return None;
    }
    parts.raw(slot).map(|text| clip_chars(text, cap))
}

fn req_clip(parts: &Parts, caps: &Caps, slot: Slot) -> String {
    clip_chars(parts.raw(slot).unwrap_or_default(), caps.get(slot))
}

fn render(parts: &Parts, caps: &Caps, truncated: bool) -> WorkerDigest {
    let last_text = parts.last_text.as_deref().map(|text| {
        let chars: Vec<char> = text.chars().collect();
        let head_cap = caps.get(Slot::Head);
        let tail_cap = caps.get(Slot::Tail);
        let (head, tail) = if chars.len() <= head_cap {
            (chars.iter().collect::<String>(), String::new())
        } else if chars.len() <= head_cap + tail_cap {
            (
                chars[..head_cap].iter().collect(),
                chars[head_cap..].iter().collect(),
            )
        } else {
            (
                chars[..head_cap].iter().collect(),
                chars[chars.len() - tail_cap..].iter().collect(),
            )
        };
        DigestText {
            head,
            tail,
            chars: chars.len() as u64,
        }
    });
    WorkerDigest {
        v: 1,
        worker: DigestWorker {
            id: req_clip(parts, caps, Slot::WorkerId),
            name: req_clip(parts, caps, Slot::Name),
            instance_id: opt_clip(parts, caps, Slot::InstanceId),
            harness: clip_chars(&parts.harness, 24),
            driver: opt_clip(parts, caps, Slot::Driver),
        },
        state: DigestState {
            roster: parts.roster.clone(),
            reason: opt_clip(parts, caps, Slot::Reason),
            lifecycle: parts.lifecycle.clone(),
            activity: parts.activity.clone(),
            watch: parts.watch.clone(),
            watch_detail: opt_clip(parts, caps, Slot::WatchDetail),
            watch_age_secs: parts.watch_age_secs,
        },
        turn: parts.turn.as_ref().map(|(status, at, _)| DigestTurn {
            status: clip_chars(status, 32),
            at: at.clone(),
            error: opt_clip(parts, caps, Slot::TurnError),
        }),
        last_text,
        pending: parts.pending.as_ref().map(|pending| DigestPending {
            kind: pending.kind.clone(),
            id: req_clip(parts, caps, Slot::PendingId),
            question: req_clip(parts, caps, Slot::Question),
            tool: opt_clip(parts, caps, Slot::Tool),
            args_untrusted: opt_clip(parts, caps, Slot::Args),
            age_secs: pending.age_secs,
        }),
        model: DigestPair {
            requested: opt_clip(parts, caps, Slot::ModelReq),
            effective: opt_clip(parts, caps, Slot::ModelEff),
        },
        effort: DigestPair {
            requested: opt_clip(parts, caps, Slot::EffortReq),
            effective: opt_clip(parts, caps, Slot::EffortEff),
        },
        configure: parts
            .configure
            .as_ref()
            .map(|(state, _, _)| DigestConfigure {
                state: state.clone(),
                command_id: req_clip(parts, caps, Slot::CommandId),
                request_id: opt_clip(parts, caps, Slot::RequestId),
            }),
        supply: parts.supply.as_ref().map(|supply| DigestSupply {
            parked: true,
            state: supply.state.to_string(),
            resets_at: supply.resets_at,
            profile: opt_clip(parts, caps, Slot::Profile),
        }),
        queued: parts.queued,
        task: opt_clip(parts, caps, Slot::Task),
        branch: req_clip(parts, caps, Slot::Branch),
        sha: opt_clip(parts, caps, Slot::Sha),
        idle_secs: parts.idle_secs,
        tool_in_flight: parts.tool_in_flight,
        truncated,
    }
}

fn encoded_len(digest: &WorkerDigest) -> usize {
    serde_json::to_vec(digest).map_or(usize::MAX, |bytes| bytes.len())
}

/// The smallest document that still names its worker: the last resort when
/// shrinking every text slot to nothing is not enough.
fn skeleton(parts: &Parts) -> WorkerDigest {
    let id: String = parts.worker_id.chars().take(64).collect();
    WorkerDigest {
        v: 1,
        worker: DigestWorker {
            id,
            name: String::new(),
            instance_id: None,
            harness: String::new(),
            driver: None,
        },
        state: DigestState {
            roster: parts.roster.clone(),
            reason: None,
            lifecycle: None,
            activity: None,
            watch: None,
            watch_detail: None,
            watch_age_secs: None,
        },
        turn: None,
        last_text: None,
        pending: None,
        model: DigestPair::default(),
        effort: DigestPair::default(),
        configure: None,
        supply: None,
        queued: parts.queued,
        task: None,
        branch: String::new(),
        sha: None,
        idle_secs: parts.idle_secs,
        tool_in_flight: parts.tool_in_flight,
        truncated: true,
    }
}

/// Render `parts` into at most [`WORKER_DIGEST_MAX_BYTES`] bytes of valid JSON.
///
/// Measures the real serialization (JSON escaping makes quotes, control
/// characters and non-BMP text larger than their character count) and, while
/// over budget, cuts the slot that currently holds the most weighted text to
/// three quarters — so prose shrinks long before identifiers do.
fn fit(parts: &Parts) -> WorkerDigest {
    let mut caps = Caps::full();
    let digest = render(parts, &caps, false);
    if encoded_len(&digest) <= WORKER_DIGEST_MAX_BYTES {
        return digest;
    }
    loop {
        let candidate = SLOTS
            .iter()
            .enumerate()
            .map(|(index, (slot, _, weight))| (index, parts.used(*slot, caps.0[index]) * weight))
            .max_by_key(|(_, weighted)| *weighted);
        let Some((index, weighted)) = candidate else {
            break;
        };
        if weighted == 0 {
            break;
        }
        let slot = SLOTS[index].0;
        let used = parts.used(slot, caps.0[index]);
        caps.0[index] = if used <= 8 { 0 } else { used * 3 / 4 };
        let digest = render(parts, &caps, true);
        if encoded_len(&digest) <= WORKER_DIGEST_MAX_BYTES {
            return digest;
        }
    }
    skeleton(parts)
}

// ── assembling the facts ───────────────────────────────────────────────────

fn watch_label(status: &WorkerWatchStatus) -> &'static str {
    match status {
        WorkerWatchStatus::Working => "working",
        WorkerWatchStatus::Done { .. } => "done",
        WorkerWatchStatus::Blocked { .. } => "blocked",
        WorkerWatchStatus::IdleApiError => "idle-api-error",
        WorkerWatchStatus::Stalled => "stalled",
        WorkerWatchStatus::Gone { .. } => "gone",
        WorkerWatchStatus::Failed { .. } => "failed",
    }
}

/// Everything the digest reads, gathered up front so the build is pure.
struct Facts<'a> {
    worker: &'a WorkerRoster,
    instance: Option<&'a InstanceRecord>,
    journal: &'a [Value],
    pending: &'a [InteractionRecord],
    commands: &'a [CommandRecord],
    profile: Option<&'a ProviderRecord>,
    now: i64,
}

fn build_parts(facts: &Facts<'_>) -> Parts {
    let Facts {
        worker,
        instance,
        journal,
        pending,
        commands,
        profile,
        now,
    } = *facts;
    let tail = scan_tail(journal);
    let (roster, reason, done_sha) = match &worker.state {
        WorkerState::Blocked { reason } => (worker.state.kind(), Some(reason.clone()), None),
        WorkerState::Done { sha } => (worker.state.kind(), None, Some(sha.clone())),
        other => (other.kind(), None, None),
    };
    let sha = done_sha.or_else(|| worker.watch.as_ref().and_then(|w| w.last_done_sha.clone()));
    let watch = worker.watch.as_ref();

    let model_eff = instance
        .and_then(|row| row.model_effective.as_ref())
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| worker.model_effective.clone());
    let model_req = instance
        .and_then(|row| row.model.clone())
        .or_else(|| worker.model.clone());
    let effort_req = instance
        .and_then(|row| row.effort_name.clone())
        .map(|name| {
            if instance.and_then(|row| row.effort_ultracode) == Some(true) {
                format!("{name}+ultracode")
            } else {
                name
            }
        });
    let effort_eff = instance
        .and_then(|row| row.effort_effective.as_ref())
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let configure = commands
        .iter()
        .find(|command| command.operation == "instance.configure" && command.state != "settled")
        .map(|command| {
            let request_id = command
                .payload
                .pointer("/effort/requestId")
                .or_else(|| command.payload.get("requestId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            (
                command.state.clone(),
                command.command_id.clone(),
                request_id,
            )
        });
    let queued = commands
        .iter()
        .filter(|command| command.state == "queued")
        .count() as u32;

    let pending_record = pending
        .iter()
        .find(|row| row.blocking)
        .or_else(|| pending.first());
    let pending_parts = pending_record.map(|row| pending_parts(row, &tail.open_tools, now));

    let supply = profile.and_then(|profile| {
        let model = model_eff
            .as_deref()
            .or(model_req.as_deref())
            .unwrap_or_default();
        supply_facts(profile, model, now)
    });

    let last_progress = tail
        .last_event_at
        .as_deref()
        .and_then(parse_unix)
        .or_else(|| instance.and_then(|row| parse_unix(&row.updated_at)));
    let turn_error = instance
        .and_then(|row| row.last_turn_error.as_ref())
        .map(|marker| redact_secrets(&marker.text));

    Parts {
        worker_id: worker.meta.id.as_id().to_string(),
        name: worker.name.clone(),
        instance_id: worker.instance_id.as_ref().map(|id| id.as_id().to_string()),
        harness: worker.harness.clone(),
        driver: worker.driver.clone(),
        roster: roster.to_string(),
        reason: reason.map(|text| redact_secrets(&text)),
        lifecycle: instance.map(|row| row.lifecycle.clone()),
        activity: instance.map(|row| row.activity.clone()),
        watch: watch.map(|w| watch_label(&w.status).to_string()),
        watch_detail: watch
            .and_then(|w| w.detail.as_deref())
            .map(redact_secrets)
            .filter(|text| !text.is_empty()),
        watch_age_secs: watch
            .and_then(|w| parse_unix(&String::from(w.observed_at.clone())))
            .map(|at| (now - at).max(0)),
        turn: tail.turn.map(|(status, at)| {
            // A result observed in the journal wins; the instance marker
            // supplies the failure text the result event does not carry.
            let error = (status == "error").then(|| turn_error.clone()).flatten();
            (status, at, error)
        }),
        last_text: tail
            .last_text
            .as_deref()
            .map(redact_secrets)
            .filter(|text| !text.is_empty()),
        pending: pending_parts,
        model_req,
        model_eff,
        effort_req,
        effort_eff,
        configure,
        supply,
        queued,
        task: worker.task_id.as_ref().map(|id| id.as_id().to_string()),
        branch: worker.branch.clone(),
        sha,
        idle_secs: last_progress.map(|at| (now - at).max(0)),
        tool_in_flight: !tail.open_tools.is_empty(),
    }
}

/// Build the digest for `worker` from Hub state alone.
pub(crate) async fn collect_digest(
    state: &AppState,
    worker: &WorkerRoster,
) -> Result<WorkerDigest, HubError> {
    let instance_id = worker.instance_id.as_ref().map(|id| id.as_id().to_string());
    let (instance, journal, pending, commands) = match &instance_id {
        Some(id) => {
            let instance = state
                .store
                .get_instance_read(id.clone())
                .await
                .map_err(map_store)?;
            let journal = state
                .store
                .read_journal_tail(id.clone(), JOURNAL_TAIL_EVENTS)
                .await
                .map_err(map_store)?;
            let pending = state
                .store
                .list_interactions(None, Some(id.clone()), None, true)
                .await
                .map_err(map_store)?;
            let commands = state
                .store
                .list_instance_commands(id.clone(), COMMAND_WINDOW)
                .await
                .map_err(map_store)?;
            (instance, journal, pending, commands)
        }
        None => (None, Vec::new(), Vec::new(), Vec::new()),
    };
    let profile_id = worker.provider_profile_id.clone().or_else(|| {
        instance
            .as_ref()
            .and_then(|row| row.provider_profile_id.clone())
    });
    let profile = match profile_id {
        Some(id) => state.store.get_provider(id).await.map_err(map_store)?,
        None => None,
    };
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let parts = build_parts(&Facts {
        worker,
        instance: instance.as_ref(),
        journal: &journal,
        pending: &pending,
        commands: &commands,
        profile: profile.as_ref(),
        now,
    });
    Ok(fit(&parts))
}

// ── glance ─────────────────────────────────────────────────────────────────

fn glance_row(worker: &WorkerRoster, why: Option<(&str, Option<String>)>, now: i64) -> GlanceRow {
    let moved = worker
        .watch
        .as_ref()
        .and_then(|w| w.last_activity_at.clone())
        .map(String::from)
        .or_else(|| Some(String::from(worker.meta.updated_at.clone())))
        .and_then(|stamp| parse_unix(&stamp));
    GlanceRow {
        id: worker.meta.id.as_id().to_string(),
        name: clip_chars(&worker.name, 64),
        state: worker.state.kind().to_string(),
        why: why.as_ref().map(|(code, _)| (*code).to_string()),
        detail: why
            .and_then(|(_, detail)| detail)
            .map(|text| clip_chars(&redact_secrets(&text), 80))
            .filter(|text| !text.is_empty()),
        task: worker
            .task_id
            .as_ref()
            .map(|id| clip_chars(id.as_id().as_str(), 64)),
        branch: clip_chars(&worker.branch, 96),
        idle_secs: moved.map(|at| (now - at).max(0)),
    }
}

/// Sort active workers into waiting (needs someone) and working. Done and
/// retired workers are in neither list.
fn classify_glance(
    workers: &[WorkerRoster],
    pending_by_instance: &HashMap<String, (String, Option<String>)>,
    now: i64,
) -> (Vec<GlanceRow>, Vec<GlanceRow>) {
    let mut waiting = Vec::new();
    let mut working = Vec::new();
    for worker in workers {
        if !worker.state.is_in_progress() && !matches!(worker.state, WorkerState::Blocked { .. }) {
            continue;
        }
        let pending = worker
            .instance_id
            .as_ref()
            .and_then(|id| pending_by_instance.get(id.as_id().as_str()));
        let why: Option<(&str, Option<String>)> = if let Some((kind, detail)) = pending {
            Some((kind.as_str(), detail.clone()))
        } else if let WorkerState::Blocked { reason } = &worker.state {
            Some(("blocked", Some(reason.clone())))
        } else {
            match worker.watch.as_ref().map(|w| &w.status) {
                Some(WorkerWatchStatus::Blocked { reason }) => {
                    Some(("blocked", Some(reason.clone())))
                }
                Some(WorkerWatchStatus::IdleApiError) => Some(("api-error", None)),
                Some(WorkerWatchStatus::Stalled) => Some(("stalled", None)),
                Some(WorkerWatchStatus::Gone { reason }) => Some(("gone", Some(reason.clone()))),
                Some(WorkerWatchStatus::Failed { reason }) => {
                    Some(("failed", Some(reason.clone())))
                }
                _ => None,
            }
        };
        match why {
            Some(why) => waiting.push(glance_row(worker, Some(why), now)),
            None => working.push(glance_row(worker, None, now)),
        }
    }
    (waiting, working)
}

/// Classify, cap and total the rows of one glance.
fn assemble_glance(
    lineage_id: String,
    day: String,
    workers: &[WorkerRoster],
    pending_by_instance: &HashMap<String, (String, Option<String>)>,
    done_today: u32,
    now: i64,
) -> LineageGlance {
    let (mut waiting, mut working) = classify_glance(workers, pending_by_instance, now);
    let waiting_total = u32::try_from(waiting.len()).unwrap_or(u32::MAX);
    let working_total = u32::try_from(working.len()).unwrap_or(u32::MAX);
    waiting.truncate(LINEAGE_GLANCE_ROW_CAP);
    working.truncate(LINEAGE_GLANCE_ROW_CAP);
    LineageGlance {
        lineage_id,
        day,
        waiting,
        waiting_total,
        working,
        working_total,
        done_today,
    }
}

impl Store {
    /// Active (not retired) roster rows for workers that the lineage
    /// dispatched: their instance's creator is a chapter of `lineage_id`.
    pub(crate) async fn list_lineage_workers(
        &self,
        lineage_id: String,
    ) -> Result<Vec<WorkerRoster>, crate::store::StoreError> {
        self.read("list_lineage_workers", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT w.doc_json FROM worker_roster w
                 WHERE w.state_kind IN ('dispatched', 'working', 'blocked')
                   AND (SELECT COALESCE(p.lineage_id, p.id)
                          FROM instances i
                          JOIN instances p
                            ON p.id = json_extract(i.spec_json, '$.parentInstanceId')
                         WHERE i.id = json_extract(w.doc_json, '$.instanceId')) = ?1
                 ORDER BY w.updated_at DESC
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![lineage_id, GLANCE_LOAD_CAP], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.iter()
                .map(|raw| serde_json::from_str(raw).map_err(crate::store::StoreError::from))
                .collect()
        })
        .await
    }

    /// Workers of the lineage that reported DONE at or after `since` (RFC3339).
    pub(crate) async fn count_lineage_done_since(
        &self,
        lineage_id: String,
        since: String,
    ) -> Result<u32, crate::store::StoreError> {
        self.read("count_lineage_done_since", move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM worker_roster w
                 WHERE w.state_kind = 'done' AND w.updated_at >= ?2
                   AND (SELECT COALESCE(p.lineage_id, p.id)
                          FROM instances i
                          JOIN instances p
                            ON p.id = json_extract(i.spec_json, '$.parentInstanceId')
                         WHERE i.id = json_extract(w.doc_json, '$.instanceId')) = ?1",
                params![lineage_id, since],
                |row| row.get(0),
            )?;
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        })
        .await
    }
}

// ── handlers ───────────────────────────────────────────────────────────────

/// Resolve `wkr_…` or an active worker name; an Agent only ever sees workers
/// the lineage read predicate admits.
async fn resolve_readable_worker(
    state: &AppState,
    device: &crate::store::Device,
    id_or_name: &str,
) -> Result<WorkerRoster, HubError> {
    let agent = origin(device) == InputOrigin::Agent;
    let mut candidates = if id_or_name.starts_with("wkr_") {
        vec![
            state
                .store
                .get_worker(id_or_name.to_string())
                .await
                .map_err(map_store)?
                .ok_or(if agent {
                    HubError::Forbidden
                } else {
                    HubError::NotFound
                })?,
        ]
    } else {
        state
            .store
            .find_active_workers_named(id_or_name.to_string())
            .await
            .map_err(map_store)?
    };
    if agent {
        let mut readable = Vec::new();
        for worker in candidates.drain(..) {
            if let Some(instance) = worker.instance_id.as_ref()
                && owns(state, device, instance.as_id().as_str()).await?
            {
                readable.push(worker);
            }
        }
        if readable.is_empty() && id_or_name.starts_with("wkr_") {
            return Err(HubError::Forbidden);
        }
        candidates = readable;
    }
    match candidates.len() {
        0 => Err(HubError::NotFound),
        1 => Ok(candidates.remove(0)),
        _ => Err(HubError::Conflict(format!(
            "worker name {id_or_name} matches multiple projects; use its wkr_ id"
        ))),
    }
}

async fn worker_digest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<WorkerDigest>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    let worker = resolve_readable_worker(&state, &device, &id).await?;
    Ok(Json(collect_digest(&state, &worker).await?))
}

async fn lineage_glance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
) -> Result<Json<LineageGlance>, HubError> {
    require_origin(&headers, &state.config)?;
    let device = caller(&state, &headers).await?;
    let own = match device.instance_id.as_deref() {
        Some(bound) if origin(&device) == InputOrigin::Agent => state
            .store
            .lineage_id_for_instance(bound.to_string())
            .await
            .map_err(map_store)?,
        _ => None,
    };
    let lineage_id = if origin(&device) == InputOrigin::Agent {
        // An Agent reads only its own lineage, by its canonical id.
        if own.as_deref() != Some(lineage_id.as_str()) {
            return Err(HubError::Forbidden);
        }
        lineage_id
    } else {
        // Operators may name any chapter; resolve it to the lineage.
        state
            .store
            .lineage_id_for_instance(lineage_id)
            .await
            .map_err(map_store)?
            .ok_or(HubError::NotFound)?
    };
    let now = OffsetDateTime::now_utc();
    let day_start = now.replace_time(time::Time::MIDNIGHT);
    let day = format!(
        "{:04}-{:02}-{:02}",
        day_start.year(),
        u8::from(day_start.month()),
        day_start.day()
    );
    let workers = state
        .store
        .list_lineage_workers(lineage_id.clone())
        .await
        .map_err(map_store)?;
    let pending = state
        .store
        .list_interactions(None, None, None, true)
        .await
        .map_err(map_store)?;
    let wanted: std::collections::HashSet<&str> = workers
        .iter()
        .filter_map(|worker| worker.instance_id.as_ref())
        .map(|id| id.as_id().as_str())
        .collect();
    let mut pending_by_instance: HashMap<String, (String, Option<String>)> = HashMap::new();
    for row in pending
        .iter()
        .filter(|row| wanted.contains(row.instance_id.as_str()))
    {
        pending_by_instance
            .entry(row.instance_id.clone())
            .or_insert_with(|| {
                let interaction = row
                    .payload
                    .pointer("/payload/interaction")
                    .or_else(|| row.payload.get("interaction"))
                    .unwrap_or(&row.payload);
                let request = interaction.get("request").unwrap_or(&Value::Null);
                let kind = request
                    .get("kind")
                    .and_then(Value::as_str)
                    .or_else(|| interaction.get("kind").and_then(Value::as_str))
                    .unwrap_or(&row.kind);
                let kind = if kind == "permission" {
                    "approval"
                } else {
                    kind
                };
                let title = request
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                (kind.to_string(), title)
            });
    }
    let done_today = state
        .store
        .count_lineage_done_since(lineage_id.clone(), crate::config::rfc3339(day_start))
        .await
        .map_err(map_store)?;
    Ok(Json(assemble_glance(
        lineage_id,
        day,
        &workers,
        &pending_by_instance,
        done_today,
        now.unix_timestamp(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_793_236_200; // 2026-10-29T01:10:00Z

    fn worker(state: Value) -> WorkerRoster {
        let id = remuda_protocol::WorkerRosterId::new().as_id().to_string();
        let project = remuda_protocol::ProjectId::new().as_id().to_string();
        let host = remuda_protocol::HostId::new().as_id().to_string();
        let workspace = remuda_protocol::WorkspaceId::new().as_id().to_string();
        let instance = remuda_protocol::InstanceId::new().as_id().to_string();
        serde_json::from_value(json!({
            "id": id, "revision": "1",
            "createdAt": "2026-10-29T01:00:00.000Z",
            "updatedAt": "2026-10-29T01:00:00.000Z",
            "projectId": project, "name": "c-fixture", "instanceId": instance,
            "hostId": host, "workspaceId": workspace, "harness": "claude",
            "driver": "claude-sdk", "model": "claude-sonnet-5-5",
            "branch": "wt/c-fixture/b-fixture-md", "worktreePath": "/w/c-fixture",
            "state": state,
        }))
        .expect("worker fixture")
    }

    fn instance(extra: Value) -> InstanceRecord {
        let mut row = json!({
            "instanceId": "ins_fixture", "hostId": "hst_fixture", "kind": "claude",
            "driver": "claude-sdk", "lifecycle": "running", "activity": "busy",
            "connectivity": "live", "journalId": "obj_fixture", "durableSeq": "0",
            "configureSeq": 0, "createdAt": "2026-10-29T01:00:00.000Z",
            "updatedAt": "2026-10-29T01:05:00.000Z", "lineageId": "ins_fixture",
            "generation": 1, "scope": {},
        });
        for (key, value) in extra.as_object().expect("object") {
            row[key] = value.clone();
        }
        serde_json::from_value(row).expect("instance fixture")
    }

    fn interaction(payload: Value) -> InteractionRecord {
        InteractionRecord {
            interaction_id: "int_fixture".into(),
            instance_id: "ins_fixture".into(),
            host_id: "hst_fixture".into(),
            kind: "permission".into(),
            state: "pending".into(),
            blocking: true,
            payload,
            created_at: "2026-10-29T01:06:00.000Z".into(),
            updated_at: "2026-10-29T01:06:00.000Z".into(),
        }
    }

    fn command(operation: &str, state: &str, payload: Value) -> CommandRecord {
        serde_json::from_value(json!({
            "commandId": format!("cmd_{operation}_{state}"), "instanceId": "ins_fixture",
            "hostId": "hst_fixture", "operation": operation, "state": state,
            "resolution": "clear", "forwarded": false, "payload": payload,
            "idempotencyKey": null,
            "createdAt": "2026-10-29T01:06:00.000Z", "updatedAt": "2026-10-29T01:06:00.000Z",
        }))
        .expect("command fixture")
    }

    fn approval_event(tool_call_id: &str, title: &str) -> Value {
        json!({ "kind": "interaction.requested", "payload": { "interaction": {
            "id": "int_fixture", "kind": "approval", "state": "pending", "blocking": true,
            "request": { "kind": "approval", "title": title, "description": "run it?",
                         "toolCallId": tool_call_id, "options": [] }
        }}})
    }

    fn tool_call(id: &str, name: &str, input: Value) -> Value {
        json!({ "kind": "tool_call", "observedAt": "2026-10-29T01:05:30.000Z", "payload": {
            "operation": "open", "toolCallId": id,
            "toolName": { "state": "known", "value": name },
            "input": { "state": "known", "value": input },
            "state": "proposed",
        }})
    }

    fn assistant_text(message_id: &str, operation: &str, text: &str) -> Value {
        json!({ "kind": "message", "observedAt": "2026-10-29T01:05:00.000Z", "payload": {
            "operation": operation, "messageId": message_id, "role": "assistant",
            "parentToolCallId": null, "blocks": [{ "type": "text", "text": text }],
        }})
    }

    fn turn_result(status: &str) -> Value {
        json!({ "kind": "lifecycle", "observedAt": "2026-10-29T01:07:00.000Z", "payload": {
            "type": "native", "topic": "turn", "nativeName": "result",
            "status": { "state": "known", "value": status },
        }})
    }

    fn digest_of(facts: &Facts<'_>) -> WorkerDigest {
        fit(&build_parts(facts))
    }

    fn facts<'a>(
        worker: &'a WorkerRoster,
        instance: Option<&'a InstanceRecord>,
        journal: &'a [Value],
        pending: &'a [InteractionRecord],
        commands: &'a [CommandRecord],
        profile: Option<&'a ProviderRecord>,
    ) -> Facts<'a> {
        Facts {
            worker,
            instance,
            journal,
            pending,
            commands,
            profile,
            now: NOW,
        }
    }

    fn assert_valid_and_bounded(digest: &WorkerDigest) -> usize {
        let bytes = serde_json::to_vec(digest).expect("serialize");
        assert!(
            bytes.len() <= WORKER_DIGEST_MAX_BYTES,
            "{} bytes > budget",
            bytes.len()
        );
        // Always parses back as the same document: nothing is cut mid-string.
        let parsed: WorkerDigest = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(&parsed, digest);
        bytes.len()
    }

    #[test]
    fn a_small_digest_is_untouched_and_not_marked_truncated() {
        let worker = worker(json!({ "state": "working" }));
        let journal = vec![assistant_text("m1", "open", "all good so far")];
        let digest = digest_of(&facts(&worker, None, &journal, &[], &[], None));
        assert!(!digest.truncated);
        let text = digest.last_text.expect("text");
        assert_eq!(
            (text.head.as_str(), text.tail.as_str()),
            ("all good so far", "")
        );
        assert_eq!(text.chars, 15);
        assert_valid_and_bounded(&digest_of(&facts(&worker, None, &journal, &[], &[], None)));
    }

    #[test]
    fn cjk_and_oversized_inputs_are_valid_json_within_budget() {
        let worker_row =
            worker(json!({ "state": "blocked", "reason": "等待所有者批准".repeat(400) }));
        // Each shape inflates differently once JSON-escaped: 3-byte CJK,
        // 4-byte emoji, quotes/backslashes (x2), control characters (x6).
        let shapes: Vec<String> = vec![
            "字".repeat(9000),
            "😀".repeat(6000),
            "\"\\".repeat(6000),
            "\u{1}".repeat(6000),
            format!(
                "{}{}{}",
                "头".repeat(3000),
                "中".repeat(3000),
                "尾".repeat(3000)
            ),
        ];
        for shape in shapes {
            let journal = vec![
                assistant_text("m1", "open", &shape),
                tool_call(
                    "tc1",
                    "Bash",
                    json!({ "command": shape, "description": shape }),
                ),
            ];
            let mut approval = approval_event("tc1", &shape);
            approval["payload"]["interaction"]["request"]["description"] = json!(shape);
            let pending = vec![interaction(approval)];
            let commands = vec![command(
                "instance.configure",
                "queued",
                json!({ "effort": { "name": "high", "requestId": shape } }),
            )];
            let mut inst = instance(json!({}));
            inst.model = Some(shape.clone());
            let digest = digest_of(&facts(
                &worker_row,
                Some(&inst),
                &journal,
                &pending,
                &commands,
                None,
            ));
            assert!(digest.truncated, "oversized input must report truncation");
            let len = assert_valid_and_bounded(&digest);
            assert!(len <= WORKER_DIGEST_MAX_BYTES);
            // The identity of the worker and the pending kind survive.
            assert_eq!(digest.worker.name, "c-fixture");
            assert_eq!(
                digest.pending.as_ref().map(|p| p.kind.as_str()),
                Some("approval")
            );
        }
    }

    #[test]
    fn the_skeleton_names_the_worker_and_always_fits() {
        let worker = worker(json!({ "state": "working" }));
        let parts = build_parts(&facts(&worker, None, &[], &[], &[], None));
        let digest = skeleton(&parts);
        assert!(digest.truncated);
        assert!(assert_valid_and_bounded(&digest) < 400);
        assert_eq!(digest.worker.id, parts.worker_id);
    }

    #[test]
    fn head_and_tail_excerpt_a_long_text_and_reassemble_a_medium_one() {
        let worker = worker(json!({ "state": "working" }));
        let long: String = (0..1500)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let journal = vec![assistant_text("m1", "open", &long)];
        let digest = digest_of(&facts(&worker, None, &journal, &[], &[], None));
        let text = digest.last_text.expect("text");
        assert_eq!(text.chars, 1500);
        assert!(long.starts_with(&text.head) && long.ends_with(&text.tail));
        assert!(!text.head.is_empty() && !text.tail.is_empty());
        let medium: String = long.chars().take(600).collect();
        let journal = vec![assistant_text("m1", "open", &medium)];
        let digest = digest_of(&facts(&worker, None, &journal, &[], &[], None));
        let text = digest.last_text.expect("text");
        assert_eq!(format!("{}{}", text.head, text.tail), medium);
    }

    #[test]
    fn redaction_removes_credentials_and_keeps_ordinary_words() {
        let secrets = [
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAA",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            "hunter2hunter2",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
            "0123456789abcdef0123456789abcdef01234567890123456789abcdef012345",
            "Zm9vYmFyQmF6UXV4MTIzNDU2Nzg5MEFCQ0RFRkdISUo",
            "correct horse battery staple",
        ];
        let text = format!(
            "export API_TOKEN={} && curl -H \"Authorization: Bearer {}\" \
             https://alice:{}@example.test/x --password '{}' --api-key {} \
             --secret={} {} {} -H 'X-Api-Key: {}'",
            secrets[0],
            secrets[0],
            secrets[3],
            secrets[7],
            secrets[1],
            secrets[2],
            secrets[4],
            secrets[5],
            secrets[6]
        );
        let redacted = redact_secrets(&text);
        for secret in secrets {
            assert!(!redacted.contains(secret), "leaked {secret}: {redacted}");
        }
        for word in ["horse", "battery", "staple"] {
            assert!(!redacted.contains(word), "leaked word {word}: {redacted}");
        }
        assert!(redacted.contains("curl") && redacted.contains("alice"));
        // Ordinary prose, paths and a git sha-1 survive.
        let plain =
            "cargo test -p remuda-hub at 0a10ebf351aa0a10ebf351aa0a10ebf351aa0a10 in /home/x/y";
        assert_eq!(redact_secrets(plain), plain);
        // Multi-line input collapses to one line.
        assert!(!redact_secrets("a\nb\n\tc").contains('\n'));
        // PEM blocks vanish whole.
        let pem =
            "before -----BEGIN PRIVATE KEY----- MIIEvQIBADANBg kQ0 -----END PRIVATE KEY----- after";
        let redacted = redact_secrets(pem);
        assert!(
            !redacted.contains("MIIEvQ") && redacted.contains("after"),
            "{redacted}"
        );
    }

    #[test]
    fn the_argument_summary_is_bounded_and_withholds_handoff_and_secrets() {
        let handoff = "owner-typed-handoff-value-9917";
        let input = json!({
            "command": format!("deploy --token ghp_abcdefghijklmnopqrstuvwxyz0123456789 {}", "x ".repeat(400)),
            "handoff": handoff,
            "stdin": handoff,
            "password": "hunter2hunter2",
            "env": { "API_KEY": "sk-live-ABCDEFGH12345678" },
            "files": ["a", "b", "c"],
            "timeout": 120,
            "note": "字".repeat(1000),
        });
        let summary = summarize_args(&input);
        assert!(!summary.contains(handoff), "{summary}");
        assert!(!summary.contains("hunter2"), "{summary}");
        assert!(
            !summary.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            "{summary}"
        );
        assert!(!summary.contains("sk-live"), "{summary}");
        assert!(summary.contains("handoff=[withheld]") && summary.contains("password=[withheld]"));
        assert!(summary.contains("files=[3 items]") && summary.contains("env={1 keys}"));
        assert!(
            summary.starts_with("command=deploy"),
            "command leads: {summary}"
        );
        // Every value is bounded; the whole summary cannot grow with the input.
        assert!(
            summary.chars().count() < 8 * ARG_VALUE_CHARS,
            "{}",
            summary.len()
        );
        // A Knowledge-wrapped input is unwrapped the same way.
        let wrapped = json!({ "state": "known", "value": { "command": "ls -la" } });
        assert_eq!(summarize_args(&wrapped), "command=ls -la");
        // A bare string input is redacted too.
        assert!(
            !summarize_args(&json!("echo sk-ant-api03-AAAAAAAAAAAAAAAAAAAA")).contains("sk-ant")
        );
    }

    #[test]
    fn a_pending_approval_quotes_the_tool_and_bounded_arguments() {
        let worker = worker(json!({ "state": "working" }));
        let journal = vec![
            tool_call(
                "tc1",
                "Bash",
                json!({ "command": "sleep 300 && echo done", "token": "abc123def456ghi789jkl012mno345pqr678" }),
            ),
            approval_event("tc1", "Bash"),
        ];
        let pending = vec![interaction(approval_event("tc1", "Bash"))];
        let digest = digest_of(&facts(&worker, None, &journal, &pending, &[], None));
        let pending = digest.pending.expect("pending");
        assert_eq!(pending.kind, "approval");
        assert_eq!(pending.tool.as_deref(), Some("Bash"));
        assert_eq!(pending.question, "run it?");
        let args = pending.args_untrusted.expect("args");
        assert!(args.starts_with("command=sleep 300 && echo done"), "{args}");
        assert!(
            args.contains("token=[withheld]") && !args.contains("abc123def456"),
            "{args}"
        );
        assert_eq!(
            pending.age_secs,
            NOW - parse_unix("2026-10-29T01:06:00.000Z").unwrap()
        );
        assert!(digest.tool_in_flight);
    }

    #[test]
    fn a_sensitive_question_field_shows_only_its_title() {
        let worker = worker(json!({ "state": "working" }));
        let pending = vec![interaction(
            json!({ "kind": "interaction.requested", "payload": {
            "interaction": { "id": "int_fixture", "kind": "question", "request": {
                "kind": "question", "title": "Need input",
                "fields": [
                    { "id": "a", "title": "Which branch?", "sensitive": false,
                      "options": [{ "id": "1", "label": "main" }] },
                    { "id": "b", "title": "Paste the production password", "sensitive": true,
                      "options": [{ "id": "1", "label": "hunter2-default" }] },
                ]
            }}}}),
        )];
        let digest = digest_of(&facts(&worker, None, &[], &pending, &[], None));
        let pending = digest.pending.expect("pending");
        assert_eq!(pending.kind, "question");
        assert!(
            pending.question.starts_with("Which branch? | Paste the"),
            "{}",
            pending.question
        );
        assert!(pending.question.contains("(sensitive)"));
        assert!(!pending.question.contains("hunter2-default"));
        assert!(pending.tool.is_none() && pending.args_untrusted.is_none());
    }

    #[test]
    fn tool_in_flight_follows_calls_results_and_turn_end() {
        let open = vec![tool_call("tc1", "Bash", json!({ "command": "sleep 9" }))];
        assert_eq!(scan_tail(&open).open_tools.len(), 1);
        let mut done = open.clone();
        done.push(json!({ "kind": "tool_result", "payload": {
            "toolCallId": "tc1", "stage": "partial" } }));
        assert_eq!(
            scan_tail(&done).open_tools.len(),
            1,
            "a partial result is not final"
        );
        done.push(json!({ "kind": "tool_result", "payload": {
            "toolCallId": "tc1", "stage": "final" } }));
        assert!(scan_tail(&done).open_tools.is_empty());
        // A turn result abandons whatever was still open.
        let mut ended = open.clone();
        ended.push(turn_result("turn_done"));
        let tail = scan_tail(&ended);
        assert!(tail.open_tools.is_empty());
        assert_eq!(tail.turn.as_ref().map(|t| t.0.as_str()), Some("turn_done"));
        assert_eq!(
            tail.turn.and_then(|t| t.1).as_deref(),
            Some("2026-10-29T01:07:00.000Z")
        );
    }

    #[test]
    fn message_folding_follows_append_replace_and_skips_subagents() {
        let events = vec![
            assistant_text("m1", "open", "hel"),
            assistant_text("m1", "append", "lo"),
            json!({ "kind": "message", "payload": { "operation": "open", "messageId": "sub",
                "role": "assistant", "parentToolCallId": "tc9",
                "blocks": [{ "type": "text", "text": "subagent chatter" }] }}),
            json!({ "kind": "message", "payload": { "operation": "open", "messageId": "u1",
                "role": "user", "parentToolCallId": null,
                "blocks": [{ "type": "text", "text": "the user said this" }] }}),
        ];
        assert_eq!(scan_tail(&events).last_text.as_deref(), Some("hello"));
        let mut replaced = events;
        replaced.push(assistant_text("m1", "replace", "final answer"));
        assert_eq!(
            scan_tail(&replaced).last_text.as_deref(),
            Some("final answer")
        );
    }

    #[test]
    fn an_errored_turn_carries_the_instance_marker_text() {
        let worker = worker(json!({ "state": "working" }));
        let inst = instance(json!({ "lastTurnError": {
            "at": "2026-10-29T01:07:00.000Z", "text": "API Error: 429 rate limited" } }));
        let journal = vec![turn_result("error")];
        let digest = digest_of(&facts(&worker, Some(&inst), &journal, &[], &[], None));
        let turn = digest.turn.expect("turn");
        assert_eq!(turn.status, "error");
        assert_eq!(turn.error.as_deref(), Some("API Error: 429 rate limited"));
    }

    fn provider(windows: Value, state: &str) -> ProviderRecord {
        serde_json::from_value(json!({
            "id": "pvp_fixture", "name": "acme-gateway", "kind": "gateway",
            "baseUrl": "https://gateway.invalid", "models": [{ "id": "claude-sonnet-5-5" }],
            "headers": {}, "defaultGateway": false, "scope": "universal", "revision": 1,
            "secretPresent": false,
            "supply": { "state": state, "windows": windows },
            "createdAt": "2026-10-29T01:00:00.000Z", "updatedAt": "2026-10-29T01:00:00.000Z",
        }))
        .expect("provider fixture")
    }

    #[test]
    fn a_parked_supply_shows_its_reset_time() {
        let worker = worker(json!({ "state": "working" }));
        let reset = NOW + 1800;
        let parked = provider(
            json!([{ "id": "primary", "appliesTo": ["*"], "usedPercent": 100.0,
                     "resetsAt": reset, "cooldownUntil": reset, "source": "observed" }]),
            "available",
        );
        let digest = digest_of(&facts(&worker, None, &[], &[], &[], Some(&parked)));
        let supply = digest.supply.expect("parked supply");
        assert!(supply.parked);
        assert_eq!(supply.state, "cooling");
        assert_eq!(supply.resets_at, Some(reset));
        assert_eq!(supply.profile.as_deref(), Some("acme-gateway"));
        // A backoff-only park (no reset time known) is parked without resetsAt.
        let backoff = provider(
            json!([{ "id": "primary", "appliesTo": ["*"], "cooldownUntil": NOW + 60,
                     "source": "observed" }]),
            "available",
        );
        let digest = digest_of(&facts(&worker, None, &[], &[], &[], Some(&backoff)));
        let supply = digest.supply.expect("backoff park");
        assert_eq!((supply.parked, supply.resets_at), (true, Some(NOW + 60)));
        // An elapsed cooldown, or a healthy profile, shows nothing.
        let elapsed = provider(
            json!([{ "id": "primary", "appliesTo": ["*"], "cooldownUntil": NOW - 5,
                     "resetsAt": NOW - 5, "source": "observed" }]),
            "available",
        );
        assert!(
            digest_of(&facts(&worker, None, &[], &[], &[], Some(&elapsed)))
                .supply
                .is_none()
        );
        assert!(
            digest_of(&facts(
                &worker,
                None,
                &[],
                &[],
                &[],
                Some(&provider(json!([]), "available"))
            ))
            .supply
            .is_none()
        );
    }

    #[test]
    fn a_pending_configure_shows_its_state_and_request_id() {
        let worker = worker(json!({ "state": "working" }));
        let inst = instance(json!({
            "model": "claude-sonnet-5-5", "effortName": "medium",
            "modelEffective": { "id": "claude-sonnet-5-5", "source": "launch",
                                "observedAt": "2026-10-29T01:00:00.000Z" },
            "effortEffective": { "name": "low", "source": "assistant",
                                 "observedAt": "2026-10-29T01:00:00.000Z" },
        }));
        let commands = vec![
            command("instance.send", "queued", json!({ "text": "hi" })),
            command(
                "instance.configure",
                "queued",
                json!({ "effort": { "name": "medium", "requestId": "effreq-abc" } }),
            ),
            command(
                "instance.configure",
                "settled",
                json!({ "effort": { "name": "low" } }),
            ),
        ];
        let digest = digest_of(&facts(&worker, Some(&inst), &[], &[], &commands, None));
        let configure = digest.configure.expect("pending configure");
        assert_eq!(configure.state, "queued");
        assert_eq!(configure.request_id.as_deref(), Some("effreq-abc"));
        assert_eq!(configure.command_id, "cmd_instance.configure_queued");
        assert_eq!(
            digest.queued, 2,
            "the send and the configure are still queued"
        );
        assert_eq!(digest.effort.requested.as_deref(), Some("medium"));
        assert_eq!(digest.effort.effective.as_deref(), Some("low"));
        assert_eq!(digest.model.requested.as_deref(), Some("claude-sonnet-5-5"));
        // Settled configures are not pending.
        let settled = vec![command("instance.configure", "settled", json!({}))];
        assert!(
            digest_of(&facts(&worker, Some(&inst), &[], &[], &settled, None))
                .configure
                .is_none()
        );
    }

    #[test]
    fn glance_splits_waiting_from_working_and_caps_each_list() {
        let mut workers = Vec::new();
        for index in 0..120 {
            let mut row = if index % 2 == 0 {
                worker(json!({ "state": "working" }))
            } else {
                worker(json!({ "state": "blocked", "reason": format!("need input {index}") }))
            };
            row.name = format!("w{index}");
            workers.push(row);
        }
        // A done worker is in neither list.
        workers.push(worker(json!({ "state": "done", "sha": "0a10ebf351aa" })));
        let glance = assemble_glance(
            "ins_lineage".into(),
            "2026-10-29".into(),
            &workers,
            &HashMap::new(),
            7,
            NOW,
        );
        assert_eq!((glance.waiting_total, glance.working_total), (60, 60));
        assert_eq!(glance.waiting.len(), LINEAGE_GLANCE_ROW_CAP);
        assert_eq!(glance.working.len(), LINEAGE_GLANCE_ROW_CAP);
        assert_eq!(glance.done_today, 7);
        assert!(
            glance
                .waiting
                .iter()
                .all(|row| row.why.as_deref() == Some("blocked"))
        );
        assert!(glance.working.iter().all(|row| row.why.is_none()));
        // A pending approval makes an otherwise-working worker wait.
        let one = worker(json!({ "state": "working" }));
        let mut pending = HashMap::new();
        pending.insert(
            one.instance_id.as_ref().unwrap().as_id().to_string(),
            ("approval".to_string(), Some("Bash".to_string())),
        );
        let glance = assemble_glance("l".into(), "d".into(), &[one], &pending, 0, NOW);
        assert_eq!(glance.waiting[0].why.as_deref(), Some("approval"));
        assert_eq!(glance.waiting[0].detail.as_deref(), Some("Bash"));
        assert!(glance.working.is_empty());
    }

    #[test]
    fn route_predicates_match_only_the_digest_and_glance_shapes() {
        assert_eq!(
            digest_read_target_for_test("/v1/workers/wkr_1/digest"),
            Some("wkr_1")
        );
        for path in [
            "/v1/workers",
            "/v1/workers/wkr_1",
            "/v1/workers/wkr_1/nudge",
            "/v1/workers/wkr_1/digest/extra",
            "/v1/workers//digest",
            "/v1/workers/a/b/digest",
        ] {
            assert_eq!(digest_read_target_for_test(path), None, "{path}");
        }
    }

    fn digest_read_target_for_test(path: &str) -> Option<&str> {
        crate::agent_scope::digest_read_target(path)
    }
}
