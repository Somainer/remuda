//! c-resumehome: locate the predecessor conversation before a resume launch.
//!
//! Every managed Instance owns a fresh native config home, but
//! `claude --resume <id>` only loads conversations from
//! `<home>/projects/<encoded cwd>/<id>.jsonl`. Before the native factory can
//! stage that layout, the Node has to find the predecessor's transcript —
//! using recorded evidence, never a newest-file guess.

use crate::model::CreateInstanceRequest;
use crate::store::LocalStore;
use remuda_protocol::Knowledge;
use std::path::{Path, PathBuf};

/// Maximum parent-chain distance a resume will search. Parent links form a
/// chain, never a graph; this only guards a corrupt store.
const MAX_RESUME_HOPS: usize = 16;

/// Outcome of looking for the transcript a resume launch continues.
#[derive(Debug, Default)]
pub(crate) struct ResumeTranscriptLookup {
    /// Existing transcript file on this host, when one was found.
    pub path: Option<PathBuf>,
    /// Every location checked, for a refusal message that names them all.
    pub checked: Vec<PathBuf>,
}

impl ResumeTranscriptLookup {
    /// A refusal message naming the session and every location checked —
    /// returned to the caller before the command is accepted, instead of
    /// letting the native process die a second later with an opaque
    /// "No conversation found".
    pub(crate) fn missing_error(&self, session_id: &str) -> String {
        if self.checked.is_empty() {
            return format!(
                "cannot resume session {session_id}: predecessor transcript not found on this host \
                 (the earlier instance must complete at least one turn)"
            );
        }
        let looked_in = self
            .checked
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "cannot resume session {session_id}: predecessor transcript not found on this host \
             (the earlier instance must complete at least one turn); looked in: {looked_in}"
        )
    }
}

/// Find the transcript file a resume launch must continue, walking the
/// `resumedFrom` chain from the immediate predecessor back.
///
/// Returns the lookup result for any resume request; the caller decides
/// (through the driver factory's `requires_local_resume_transcript`) whether a
/// missing transcript is fatal. A non-resume request returns the default
/// empty lookup.
///
/// Resolution order per ancestor:
/// 1. the instance's recorded `nativeTranscriptPath` — authoritative, and the
///    only source for a promoted session whose transcript lives outside a
///    Remuda-managed native home;
/// 2. the durable launch recipe's exact native home + cwd, in Claude's own
///    `projects/<encoded cwd>/<session>.jsonl` layout (structured drivers do
///    not report a transcript path, but their recipe records the home).
pub(crate) fn resolve_resume_transcript(
    store: &dyn LocalStore,
    request: &CreateInstanceRequest,
) -> Result<ResumeTranscriptLookup, crate::NodeError> {
    let Some(session_id) = request
        .resume_session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(ResumeTranscriptLookup::default());
    };
    let mut lookup = ResumeTranscriptLookup::default();
    let mut cursor = request.resumed_from.clone();
    for _ in 0..MAX_RESUME_HOPS {
        let Some(parent_id) = cursor.take() else {
            break;
        };
        let Ok(parent) = store.get_instance(&parent_id) else {
            break;
        };
        if let Knowledge::Known { value } = &parent.native_ref.transcript {
            let path = PathBuf::from(value.source_path.trim());
            if path.is_file() {
                lookup.path = Some(path);
                return Ok(lookup);
            }
            if !value.source_path.trim().is_empty() {
                lookup.checked.push(path);
            }
        }
        if let Some(recipe) = store.launch_recipe(&parent_id)? {
            let path = remuda_driver::claude_transcript::project_dir(
                Path::new(recipe.native_home.trim()),
                Path::new(recipe.cwd.trim()),
            )
            .join(format!("{session_id}.jsonl"));
            if path.is_file() {
                lookup.path = Some(path);
                return Ok(lookup);
            }
            lookup.checked.push(path);
        }
        cursor = parent.parent.map(|link| link.instance_id);
    }
    Ok(lookup)
}
