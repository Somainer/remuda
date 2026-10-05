//! §9.1 effective-effort tracking shared by every Claude transcript mapper.
//!
//! The driver's live mapper (`remuda-driver::TranscriptMapper`) and the
//! journal tailer (`remuda-journal`) must agree on what an assistant record
//! says about effort and how a `/effort` slash record attributes the change,
//! so the state machine lives here rather than being copied.
//!
//! ## Transcript shapes
//!
//! Measured on 2.1.221, re-measured on 2.1.272/2.1.273 (coupled,
//! `docs/design/evidence/effort-sync-1.md`…`effort-sync-3.md`) and on
//! 2.1.289 (decoupled, `effort-sync-4.md`; ADR D-056):
//!
//! - assistant records carry a top-level `effort: "low|medium|high|xhigh|max"`
//!   and a top-level `perTurnEffort: string | null`, and a top-level
//!   `version`; the ultracode flag is NEVER on an assistant record;
//! - `/effort …` is journaled as a `user` record whose message content is
//!   `<command-name>/effort</command-name>` … `<command-args>…</command-args>`
//!   followed by a SECOND user record carrying the verdict in
//!   `<local-command-stdout>…</local-command-stdout>`;
//! - the ultracode toggle additionally rides `ultra_effort_enter` (full or
//!   sparse) / `ultra_effort_exit` attachments on the next prompt.
//!
//! ## Version gate (D-056)
//!
//! The tracker learns the Claude Code version from every transcript record's
//! `version` field ([`EffortTracker::note_version`]); the live mapper also
//! seeds it from the pinned binary version.
//!
//! - **Coupled (2.1.203–2.1.283):** `ultracode` is the xhigh level plus the
//!   workflow flag; accepting any plain level positively turns the flag off.
//! - **Decoupled (≥ 2.1.284):** the toggle is orthogonal and latches at ANY
//!   level; a plain level verdict never changes the flag, and assistant
//!   records never clear it.
//! - **Unknown:** like decoupled for flag purposes — a plain level verdict
//!   never changes the flag, and `ultracode` is not parsed as a level.
//!
//! ## Verdict strings
//!
//! Every string [`parse_effort_stdout`] matches is copied verbatim from a
//! recorded transcript (see the parser docs). The only strings recorded on
//! 2.1.277 rather than 2.1.289 are the two `CLAUDE_CODE_EFFORT_LEVEL`
//! override shapes (2.1.289 wording remains D-056 open question 3); both
//! pivot on the stable env-var name and the word "overrides".

use crate::{EffortName, EffortSource, EventId, Id};

/// Deterministic observation id for an effort edge read from one native
/// assistant record.
///
/// Both channels that can observe the edge — the driver's live transcript
/// hydrator and the journal file tailer — process the *same* native record, so
/// deriving the event id from `(instance scope, assistant record id, level)`
/// via [`Id::derive`] makes the observation stable across channels: re-tailing
/// the file after the live event was already journaled does not mint a second
/// identity for the same edge.
pub fn effort_event_id(scope: &str, assistant_native_id: &str, name: EffortName) -> EventId {
    let native = format!("effort:{}:{assistant_native_id}", name_wire(name));
    // Id::derive is infallible for the registered `evt` prefix and valid
    // `(scope, native)` strings; the branded constructor enforces the prefix.
    let id: Id = Id::derive("evt", scope, &native).expect("evt prefix registered");
    EventId::try_from(String::from(id)).expect("derive with the evt prefix yields an EventId")
}

/// Native-key label for an effort edge settled by a `/effort` accept verdict
/// record (one that carries no assistant message id). Shared by the live
/// mapper and the journal tailer so both derive the same event id.
pub const EFFORT_STDOUT_NATIVE: &str = "effort-stdout";
/// Native-key label for an effort edge learned from a `/effort status`
/// verdict. Shared by the live mapper and the journal tailer.
pub const EFFORT_STATUS_NATIVE: &str = "effort-status";
/// Native-key label for an effort level learned from a `/model` verdict whose
/// text ends `` with `<level>` effort``.
pub const EFFORT_MODEL_NATIVE: &str = "effort-model-stdout";

/// Deterministic event id for an effort edge settled by a `/effort` or
/// `/model` stdout verdict instead of an assistant record. `label` is one of
/// the [`EFFORT_STDOUT_NATIVE`] / [`EFFORT_STATUS_NATIVE`] /
/// [`EFFORT_MODEL_NATIVE`] constants (or the bare attachment type), and
/// `record_native_id` is the verdict/attachment record's uuid.
pub fn effort_record_event_id(
    scope: &str,
    label: &str,
    record_native_id: &str,
    name: EffortName,
) -> EventId {
    effort_event_id(scope, &format!("{label}:{record_native_id}"), name)
}

fn name_wire(name: EffortName) -> &'static str {
    match name {
        EffortName::Low => "low",
        EffortName::Medium => "medium",
        EffortName::High => "high",
        EffortName::Xhigh => "xhigh",
        EffortName::Max => "max",
        EffortName::Ultra => "ultra",
        // Claude assistant records never report the legacy `minimal` word.
        EffortName::Minimal => "minimal",
    }
}

/// Claude Code effort semantics, gated by the transcript record version
/// (D-056).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EffortSemantics {
    /// No parseable version seen yet. A plain level verdict never changes the
    /// ultracode flag and `ultracode` is not read as a level word.
    #[default]
    Unknown,
    /// 2.1.203–2.1.283: ultracode is xhigh plus the workflow flag; any plain
    /// level accept clears it.
    Coupled,
    /// ≥ 2.1.284: ultracode is an orthogonal session toggle that latches at
    /// every level.
    Decoupled,
}

/// Parse a Claude Code version string into effort semantics.
///
/// Accepts the shapes transcript records actually carry, e.g. `2.1.289` and
/// `2.1.277 (Claude Code)`: a leading dotted-numeric head of at least
/// major.minor is required. A missing patch component reads as 0. Unparseable
/// input yields [`EffortSemantics::Unknown`] via `None`.
pub fn parse_effort_version(raw: &str) -> Option<EffortSemantics> {
    let head: String = raw
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = head
        .split('.')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next().unwrap_or(0);
    Some(if (major, minor, patch) >= (2, 1, 284) {
        EffortSemantics::Decoupled
    } else {
        EffortSemantics::Coupled
    })
}

/// Parse one of Claude's five plain effort levels (`low…max`). The input
/// alias `ultracode` is NOT a level here; use [`EffortTracker::parse_level`]
/// for version-aware parsing.
pub fn parse_plain_level(word: &str) -> Option<EffortName> {
    match word.trim().to_ascii_lowercase().as_str() {
        "low" => Some(EffortName::Low),
        "medium" => Some(EffortName::Medium),
        "high" => Some(EffortName::High),
        "xhigh" => Some(EffortName::Xhigh),
        "max" => Some(EffortName::Max),
        _ => None,
    }
}

