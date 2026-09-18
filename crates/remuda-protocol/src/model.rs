//! §9.1 effective-model tracking shared by every Claude transcript mapper.
//!
//! Sibling of [`crate::effort`]: the driver's live mapper and the journal
//! tailer must agree on what an assistant record says about the model and how
//! a `/model` slash record attributes the change.
//!
//! Claude transcript shape measured on 2.1.221 and re-measured on 2.1.272
//! (`docs/design/evidence/model-sync-1.md`):
//!
//! - assistant records carry the resolved model id at `message.model`
//!   (gateway ids included, e.g. `ark/seed-evolving`);
//! - `/model <id>` is journaled as a `user` record whose message content is
//!   `<command-name>/model</command-name> … <command-args>id</command-args>`
//!   followed immediately by a SECOND `user` record carrying the verdict in
//!   `<local-command-stdout>…</local-command-stdout>` — on 2.1.272 a valid id
//!   applies with NO confirmation dialog and the two records share one
//!   timestamp;
//! - an unknown id and a dismissed picker are journaled as `system` records
//!   (`subtype: "local_command"`, top-level `content`), NOT `user` records:
//!   * `Model '<id>' not found`
//!   * `Kept model as \`<id>\`` (picker cancelled)
//! - accept verdicts:
//!   * 2.1.272: `` Set model to `<resolved>` and saved as your default for
//!     new sessions `` optionally followed by a dim `ANTHROPIC_MODEL is set
//!     to …` note on the next line;
//!   * 2.1.221: `Set model to <bold>name</bold> and saved as your default …`.

use crate::{EffortSource, EventId, Id, Timestamp};

/// Strip SGR escapes (`\x1b[…m`) only. The verdict carries bold/dim styling;
/// other escape families never appear inside `<local-command-stdout>`.
fn strip_sgr(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let c = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Deterministic observation id for a model edge read from one native
/// transcript record. Both observing channels process the same native record,
/// so deriving the id from `(scope, native id, model)` via [`Id::derive`]
/// makes the edge stable across channels.
pub fn model_event_id(scope: &str, assistant_native_id: &str, model: &str) -> EventId {
    let native = format!("model:{assistant_native_id}:{model}");
    let id: Id = Id::derive("evt", scope, &native).expect("evt prefix registered");
    EventId::try_from(String::from(id)).expect("derive with the evt prefix yields an EventId")
}

/// A model id as read off a transcript (verdict or assistant record).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ObservedModel {
    /// Resolved model id; the verdict spells it in backticks (2.1.272) or bold
    /// (2.1.221), and `message.model` carries it plainly.
    pub id: String,
}

/// What the `<local-command-stdout>` sibling of a `/model` slash record says.
///
/// The slash record only proves the bytes were submitted; the stdout line is
/// the verdict — same split as `/effort` on 2.1.272.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStdout {
    /// `Set model to <id>` — the switch is in effect now. Carries the resolved
    /// id the TUI printed (an alias like `sonnet` resolves to a concrete id).
    Accepted(ObservedModel),
    /// `Kept model as <id>` — the picker/dialog was dismissed.
    Kept,
    /// `Model '<id>' not found`.
    NotFound,
    /// Unrelated stdout.
    Other,
}

