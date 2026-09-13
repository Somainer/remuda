//! `remuda journal diff` — the D-028 §12 step 3 / §13 P7 parity gate.
//!
//! Runs the same scenario's two journal dumps (one from `claude-print`, one
//! from `agent-pty`) through the same normalizer and reports whether the
//! logical event streams agree. Volatile envelope fields (`seq`, timestamps,
//! ids, revisions) are stripped, message append chains are collapsed into one
//! canonical node per `messageId`, and whatever still differs must be covered
//! by a rule in the parity whitelist or the command exits non-zero.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Whitelist consulted when `--whitelist` is not given, relative to the cwd.
const DEFAULT_WHITELIST: &str = "docs/design/parity-whitelist.toml";

/// `remuda journal` subcommands.
#[derive(clap::Args)]
#[command(about = "Inspect and compare instance journals.")]
pub(crate) struct Args {
    #[command(subcommand)]
    command: JournalCommand,
}

impl super::registry::Entrypoint for Args {
    fn enter(self, _context: super::registry::Context) -> Result<i32> {
        match self.command {
            JournalCommand::Diff(args) => run(args),
        }
    }

    /// The diff is a gate: stdout stays parseable and logs stay out of it.
    fn tracing(&self) -> bool {
        false
    }
}

/// Journal operations that do not need a Hub connection.
#[derive(Subcommand)]
enum JournalCommand {
    /// Compare two journal dumps of the same scenario (D-028 parity gate).
    Diff(DiffArgs),
}

/// `remuda journal diff <left> <right>`.
#[derive(clap::Args)]
pub(crate) struct DiffArgs {
    /// Baseline dump, conventionally the `claude-print` run.
    left: PathBuf,
    /// Candidate dump, conventionally the `agent-pty` run.
    right: PathBuf,
    /// Parity whitelist TOML; defaults to `docs/design/parity-whitelist.toml`.
    #[arg(long)]
    whitelist: Option<PathBuf>,
    /// Compare with no whitelist at all (every difference fails).
    #[arg(long, conflicts_with = "whitelist")]
    no_whitelist: bool,
    /// Emit `{equal, differences, whitelisted}` instead of a human diff.
    #[arg(long)]
    json: bool,
    /// Colorize the human diff.
    #[arg(long, default_value = "auto", value_parser = ["auto", "always", "never"])]
    color: String,
}

/// Exit 0 when the streams agree modulo the whitelist, 1 otherwise.
fn run(args: DiffArgs) -> Result<i32> {
    let left = load(&args.left)?;
    let right = load(&args.right)?;
    let (whitelist, whitelist_path) = if args.no_whitelist {
        (Whitelist::default(), None)
    } else {
        let path = args
            .whitelist
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_WHITELIST));
        if args.whitelist.is_none() && !path.exists() {
            // An absent default is stricter, never laxer: nothing is excused.
            (Whitelist::default(), None)
        } else {
            (Whitelist::load(&path)?, Some(path))
        }
    };

    let left = canonicalize(&left, &whitelist).context("cannot normalize the left dump")?;
    let right = canonicalize(&right, &whitelist).context("cannot normalize the right dump")?;
    let report = compare(&left, &right, &whitelist);

    if args.json {
        let payload = json!({
            "equal": report.equal,
            "differences": report.differences,
            "whitelisted": report.whitelisted,
            "left": {"path": args.left, "events": left.len()},
            "right": {"path": args.right, "events": right.len()},
            "whitelistPath": whitelist_path,
            "rules": whitelist.rules.len(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print!(
            "{}",
            render(&report, &args, left.len(), right.len(), color(&args.color))
        );
    }
    Ok(i32::from(!report.equal))
}

/// `--color auto` follows stdout and `NO_COLOR`.
fn color(mode: &str) -> bool {
    match mode {
        "always" => true,
        "never" => false,
        _ => std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    }
}

// ---------------------------------------------------------------- loading --

/// Read one dump into a flat list of observations.
///
/// Accepts the Hub's `GET /v1/instances/{id}/journal` body (`{events:[…]}`),
/// the coordinator's jdump / `remuda instance read --source journal` body
/// (`{observations:[…]}`), a bare array, and JSONL of either observations or
/// batch objects.
fn load(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read journal dump {}", path.display()))?;
    let events = if let Ok(value) = serde_json::from_str::<Value>(&text) {
        batch(value).with_context(|| format!("{}: unrecognized journal dump", path.display()))?
    } else {
        let mut events = Vec::new();
        for (number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .with_context(|| format!("{}:{}: invalid JSON", path.display(), number + 1))?;
            events.extend(batch(value).with_context(|| {
                format!("{}:{}: not an observation", path.display(), number + 1)
            })?);
        }
        events
    };
    if events.is_empty() {
        bail!("{}: dump contains no observations", path.display());
    }
    Ok(events)
}

/// Unwrap one dump container into its observations.
fn batch(value: Value) -> Result<Vec<Value>> {
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(mut object) => {
            for key in ["events", "observations"] {
                if let Some(Value::Array(items)) = object.remove(key) {
                    return Ok(items);
                }
            }
            if object.contains_key("kind") {
                return Ok(vec![Value::Object(object)]);
            }
            bail!("expected an `events` or `observations` array, or a bare observation")
        }
        _ => bail!("expected a JSON object or array"),
    }
}

// --------------------------------------------------------------- whitelist --

/// One parity rule; every rule carries the reason it exists.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Rule {
    /// Canonical event kind this rule applies to (`message`, `usage`, …).
    kind: Option<String>,
    /// Canonical topic: the lifecycle topic for `lifecycle`, else the kind.
    topic: Option<String>,
    /// Sugar for `kind = "lifecycle"` plus `topic = <value>`.
    lifecycle: Option<String>,
    /// Canonical field path that may differ; omit to cover the whole event.
    field: Option<String>,
    /// Values treated as interchangeable; omit to permit any value.
    allow: Option<Allow>,
    /// Drop matching events from both sides before aligning.
    #[serde(default)]
    ignore: bool,
    /// Permit a matching event to appear on only one side.
    #[serde(default)]
    allow_unmatched: bool,
    /// Why this difference is acceptable. Required.
    reason: String,
}