/// An effort level as read off an assistant transcript record.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ObservedEffort {
    /// Observed level.
    pub name: EffortName,
    /// Observed dynamic-workflow flag; positively true after an ultracode-on
    /// verdict/attachment, positively false after an ultracode-off verdict,
    /// a slider ` · Ultracode off` suffix or a coupled plain-level accept, and
    /// `None` until this process has evidence either way.
    pub ultracode: Option<bool>,
}

/// What the `<local-command-stdout>` sibling of a `/effort` slash record says.
///
/// The slash record only proves the bytes were submitted (on 2.1.272 it is
/// written even for a dismissed dialog or an invalid argument); the stdout
/// line is the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortStdout {
    /// A level or flag change accepted; this is what is now in effect.
    ///
    /// `ultracode` is `None` for a decoupled plain-level verdict that does not
    /// mention the toggle: the latch (if any) is preserved.
    Accepted(ObservedEffort),
    /// `/effort status` / `/effort current` output: an observation only, with
    /// no bridge effect. The level is the resolved level (`auto` resolves to
    /// it). `ultracode` is positively false when the ` · Ultracode on`
    /// suffix is absent.
    Status(ObservedEffort),
    /// `Kept effort level as <level>` — the confirmation dialog was dismissed.
    Kept,
    /// `Invalid argument: <x>. Valid options are: …`.
    Invalid,
    /// `Ultracode needs dynamic workflows enabled (see /config). …`.
    WorkflowsDisabled,
    /// `Ultracode isn't available on <model>. …`.
    UnavailableForModel,
    /// A verdict saying `CLAUDE_CODE_EFFORT_LEVEL=…` pins the level.
    EnvOverride,
    /// `Effort level set to auto` or unrelated stdout.
    Other,
}

impl EffortStdout {
    /// Stable journal/bridge rejection reason for a refusal verdict, or
    /// `None` for accepts, status and unrelated output.
    #[must_use]
    pub fn reject_reason(&self) -> Option<&'static str> {
        match self {
            EffortStdout::Kept => Some("dialog-kept"),
            EffortStdout::Invalid => Some("invalid-argument"),
            EffortStdout::WorkflowsDisabled => Some("ultracode-workflows-disabled"),
            EffortStdout::UnavailableForModel => Some("ultracode-unavailable-for-model"),
            EffortStdout::EnvOverride => Some("env-override"),
            EffortStdout::Accepted(_) | EffortStdout::Status(_) | EffortStdout::Other => None,
        }
    }
}

/// ` · Ultracode on|off` suffix a slider (or any joined) verdict carries when
/// it applies the toggle together with the level.
fn suffix_ultracode(t: &str) -> Option<bool> {
    if t.ends_with(" · Ultracode on") {
        Some(true)
    } else if t.ends_with(" · Ultracode off") {
        Some(false)
    } else {
        None
    }
}

/// Level/flag for an accepted plain-level verdict, applying the version gate
/// to the missing flag: coupled builds positively clear it, decoupled/unknown
/// leave the latch untouched.
fn plain_accept(name: EffortName, t: &str, semantics: EffortSemantics) -> ObservedEffort {
    let ultracode = match suffix_ultracode(t) {
        Some(flag) => Some(flag),
        None if semantics == EffortSemantics::Coupled => Some(false),
        None => None,
    };
    ObservedEffort { name, ultracode }
}

/// The `<level>` in `… Effort stays <level>.` of an ultracode on/off verdict.
fn effort_stays_level(t: &str, semantics: EffortSemantics) -> Option<ObservedEffort> {
    let rest = t.split("Effort stays ").nth(1)?;
    let word = rest.trim().trim_end_matches('.');
    let name = EffortTracker::parse_level(word, semantics)?;
    Some(ObservedEffort {
        name,
        // The on/off prefix is the positive flag evidence.
        ultracode: None,
    })
}

/// The clamped level in
/// `Effort '<x>' exceeds the cap for <model> …; set to '<y>' instead …`.
fn clamped_level(t: &str, semantics: EffortSemantics) -> Option<ObservedEffort> {
    let marker = "set to '";
    let start = t.find(marker)? + marker.len();
    let end = start + t[start..].find('\'')?;
    let name = EffortTracker::parse_level(&t[start..end], semantics)?;
    Some(plain_accept(name, t, semantics))
}

/// Parse the verdict out of a `/effort` `<local-command-stdout>` line.
///
/// The strings below are copied verbatim from recorded transcripts:
///
/// Coupled (2.1.221 / 2.1.272):
/// - `Set effort level to xhigh (saved as your default for new sessions): …`
/// - `Set effort level to ultracode (this session only): xhigh + dynamic …`
/// - `Set effort level: high (saved as your default for new sessions)` (older
///   spelling, accepted defensively)
/// - `Kept effort level as xhigh`
/// - `Invalid argument: bogus. Valid options are: low, medium, …`
/// - `Effort level set to auto`
///
/// Decoupled (2.1.289, effort-sync-4):
/// - `Ultracode on (this session only): dynamic workflows on every task.
///   Effort stays <level>.`
/// - `Ultracode off. Effort stays <level>.`
/// - `Set effort level to <level> (saved as your default for new sessions|this
///   session only): <desc>`, optionally joined with ` · Ultracode off|on`
/// - `Effort '<x>' exceeds the cap for <model> …; set to '<y>' instead …`
/// - `Ultracode needs dynamic workflows enabled (see /config). Valid …`
/// - `Ultracode isn't available on <model>. Valid options are: …`
/// - `Current effort level: <level> (<desc>) · Ultracode on`
/// - `Effort level: auto (currently <level>) · Ultracode on`
///
/// Env override (measured on 2.1.277, D-056 open question 3 for 2.1.289):
/// - `Not applied: CLAUDE_CODE_EFFORT_LEVEL=high overrides effort this
///   session, and max is session-only (nothing saved)`
/// - `CLAUDE_CODE_EFFORT_LEVEL=high overrides this session — clear it and
///   xhigh takes over`
pub fn parse_effort_stdout(text: &str, semantics: EffortSemantics) -> EffortStdout {
    let t = text.trim();

    // Coupled accept (2.1.221/2.1.272): ultracode is the xhigh level plus the
    // flag. Unreachable on ≥ 2.1.284, where this verdict shape is not emitted,
    // and unambiguous wherever it appears.
    if t.contains("Set effort level to ultracode") {
        return EffortStdout::Accepted(ObservedEffort {
            name: EffortName::Xhigh,
            ultracode: Some(true),
        });
    }
    // Cap clamp (2.1.289): accepted, at the level the cap set.
    if t.starts_with("Effort '")
        && let Some(observed) = clamped_level(t, semantics)
    {
        return EffortStdout::Accepted(observed);
    }
    // Decoupled ultracode toggle verdicts, carrying the level the flag toggled
    // at ("Effort stays <level>").
    if t.starts_with("Ultracode on")
        && let Some(mut observed) = effort_stays_level(t, semantics)
    {
        observed.ultracode = Some(true);
        return EffortStdout::Accepted(observed);
    }
    if t.starts_with("Ultracode off")
        && let Some(mut observed) = effort_stays_level(t, semantics)
    {
        observed.ultracode = Some(false);
        return EffortStdout::Accepted(observed);
    }
    // Plain level accepts, current spelling ("to") and the 2.1.221 spelling.
    if let Some(rest) = t.strip_prefix("Set effort level to ")
        && let Some(name) = first_word(rest).and_then(|w| EffortTracker::parse_level(w, semantics))
    {
        return EffortStdout::Accepted(plain_accept(name, t, semantics));
    }
    if let Some(rest) = t.strip_prefix("Set effort level: ")
        && let Some(name) = first_word(rest).and_then(|w| EffortTracker::parse_level(w, semantics))
    {
        return EffortStdout::Accepted(plain_accept(name, t, semantics));
    }
    // /effort status|current: an observation only.
    if let Some(status) = parse_status(t) {
        return EffortStdout::Status(status);
    }
    // Refusals with stable reason codes (D-056).
    if t.starts_with("Ultracode needs dynamic workflows enabled") {
        return EffortStdout::WorkflowsDisabled;
    }
    if t.starts_with("Ultracode isn't available on") {
        return EffortStdout::UnavailableForModel;
    }
    if (t.starts_with("Not applied: CLAUDE_CODE_EFFORT_LEVEL=")
        || t.starts_with("CLAUDE_CODE_EFFORT_LEVEL="))
        && t.contains("overrid")
    {
        return EffortStdout::EnvOverride;
    }
    if t.starts_with("Kept effort level") {
        return EffortStdout::Kept;
    }
    if t.starts_with("Invalid argument") && t.contains("Valid options are") {
        return EffortStdout::Invalid;
    }
    EffortStdout::Other
}