/// Extract the first backticked token, else the run up to the next boundary.
fn resolved_id_after_prefix(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix('`')
        && let Some(end) = after.find('`')
    {
        let id = after[..end].trim();
        return (!id.is_empty()).then(|| id.to_owned());
    }
    // 2.1.221 spells the name in bold SGR; strip_sgr already ran, so take the
    // token run up to " and " / newline.
    let cut = rest
        .find(" and ")
        .or_else(|| rest.find('\n'))
        .unwrap_or(rest.len());
    let id = rest[..cut].trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// Parse the verdict out of a `/model` `<local-command-stdout>` line.
///
/// Measured strings:
/// - `` Set model to `model_hub/es1_orange_o48[1m]` and saved as your default
///   for new sessions `` (plus an optional dim second line about
///   `ANTHROPIC_MODEL` being set)
/// - `Set model to \x1b[1mseed-evolving\x1b[22m and saved as your default …`
/// - `` Kept model as `model_hub/es1_orange_o48[1m]` ``
/// - `Model 'bogus-xyz-123' not found`
pub fn parse_model_stdout(text: &str) -> ModelStdout {
    // The env-override hint rides a second dim line and itself contains
    // backticked ids — never parse past the first newline.
    let first_line = text.split('\n').next().unwrap_or(text);
    let t = strip_sgr(first_line);
    let t = t.trim();
    if let Some(rest) = t.strip_prefix("Set model to")
        && let Some(id) = resolved_id_after_prefix(rest)
    {
        return ModelStdout::Accepted(ObservedModel { id });
    }
    if t.starts_with("Kept model as") {
        return ModelStdout::Kept;
    }
    if t.starts_with("Model ") && t.ends_with("not found") {
        return ModelStdout::NotFound;
    }
    ModelStdout::Other
}

/// Transcript-side effective-model state: dedup and source attribution.
///
/// Edges only: [`ModelTracker::observe`] returns `None` for an unchanged id.
/// Pure state shared by both transcript mappers; not a wire type.
#[derive(Debug, Clone)]
pub struct ModelTracker {
    last: Option<String>,
    pending_source: EffortSource,
    /// A Remuda switch awaiting read-back at this raw request id.
    awaiting: Option<String>,
}

impl Default for ModelTracker {
    fn default() -> Self {
        Self {
            last: None,
            pending_source: EffortSource::Unknown,
            awaiting: None,
        }
    }
}

impl ModelTracker {
    /// Construct an empty tracker (no model observed yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a `/model <args>` slash record. `from_remuda` says whether
    /// Remuda typed the bytes (otherwise the human gets the credit). A bare
    /// `/model` (picker, empty args) neither arms attribution nor switches.
    pub fn note_slash(&mut self, args: &str, from_remuda: bool) -> bool {
        let id = args.trim();
        if id.is_empty() {
            return false;
        }
        self.pending_source = if from_remuda {
            EffortSource::Remuda
        } else {
            EffortSource::Slash
        };
        if from_remuda {
            self.awaiting = Some(id.to_owned());
        }
        true
    }

    /// Settle a switch from its `<local-command-stdout>` verdict. Returns an
    /// edge observation for an accept that changes the effective id; a
    /// dismiss/not-found clears the awaiting attribution and returns `None`.
    ///
    /// The caller resolves its switch bridge independently of the edge: a
    /// switch to the id already in effect is still an accepted switch.
    pub fn note_stdout(
        &mut self,
        stdout: &str,
        from_remuda: bool,
    ) -> Option<(ObservedModel, EffortSource)> {
        match parse_model_stdout(stdout) {
            ModelStdout::Accepted(observed) => {
                let mut source = if from_remuda {
                    EffortSource::Remuda
                } else {
                    EffortSource::Slash
                };
                if self.awaiting.as_deref() == Some(observed.id.as_str()) {
                    source = EffortSource::Remuda;
                }
                // A verdict positively settles every awaiting attribution:
                // the requested alias resolved to this concrete id.
                self.awaiting = None;
                let edge = self.last.as_deref() != Some(observed.id.as_str());
                self.last = Some(observed.id.clone());
                if edge {
                    self.pending_source = EffortSource::Unknown;
                    Some((observed, source))
                } else {
                    None
                }
            }
            // No model change; a later natural edge must not be credited to a
            // Remuda switch that Claude actually refused.
            ModelStdout::Kept | ModelStdout::NotFound => {
                self.awaiting = None;
                None
            }
            ModelStdout::Other => None,
        }
    }

    /// Declare the launch-time source before the first assistant record.
    pub fn mark_launch(&mut self) {
        self.pending_source = EffortSource::Launch;
    }

    /// Arm a Remuda switch awaiting read-back for `id`.
    pub fn arm_awaiting(&mut self, id: &str) {
        self.awaiting = Some(id.to_owned());
    }

    /// Feed one assistant record's resolved `message.model`; returns an edge
    /// observation only. Assistant records corroborate the verdict but never
    /// resolve a live switch — the command verdict does.
    pub fn observe(&mut self, model: Option<&str>) -> Option<(ObservedModel, EffortSource)> {
        let raw = model?.trim();
        if raw.is_empty() {
            return None;
        }
        let mut source = self.pending_source;
        // The resolved id may differ from the requested alias; treat any
        // outstanding await as settled by the first post-switch record.
        if self.awaiting.is_some() {
            source = EffortSource::Remuda;
            self.awaiting = None;
        }
        let edge = self.last.as_deref() != Some(raw);
        self.last = Some(raw.to_owned());
        if edge {
            self.pending_source = EffortSource::Unknown;
            Some((ObservedModel { id: raw.to_owned() }, source))
        } else {
            None
        }
    }
}

/// Extract the argument of a `/model` slash-command transcript record.
///
/// Both `user` records (`message.content` string) and `system` local-command
/// records (top-level `content` string) carry markup shaped:
/// `<command-name>/model</command-name> … <command-args>id</command-args>`.
/// Returns the raw args, which may be empty (a bare `/model` opens the
/// picker); `None` for a non-`/model` record.
pub fn slash_model_args(content: &str) -> Option<String> {
    if !content.contains("<command-name>/model</command-name>") {
        return None;
    }
    let start = content.find("<command-args>")? + "<command-args>".len();
    let end = content[start..].find("</command-args>")?;
    Some(content[start..start + end].trim().to_owned())
}

/// Whether an observed model id contradicts a requested pin.
///
/// Verdict of the model-pin comparison (`evidence/model-pin-1.md` §3). Three
/// outcomes, because two ids being unequal is *not* the same as them
/// disagreeing: a gateway resolves a catalog id to an upstream vendor name, so
/// an unequal pair is often a correct launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPinVerdict {
    /// The observation is the pin, or the pin's other context-window spelling.
    Honoured,
    /// A different id **in the pin's own namespace** answered. Decidable, and a
    /// refusal offence.
    Mismatch,
    /// The observation is an upstream resolution of the pin, in a vocabulary
    /// the pin cannot be compared against (`model_hub/es1_orange_o50[1m]` →
    /// `claude-opus-5`). Reported, never refused: measured on a real gateway,
    /// a correct launch and a substituted one look identical here.
    Unresolvable,
}

