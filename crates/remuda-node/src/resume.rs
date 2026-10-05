//! c-resumehome: locate the predecessor conversation before a resume launch.
//!
//! Every managed Instance owns a fresh native config home, but
//! `claude --resume <id>` only loads conversations from
//! `<home>/projects/<encoded cwd>/<id>.jsonl`. Before the native factory can
//! stage that layout, the Node has to find the predecessor's transcript —
//! using recorded evidence, never a newest-file guess, and never an older
//! ancestor's file.

use crate::model::CreateInstanceRequest;
use crate::store::LocalStore;
use remuda_protocol::Knowledge;
use std::path::{Path, PathBuf};

/// Outcome of looking for the transcript a resume launch continues.
#[derive(Debug, Default)]
pub(crate) struct ResumeTranscriptLookup {
    /// Existing transcript file on this host, when one was found.
    pub path: Option<PathBuf>,
    /// Every location checked, for a refusal message that names them all.
    pub checked: Vec<PathBuf>,
    /// The `resumedFrom` instance whose transcript was required (the newest
    /// chapter), when one was named. Used to name the missing transcript
    /// instead of blaming an older ancestor.
    pub newest_instance: Option<String>,
}

impl ResumeTranscriptLookup {
    /// A refusal message naming the session, the newest predecessor and every
    /// location checked — returned to the caller before the command is
    /// accepted, instead of letting the native process die a second later with
    /// an opaque "No conversation found".
    pub(crate) fn missing_error(&self, session_id: &str) -> String {
        let newest = self
            .newest_instance
            .as_deref()
            .map(|id| format!(" (the newest chapter, instance {id})"))
            .unwrap_or_default();
        if self.checked.is_empty() {
            return format!(
                "cannot resume session {session_id}: predecessor transcript not found on this \
                 host{newest} (the earlier instance must complete at least one turn)"
            );
        }
        let looked_in = self
            .checked
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "cannot resume session {session_id}: predecessor transcript not found on this \
             host{newest} (the earlier instance must complete at least one turn); \
             looked in: {looked_in}"
        )
    }
}

/// Find the transcript file a resume launch must continue.
///
/// Only the **immediate** predecessor named by `resumedFrom` is consulted
/// (review item 5): that instance is the newest chapter, and a resume must
/// continue its conversation. If its transcript is missing on this host the
/// request is refused with a message naming it — the resolver never falls
/// back to a grandparent, whose older transcript would silently lose every
/// turn the newest chapter added.
///
/// Returns the lookup result for any resume request; the caller decides
/// (through the driver factory's `requires_local_resume_transcript`) whether a
/// missing transcript is fatal. A non-resume request returns the default empty
/// lookup.
///
/// Resolution order against the newest predecessor:
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
    // Review item 1: validate BEFORE acceptance. The token is interpolated into
    // file names on both sides of the staging boundary, so a traversal-shaped
    // id is an invalid request, never a lookup.
    if !remuda_driver::claude_transcript::is_safe_session_id(session_id) {
        return Err(crate::NodeError::InvalidRequest(format!(
            "cannot resume session {session_id:?}: not a valid Claude session id \
             (expected a UUID-style single file-name component)"
        )));
    }
    let mut lookup = ResumeTranscriptLookup::default();
    let Some(parent_id) = request.resumed_from.clone() else {
        return Ok(lookup);
    };
    lookup.newest_instance = Some(parent_id.as_id().as_str().to_owned());
    let Ok(parent) = store.get_instance(&parent_id) else {
        return Ok(lookup);
    };
    // Review item 1: the id must be the *predecessor's* recorded native
    // session, not any conversation that happens to sit on this host. An
    // unknown recording is inconclusive (structured drivers report late), and
    // so is the create-time `ins_…` placeholder that stands in until the
    // driver reports its real id; a known-but-different native id is a refusal.
    if let Knowledge::Known { value } = &parent.native_ref.session_id {
        let recorded = value.trim();
        let is_placeholder = recorded.is_empty() || recorded.starts_with("ins_");
        if !is_placeholder && recorded != session_id {
            return Err(crate::NodeError::InvalidRequest(format!(
                "cannot resume session {session_id}: predecessor {} records native session {recorded}",
                parent_id.as_id(),
            )));
        }
    }
    if let Knowledge::Known { value } = &parent.native_ref.transcript {
        let path = PathBuf::from(value.source_path.trim());
        if !value.source_path.trim().is_empty() {
            match readable_regular_file(&path) {
                Ok(true) => {
                    lookup.path = Some(path);
                    return Ok(lookup);
                }
                Ok(false) => lookup.checked.push(path),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => lookup.checked.push(path),
                Err(err) => {
                    return Err(crate::NodeError::Conflict(format!(
                        "cannot resume session {session_id}: predecessor transcript {} is not \
                         readable before acceptance: {err}",
                        path.display()
                    )));
                }
            }
        }
    }
    if let Some(recipe) = store.launch_recipe(&parent_id)? {
        let path = remuda_driver::claude_transcript::project_dir(
            Path::new(recipe.native_home.trim()),
            Path::new(recipe.cwd.trim()),
        )
        .join(format!("{session_id}.jsonl"));
        match readable_regular_file(&path) {
            Ok(true) => {
                lookup.path = Some(path);
                return Ok(lookup);
            }
            // Review item 6: a present-but-unreadable file (permissions, an IO
            // error) is refused explicitly instead of accepted and failed by
            // the staging factory; an absent path is the ordinary "looked in".
            Ok(false) => lookup.checked.push(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => lookup.checked.push(path),
            Err(err) => {
                return Err(crate::NodeError::Conflict(format!(
                    "cannot resume session {session_id}: predecessor transcript {} is not \
                     readable before acceptance: {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(lookup)
}

/// Whether `path` is an existing regular file that this process can actually
/// open for reading (review item 6).
///
/// `Path::is_file` only stats and follows symlinks, so a transcript whose
/// permissions deny reading would pass acceptance and fail only later inside
/// the staging factory. This performs the open the copy will need, using
/// `symlink_metadata` first so a symlink is never accepted as the source.
/// `Ok(false)` means absent or not a readable regular file candidate; an
/// `Err` other than [`std::io::ErrorKind::NotFound`] names the real failure.
fn readable_regular_file(path: &Path) -> std::io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}