/// Parse a `/effort status|current` line into a level/flag observation.
fn parse_status(t: &str) -> Option<ObservedEffort> {
    let flag = match suffix_ultracode(t) {
        Some(on) => Some(on),
        // The suffix is absent exactly when the toggle is off (effort-sync-4
        // §5 row 8): positive evidence, not uncertainty.
        None => Some(false),
    };
    if let Some(rest) = t.strip_prefix("Current effort level: ") {
        let name = first_word(rest).and_then(parse_plain_level)?;
        return Some(ObservedEffort {
            name,
            ultracode: flag,
        });
    }
    if let Some(rest) = t.strip_prefix("Effort level: auto (currently ") {
        // Cut at the closing paren; the ` · Ultracode …` suffix follows it.
        let inside = rest.split(')').next().unwrap_or(rest);
        let word = inside.split_whitespace().next()?;
        let name = parse_plain_level(word)?;
        return Some(ObservedEffort {
            name,
            ultracode: flag,
        });
    }
    None
}

fn first_word(rest: &str) -> Option<&str> {
    let w = rest
        .split(|c: char| c.is_whitespace() || c == ':' || c == '(')
        .next()
        .unwrap_or("");
    (!w.is_empty()).then_some(w)
}

/// What a `/effort` slash record's arguments ask for (D-056 version gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSlash {
    /// `/effort <level>` — one of the five plain levels.
    Level(EffortName),
    /// `/effort ultracode` (decoupled) or `/effort ultracode on`.
    UltracodeOn {
        /// True for the bare `/effort ultracode` spelling, which on coupled
        /// builds is the xhigh-plus-flag level command.
        bare: bool,
    },
    /// `/effort ultracode off`.
    UltracodeOff,
    /// Bare `/effort` — the slider; a human action, never Remuda-armed.
    Slider,
    /// `auto`, `status`, `current`, or invalid arguments. No switch.
    NotASwitch,
}

/// Classify the arguments of a `/effort` slash record. See [`EffortSlash`].
pub fn classify_effort_slash(args: &str, semantics: EffortSemantics) -> EffortSlash {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    match tokens.as_slice() {
        [] => EffortSlash::Slider,
        [word] => {
            // The bare word is its own token in both eras; note_slash decides
            // whether it means xhigh-plus-flag (coupled) or a flag-only toggle
            // (decoupled/unknown).
            if word.eq_ignore_ascii_case("ultracode") {
                return EffortSlash::UltracodeOn { bare: true };
            }
            if let Some(name) = EffortTracker::parse_level(word, semantics) {
                return EffortSlash::Level(name);
            }
            EffortSlash::NotASwitch
        }
        [word, on_off] if word.eq_ignore_ascii_case("ultracode") => {
            if on_off.eq_ignore_ascii_case("on") {
                EffortSlash::UltracodeOn { bare: false }
            } else if on_off.eq_ignore_ascii_case("off") {
                EffortSlash::UltracodeOff
            } else {
                // `ultracode bogus` — invalid argument.
                EffortSlash::NotASwitch
            }
        }
        _ => EffortSlash::NotASwitch,
    }
}

/// Transcript-side effective-effort state: dedup and source attribution.
///
/// Edges only: [`EffortTracker::observe`] returns `None` for an unchanged
/// level, so a long turn with many assistant records emits once.
///
/// Pure state shared by both transcript mappers; not a wire type.
#[derive(Debug, Clone)]
pub struct EffortTracker {
    /// Coupled/decoupled, learned from transcript `version` fields.
    semantics: EffortSemantics,
    last: Option<ObservedEffort>,
    pending_source: EffortSource,
    /// Coupled builds only: a `/effort ultracode` slash record armed xhigh +
    /// flag ahead of its verdict/assistant record.
    ultracode_pending: bool,
    /// Positively observed flag state for THIS process; `None` until the
    /// first verdict/status/attachment says either way.
    flag: Option<bool>,
    /// A Remuda level switch awaiting its read-back at this level. Flag
    /// toggles on decoupled builds do not arm this: the level stays put.
    awaiting: Option<EffortName>,
    /// D-056 (4): only records produced by the CURRENT process may settle
    /// state. While a live mapper replays a resumed session's pre-launch
    /// history this is `false` and every state-mutating input is ignored; the
    /// first record the current process produces flips it on forever. The
    /// journal file tailer replays history on purpose and never engages this
    /// gate, so the default is `true`.
    current_process: bool,
}

impl Default for EffortTracker {
    fn default() -> Self {
        Self {
            semantics: EffortSemantics::Unknown,
            last: None,
            pending_source: EffortSource::Unknown,
            ultracode_pending: false,
            flag: None,
            awaiting: None,
            current_process: true,
        }
    }
}

impl EffortTracker {
    /// Parse the five Claude levels, plus the `ultracode` spelling on COUPLED
    /// builds only (there it reads as xhigh). `auto` and unknown words yield
    /// `None` — they are not levels.
    pub fn parse_level(word: &str, semantics: EffortSemantics) -> Option<EffortName> {
        let word = word.trim();
        if word.eq_ignore_ascii_case("ultracode") {
            return (semantics == EffortSemantics::Coupled).then_some(EffortName::Xhigh);
        }
        parse_plain_level(word)
    }