/// Strip a trailing context-window variant suffix (`…[1m]`).
///
/// The suffix selects a real variant, so it is never dropped when *sending* an
/// id; it is only ignored when deciding whether two observations name the same
/// model family.
fn without_context_suffix(id: &str) -> &str {
    let id = id.trim();
    match (id.rfind('['), id.ends_with(']')) {
        (Some(open), true) => id[..open].trim_end(),
        _ => id,
    }
}

/// Whether an id is namespaced (`vendor/model`), i.e. spoken in the vocabulary
/// a gateway catalog and a `/model` verdict use.
fn is_namespaced(id: &str) -> bool {
    id.contains('/')
}

/// Compare an observed effective model against the requested pin.
///
/// The rule, and why it is not equality (measured, `model-pin-1.md` §3):
///
/// - equal, or equal ignoring a `[1m]` context suffix → [`Honoured`];
/// - both ids namespaced but different → [`Mismatch`]. Both the pin and a
///   `/model` verdict speak this vocabulary, so a disagreement here is real:
///   the session is on a model nobody asked for.
/// - the pin is namespaced and the observation is not → [`Unresolvable`]. This
///   is the gateway resolving the pin to a vendor name. A correctly pinned
///   session records `claude-opus-5` for `model_hub/es1_orange_o50[1m]`, and a
///   substituted one records `claude-opus-4-8`; neither upstream name is in the
///   catalog, so this channel cannot tell them apart and must not refuse.
/// - neither namespaced and different → [`Mismatch`]: two bare aliases
///   (`sonnet` vs `haiku`) are the same vocabulary.
///
/// `catalog` is the session's discovered model list when known. An observation
/// that is itself a catalog id confirms the vocabulary is comparable, which is
/// what lets a namespaced disagreement be trusted.
///
/// [`Honoured`]: ModelPinVerdict::Honoured
/// [`Mismatch`]: ModelPinVerdict::Mismatch
/// [`Unresolvable`]: ModelPinVerdict::Unresolvable
#[must_use]
pub fn compare_model_pin(pin: &str, observed: &str, catalog: &[String]) -> ModelPinVerdict {
    let pin = pin.trim();
    let observed = observed.trim();
    if pin.is_empty() || observed.is_empty() {
        // Nothing to contradict: a pin that was never requested cannot mismatch.
        return ModelPinVerdict::Honoured;
    }
    if observed == pin || without_context_suffix(observed) == without_context_suffix(pin) {
        return ModelPinVerdict::Honoured;
    }
    // A catalog hit proves the observation is spoken in the catalog's
    // vocabulary, the same one the pin uses.
    let in_catalog = |id: &str| {
        catalog
            .iter()
            .any(|entry| entry == id || without_context_suffix(entry) == without_context_suffix(id))
    };
    match (is_namespaced(pin), is_namespaced(observed)) {
        (true, true) => ModelPinVerdict::Mismatch,
        (true, false) if in_catalog(observed) => ModelPinVerdict::Mismatch,
        (true, false) => ModelPinVerdict::Unresolvable,
        // A bare pin (`sonnet`) against a namespaced observation is the gateway
        // resolving an alias: `sonnet` → `model_hub/es1_orange_o48`. Not
        // comparable either way round.
        (false, true) => ModelPinVerdict::Unresolvable,
        (false, false) => ModelPinVerdict::Mismatch,
    }
}