/// `allow = "x"` and `allow = ["x", "y"]` are both accepted.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Allow {
    /// Single permitted value.
    One(String),
    /// Any of these permitted values.
    Many(Vec<String>),
}

impl Allow {
    /// Whether `value` rendered as a scalar is listed.
    fn contains(&self, value: &Value) -> bool {
        let text = scalar(value);
        match self {
            Self::One(one) => *one == text,
            Self::Many(many) => many.contains(&text),
        }
    }
}

/// The parsed whitelist file.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Whitelist {
    /// Declared file format version; unknown majors are rejected.
    #[serde(default = "one")]
    version: u32,
    /// Ordered rules; the first match wins.
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
}

/// Default `version` for whitelists that omit it.
fn one() -> u32 {
    1
}

impl Whitelist {
    /// Parse a whitelist, rejecting unknown keys and unknown versions.
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read parity whitelist {}", path.display()))?;
        let whitelist: Self = toml::from_str(&text)
            .with_context(|| format!("cannot parse parity whitelist {}", path.display()))?;
        if whitelist.version != 1 {
            bail!(
                "{}: whitelist version {} is not supported",
                path.display(),
                whitelist.version
            );
        }
        for rule in &whitelist.rules {
            if rule.reason.trim().is_empty() {
                bail!("{}: every rule needs a non-empty reason", path.display());
            }
            if rule.ignore && (rule.field.is_some() || rule.allow.is_some()) {
                bail!(
                    "{}: `ignore` drops the whole event, so it cannot carry `field` or `allow`",
                    path.display()
                );
            }
        }
        Ok(whitelist)
    }

    /// Whether this event is dropped before alignment.
    fn ignores(&self, kind: &str, topic: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.ignore && rule.selects(kind, topic))
    }

    /// Rule excusing a field difference, if any.
    fn excuses_field(
        &self,
        kind: &str,
        topic: &str,
        field: &str,
        values: [&Value; 2],
    ) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            !rule.ignore
                && rule.selects(kind, topic)
                && rule.covers(field)
                && match &rule.allow {
                    // An unconstrained rule accepts whatever this field holds.
                    None => true,
                    // One recognized side is enough: parity differences are
                    // asymmetric by nature (token-level vs record-level).
                    Some(allow) => values.iter().any(|value| allow.contains(value)),
                }
        })
    }

    /// Rule excusing an event that exists on only one side.
    fn excuses_unmatched(&self, kind: &str, topic: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.allow_unmatched && !rule.ignore && rule.selects(kind, topic))
    }
}

impl Rule {
    /// Whether the selectors match this canonical event.
    fn selects(&self, kind: &str, topic: &str) -> bool {
        if let Some(want) = &self.lifecycle
            && (kind != "lifecycle" || want != topic)
        {
            return false;
        }
        if let Some(want) = &self.kind
            && want != kind
        {
            return false;
        }
        if let Some(want) = &self.topic
            && want != topic
        {
            return false;
        }
        self.lifecycle.is_some() || self.kind.is_some() || self.topic.is_some()
    }

