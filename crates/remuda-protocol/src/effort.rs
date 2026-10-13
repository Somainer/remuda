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

/// `NativeLifecycle.related_ids` key carrying the per-request id of an effort
/// `instance.configure` (c-effortread r10, items 1–4).
///
/// The web mints one id per switch; it rides web → Hub → Node → driver on the
/// configure, and every `effort-queued` / `effort-applied` /
/// `effort-degraded` lifecycle the switch worker journals echoes it under this
/// key. A client settles, rolls back or clears a pending switch ONLY on an
/// edge carrying ITS id (or, on the id-less read-back fallback, on evidence
/// newer than both the pre-request baseline and the queued time). A timeout,
/// cancellation or refusal of request A therefore can never touch request B.
pub const EFFORT_REQUEST_ID_KEY: &str = "effortRequestId";

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
/// input yields `None`.
///
/// The version map (D-056): `≥ 2.1.284` is [`EffortSemantics::Decoupled`],
/// `2.1.203–2.1.283` is [`EffortSemantics::Coupled`], and anything BELOW
/// `2.1.203` reads as [`EffortSemantics::Unknown`] — those builds have no
/// ultracode at all, so like an unparseable version they refuse the flag
/// rather than being treated as coupled. [`EffortSemantics::Unknown`] is
/// returned rather than `None` so a record carrying a real but-too-old version
/// still seeds the gate; `None` is reserved for a string that is not a
/// version.
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
    } else if (major, minor, patch) >= (2, 1, 203) {
        EffortSemantics::Coupled
    } else {
        EffortSemantics::Unknown
    })
}

/// Whether a build with `semantics` accepts the ultracode workflow flag at
/// all (D-056). Only coupled (2.1.203–2.1.283) and decoupled (≥ 2.1.284)
/// builds do; a pre-2.1.203 build, or a version that could not be determined,
/// must refuse `ultracode: true` — fail closed, never guess.
#[must_use]
pub fn ultracode_supported(semantics: EffortSemantics) -> bool {
    matches!(
        semantics,
        EffortSemantics::Coupled | EffortSemantics::Decoupled
    )
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

/// Which axis a live `instance.configure` effort switch intends to move.
///
/// c-effortread r9 item 6: without this the pair `{name, ultracode}` is
/// ambiguous — a cold flag-on click `{high, true}` cannot be told apart from
/// a tier move. The web now states the intent explicitly so the driver
/// executes exactly that operation instead of substituting a different one:
///
/// - [`EffortAxis::Flag`] — the orthogonal ultracode switch only (`/effort
///   ultracode on|off` on decoupled builds; the tier is unchanged).
/// - [`EffortAxis::Tier`] — a level move only (`/effort <level>`).
///
/// `None` is a legacy client that omits the axis; the driver then
/// disambiguates from provenance and REJECTS a cold flag-on it cannot prove
/// rather than guessing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum EffortAxis {
    /// Move the ultracode workflow flag, leaving the tier where it is.
    Flag,
    /// Move the effort tier.
    Tier,
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

/// c-effortread r19 item 2 ruling B: the outcome of feeding an UNOWNED
/// accepted verdict while an owned switch is armed. The verdict's effective
/// state is ALWAYS latched into the single deferred slot (including when it
/// equals the last published state — a return to baseline);
/// `changes_published` says whether it would have produced an edge, which the
/// mapper only uses to know what the deferred publication will emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnownedAccept {
    /// The effective state the unowned acceptance latched.
    pub observed: ObservedEffort,
    /// Whether `observed` differed from the last PUBLISHED state at the time
    /// of the acceptance (the edge the mapper suppressed).
    pub changes_published: bool,
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

/// Which channel published the level in [`EffortTracker::last`]
/// (c-effortread r22 ruling S). It describes the source of the PUBLISHED
/// level and changes ONLY when a level is actually published, set to the
/// channel that published it.
///
/// A `/effort status|current` read-back is authoritative for the FLAG but
/// only REPORTS the level: a status's resolved level (e.g. `Effort level:
/// auto (currently …)`) replaces a level whose source is [`Status`] or
/// [`Verdict`], but yields to a level published by an assistant/model
/// read-back ([`Assistant`], the model-clamping precedence). A suppressed
/// (unpublished) acceptance never changes the source — not even when the
/// acceptance latched a different level into the deferred slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LevelSource {
    /// No level published yet.
    #[default]
    None,
    /// The published level came from a `/effort status|current` read-back.
    /// Another status read-back may replace it.
    Status,
    /// The published level came from an accepted `/effort` verdict —
    /// including a suppressed acceptance when `publish_deferred` releases it.
    /// Verdicts carry NO clamping precedence: a later status read-back may
    /// replace the level.
    Verdict,
    /// The published level came from an assistant record or a `/model`
    /// verdict's effort clause. Only this source outranks a later status
    /// read-back (the launch-case status vs assistant-record disagreement).
    Assistant,
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
    /// c-effortread r22 ruling S: which channel published the level in
    /// `last`; changes ONLY when a level is published. Decides whether a
    /// status read-back may replace that level.
    level_source: LevelSource,
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
    /// c-effortread r18 item 3 / r19 item 2 ruling B: the effective state the
    /// LATEST unowned accepted verdict latched while an armed Remuda switch
    /// was still outstanding — ONE slot, not a queue. It is kept SEPARATE
    /// from [`Self::last`] (the last PUBLISHED state): the mapper suppresses
    /// that verdict's edge, but the transcript really changed and the value
    /// must not be swallowed by edge dedup. Every suppressed acceptance
    /// overwrites the slot, including one equal to the published baseline (a
    /// return to baseline). The slot is published once the armed switch ends
    /// (a refusal/timeout, drained by the mapper) ONLY when it still differs
    /// from the last published state, or cleared by the next authoritative
    /// effective-state publication (owned accept, assistant read-back, the
    /// `/model` effort clause, the ultra attachment, or `/effort status`).
    /// Its source is terminal-side.
    deferred: Option<ObservedEffort>,
}