/// The effective-model half of a [`ModelPayload`]: the resolved id plus what
/// established it.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveModel {
    /// Resolved model id.
    pub id: String,
    /// What established this id (launch argv, a terminal slash, a Remuda
    /// switch). Same vocabulary as the effort effective observation.
    pub source: EffortSource,
    /// When the observation was made.
    pub observed_at: Timestamp,
}

/// Where the session's model list was discovered.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ModelListSource {
    /// `<config dir>/cache/gateway-models.json` written by Claude Code's
    /// gateway model discovery (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`).
    GatewayDiscovery,
    /// The launch settings/env: `model`, `modelSettings`,
    /// `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`.
    Settings,
    /// Nothing discovered on disk; the built-in alias set the picker always
    /// knows (`opus` / `sonnet` / `haiku` / `auto`).
    Builtin,
}

/// Which `cache/gateway-models.json` file answered a catalog resolution.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ModelCacheScope {
    /// The session's own scoped config dir (`CLAUDE_CONFIG_DIR`).
    ScopedConfigDir,
    /// The host user's conventional `~/.claude` dir, used as a fallback
    /// because the scoped cache had not been written yet.
    HostFallback,
}

/// Provenance of the gateway discovery cache that answered a catalog
/// resolution, so the UI can flag a list the session's own CLI may reject.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCacheInfo {
    /// Scoped session cache or the host fallback.
    pub scope: ModelCacheScope,
    /// `baseUrl` recorded in the cache document, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `fetchedAt` recorded in the cache document, verbatim, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
}

/// How a Remuda-initiated switch selected its id. The verdict (not this
/// marker) remains the acceptance authority; this only says whether the id
/// was offered by the session's own discovered list.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ModelSelectionPath {
    /// The id was listed by the session's own resolved catalog.
    Listed,
    /// The id was not listed; Remuda typed `/model <id>` verbatim and let
    /// the CLI verdict decide.
    Typed,
}

/// The model list a session can actually switch to, with its provenance.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogInfo {
    /// Model ids/aliases, display order, de-duplicated.
    pub models: Vec<String>,
    /// Where the list came from.
    pub source: ModelListSource,
    /// When the list was resolved.
    pub observed_at: Timestamp,
    /// Gateway cache provenance when `source` is gateway discovery: which
    /// cache file answered (scoped or host fallback) and the base URL / fetch
    /// time it recorded. Absent for settings/builtin resolutions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<ModelCacheInfo>,
    /// Whether `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` was present in
    /// the launch environment. Absent when it could not be determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_env: Option<bool>,
}