    /// Construct an empty tracker (no level observed yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Current version-gated semantics.
    #[must_use]
    pub fn semantics(&self) -> EffortSemantics {
        self.semantics
    }

    /// Whether records fed now are produced by the current process. While this
    /// is false a live mapper must not arm or settle a switch bridge from a
    /// replayed slash record or verdict, even one whose words match a switch
    /// that was armed concurrently.
    #[must_use]
    pub fn is_current_process(&self) -> bool {
        self.current_process
    }

    /// The last settled effective state, if any. Lets a mapper resolve its
    /// switch bridge with the post-latch observation even when a verdict
    /// produced no edge.
    #[must_use]
    pub fn last_observed(&self) -> Option<ObservedEffort> {
        self.last
    }

    /// Learn the semantics from one transcript record's `version` field. A
    /// later record cannot move a known gate back to unknown. All records of a
    /// process carry one version, so the order they are fed in does not
    /// matter.
    pub fn note_version(&mut self, version: &str) {
        if let Some(found) = parse_effort_version(version) {
            self.semantics = found;
        }
    }

    /// Seed the semantics from the pinned binary version before any transcript
    /// record exists (live mapper only).
    pub fn seed_version(&mut self, version: &str) {
        if self.semantics == EffortSemantics::Unknown {
            self.note_version(version);
        }
    }

    /// Put the tracker in pre-launch history mode (live mapper only). Until
    /// [`Self::mark_current_process`] is called, every state-mutating record
    /// (assistant levels, verdicts, attachments, slash arms) is ignored: a
    /// resumed session's replayed verdicts must neither set the effective
    /// state nor settle a fresh switch (D-056 (4)). The version gate is
    /// deliberately not affected — the mapper needs the semantics before the
    /// first current-process record.
    pub fn begin_history(&mut self) {
        self.current_process = false;
    }

    /// Mark every record fed from now on as produced by the CURRENT process.
    /// The live mapper calls this at the top of each mapped record; it flips
    /// permanently on the first one, so a resumed tail starting at end-of-file
    /// flips on the first post-launch record, and a fresh tail flips on its
    /// SessionStart record.
    pub fn mark_current_process(&mut self) {
        self.current_process = true;
    }

    /// Record a `/effort …` slash record. `args` is the raw (trimmed,
    /// lowercased) command-args body, possibly empty for the bare slider;
    /// `from_remuda` says whether Remuda typed the bytes (otherwise the human
    /// gets the credit). Returns false for arguments that are not a switch
    /// (`auto`, `status`, `current`, invalid words).
    pub fn note_slash(&mut self, args: &str, from_remuda: bool) -> bool {
        // A replayed slash from before this process launched arms nothing.
        if !self.current_process {
            return false;
        }
        match classify_effort_slash(args, self.semantics) {
            EffortSlash::Level(name) => {
                self.ultracode_pending = false;
                self.pending_source = if from_remuda {
                    EffortSource::Remuda
                } else {
                    EffortSource::Slash
                };
                if from_remuda {
                    self.awaiting = Some(name);
                }
                true
            }
            EffortSlash::UltracodeOn { bare } => {
                // Only the coupled bare word is a level command (xhigh); the
                // decoupled toggle does not move the level at all.
                self.ultracode_pending = bare && self.semantics == EffortSemantics::Coupled;
                self.pending_source = if from_remuda {
                    EffortSource::Remuda
                } else {
                    EffortSource::Slash
                };
                if from_remuda && bare && self.semantics == EffortSemantics::Coupled {
                    self.awaiting = Some(EffortName::Xhigh);
                }
                true
            }
            EffortSlash::UltracodeOff => {
                self.ultracode_pending = false;
                self.pending_source = if from_remuda {
                    EffortSource::Remuda
                } else {
                    EffortSource::Slash
                };
                true
            }
            EffortSlash::Slider => {
                // The slider is always a human action (Remuda types words).
                self.ultracode_pending = false;
                self.pending_source = EffortSource::Slash;
                true
            }
            EffortSlash::NotASwitch => false,
        }
    }

    /// Settle a switch from its `<local-command-stdout>` verdict. Returns an
    /// edge observation for an accept/status that changes the effective state;
    /// a dismiss/invalid/refusal clears the awaiting attribution and returns
    /// `None`.
    ///
    /// This is the only channel that proves acceptance immediately (the slash
    /// record is written before the dialog resolves). The caller resolves its
    /// switch bridge independently of the edge: a switch to the state already
    /// in effect is still an accepted switch.
    pub fn note_stdout(
        &mut self,
        stdout: &str,
        from_remuda: bool,
    ) -> Option<(ObservedEffort, EffortSource)> {
        // A replayed verdict from before this process launched settles nothing.
        if !self.current_process {
            return None;
        }
        match parse_effort_stdout(stdout, self.semantics) {
            EffortStdout::Accepted(observed) => self.apply_accept(observed, from_remuda),
            EffortStdout::Status(observed) => self.note_status(observed),
            // No state change; a later natural edge must not be credited to a
            // Remuda switch that Claude actually refused.
            EffortStdout::Kept
            | EffortStdout::Invalid
            | EffortStdout::WorkflowsDisabled
            | EffortStdout::UnavailableForModel
            | EffortStdout::EnvOverride => {
                // No state change; a later natural edge must not be credited to
                // a slash/switch that Claude actually refused.
                self.awaiting = None;
                self.ultracode_pending = false;
                self.pending_source = EffortSource::Unknown;
                None
            }
            EffortStdout::Other => None,
        }
    }

    /// Apply one parsed accept verdict.
    fn apply_accept(
        &mut self,
        parsed: ObservedEffort,
        from_remuda: bool,
    ) -> Option<(ObservedEffort, EffortSource)> {
        let flag = match parsed.ultracode {
            Some(flag) => Some(flag),
            // A decoupled/unknown plain-level verdict leaves the flag where
            // the last evidence put it; a coupled verdict positively clears.
            None if self.semantics == EffortSemantics::Coupled => Some(false),
            None => self.flag,
        };
        if let Some(flag) = flag {
            self.flag = Some(flag);
        }
        self.ultracode_pending = false;
        let mut source = if from_remuda {
            EffortSource::Remuda
        } else {
            EffortSource::Slash
        };
        if self.awaiting.is_some() && (from_remuda || self.awaiting == Some(parsed.name)) {
            // The verdict — including a clamp to a different level — is the
            // acceptance authority and positively settles the armed switch.
            source = EffortSource::Remuda;
        }
        self.awaiting = None;
        let observed = ObservedEffort {
            name: parsed.name,
            ultracode: flag,
        };
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        if edge {
            self.pending_source = EffortSource::Unknown;
            Some((observed, source))
        } else {
            None
        }
    }