impl Default for EffortTracker {
    fn default() -> Self {
        Self {
            semantics: EffortSemantics::Unknown,
            last: None,
            level_source: LevelSource::None,
            pending_source: EffortSource::Unknown,
            ultracode_pending: false,
            flag: None,
            awaiting: None,
            current_process: true,
            deferred: None,
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

    /// c-effortread r22 ruling S: the channel that published the level in
    /// [`Self::last_observed`]. Carried across a demote/re-promotion so a
    /// level that was status-only stays movable by a later status.
    #[must_use]
    pub fn level_source(&self) -> LevelSource {
        self.level_source
    }

    /// Mark effective effort read-back as unavailable for the rest of the
    /// process (D-056 (4): a resume boundary that was verified became
    /// unverifiable).
    ///
    /// The ONLY triggers are the tracked transcript shrinking below the read
    /// cursor, being replaced (different inode/head), or vanishing (one
    /// ENOENT/EOF) (c-effortread r10 item 10). A session-local `exec` that
    /// keeps the shell pid is NOT one: it preserves the verified boundary, as
    /// is a backward clock step — a session that merely lacked process-start
    /// evidence (or ran under such an exec) was simply Unverified from the
    /// outset, so no withdrawal edge is emitted.
    ///
    /// Clears the projected effective level/flag so a previously published
    /// `{high, ultracode on}` no longer masquerades as "effective" (the UI
    /// falls back to `?`, per D-028a). It does NOT resolve any awaiting
    /// switch: with read-back gone no switch may report Applied, so the
    /// pending generation keeps waiting for its bounded timeout and degrades
    /// rather than claiming success. Returns the cleared observation (if
    /// there was a projected state) so the caller can publish one explicit
    /// read-back-unavailable edge.
    ///
    /// c-effortread r22 ruling S (item 3): this is a full reset EVEN when
    /// nothing was published yet — a suppressed acceptance, an armed switch
    /// or an ultracode pre-latch may hold latches while `last` is still
    /// `None`, and provenance can be re-established afterwards. Clear
    /// LevelSource together with the flag latch, the deferred slot, the
    /// awaiting switch and the ultracode pre-latch on BOTH paths, so the
    /// first status after recovery starts clean (publishes) and a second
    /// status can still move the level.
    pub fn read_back_unavailable(&mut self) -> Option<ObservedEffort> {
        let cleared = self.last.take();
        // Drop every process-specific latch: no record carried over the
        // unverifiable window is trusted to re-set them, no pending switch
        // resolves Applied, and a suppressed acceptance must never resurrect.
        self.level_source = LevelSource::None;
        self.flag = None;
        self.awaiting = None;
        self.ultracode_pending = false;
        self.deferred = None;
        self.current_process = false;
        cleared
    }

    /// c-effortread r18 item 3: the latest effective state latched by a
    /// suppressed unowned acceptance but not yet published. The mapper stashes
    /// the verdict alongside; this lets it drop the stash once another
    /// channel published/superseded the deferred state.
    #[must_use]
    pub fn deferred_observation(&self) -> Option<ObservedEffort> {
        self.deferred
    }

    /// c-effortread r19 item 2 ruling B: drop the deferred slot. Every
    /// AUTHORITATIVE effective-state publication (an owned accept, an
    /// assistant read-back, the effort clause of a `/model` verdict, an
    /// `ultra_effort_enter|exit` attachment) clears the slot; the mapper
    /// pairs this with dropping its own one-edge stash through its single
    /// shared invalidation function so a suppressed acceptance can never be
    /// published after it was superseded.
    pub fn clear_deferred(&mut self) {
        self.deferred = None;
    }

    /// Learn the semantics from one transcript record's `version` field. A
    /// later record cannot move a known gate back to unknown: a pre-2.1.203
    /// version now parses to [`EffortSemantics::Unknown`], and adopting it
    /// would wrongly reopen a gate an in-process record already settled. All
    /// records of a process carry one version, so the order they are fed in
    /// otherwise does not matter.
    pub fn note_version(&mut self, version: &str) {
        if let Some(found) = parse_effort_version(version)
            && found != EffortSemantics::Unknown
        {
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

    /// c-effortread r8 item 5: restore the last published observation when a
    /// demote/re-promotion builds a fresh tracker for the SAME (pid, session).
    /// The seeded tracker re-latches level/flag/source/semantics so an
    /// attachment-less assistant record after re-promotion keeps reporting the
    /// known flag (e.g. `{high, ultracode: Some(true)}`) instead of dropping
    /// it to `None`. Current-process is already true (this is a live
    /// continuation, not a resume replay).
    ///
    /// c-effortread r22 ruling S (item 2): `level_source` is the channel the
    /// SEEDED tracker had when it published the level — the seed is a
    /// continuation, not new evidence, so a status-only level stays Status and
    /// a later status read-back may still move it.
    pub fn seed_observed(
        &mut self,
        name: EffortName,
        ultracode: Option<bool>,
        source: EffortSource,
        semantics: EffortSemantics,
        level_source: LevelSource,
    ) {
        self.semantics = semantics;
        self.last = Some(ObservedEffort { name, ultracode });
        self.level_source = level_source;
        // `source` is mapper-owned bookkeeping; preserve the seeded source so
        // the next observation is attributed consistently.
        self.pending_source = source;
        self.flag = ultracode;
        self.ultracode_pending = false;
        self.awaiting = None;
        self.deferred = None;
        self.current_process = true;
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

    /// c-effortread r17 item 1 / r18 item 3 / r19 item 2: fold a verdict that
    /// answers an UNOWNED slash slot (an older pre-floor same-word command, or
    /// a human `/effort`) while a LATER armed Remuda switch is still waiting
    /// for ITS OWN verdict. An accepted verdict latches its level/flag into
    /// the single DEFERRED slot, NOT `last` (the last published state): the
    /// mapper suppresses the edge now and publishes it via
    /// [`Self::publish_deferred`] once the armed switch ends, or lets the next
    /// authoritative same-value read-back emit it. The armed attribution
    /// (`awaiting`, `pending_source`, `ultracode_pending`) is left completely
    /// untouched: the verdict is never relabelled Remuda and a refusal of the
    /// unowned command can never disarm the owned switch.
    ///
    /// Returns the latched outcome for EVERY accepted verdict, including one
    /// equal to the last published state (a return to baseline): r19 item 2
    /// ruling B makes the stash ONE latest slot and every suppressed
    /// acceptance overwrites it, so the mapper must stash even when
    /// `changes_published` is false. A refusal/status outcome returns `None`
    /// when it emits nothing.
    pub fn note_unowned_stdout(&mut self, stdout: &str) -> Option<UnownedAccept> {
        if !self.current_process {
            return None;
        }
        match parse_effort_stdout(stdout, self.semantics) {
            EffortStdout::Accepted(observed) => Some(self.apply_unowned_accept(observed)),
            // Status never settles an awaiting switch; fold it as always.
            EffortStdout::Status(observed) => {
                self.note_status(observed)
                    .map(|(observed, _)| UnownedAccept {
                        observed,
                        changes_published: true,
                    })
            }
            // A refusal/dismissal concerns the unowned command only: the owned
            // switch's armed attribution is preserved verbatim.
            EffortStdout::Kept
            | EffortStdout::Invalid
            | EffortStdout::WorkflowsDisabled
            | EffortStdout::UnavailableForModel
            | EffortStdout::EnvOverride
            | EffortStdout::Other => None,
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
        // B's own verdict is the authority: any state an unowned verdict
        // latched while B was armed is superseded/published by THIS verdict
        // (c-effortread r18 item 3) and must not be re-emitted afterwards.
        let observed = ObservedEffort {
            name: parsed.name,
            ultracode: flag,
        };
        // c-effortread r20 ruling B′: the owned accept flows through the one
        // shared authoritative reconcile (latch + clear deferred + dedupe
        // against the last PUBLISHED tuple).
        // c-effortread r22 ruling S: the accepted verdict PUBLISHES its
        // level, so the source is Verdict. A verdict carries no clamping
        // precedence — a later status read-back may replace this level; only
        // an assistant/model read-back outranks a status.
        self.level_source = LevelSource::Verdict;
        let edge = self.reconcile_authoritative(observed, source);
        if edge.is_some() {
            self.pending_source = EffortSource::Unknown;
        }
        edge
    }

    /// Latch the transcript effect of an unowned command's accepted verdict
    /// (c-effortread r17 item 1) WITHOUT touching the later owned switch's
    /// armed attribution (`awaiting`/`pending_source`/`ultracode_pending`).
    /// The edge is always terminal-side ([`EffortSource::Slash`]) — an owned
    /// switch waiting for its own verdict is never credited (or disarmed) by a
    /// verdict that answers a different slash.
    ///
    /// c-effortread r18 item 3 / r19 item 2 ruling B: the latched state is
    /// held in the single `deferred` SLOT, NOT in `last`. The mapper
    /// suppresses that verdict's edge entirely, so writing `last` would make
    /// the next same-value authoritative read-back look like an unchanged
    /// dedup and the effective state would never be published at all (a
    /// suppressed pre-floor max acceptance followed by B's Kept and max
    /// assistant records). EVERY suppressed acceptance overwrites the slot,
    /// including one equal to the published baseline (a return to baseline),
    /// so the outcome is returned with `changes_published` rather than as an
    /// `Option`: the deferred state is published by [`Self::publish_deferred`]
    /// once the armed switch ends, or cleared by the next authoritative
    /// read-back/publication.
    fn apply_unowned_accept(&mut self, parsed: ObservedEffort) -> UnownedAccept {
        let flag = match parsed.ultracode {
            Some(flag) => Some(flag),
            None if self.semantics == EffortSemantics::Coupled => Some(false),
            None => self.flag,
        };
        if let Some(flag) = flag {
            self.flag = Some(flag);
        }
        let observed = ObservedEffort {
            name: parsed.name,
            ultracode: flag,
        };
        let changes_published = self.last != Some(observed);
        // c-effortread r22 ruling S: this acceptance is SUPPRESSED
        // (unpublished): latch the flag and stash the deferred state, but do
        // NOT touch level_source. The channel of the published level stays
        // exactly what it was — a status-only high is not protected by the
        // suppressed max, so a later status may still move it. The source
        // becomes Verdict only if/when publish_deferred releases the slot.
        self.deferred = Some(observed);
        UnownedAccept {
            observed,
            changes_published,
        }
    }

    /// c-effortread r18 item 3 / r19 item 2 ruling B: publish the ONE stashed
    /// suppressed acceptance once the armed switch has ended (its
    /// refusal/timeout). Latches `observed` as the last published state
    /// exactly like a normally emitted verdict edge — INCLUDING its ultracode
    /// value, so the tracker flag agrees with what is published — and returns
    /// the terminal-side edge only when it differs from the last published
    /// state. The slot is therefore published AT MOST ONCE: a stashed
    /// return-to-baseline (or a state already published by an intervening
    /// authoritative read-back) emits nothing.
    pub fn publish_deferred(
        &mut self,
        observed: ObservedEffort,
    ) -> Option<(ObservedEffort, EffortSource)> {
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        // The released stashed acceptance was a real /effort verdict: its
        // level is published now, so the source is Verdict (c-effortread r22
        // ruling S) — a later status read-back may replace it.
        self.level_source = LevelSource::Verdict;
        // Keep the latched flag consistent with the published value even when
        // the slot was overwritten after the flag was last latched.
        if let Some(flag) = observed.ultracode {
            self.flag = Some(flag);
        }
        self.deferred = None;
        edge.then_some((observed, EffortSource::Slash))
    }

    /// c-effortread r20 ruling B′ (completes r19 ruling B): the ONE reconcile
    /// every AUTHORITATIVE effective-state evidence flows through — an owned
    /// accept, an assistant read-back, the `/model` effort clause, an
    /// `ultra_effort_enter|exit` attachment, and `/effort status` — whether or
    /// not it produces a published edge. It latches the evidenced tuple,
    /// clears the deferred suppressed-acceptance slot (the mapper pairs this
    /// with dropping its own stashed edge), and returns an edge IFF the
    /// resulting tuple differs from the last PUBLISHED tuple. Dedupe is
    /// against that published tuple — never the internal flag/latch: an
    /// unpublished suppressed acceptance may have moved the latch.
    fn reconcile_authoritative(
        &mut self,
        observed: ObservedEffort,
        source: EffortSource,
    ) -> Option<(ObservedEffort, EffortSource)> {
        let edge = self.last != Some(observed);
        self.last = Some(observed);
        self.deferred = None;
        edge.then_some((observed, source))
    }

    /// Feed a `/effort status|current` observation. Status never settles an
    /// awaiting switch and is never attributed to one ([`EffortSource::Unknown`]
    /// ): it reports the current state rather than changing it.
    ///
    /// c-effortread r22 ruling S (supersedes r21 ruling B″(2)): status is
    /// authoritative for the FLAG but only REPORTS the level. The reported
    /// level replaces a level whose source is Status OR Verdict (a verdict
    /// carries no clamping precedence); it yields ONLY to a level published
    /// by an assistant/model read-back (`Assistant` — the launch case g,
    /// where the two can disagree and the assistant level wins). A suppressed
    /// acceptance never protects the published level: it leaves the source
    /// unchanged, so a status moves a status-only level right past it.
    ///
    /// c-effortread r20 ruling B′: status is an AUTHORITATIVE read-back like
    /// the other channels and goes through the one shared reconcile, whether
    /// or not it produces an edge. In particular a same-level status WITHOUT
    /// the ` · Ultracode on` suffix is positive flag-OFF evidence: even when
    /// its tuple equals the last published one it latches the flag off and
    /// clears BOTH deferred copies, so a suppressed `{max, on}` cannot be
    /// published by B's later refusal.
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
        let established_by_assistant = self.level_source == LevelSource::Assistant;
        let name = match self.last {
            Some(last) if established_by_assistant => last.name,
            _ => parsed.name,
        };
        let observed = ObservedEffort {
            name,
            ultracode: self.flag,
        };
        let edge = self.reconcile_authoritative(observed, EffortSource::Unknown);
        // When the status actually carried the published level its channel
        // is Status; while yielding to Assistant the source stays exactly
        // what it was (changes happen only on publication).
        if !established_by_assistant {
            self.level_source = LevelSource::Status;
        }
        edge
    }

    /// Feed the `ultra_effort_enter|exit` attachment riding the next prompt.
    /// Both `full` and `sparse` enters set the flag on; a sparse repeat while
    /// it is already on is therefore not an edge. When the attachment arrives
    /// before any level is known it only latches: the edge is emitted together
    /// with the first level.
    ///
    /// c-effortread r19 item 2 ruling B / r20 ruling B′: the attachment is an
    /// AUTHORITATIVE publication of the flag, so it clears the deferred
    /// suppressed-acceptance slot for the CURRENT process — even a sparse
    /// repeat that emits no edge — and the mapper drops its stashed edge in
    /// lockstep. The edge is decided against the last PUBLISHED tuple, never
    /// the internal flag latch: a suppressed `… · Ultracode on|off` acceptance
    /// may already have moved `flag` without publishing, so deduping on the
    /// latch would swallow `{level, flag}` permanently.
    pub fn note_ultra_attachment(
        &mut self,
        enters: bool,
    ) -> Option<(ObservedEffort, EffortSource)> {
        if !self.current_process {
            return None;
        }
        // Authoritative flag evidence: latch it unconditionally, then
        // reconcile against the published tuple via the shared path.
        self.flag = Some(enters);
        let Some(last) = self.last else {
            // Latch only; the first assistant/verdict level carries the flag.
            // The attachment is still authoritative evidence that supersedes
            // any suppressed acceptance.
            self.deferred = None;
            return None;
        };
        let observed = ObservedEffort {
            name: last.name,
            ultracode: Some(enters),
        };
        self.reconcile_authoritative(observed, EffortSource::Slash)
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
    ///
    /// c-effortread r19 item 2 ruling B: this is an AUTHORITATIVE effective-
    /// state publication, so it clears the deferred suppressed-acceptance
    /// slot unconditionally — including a same-value read-back that emits no
    /// edge (the stashed acceptance can then never publish after it).
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
        // c-effortread r22 ruling S: an assistant/model read-back publishes
        // the level with source Assistant — the ONLY channel a later status
        // read-back yields to (model-clamping precedence).
        self.level_source = LevelSource::Assistant;
        // c-effortread r20 ruling B′: one shared authoritative reconcile. The
        // suppressed slot is superseded even when this emits nothing (the
        // read-back equals the last published state).
        let edge = self.reconcile_authoritative(observed, source);
        if edge.is_some() {
            self.pending_source = EffortSource::Unknown;
            self.ultracode_pending = false;
        }
        edge
    }

    /// The flag an assistant record (or model-effort verdict) at `name`
    /// presents given the latch and the version gate.
    fn assistant_flag(&self, name: EffortName) -> Option<bool> {
        match self.semantics {
            EffortSemantics::Coupled => {
                if self.ultracode_pending || (self.flag == Some(true) && name == EffortName::Xhigh)
                {
                    Some(true)
                } else if let Some(flag) = self.flag {
                    // c-effortread r21 ruling B″(1): a POSITIVELY LATCHED flag
                    // wins over the last PUBLISHED tuple; the published flag is
                    // a fallback used ONLY when no positive flag evidence
                    // exists (the tuple dedupes, it never fills or overrides
                    // evidence). A suppressed unowned plain-level acceptance
                    // latches flag-OFF while a switch is armed, and a later
                    // level-only read-back (an assistant record or the /model
                    // effort clause) supersedes its tuple only in the deferred
                    // slot — the positive off evidence stays, so `{xhigh, off}`
                    // publishes even though the last published tuple is
                    // `{xhigh, on}`. On coupled builds the flag presents on
                    // only at the xhigh ultracode level, the same gate the
                    // arm above enforces.
                    Some(flag && name == EffortName::Xhigh)
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
        // Below the coupled floor the build has no ultracode; it reads Unknown
        // (refuse the flag), NOT Coupled.
        assert_eq!(
            parse_effort_version("2.1.202"),
            Some(EffortSemantics::Unknown)
        );
        assert_eq!(
            parse_effort_version("2.1.180"),
            Some(EffortSemantics::Unknown)
        );
        assert_eq!(
            parse_effort_version("2.0.0"),
            Some(EffortSemantics::Unknown)
        );
        // A missing patch is 0, so "2.1" is below the 2.1.203 floor.
        assert_eq!(parse_effort_version("2.1"), Some(EffortSemantics::Unknown));
        assert_eq!(parse_effort_version("2"), None);
        assert_eq!(parse_effort_version(""), None);
        assert_eq!(parse_effort_version("fixture"), None);
    }

    #[test]
    fn ultracode_is_refused_below_the_coupled_floor_or_when_unknown() {
        assert!(ultracode_supported(EffortSemantics::Coupled));
        assert!(ultracode_supported(EffortSemantics::Decoupled));
        assert!(!ultracode_supported(EffortSemantics::Unknown));
        // A pre-2.1.203 record must not downgrade a gate a current record set.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        assert_eq!(tracker.semantics(), EffortSemantics::Decoupled);
        tracker.note_version("2.1.180");
        assert_eq!(
            tracker.semantics(),
            EffortSemantics::Decoupled,
            "a too-old version must not move a known gate back to unknown"
        );
        // A tracker that only ever saw a too-old version stays Unknown.
        let mut old = EffortTracker::new();
        old.note_version("2.1.180");
        assert_eq!(old.semantics(), EffortSemantics::Unknown);
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
    fn every_verbatim_effort_sync_4_string_parses_to_its_measured_verdict() {
        // c-effortread r2 item 5: EVERY distinct verdict/status/refusal string
        // measured on 2.1.289 in effort-sync-4, copied byte-for-byte from the
        // recorded transcripts (walk 1–11 + launch a,b,c1,c2,d,e,f1,f2,g).
        // Each is asserted against its full parsed value, not just a prefix —
        // in particular the full max and xhigh accept descriptions, which a
        // prefix match could silently truncate.
        let d = EffortSemantics::Decoupled;
        let accepted = |name, flag| {
            EffortStdout::Accepted(ObservedEffort {
                name,
                ultracode: flag,
            })
        };
        let status = |name, flag| {
            EffortStdout::Status(ObservedEffort {
                name,
                ultracode: flag,
            })
        };

        let cases: [(&str, EffortStdout); 20] = [
            // Walk 1 / 5, launch f1: flag on, carrying the level it stayed at.
            (
                "Ultracode on (this session only): dynamic workflows on every task. Effort stays high.",
                accepted(EffortName::High, Some(true)),
            ),
            (
                "Ultracode on (this session only): dynamic workflows on every task. Effort stays max.",
                accepted(EffortName::Max, Some(true)),
            ),
            // Walk 4: flag off at max.
            (
                "Ultracode off. Effort stays max.",
                accepted(EffortName::Max, Some(false)),
            ),
            // Walk 2 / 11 level half: full plain high accept.
            (
                "Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation",
                accepted(EffortName::High, None),
            ),
            // Walk 3: the FULL max accept string, "(this session only)" plus
            // the whole multi-sentence description.
            (
                "Set effort level to max (this session only): Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks.",
                accepted(EffortName::Max, None),
            ),
            // Walk 6 / launch g: the FULL xhigh accept string.
            (
                "Set effort level to xhigh (saved as your default for new sessions): Deeper reasoning than high, just below maximum (on supported models)",
                accepted(EffortName::Xhigh, None),
            ),
            // Walk 11: slider applied level and the toggle together.
            (
                "Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation · Ultracode off",
                accepted(EffortName::High, Some(false)),
            ),
            // Launch e: cap clamp accepted at the clamped level, flag unstated.
            (
                "Effort 'max' exceeds the cap for claude-opus-5-5 set by your settings or organization; set to 'high' instead (this session only): Comprehensive implementation with extensive testing and documentation",
                accepted(EffortName::High, None),
            ),
            // Launch a: high status with the flag suffix.
            (
                "Current effort level: high (Comprehensive implementation with extensive testing and documentation) · Ultracode on",
                status(EffortName::High, Some(true)),
            ),
            // Launch b: xhigh status with the flag suffix.
            (
                "Current effort level: xhigh (Deeper reasoning than high, just below maximum (on supported models)) · Ultracode on",
                status(EffortName::Xhigh, Some(true)),
            ),
            // Launch c1 / walk 8 / launch d-after: auto resolved to medium.
            (
                "Effort level: auto (currently medium) · Ultracode on",
                status(EffortName::Medium, Some(true)),
            ),
            (
                "Effort level: auto (currently medium)",
                status(EffortName::Medium, Some(false)),
            ),
            // Launch c2: medium status, suffix absent positively means off.
            (
                "Current effort level: medium (Balanced approach with standard implementation and testing)",
                status(EffortName::Medium, Some(false)),
            ),
            // Launch e / g status lines after the flag is off.
            (
                "Current effort level: high (Comprehensive implementation with extensive testing and documentation)",
                status(EffortName::High, Some(false)),
            ),
            (
                "Current effort level: xhigh (Deeper reasoning than high, just below maximum (on supported models))",
                status(EffortName::Xhigh, Some(false)),
            ),
            // Launch d: workflows disabled refusal.
            (
                "Ultracode needs dynamic workflows enabled (see /config). Valid options are: low, medium, high, xhigh, max, auto",
                EffortStdout::WorkflowsDisabled,
            ),
            // Launch g: model-unavailable refusal.
            (
                "Ultracode isn't available on claude-sonnet-4-6. Valid options are: low, medium, high, xhigh, max, auto",
                EffortStdout::UnavailableForModel,
            ),
            // Walk 9 / 10: invalid argument in both measured list orders.
            (
                "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, auto, ultracode [on|off]",
                EffortStdout::Invalid,
            ),
            (
                "Invalid argument: ultracode bogus. Valid options are: low, medium, high, xhigh, max, auto, ultracode [on|off]",
                EffortStdout::Invalid,
            ),
            // Walk 7: the separate auto acknowledgement is not an effort accept.
            ("Effort level set to auto", EffortStdout::Other),
        ];
        for (stdout, expected) in cases {
            assert_eq!(
                parse_effort_stdout(stdout, d),
                expected,
                "stdout: {stdout:?}"
            );
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
    fn an_unowned_verdict_while_a_switch_is_armed_keeps_the_armed_attribution() {
        // c-effortread r17 item 1: B armed `/effort max`; an unowned older
        // command's accepted verdict lands first (a different word here, so the
        // transcript edge is visible). It latches the level as a terminal edge
        // — never Remuda — and B's awaiting survives, so B's own later max
        // evidence is still credited Remuda.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", true);
        let accept = tracker
            .note_unowned_stdout("Set effort level to xhigh")
            .expect("the unowned acceptance still latches the transcript level");
        assert_eq!(accept.observed.name, EffortName::Xhigh);
        assert!(
            accept.changes_published,
            "xhigh differs from published high"
        );
        // The latched edge is terminal-side when published later
        // (publish_deferred stamps EffortSource::Slash, never Remuda).
        let owned = tracker
            .observe(Some("max"), None)
            .expect("B's armed awaiting survived the unowned verdict");
        assert_eq!(owned.1, EffortSource::Remuda);

        // Same-word refusal variant: the unowned "Kept" verdict clears NOTHING;
        // B's armed switch remains attributable by its own later evidence.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", true);
        assert!(
            tracker
                .note_unowned_stdout("Kept effort level as high")
                .is_none()
        );
        let owned = tracker
            .observe(Some("max"), None)
            .expect("the unowned refusal never disarmed B");
        assert_eq!(owned.1, EffortSource::Remuda);
    }

    #[test]
    fn a_suppressed_acceptance_defers_its_edge_until_published_after_the_switch_ends() {
        // c-effortread r18 item 3: an unowned acceptance folds the transcript
        // level into a DEFERRED slot without touching the last PUBLISHED
        // state; the edge comes out via publish_deferred once the armed switch
        // ends (here: B refused), and a second publish is a dedup no-op.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        // The old command is a (human/pre-floor) slash, so the latched edge
        // carries terminal Slash attribution; nothing is armed as Remuda.
        tracker.note_slash("max", false);
        let latched = tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("max differs from the published high");
        assert_eq!(latched.observed.name, EffortName::Max);
        assert!(latched.changes_published);
        // The last PUBLISHED state is still high: suppression consumed
        // nothing.
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High)
        );
        assert_eq!(
            tracker.deferred_observation().map(|observed| observed.name),
            Some(EffortName::Max)
        );
        // B refuses (Kept) — the refusal folds nothing of max; the deferred
        // edge is now published.
        assert!(
            tracker
                .note_stdout("Kept effort level as high", true)
                .is_none()
        );
        let published = tracker
            .publish_deferred(latched.observed)
            .expect("the deferred max is published after B ends");
        assert_eq!(published.0.name, EffortName::Max);
        assert_eq!(published.1, EffortSource::Slash);
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::Max)
        );
        assert!(tracker.deferred_observation().is_none());
        // Re-publishing the same value is a dedup no-op.
        assert!(tracker.publish_deferred(latched.observed).is_none());
    }

    #[test]
    fn a_suppressed_acceptance_is_emitted_by_the_next_same_value_readback() {
        // r18 item 3 alternative: when the armed switch ends on its bounded
        // timeout without a verdict through this mapper, the next
        // authoritative assistant read-back at the suppressed value must EMIT
        // (it still differs from the last published state), not be swallowed
        // as an unchanged dedup. A later same-value read then dedups.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", false);
        tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("deferred max latched");
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High),
            "suppression never publishes"
        );
        // Assistant records then report max (B timed out unconfirmed): the
        // same-value read-back emits the edge the suppression withheld.
        let readback = tracker
            .observe(Some("max"), None)
            .expect("the same-value read-back emits");
        assert_eq!(readback.0.name, EffortName::Max);
        assert_eq!(readback.1, EffortSource::Slash);
        assert!(tracker.deferred_observation().is_none());
        assert!(
            tracker.observe(Some("max"), None).is_none(),
            "the next max read is a dedup"
        );
        // A newer different read-back supersedes a still-deferred value:
        // deferred is dropped and its edge never comes out later.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", false);
        tracker.note_unowned_stdout("Set effort level to max");
        let xhigh = tracker
            .observe(Some("xhigh"), None)
            .expect("newer authoritative read-back emits");
        assert_eq!(xhigh.0.name, EffortName::Xhigh);
        assert!(tracker.deferred_observation().is_none());
        // The mapper drops its stashed edge on this supersession, so it never
        // calls publish_deferred again; the published state stays xhigh.
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::Xhigh)
        );
    }

    #[test]
    fn a_later_suppressed_acceptance_overwrites_the_slot_even_when_it_returns_to_baseline() {
        // c-effortread r19 item 2 ruling B: the stash is ONE latest slot.
        // Suppressed max is overwritten by a suppressed acceptance back at the
        // published high; the outcome is handed back even though it changes
        // nothing, the slot holds high, and publishing after B ends emits
        // NOTHING — the older max can never surface.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", false);
        let maxed = tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("suppressed max latched");
        assert!(maxed.changes_published);
        assert_eq!(
            tracker.deferred_observation().map(|observed| observed.name),
            Some(EffortName::Max)
        );
        tracker.note_slash("high", false);
        let back = tracker
            .note_unowned_stdout("Set effort level to high")
            .expect("a return-to-baseline acceptance is still handed back");
        assert_eq!(back.observed.name, EffortName::High);
        assert!(!back.changes_published, "it equals the last published high");
        // The single slot is now the return-to-baseline high, not max.
        assert_eq!(
            tracker.deferred_observation().map(|observed| observed.name),
            Some(EffortName::High)
        );
        // B refuses: the slot publishes nothing (high is already published).
        assert!(
            tracker
                .note_stdout("Kept effort level as max", true)
                .is_none()
        );
        assert!(tracker.publish_deferred(back.observed).is_none());
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High),
            "no max is ever published"
        );
    }

    #[test]
    fn authoritative_publications_clear_the_deferred_slot_even_without_an_edge() {
        // c-effortread r19 item 2 ruling B: an assistant read-back and an
        // ultra attachment clear the deferred slot even when they emit no edge
        // (same value / sparse repeat).
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", false);
        tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("suppressed max latched");
        // A same-VALUE assistant read-back (high) emits no edge but still
        // supersedes the suppressed max.
        assert!(
            tracker.observe(Some("high"), None).is_none(),
            "same-value assistant read is a dedup"
        );
        assert!(
            tracker.deferred_observation().is_none(),
            "an assistant read-back clears the slot unconditionally"
        );

        // Attachment variant: suppressed max, then a sparse repeat of an
        // already-on flag (no edge) still clears the slot.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        tracker
            .note_ultra_attachment(true)
            .expect("the first enter latches");
        tracker.note_slash("max", false);
        tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("suppressed max latched");
        assert!(
            tracker.note_ultra_attachment(true).is_none(),
            "a sparse repeat enter emits no edge"
        );
        assert!(
            tracker.deferred_observation().is_none(),
            "the attachment clears the slot unconditionally"
        );

        // /model effort clause variant: a same-value xhigh read-back clears.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("xhigh"), None);
        tracker.note_slash("max", false);
        tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("suppressed max latched");
        assert!(
            tracker
                .observe_level(EffortName::Xhigh, EffortSource::Slash)
                .is_none(),
            "same-value /model read-back is a dedup"
        );
        assert!(
            tracker.deferred_observation().is_none(),
            "the /model effort clause clears the slot unconditionally"
        );
    }

    #[test]
    fn publish_deferred_latches_its_ultracode_value_into_the_flag() {
        // c-effortread r19 item 2 ruling B: whatever deferred state is
        // published, the tracker flag must agree with its ultracode value.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        tracker.note_slash("max", false);
        let latched = tracker
            .note_unowned_stdout("Set effort level to max")
            .expect("suppressed max latched");
        // The mapper's one slot is published carrying an authoritative flag
        // value (e.g. the attachment latched between suppression and end).
        let published = ObservedEffort {
            name: latched.observed.name,
            ultracode: Some(true),
        };
        let edge = tracker
            .publish_deferred(published)
            .expect("max differs from the published high");
        assert_eq!(edge.0.ultracode, Some(true));
        // The latched flag agrees with the published value and SURVIVES the
        // next assistant level read-back on a decoupled build (the flag is
        // preserved at ANY level): without the latch a high record would have
        // dropped it to None.
        let moved = tracker
            .observe(Some("high"), None)
            .expect("the level moves but the flag stays latched");
        assert_eq!(moved.0.name, EffortName::High);
        assert_eq!(moved.0.ultracode, Some(true));
        // An EXIT then emits the flag-off edge at the current level.
        let exit = tracker
            .note_ultra_attachment(false)
            .expect("exit flips the agreed flag");
        assert_eq!(exit.0.ultracode, Some(false));
        assert_eq!(exit.0.name, EffortName::High);
    }

    #[test]
    fn an_attachment_after_a_flag_moving_suppressed_accept_publishes_against_the_published_tuple() {
        // c-effortread r20 ruling B′ (item 1), ENTER: published {high, off},
        // a suppressed "… max · Ultracode on" acceptance moves the internal
        // flag to on WITHOUT publishing, then the ultra_effort_enter
        // attachment must still publish {high, on}: it reconciles against the
        // last PUBLISHED tuple, not the moved latch, and clears the deferred
        // slot. After B's Kept nothing stale surfaces and the published value
        // dedups.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        // Published baseline {high, off} (human slider accepted off at high).
        tracker.note_slash("high", false);
        tracker
            .note_stdout("Set effort level to high · Ultracode off", false)
            .expect("the high/off baseline publishes");
        // The unowned command's acceptance is suppressed while B is armed:
        // it latches flag on and defers {max, on}, publishing nothing.
        tracker.note_slash("max", false);
        let latched = tracker
            .note_unowned_stdout("Set effort level to max · Ultracode on")
            .expect("the suppressed acceptance is handed back");
        assert_eq!(
            latched.observed,
            ObservedEffort {
                name: EffortName::Max,
                ultracode: Some(true),
            }
        );
        assert!(latched.changes_published);
        assert_eq!(
            tracker.last_observed(),
            Some(ObservedEffort {
                name: EffortName::High,
                ultracode: Some(false),
            }),
            "the suppressed acceptance never publishes"
        );
        // The attachment ENTER must not dedupe against the moved latch.
        let enter = tracker
            .note_ultra_attachment(true)
            .expect("{high, on} differs from the published {high, off}");
        assert_eq!(
            enter.0,
            ObservedEffort {
                name: EffortName::High,
                ultracode: Some(true),
            }
        );
        assert_eq!(enter.1, EffortSource::Slash);
        assert!(
            tracker.deferred_observation().is_none(),
            "the attachment clears the deferred slot"
        );
        // B is Kept: nothing is left to publish, and the level never became max.
        assert!(
            tracker
                .note_stdout("Kept effort level as max", true)
                .is_none()
        );
        assert!(
            tracker.deferred_observation().is_none(),
            "the tracker holds no deferred value for the mapper to publish"
        );
        assert_eq!(
            tracker.last_observed().map(|o| o.name),
            Some(EffortName::High)
        );
        // A sparse repeat enter dedups against the published tuple.
        assert!(tracker.note_ultra_attachment(true).is_none());

        // EXIT mirror: published {high, on}, a suppressed "… · Ultracode off"
        // acceptance moves the latch off, then the ultra_effort_exit
        // attachment publishes {high, off} once.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        tracker.note_slash("ultracode on", false);
        tracker
            .note_stdout(
                "Ultracode on (this session only): dynamic workflows on every task. Effort stays \
                 high.",
                false,
            )
            .expect("the high/on baseline publishes");
        tracker.note_slash("high", false);
        let latched = tracker
            .note_unowned_stdout("Set effort level to high · Ultracode off")
            .expect("the suppressed off acceptance is handed back");
        assert_eq!(latched.observed.ultracode, Some(false));
        let exit = tracker
            .note_ultra_attachment(false)
            .expect("{high, off} differs from the published {high, on}");
        assert_eq!(
            exit.0,
            ObservedEffort {
                name: EffortName::High,
                ultracode: Some(false),
            }
        );
        assert!(tracker.deferred_observation().is_none());
        assert!(
            tracker
                .note_stdout("Kept effort level as max", true)
                .is_none()
        );
        assert!(
            tracker.deferred_observation().is_none(),
            "the tracker holds no deferred value for the mapper to publish"
        );
        // A repeat exit is a dedup.
        assert!(tracker.note_ultra_attachment(false).is_none());
    }

    #[test]
    fn a_same_level_status_without_the_ultracode_suffix_clears_the_deferred_slot() {
        // c-effortread r20 item 2 ruling B′: published {high, off}; while
        // B=max is armed a suppressed "… max · Ultracode on" acceptance
        // latches {max, on} and moves the flag latch on. A same-level status
        // WITHOUT the ultracode suffix is positive flag-OFF evidence: it emits
        // no edge but still goes through the shared reconcile — it latches the
        // flag off and clears the deferred slot, so B's later refusal cannot
        // publish the stale {max, on}.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.observe(Some("high"), None);
        tracker.note_slash("high", false);
        tracker
            .note_stdout("Set effort level to high · Ultracode off", false)
            .expect("the high/off baseline publishes");
        tracker.note_slash("max", false);
        tracker
            .note_unowned_stdout("Set effort level to max · Ultracode on")
            .expect("the suppressed max/on acceptance is latched");
        let EffortStdout::Status(parsed) = parse_effort_stdout(
            "Current effort level: high (Comprehensive implementation with extensive testing \
             and documentation)",
            EffortSemantics::Decoupled,
        ) else {
            panic!("the status line parses");
        };
        assert!(
            tracker.note_status(parsed).is_none(),
            "a same-level flag-off status changes no published tuple"
        );
        assert!(
            tracker.deferred_observation().is_none(),
            "the status clears the deferred {{max, on}} even without an edge"
        );
        // The flag is positively off: an exit attachment is a dedup.
        assert!(
            tracker.note_ultra_attachment(false).is_none(),
            "the status latched the flag off"
        );
        // B refuses: nothing folds and the effective state stays {high, off}.
        assert!(
            tracker
                .note_stdout("Kept effort level as max", true)
                .is_none()
        );
        assert_eq!(
            tracker.last_observed(),
            Some(ObservedEffort {
                name: EffortName::High,
                ultracode: Some(false),
            })
        );
        // A repeated same-level status stays silent.
        assert!(tracker.note_status(parsed).is_none());
    }

    #[test]
    fn r21_item1_a_suppressed_plain_off_latch_beats_the_published_xhigh_on_fallback() {
        // c-effortread r21 ruling B″(1), COUPLED 2.1.272: the last published
        // tuple is {xhigh, on} and B=max is armed. An older unowned plain
        // "Set effort level to xhigh" verdict (positive flag-OFF on coupled)
        // is suppressed, latching the flag off. The level-only /model effort
        // clause at xhigh must then publish {xhigh, OFF}: the positively
        // latched flag wins over the published-state fallback. Previously the
        // coupled branch fell back to the published flag-on, the read-back
        // equalled the published tuple and only cleared the stashes, so B's
        // Kept published nothing and the UI stayed "ultracode on".
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        tracker.note_slash("ultracode", false);
        tracker
            .note_stdout(
                "Set effort level to ultracode (this session only): xhigh + dynamic …",
                false,
            )
            .expect("the {xhigh, on} baseline publishes");
        assert_eq!(
            tracker.last_observed(),
            Some(ObservedEffort {
                name: EffortName::Xhigh,
                ultracode: Some(true),
            })
        );
        // B=max armed; the older unowned plain xhigh accept is suppressed but
        // positively latches the flag off.
        tracker.note_slash("max", true);
        let accept = tracker
            .note_unowned_stdout(
                "Set effort level to xhigh (saved as your default for new sessions): …",
            )
            .expect("the suppressed plain xhigh accept is handed back");
        assert_eq!(
            accept.observed,
            ObservedEffort {
                name: EffortName::Xhigh,
                ultracode: Some(false),
            }
        );
        // The /model verdict's level-only effort clause at xhigh: the positive
        // flag-OFF latch beats the published {xhigh, on} fallback.
        let edge = tracker
            .observe_level(EffortName::Xhigh, EffortSource::Slash)
            .expect("{xhigh, off} publishes");
        assert_eq!(
            edge.0,
            ObservedEffort {
                name: EffortName::Xhigh,
                ultracode: Some(false),
            }
        );
        assert!(tracker.deferred_observation().is_none());
        // B refuses: nothing folds, nothing stale surfaces.
        assert!(
            tracker
                .note_stdout("Kept effort level as max", true)
                .is_none()
        );
        assert_eq!(
            tracker.last_observed(),
            Some(ObservedEffort {
                name: EffortName::Xhigh,
                ultracode: Some(false),
            })
        );
        // A following xhigh assistant record agrees with the published tuple.
        assert!(tracker.observe(Some("xhigh"), None).is_none());
    }

    #[test]
    fn r21_item2_a_status_resolved_level_replaces_a_status_only_level_but_yields_to_evidence() {
        // c-effortread r21 ruling B″(2), fresh current-process tracker with no
        // assistant record: a status can be the only level evidence, and the
        // level it resolves to moves (e.g. after `/effort auto`). Once a
        // verdict/assistant channel establishes the level, a status yields.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        let status_high = match parse_effort_stdout(
            "Current effort level: high (Comprehensive implementation with extensive testing and \
             documentation)",
            EffortSemantics::Decoupled,
        ) {
            EffortStdout::Status(parsed) => parsed,
            other => panic!("{other:?}"),
        };
        let status_auto_medium = match parse_effort_stdout(
            "Effort level: auto (currently medium)",
            EffortSemantics::Decoupled,
        ) {
            EffortStdout::Status(parsed) => parsed,
            other => panic!("{other:?}"),
        };
        // 1) The first status publishes its resolved level {high, off}.
        let first = tracker
            .note_status(status_high)
            .expect("the first status establishes the level");
        assert_eq!(
            first.0,
            ObservedEffort {
                name: EffortName::High,
                ultracode: Some(false),
            }
        );
        // 2) /effort auto acknowledges without changing the projected state.
        assert!(
            tracker
                .note_stdout("Effort level set to auto", false)
                .is_none()
        );
        // 3) The auto status's resolved level replaces the status-only level.
        let resolved = tracker
            .note_status(status_auto_medium)
            .expect("auto now resolves to medium; the status-only level moves");
        assert_eq!(
            resolved.0,
            ObservedEffort {
                name: EffortName::Medium,
                ultracode: Some(false),
            }
        );
        assert_eq!(tracker.last_observed(), Some(resolved.0));
        // A repeat is a dedup.
        assert!(tracker.note_status(status_auto_medium).is_none());

        // Model-clamping precedence: an assistant record then ESTABLISHES
        // high; a status claiming auto-resolved medium must not move it.
        let established = tracker
            .observe(Some("high"), None)
            .expect("the assistant high read-back is an edge");
        assert_eq!(established.0.name, EffortName::High);
        assert!(
            tracker.note_status(status_auto_medium).is_none(),
            "the status yields to the assistant-established level"
        );
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High),
            "the status's medium never replaces an evidence level"
        );
        // A repeated status keeps yielding: no level edge, no settlement.
        assert!(tracker.note_status(status_auto_medium).is_none());
    }

    #[test]
    fn r22_item1_level_source_tracks_the_publishing_channel() {
        // c-effortread r22 ruling S: a suppressed acceptance leaves the
        // source of the PUBLISHED level alone; an ordinary accepted verdict
        // publishes with source Verdict and a later status may replace it;
        // only an assistant/model read-back sets Assistant and makes a
        // status yield.
        fn status(level: &str) -> ObservedEffort {
            match parse_effort_stdout(
                &format!("Current effort level: {level} (resolved level)"),
                EffortSemantics::Decoupled,
            ) {
                EffortStdout::Status(parsed) => parsed,
                other => panic!("{other:?}"),
            }
        }
        let accept_max = "Set effort level to max (saved as your default for new sessions): …";

        // (a) status-published high, B armed, suppressed max: a later status
        // reporting medium PUBLISHES medium — the suppressed acceptance did
        // not protect the status-only high.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker
            .note_status(status("high"))
            .expect("the first status publishes high");
        assert_eq!(tracker.level_source(), LevelSource::Status);
        tracker.note_slash("max", true);
        let suppressed = tracker
            .note_unowned_stdout(accept_max)
            .expect("the suppressed acceptance latches a deferred state");
        assert!(suppressed.changes_published);
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High),
            "the suppressed acceptance publishes nothing"
        );
        assert_eq!(
            tracker.level_source(),
            LevelSource::Status,
            "a suppressed (unpublished) acceptance never changes LevelSource"
        );
        let medium = tracker
            .note_status(status("medium"))
            .expect("a status moves the status-only level right past the suppression");
        assert_eq!(medium.0.name, EffortName::Medium);
        assert_eq!(tracker.level_source(), LevelSource::Status);

        // (b) an ordinary accepted verdict max publishes with source Verdict;
        // a later status reporting xhigh, with NO assistant record, PUBLISHES
        // xhigh — verdicts carry no clamping precedence.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.note_slash("max", false);
        tracker
            .note_stdout(accept_max, false)
            .expect("the human verdict publishes max");
        assert_eq!(tracker.level_source(), LevelSource::Verdict);
        let xhigh = tracker
            .note_status(status("xhigh"))
            .expect("a status replaces a verdict-published level");
        assert_eq!(xhigh.0.name, EffortName::Xhigh);
        assert_eq!(tracker.level_source(), LevelSource::Status);

        // (c) an assistant-established high stays: a status reporting medium
        // yields and publishes nothing (model-clamping precedence kept).
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker
            .observe(Some("high"), None)
            .expect("the assistant read-back publishes high");
        assert_eq!(tracker.level_source(), LevelSource::Assistant);
        // The status may latch its positive flag-OFF (a flag edge), but while
        // yielding it must never publish the medium LEVEL.
        if let Some((observed, _)) = tracker.note_status(status("medium")) {
            assert_eq!(
                observed.name,
                EffortName::High,
                "the status yields on the level even when its flag latch edges"
            );
        }
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High)
        );
        assert_eq!(tracker.level_source(), LevelSource::Assistant);

        // (d) a suppressed acceptance released via publish_deferred
        // publishes with source Verdict, so a later status may move it.
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.289");
        tracker.note_slash("max", true);
        tracker
            .note_unowned_stdout(accept_max)
            .expect("the acceptance is suppressed while B is armed");
        assert_eq!(tracker.level_source(), LevelSource::None);
        assert!(
            tracker
                .note_stdout("Kept effort level as max", true)
                .is_none()
        );
        let deferred = tracker
            .deferred_observation()
            .expect("the stashed acceptance survived B's refusal");
        let released = tracker
            .publish_deferred(deferred)
            .expect("the end-of-switch release publishes the stashed max");
        assert_eq!(released.0.name, EffortName::Max);
        assert_eq!(
            tracker.level_source(),
            LevelSource::Verdict,
            "a deferred acceptance is a Verdict publication when released"
        );
        assert!(
            tracker.note_status(status("medium")).is_some(),
            "the released verdict level yields to a later status"
        );
    }

    #[test]
    fn r22_item2_a_repromoted_seed_carries_the_published_levels_source() {
        // c-effortread r22 item 2 ruling S: a re-promotion seed is a
        // continuation, not new evidence — it carries the channel the prior
        // tracker published the level with. A status-only level stays Status
        // and a later status moves it; an assistant-established level keeps
        // yielding.
        fn status(level: &str) -> ObservedEffort {
            match parse_effort_stdout(
                &format!("Current effort level: {level} (resolved level)"),
                EffortSemantics::Decoupled,
            ) {
                EffortStdout::Status(parsed) => parsed,
                other => panic!("{other:?}"),
            }
        }
        let reseed = |prior: &EffortTracker| {
            let last = prior.last_observed().expect("the prior tracker published");
            let mut tracker = EffortTracker::new();
            tracker.seed_observed(
                last.name,
                last.ultracode,
                EffortSource::Remuda,
                EffortSemantics::Decoupled,
                prior.level_source(),
            );
            tracker
        };

        // Status-only high → re-promotion → a later status moves it.
        let mut prior = EffortTracker::new();
        prior.note_version("2.1.289");
        prior
            .note_status(status("high"))
            .expect("the prior tracker publishes high from status");
        assert_eq!(prior.level_source(), LevelSource::Status);
        let mut tracker = reseed(&prior);
        assert_eq!(tracker.level_source(), LevelSource::Status);
        let medium = tracker
            .note_status(status("medium"))
            .expect("the re-promoted status-only level stays movable by status");
        assert_eq!(medium.0.name, EffortName::Medium);

        // Assistant-established high → re-promotion → a status still yields.
        let mut prior = EffortTracker::new();
        prior.note_version("2.1.289");
        prior
            .observe(Some("high"), None)
            .expect("the prior tracker publishes high from an assistant record");
        assert_eq!(prior.level_source(), LevelSource::Assistant);
        let mut tracker = reseed(&prior);
        assert_eq!(tracker.level_source(), LevelSource::Assistant);
        if let Some((observed, _)) = tracker.note_status(status("medium")) {
            assert_eq!(
                observed.name,
                EffortName::High,
                "the re-promoted assistant level keeps its clamping precedence"
            );
        }
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::High)
        );

        // A verdict-published max → re-promotion → a status replaces it.
        let mut prior = EffortTracker::new();
        prior.note_version("2.1.289");
        prior.note_slash("max", false);
        prior
            .note_stdout(
                "Set effort level to max (saved as your default for new sessions): …",
                false,
            )
            .expect("the prior tracker publishes max from a verdict");
        assert_eq!(prior.level_source(), LevelSource::Verdict);
        let mut tracker = reseed(&prior);
        let xhigh = tracker
            .note_status(status("xhigh"))
            .expect("a re-promoted verdict level stays movable by status");
        assert_eq!(xhigh.0.name, EffortName::Xhigh);
    }

    #[test]
    fn r22_item3_withdrawal_with_nothing_published_clears_every_latch() {
        // c-effortread r22 item 3 ruling S: read-back becoming unavailable is
        // a full reset even when no level was ever published. A suppressed
        // acceptance (with B armed) holds the flag and the deferred slot while
        // `last` is still None; withdrawal must clear them together with
        // LevelSource and awaiting, so after provenance is re-established the
        // first status publishes and a second status can still move it.
        fn status(level: &str) -> ObservedEffort {
            match parse_effort_stdout(
                &format!("Current effort level: {level} (resolved level)"),
                EffortSemantics::Coupled,
            ) {
                EffortStdout::Status(parsed) => parsed,
                other => panic!("{other:?}"),
            }
        }
        let mut tracker = EffortTracker::new();
        tracker.note_version("2.1.272");
        // B armed, nothing published, then an older unowned acceptance is
        // suppressed: it latches flag/deferred but publishes nothing.
        tracker.note_slash("max", true);
        tracker
            .note_unowned_stdout(
                "Set effort level to xhigh (saved as your default for new sessions): …",
            )
            .expect("the unowned acceptance is folded and suppressed");
        assert_eq!(tracker.last_observed(), None);
        assert_eq!(tracker.awaiting, Some(EffortName::Max));
        assert!(tracker.deferred.is_some());
        assert_eq!(
            tracker.flag,
            Some(false),
            "the coupled accept latches flag-off"
        );

        // Provenance becomes unverifiable BEFORE any publication.
        assert_eq!(
            tracker.read_back_unavailable(),
            None,
            "nothing was published, so no withdrawal edge is emitted"
        );
        assert_eq!(tracker.level_source, LevelSource::None);
        assert_eq!(tracker.flag, None);
        assert_eq!(tracker.awaiting, None);
        assert!(!tracker.ultracode_pending);
        assert_eq!(
            tracker.deferred, None,
            "the suppressed acceptance is withdrawn, never to be released"
        );
        assert!(!tracker.is_current_process());

        // Provenance is re-established: a clean slate.
        tracker.mark_current_process();
        let first = tracker
            .note_status(status("high"))
            .expect("the first status after recovery publishes");
        assert_eq!(first.0.name, EffortName::High);
        let second = tracker
            .note_status(status("medium"))
            .expect("a second status can still move the status-only level");
        assert_eq!(second.0.name, EffortName::Medium);
        assert_eq!(
            tracker.last_observed().map(|observed| observed.name),
            Some(EffortName::Medium)
        );
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