/// Parse a `cache/gateway-models.json` document into ordered model ids.
///
/// Shape measured on this host:
/// `{ "baseUrl": …, "fetchedAt": …, "models": [{ "id": …, "display_name": …,
/// "description": … }] }`. Entries without a string `id` are skipped.
pub fn parse_gateway_models_json(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .get("models")
        .and_then(|models| models.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_measured_accept_shape() {
        match parse_model_stdout(
            "Set model to `model_hub/es1_orange_o48[1m]` and saved as your default for new sessions",
        ) {
            ModelStdout::Accepted(o) => assert_eq!(o.id, "model_hub/es1_orange_o48[1m]"),
            other => panic!("{other:?}"),
        }
        // The dim ANTHROPIC_MODEL hint rides the second line with more
        // backticked ids — it must not be mistaken for the resolved id.
        let with_hint = "Set model to `model_hub/es1_orange_o50` and saved as your default for \
new sessions\x1b[2m\x1b[22m\n\x1b[2m     ANTHROPIC_MODEL is set to \x1b[22m`model_hub/es1_orange_o48[1m]`\x1b[22m";
        match parse_model_stdout(with_hint) {
            ModelStdout::Accepted(o) => assert_eq!(o.id, "model_hub/es1_orange_o50"),
            other => panic!("{other:?}"),
        }
        // 2.1.221 bold spelling.
        match parse_model_stdout(
            "Set model to \x1b[1mseed-evolving\x1b[22m and saved as your default for new sessions",
        ) {
            ModelStdout::Accepted(o) => assert_eq!(o.id, "seed-evolving"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_kept_and_not_found() {
        assert_eq!(
            parse_model_stdout("Kept model as `model_hub/es1_orange_o48[1m]`"),
            ModelStdout::Kept
        );
        assert_eq!(
            parse_model_stdout("Model 'bogus-xyz-123' not found"),
            ModelStdout::NotFound
        );
        assert_eq!(
            parse_model_stdout("some unrelated command output"),
            ModelStdout::Other
        );
    }

    #[test]
    fn slash_args_shape_kept_raw_including_slashes_and_brackets() {
        let content = "<command-name>/model</command-name>\n<command-message>model</command-message>\n\
            <command-args>model_hub/es1_orange_o50</command-args>";
        assert_eq!(
            slash_model_args(content).as_deref(),
            Some("model_hub/es1_orange_o50")
        );
        let bare = "<command-name>/model</command-name>\n<command-args></command-args>";
        assert_eq!(slash_model_args(bare).as_deref(), Some(""));
        assert!(slash_model_args("<command-name>/clear</command-name>").is_none());
    }

    #[test]
    fn accept_is_an_immediate_edge_and_same_id_dedupes() {
        let mut tracker = ModelTracker::new();
        tracker.mark_launch();
        let first = tracker.observe(Some("claude-opus-5")).expect("first edge");
        assert_eq!(first.0.id, "claude-opus-5");
        assert_eq!(first.1, EffortSource::Launch);
        assert!(tracker.observe(Some("claude-opus-5")).is_none());
        // Verdict accept settles immediately, attributed to the human.
        tracker.note_slash("model_hub/x", false);
        let edge = tracker
            .note_stdout(
                "Set model to `model_hub/x` and saved as your default for new sessions",
                false,
            )
            .expect("edge");
        assert_eq!(edge.0.id, "model_hub/x");
        assert_eq!(edge.1, EffortSource::Slash);
        assert!(tracker.observe(Some("model_hub/x")).is_none());
    }

    #[test]
    fn remuda_awaiting_wins_attribution_even_when_alias_resolves() {
        let mut tracker = ModelTracker::new();
        tracker.observe(Some("model_hub/es1_orange_o48[1m]"));
        tracker.note_slash("sonnet", true);
        // `sonnet` resolves through the pinned env back to the same concrete id.
        let edge = tracker.note_stdout(
            "Set model to `model_hub/es1_orange_o48[1m]` and saved as your default …",
            true,
        );
        // Same concrete id: no edge, but awaiting attribution is cleared.
        assert!(edge.is_none());
        // A later natural edge must not be credited to Remuda.
        tracker.note_slash("other", false);
        let next = tracker
            .note_stdout("Set model to `other` and saved as …", false)
            .unwrap();
        assert_eq!(next.1, EffortSource::Slash);
    }

    #[test]
    fn kept_and_not_found_clear_attribution() {
        let mut tracker = ModelTracker::new();
        tracker.observe(Some("a"));
        tracker.note_slash("b", true);
        assert!(tracker.note_stdout("Kept model as `a`", true).is_none());
        tracker.note_slash("bogus", true);
        assert!(
            tracker
                .note_stdout("Model 'bogus' not found", true)
                .is_none()
        );
        assert!(tracker.observe(Some("a")).is_none());
    }

    #[test]
    fn gateway_cache_parse_orders_ids_and_skips_junk() {
        let body = serde_json::json!({
            "baseUrl": "https://relay.example",
            "fetchedAt": 1,
            "models": [
                {"id": "ark/a", "display_name": "A"},
                {"display_name": "no id"},
                {"id": "  "},
                {"id": "model_hub/b"}
            ]
        })
        .to_string();
        assert_eq!(
            parse_gateway_models_json(&body),
            vec!["ark/a".to_string(), "model_hub/b".to_string()]
        );
        assert!(parse_gateway_models_json("not json").is_empty());
    }

    #[test]
    fn event_id_is_stable_and_namespaced() {
        let a = model_event_id("ins_one", "msg_1", "ark/a");
        assert_eq!(a, model_event_id("ins_one", "msg_1", "ark/a"));
        assert_ne!(a, model_event_id("ins_one", "msg_2", "ark/a"));
        assert_ne!(a, model_event_id("ins_two", "msg_1", "ark/a"));
        assert!(a.as_id().as_str().starts_with("evt_"));
    }

    /// model-pin-1 §3: the pin comparison, anchored on ids measured against a
    /// real gateway rather than on invented pairs.
    #[test]
    fn a_gateway_resolution_is_not_a_mismatch() {
        // Measured: a session correctly pinned to o50 records `claude-opus-5`.
        // Refusing this was the flaw in the equality gate.
        assert_eq!(
            compare_model_pin("model_hub/es1_orange_o50[1m]", "claude-opus-5", &[]),
            ModelPinVerdict::Unresolvable
        );
        // And the substituted case looks identical through this channel, which
        // is precisely why neither may refuse.
        assert_eq!(
            compare_model_pin("model_hub/es1_orange_o48[1m]", "claude-opus-4-8", &[]),
            ModelPinVerdict::Unresolvable
        );
    }

    #[test]
    fn the_pin_answering_is_honoured_with_or_without_its_context_suffix() {
        assert_eq!(
            compare_model_pin("ark/seed-evolving[1m]", "ark/seed-evolving", &[]),
            ModelPinVerdict::Honoured
        );
        assert_eq!(
            compare_model_pin("ark/seed-evolving", "ark/seed-evolving[1m]", &[]),
            ModelPinVerdict::Honoured
        );
        assert_eq!(
            compare_model_pin(
                "model_hub/es1_orange_o50[1m]",
                "model_hub/es1_orange_o50[1m]",
                &[]
            ),
            ModelPinVerdict::Honoured
        );
    }

    /// The decidable case: a `/model` verdict speaks the pin's vocabulary, so a
    /// different namespaced id is a real substitution — the demo's failure.
    #[test]
    fn a_different_id_in_the_pins_namespace_is_a_mismatch() {
        assert_eq!(
            compare_model_pin(
                "model_hub/es1_orange_o50[1m]",
                "model_hub/es1_orange_o48[1m]",
                &[]
            ),
            ModelPinVerdict::Mismatch
        );
        // Two bare aliases are also one vocabulary.
        assert_eq!(
            compare_model_pin("sonnet", "haiku", &[]),
            ModelPinVerdict::Mismatch
        );
    }

    /// A catalog hit proves the observation is spoken in the catalog's
    /// vocabulary, which upgrades an otherwise unresolvable pair to a mismatch.
    #[test]
    fn a_catalog_hit_makes_an_unnamespaced_observation_comparable() {
        let catalog = vec!["es1_orange_o48".to_owned(), "es1_orange_o50".to_owned()];
        assert_eq!(
            compare_model_pin("model_hub/es1_orange_o50[1m]", "es1_orange_o48", &catalog),
            ModelPinVerdict::Mismatch
        );
        // Not in the catalog: still an upstream name we cannot reason about.
        assert_eq!(
            compare_model_pin("model_hub/es1_orange_o50[1m]", "claude-opus-5", &catalog),
            ModelPinVerdict::Unresolvable
        );
    }

    /// A bare alias pin resolving to a concrete gateway id is normal.
    #[test]
    fn an_alias_pin_resolving_to_a_gateway_id_is_not_a_mismatch() {
        assert_eq!(
            compare_model_pin("sonnet", "model_hub/es1_orange_o48", &[]),
            ModelPinVerdict::Unresolvable
        );
    }

    /// No pin, or nothing observed yet: nothing to contradict.
    #[test]
    fn an_absent_pin_or_observation_never_mismatches() {
        assert_eq!(
            compare_model_pin("", "claude-opus-5", &[]),
            ModelPinVerdict::Honoured
        );
        assert_eq!(
            compare_model_pin("model_hub/es1_orange_o50[1m]", "   ", &[]),
            ModelPinVerdict::Honoured
        );
    }
}
