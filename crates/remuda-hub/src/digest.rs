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
    DigestConfigure, DigestEffort, DigestModel, DigestPending, DigestRoster, DigestSupply,
    DigestText, DigestTurn, GlanceMore, GlanceWaiting, GlanceWorking, InputOrigin,
    LINEAGE_GLANCE_ROW_CAP, LineageGlance, WORKER_DIGEST_MAX_BYTES, WorkerDigest, WorkerRoster,
    WorkerState, WorkerWatchStatus,
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
///
/// The prefix table must cover every token-prefix rule in
/// `scripts/ci/secret-scan.py` (D-060: "the same set of candidates"); the
/// parity test below reads the script and fails when one is missing. Prefixes
/// match case-insensitively: a redactor errs toward withholding.
fn is_secret_shaped(token: &str) -> bool {
    let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    const PREFIXES: &[&str] = &[
        // Remuda's own agent token, and the shapes the repo scanner knows.
        "agk_",
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
        "xai-",
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
    if PREFIXES.iter().any(|prefix| {
        token
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            && token.len() >= prefix.len() + 8
    }) {
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
            // `--header=Bearer <token>` and `--header=Authorization: Bearer`:
            // this word names the scheme or the header, and the credential is
            // the NEXT word. Returning here without re-arming the scrubber
            // would leave that token to the shape heuristic alone.
            let value_bare = value.trim_matches(|c: char| matches!(c, '"' | '\''));
            let names_a_secret_header = value_bare
                .strip_suffix(':')
                .is_some_and(|header| looks_like_name(header) && is_secret_name(header));
            if value_bare.eq_ignore_ascii_case("bearer")
                || value_bare.eq_ignore_ascii_case("basic")
                || names_a_secret_header
            {
                scrub.next_secret = true;
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
    question: String,
    tool: Option<String>,
    args: Option<String>,
}

fn parse_unix(stamp: &str) -> Option<i64> {
    OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .map(|at| at.unix_timestamp())
        .ok()
}

fn pending_parts(record: &InteractionRecord, open: &[OpenTool]) -> PendingParts {
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
        question: redact_secrets(&question),
        tool,
        // Every value in the summary was redacted as it was rendered; a second
        // pass would rewrite the `[withheld]` markers.
        args: args.filter(|args| !args.is_empty()),
    }
}

// ── supply ─────────────────────────────────────────────────────────────────

struct SupplyFacts {
    window_id: Option<String>,
    resets_at: Option<i64>,
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
    let parked = matches!(
        state,
        remuda_protocol::SupplyState::Cooling | remuda_protocol::SupplyState::Exhausted
    ) || model_windows.iter().any(|w| w.is_cooling_at(now));
    if !parked {
        return None;
    }
    // `refresh_state` cleared elapsed cooldowns on `windows`; the survivors
    // are the windows actually parking admissions now.
    let resets_at = windows
        .iter()
        .filter(|window| window.is_cooling_at(now))
        .filter_map(|window| window.resets_at.or(window.cooldown_until))
        .max();
    let window_id = windows
        .iter()
        .filter(|window| window.is_cooling_at(now))
        .max_by_key(|window| window.resets_at.or(window.cooldown_until).unwrap_or(0))
        .map(|window| window.id.clone());
    Some(SupplyFacts {
        window_id,
        resets_at,
    })
}

// ── rendering under the byte budget ────────────────────────────────────────

/// Raw (already redacted) text slots and their current character caps.
///
/// Only text the document actually carries is a slot: `shrunk` names a field
/// whose rendered text is shorter than its source, so a value the renderer
/// never emits must not consume a slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Head,
    Tail,
    Question,
    Args,
    Branch,
    Name,
    Tool,
    ModelReq,
    ModelEff,
    EffortReq,
    EffortEff,
    Task,
    Sha,
    ProjectId,
    WorkerId,
    InstanceId,
}

const SLOTS: [(Slot, usize, usize); 14] = [
    // (slot, initial char cap, shrink weight): prose shrinks before ids.
    (Slot::Head, 420, 3),
    (Slot::Tail, 260, 3),
    (Slot::Question, 240, 3),
    (Slot::Args, 320, 2),
    (Slot::Branch, 96, 1),
    (Slot::Name, 64, 1),
    (Slot::Tool, 48, 1),
    (Slot::ModelReq, 64, 1),
    (Slot::ModelEff, 64, 1),
    (Slot::EffortReq, 24, 1),
    (Slot::EffortEff, 24, 1),
    (Slot::Task, 64, 1),
    (Slot::Sha, 64, 1),
    (Slot::ProjectId, 64, 1),
];

/// Slots that identify the worker. They are never shortened: a digest that
/// cannot name its worker is useless, and the skeleton is the fallback.
const UNSHRINKABLE: &[Slot] = &[
    Slot::Name,
    Slot::WorkerId,
    Slot::InstanceId,
    Slot::ProjectId,
];

/// Everything a digest says, before the byte budget is applied.
struct Parts {
    worker_id: String,
    name: String,
    instance_id: Option<String>,
    roster: String,
    watch: Option<String>,
    turn: Option<(String, Option<String>)>,
    last_text: Option<String>,
    pending: Option<PendingParts>,
    model_req: Option<String>,
    model_eff: Option<String>,
    effort_req: Option<String>,
    effort_eff: Option<String>,
    configure: Option<(String, Option<String>)>,
    supply: Option<SupplyFacts>,
    queued: u32,
    task: Option<String>,
    project_id: Option<String>,
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
            Slot::Branch => Some(&self.branch),
            Slot::Name => Some(&self.name),
            Slot::Tool => self.pending.as_ref().and_then(|p| p.tool.as_deref()),
            Slot::ModelReq => self.model_req.as_deref(),
            Slot::ModelEff => self.model_eff.as_deref(),
            Slot::EffortReq => self.effort_req.as_deref(),
            Slot::EffortEff => self.effort_eff.as_deref(),
            Slot::Task => self.task.as_deref(),
            Slot::Sha => self.sha.as_deref(),
            Slot::ProjectId => self.project_id.as_deref(),
            Slot::WorkerId => Some(&self.worker_id),
            Slot::InstanceId => self.instance_id.as_deref(),
        }
    }

    /// Characters a slot occupies at `cap` (an excerpt is bounded by its cap
    /// and by the text length; head and tail never overlap).
    fn used(&self, slot: Slot, cap: usize) -> usize {
        self.raw(slot)
            .map_or(0, |text| text.chars().count().min(cap))
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

/// Whether a field's rendered text is shorter than its source.
fn cut(raw: Option<&str>, shown: Option<&str>) -> bool {
    match (raw, shown) {
        (None, _) => false,
        (Some(raw), None) => !raw.is_empty(),
        (Some(raw), Some(shown)) => shown.chars().count() < raw.chars().count(),
    }
}

/// Fields whose rendered text is shorter than the source, in wire order.
///
/// Derived from what the document actually says, not from the caps: a medium
/// `lastText` reassembles whole from head + tail, so it was not shortened, and
/// a value the renderer never emits cannot be listed at all.
fn shrunk_fields(digest: &WorkerDigest, parts: &Parts) -> Vec<String> {
    let mut shrunk: Vec<&'static str> = Vec::new();
    {
        let mut note = |field: &'static str, raw: Option<&str>, shown: Option<&str>| {
            if cut(raw, shown) && !shrunk.contains(&field) {
                shrunk.push(field);
            }
        };
        let text = parts.last_text.as_deref();
        let shown = digest
            .last_text
            .as_ref()
            .map(|text| format!("{}{}", text.head, text.tail));
        note("lastText", text, shown.as_deref());
        let pending = parts.pending.as_ref();
        note(
            "question",
            pending.map(|p| p.question.as_str()),
            digest.pending.as_ref().map(|p| p.question.as_str()),
        );
        note(
            "argsSummary",
            pending.and_then(|p| p.args.as_deref()),
            digest
                .pending
                .as_ref()
                .and_then(|p| p.args_summary.as_deref()),
        );
        note(
            "tool",
            pending.and_then(|p| p.tool.as_deref()),
            digest.pending.as_ref().and_then(|p| p.tool.as_deref()),
        );
        note(
            "branch",
            Some(parts.branch.as_str()),
            Some(digest.branch.as_str()),
        );
        note(
            "name",
            Some(parts.name.as_str()),
            Some(digest.name.as_str()),
        );
        note("taskId", parts.task.as_deref(), digest.task_id.as_deref());
        note("sha", parts.sha.as_deref(), digest.sha.as_deref());
        note(
            "projectId",
            parts.project_id.as_deref(),
            digest.project_id.as_deref(),
        );
        note(
            "workerId",
            Some(parts.worker_id.as_str()),
            Some(digest.worker_id.as_str()),
        );
        note(
            "instanceId",
            parts.instance_id.as_deref(),
            digest.instance_id.as_deref(),
        );
        note(
            "model",
            parts.model_req.as_deref(),
            digest.model.requested.as_deref(),
        );
        note(
            "model",
            parts.model_eff.as_deref(),
            digest.model.effective.as_deref(),
        );
        note(
            "effort",
            parts.effort_req.as_deref(),
            digest.model.effort.requested.as_deref(),
        );
        note(
            "effort",
            parts.effort_eff.as_deref(),
            digest.model.effort.effective.as_deref(),
        );
        note(
            "turn",
            parts.turn.as_ref().map(|turn| turn.0.as_str()),
            digest.turn.as_ref().map(|turn| turn.result.as_str()),
        );
        note(
            "configure",
            parts.configure.as_ref().and_then(|(_, id)| id.as_deref()),
            digest
                .model
                .configure
                .as_ref()
                .and_then(|configure| configure.request_id.as_deref()),
        );
        note(
            "supply",
            parts.supply.as_ref().and_then(|s| s.window_id.as_deref()),
            digest.supply.as_ref().and_then(|s| s.window_id.as_deref()),
        );
    }
    shrunk.into_iter().map(str::to_string).collect()
}