    /// Whether this rule's `field` covers a canonical leaf path. A rule on
    /// `cost` also covers `cost.amount`, so whitelisting a composite value
    /// does not require enumerating its leaves.
    fn covers(&self, path: &str) -> bool {
        let Some(field) = &self.field else {
            return false;
        };
        path == field
            || path
                .strip_prefix(field.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    }
}

// ---------------------------------------------------------- canonical form --

/// One logical event after normalization.
#[derive(Debug, Clone)]
struct Canon {
    /// Observation kind (`message`, `tool_call`, `lifecycle`, …).
    kind: String,
    /// Lifecycle topic for `lifecycle`, otherwise the kind.
    topic: String,
    /// Stable alias used to align the two sides (`msg#1`, `tool#2`).
    key: String,
    /// Comparable body with volatile fields removed.
    body: Value,
}

impl Canon {
    /// Alignment signature: identity only, never content.
    fn signature(&self) -> String {
        format!("{}\u{1}{}\u{1}{}", self.kind, self.topic, self.key)
    }

    /// Human label for the diff header of this event.
    fn label(&self) -> String {
        if self.key.is_empty() {
            format!("{} {}", self.kind, self.topic)
        } else {
            format!("{} {}", self.kind, self.key)
        }
    }
}

/// Per-side alias table: native ids are volatile, ordinals are not.
#[derive(Default)]
struct Aliases(BTreeMap<String, String>);

impl Aliases {
    /// Stable `prefix#n` alias for a native id, assigned on first sight.
    fn alias(&mut self, prefix: &str, id: &Value) -> String {
        let Some(id) = id.as_str().filter(|id| !id.is_empty()) else {
            return format!("{prefix}#?");
        };
        let slot = format!("{prefix}\u{1}{id}");
        if let Some(existing) = self.0.get(&slot) {
            return existing.clone();
        }
        let ordinal = self
            .0
            .keys()
            .filter(|key| key.starts_with(&format!("{prefix}\u{1}")))
            .count()
            + 1;
        let alias = format!("{prefix}#{ordinal}");
        self.0.insert(slot, alias.clone());
        alias
    }
}

/// Mutation replay state for one message / thought / tool node.
#[derive(Default)]
struct Node {
    /// Position of this node's `open` in the canonical stream.
    slot: usize,
    /// Current content blocks.
    blocks: Vec<Value>,
    /// Text added by each `append`, for the granularity verdict.
    appends: Vec<String>,
    /// Latest scalar fields carried by the chain.
    fields: Map<String, Value>,
    /// Whether a `replace` or `close` supplied a full snapshot.
    snapshotted: bool,
}

/// Kinds whose `open`/`append`/`replace`/`close` chain folds into one event,
/// with the payload field holding the node id and the alias prefix to use.
fn chained(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "message" => Some(("messageId", "msg")),
        "thought" => Some(("thoughtId", "tht")),
        "tool_call" => Some(("toolCallId", "tool")),
        "tool_result" => Some(("toolCallId", "tool")),
        _ => None,
    }
}

/// Normalize one dump: strip volatile fields, collapse chains, order by turn.
fn canonicalize(events: &[Value], whitelist: &Whitelist) -> Result<Vec<Canon>> {
    let mut aliases = Aliases::default();
    let mut out: Vec<Option<Canon>> = Vec::new();
    // `kind`+alias -> replay state, so every append chain folds into its slot.
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();

    for event in events {
        let kind = event
            .get("kind")
            .and_then(Value::as_str)
            .context("observation has no `kind`")?
            .to_owned();
        let payload = event.get("payload").cloned().unwrap_or(Value::Null);
        let topic = topic_of(&kind, &payload);
        if whitelist.ignores(&kind, &topic).is_some() {
            continue;
        }

        if let Some((id_field, prefix)) = chained(&kind) {
            let alias = aliases.alias(prefix, payload.get(id_field).unwrap_or(&Value::Null));
            // tool_call and tool_result share an id; the kind keeps them apart.
            let node = nodes
                .entry(format!("{kind}\u{1}{alias}"))
                .or_insert_with(|| {
                    out.push(None);
                    Node {
                        slot: out.len() - 1,
                        ..Node::default()
                    }
                });
            apply(node, &kind, &payload);
            out[node.slot] = Some(finish(&kind, &topic, &alias, node));
        } else {
            out.push(Some(Canon {
                key: key_of(&kind, &payload, &mut aliases),
                body: body_of(&kind, &payload),
                kind,
                topic,
            }));
        }
    }
    Ok(out.into_iter().flatten().collect())
}