    /// Feed a `/effort status|current` observation. Status never settles an
    /// awaiting switch and is never attributed to one ([`EffortSource::Unknown`]
    /// ): it reports the current state rather than changing it. The reported
    /// level is ignored while an assistant record has established the
    /// effective level (the two can disagree — launch case g).
    pub fn note_status(
        &mut self,
        parsed: ObservedEffort,
    ) -> Option<(ObservedEffort, EffortSource)> {
        if !self.current_process {
            return None;
        }
        if let Some(flag) = parsed.ultracode {
            self.flag = Some(flag);
        }
        let name = self.last.map(|last| last.name).unwrap_or(parsed.name);
        let observed = ObservedEffort {
            name,
            ultracode: self.flag,
        };
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        edge.then_some((observed, EffortSource::Unknown))
    }

    /// Feed the `ultra_effort_enter|exit` attachment riding the next prompt.
    /// Both `full` and `sparse` enters set the flag on; a sparse repeat while
    /// it is already on is therefore not an edge. When the attachment arrives
    /// before any level is known it only latches: the edge is emitted together
    /// with the first level.
    pub fn note_ultra_attachment(
        &mut self,
        enters: bool,
    ) -> Option<(ObservedEffort, EffortSource)> {
        if !self.current_process {
            return None;
        }
        if self.flag == Some(enters) {
            return None;
        }
        self.flag = Some(enters);
        let Some(last) = self.last else {
            // Latch only; the first assistant/verdict level carries the flag.
            return None;
        };
        let observed = ObservedEffort {
            name: last.name,
            ultracode: Some(enters),
        };
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        edge.then_some((observed, EffortSource::Slash))
    }

    /// Declare the launch-level source before the first assistant record.
    pub fn mark_launch(&mut self) {
        self.pending_source = EffortSource::Launch;
    }

    /// Arm a Remuda level switch awaiting read-back at `name`.
    pub fn arm_awaiting(&mut self, name: EffortName) {
        self.awaiting = Some(name);
    }

    /// Feed one assistant record's raw `effort` / `perTurnEffort` strings;
    /// `perTurnEffort` is the fallback. Returns an edge observation only.
    ///
    /// Assistant records corroborate the level but never carry the ultracode
    /// flag: on decoupled builds the latched flag is preserved at ANY level
    /// and assistant records never clear it; on coupled builds the flag only
    /// shows at xhigh.
    pub fn observe(
        &mut self,
        effort: Option<&str>,
        per_turn: Option<&str>,
    ) -> Option<(ObservedEffort, EffortSource)> {
        // An assistant record from before this process launched establishes
        // neither the level nor the flag.
        if !self.current_process {
            return None;
        }
        let raw = effort
            .filter(|value| !value.is_empty())
            .or_else(|| per_turn.filter(|value| !value.is_empty()))?;
        let name = Self::parse_level(raw, self.semantics)?;
        let mut source = self.pending_source;
        if self.awaiting == Some(name) {
            source = EffortSource::Remuda;
            self.awaiting = None;
        }
        self.observe_level(name, source)
    }

    /// Observe a level from a non-assistant verdict with an explicit source —
    /// the `/model` verdict's `` with `<level>` effort`` suffix. Flag
    /// handling is the assistant-record rule (the flag is never stated here),
    /// and effort-bridge awaiting is left untouched.
    pub fn observe_level(
        &mut self,
        name: EffortName,
        source: EffortSource,
    ) -> Option<(ObservedEffort, EffortSource)> {
        if !self.current_process {
            return None;
        }
        let ultracode = self.assistant_flag(name);
        let observed = ObservedEffort { name, ultracode };
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        if edge {
            self.pending_source = EffortSource::Unknown;
            self.ultracode_pending = false;
            Some((observed, source))
        } else {
            None
        }
    }

    /// The flag an assistant record (or model-effort verdict) at `name`
    /// presents given the latch and the version gate.
    fn assistant_flag(&self, name: EffortName) -> Option<bool> {
        match self.semantics {
            EffortSemantics::Coupled => {
                if self.ultracode_pending || (self.flag == Some(true) && name == EffortName::Xhigh)
                {
                    Some(true)
                } else {
                    self.last
                        .filter(|last| last.name == name)
                        .and_then(|last| last.ultracode)
                }
            }
            EffortSemantics::Decoupled | EffortSemantics::Unknown => {
                if let Some(flag) = self.flag {
                    Some(flag)
                } else {
                    self.last
                        .filter(|last| last.name == name)
                        .and_then(|last| last.ultracode)
                }
            }
        }
    }
}