fn render(parts: &Parts, caps: &Caps) -> WorkerDigest {
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
        DigestText { head, tail }
    });
    let mut digest = WorkerDigest {
        v: 1,
        worker_id: parts.worker_id.clone(),
        instance_id: parts.instance_id.clone(),
        name: req_clip(parts, caps, Slot::Name),
        project_id: opt_clip(parts, caps, Slot::ProjectId),
        task_id: opt_clip(parts, caps, Slot::Task),
        branch: req_clip(parts, caps, Slot::Branch),
        sha: opt_clip(parts, caps, Slot::Sha),
        roster: DigestRoster {
            state: parts.roster.clone(),
            watch: parts.watch.clone(),
        },
        turn: parts.turn.as_ref().map(|(status, at)| DigestTurn {
            result: clip_chars(status, 32),
            ended_at: at.clone(),
        }),
        last_text,
        pending: parts.pending.as_ref().map(|pending| DigestPending {
            kind: pending.kind.clone(),
            question: req_clip(parts, caps, Slot::Question),
            tool: opt_clip(parts, caps, Slot::Tool),
            args_summary: opt_clip(parts, caps, Slot::Args),
        }),
        model: DigestModel {
            requested: opt_clip(parts, caps, Slot::ModelReq),
            effective: opt_clip(parts, caps, Slot::ModelEff),
            effort: DigestEffort {
                requested: opt_clip(parts, caps, Slot::EffortReq),
                effective: opt_clip(parts, caps, Slot::EffortEff),
            },
            configure: parts
                .configure
                .as_ref()
                .map(|(state, request_id)| DigestConfigure {
                    state: state.clone(),
                    request_id: request_id.clone(),
                }),
        },
        supply: parts.supply.as_ref().map(|supply| DigestSupply {
            window_id: supply.window_id.clone(),
            resets_at: supply.resets_at,
        }),
        queued: parts.queued,
        since_progress_secs: parts.idle_secs,
        tool_in_flight: parts.tool_in_flight,
        shrunk: Vec::new(),
    };
    digest.shrunk = shrunk_fields(&digest, parts);
    digest
}