/// Fold one mutation into a node's replay state.
fn apply(node: &mut Node, kind: &str, payload: &Value) {
    let operation = payload
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("open");
    let incoming = match kind {
        "message" | "tool_result" => payload
            .get("blocks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        "tool_call" => {
            // Print streams the argument JSON as `inputTextDelta`; the PTY path
            // gets one completed item. Replay both as text so they converge.
            match payload.get("inputTextDelta") {
                Some(Value::String(delta)) => vec![json!({"type": "text", "text": delta})],
                _ => Vec::new(),
            }
        }
        // Thoughts carry plain text; wrap it so every kind replays identically.
        _ => match payload.get("text") {
            Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
            _ => Vec::new(),
        },
    };

    match operation {
        "append" => {
            node.appends.push(text_of(&incoming));
            let target = payload
                .get("targetBlock")
                .and_then(Value::as_u64)
                .map(|index| index as usize);
            for block in incoming {
                let slot = target.filter(|index| *index < node.blocks.len());
                match slot.and_then(|index| node.blocks.get_mut(index)) {
                    Some(existing) if is_text(existing) && is_text(&block) => {
                        let added = block.get("text").and_then(Value::as_str).unwrap_or("");
                        let current = existing.get("text").and_then(Value::as_str).unwrap_or("");
                        *existing = json!({"type": "text", "text": format!("{current}{added}")});
                    }
                    _ => node.blocks.push(block),
                }
            }
        }
        "replace" | "close" => {
            // `close` may restate the node or only settle its status.
            if operation == "replace" || !incoming.is_empty() {
                node.blocks = incoming;
                node.snapshotted = true;
            }
        }
        _ => node.blocks = incoming,
    }

    for field in [
        "role",
        "phase",
        "status",
        "representation",
        "parentToolCallId",
        "nativeOrigin",
        "toolName",
        "displayTitle",
        "category",
        "input",
        "state",
        "executor",
        "stage",
        "outcome",
        "exitCode",
        "structuredResult",
        "changes",
    ] {
        if let Some(value) = payload.get(field)
            && !value.is_null()
        {
            node.fields.insert(field.to_owned(), value.clone());
        }
    }
}

/// Read one latest field off a replayed node.
fn field(node: &Node, name: &str) -> Value {
    node.fields.get(name).cloned().unwrap_or(Value::Null)
}

