//! The lineage-level seat profile (cb-seat-profile, D-063, main-agent.md
//! §3.4.2; protocol.md §7.2 and §8.4).
//!
//! A Human-edited document with a revision. Every edit is a compare-and-set on
//! `expectedRevision`, and an edit takes effect at the lineage's next chapter,
//! never mid-chapter. `modelCandidates` reach the agent as a fact in
//! `skillContext.profile`; nothing switches a model automatically.

use crate::{LaunchCapabilitiesPayload, U64};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Most model candidates a profile may list.
pub const SEAT_PROFILE_MAX_MODELS: usize = 5;
/// Most configured skill directories a profile (or a spec) may list.
pub const SEAT_PROFILE_MAX_SKILLS: usize = 16;
/// Longest display name, in characters.
pub const SEAT_PROFILE_MAX_DISPLAY_NAME: usize = 64;
/// Longest one-line role, in characters.
pub const SEAT_PROFILE_MAX_ROLE: usize = 160;
/// Longest model name, in characters.
pub const SEAT_PROFILE_MAX_MODEL: usize = 128;
/// The skill directory names Remuda installs itself and a configured skill
/// may not take.
pub const RESERVED_SKILL_NAMES: &[&str] = &["remuda", "codex-computer-use"];

/// Whether `name` is a valid configured skill name: `[A-Za-z0-9._-]{1,64}`,
/// not `.`/`..` and not reserved.
#[must_use]
pub fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !RESERVED_SKILL_NAMES.contains(&name)
}

/// The profile facts a chapter launched with, stamped by the Hub inside
/// `skillContext.profile`; protocol.md §7.2.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeatProfileFacts {
    /// The profile revision this chapter applied.
    pub revision: U64,
    /// Display name, when the owner set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// One-line role, when the owner set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Model routes the owner listed, the first being the default. A fact,
    /// not a failover: whether to switch is the agent's own decision.
    #[serde(default)]
    pub model_candidates: Vec<String>,
}

/// One lineage's profile document; protocol.md §8.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeatProfile {
    /// The lineage (its first chapter's id).
    pub lineage_id: String,
    /// Bumped by every edit; the first edit creates revision 1.
    pub revision: u64,
    /// Display name.
    pub display_name: Option<String>,
    /// One-line role.
    pub role: Option<String>,
    /// 1 to 5 model routes, the first being the default.
    pub model_candidates: Vec<String>,
    /// `{name, ultracode}`, as on a create body; null: the harness decides.
    pub effort: Option<serde_json::Value>,
    /// The permission mode the next chapter launches with.
    pub permission_mode: String,
    /// Host-absolute skill directories, as `spec.skills`.
    pub skills: Vec<String>,
    /// When the row was created (RFC 3339).
    pub created_at: String,
    /// When it was last edited (RFC 3339).
    pub updated_at: String,
    /// The Human device that made the last edit.
    pub updated_by: String,
}

/// The live chapter, as the profile read reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeatProfileChapter {
    /// The lineage's current chapter.
    pub instance_id: String,
    /// Its generation.
    pub generation: u64,
    /// The profile revision it launched with; null when it launched without
    /// one.
    pub applied_revision: Option<u64>,
}

/// The newest `launch.capabilities` event of the lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeatProfileLastLaunch {
    /// The chapter that launched.
    pub instance_id: String,
    /// Its generation.
    pub generation: u64,
    /// The event's payload.
    pub payload: LaunchCapabilitiesPayload,
}

/// `GET /v1/lineages/{id}/profile`; protocol.md §8.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeatProfileView {
    /// The lineage.
    pub lineage_id: String,
    /// The document; null until the first edit.
    pub profile: Option<SeatProfile>,
    /// The lineage's current chapter.
    pub live_chapter: SeatProfileChapter,
    /// A profile exists and its revision is newer than the live chapter's
    /// applied one: the edit waits for the next chapter (「待下一章节生效」).
    pub pending: bool,
    /// The newest launch snapshot of the lineage's chapters.
    pub last_launch: Option<SeatProfileLastLaunch>,
}

/// `PUT /v1/lineages/{id}/profile` result; protocol.md §8.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeatProfilePutResult {
    /// The document after the edit.
    pub profile: SeatProfile,
    /// As on the read.
    pub pending: bool,
    /// The fields this edit changed, by wire name.
    pub changed: Vec<String>,
}