fn encoded_len(digest: &WorkerDigest) -> usize {
    serde_json::to_vec(digest).map_or(usize::MAX, |bytes| bytes.len())
}

/// The smallest document that still names its worker: the last resort when
/// shrinking every text slot to nothing is not enough.
fn skeleton(parts: &Parts) -> WorkerDigest {
    let mut digest = WorkerDigest {
        v: 1,
        worker_id: parts.worker_id.chars().take(64).collect(),
        instance_id: None,
        name: parts.name.chars().take(64).collect(),
        project_id: None,
        task_id: None,
        branch: String::new(),
        sha: None,
        roster: DigestRoster {
            state: parts.roster.clone(),
            watch: None,
        },
        turn: None,
        last_text: None,
        pending: parts.pending.as_ref().map(|pending| DigestPending {
            kind: pending.kind.clone(),
            question: String::new(),
            tool: None,
            args_summary: None,
        }),
        model: DigestModel::default(),
        supply: None,
        queued: parts.queued,
        since_progress_secs: parts.idle_secs,
        tool_in_flight: parts.tool_in_flight,
        shrunk: Vec::new(),
    };
    digest.shrunk = shrunk_fields(&digest, parts);
    digest
}

/// Render `parts` into at most [`WORKER_DIGEST_MAX_BYTES`] bytes of valid JSON.
///
/// Measures the real serialization (JSON escaping makes quotes, control
/// characters and non-BMP text larger than their character count) and, while
/// over budget, cuts the slot that currently holds the most weighted text to
/// three quarters — so prose shrinks long before identifiers do.
fn fit(parts: &Parts) -> WorkerDigest {
    let mut caps = Caps::full();
    let digest = render(parts, &caps);
    if encoded_len(&digest) <= WORKER_DIGEST_MAX_BYTES {
        return digest;
    }
    loop {
        let candidate = SLOTS
            .iter()
            .enumerate()
            .filter(|(_, (slot, _, _))| !UNSHRINKABLE.contains(slot))
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
        let digest = render(parts, &caps);
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
    // Only the state KIND is serialized: a blocked reason and a watch detail
    // stay on the row for `remuda watch`, never in this bounded document.
    let (roster, done_sha) = match &worker.state {
        WorkerState::Done { sha } => (worker.state.kind(), Some(sha.clone())),
        other => (other.kind(), None),
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
            (command.state.clone(), request_id)
        });
    let queued = commands
        .iter()
        .filter(|command| command.state == "queued")
        .count() as u32;

    let pending_record = pending
        .iter()
        .find(|row| row.blocking)
        .or_else(|| pending.first());
    let pending_parts = pending_record.map(|row| pending_parts(row, &tail.open_tools));

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

    Parts {
        worker_id: worker.meta.id.as_id().to_string(),
        name: worker.name.clone(),
        instance_id: worker.instance_id.as_ref().map(|id| id.as_id().to_string()),
        roster: roster.to_string(),
        watch: watch.map(|w| watch_label(&w.status).to_string()),
        turn: tail.turn,
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
        project_id: Some(worker.project_id.as_id().to_string()),
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

fn glance_since(worker: &WorkerRoster, now: i64) -> Option<i64> {
    worker
        .watch
        .as_ref()
        .and_then(|w| w.last_activity_at.clone())
        .map(String::from)
        .or_else(|| Some(String::from(worker.meta.updated_at.clone())))
        .and_then(|stamp| parse_unix(&stamp))
        .map(|at| (now - at).max(0))
}

fn glance_instance(worker: &WorkerRoster) -> Option<String> {
    worker.instance_id.as_ref().map(|id| id.as_id().to_string())
}

fn glance_waiting(
    worker: &WorkerRoster,
    kind: &str,
    question: Option<String>,
    now: i64,
) -> GlanceWaiting {
    GlanceWaiting {
        instance_id: glance_instance(worker),
        name: clip_chars(&worker.name, 64),
        since: glance_since(worker, now),
        kind: kind.to_string(),
        question: question
            .map(|text| clip_chars(&redact_secrets(&text), 80))
            .filter(|text| !text.is_empty()),
    }
}

fn glance_working(worker: &WorkerRoster, now: i64) -> GlanceWorking {
    GlanceWorking {
        instance_id: glance_instance(worker),
        name: clip_chars(&worker.name, 64),
        since: glance_since(worker, now),
        task_id: worker
            .task_id
            .as_ref()
            .map(|id| clip_chars(id.as_id().as_str(), 64)),
    }
}

/// Sort active workers into waiting (needs someone) and working. Done and
/// retired workers are in neither list.
fn classify_glance(
    workers: &[WorkerRoster],
    pending_by_instance: &HashMap<String, (String, Option<String>)>,
    now: i64,
) -> (Vec<GlanceWaiting>, Vec<GlanceWorking>) {
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
            Some((kind, question)) => waiting.push(glance_waiting(worker, kind, question, now)),
            None => working.push(glance_working(worker, now)),
        }
    }
    (waiting, working)
}