/// Project a replayed node into its canonical event.
fn finish(kind: &str, topic: &str, key: &str, node: &Node) -> Canon {
    let mut body = Map::new();
    match kind {
        "message" => {
            body.insert("role".into(), field(node, "role"));
            body.insert("phase".into(), field(node, "phase"));
        }
        "thought" => {
            body.insert("representation".into(), field(node, "representation"));
        }
        "tool_call" => {
            body.insert("toolName".into(), knowledge(node.fields.get("toolName")));
            body.insert(
                "displayTitle".into(),
                knowledge(node.fields.get("displayTitle")),
            );
            body.insert("category".into(), field(node, "category"));
            // A completed `input` outranks the reassembled delta text; the
            // delta only stands in while the arguments are still arriving.
            body.insert(
                "input".into(),
                match node.fields.get("input") {
                    Some(input) => knowledge(Some(input)),
                    None => Value::String(text_of(&node.blocks)),
                },
            );
            body.insert("state".into(), field(node, "state"));
        }
        "tool_result" => {
            body.insert("stage".into(), field(node, "stage"));
            body.insert("outcome".into(), field(node, "outcome"));
            body.insert("exitCode".into(), knowledge(node.fields.get("exitCode")));
            body.insert(
                "structuredResult".into(),
                knowledge(node.fields.get("structuredResult")),
            );
            body.insert(
                "changes".into(),
                match node.fields.get("changes").and_then(Value::as_array) {
                    Some(changes) => Value::Array(
                        changes
                            .iter()
                            .map(|change| {
                                json!({
                                    "path": change.get("path").cloned().unwrap_or(Value::Null),
                                    "application": change.get("application").cloned()
                                        .unwrap_or(Value::Null),
                                })
                            })
                            .collect(),
                    ),
                    None => Value::Array(Vec::new()),
                },
            );
        }
        _ => {}
    }
    if kind != "tool_call" {
        body.insert("text".into(), Value::String(text_of(&node.blocks)));
    }
    let attachments: Vec<Value> = node
        .blocks
        .iter()
        .filter(|block| !is_text(block))
        .map(|block| {
            json!({
                "type": block.get("type").cloned().unwrap_or(Value::Null),
                "mediaType": block.get("mediaType").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    if !attachments.is_empty() {
        body.insert("attachments".into(), Value::Array(attachments));
    }
    if matches!(kind, "message" | "thought") {
        body.insert("status".into(), field(node, "status"));
    }
    body.insert("granularity".into(), Value::String(granularity(node)));

    Canon {
        kind: kind.to_owned(),
        topic: topic.to_owned(),
        key: key.to_owned(),
        body: Value::Object(body),
    }
}

/// How the text arrived: the whitelistable print-vs-pty difference (§12.3).
fn granularity(node: &Node) -> String {
    if node.appends.is_empty() {
        return if node.snapshotted {
            "snapshot".into()
        } else {
            "single".into()
        };
    }
    let line_shaped = node
        .appends
        .iter()
        .enumerate()
        .all(|(index, delta)| delta.ends_with('\n') || index + 1 == node.appends.len());
    if line_shaped {
        "record-level".into()
    } else {
        "token-level".into()
    }
}

/// Concatenate the text blocks of a content list.
fn text_of(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect()
}

/// Whether a content block is a text block.
fn is_text(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
}

/// Lifecycle events discriminate on their topic; everything else on its kind.
fn topic_of(kind: &str, payload: &Value) -> String {
    if kind != "lifecycle" {
        return kind.to_owned();
    }
    match payload.get("type").and_then(Value::as_str) {
        Some("entity") => payload
            .get("entityType")
            .and_then(Value::as_str)
            .unwrap_or("entity")
            .to_owned(),
        _ => payload
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or("native")
            .to_owned(),
    }
}

/// Alignment key for the kinds that arrive as one self-contained event.
fn key_of(kind: &str, payload: &Value, aliases: &mut Aliases) -> String {
    let null = Value::Null;
    match kind {
        "interaction.requested" => aliases.alias(
            "int",
            payload
                .get("interaction")
                .and_then(|interaction| interaction.get("id"))
                .unwrap_or(&null),
        ),
        "interaction.answered" | "interaction.expired" => {
            let alias = aliases.alias("int", payload.get("interactionId").unwrap_or(&null));
            let suffix = kind.rsplit('.').next().unwrap_or(kind);
            format!("{alias}/{suffix}")
        }
        "usage" => {
            let alias = aliases.alias("usg", payload.get("usageId").unwrap_or(&null));
            let scope = payload.get("scope").and_then(Value::as_str).unwrap_or("");
            format!("{alias}/{scope}")
        }
        "artifact" => aliases.alias("art", payload.get("artifactId").unwrap_or(&null)),
        "workflow.run" | "workflow.phase" | "workflow.member" => {
            aliases.alias("wf", payload.get("workflowId").unwrap_or(&null))
        }
        "lifecycle" => match payload.get("type").and_then(Value::as_str) {
            Some("entity") => {
                let alias = aliases.alias("ent", payload.get("entityId").unwrap_or(&null));
                let state = payload.get("state").and_then(Value::as_str).unwrap_or("");
                format!("{alias}/{state}")
            }
            _ => payload
                .get("nativeName")
                .and_then(Value::as_str)
                .unwrap_or("native")
                .to_owned(),
        },
        "raw_tty" => payload
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("output")
            .to_owned(),
        "opaque" => payload
            .get("nativeType")
            .and_then(Value::as_str)
            .unwrap_or("opaque")
            .to_owned(),
        _ => String::new(),
    }
}

/// Canonical body for the kinds that arrive as one self-contained event. Ids
/// become aliases, `Knowledge` collapses to its value, and revisions and
/// cursors drop out entirely.
fn body_of(kind: &str, payload: &Value) -> Value {
    let get = |field: &str| payload.get(field).cloned().unwrap_or(Value::Null);
    let known = |field: &str| knowledge(payload.get(field));
    let null = Value::Null;
    match kind {
        "interaction.requested" => {
            let interaction = payload.get("interaction").unwrap_or(&null);
            json!({
                "kind": interaction.get("kind").cloned().unwrap_or(Value::Null),
                "blocking": interaction.get("blocking").cloned().unwrap_or(Value::Null),
                "answerable": interaction.get("answerable").cloned().unwrap_or(Value::Null),
                "carrier": interaction.get("carrier").cloned().unwrap_or(Value::Null),
                "request": request(interaction.get("request")),
            })
        }
        "interaction.answered" => json!({
            "actor": payload.get("actor").and_then(|actor| actor.get("type")).cloned().unwrap_or(Value::Null),
            "delivery": get("delivery"),
        }),
        "interaction.expired" => json!({"reason": get("reason")}),
        "usage" => json!({
            "scope": get("scope"),
            "mode": get("mode"),
            "inputTokens": known("inputTokens"),
            "outputTokens": known("outputTokens"),
            "reasoningTokens": known("reasoningTokens"),
            "totalTokens": known("totalTokens"),
            "inputAccounting": get("inputAccounting"),
            "accounting": get("accounting"),
            "cost": knowledge(payload.get("cost")),
        }),
        "artifact" => json!({
            "action": get("action"),
            "type": get("type"),
            "title": known("title"),
            "mediaType": known("mediaType"),
            "verification": get("verification"),
        }),
        "lifecycle" => match payload.get("type").and_then(Value::as_str) {
            Some("entity") => json!({
                "entityType": get("entityType"),
                "previousState": get("previousState"),
                "state": get("state"),
                "reasonCode": get("reasonCode"),
            }),
            _ => json!({
                "nativeName": get("nativeName"),
                "status": known("status"),
                "severity": get("severity"),
                "affectsCompletion": get("affectsCompletion"),
            }),
        },
        "workflow.run" => json!({
            "engine": get("engine"), "state": get("state"), "title": known("title"),
        }),
        "workflow.phase" => json!({
            "label": known("label"), "state": get("state"),
        }),
        "workflow.member" => json!({
            "label": known("label"), "state": get("state"),
            "modelResolved": known("modelResolved"),
        }),
        "raw_tty" => json!({
            "direction": get("direction"),
            "representation": get("representation"),
        }),
        "opaque" => json!({"nativeType": get("nativeType"), "reason": get("reason")}),
        _ => Value::Null,
    }
}

/// Canonical form of an interaction request: prompt shape, not prompt ids.
fn request(request: Option<&Value>) -> Value {
    let Some(request) = request else {
        return Value::Null;
    };
    let fields = request
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .map(|field| {
                    json!({
                        "title": field.get("title").cloned().unwrap_or(Value::Null),
                        "input": field.get("input").cloned().unwrap_or(Value::Null),
                        "required": field.get("required").cloned().unwrap_or(Value::Null),
                        "options": field.get("options").and_then(Value::as_array).map(|options| {
                            options.iter()
                                .filter_map(|option| option.get("label").cloned())
                                .collect::<Vec<_>>()
                        }).unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "kind": request.get("kind").cloned().unwrap_or(Value::Null),
        "title": request.get("title").cloned().unwrap_or(Value::Null),
        "fields": fields,
        "options": request.get("options").and_then(Value::as_array).map(|options| {
            options.iter().filter_map(|option| option.get("label").cloned()).collect::<Vec<_>>()
        }).unwrap_or_default(),
    })
}

/// Collapse a `Knowledge<T>` to its value; unknown and n/a become markers.
fn knowledge(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    match value.get("state").and_then(Value::as_str) {
        // The `reason` and `evidenceEventIds` of an unknown are volatile.
        Some("known") => value.get("value").cloned().unwrap_or(Value::Null),
        Some("unknown") => Value::String("<unknown>".into()),
        Some("not-applicable") => Value::String("<n/a>".into()),
        _ => value.clone(),
    }
}

// -------------------------------------------------------------------- diff --

/// Where a difference sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum Side {
    /// Only the left dump has this event.
    Left,
    /// Only the right dump has this event.
    Right,
    /// Both dumps have the event; a field disagrees.
    Both,
}

/// One reported difference, whitelisted or not.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Difference {
    /// Canonical event kind.
    kind: String,
    /// Canonical topic.
    topic: String,
    /// Alignment key.
    key: String,
    /// Which side(s) the difference concerns.
    side: Side,
    /// Canonical field path, when a field disagrees.
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    /// Left value, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    left: Option<Value>,
    /// Right value, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    right: Option<Value>,
    /// Whitelist reason, when this difference was excused.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Outcome of one comparison.
struct Report {
    /// Whether every difference was whitelisted.
    equal: bool,
    /// Differences that fail the gate.
    differences: Vec<Difference>,
    /// Differences a whitelist rule excused.
    whitelisted: Vec<Difference>,
}

/// Align both streams and classify every difference.
fn compare(left: &[Canon], right: &[Canon], whitelist: &Whitelist) -> Report {
    let mut report = Report {
        equal: true,
        differences: Vec::new(),
        whitelisted: Vec::new(),
    };
    let mut push = |difference: Difference| {
        if difference.reason.is_some() {
            report.whitelisted.push(difference);
        } else {
            report.equal = false;
            report.differences.push(difference);
        }
    };

    for step in align(left, right) {
        match step {
            (Some(l), Some(r)) => {
                for (field, lv, rv) in fields(&l.body, &r.body) {
                    let excuse = whitelist.excuses_field(&l.kind, &l.topic, &field, [&lv, &rv]);
                    push(Difference {
                        kind: l.kind.clone(),
                        topic: l.topic.clone(),
                        key: l.key.clone(),
                        side: Side::Both,
                        field: Some(field),
                        left: Some(lv),
                        right: Some(rv),
                        reason: excuse.map(|rule| rule.reason.clone()),
                    });
                }
            }
            (Some(l), None) => {
                let excuse = whitelist.excuses_unmatched(&l.kind, &l.topic);
                push(Difference {
                    kind: l.kind.clone(),
                    topic: l.topic.clone(),
                    key: l.key.clone(),
                    side: Side::Left,
                    field: None,
                    left: Some(l.body.clone()),
                    right: None,
                    reason: excuse.map(|rule| rule.reason.clone()),
                });
            }
            (None, Some(r)) => {
                let excuse = whitelist.excuses_unmatched(&r.kind, &r.topic);
                push(Difference {
                    kind: r.kind.clone(),
                    topic: r.topic.clone(),
                    key: r.key.clone(),
                    side: Side::Right,
                    field: None,
                    left: None,
                    right: Some(r.body.clone()),
                    reason: excuse.map(|rule| rule.reason.clone()),
                });
            }
            (None, None) => {}
        }
    }
    report
}

/// Longest-common-subsequence alignment over event signatures.
fn align<'a>(left: &'a [Canon], right: &'a [Canon]) -> Vec<(Option<&'a Canon>, Option<&'a Canon>)> {
    let l: Vec<String> = left.iter().map(Canon::signature).collect();
    let r: Vec<String> = right.iter().map(Canon::signature).collect();
    let mut table = vec![vec![0usize; r.len() + 1]; l.len() + 1];
    for i in (0..l.len()).rev() {
        for j in (0..r.len()).rev() {
            table[i][j] = if l[i] == r[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut steps) = (0, 0, Vec::new());
    while i < l.len() && j < r.len() {
        if l[i] == r[j] {
            steps.push((Some(&left[i]), Some(&right[j])));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            steps.push((Some(&left[i]), None));
            i += 1;
        } else {
            steps.push((None, Some(&right[j])));
            j += 1;
        }
    }
    steps.extend(left[i..].iter().map(|event| (Some(event), None)));
    steps.extend(right[j..].iter().map(|event| (None, Some(event))));
    steps
}

/// Leaf-path differences between two canonical bodies.
fn fields(left: &Value, right: &Value) -> Vec<(String, Value, Value)> {
    let mut out = Vec::new();
    walk("", left, right, &mut out);
    out
}

/// Recursive leaf walk; objects merge key sets, everything else compares whole.
fn walk(path: &str, left: &Value, right: &Value, out: &mut Vec<(String, Value, Value)>) {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            let mut keys: Vec<&String> = l.keys().chain(r.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(
                    &child,
                    l.get(key).unwrap_or(&Value::Null),
                    r.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ if left == right => {}
        _ => out.push((path.to_owned(), left.clone(), right.clone())),
    }
}

/// Render a scalar for whitelist matching and human output.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

// ------------------------------------------------------------------ render --

/// Colored unified-style report.
fn render(report: &Report, args: &DiffArgs, left: usize, right: usize, color: bool) -> String {
    let paint = |code: &str, text: &str| {
        if color {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_owned()
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        paint("1", &format!("--- {} ({left} events)", args.left.display()))
    );
    let _ = writeln!(
        out,
        "{}",
        paint(
            "1",
            &format!("+++ {} ({right} events)", args.right.display())
        )
    );

    for difference in &report.differences {
        let _ = writeln!(out, "{}", paint("31", &line(difference)));
    }
    for difference in &report.whitelisted {
        let _ = writeln!(out, "{}", paint("33", &line(difference)));
    }
    let _ = writeln!(out);
    let verdict = if report.equal {
        paint(
            "32",
            &format!(
                "parity ok: {} whitelisted difference(s), 0 blocking",
                report.whitelisted.len()
            ),
        )
    } else {
        paint(
            "31",
            &format!(
                "parity FAILED: {} blocking difference(s), {} whitelisted",
                report.differences.len(),
                report.whitelisted.len()
            ),
        )
    };
    let _ = writeln!(out, "{verdict}");
    out
}

/// One diff line.
fn line(difference: &Difference) -> String {
    let label = Canon {
        kind: difference.kind.clone(),
        topic: difference.topic.clone(),
        key: difference.key.clone(),
        body: Value::Null,
    }
    .label();
    let mut text = match difference.side {
        Side::Left => format!("- {label}  (only in left)"),
        Side::Right => format!("+ {label}  (only in right)"),
        Side::Both => format!(
            "~ {label}  {}: {} -> {}",
            difference.field.as_deref().unwrap_or(""),
            brief(difference.left.as_ref()),
            brief(difference.right.as_ref())
        ),
    };
    if let Some(reason) = &difference.reason {
        let _ = write!(text, "  [whitelisted: {reason}]");
    }
    text
}

/// Bounded one-line value for the human diff.
fn brief(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "-".into();
    };
    let text = scalar(value).replace('\n', "\\n");
    super::table::cell(&text, 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(kind: &str, payload: Value) -> Value {
        json!({"seq": "1", "kind": kind, "payload": payload})
    }

    #[test]
    fn append_chain_collapses_to_one_event_with_final_text() {
        let events = vec![
            observation(
                "message",
                json!({"messageId": "a", "operation": "open", "role": "assistant",
                       "phase": "final", "status": "streaming", "blocks": []}),
            ),
            observation(
                "message",
                json!({"messageId": "a", "operation": "append", "targetBlock": 0,
                       "blocks": [{"type": "text", "text": "He"}]}),
            ),
            observation(
                "message",
                json!({"messageId": "a", "operation": "append", "targetBlock": 0,
                       "blocks": [{"type": "text", "text": "llo"}]}),
            ),
            observation(
                "message",
                json!({"messageId": "a", "operation": "close", "status": "complete"}),
            ),
        ];
        let canon = canonicalize(&events, &Whitelist::default()).unwrap();
        assert_eq!(canon.len(), 1);
        assert_eq!(canon[0].key, "msg#1");
        assert_eq!(canon[0].body["text"], json!("Hello"));
        assert_eq!(canon[0].body["status"], json!("complete"));
        assert_eq!(canon[0].body["granularity"], json!("token-level"));
    }

    #[test]
    fn line_shaped_appends_report_record_level_granularity() {
        let events = vec![
            observation(
                "message",
                json!({"messageId": "a", "operation": "open", "role": "assistant",
                       "phase": "final", "status": "streaming", "blocks": []}),
            ),
            observation(
                "message",
                json!({"messageId": "a", "operation": "append", "targetBlock": 0,
                       "blocks": [{"type": "text", "text": "one\n"}]}),
            ),
            observation(
                "message",
                json!({"messageId": "a", "operation": "append", "targetBlock": 0,
                       "blocks": [{"type": "text", "text": "two"}]}),
            ),
        ];
        let canon = canonicalize(&events, &Whitelist::default()).unwrap();
        assert_eq!(canon[0].body["granularity"], json!("record-level"));
        assert_eq!(canon[0].body["text"], json!("one\ntwo"));
    }

    #[test]
    fn volatile_ids_become_stable_aliases_per_side() {
        let left = canonicalize(
            &[observation(
                "tool_call",
                json!({"toolCallId": "obj_aaa", "toolName": {"state": "known", "value": "Read"},
                       "category": "file-read", "state": "running"}),
            )],
            &Whitelist::default(),
        )
        .unwrap();
        let right = canonicalize(
            &[observation(
                "tool_call",
                json!({"toolCallId": "obj_zzz", "toolName": {"state": "known", "value": "Read"},
                       "category": "file-read", "state": "running"}),
            )],
            &Whitelist::default(),
        )
        .unwrap();
        assert_eq!(left[0].key, right[0].key);
        let report = compare(&left, &right, &Whitelist::default());
        assert!(report.equal, "{:?}", report.differences);
    }

    #[test]
    fn unmatched_event_fails_without_a_rule() {
        let left = canonicalize(
            &[observation(
                "tool_result",
                json!({"toolCallId": "t", "stage": "final", "outcome": "succeeded", "blocks": []}),
            )],
            &Whitelist::default(),
        )
        .unwrap();
        let report = compare(&left, &[], &Whitelist::default());
        assert!(!report.equal);
        assert_eq!(report.differences[0].side, Side::Left);
    }

    #[test]
    fn ignore_rule_drops_the_event_before_alignment() {
        let whitelist: Whitelist = toml::from_str(
            "version = 1\n[[rule]]\nlifecycle = 'hook'\nignore = true\nreason = 'pty only'\n",
        )
        .unwrap();
        let events = vec![observation(
            "lifecycle",
            json!({"type": "native", "topic": "hook", "nativeName": "PreToolUse"}),
        )];
        assert!(canonicalize(&events, &whitelist).unwrap().is_empty());
    }

    #[test]
    fn allow_excuses_a_field_when_one_side_matches() {
        let whitelist: Whitelist = toml::from_str(
            "version = 1\n[[rule]]\nkind = 'message'\nfield = 'granularity'\n\
             allow = 'record-level'\nreason = 'pty tails lines'\n",
        )
        .unwrap();
        let rule = whitelist.excuses_field(
            "message",
            "message",
            "granularity",
            [&json!("token-level"), &json!("record-level")],
        );
        assert!(rule.is_some());
        assert!(
            whitelist
                .excuses_field(
                    "message",
                    "message",
                    "granularity",
                    [&json!("token-level"), &json!("snapshot")]
                )
                .is_none()
        );
    }

    #[test]
    fn rules_need_a_selector_and_a_reason() {
        let bare: Whitelist =
            toml::from_str("version = 1\n[[rule]]\nfield = 'text'\nreason = 'x'\n").unwrap();
        assert!(!bare.rules[0].selects("message", "message"));
        let path = std::env::temp_dir().join("remuda-parity-empty-reason.toml");
        std::fs::write(
            &path,
            "version = 1\n[[rule]]\nkind = 'usage'\nreason = '  '\n",
        )
        .unwrap();
        assert!(Whitelist::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hub_jdump_and_jsonl_containers_all_load() {
        let dir = std::env::temp_dir().join("remuda-parity-load");
        std::fs::create_dir_all(&dir).unwrap();
        let event = observation("lifecycle", json!({"type": "native", "topic": "session"}));
        for (name, body) in [
            (
                "hub.json",
                json!({"durableSeq": "1", "events": [event]}).to_string(),
            ),
            ("jdump.json", json!({"observations": [event]}).to_string()),
            ("bare.json", json!([event]).to_string()),
            ("lines.jsonl", format!("{}\n\n{}\n", event, event)),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            assert!(!load(&path).unwrap().is_empty(), "{name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