/// Extract the argument body of a `/effort` slash-command transcript record.
///
/// Such records carry message content shaped:
/// `<command-name>/effort</command-name> … <command-args>xhigh</command-args>`.
/// Returns the lowercased, trimmed args — `Some("")` for the bare slider
/// command — or `None` for a non-`/effort` record / one without an args tag.
pub fn slash_effort_args(content: &str) -> Option<String> {
    if !content.contains("<command-name>/effort</command-name>") {
        return None;
    }
    let start_tag = "<command-args>";
    let start = content.find(start_tag)? + start_tag.len();
    let end = content[start..].find("</command-args>")?;
    Some(content[start..start + end].trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_parses_every_measured_shape() {
        assert_eq!(
            parse_effort_version("2.1.289"),
            Some(EffortSemantics::Decoupled)
        );
        assert_eq!(
            parse_effort_version("2.1.289 (Claude Code)"),
            Some(EffortSemantics::Decoupled)
        );
        assert_eq!(
            parse_effort_version("2.1.284"),
            Some(EffortSemantics::Decoupled)
        );
        assert_eq!(
            parse_effort_version("2.1.283"),
            Some(EffortSemantics::Coupled)
        );
        assert_eq!(
            parse_effort_version("2.1.277"),
            Some(EffortSemantics::Coupled)
        );
        assert_eq!(
            parse_effort_version("2.1.203"),
            Some(EffortSemantics::Coupled)
        );
        assert_eq!(parse_effort_version("2"), None);
        assert_eq!(parse_effort_version(""), None);
        assert_eq!(parse_effort_version("fixture"), None);
    }

    #[test]
    fn resumed_history_sets_no_state_and_cannot_settle_a_fresh_switch() {
        // D-056 (4): a resume launch replays the prior session's records. Its
        // ultracode-on verdict must leave the current state off/unset, and its
        // slash must not arm an await; only the new process's own verdict
        // settles a fresh switch.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.begin_history();
        assert!(!tracker.is_current_process());

        // The history says "Ultracode on" at high …
        assert!(!tracker.note_slash("ultracode", true));
        assert!(tracker
            .note_stdout(
                "Ultracode on (this session only): dynamic workflows on every task. Effort stays \
                 high.",
                true,
            )
            .is_none());
        // … and the next assistant record says high — still nothing.
        assert!(tracker.observe(Some("high"), None).is_none());
        assert!(tracker.note_ultra_attachment(true).is_none());
        // The state starts with no level and a positively unknown flag.
        assert_eq!(tracker.last_observed(), None);

        // First post-launch record flips the gate permanently.
        tracker.mark_current_process();
        assert!(tracker.is_current_process());
        tracker.mark_current_process();
        assert!(tracker.is_current_process());

        // A fresh `/effort high` arms only now, and its own verdict settles it.
        assert!(tracker.note_slash("high", true));
        let edge = tracker
            .note_stdout(
                "Set effort level to high (saved as your default for new sessions): Comprehensive \
                 implementation with extensive testing and documentation",
                true,
            )
            .expect("the fresh switch's own verdict is an edge");
        assert_eq!(edge.0.name, EffortName::High);
    }

    #[test]
    fn first_record_is_an_edge_then_unchanged_records_are_deduped() {
        let mut tracker = EffortTracker::new();
        tracker.mark_launch();
        let first = tracker.observe(Some("high"), None).expect("edge");
        assert_eq!(first.0.name, EffortName::High);
        assert_eq!(first.1, EffortSource::Launch);
        assert!(tracker.observe(Some("high"), None).is_none());
    }

    #[test]
    fn a_user_slash_marks_the_next_new_level() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        assert!(tracker.note_slash("xhigh", false));
        let edge = tracker.observe(Some("xhigh"), None).unwrap();
        assert_eq!(edge.1, EffortSource::Slash);
        assert_eq!(edge.0.ultracode, None);
    }

    #[test]
    fn coupled_ultracode_slash_reads_back_as_xhigh_with_the_flag() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker.note_slash("ultracode", false);
        let edge = tracker.observe(Some("xhigh"), None).unwrap();
        assert_eq!(edge.0.name, EffortName::Xhigh);
        assert_eq!(edge.0.ultracode, Some(true));
        assert_eq!(edge.1, EffortSource::Slash);
    }

    #[test]
    fn remuda_awaiting_wins_attribution() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        tracker.arm_awaiting(EffortName::Max);
        assert_eq!(
            tracker.observe(Some("max"), None).unwrap().1,
            EffortSource::Remuda
        );
    }

    #[test]
    fn level_words_are_version_gated() {
        for word in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                EffortTracker::parse_level(word, EffortSemantics::Coupled),
                EffortTracker::parse_level(word, EffortSemantics::Decoupled),
                "{word} parses in both eras"
            );
        }
        assert_eq!(
            EffortTracker::parse_level("ultracode", EffortSemantics::Coupled),
            Some(EffortName::Xhigh)
        );
        assert_eq!(
            EffortTracker::parse_level("ultracode", EffortSemantics::Decoupled),
            None
        );
        assert_eq!(
            EffortTracker::parse_level("ultracode", EffortSemantics::Unknown),
            None
        );
        assert!(EffortTracker::parse_level("auto", EffortSemantics::Decoupled).is_none());
        assert!(EffortTracker::parse_level("bogus", EffortSemantics::Coupled).is_none());
        assert_eq!(
            EffortTracker::parse_level("XHigh", EffortSemantics::Decoupled),
            Some(EffortName::Xhigh)
        );
    }

    #[test]
    fn slash_classification_covers_every_decoupled_form() {
        use EffortSlash::*;
        let d = EffortSemantics::Decoupled;
        assert!(matches!(
            classify_effort_slash("high", d),
            Level(EffortName::High)
        ));
        assert!(matches!(classify_effort_slash("", d), Slider));
        assert!(matches!(
            classify_effort_slash("ultracode", d),
            UltracodeOn { bare: true }
        ));
        assert!(matches!(
            classify_effort_slash("ultracode on", d),
            UltracodeOn { bare: false }
        ));
        assert!(matches!(
            classify_effort_slash("ultracode off", d),
            UltracodeOff
        ));
        // Both invalid-argument orders from effort-sync-4 walk 9 and 10.
        assert!(matches!(classify_effort_slash("bogus", d), NotASwitch));
        assert!(matches!(
            classify_effort_slash("ultracode bogus", d),
            NotASwitch
        ));
        for observation_only in ["auto", "status", "current"] {
            assert!(matches!(
                classify_effort_slash(observation_only, d),
                NotASwitch
            ));
        }
        // The bare word is the xhigh level command on coupled builds.
        assert!(matches!(
            classify_effort_slash("ultracode", EffortSemantics::Coupled),
            UltracodeOn { bare: true }
        ));
    }

    #[test]
    fn slash_args_shape() {
        let content = "<command-name>/effort</command-name>\n<command-args>max</command-args>";
        assert_eq!(slash_effort_args(content).as_deref(), Some("max"));
        // The bare slider command is Some(""), not None.
        let bare = "<command-name>/effort</command-name>\n<command-args></command-args>";
        assert_eq!(slash_effort_args(bare).as_deref(), Some(""));
        assert!(slash_effort_args("<command-name>/clear</command-name>").is_none());
    }

    #[test]
    fn coupled_verdict_strings_still_parse() {
        // Measured verbatim on 2.1.272.
        match parse_effort_stdout(
            "Set effort level to xhigh (saved as your default for new sessions): Deeper …",
            EffortSemantics::Coupled,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::Xhigh);
                assert_eq!(o.ultracode, Some(false));
            }
            other => panic!("{other:?}"),
        }
        match parse_effort_stdout(
            "Set effort level to ultracode (this session only): xhigh + dynamic workflow \
             orchestration",
            EffortSemantics::Coupled,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::Xhigh);
                assert_eq!(o.ultracode, Some(true));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            parse_effort_stdout("Kept effort level as xhigh", EffortSemantics::Coupled),
            EffortStdout::Kept
        );
        assert_eq!(
            parse_effort_stdout(
                "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, \
                 ultracode, auto",
                EffortSemantics::Coupled
            ),
            EffortStdout::Invalid
        );
        assert_eq!(
            parse_effort_stdout("Effort level set to auto", EffortSemantics::Coupled),
            EffortStdout::Other
        );
        // Older build spelling.
        match parse_effort_stdout(
            "Set effort level: high (saved as your default for new sessions)",
            EffortSemantics::Coupled,
        ) {
            EffortStdout::Accepted(o) => assert_eq!(o.name, EffortName::High),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decoupled_verdict_strings_parse_verbatim() {
        let d = EffortSemantics::Decoupled;
        // Walk 1: toggle on keeps the current level.
        match parse_effort_stdout(
            "Ultracode on (this session only): dynamic workflows on every task. Effort stays high.",
            d,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::High);
                assert_eq!(o.ultracode, Some(true));
            }
            other => panic!("{other:?}"),
        }
        // Walk 4: toggle off at max.
        match parse_effort_stdout("Ultracode off. Effort stays max.", d) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::Max);
                assert_eq!(o.ultracode, Some(false));
            }
            other => panic!("{other:?}"),
        }
        // Walk 5: back on at max.
        match parse_effort_stdout(
            "Ultracode on (this session only): dynamic workflows on every task. Effort stays max.",
            d,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::Max);
                assert_eq!(o.ultracode, Some(true));
            }
            other => panic!("{other:?}"),
        }
        // Plain level accept does not state the flag: latch preserved.
        match parse_effort_stdout(
            "Set effort level to high (saved as your default for new sessions): Comprehensive \
             implementation with extensive testing and documentation",
            d,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::High);
                assert_eq!(o.ultracode, None);
            }
            other => panic!("{other:?}"),
        }
        // Slider verdict applies level and toggle together, joined by ' · '.
        match parse_effort_stdout(
            "Set effort level to high (saved as your default for new sessions): Comprehensive \
             implementation with extensive testing and documentation · Ultracode off",
            d,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::High);
                assert_eq!(o.ultracode, Some(false));
            }
            other => panic!("{other:?}"),
        }
        // Cap clamp: accepted at the clamped level, flag not stated.
        match parse_effort_stdout(
            "Effort 'max' exceeds the cap for claude-opus-5-5 set by your settings or \
             organization; set to 'high' instead (this session only): Comprehensive \
             implementation with extensive testing and documentation",
            d,
        ) {
            EffortStdout::Accepted(o) => {
                assert_eq!(o.name, EffortName::High);
                assert_eq!(o.ultracode, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decoupled_refusals_carry_stable_reasons() {
        let d = EffortSemantics::Decoupled;
        let cases = [
            (
                "Ultracode needs dynamic workflows enabled (see /config). Valid options are: low, \
                 medium, high, xhigh, max, auto",
                "ultracode-workflows-disabled",
                EffortStdout::WorkflowsDisabled,
            ),
            (
                "Ultracode isn't available on claude-sonnet-4-6. Valid options are: low, medium, \
                 high, xhigh, max, auto",
                "ultracode-unavailable-for-model",
                EffortStdout::UnavailableForModel,
            ),
        ];
        for (stdout, reason, expected) in cases {
            let verdict = parse_effort_stdout(stdout, d);
            assert_eq!(verdict, expected);
            assert_eq!(verdict.reject_reason(), Some(reason));
        }
        // Measured on 2.1.277 (D-056 open question 3 for 2.1.289).
        let env = parse_effort_stdout(
            "Not applied: CLAUDE_CODE_EFFORT_LEVEL=high overrides effort this session, and max is \
             session-only (nothing saved)",
            EffortSemantics::Coupled,
        );
        assert_eq!(env, EffortStdout::EnvOverride);
        assert_eq!(env.reject_reason(), Some("env-override"));
        let env2 = parse_effort_stdout(
            "CLAUDE_CODE_EFFORT_LEVEL=high overrides this session — clear it and xhigh takes over",
            EffortSemantics::Coupled,
        );
        assert_eq!(env2, EffortStdout::EnvOverride);
    }

    #[test]
    fn invalid_argument_in_either_list_order_is_invalid() {
        let d = EffortSemantics::Decoupled;
        assert_eq!(
            parse_effort_stdout(
                "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, auto, \
                 ultracode [on|off]",
                d
            ),
            EffortStdout::Invalid
        );
        assert_eq!(
            parse_effort_stdout(
                "Invalid argument: ultracode bogus. Valid options are: low, medium, high, xhigh, \
                 max, auto, ultracode [on|off]",
                d
            ),
            EffortStdout::Invalid
        );
    }

    #[test]
    fn status_is_an_observation_with_the_resolved_level_and_flag() {
        let d = EffortSemantics::Decoupled;
        match parse_effort_stdout(
            "Current effort level: high (Comprehensive implementation with extensive testing and \
             documentation) · Ultracode on",
            d,
        ) {
            EffortStdout::Status(o) => {
                assert_eq!(o.name, EffortName::High);
                assert_eq!(o.ultracode, Some(true));
            }
            other => panic!("{other:?}"),
        }
        match parse_effort_stdout("Effort level: auto (currently medium) · Ultracode on", d) {
            EffortStdout::Status(o) => {
                assert_eq!(o.name, EffortName::Medium);
                assert_eq!(o.ultracode, Some(true));
            }
            other => panic!("{other:?}"),
        }
        // Absent suffix is positive off-evidence.
        match parse_effort_stdout(
            "Current effort level: medium (Balanced approach with standard implementation and \
             testing)",
            d,
        ) {
            EffortStdout::Status(o) => assert_eq!(o.ultracode, Some(false)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decoupled_flag_latches_across_levels_and_assistant_records() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        // Baseline high, flag unknown.
        assert_eq!(
            tracker.observe(Some("high"), None).unwrap().0,
            ObservedEffort {
                name: EffortName::High,
                ultracode: None
            }
        );
        // Ultracode on, level stays high.
        let edge = tracker
            .note_stdout(
                "Ultracode on (this session only): dynamic workflows on every task. Effort stays \
                 high.",
                false,
            )
            .expect("flag edge");
        assert_eq!(
            edge.0,
            ObservedEffort {
                name: EffortName::High,
                ultracode: Some(true)
            }
        );
        // A plain high verdict does not clear the flag and emits no edge.
        assert!(
            tracker
                .note_stdout(
                    "Set effort level to high (saved as your default for new sessions): …",
                    false
                )
                .is_none()
        );
        // Moving to max carries the flag, both from the verdict and the
        // following assistant record (which never clears it).
        let to_max = tracker
            .note_stdout("Set effort level to max (this session only): …", false)
            .expect("max edge");
        assert_eq!(to_max.0.ultracode, Some(true));
        assert!(tracker.observe(Some("max"), None).is_none());
        // The flag survives xhigh and auto-resolved-to-medium as well.
        tracker
            .note_stdout(
                "Set effort level to xhigh (saved as your default …): …",
                false,
            )
            .unwrap();
        assert!(tracker.observe(Some("xhigh"), None).is_none());
        tracker.note_stdout("Effort level set to auto", false);
        let medium = tracker
            .observe(Some("medium"), None)
            .expect("auto resolves to medium");
        assert_eq!(medium.0.ultracode, Some(true));
    }

    #[test]
    fn sparse_enter_repeat_is_not_an_edge() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("medium"), None);
        // Verdict turned the flag on; the full attachment that follows agrees.
        tracker
            .note_stdout(
                "Ultracode on (this session only): … Effort stays medium.",
                false,
            )
            .unwrap();
        assert!(tracker.note_ultra_attachment(true).is_none());
        // A sparse re-reminder several turns later changes nothing.
        assert!(tracker.note_ultra_attachment(true).is_none());
        assert!(tracker.observe(Some("medium"), None).is_none());
        // Exit is an edge.
        let off = tracker.note_ultra_attachment(false).expect("exit edge");
        assert_eq!(off.0.ultracode, Some(false));
    }

    #[test]
    fn attachment_before_any_level_latches_and_rides_the_first_level() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.mark_launch();
        // Launch-a: the enter attachment lands with the first prompt, before
        // any assistant record.
        assert!(tracker.note_ultra_attachment(true).is_none());
        let first = tracker.observe(Some("high"), None).expect("first level");
        assert_eq!(first.0.name, EffortName::High);
        assert_eq!(first.0.ultracode, Some(true));
        assert_eq!(first.1, EffortSource::Launch);
    }

    #[test]
    fn exit_attachment_at_max_sets_the_flag_off_at_max() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.note_slash("ultracode on", false);
        tracker
            .note_stdout(
                "Ultracode on (this session only): … Effort stays max.",
                false,
            )
            .unwrap();
        tracker.observe(Some("max"), None);
        // Decoupled walk 4: exit arrives with the level unchanged at max.
        let off = tracker.note_ultra_attachment(false).expect("edge");
        assert_eq!(off.0.name, EffortName::Max);
        assert_eq!(off.0.ultracode, Some(false));
    }

    #[test]
    fn status_reports_but_never_settles_and_prefers_the_assistant_level() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        // launch-g: status claims xhigh while the assistant record says high.
        let status = parse_effort_stdout(
            "Current effort level: xhigh (Deeper reasoning than high, just below maximum)",
            EffortSemantics::Decoupled,
        );
        let EffortStdout::Status(parsed) = status else {
            panic!("{status:?}")
        };
        let edge = tracker.note_status(parsed).expect("flag edge");
        assert_eq!(edge.0.name, EffortName::High, "level stays the assistant's");
        assert_eq!(edge.0.ultracode, Some(false));
        assert_eq!(edge.1, EffortSource::Unknown);
        // The same status again emits nothing.
        assert!(tracker.note_status(parsed).is_none());
        // It must not settle an awaiting switch.
        tracker.arm_awaiting(EffortName::Max);
        tracker.note_status(parsed);
        assert_eq!(tracker.awaiting, Some(EffortName::Max));
    }

    #[test]
    fn unknown_semantics_plain_verdict_never_changes_the_flag() {
        let mut tracker = EffortTracker::new();
        tracker.observe(Some("high"), None);
        // Positive evidence the flag was on (attachment), then a plain level
        // verdict on an unknown-version transcript must not clear it.
        tracker.note_ultra_attachment(true);
        assert!(
            tracker
                .note_stdout(
                    "Set effort level to high (saved as your default for new sessions): …",
                    false
                )
                .is_none()
        );
        assert_eq!(
            tracker.observe(Some("high"), None),
            None,
            "flag preserved without version evidence"
        );
    }

    #[test]
    fn a_clamped_verdict_settles_the_armed_level_and_keeps_remuda_source() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("medium"), None);
        // Remuda armed /effort max; the org cap clamps the accept to high.
        tracker.note_slash("max", true);
        assert_eq!(tracker.awaiting, Some(EffortName::Max));
        let edge = tracker
            .note_stdout(
                "Effort 'max' exceeds the cap for claude-opus-5-5 set by your settings or \
                 organization; set to 'high' instead (this session only): Comprehensive \
                 implementation with extensive testing and documentation",
                true,
            )
            .expect("clamp edge");
        assert_eq!(
            edge.0,
            ObservedEffort {
                name: EffortName::High,
                ultracode: None,
            }
        );
        assert_eq!(edge.1, EffortSource::Remuda);
        // The await is settled: a later natural max edge is not Remuda's.
        assert_eq!(tracker.awaiting, None);
        tracker.note_slash("max", false);
        let later = tracker
            .note_stdout("Set effort level to max (this session only): …", false)
            .unwrap();
        assert_eq!(later.1, EffortSource::Slash);
    }

    #[test]
    fn stdout_accept_is_an_immediate_edge_without_an_assistant_record() {
        // This is the core read-back fix: the level is settled by the verdict
        // line, not by the next turn's assistant record.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("low"), None);
        tracker.note_slash("xhigh", true);
        let edge = tracker
            .note_stdout(
                "Set effort level to xhigh (saved as your default for new sessions): …",
                true,
            )
            .expect("edge");
        assert_eq!(edge.0.name, EffortName::Xhigh);
        assert_eq!(edge.1, EffortSource::Remuda);
        // A same-level assistant record afterwards is deduped, carrying no flag.
        assert!(tracker.observe(Some("xhigh"), None).is_none());
    }

    #[test]
    fn dismissed_dialog_and_bad_arg_do_not_change_the_level() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("xhigh"), None);
        // Esc on the dialog: slash record armed, verdict says kept.
        tracker.note_slash("max", true);
        assert!(
            tracker
                .note_stdout("Kept effort level as xhigh", true)
                .is_none()
        );
        // The refused awaiting must not credit a later natural max edge.
        assert!(tracker.observe(Some("xhigh"), None).is_none());
        // Invalid argument: same story.
        tracker.note_slash("bogus", false);
        assert!(
            tracker
                .note_stdout(
                    "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max",
                    false
                )
                .is_none()
        );
    }

    #[test]
    fn coupled_ultracode_flag_is_sticky_across_later_xhigh_records_until_exit() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker
            .note_stdout(
                "Set effort level to ultracode (this session only): xhigh + dynamic …",
                false,
            )
            .expect("edge to ultracode");
        // The attachment that rides the next prompt is consistent (no edge).
        assert!(tracker.note_ultra_attachment(true).is_none());
        // Many xhigh assistant records later still carry the flag.
        let again = tracker.observe(Some("xhigh"), None);
        assert!(again.is_none(), "deduped, flag latched: {again:?}");
        // Switch back to high: stdout positively turns the flag off.
        let back = tracker
            .note_stdout(
                "Set effort level to high (saved as your default for new sessions): …",
                false,
            )
            .expect("edge back to high");
        assert_eq!(back.0.name, EffortName::High);
        assert_eq!(back.0.ultracode, Some(false));
    }

    #[test]
    fn a_hand_typed_switch_attributes_slash_and_settles_from_stdout() {
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker.note_slash("low", false);
        let edge = tracker
            .note_stdout("Set effort level to low (saved as your default …)", false)
            .expect("edge");
        assert_eq!(edge.0.name, EffortName::Low);
        assert_eq!(edge.1, EffortSource::Slash);
    }

    #[test]
    fn effort_event_id_is_stable_across_channels_and_namespaced_per_instance() {
        // The live hydrator and the file tailer both observe the same native
        // assistant record; they must draw one event id (live-view §2.3).
        let a = effort_event_id("ins_one", "msg_42", EffortName::Xhigh);
        let b = effort_event_id("ins_one", "msg_42", EffortName::Xhigh);
        assert_eq!(a, b);
        // A different record or level is a different edge.
        assert_ne!(a, effort_event_id("ins_one", "msg_43", EffortName::Xhigh));
        assert_ne!(a, effort_event_id("ins_one", "msg_42", EffortName::High));
        // Ids never collide across sessions.
        assert_ne!(a, effort_event_id("ins_two", "msg_42", EffortName::Xhigh));
        assert!(a.as_id().as_str().starts_with("evt_"));
        // Verdict/attachment records use the shared labelled form.
        let stdout =
            effort_record_event_id("ins_one", EFFORT_STDOUT_NATIVE, "cmd-1", EffortName::High);
        assert_eq!(
            stdout,
            effort_record_event_id("ins_one", EFFORT_STDOUT_NATIVE, "cmd-1", EffortName::High)
        );
        assert_ne!(
            stdout,
            effort_record_event_id("ins_one", EFFORT_STATUS_NATIVE, "cmd-1", EffortName::High)
        );
        assert!(stdout.as_id().as_str().starts_with("evt_"));
    }
}