/// Classify, cap and total the rows of one glance.
fn assemble_glance(
    lineage_id: String,
    generated_at: String,
    workers: &[WorkerRoster],
    pending_by_instance: &HashMap<String, (String, Option<String>)>,
    done_today: u32,
    now: i64,
) -> LineageGlance {
    let (mut waiting, mut working) = classify_glance(workers, pending_by_instance, now);
    let waiting_more =
        u32::try_from(waiting.len().saturating_sub(LINEAGE_GLANCE_ROW_CAP)).unwrap_or(u32::MAX);
    let working_more =
        u32::try_from(working.len().saturating_sub(LINEAGE_GLANCE_ROW_CAP)).unwrap_or(u32::MAX);
    waiting.truncate(LINEAGE_GLANCE_ROW_CAP);
    working.truncate(LINEAGE_GLANCE_ROW_CAP);
    LineageGlance {
        lineage_id,
        generated_at,
        waiting,
        working,
        more: GlanceMore {
            waiting: waiting_more,
            working: working_more,
        },
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
    ///
    /// `doneAt` is the transition stamp, not `updated_at`: every watch pass
    /// rewrites a roster row, so counting writes would report a worker that
    /// finished last week as today's work.
    pub(crate) async fn count_lineage_done_since(
        &self,
        lineage_id: String,
        since: String,
    ) -> Result<u32, crate::store::StoreError> {
        self.read("count_lineage_done_since", move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM worker_roster w
                 WHERE w.state_kind = 'done' AND json_extract(w.doc_json, '$.doneAt') >= ?2
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
        crate::config::rfc3339(now),
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
        assert!(digest.shrunk.is_empty());
        let text = digest.last_text.expect("text");
        assert_eq!(
            (text.head.as_str(), text.tail.as_str()),
            ("all good so far", "")
        );
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
            assert!(
                !digest.shrunk.is_empty(),
                "oversized input must report shrinkage"
            );
            let len = assert_valid_and_bounded(&digest);
            assert!(len <= WORKER_DIGEST_MAX_BYTES);
            // The identity of the worker and the pending kind survive.
            assert_eq!(digest.name, "c-fixture");
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
        assert_eq!(digest.worker_id, parts.worker_id);
        // The id and the name survive whole, so they are not listed; the
        // fields the skeleton dropped are, because their content did go.
        assert!(
            !digest.shrunk.contains(&"workerId".to_string())
                && !digest.shrunk.contains(&"name".to_string()),
            "{:?}",
            digest.shrunk
        );
        for field in ["branch", "projectId", "instanceId", "model"] {
            assert!(
                digest.shrunk.contains(&field.to_string()),
                "the skeleton dropped {field}: {:?}",
                digest.shrunk
            );
        }
        assert!(assert_valid_and_bounded(&digest) < 400);
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
        assert!(long.starts_with(&text.head) && long.ends_with(&text.tail));
        assert!(!text.head.is_empty() && !text.tail.is_empty());
        let medium: String = long.chars().take(600).collect();
        let journal = vec![assistant_text("m1", "open", &medium)];
        let digest = digest_of(&facts(&worker, None, &journal, &[], &[], None));
        let text = digest.last_text.expect("text");
        assert_eq!(format!("{}{}", text.head, text.tail), medium);
    }

    #[test]
    fn shrunk_names_only_fields_whose_text_was_actually_cut() {
        let row = worker(json!({ "state": "working" }));
        // 600 characters reassemble fully from head (420) + tail (260): nothing
        // was cut, so the document must not claim it was.
        let medium: String = (0..600)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let journal = vec![assistant_text("m1", "open", &medium)];
        let digest = digest_of(&facts(&row, None, &journal, &[], &[], None));
        let text = digest.last_text.as_ref().expect("text");
        assert_eq!(format!("{}{}", text.head, text.tail), medium);
        assert!(digest.shrunk.is_empty(), "nothing was cut: {digest:?}");

        // A long blocked reason and a long watch detail are never serialized,
        // so they cannot be reported as shortened either.
        let blocked_row = worker(json!({ "state": "blocked", "reason": "r".repeat(500) }));
        let blocked = digest_of(&facts(&blocked_row, None, &[], &[], &[], None));
        assert!(
            blocked.shrunk.is_empty(),
            "unserialized text must not be listed: {blocked:?}"
        );
        let mut watched_row = worker(json!({ "state": "working" }));
        watched_row.watch = Some(remuda_protocol::WorkerWatch {
            status: remuda_protocol::WorkerWatchStatus::Working,
            observed_at: remuda_protocol::Timestamp::try_from(
                "2026-10-29T01:05:00.000Z".to_string(),
            )
            .expect("timestamp"),
            detail: Some("d".repeat(500)),
            last_done_sha: None,
            last_blocked: None,
            last_screen_digest: None,
            last_activity_at: None,
        });
        let watched = digest_of(&facts(&watched_row, None, &[], &[], &[], None));
        assert!(
            watched.shrunk.is_empty(),
            "an unrendered watch detail must not be listed: {watched:?}"
        );

        // The skeleton drops real content, so it names exactly what went, and
        // never a field it kept intact.
        let journal = vec![assistant_text("m1", "open", "hello")];
        let parts = build_parts(&facts(&row, None, &journal, &[], &[], None));
        let skeleton = skeleton(&parts);
        assert_eq!(skeleton.worker_id, parts.worker_id);
        assert!(
            !skeleton.shrunk.contains(&"workerId".to_string()),
            "the worker id was kept whole: {:?}",
            skeleton.shrunk
        );
        assert!(
            skeleton.shrunk.contains(&"lastText".to_string()),
            "the skeleton dropped the text: {:?}",
            skeleton.shrunk
        );
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

    /// The digest must exclude the same candidates `scripts/ci/secret-scan.py`
    /// flags. Read the script's own prefix table, so a rule added there fails
    /// here until the redactor covers it.
    #[test]
    fn the_redactor_covers_every_secret_scan_prefix() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/ci/secret-scan.py"),
        )
        .expect("scripts/ci/secret-scan.py is part of the repo");
        let mut literals: Vec<(String, String)> = Vec::new();
        for line in source.lines() {
            let Some(rest) = line.trim().strip_prefix("P_") else {
                continue;
            };
            let Some((name, expression)) = rest.split_once(" = ") else {
                continue;
            };
            let mut value = String::new();
            for part in expression.split(" + ") {
                let Some(literal) = part
                    .trim()
                    .strip_prefix('"')
                    .and_then(|p| p.strip_suffix('"'))
                else {
                    value.clear();
                    break;
                };
                value.push_str(literal);
            }
            if !value.is_empty() {
                literals.push((name.to_string(), value));
            }
        }
        assert!(
            literals.len() >= 10,
            "parsed only {} prefixes from secret-scan.py",
            literals.len()
        );
        const TOKEN: &str = "q7Zr2LmW8pXc4VbN1";
        for (name, literal) in literals {
            let text = match literal.as_str() {
                "Bearer " => format!("curl -H 'Authorization: Bearer {TOKEN}'"),
                "BEGIN PRIVATE KEY" => {
                    format!("-----{literal}----- MIIEvQIBADANBg -----END PRIVATE KEY-----")
                }
                // A `NAME=` rule: the credential follows the name directly.
                _ if literal.ends_with('=') => format!("{literal}{TOKEN}"),
                // A bare-name rule: the credential follows a `=`/`:` separator.
                _ if !(literal.ends_with('_') || literal.ends_with('-')) => {
                    format!("{literal}={TOKEN}")
                }
                // A token-prefix rule: the credential starts with the prefix.
                _ => format!("echo {literal}{TOKEN}"),
            };
            let redacted = redact_secrets(&text);
            assert!(
                !redacted.contains(TOKEN) && !redacted.contains("MIIEvQIBADANBg"),
                "{name} ({literal}) leaked through the redactor: {redacted}"
            );
        }
    }

    #[test]
    fn argv_secrets_after_a_bearer_header_or_by_their_own_prefix_are_withheld() {
        // Remuda's own agent token and the xAI key shape: lowercase/digit
        // bodies of any length slip past the opaque heuristic, so the prefix
        // must be recognized on its own. The scan matches them in any case.
        for secret in [
            "agk_9f8e7d6c5b4a39281706",
            "AGK_9F8E7D6C5B4A39281706",
            "xai-9f8e7d6c5b4a39281706",
            "XAI-9F8E7D6C5B4A39281706",
        ] {
            let summary = summarize_args(&json!({ "command": format!("echo {secret}") }));
            assert!(
                !summary.contains(&secret[4..]),
                "leaked {secret}: {summary}"
            );
            let summary = summarize_args(&json!({ "command": format!("export K={secret}") }));
            assert!(
                !summary.contains(&secret[4..]),
                "leaked {secret}: {summary}"
            );
        }
        // `--header=Bearer <token>`: the credential is the NEXT word, so the
        // `=` branch must re-arm the redactor instead of returning early.
        for text in [
            "curl --header=Bearer q7Zr2LmW8pXc4VbN1 https://example.test",
            "curl -H 'Authorization: Bearer q7Zr2LmW8pXc4VbN1' https://example.test",
            "curl --header=Authorization: Bearer q7Zr2LmW8pXc4VbN1 https://example.test",
        ] {
            let summary = summarize_args(&json!({ "command": text }));
            assert!(
                !summary.contains("q7Zr2LmW8pXc4VbN1"),
                "leaked the header token: {summary}"
            );
        }
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
        let args = pending.args_summary.expect("args");
        assert!(args.starts_with("command=sleep 300 && echo done"), "{args}");
        assert!(
            args.contains("token=[withheld]") && !args.contains("abc123def456"),
            "{args}"
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
        assert!(pending.tool.is_none() && pending.args_summary.is_none());
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
        assert_eq!(turn.result, "error");
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
        assert_eq!(supply.window_id.as_deref(), Some("primary"));
        assert_eq!(supply.resets_at, Some(reset));
        // A backoff-only park (no reset time known) reports the backoff window.
        let backoff = provider(
            json!([{ "id": "primary", "appliesTo": ["*"], "cooldownUntil": NOW + 60,
                     "source": "observed" }]),
            "available",
        );
        let digest = digest_of(&facts(&worker, None, &[], &[], &[], Some(&backoff)));
        let supply = digest.supply.expect("backoff park");
        assert_eq!(
            (supply.window_id.as_deref(), supply.resets_at),
            (Some("primary"), Some(NOW + 60))
        );
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
        let configure = digest.model.configure.expect("pending configure");
        assert_eq!(configure.state, "queued");
        assert_eq!(configure.request_id.as_deref(), Some("effreq-abc"));
        assert_eq!(
            digest.queued, 2,
            "the send and the configure are still queued"
        );
        assert_eq!(digest.model.effort.requested.as_deref(), Some("medium"));
        assert_eq!(digest.model.effort.effective.as_deref(), Some("low"));
        assert_eq!(digest.model.requested.as_deref(), Some("claude-sonnet-5-5"));
        // Settled configures are not pending.
        let settled = vec![command("instance.configure", "settled", json!({}))];
        assert!(
            digest_of(&facts(&worker, Some(&inst), &[], &[], &settled, None))
                .model
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
        assert_eq!((glance.more.waiting, glance.more.working), (10, 10));
        assert_eq!(glance.waiting.len(), LINEAGE_GLANCE_ROW_CAP);
        assert_eq!(glance.working.len(), LINEAGE_GLANCE_ROW_CAP);
        assert_eq!(glance.done_today, 7);
        assert!(glance.waiting.iter().all(|row| row.kind == "blocked"));
        // A pending approval makes an otherwise-working worker wait.
        let one = worker(json!({ "state": "working" }));
        let mut pending = HashMap::new();
        pending.insert(
            one.instance_id.as_ref().unwrap().as_id().to_string(),
            ("approval".to_string(), Some("Bash".to_string())),
        );
        let glance = assemble_glance("l".into(), "d".into(), &[one], &pending, 0, NOW);
        assert_eq!(glance.waiting[0].kind, "approval");
        assert_eq!(glance.waiting[0].question.as_deref(), Some("Bash"));
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
