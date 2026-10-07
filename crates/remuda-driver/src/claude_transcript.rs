//! Deterministic binding of a promoted terminal (D-025) to ITS OWN Claude
//! transcript, and an incremental tail over the bound file.
//!
//! # Why not "newest file in the slug dir"
//!
//! Several Claude sessions can run in the same repo cwd; they all write to
//! `~/.claude/projects/<encoded cwd>/`. Picking the newest-mtime `*.jsonl`
//! makes the busiest *other* session win, so the promoted terminal's structured
//! view shows a stranger's conversation. Creation/mtime/process-start windows
//! are a guess and still collide when another session starts moments later, so
//! this module never guesses: a transcript is claimed only through exact
//! identity, in this precedence:
//!
//! 1. **Hook** — Claude's own `SessionStart` hook hands over
//!    `{hook_event_name, session_id, transcript_path, cwd, ppid}`. The hook
//!    process is a child of the `claude` process, so `ppid` equals the
//!    foreground pid the promotion poller detected: an exact pid match binds
//!    it. (D-028 P1's launch shim registers the hook and relays this payload;
//!    until then [`SessionStartReport::from_stdin`] documents the shape.)
//! 2. **Pid file** — Claude writes `~/.claude/sessions/<pid>.json`
//!    (`{pid, sessionId, cwd, …}`) at startup. Reading the file named after
//!    the detected foreground pid is an exact pid → session → cwd map with no
//!    time window.
//! 3. **Argv** — an explicit `--session-id` / `--resume <uuid>` is already
//!    parsed from the process group by the promotion detector.
//!
//! If none yields, the caller stays **unbound**, hydrates nothing, and offers
//! the human an explicit picker ([`list_candidates`]); it never auto-picks.
//! Once bound, the binding is locked for the whole promotion epoch.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Encode an absolute cwd the way Claude names its project directory.
///
/// Every `/` and `.` becomes `-`, so `/Users/x/p.d` is `-Users-x-p-d`.
#[must_use]
pub fn encode_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|ch| if ch == '/' || ch == '.' { '-' } else { ch })
        .collect()
}

/// `<home>/.claude/projects/<encoded cwd>`.
///
/// Claude names the directory after the cwd **it** resolved, which on macOS is
/// the canonical path (`/tmp/x` → `/private/tmp/x`). Canonicalizing here is
/// what makes the lookup find a real session rather than an empty directory;
/// when the path cannot be resolved the literal spelling is used unchanged.
#[must_use]
pub fn project_dir(claude_home: &Path, cwd: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    claude_home
        .join("projects")
        .join(encode_project_dir(&resolved))
}

/// Maximum native session id length accepted when an id is used as a file
/// name component. UUIDs are 36 chars; this leaves headroom for another native
/// shape without letting a token double as a path.
const MAX_SESSION_ID_LEN: usize = 64;

/// Validate the native Claude session id used by `--resume <id>` and by every
/// file name the resume staging derives from it (c-resumehome, review item 1).
///
/// Real ids are UUIDs. The accepted class is deliberately a little wider — one
/// non-empty path component of ASCII alphanumerics, `-` or `_`, at most
/// [`MAX_SESSION_ID_LEN`] chars — so a future native id shape does not hard
/// fail, while anything that could traverse (`/`, `\`, `..`), hide as a dot
/// name, or smuggle a second component is rejected before acceptance.
#[must_use]
pub fn is_safe_session_id(session_id: &str) -> bool {
    let id = session_id.trim();
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && {
            let mut components = Path::new(id).components();
            matches!(
                (components.next(), components.next()),
                (Some(std::path::Component::Normal(_)), None)
            )
        }
}

/// Construct an [`std::io::Error`] with kind [`std::io::ErrorKind::InvalidInput`].
fn invalid_input(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// Construct a security refusal (a symlink or other non-regular entry where
/// the staging walk requires a real file/directory). Same kind the
/// descriptor-relative walk itself returns, so callers cannot distinguish a
/// walk-level refusal from an explicit one.
/// Validate the sidecar roots (`<session>/` and `memory/`) under an inherited
/// destination slug dir: each present root must be a real directory reached
/// through pinned fds (a symlink is refused). Guards the inherited-home
/// no-op's two known sidecar entry points.
fn validate_inherited_sidecar_roots(dest_dir_fd: &DirFd, session_id: &str) -> std::io::Result<()> {
    for root in [session_id, "memory"] {
        if let Some(entry) = dest_dir_fd.classify_leaf(root.as_bytes())?
            && entry.kind != LeafKind::Directory
        {
            return Err(symlink_refused(format!(
                "inherited resume sidecar root {root} is a symlink or non-directory; refusing the \
                 no-op"
            )));
        }
    }
    Ok(())
}

fn symlink_refused(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.into())
}

/// How a promoted terminal came to be bound to a transcript.
///
/// Reported to the UI so the header chip can state the channel honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingSource {
    /// Claude's own `SessionStart` hook, matched to the foreground pid.
    Hook,
    /// `~/.claude/sessions/<pid>.json` keyed by the foreground pid.
    PidFile,
    /// `--session-id` / `--resume` read off the agent's argv.
    Argv,
    /// Human picked the file from the candidate list.
    Manual,
}

impl BindingSource {
    /// Short stable wire label used in lifecycle `relatedIds`.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::PidFile => "pid",
            Self::Argv => "argv",
            Self::Manual => "manual",
        }
    }
}

/// An exact, epoch-scoped claim on one transcript file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptBinding {
    /// Native Claude session id (the transcript's file stem).
    pub session_id: String,
    /// Absolute path of the `<slug>/<session>.jsonl` being followed.
    pub path: PathBuf,
    /// cwd the transcript belongs to, from the channel that proved identity.
    pub cwd: PathBuf,
    /// Which deterministic channel established the binding.
    pub source: BindingSource,
}

impl TranscriptBinding {
    /// First 8 chars of the session id, for compact UI display.
    #[must_use]
    pub fn session_short(&self) -> String {
        self.session_id.chars().take(8).collect()
    }

    /// An incremental reader over the bound file, starting at byte 0.
    #[must_use]
    pub fn tail(&self) -> TranscriptTail {
        TranscriptTail::new(self.path.clone())
    }
}

/// `~/.claude/sessions/<pid>.json` as Claude writes it at process startup.
///
/// Only the fields the binding needs are modelled; the file carries more
/// (status, version, peer features, …) which is ignored.
#[derive(Debug, Deserialize)]
struct PidSession {
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: PathBuf,
}

/// Resolve an explicit session id from argv (`--session-id` / `--resume`).
///
/// The file must exist under the slug dir for the terminal cwd; an id that
/// names no file is unbound rather than guessed around.
#[must_use]
pub fn bind_by_session_id(
    claude_home: &Path,
    cwd: &Path,
    session_id: &str,
) -> Option<TranscriptBinding> {
    let session_id = session_id.trim();
    // The id becomes a file name: reject anything but one safe component
    // instead of letting a traversal-shaped id escape the slug dir.
    if !is_safe_session_id(session_id) {
        return None;
    }
    let path = project_dir(claude_home, cwd).join(format!("{session_id}.jsonl"));
    path.is_file().then(|| TranscriptBinding {
        session_id: session_id.to_owned(),
        path,
        cwd: cwd.to_path_buf(),
        source: BindingSource::Argv,
    })
}

/// Bind from Claude's own `~/.claude/sessions/<pid>.json` map (channel B).
///
/// The file is named after the detected foreground pid and carries the exact
/// `sessionId` and `cwd`, so this needs no time window. The transcript must
/// already exist under the cwd's slug dir; a pid file whose transcript has not
/// appeared yet returns `None` and the caller retries on the next poll.
#[must_use]
pub fn bind_by_pid_file(
    claude_home: &Path,
    pid: i32,
    terminal_cwd: &Path,
) -> Option<TranscriptBinding> {
    if pid <= 0 {
        return None;
    }
    let body =
        std::fs::read_to_string(claude_home.join("sessions").join(format!("{pid}.json"))).ok()?;
    let record: PidSession = serde_json::from_str(&body).ok()?;
    if !is_safe_session_id(&record.session_id) {
        tracing::warn!(
            pid,
            session_id = %record.session_id,
            "pid session id is not a single safe file-name component; refusing transcript"
        );
        return None;
    }
    let record_session = record.session_id.trim().to_owned();
    let path = project_dir(claude_home, &record.cwd).join(format!("{record_session}.jsonl"));
    if !path.is_file() {
        return None;
    }
    // The pid file's own cwd must be the terminal's cwd: a pid collision across
    // dirs must never hydrate the wrong slug.
    if !same_dir(&record.cwd, terminal_cwd) {
        tracing::warn!(
            pid,
            session_id = %record_session,
            claimed = %record.cwd.display(),
            terminal = %terminal_cwd.display(),
            "pid session cwd does not match the promoted terminal; refusing transcript"
        );
        return None;
    }
    Some(TranscriptBinding {
        session_id: record_session,
        path,
        cwd: record.cwd,
        source: BindingSource::PidFile,
    })
}

/// Payload Claude pipes to a SessionStart hook's stdin, with `ppid` added.
///
/// The launch shim (D-028 P1) registers a hook that forwards this exact shape
/// over `remuda hook emit`; a minimal hook to produce it is:
///
/// ```sh
/// #!/bin/sh
/// python3 -c 'import json,os,sys; d=json.load(sys.stdin);
///             d["ppid"]=os.getppid(); print(json.dumps(d))'
/// ```
///
/// `ppid` is the pid of the parent `claude` process — the foreground pid
/// promotion detected — which is what makes the bind exact rather than "the
/// most recent hook".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStartReport {
    /// Native session id; names the transcript file.
    pub session_id: String,
    /// Absolute path Claude itself writes the transcript to.
    pub transcript_path: PathBuf,
    /// cwd Claude resolved for the session.
    pub cwd: Option<PathBuf>,
    /// Pid of the `claude` process (the hook process's parent).
    pub ppid: Option<i64>,
}

impl SessionStartReport {
    /// Parse one hook stdin payload (`{hook_event_name, session_id,
    /// transcript_path, cwd, ppid}`; camelCase tolerated, `ppid` optional).
    pub fn from_stdin(json: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default, rename = "session_id", alias = "sessionId")]
            session_id: String,
            #[serde(default, rename = "transcript_path", alias = "transcriptPath")]
            transcript_path: PathBuf,
            #[serde(default)]
            cwd: Option<PathBuf>,
            #[serde(default)]
            ppid: Option<i64>,
        }
        let raw: Raw = serde_json::from_str(json).map_err(|err| err.to_string())?;
        if raw.session_id.trim().is_empty() || raw.transcript_path.as_os_str().is_empty() {
            return Err("SessionStart payload missing session_id/transcript_path".into());
        }
        Ok(Self {
            session_id: raw.session_id,
            transcript_path: raw.transcript_path,
            cwd: raw.cwd,
            ppid: raw.ppid,
        })
    }

    /// Bind iff the hook's parent pid is exactly the foreground agent pid and
    /// the named transcript exists. A hook from another session (same cwd,
    /// different pid) is ignored.
    ///
    /// This proves pid + file only; the slug is lossy (`.` and `/` both encode
    /// to `-`), so cwd consistency is the caller's job — a shell-pty promotion
    /// additionally requires [`transcript_belongs_to_cwd`] against the cwd it
    /// observed the agent in.
    #[must_use]
    pub fn bind(&self, foreground_pid: i32) -> Option<TranscriptBinding> {
        if self.ppid != Some(i64::from(foreground_pid)) {
            return None;
        }
        if !self.transcript_path.is_file() {
            return None;
        }
        Some(TranscriptBinding {
            session_id: self.session_id.clone(),
            path: self.transcript_path.clone(),
            cwd: self.cwd.clone().unwrap_or_default(),
            source: BindingSource::Hook,
        })
    }
}

/// True iff `path` is a transcript inside the slug dir for `cwd`.
///
/// The exact canonical-directory comparison a hook payload still has to pass:
/// matching the pid proves which `claude` process fired it, and this proves its
/// transcript belongs to the directory the terminal was promoted in.
#[must_use]
pub fn transcript_belongs_to_cwd(claude_home: &Path, cwd: &Path, path: &Path) -> bool {
    path.parent()
        .is_some_and(|dir| same_dir(dir, &project_dir(claude_home, cwd)))
}

/// One unbound candidate transcript for the manual picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptCandidate {
    /// Session id (also the option id the picker answers with).
    pub session_id: String,
    /// Absolute transcript path.
    pub path: PathBuf,
    /// First cwd found inside the file, if any record has been written.
    pub cwd: Option<PathBuf>,
    /// ISO timestamp of the first user prompt, if readable.
    pub started_at: Option<String>,
    /// Short excerpt of the first user prompt.
    pub excerpt: String,
    /// Last-write time of the file.
    pub last_activity: Option<SystemTime>,
}

impl TranscriptCandidate {
    /// Picker label: short session id, start time, and first-prompt excerpt.
    #[must_use]
    pub fn label(&self) -> String {
        let mut text = self.session_id.chars().take(8).collect::<String>();
        if let Some(started) = self.started_at.as_deref().and_then(short_ts) {
            text.push_str(" · ");
            text.push_str(&started);
        }
        let excerpt = self.excerpt.chars().take(60).collect::<String>();
        if !excerpt.is_empty() {
            text.push_str(" · ");
            text.push_str(&excerpt.replace('\n', " "));
        }
        text
    }
}

/// Enumerate every `*.jsonl` in the slug dir for the manual picker, newest
/// activity first. Reads at most the head of each file.
#[must_use]
pub fn list_candidates(claude_home: &Path, cwd: &Path) -> Vec<TranscriptCandidate> {
    let dir = project_dir(claude_home, cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") || !path.is_file() {
            continue;
        }
        let Some(session_id) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let head = read_head(&path, HEAD_BYTES);
        let (record_cwd, started_at, excerpt) = first_identity(&head);
        let last_activity = entry.metadata().and_then(|meta| meta.modified()).ok();
        candidates.push(TranscriptCandidate {
            session_id,
            path,
            cwd: record_cwd,
            started_at,
            excerpt,
            last_activity,
        });
    }
    candidates.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    candidates
}

/// Bind a manually chosen candidate session id (picker answer).
///
/// The id must name an existing file in the slug dir; anything else is
/// rejected so a stale option cannot bind after the epoch moved on.
#[must_use]
pub fn bind_manual(claude_home: &Path, cwd: &Path, session_id: &str) -> Option<TranscriptBinding> {
    let session_id = session_id.trim();
    if !is_safe_session_id(session_id) {
        return None;
    }
    let path = project_dir(claude_home, cwd).join(format!("{session_id}.jsonl"));
    path.is_file().then(|| TranscriptBinding {
        session_id: session_id.to_owned(),
        path,
        cwd: cwd.to_path_buf(),
        source: BindingSource::Manual,
    })
}

/// First `cwd` the transcript itself records (bounded head read).
///
/// `None` means no cwd-bearing record has been written yet, which is normal in
/// the first moments after a session starts.
#[must_use]
pub fn recorded_cwd(path: &Path) -> Option<String> {
    scan_cwd(&read_head(path, HEAD_BYTES))
}

/// Deterministic post-bind sanity check.
///
/// The transcript's own `cwd` field (present on user/assistant/system records)
/// must equal the terminal cwd. If no record carrying a cwd has been written
/// yet, the identity channels (pid match, exact slug path) already prove the
/// file, so this returns `true` and the caller re-checks after the first pump.
#[must_use]
pub fn cwd_matches(path: &Path, terminal_cwd: &Path) -> bool {
    let Some(found) = recorded_cwd(path) else {
        return true;
    };
    same_dir(Path::new(&found), terminal_cwd)
}

// ============================ resume staging ============================
//
// Everything below reaches the filesystem through descriptor-relative walks
// (remuda-fdsafe): see [`stage_for_resume`] for the safety contract.
use remuda_fdsafe::{DirFd, FdErrorKind, LeafKind, OpenedLeaf};
use std::os::unix::ffi::OsStrExt;

/// What [`stage_for_resume`] made visible inside the resume launch's home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedResume {
    /// The transcript path the resumed process opens (`--resume <id>` target).
    pub transcript: PathBuf,
    /// Side directories copied beside it (subagent transcripts, memory, …).
    pub sidecar_dirs: Vec<PathBuf>,
    /// Non-regular source entries (symlinks, FIFOs, sockets, devices) that were
    /// skipped: `"kind:project-relative/path"`. A skipped entry is never opened
    /// for reading or reproduced, and a symlink's target is never touched.
    pub skipped: Vec<String>,
}

/// Make a predecessor conversation visible inside a new launch's config home.
///
/// `claude --resume <session>` resolves the conversation **only** as
/// `<CLAUDE_CONFIG_DIR>/projects/<encoded cwd>/<session>.jsonl`. Every Remuda
/// managed instance owns a fresh native home, so without staging the resumed
/// process starts in an empty home and dies with "No conversation found with
/// session ID". The source is always the predecessor's *recorded* transcript
/// path — never a newest-file guess.
///
/// # Filesystem safety (c-resumehome round 3)
///
/// Every path is reached through `remuda-fdsafe` descriptor-relative walks:
///
/// - trusted roots (the predecessor project dir, the new home) are pinned as
///   directory fds; every intermediate component is opened
///   `O_NOFOLLOW|O_DIRECTORY`, so a symlinked `projects/` or `<session>/` can
///   never redirect staging, and an in-tree sidecar link (`exfil ->
///   ../bridge/passwd`) is never resolved;
/// - leaves are classified with `fstatat(AT_SYMLINK_NOFOLLOW)`, opened
///   `O_NOFOLLOW|O_NONBLOCK`, and re-classified by `fstat` on the opened fd —
///   there is no stat→open window, and a FIFO can neither block nor be copied;
/// - the whole conversation is copied into a private `.stage-…` temp tree,
///   verified against a complete manifest (names, sizes, sha256 read back from
///   the temp fds), and only then renamed into place one file at a time. A
///   crash leaves either the previous complete file or an unpublished temp
///   tree, never a partial file under a final name;
/// - provenance is the sha256 of the bytes ACTUALLY streamed, not a second
///   read of the source path;
/// - the complete selected conversation — retained files included — is charged
///   against the limits before the transcript is published.
///
/// A destination symlink is refused even when it points at the source file;
/// the inherited-home no-op happens only after that check, on a
/// `(st_dev, st_ino)` identity read from both opened fds.
///
/// # Limits
///
/// At most 256 MiB of regular-file bytes, 10 000 files and 32 sidecar
/// directory levels, transcript included. The transcript size is refused
/// before a single byte is hashed.
///
/// # Existing destinations
///
/// An existing `<session>.jsonl` is kept ONLY when a sibling staging marker
/// proves this launch staged it from exactly this predecessor (source path,
/// size, streamed sha256); the child's appended turns are preserved. Anything
/// else is a clear conflict. Sidecar destinations are accepted only when
/// byte-identical to the selected source; mismatches are refused, never
/// overwritten.
pub fn stage_for_resume(
    source_transcript: &Path,
    target_home: &Path,
    target_cwd: &Path,
    session_id: &str,
) -> std::io::Result<StagedResume> {
    stage_for_resume_with_limits(
        source_transcript,
        target_home,
        target_cwd,
        session_id,
        DEFAULT_STAGE_LIMITS,
    )
}

/// Bounds on a resume staging copy (review item 7). A predecessor home is
/// untrusted input: without caps, a huge transcript or an enormous sidecar
/// tree could fill the child instance's disk or pin the staging worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StageLimits {
    /// Total regular-file bytes in the complete conversation, transcript
    /// included and retained files included.
    max_bytes: u64,
    /// Total regular files in the complete conversation.
    max_files: u64,
    /// Directory depth below a sidecar root.
    max_depth: u32,
}

/// 256 MiB / 10 000 files / 32 levels: well above a real conversation with
/// subagent transcripts and project memory, bounded against abuse.
const DEFAULT_STAGE_LIMITS: StageLimits = StageLimits {
    max_bytes: 256 * 1024 * 1024,
    max_files: 10_000,
    max_depth: 32,
};

/// Running tally charged against [`StageLimits`] for the COMPLETE selected
/// conversation: every selected file is charged before streaming or
/// publishing starts, retained destinations included (round 3 item 6).
#[derive(Debug, Default)]
struct CopyBudget {
    files: u64,
    bytes: u64,
}

impl CopyBudget {
    fn note_file(&mut self, limits: &StageLimits) -> std::io::Result<()> {
        let next = self
            .files
            .checked_add(1)
            .ok_or_else(|| limit_exceeded("file count overflow while staging"))?;
        if next > limits.max_files {
            return Err(limit_exceeded(format!(
                "resume staging file count limit exceeded: more than {} files",
                limits.max_files
            )));
        }
        self.files = next;
        Ok(())
    }

    fn charge_bytes(&mut self, size: u64, limits: &StageLimits) -> std::io::Result<()> {
        let next = self
            .bytes
            .checked_add(size)
            .ok_or_else(|| limit_exceeded("byte count overflow while staging"))?;
        if next > limits.max_bytes {
            return Err(limit_exceeded(format!(
                "resume staging size limit exceeded: more than {} bytes",
                limits.max_bytes
            )));
        }
        self.bytes = next;
        Ok(())
    }
}

fn limit_exceeded(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

/// Dot-directory holding per-session provenance markers next to the slug dir.
const STAGING_MARKER_DIR: &str = ".remuda-staging";
/// Prefix of the per-attempt private temp tree, also inside the slug dir.
const STAGING_TMP_PREFIX: &str = ".stage-";
/// Env seam for out-of-crate integration tests; in-crate unit tests use the
/// `cfg(test)` thread-local instead (the workspace forbids `unsafe`, and
/// `std::env::set_var` is unsafe on this edition).
const PUBLISH_NO_MARKER_SEAM_ENV: &str = "REMUDA_RESUME_STAGE_SEAM_PUBLISH_NO_MARKER";
/// Env seam for a crash after copy but before publish.
const CRASH_AFTER_STAGE_SEAM_ENV: &str = "REMUDA_RESUME_STAGE_SEAM_AFTER_STAGE";

#[cfg(not(test))]
fn publish_no_marker_seam() -> bool {
    std::env::var_os(PUBLISH_NO_MARKER_SEAM_ENV).is_some()
}

#[cfg(test)]
thread_local! {
    static PUBLISH_NO_MARKER_SEAM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CRASH_AFTER_STAGE_SEAM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn publish_no_marker_seam() -> bool {
    PUBLISH_NO_MARKER_SEAM.with(std::cell::Cell::get)
        || std::env::var_os(PUBLISH_NO_MARKER_SEAM_ENV).is_some()
}

#[cfg(not(test))]
fn crash_after_stage_seam() -> bool {
    std::env::var_os(CRASH_AFTER_STAGE_SEAM_ENV).is_some()
}

#[cfg(test)]
fn crash_after_stage_seam() -> bool {
    CRASH_AFTER_STAGE_SEAM.with(std::cell::Cell::get)
        || std::env::var_os(CRASH_AFTER_STAGE_SEAM_ENV).is_some()
}

/// Provenance recorded next to a staged transcript (review item 4). The sha is
/// computed over the bytes actually streamed into the child home (round 3
/// item 3), never re-read from the source path.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StagingProvenance {
    /// Marker format version.
    version: u32,
    /// Absolute path of the predecessor transcript this copy came from.
    source: String,
    /// Source size in bytes at staging time.
    source_size: u64,
    /// Lower-hex SHA-256 of the streamed source bytes.
    source_sha256: String,
}

impl StagingProvenance {
    fn new(source: &Path, source_size: u64, source_sha256: &str) -> Self {
        Self {
            version: 1,
            source: source.display().to_string(),
            source_size,
            source_sha256: source_sha256.to_owned(),
        }
    }

    fn covers(&self, source: &Path, source_size: u64, source_sha256: &str) -> bool {
        self.version == 1
            && self.source == source.display().to_string()
            && self.source_size == source_size
            && self.source_sha256 == source_sha256
    }
}

/// One verified file in the private temp tree. `rel` is relative to the slug
/// directory, `/`-separated, e.g. `<S>/subagents/side.jsonl` or `<S>.jsonl`.
///
/// `src_identity` is the `(dev,ino)` of the SOURCE leaf this entry was
/// streamed from. It is `#[serde(skip)]` (not persisted to the manifest) and
/// used only at publish time to detect a destination that is a hardlink to
/// the predecessor: such a destination shares bytes but is a foreign inode and
/// must be replaced by the independent staged copy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ManifestEntry {
    rel: String,
    size: u64,
    sha256: String,
    /// `(dev, ino)` of the source leaf; `None` when reconstructed from the
    /// serialized manifest (verification then relies on size+sha only).
    #[serde(skip)]
    src_identity: Option<(u64, u64)>,
}

/// The complete temp-tree manifest, verified before anything is published.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StagingManifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

fn stage_for_resume_with_limits(
    source_transcript: &Path,
    target_home: &Path,
    target_cwd: &Path,
    session_id: &str,
    limits: StageLimits,
) -> std::io::Result<StagedResume> {
    let session_id = session_id.trim();
    if !is_safe_session_id(session_id) {
        return Err(invalid_input(format!(
            "resume session id {session_id:?} is not a single safe file-name component \
             (expected a UUID-style token)"
        )));
    }

    // ---- Source: pin the predecessor HOME as the trusted root and WALK the
    // `projects/<slug>` descendants with O_NOFOLLOW, then open the transcript
    // as a real regular leaf. A symlinked `projects`/`<slug>` component — e.g.
    // `predecessor-home/projects -> /outside` — fails the walk instead of
    // letting staging copy an outside tree (round 5 part 2 item 1).
    let source_abs = absolutize(source_transcript);
    let source_parts: Vec<std::ffi::OsString> =
        source_abs.iter().map(std::ffi::OsString::from).collect();
    let source_name = source_abs
        .file_name()
        .map(std::ffi::OsString::from)
        .ok_or_else(|| {
            invalid_input(format!(
                "resume transcript path has no file name: {}",
                source_abs.display()
            ))
        })?;
    let src_dir_fd = source_project_dir_fd(&source_abs, &source_parts)?;
    let src_leaf = src_dir_fd.open_regular_leaf(source_name.as_bytes()).map_err(|error| {
        if error.kind == FdErrorKind::Missing {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "resume transcript not found at {}: it is not a readable regular file in the \
                     predecessor directory",
                    source_abs.display()
                ),
            )
        } else {
            std::io::Error::from(error)
        }
    })?;
    let source_size = src_leaf.len;
    let src_identity = src_leaf.identity()?;
    // Round 3 item 7: refuse an oversized transcript before reading/hashing.
    if source_size > limits.max_bytes {
        return Err(limit_exceeded(format!(
            "resume staging size limit exceeded: predecessor transcript is {source_size} bytes, \
             more than the {} byte cap",
            limits.max_bytes
        )));
    }

    // ---- Destination: walk/create `home/projects/<slug>` with every
    // intermediate opened O_NOFOLLOW|O_DIRECTORY.
    let resolved_cwd =
        std::fs::canonicalize(target_cwd).unwrap_or_else(|_| target_cwd.to_path_buf());
    let slug = encode_project_dir(&resolved_cwd);
    let dest_home_fd = DirFd::anchor_or_create(&absolutize(target_home))?;
    let projects_fd = dest_home_fd.ensure_subdir(b"projects")?;
    let dest_dir_fd = projects_fd.ensure_subdir(slug.as_bytes())?;
    let dest_dir_identity = dest_dir_fd.dir_identity()?;
    let dest_dir = project_dir(target_home, &resolved_cwd);
    let transcript_name = format!("{session_id}.jsonl");
    let transcript_name_bytes = transcript_name.as_bytes().to_vec();
    let dest_transcript = dest_dir.join(&transcript_name);

    // Round 3 item 1: classify the destination leaf BEFORE any same-file
    // identity check. A symlinked `<S>.jsonl` pointing at the predecessor is a
    // refusal, not an inherited-home no-op.
    let existing = dest_dir_fd.classify_leaf(&transcript_name_bytes)?;
    let retained = match existing {
        Some(entry) if entry.kind == LeafKind::Symlink => {
            return Err(symlink_refused(format!(
                "resume destination {} is a symlink, not a real file; refusing to stage through it",
                dest_transcript.display()
            )));
        }
        Some(entry) if entry.kind != LeafKind::Regular => {
            return Err(invalid_input(format!(
                "resume destination {} is not a regular file",
                dest_transcript.display()
            )));
        }
        Some(_) => {
            let dest_leaf = dest_dir_fd.open_regular_leaf(&transcript_name_bytes)?;
            if dest_leaf.identity()? == src_identity
                && Some(dest_dir_identity) == src_dir_fd.dir_identity().ok()
            {
                // Same file in the SAME inherited project directory. Round 5
                // part 2 item 5: even the inherited-home no-op must not return
                // before validating that the `<S>/` and `memory/` sidecar
                // roots present under the destination are real directories
                // (never symlinks). The transcript itself is already proven a
                // real regular leaf above.
                validate_inherited_sidecar_roots(&dest_dir_fd, session_id)?;
                return Ok(StagedResume {
                    transcript: dest_transcript,
                    sidecar_dirs: Vec::new(),
                    skipped: Vec::new(),
                });
            }
            // A different regular file: it survives ONLY with valid
            // provenance, checked after the source has been streamed (the
            // marker is validated against the streamed sha, not a path re-read).
            true
        }
        None => false,
    };

    // ---- Private temp tree for this attempt, inside the pinned slug dir.
    // Round 3 item 5: first discard every temp tree a crashed/ENOSPC-killed
    // earlier attempt left behind — none of their partial files may be charged,
    // trusted, or published. They are private (dot-prefixed, EXCL-created by
    // this code), so removal never touches an operator's directory.
    for entry in dest_dir_fd.entries()? {
        let name = String::from_utf8_lossy(&entry.name);
        if name.starts_with(STAGING_TMP_PREFIX) {
            match entry.kind {
                LeafKind::Directory => dest_dir_fd.remove_private_tree(&entry.name)?,
                // A link wearing our private prefix is an intrusion, not
                // removable as a temp tree and never to be touched.
                LeafKind::Symlink | LeafKind::Other => {
                    return Err(invalid_input(format!(
                        "resume staging refuses a non-directory entry wearing its private temp \
                         prefix in {}: {name}",
                        dest_dir.display()
                    )));
                }
                LeafKind::Regular => {
                    // A regular file with the temp prefix cannot be one of ours
                    // (temp trees are directories); leave it alone and refuse.
                    return Err(invalid_input(format!(
                        "resume staging refuses a regular file wearing its private temp prefix \
                         in {}: {name}",
                        dest_dir.display()
                    )));
                }
            }
        }
    }
    let tmp_name = format!(
        "{STAGING_TMP_PREFIX}{session_id}-{}",
        uuid::Uuid::new_v4().as_simple()
    );
    let tmp_name_bytes = tmp_name.as_bytes().to_vec();
    let temp_fd = dest_dir_fd.ensure_subdir(&tmp_name_bytes)?;

    let mut budget = CopyBudget::default();
    let mut skipped: Vec<String> = Vec::new();
    let outcome = build_verify_and_publish(
        src_leaf,
        &src_dir_fd,
        &source_name,
        source_transcript,
        session_id,
        &dest_dir,
        &dest_dir_fd,
        &temp_fd,
        &transcript_name,
        source_size,
        retained,
        &limits,
        &mut budget,
        &mut skipped,
    );

    // Round 3 item 6: a failed attempt's unpublished temp tree is ALWAYS
    // discarded — a later retry must not charge or trust its leftovers.
    if let Err(error) = dest_dir_fd.remove_private_tree(&tmp_name_bytes) {
        tracing::debug!(%error, "resume staging temp tree cleanup failed");
    }
    let sidecar_dirs = outcome?;
    Ok(StagedResume {
        transcript: dest_transcript,
        sidecar_dirs,
        skipped,
    })
}

/// Copy the whole selected conversation into `temp_fd`, verify its manifest,
/// then publish into `dest_dir_fd`. On any error nothing published is left
/// half-written: files land via `renameat`, sidecars land before the
/// transcript, and the caller removes the temp tree.
#[allow(clippy::too_many_arguments)]
fn build_verify_and_publish(
    transcript_leaf: OpenedLeaf,
    src_dir_fd: &DirFd,
    source_name: &std::ffi::OsStr,
    source_transcript: &Path,
    session_id: &str,
    dest_dir: &Path,
    dest_dir_fd: &DirFd,
    temp_fd: &DirFd,
    transcript_name: &str,
    source_size: u64,
    retained: bool,
    limits: &StageLimits,
    budget: &mut CopyBudget,
    skipped: &mut Vec<String>,
) -> std::io::Result<Vec<PathBuf>> {
    use std::io::Write;

    let mut seeds: Vec<ManifestEntry> = Vec::new();
    let mut published_roots: Vec<PathBuf> = Vec::new();

    // (1) Transcript. Reserve the file count up front; bytes are charged on
    // the real stream (round 4 item 10), capped at the remaining budget.
    budget.note_file(limits)?;
    // Round 4 item 4: stream the ORIGINAL opened fd; the name is never
    // re-opened after validation.
    let mut transcript_src = transcript_leaf;
    let transcript_leaf_identity = transcript_src.identity()?;
    let mut transcript_tmp = temp_fd.create_leaf_excl(transcript_name.as_bytes())?;
    let remaining_bytes = limits.max_bytes.saturating_sub(budget.bytes);
    let streamed = stream_hashed(
        &mut &mut transcript_src.file,
        &mut transcript_tmp,
        source_size,
        remaining_bytes,
    )?;
    let transcript_sha = streamed.sha256;
    budget.charge_bytes(streamed.bytes, limits)?;
    drop(transcript_tmp);
    seeds.push(ManifestEntry {
        rel: transcript_name.to_owned(),
        size: streamed.bytes,
        sha256: transcript_sha.clone(),
        src_identity: Some(transcript_leaf_identity),
    });

    // (2) Sidecar roots. `<dir>/<S>/` first (a renamed transcript keeps the
    // native id on its sidecar dir), the file-stem dir only for older layouts,
    // then project-global `memory/`.
    let mut candidates: Vec<std::ffi::OsString> = vec![session_id.into()];
    if let Some(stem) = Path::new(source_name)
        .file_stem()
        .map(std::ffi::OsString::from)
        .filter(|stem| stem != session_id)
    {
        candidates.push(stem);
    }
    candidates.push("memory".into());

    let mut session_root_taken = false;
    for (index, candidate) in candidates.iter().enumerate() {
        let is_memory = index == candidates.len() - 1;
        let root_name = candidate.as_bytes();
        let root_label = String::from_utf8_lossy(root_name).into_owned();
        let entry = match src_dir_fd.classify_leaf(root_name)? {
            Some(entry) => entry,
            None => continue,
        };
        match entry.kind {
            LeafKind::Directory => {
                // For the per-session slot only the first existing root wins;
                // `memory` is its own independent root.
                if !is_memory && session_root_taken {
                    continue;
                }
                let root_fd = src_dir_fd.subdir(root_name)?;
                let mut files: Vec<(String, u64)> = Vec::new();
                enumerate_regular(
                    &root_fd,
                    "",
                    0,
                    &root_label,
                    limits,
                    budget,
                    skipped,
                    &mut files,
                )?;
                for (rel, size) in files {
                    let mut account = StageAccount {
                        limits,
                        budget,
                        seeds: &mut seeds,
                    };
                    copy_sidecar_seed(&root_fd, &root_label, &rel, size, temp_fd, &mut account)?;
                    // account holds &mut seeds; drop it before the next loop use.
                }
                if !is_memory {
                    session_root_taken = true;
                }
                published_roots.push(dest_dir.join(&root_label));
            }
            // Round 3 items 2/4: a symlinked or non-regular sidecar ROOT is
            // skipped and reported, never opened and never recreated; the
            // transcript still stages.
            LeafKind::Symlink => skipped.push(format!("symlink:{root_label}")),
            LeafKind::Other => skipped.push(format!("non-regular:{root_label}")),
            LeafKind::Regular => skipped.push(format!("non-directory:{root_label}")),
        }
    }

    // (3) Write + verify the manifest against the temp tree itself.
    let manifest = StagingManifest {
        version: 1,
        entries: seeds.clone(),
    };
    let body = serde_json::to_vec_pretty(&manifest).map_err(std::io::Error::other)?;
    temp_fd
        .create_leaf_excl(b".manifest.json")?
        .write_all(&body)?;
    verify_temp_tree(temp_fd, &seeds)?;

    // Test seam: crash after files are staged (and verified) in the private
    // temp tree but before anything is published. The caller must remove the
    // whole populated stage dir; a regression test asserts it is gone.
    if crash_after_stage_seam() {
        return Err(std::io::Error::other(
            "injected staging failure after copy, before publish (test seam)",
        ));
    }

    // (4) A retained destination transcript now has to prove provenance
    // against the source bytes we actually streamed. Round 4 item 9: a
    // transcript published in an earlier attempt whose marker never landed
    // (a crash in the publish window) is RECOVERABLE — re-hash the bytes at
    // the destination and accept them only if they are exactly the bytes the
    // current source streamed.
    // Decide whether to publish the fresh staged transcript over an existing
    // destination. Markerless recovery MUST replace the foreign inode (it may
    // be a hardlink to an unrelated outside file with identical bytes); a
    // marker-matched independent file is kept.
    #[derive(PartialEq, Eq)]
    enum TranscriptPublish {
        RenameOver,
        KeepExisting,
    }
    let transcript_publish = if !retained {
        TranscriptPublish::RenameOver
    } else {
        let published_size = seeds
            .iter()
            .find(|seed| seed.rel == transcript_name)
            .map_or(source_size, |seed| seed.size);
        match read_staging_provenance(dest_dir_fd, transcript_name)? {
            // A marker covering the predecessor we just streamed. Keep the
            // existing inode only if it is independent (nlink == 1) and its
            // inode did not change to the source's inode; otherwise replace it
            // so the published inode is ours (round 5 part 2 item 3).
            Some(marker) if marker.covers(source_transcript, published_size, &transcript_sha) => {
                let dest_leaf = dest_dir_fd.open_regular_leaf(transcript_name.as_bytes())?;
                let same_as_source = dest_leaf.identity()? == transcript_leaf_identity;
                if same_as_source || dest_leaf.nlink()? > 1 {
                    TranscriptPublish::RenameOver
                } else {
                    TranscriptPublish::KeepExisting
                }
            }
            Some(_) => {
                return Err(invalid_input(format!(
                    "resume destination {} exists but its staging provenance does not match \
                     predecessor {} (size {published_size}, sha256 {transcript_sha}); refusing to \
                     overwrite",
                    dest_dir.join(transcript_name).display(),
                    source_transcript.display()
                )));
            }
            None => {
                // No marker: an interrupted publish OR a markerless foreign
                // file. In BOTH cases replace the inode with the freshly
                // written O_EXCL temp copy, but only accept doing so when the
                // bytes already at the destination match what we streamed
                // (otherwise a foreign conversation is at risk → refuse).
                let bytes_match = destination_bytes_match(
                    dest_dir_fd,
                    transcript_name,
                    published_size,
                    &transcript_sha,
                )?;
                if !bytes_match {
                    return Err(invalid_input(format!(
                        "resume destination {dest_transcript} already exists without Remuda staging \
                         provenance for predecessor {source} (and its bytes do not match the staged \
                         copy); refusing to keep or overwrite a conversation this launch did not \
                         stage (resume in a fresh native home or remove the stale file)",
                        dest_transcript = dest_dir.join(transcript_name).display(),
                        source = source_transcript.display()
                    )));
                }
                TranscriptPublish::RenameOver
            }
        }
    };

    // (5) Publish: empty root dirs first, then sidecars (identical files are
    // kept, foreign/diverging files are a conflict), transcript last.
    for root in &published_roots {
        let name = root
            .file_name()
            .map(std::ffi::OsStr::as_bytes)
            .unwrap_or_default()
            .to_vec();
        dest_dir_fd.ensure_subdir(&name)?;
    }
    for seed in &seeds {
        if seed.rel == transcript_name {
            continue;
        }
        publish_one_seed(temp_fd, dest_dir_fd, seed)?;
    }
    // Publish the transcript. A fresh destination is renamed over; the
    // absence re-check below refuses to replace a file that raced in. A
    // retained independent file (valid marker, nlink==1, not the source inode)
    // keeps its bytes (it may carry this child's appended turns) and drops the
    // staged copy.
    if transcript_publish == TranscriptPublish::RenameOver
        && !retained
        && dest_dir_fd
            .classify_leaf(transcript_name.as_bytes())?
            .is_some()
    {
        return Err(invalid_input(format!(
            "resume destination {} appeared during staging without provenance; refusing to \
             overwrite",
            dest_dir.join(transcript_name).display()
        )));
    }
    if transcript_publish == TranscriptPublish::RenameOver {
        temp_fd.rename(
            transcript_name.as_bytes(),
            dest_dir_fd,
            transcript_name.as_bytes(),
        )?;
    } else {
        let _ = temp_fd.unlink_file(transcript_name.as_bytes());
    }

    // Test seam: simulate a crash AFTER the transcript (and sidecars) are
    // published but BEFORE the provenance marker is written. The transcript is
    // left stranded without a marker; a retry must detect and recover it via
    // destination_bytes_match. Toggled by the cfg(test) thread-local (the
    // workspace forbids the unsafe env::set_var); the env var is the seam for
    // out-of-crate integration tests.
    if publish_no_marker_seam() {
        return Err(std::io::Error::other(
            "injected staging failure between transcript publish and marker (test seam)",
        ));
    }

    // (6) Provenance marker last (a reader sees the transcript before the
    // marker can validate it; the reverse order would validate nothing).
    let published_size = seeds
        .iter()
        .find(|seed| seed.rel == transcript_name)
        .map_or(source_size, |seed| seed.size);
    write_staging_provenance(
        dest_dir_fd,
        transcript_name,
        &StagingProvenance::new(source_transcript, published_size, &transcript_sha),
    )?;

    Ok(published_roots)
}

/// Whether the existing destination transcript is byte-identical to the copy
/// we just streamed from the source: same size and same sha256. Used to
/// recover an interrupted publish (transcript renamed, marker never written).
fn destination_bytes_match(
    dest_dir_fd: &DirFd,
    transcript_name: &str,
    source_size: u64,
    source_sha: &str,
) -> std::io::Result<bool> {
    let leaf = match dest_dir_fd.open_regular_leaf(transcript_name.as_bytes()) {
        Ok(leaf) => leaf,
        Err(error) if error.kind == FdErrorKind::Missing => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if leaf.len != source_size {
        return Ok(false);
    }
    let sha = hash_reader(&mut &leaf.file)?;
    Ok(sha == source_sha)
}

/// Recursively classify a source sidecar directory. Regular files are
/// pre-charged (count + recorded size) and collected with paths RELATIVE TO
/// THE ROOT (not including its label); directories are descended within the
/// depth cap; symlinks and other non-regular entries are skipped and reported
/// with the root label prefix — never opened, never followed.
#[allow(clippy::too_many_arguments)]
fn enumerate_regular(
    dir_fd: &DirFd,
    rel_inside: &str,
    depth: u32,
    root_label: &str,
    limits: &StageLimits,
    budget: &mut CopyBudget,
    skipped: &mut Vec<String>,
    files: &mut Vec<(String, u64)>,
) -> std::io::Result<()> {
    for entry in dir_fd.entries()? {
        let rel = if rel_inside.is_empty() {
            String::from_utf8_lossy(&entry.name).into_owned()
        } else {
            format!("{rel_inside}/{}", String::from_utf8_lossy(&entry.name))
        };
        match entry.kind {
            LeafKind::Directory => {
                if depth >= limits.max_depth {
                    return Err(limit_exceeded(format!(
                        "resume staging depth limit exceeded: more than {} nested directories",
                        limits.max_depth
                    )));
                }
                let sub = dir_fd.subdir(&entry.name)?;
                enumerate_regular(
                    &sub,
                    &rel,
                    depth + 1,
                    root_label,
                    limits,
                    budget,
                    skipped,
                    files,
                )?;
            }
            LeafKind::Regular => {
                // Round 4 item 10: do NOT charge the enumerated size here —
                // a file can grow between enumeration and the open. Only the
                // file count is reserved now; bytes are charged on the real
                // stream against the REMAINING aggregate budget.
                budget.note_file(limits)?;
                files.push((rel, entry.len));
            }
            LeafKind::Symlink => skipped.push(format!("symlink:{root_label}/{rel}")),
            LeafKind::Other => skipped.push(format!("non-regular:{root_label}/{rel}")),
        }
    }
    Ok(())
}

/// Reopen one enumerated sidecar file through the fd walk (a swap to a
/// symlink between enumeration and open is refused here), stream it into the
/// temp tree under `<root_label>/<rel>`, and record its slug-relative manifest
/// entry.
/// Mutable per-attempt accounting shared by the transcript and every
/// sidecar stream: the live aggregate budget and the manifest being built.
struct StageAccount<'a> {
    limits: &'a StageLimits,
    budget: &'a mut CopyBudget,
    seeds: &'a mut Vec<ManifestEntry>,
}

fn copy_sidecar_seed(
    root_fd: &DirFd,
    root_label: &str,
    rel: &str,
    expected_size: u64,
    temp_fd: &DirFd,
    account: &mut StageAccount,
) -> std::io::Result<()> {
    let seeds = &mut *account.seeds;
    let inside: Vec<&[u8]> = rel.split('/').map(str::as_bytes).collect();
    // Source: walk from the root fd along the root-relative chain.
    let mut source_chain: Vec<DirFd> = Vec::new();
    for component in &inside[..inside.len().saturating_sub(1)] {
        let base = source_chain.last().unwrap_or(root_fd);
        source_chain.push(base.subdir(component)?);
    }
    let source_parent = source_chain.last().unwrap_or(root_fd);
    let leaf = source_parent.open_regular_leaf(inside.last().unwrap())?;

    // Destination in the temp tree mirrors the slug: root label first.
    let root_bytes = root_label.as_bytes();
    let mut temp_chain: Vec<DirFd> = Vec::new();
    {
        let base = temp_chain.last().unwrap_or(temp_fd);
        temp_chain.push(base.ensure_subdir(root_bytes)?);
    }
    for component in &inside[..inside.len().saturating_sub(1)] {
        let base = temp_chain.last().unwrap_or(temp_fd);
        temp_chain.push(base.ensure_subdir(component)?);
    }
    let temp_parent = temp_chain.last().unwrap_or(temp_fd);
    let mut out = temp_parent.create_leaf_excl(inside.last().unwrap())?;
    // Stream at most the REMAINING aggregate byte budget (+1 byte is probed
    // inside copy_capped to detect an over-cap stream); charge the actual
    // byte count afterwards.
    let source_identity = leaf.identity()?;
    let remaining_bytes = account
        .limits
        .max_bytes
        .saturating_sub(account.budget.bytes);
    let streamed = stream_hashed(&mut &leaf.file, &mut out, expected_size, remaining_bytes)?;
    account
        .budget
        .charge_bytes(streamed.bytes, account.limits)?;
    seeds.push(ManifestEntry {
        rel: format!("{root_label}/{rel}"),
        size: streamed.bytes,
        sha256: streamed.sha256,
        src_identity: Some(source_identity),
    });
    Ok(())
}

/// Copy result: the number of bytes streamed and their sha256.
struct Streamed {
    bytes: u64,
    sha256: String,
}

/// Stream `src` to `dst`, SHA-256-ing every byte actually copied.
///
/// `min_size` is the size the entry was classified with; the stream must read
/// at least that many (a truncated file fails). `remaining_bytes` is the
/// caller's remaining AGGREGATE budget; no more than that many bytes are
/// copied, and a stream with one more byte fails. A file that grew after
/// enumeration therefore fails the cap instead of pushing the copy over.
fn stream_hashed<R, W>(
    src: &mut R,
    dst: &mut W,
    min_size: u64,
    remaining_bytes: u64,
) -> std::io::Result<Streamed>
where
    R: std::io::Read,
    W: std::io::Write,
{
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut hashing = HashedWriter {
        out: dst,
        hasher: &mut hasher,
    };
    let bytes = remuda_fdsafe::copy_capped(src, &mut hashing, remaining_bytes)?;
    if bytes < min_size {
        return Err(std::io::Error::other(format!(
            "resume staging source file shrank while copying: classified {min_size}, read {bytes}"
        )));
    }
    Ok(Streamed {
        bytes,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// A writer that tees each byte into a Sha256 hasher and onward to `out`.
struct HashedWriter<'a, W: std::io::Write> {
    out: &'a mut W,
    hasher: &'a mut sha2::Sha256,
}
impl<W: std::io::Write> std::io::Write for HashedWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        sha2::Digest::update(&mut *self.hasher, buf);
        self.out.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// Walk the temp tree and prove every manifest entry is present at exactly the
/// recorded size and sha256, with no unrecorded regular file.
fn verify_temp_tree(temp_fd: &DirFd, seeds: &[ManifestEntry]) -> std::io::Result<()> {
    use std::collections::HashMap;
    let mut expected: HashMap<String, &ManifestEntry> =
        seeds.iter().map(|seed| (seed.rel.clone(), seed)).collect();
    verify_dir(temp_fd, String::new(), &mut expected)?;
    if !expected.is_empty() {
        let missing: Vec<&str> = expected.keys().map(String::as_str).collect();
        return Err(invalid_input(format!(
            "resume staging manifest verification failed; missing temp entries: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

fn verify_dir(
    dir_fd: &DirFd,
    prefix: String,
    expected: &mut std::collections::HashMap<String, &ManifestEntry>,
) -> std::io::Result<()> {
    for entry in dir_fd.entries()? {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        if prefix.is_empty() && name == ".manifest.json" {
            continue;
        }
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match entry.kind {
            LeafKind::Directory => {
                let sub = dir_fd.subdir(&entry.name)?;
                verify_dir(&sub, rel, expected)?;
            }
            LeafKind::Regular => {
                let seed = match expected.remove(&rel) {
                    Some(seed) => seed,
                    None => {
                        return Err(invalid_input(format!(
                            "resume staging manifest verification failed; unrecorded temp file {rel}"
                        )));
                    }
                };
                let leaf = dir_fd.open_regular_leaf(&entry.name)?;
                if leaf.len != seed.size {
                    return Err(invalid_input(format!(
                        "resume staging manifest verification failed; {rel} size {} != {}",
                        leaf.len, seed.size
                    )));
                }
                let mut file = &leaf.file;
                let sha = hash_reader(&mut file)?;
                if sha != seed.sha256 {
                    return Err(invalid_input(format!(
                        "resume staging manifest verification failed; {rel} sha256 mismatch"
                    )));
                }
            }
            // The temp tree is built entirely by this process with EXCL
            // creates, so any link or special file is an intrusion.
            LeafKind::Symlink | LeafKind::Other => {
                return Err(invalid_input(format!(
                    "resume staging manifest verification failed; non-regular temp entry {rel}"
                )));
            }
        }
    }
    Ok(())
}

fn hash_reader<R>(reader: &mut R) -> std::io::Result<String>
where
    R: std::io::Read + ?Sized,
{
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            return Ok(format!("{:x}", hasher.finalize()));
        }
        hasher.update(&buf[..read]);
    }
}

/// Split a `/`-relative path into single byte components.
fn rel_components(rel: &str) -> Vec<&[u8]> {
    rel.split('/')
        .filter(|part| !part.is_empty())
        .map(str::as_bytes)
        .collect()
}

/// Publish one sidecar file: an identical existing destination is kept (and
/// the temp copy dropped); a missing destination gets the temp file renamed
/// into place after creating real parent directories; a symlink, special file
/// or diverging regular file is a hard conflict.
fn publish_one_seed(
    temp_fd: &DirFd,
    dest_dir_fd: &DirFd,
    seed: &ManifestEntry,
) -> std::io::Result<()> {
    let components = rel_components(&seed.rel);
    let Some((&last, parent_components)) = components.split_last() else {
        return Err(invalid_input(format!(
            "empty staging relpath: {}",
            seed.rel
        )));
    };

    // Existing destination? Walk the real parent chain; a missing intermediate
    // means "not present yet".
    let mut dest_chain: Vec<DirFd> = Vec::new();
    let mut existed = true;
    for &component in parent_components {
        let base = dest_chain.last().unwrap_or(dest_dir_fd);
        match base.subdir(component) {
            Ok(next) => dest_chain.push(next),
            Err(error) if error.kind == FdErrorKind::Missing => {
                existed = false;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let dest_parent = dest_chain.last().unwrap_or(dest_dir_fd);
    if existed {
        // Parents exist; the leaf itself may still be absent.
        match dest_parent.classify_leaf(last)? {
            None => { /* absent — fall through to the rename */ }
            Some(entry) if entry.kind == LeafKind::Regular => {
                // Verify identity of the bytes already at the destination.
                let leaf = dest_parent.open_regular_leaf(last)?;
                let mut file = &leaf.file;
                let bytes_match = leaf.len == seed.size && hash_reader(&mut file)? == seed.sha256;
                let dest_identity = leaf.identity()?;
                let dest_nlink = leaf.nlink()?;
                let is_hardlink_to_source =
                    seed.src_identity.is_some_and(|src| src == dest_identity) || dest_nlink > 1;
                if !bytes_match {
                    return Err(invalid_input(format!(
                        "resume sidecar {} already exists with different content; refusing to \
                         overwrite an unverified file",
                        seed.rel
                    )));
                }
                if is_hardlink_to_source {
                    // Byte-identical but a FOREIGN inode still shared with the
                    // predecessor (hardlink): fall through to replace it with
                    // the independent staged copy (drop the temp after rename).
                } else {
                    // Independent file with identical bytes: keep it, drop the
                    // staged copy.
                    unlink_temp_relative(temp_fd, &components)?;
                    return Ok(());
                }
            }
            Some(_) => {
                return Err(invalid_input(format!(
                    "resume sidecar destination {} is a symlink or non-regular file; refusing \
                     to stage through it",
                    seed.rel
                )));
            }
        }
    }

    // Missing: create real destination parent dirs (O_NOFOLLOW revalidation).
    let mut publish_chain: Vec<DirFd> = Vec::new();
    for &component in parent_components {
        let base = publish_chain.last().unwrap_or(dest_dir_fd);
        publish_chain.push(base.ensure_subdir(component)?);
    }
    let publish_parent = publish_chain.last().unwrap_or(dest_dir_fd);

    // Open the temp parent and rename the leaf across the two pinned fds.
    let mut temp_chain: Vec<DirFd> = Vec::new();
    for &component in parent_components {
        let base = temp_chain.last().unwrap_or(temp_fd);
        temp_chain.push(base.subdir(component)?);
    }
    let temp_parent = temp_chain.last().unwrap_or(temp_fd);
    temp_parent.rename(last, publish_parent, last)?;
    Ok(())
}

/// Unlink one file at `components` below `root`, ignoring a missing entry
/// (idempotent cleanup of already-published temp copies).
fn unlink_temp_relative(root: &DirFd, components: &[&[u8]]) -> std::io::Result<()> {
    let Some((&last, parents)) = components.split_last() else {
        return Ok(());
    };
    let mut chain: Vec<DirFd> = Vec::new();
    for &component in parents {
        let base = chain.last().unwrap_or(root);
        chain.push(base.subdir(component)?);
    }
    let parent = chain.last().unwrap_or(root);
    match parent.unlink_file(last) {
        Ok(())
        | Err(remuda_fdsafe::FdError {
            kind: FdErrorKind::Missing,
            ..
        }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Read the provenance marker for `transcript_name` through the pinned fd.
/// `Ok(None)`: no marker directory or marker file (a corrupt marker also
/// reads as absent — fail closed).
fn read_staging_provenance(
    dest_dir_fd: &DirFd,
    transcript_name: &str,
) -> std::io::Result<Option<StagingProvenance>> {
    let marker_dir = match dest_dir_fd.subdir(STAGING_MARKER_DIR.as_bytes()) {
        Ok(dir) => dir,
        Err(error) if error.kind == FdErrorKind::Missing => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let marker_name = marker_filename(transcript_name);
    let leaf = match marker_dir.open_regular_leaf(marker_name.as_bytes()) {
        Ok(leaf) => leaf,
        Err(error) if error.kind == FdErrorKind::Missing => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let body = {
        use std::io::Read;
        let mut buf = String::new();
        (&leaf.file).read_to_string(&mut buf)?;
        buf
    };
    let marker = serde_json::from_str::<StagingProvenance>(&body)
        .ok()
        .filter(|marker| marker.version == 1);
    Ok(marker)
}

/// Write the marker via an `O_EXCL` temp file in the marker directory and a
/// final `renameat`; a symlink or special file at either path is refused.
fn write_staging_provenance(
    dest_dir_fd: &DirFd,
    transcript_name: &str,
    provenance: &StagingProvenance,
) -> std::io::Result<()> {
    let marker_dir = dest_dir_fd.ensure_subdir(STAGING_MARKER_DIR.as_bytes())?;
    let marker_name = marker_filename(transcript_name);
    // An existing marker must be a real regular file.
    match marker_dir.classify_leaf(marker_name.as_bytes())? {
        Some(entry) if entry.kind != LeafKind::Regular => {
            return Err(invalid_input(format!(
                "staging provenance marker {} is not a regular file",
                marker_name
            )));
        }
        _ => {}
    }
    let tmp_name = format!(
        ".marker-tmp-{}-{}",
        transcript_name.trim_end_matches(".jsonl"),
        uuid::Uuid::new_v4().as_simple()
    );
    use std::io::Write;
    let body = serde_json::to_vec_pretty(provenance).map_err(std::io::Error::other)?;
    marker_dir
        .create_leaf_excl(tmp_name.as_bytes())?
        .write_all(&body)?;
    match marker_dir.rename(tmp_name.as_bytes(), &marker_dir, marker_name.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = marker_dir.unlink_file(tmp_name.as_bytes());
            Err(error.into())
        }
    }
}

/// `<session>.jsonl` → `<session>.json` marker file name.
fn marker_filename(transcript_name: &str) -> String {
    transcript_name
        .strip_suffix('l')
        .map_or_else(|| transcript_name.to_owned(), str::to_owned)
}

/// Pin the predecessor home (everything up to `projects`) as the trusted
/// anchor and walk `projects/<slug>` below it link-free; fall back to anchoring
/// the immediate parent for a transcript not under a `projects/` layout. This
/// refuses a symlinked `projects` or `<slug>` component instead of following
/// it into an outside tree.
fn source_project_dir_fd(
    source_abs: &Path,
    parts: &[std::ffi::OsString],
) -> std::io::Result<DirFd> {
    if let Some(projects_idx) = parts.iter().position(|p| p == "projects")
        && projects_idx > 0
    {
        let home: PathBuf = parts[..projects_idx].iter().collect();
        let below: PathBuf = parts[projects_idx..parts.len() - 1].iter().collect();
        let home_fd = DirFd::anchor_existing(&home).map_err(|error| {
            if error.kind == FdErrorKind::Missing {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "resume transcript not found at {}: the predecessor home does not exist",
                        source_abs.display()
                    ),
                )
            } else {
                error.into()
            }
        })?;
        return if below.as_os_str().is_empty() {
            Ok(home_fd)
        } else {
            home_fd.subpath(&below).map_err(std::io::Error::from)
        };
    }
    let parent = source_abs
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            invalid_input(format!(
                "resume transcript path has no parent directory: {}",
                source_abs.display()
            ))
        })?;
    DirFd::anchor_existing(parent).map_err(|error| {
        if error.kind == FdErrorKind::Missing {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "resume transcript not found at {}: the predecessor directory does not exist",
                    source_abs.display()
                ),
            )
        } else {
            error.into()
        }
    })
}

/// Make a path absolute against the process cwd without following symlinks
/// beyond what the kernel itself does when opening it.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// Compare two directories the way Claude resolves them (canonical, no symlinks).
fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// First cwd-bearing record's value, the first user timestamp, and an excerpt
/// of the first real user prompt. Operates on a bounded head read.
fn first_identity(head: &str) -> (Option<PathBuf>, Option<String>, String) {
    let mut cwd = None;
    let mut started = None;
    let mut excerpt = String::new();
    for line in head.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = value
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
        }
        if started.is_none()
            && value.get("type").and_then(serde_json::Value::as_str) == Some("user")
        {
            started = value
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            if excerpt.is_empty() {
                excerpt = first_user_text(&value);
            }
        }
        if cwd.is_some() && started.is_some() {
            break;
        }
    }
    (cwd, started, excerpt)
}

/// Pull a readable prompt string out of a `user` record's text content.
fn first_user_text(value: &serde_json::Value) -> String {
    let content = value.pointer("/message/content");
    let text = match content {
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().find_map(|block| {
            block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.trim().is_empty())
        }),
        _ => None,
    };
    text.unwrap_or("").trim().chars().take(120).collect()
}

/// First non-empty `cwd` field in a bounded head read.
fn scan_cwd(head: &str) -> Option<String> {
    head.lines().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
    })
}

/// Trim an RFC3339 timestamp to `YYYY-MM-DD HH:MM` for a compact option label.
fn short_ts(ts: &str) -> Option<String> {
    let date = ts.get(0..10)?;
    let time = ts.get(11..16)?;
    Some(format!("{date} {time}"))
}

/// Bytes from the head of a transcript worth reading to establish identity.
const HEAD_BYTES: u64 = 256 * 1024;

fn read_head(path: &Path, max: u64) -> String {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = vec![0_u8; max as usize];
    let read = file.read(&mut buf).unwrap_or(0);
    buf.truncate(read);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Incremental reader over one transcript file.
///
/// Holds a byte offset and hands back whole lines only, so a half-written tail
/// is read on the next poll rather than parsed as truncated JSON.
#[derive(Debug)]
pub struct TranscriptTail {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl TranscriptTail {
    /// Start at the beginning of `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: String::new(),
        }
    }

    /// File being followed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read whatever whole lines have been appended since the last call.
    ///
    /// A file that shrank was rotated or truncated, so reading restarts at 0.
    /// An open error (the bound file was deleted) is surfaced to the caller,
    /// which marks the binding degraded rather than silently rebinding.
    pub fn poll(&mut self) -> std::io::Result<Vec<String>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        let read = file.read_to_end(&mut buf)? as u64;
        self.offset += read;
        self.partial.push_str(&String::from_utf8_lossy(&buf));
        let mut lines = Vec::new();
        while let Some(index) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=index).collect();
            let line = line.trim_end_matches(['\n', '\r']).to_owned();
            if !line.is_empty() {
                lines.push(line);
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn project_dir_encoding_matches_claudes_own_naming() {
        assert_eq!(
            encode_project_dir(Path::new("/Users/x/Projects/remuda-wt/x-promote")),
            "-Users-x-Projects-remuda-wt-x-promote"
        );
        // Dots collapse the same way slashes do.
        assert_eq!(encode_project_dir(Path::new("/tmp/a.b")), "-tmp-a-b");
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut file = std::fs::File::create(path).expect("create");
        file.write_all(body.as_bytes()).expect("write");
    }

    fn slug_file(tmp: &Path, cwd: &Path, session: &str) -> PathBuf {
        let path = project_dir(tmp, cwd).join(format!("{session}.jsonl"));
        write(&path, "{}\n");
        path
    }

    const SID: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn an_explicit_argv_session_id_binds_exactly_and_never_falls_back() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = Path::new("/work/repo");
        let path = slug_file(tmp.path(), cwd, SID);
        // A busier, newer other session must not be adopted.
        let newer = "99999999-8888-4777-8666-555555555555";
        slug_file(tmp.path(), cwd, newer);
        assert_eq!(
            bind_by_session_id(tmp.path(), cwd, SID)
                .expect("bound")
                .path,
            path
        );
        assert_eq!(bind_by_session_id(tmp.path(), cwd, "missing"), None);
        assert_eq!(bind_by_session_id(tmp.path(), cwd, ""), None);
    }

    #[test]
    fn the_pid_file_binds_the_exact_foreground_pid_even_when_another_session_is_newer() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let correct = "aaaaaaaa-2222-4333-8444-555555555555";
        let busy_other = "bbbbbbbb-8888-4777-8666-555555555555";
        slug_file(tmp.path(), &cwd, busy_other);
        let correct_path = slug_file(tmp.path(), &cwd, correct);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // The other session keeps writing and is the newest mtime.
        let mut busy = std::fs::OpenOptions::new()
            .append(true)
            .open(project_dir(tmp.path(), &cwd).join(format!("{busy_other}.jsonl")))
            .expect("open");
        writeln!(busy, "{{}}").expect("write");
        // The foreground pid's own session file names the *correct* session.
        write(
            &tmp.path().join("sessions").join("4242.json"),
            &format!(
                r#"{{"pid":4242,"sessionId":"{correct}","cwd":{}}}"#,
                serde_json::json!(cwd.to_string_lossy())
            ),
        );
        let binding = bind_by_pid_file(tmp.path(), 4242, &cwd).expect("bound");
        assert_eq!(binding.path, correct_path);
        assert_eq!(binding.source, BindingSource::PidFile);
        // No pid file for another pid → nothing, never the newest file.
        assert_eq!(bind_by_pid_file(tmp.path(), 7777, &cwd), None);
    }

    #[test]
    fn a_pid_file_for_a_different_cwd_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let terminal = tmp.path().join("a");
        let other = tmp.path().join("b");
        std::fs::create_dir_all(&terminal).expect("mkdir");
        std::fs::create_dir_all(&other).expect("mkdir");
        let sid = "cccccccc-2222-4333-8444-555555555555";
        slug_file(tmp.path(), &other, sid);
        write(
            &tmp.path().join("sessions").join("9.json"),
            &format!(
                r#"{{"pid":9,"sessionId":"{sid}","cwd":{}}}"#,
                serde_json::json!(other.to_string_lossy())
            ),
        );
        assert_eq!(bind_by_pid_file(tmp.path(), 9, &terminal), None);
    }

    #[test]
    fn a_hook_binds_only_when_its_ppid_is_the_foreground_pid() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let path = cwd.join("session.jsonl");
        write(&path, "{}\n");
        let json = format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"{SID}","transcript_path":{},"cwd":{},"ppid":5150}}"#,
            serde_json::json!(path.to_string_lossy()),
            serde_json::json!(cwd.to_string_lossy()),
        );
        let report = SessionStartReport::from_stdin(&json).expect("parse");
        assert_eq!(report.ppid, Some(5150));
        assert!(report.bind(5150).is_some(), "own foreground pid binds");
        assert!(
            report.bind(9999).is_none(),
            "another session's hook is ignored"
        );
    }

    #[test]
    fn hook_stdin_tolerates_camel_case_and_a_missing_optional_ppid() {
        let report = SessionStartReport::from_stdin(&format!(
            r#"{{"hookEventName":"SessionStart","sessionId":"{SID}","transcriptPath":"/tmp/x.jsonl"}}"#
        ))
        .expect("parse");
        assert_eq!(report.session_id, SID);
        assert_eq!(report.ppid, None);
        assert!(SessionStartReport::from_stdin(r#"{"session_id":""}"#).is_err());
    }

    #[test]
    fn candidates_are_listed_for_manual_pick_and_never_auto_selected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let older = "dddddddd-1111-4333-8444-aaaaaaaaaaaa";
        let younger = "eeeeeeee-2222-4333-8444-bbbbbbbbbbbb";
        write(
            &project_dir(tmp.path(), &cwd).join(format!("{older}.jsonl")),
            &format!(
                "{}\n{}\n",
                serde_json::json!({"type":"user","timestamp":"2026-09-10T08:00:00.000Z","cwd":cwd.to_string_lossy(),"message":{"role":"user","content":"oldest prompt"}}),
                serde_json::json!({"type":"assistant","cwd":cwd.to_string_lossy()}),
            ),
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &project_dir(tmp.path(), &cwd).join(format!("{younger}.jsonl")),
            &format!(
                "{}\n",
                serde_json::json!({"type":"user","timestamp":"2026-09-14T09:30:00.000Z","cwd":cwd.to_string_lossy(),"message":{"role":"user","content":[{"type":"text","text":"the correct new session"}]}}),
            ),
        );
        let candidates = list_candidates(tmp.path(), &cwd);
        assert_eq!(
            candidates.len(),
            2,
            "both sessions are offered, neither chosen"
        );
        // Newest activity sorts first.
        assert_eq!(candidates[0].session_id, younger);
        assert_eq!(
            candidates[0].started_at.as_deref(),
            Some("2026-09-14T09:30:00.000Z")
        );
        assert_eq!(candidates[0].excerpt, "the correct new session");
        assert!(
            candidates[0]
                .label()
                .contains("eeeeeeee · 2026-09-14 09:30")
        );
        // Manual bind is exact and honors the human's choice even if older.
        let chosen = bind_manual(tmp.path(), &cwd, older).expect("manual bind");
        assert_eq!(chosen.source, BindingSource::Manual);
        assert!(chosen.path.ends_with(format!("{older}.jsonl")));
        assert_eq!(bind_manual(tmp.path(), &cwd, "does-not-exist"), None);
    }

    #[test]
    fn content_cwd_mismatch_is_detected_but_an_empty_head_is_indeterminate() {
        let tmp = tempfile::tempdir().expect("tmp");
        let right = tmp.path().join("right");
        let wrong = tmp.path().join("wrong");
        std::fs::create_dir_all(&right).expect("mkdir");
        std::fs::create_dir_all(&wrong).expect("mkdir");
        let path = tmp.path().join("t.jsonl");
        write(
            &path,
            &format!(
                "{}\n",
                serde_json::json!({"type":"user","cwd":wrong.to_string_lossy(),"message":{"role":"user","content":"x"}}),
            ),
        );
        assert!(!cwd_matches(&path, &right));
        assert!(cwd_matches(&path, &wrong));
        let empty = tmp.path().join("empty.jsonl");
        write(&empty, "");
        assert!(
            cwd_matches(&empty, &right),
            "no cwd record yet → not rejected"
        );
    }

    #[test]
    fn the_tail_yields_only_whole_lines_and_resumes_where_it_stopped() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"a\":1}\n{\"b\":2}\n{\"parti");
        let mut tail = TranscriptTail::new(path.clone());
        assert_eq!(tail.poll().expect("poll"), vec!["{\"a\":1}", "{\"b\":2}"]);
        // The partial line is held back until its newline arrives.
        assert_eq!(tail.poll().expect("poll"), Vec::<String>::new());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        file.write_all(b"al\":3}\n").expect("write");
        assert_eq!(tail.poll().expect("poll"), vec!["{\"partial\":3}"]);
    }

    #[test]
    fn truncation_restarts_the_tail_at_zero() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"a\":1}\n{\"b\":2}\n");
        let mut tail = TranscriptTail::new(path.clone());
        assert_eq!(tail.poll().expect("poll").len(), 2);
        write(&path, "{\"c\":3}\n");
        assert_eq!(tail.poll().expect("poll"), vec!["{\"c\":3}"]);
    }

    // --- c-resumehome: staging a predecessor conversation for `--resume` ---

    fn write_file(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    /// Every regular file below `root` (symlinks deliberately not followed —
    /// a staged tree must contain none).
    fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let entry = entry.expect("entry");
                let ty = entry.file_type().expect("file type");
                let path = entry.path();
                if ty.is_dir() {
                    stack.push(path);
                } else if ty.is_file() {
                    out.push(path);
                }
            }
        }
        out
    }

    fn transcript_layout(home: &std::path::Path, cwd: &std::path::Path, session: &str) -> PathBuf {
        project_dir(home, cwd).join(format!("{session}.jsonl"))
    }

    /// A SHORT, randomised, exclusive 0700 scratch dir under `/tmp` with Drop
    /// cleanup. Needed so a unix socket bound below it stays within macOS
    /// `sun_path` (104 bytes); the long std `tempfile()` path does not.
    struct ShortTmp {
        path: PathBuf,
    }
    impl ShortTmp {
        fn new() -> ShortTmp {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SEQ: AtomicU32 = AtomicU32::new(0);
            use std::os::unix::fs::DirBuilderExt;
            for _ in 0..16 {
                let name = format!(
                    "rt{:x}{:x}",
                    std::process::id() ^ SEQ.fetch_add(1, Ordering::Relaxed),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0)
                );
                let path = PathBuf::from("/tmp").join(name);
                if std::fs::DirBuilder::new().mode(0o700).create(&path).is_ok() {
                    return ShortTmp { path };
                }
            }
            panic!("no short scratch dir");
        }
    }
    impl Drop for ShortTmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The project slug Claude itself uses: it derives the directory from the
    /// PHYSICAL cwd (`getcwd()` returns `/private/var/...` when the shell is in
    /// a `/var/...` symlink on macOS). Every test that builds a Claude-shaped
    /// source/destination tree must use THIS, not `encode_project_dir(&cwd)` on
    /// a possibly-logical temp path, or source-side and destination-side slugs
    /// disagree on macOS.
    fn slug_for(cwd: &std::path::Path) -> String {
        let physical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        encode_project_dir(&physical)
    }

    #[test]
    fn resume_staging_copies_transcript_session_dir_and_memory() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old-native-home");
        let new_home = tmp.path().join("new-native-home");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000aa";

        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{\"type\":\"user\"}\n");
        write_file(
            &old_home
                .join("projects")
                .join(slug_for(&cwd))
                .join(session)
                .join("subagents")
                .join("side.jsonl"),
            "{\"side\":true}\n",
        );
        write_file(
            &old_home
                .join("projects")
                .join(slug_for(&cwd))
                .join("memory")
                .join("MEMORY.md"),
            "project memory\n",
        );

        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");

        assert_eq!(
            staged.transcript,
            transcript_layout(&new_home, &cwd, session)
        );
        assert!(staged.transcript.is_file());
        assert_eq!(
            std::fs::read_to_string(&staged.transcript).unwrap(),
            "{\"type\":\"user\"}\n"
        );
        let session_dir = staged
            .sidecar_dirs
            .iter()
            .find(|dir| dir.ends_with(session))
            .expect("session sidecar dir");
        assert!(session_dir.join("subagents/side.jsonl").is_file());
        let memory_dir = staged
            .sidecar_dirs
            .iter()
            .find(|dir| dir.ends_with("memory"))
            .expect("memory sidecar dir");
        assert!(memory_dir.join("MEMORY.md").is_file());
    }

    #[test]
    fn resume_staging_never_mutates_the_predecessor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000bb";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "original\n");

        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");

        // The resumed process appends to its own copy; the predecessor file
        // must stay exactly as it was (no shared inode, no propagated write).
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&staged.transcript)
                .unwrap(),
            "continued"
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "original\n",
            "predecessor transcript must stay frozen"
        );
        assert_ne!(
            std::fs::metadata(&source).unwrap().len(),
            std::fs::metadata(&staged.transcript).unwrap().len(),
            "a copy, not a hardlink"
        );
    }

    #[test]
    fn resume_staging_chains_across_generations() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000cc";
        let home0 = tmp.path().join("home0");
        let home1 = tmp.path().join("home1");
        let home2 = tmp.path().join("home2");

        let transcript0 = transcript_layout(&home0, &cwd, session);
        write_file(&transcript0, "turn-0\n");
        let staged1 = stage_for_resume(&transcript0, &home1, &cwd, session).expect("stage 1");
        write_file(&staged1.transcript, "turn-0\nturn-1\n");
        // Second-generation resume: source is the first child's home.
        let staged2 =
            stage_for_resume(&staged1.transcript, &home2, &cwd, session).expect("stage 2");
        assert_eq!(
            std::fs::read_to_string(&staged2.transcript).unwrap(),
            "turn-0\nturn-1\n",
            "the grandchild inherits the newest generation's transcript"
        );
        assert_eq!(
            std::fs::read_to_string(&transcript0).unwrap(),
            "turn-0\n",
            "the original instance stays untouched"
        );
    }

    #[test]
    fn resume_staging_is_a_noop_when_source_already_lands_in_target() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000dd";
        let transcript = transcript_layout(&home, &cwd, session);
        write_file(&transcript, "{}\n");

        let staged = stage_for_resume(&transcript, &home, &cwd, session).expect("stage");
        assert_eq!(staged.transcript, transcript);
        assert!(staged.sidecar_dirs.is_empty());
    }

    #[test]
    fn resume_staging_refuses_an_unprovenanced_existing_destination() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000ee";
        let source = transcript_layout(&old_home, &cwd, session);
        let dest = transcript_layout(&new_home, &cwd, session);
        write_file(&source, "source\n");
        // A stale same-session file with NO Remuda provenance marker.
        write_file(&dest, "stale conversation from another launch\n");

        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("an unprovenanced destination is a conflict, not a silent keep");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("staging provenance"),
            "the conflict names the missing provenance: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "stale conversation from another launch\n",
            "the foreign file is never overwritten"
        );
    }

    #[test]
    fn resume_staging_keeps_a_provenanced_destination_and_its_appended_turns() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000f0";
        let source = transcript_layout(&old_home, &cwd, session);
        let dest = transcript_layout(&new_home, &cwd, session);
        write_file(&source, "source\n");

        // First staging publishes the transcript plus provenance marker.
        let first = stage_for_resume(&source, &new_home, &cwd, session).expect("first stage");
        assert!(
            dest.parent()
                .unwrap()
                .join(STAGING_MARKER_DIR)
                .join(format!("{session}.json"))
                .is_file(),
            "a provenance marker is published"
        );
        // The resumed instance appended its own turn to its staged copy.
        write_file(&dest, "source\nchild turn\n");

        // A retried build keeps the child's copy exactly.
        let again = stage_for_resume(&source, &new_home, &cwd, session).expect("retry stage");
        assert_eq!(again.transcript, first.transcript);
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "source\nchild turn\n",
            "a retried build must not clobber the child's own transcript"
        );
        assert!(
            dest.parent()
                .unwrap()
                .read_dir()
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".transcript-tmp-")),
            "temp staging files are never left behind"
        );
    }

    #[test]
    fn resume_staging_enforces_the_size_file_and_depth_limits() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000f2";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "012345678\n");

        // Bytes: the transcript alone exceeds the cap.
        let tight_bytes = StageLimits {
            max_bytes: 4,
            max_files: 100,
            max_depth: 32,
        };
        let error = stage_for_resume_with_limits(
            &source,
            &tmp.path().join("new-bytes"),
            &cwd,
            session,
            tight_bytes,
        )
        .expect_err("an oversized transcript is refused before publishing");
        assert!(
            error.to_string().contains("size limit exceeded"),
            "unexpected message: {error}"
        );

        // File count: transcript + two sidecar files, capped at two.
        let project = source.parent().unwrap();
        write_file(&project.join(session).join("a.jsonl"), "a\n");
        write_file(&project.join("memory").join("M.md"), "m\n");
        let tight_files = StageLimits {
            max_bytes: 1 << 20,
            max_files: 2,
            max_depth: 32,
        };
        let error = stage_for_resume_with_limits(
            &source,
            &tmp.path().join("new-files"),
            &cwd,
            session,
            tight_files,
        )
        .expect_err("too many files is refused");
        assert!(
            error.to_string().contains("file count limit exceeded"),
            "unexpected message: {error}"
        );

        // Depth: a sidecar tree nested deeper than the cap.
        let deep_home = tmp.path().join("old-deep");
        let deep_source = transcript_layout(&deep_home, &cwd, session);
        write_file(&deep_source, "x\n");
        write_file(
            &deep_home
                .join("projects")
                .join(slug_for(&cwd))
                .join(session)
                .join("a/b/c/deep.jsonl"),
            "deep\n",
        );
        let tight_depth = StageLimits {
            max_bytes: 1 << 20,
            max_files: 100,
            max_depth: 1,
        };
        let error = stage_for_resume_with_limits(
            &deep_source,
            &tmp.path().join("new-depth"),
            &cwd,
            session,
            tight_depth,
        )
        .expect_err("an overly deep sidecar tree is refused");
        assert!(
            error.to_string().contains("depth limit exceeded"),
            "unexpected message: {error}"
        );
    }

    #[test]
    fn resume_staging_within_default_limits_copies_everything() {
        // Sanity: the production caps never bite a realistically sized resume
        // (the main copy/chain tests also exercise the default path).
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000f3";
        let source = transcript_layout(&tmp.path().join("old"), &cwd, session);
        write_file(&source, "real conversation\n");
        let project = source.parent().unwrap();
        write_file(&project.join(session).join("sub.jsonl"), "sub\n");
        write_file(&project.join("memory/MEMORY.md"), "memory\n");

        let staged =
            stage_for_resume(&source, &tmp.path().join("new"), &cwd, session).expect("stage");
        assert!(staged.transcript.is_file());
        assert_eq!(staged.sidecar_dirs.len(), 2);
    }

    #[test]
    fn resume_staging_conflicts_when_provenance_names_a_different_predecessor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000f1";
        let source = transcript_layout(&old_home, &cwd, session);
        let dest = transcript_layout(&new_home, &cwd, session);
        write_file(&source, "v2 source\n");

        // A previous staging of a DIFFERENT predecessor byte-content.
        let other_home = tmp.path().join("other");
        let other = transcript_layout(&other_home, &cwd, session);
        write_file(&other, "v1 source\n");
        stage_for_resume(&other, &new_home, &cwd, session).expect("stage v1");

        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("provenance for a different predecessor is a conflict");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("provenance does not match"),
            "unexpected message: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "v1 source\n",
            "the conflict leaves the previously staged conversation in place"
        );
    }

    #[test]
    fn resume_staging_missing_transcript_is_a_clear_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let missing = tmp
            .path()
            .join("gone/01993ab0-0000-7000-8000-0000000000ff.jsonl");

        let session = "01993ab0-0000-7000-8000-0000000000ff";
        let error = stage_for_resume(&missing, &tmp.path().join("new"), &cwd, session)
            .expect_err("a missing transcript must error, not silently launch");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains("resume transcript not found"),
            "unexpected message: {error}"
        );
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "the message names the path it looked for: {error}"
        );
    }

    #[test]
    fn safe_session_id_accepts_uuid_shaped_tokens_and_rejects_traversal() {
        assert!(is_safe_session_id("01993ab0-0000-7000-8000-0000000000aa"));
        assert!(is_safe_session_id("  abc-123_DEF  "));
        for dangerous in [
            "",
            "  ",
            ".",
            "..",
            "../evil",
            "foo/bar",
            "foo/../../etc/passwd",
            r"foo\bar",
            "a/b",
            "x.jsonl",
            "with space",
            &"x".repeat(65),
        ] {
            assert!(
                !is_safe_session_id(dangerous),
                "{dangerous:?} must not be a safe session id"
            );
        }
    }

    #[test]
    fn resume_staging_rejects_a_traversal_shaped_session_id() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000aa";
        let source = transcript_layout(&home, &cwd, session);
        write_file(&source, "{}\n");

        for dangerous in ["../../../../tmp/evil", "..%2fevil", "a/b", "..", "x.txt"] {
            let error = stage_for_resume(&source, &home, &cwd, dangerous)
                .expect_err("traversal ids must be rejected before any path is built");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                error.to_string().contains("file-name component"),
                "unexpected message: {error}"
            );
        }
        // Nothing was created outside the source layout.
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "{}\n",
            "a rejected staging touches nothing"
        );
    }

    #[test]
    fn exact_session_binds_reject_traversal_tokens() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        assert!(bind_by_session_id(&home, &cwd, "../escape").is_none());
        assert!(bind_manual(&home, &cwd, "../escape").is_none());
        assert!(bind_by_session_id(&home, &cwd, "a/b").is_none());
        assert!(bind_manual(&home, &cwd, ".").is_none());
    }

    #[test]
    fn resume_staging_renamed_transcript_finds_sidecars_by_session_id_first() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000af";
        // A promoted/renamed transcript, outside any managed home and carrying a
        // file name that is NOT the session id.
        let external_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&external_dir).expect("external dir");
        let source = external_dir.join("promoted-conversation.jsonl");
        write_file(&source, "{}\n");
        // The real sidecars keep the native session id; a stale stem-named dir
        // also exists and must NOT be the one staged.
        write_file(
            &external_dir.join(session).join("subagents/real-side.jsonl"),
            "{\"side\":\"real\"}\n",
        );
        write_file(
            &external_dir
                .join("promoted-conversation")
                .join("stale-side.jsonl"),
            "{\"side\":\"stale\"}\n",
        );

        let new_home = tmp.path().join("new-home");
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");

        let dest_project = staged.transcript.parent().unwrap();
        assert!(
            dest_project
                .join(session)
                .join("subagents/real-side.jsonl")
                .is_file(),
            "sidecars under <dir>/<S>/ are staged for a renamed transcript"
        );
        assert!(
            !dest_project.join(session).join("stale-side.jsonl").exists(),
            "the file-stem dir is only a fallback when no <S>/ dir exists"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_staging_skips_and_reports_every_sidecar_symlink_never_resolving_it() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000ac";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        let project = source.parent().unwrap();

        // A real regular sidecar (must still be staged) beside three links.
        let subagents = project.join(session).join("subagents");
        std::fs::create_dir_all(&subagents).expect("mkdir");
        write_file(&subagents.join("real-side.jsonl"), "{\"real\":true}\n");
        write_file(&project.join("memory/MEMORY.md"), "project memory\n");
        // In-tree link that WOULD resolve to a regular file under round 2's
        // hop-following rules.
        symlink("../../memory/MEMORY.md", subagents.join("memory-link.md")).expect("internal link");
        // Multi-hop in-tree chain.
        write_file(&project.join(session).join("inner.jsonl"), "inner-target\n");
        symlink("inner.jsonl", project.join(session).join("mid.jsonl")).expect("mid link");
        symlink("mid.jsonl", project.join(session).join("chain.jsonl")).expect("chain link");

        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");

        // The regular sidecar is copied.
        assert!(
            staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("subagents/real-side.jsonl")
                .is_file()
        );
        assert!(
            staged
                .transcript
                .parent()
                .unwrap()
                .join("memory/MEMORY.md")
                .is_file()
        );
        // Every link is skipped — none is recreated, neither as a link nor as
        // an independent regular file.
        for rel in [
            format!("{session}/subagents/memory-link.md"),
            format!("{session}/mid.jsonl"),
            format!("{session}/chain.jsonl"),
        ] {
            assert!(
                !staged.transcript.parent().unwrap().join(&rel).exists(),
                "{rel} must not be staged at all"
            );
        }
        let reports = staged.skipped.join("|");
        assert!(
            reports.contains("symlink:"),
            "links are reported: {reports}"
        );
        assert!(
            reports.contains(&format!("{session}/subagents/memory-link.md")),
            "{reports}"
        );
        assert!(
            reports.contains(&format!("{session}/chain.jsonl")),
            "{reports}"
        );
        // The in-tree target bytes must not leak in under a link's name.
        assert!(
            !staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("subagents/memory-link.md")
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_staging_skips_escaping_broken_and_directory_links_without_following_them() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000ad";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        let project = source.parent().unwrap();
        let side = project.join(session);
        std::fs::create_dir_all(&side).expect("mkdir");
        write_file(&side.join("real.jsonl"), "real sidecar\n");

        // Outside the project tree: relative traversal and an absolute link.
        let outside = tmp.path().join("outside.txt");
        write_file(&outside, "OUTSIDE-SECRET-BYTES\n");
        symlink(&outside, side.join("escaping-abs")).expect("abs link");
        symlink("../../../../outside.txt", side.join("escaping-rel")).expect("rel link");
        // Multi-hop escape: the first hop looks internal, the second leaves.
        symlink("../../hop2", side.join("hop1")).expect("hop1");
        symlink(&outside, project.join("hop2")).expect("hop2 leaves tree");
        // Broken link.
        symlink("does-not-exist", side.join("broken")).expect("broken link");
        // Directory link, internal.
        std::fs::create_dir_all(project.join("realdir")).expect("realdir");
        symlink("realdir", side.join("dir-link")).expect("dir link");

        // Round 3: non-regular leaves are skipped and reported, never opened;
        // the transcript and regular sidecars still stage.
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");
        assert!(staged.transcript.is_file());
        assert!(
            staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("real.jsonl")
                .is_file(),
            "the legitimate regular sidecar is staged"
        );
        for name in ["escaping-abs", "escaping-rel", "hop1", "broken", "dir-link"] {
            assert!(
                !staged
                    .transcript
                    .parent()
                    .unwrap()
                    .join(session)
                    .join(name)
                    .exists(),
                "{name} must not exist in the staged tree"
            );
        }
        // The outside content must not appear ANYWHERE in the staged tree.
        for entry in walkdir(staged.transcript.parent().unwrap()) {
            let body = std::fs::read(&entry).unwrap_or_default();
            assert!(
                !body.windows(21).any(|w| w == b"OUTSIDE-SECRET-BYTES"),
                "outside bytes leaked into {}",
                entry.display()
            );
        }
        assert_eq!(
            staged.skipped.len(),
            5,
            "all five links reported: {:?}",
            staged.skipped
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_staging_skips_a_symlinked_sidecar_root_and_never_reads_through_it() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000ae";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        let project = source.parent().unwrap();
        let elsewhere = tmp.path().join("elsewhere-memory");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        write_file(&elsewhere.join("SECRET.md"), "ELSEWHERE-MEMORY-SECRET\n");
        symlink(&elsewhere, project.join("memory")).expect("symlinked memory root");

        // The root is a link: skipped and reported, but the transcript stages.
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");
        assert!(staged.transcript.is_file());
        assert!(
            !staged
                .transcript
                .parent()
                .unwrap()
                .join("memory/SECRET.md")
                .exists(),
            "a symlinked memory root is never read through"
        );
        assert!(
            staged.skipped.iter().any(|entry| entry == "symlink:memory"),
            "the root link is reported: {:?}",
            staged.skipped
        );
    }

    // ===================== c-resumehome round 3 regressions =====================

    /// Item 2: the exact round-2 escape — `<session>/exfil -> ../bridge/passwd`
    /// with `bridge -> /etc` — used to pass the lexical containment check
    /// because a stat followed the intermediate component. The fd walk never
    /// resolves any sidecar link: the exfil entry is skipped and reported, and
    /// no byte from `/etc/passwd` lands in the staged tree.
    #[cfg(unix)]
    #[test]
    fn round3_an_in_tree_hop_to_an_outside_target_is_skipped_and_never_read() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b3";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        let project = source.parent().unwrap();
        let side = project.join(session);
        std::fs::create_dir_all(&side).expect("side dir");
        symlink("/etc", project.join("bridge")).expect("bridge -> /etc");
        symlink("../bridge/passwd", side.join("exfil")).expect("in-tree hop");

        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("staging succeeds");
        assert!(staged.transcript.is_file(), "the transcript still stages");
        let exfil = staged
            .transcript
            .parent()
            .unwrap()
            .join(session)
            .join("exfil");
        assert!(!exfil.exists(), "the exfil link is never reproduced");
        assert!(
            staged.skipped.iter().any(|entry| entry.contains("exfil")),
            "the hop is reported: {:?}",
            staged.skipped
        );
        for entry in walkdir(staged.transcript.parent().unwrap()) {
            let body = std::fs::read(&entry).unwrap_or_default();
            assert!(
                !body.windows(5).any(|w| w == b"root:"),
                "/etc/passwd bytes leaked into {}",
                entry.display()
            );
        }
    }

    /// Item 1: a symlink at ANY destination level is refused before a file is
    /// created through it — the projects slug dir, a sidecar directory, and the
    /// transcript leaf.
    #[cfg(unix)]
    #[test]
    fn round3_destination_symlinks_at_every_level_are_refused() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b1";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");

        // (a) `projects` in the new home is a symlink: never create_dir_all
        // through it (the sibling dir must not receive the slug).
        let new_a = tmp.path().join("new-a");
        std::fs::create_dir_all(&new_a).expect("home");
        let project_target = tmp.path().join("projects-elsewhere-a");
        std::fs::create_dir_all(&project_target).expect("target dir");
        symlink(&project_target, new_a.join("projects")).expect("projects link");
        let error = stage_for_resume(&source, &new_a, &cwd, session).expect_err("projects link");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{error}"
        );
        assert!(
            !project_target.join(slug_for(&cwd)).exists(),
            "the slug dir must not be created through the symlinked projects"
        );

        // (b) The destination transcript leaf is a symlink at the predecessor
        // (would previously pass same_path-style acceptance).
        let new_b = tmp.path().join("new-b");
        let dest = transcript_layout(&new_b, &cwd, session);
        std::fs::create_dir_all(dest.parent().unwrap()).expect("slug");
        symlink(&source, &dest).expect("dest transcript link");
        let error = stage_for_resume(&source, &new_b, &cwd, session)
            .expect_err("a symlinked destination transcript is refused first");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{error}"
        );
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    /// Item 1: an existing symlinked destination SIDECAR is never kept or
    /// written through, even though a regular identical sidecar would be kept.
    #[cfg(unix)]
    #[test]
    fn round3_an_existing_destination_sidecar_symlink_is_refused_not_kept() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b2";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        write_file(
            &source.parent().unwrap().join(session).join("side.jsonl"),
            "real side\n",
        );
        // First, a clean stage so the home has a real slug dir + transcript.
        stage_for_resume(&source, &new_home, &cwd, session).expect("first stage");
        // Then swap in a symlink where the sidecar would land.
        let dest_side = transcript_layout(&new_home, &cwd, session)
            .parent()
            .unwrap()
            .join(session)
            .join("side.jsonl");
        std::fs::create_dir_all(dest_side.parent().unwrap()).expect("dest side dir");
        // Attacker swaps the previously-copied sidecar for a symlink.
        std::fs::remove_file(&dest_side).expect("remove the good sidecar");
        let outside = tmp.path().join("outside-side.txt");
        write_file(&outside, "side-target\n");
        symlink(&outside, &dest_side).expect("dest sidecar link");
        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("a symlinked destination sidecar aborts staging");
        assert!(
            error.to_string().contains("symlink")
                || error.kind() == std::io::ErrorKind::PermissionDenied,
            "{error}"
        );
        assert_eq!(
            std::fs::symlink_metadata(&outside).unwrap().len(),
            "side-target\n".len() as u64,
            "the link target is never written through"
        );
    }

    /// Item 3: provenance/streaming reads the OPENED fd. Replace the source
    /// path with different bytes AFTER the fd is opened; the stream must still
    /// see the original file, and its sha must match the original bytes — not a
    /// re-read of the (now different) path.
    #[test]
    fn round3_streaming_hashes_the_opened_fd_not_a_path_reread() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = remuda_fdsafe::DirFd::anchor_existing(tmp.path()).expect("root");
        root.create_leaf_excl(b"t.jsonl")
            .expect("create")
            .write_all(b"original-bytes\n")
            .expect("write original");
        // Open the leaf, then swap the path away (rename a new file over it).
        let leaf = root.open_regular_leaf(b"t.jsonl").expect("open leaf");
        let mut temp_name = root.create_leaf_excl(b"replacement").expect("temp");
        temp_name
            .write_all(b"REPLACED-BYTES\n")
            .expect("write replacement");
        root.rename(b"replacement", &root, b"t.jsonl")
            .expect("swap path");
        let mut reader = leaf.file;
        let mut out = root.create_leaf_excl(b"copy").expect("copy");
        let streamed = stream_hashed(
            &mut reader,
            &mut out,
            "original-bytes\n".len() as u64,
            DEFAULT_STAGE_LIMITS.max_bytes,
        )
        .expect("stream from held fd");
        let mut got = Vec::new();
        use std::io::Read;
        root.open_regular_leaf(b"copy")
            .expect("reopen copy")
            .file
            .read_to_end(&mut got)
            .expect("read copy");
        assert_eq!(&got, b"original-bytes\n", "the held fd is the old inode");
        let expected = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(b"original-bytes\n"))
        };
        assert_eq!(
            streamed.sha256, expected,
            "provenance hashes the streamed bytes, not the swapped path"
        );
        assert_eq!(streamed.bytes, "original-bytes\n".len() as u64);
    }

    /// Item 4: a FIFO sidecar can never block staging or be copied: it is
    /// classified Other before any open, skipped, and reported.
    #[cfg(unix)]
    #[test]
    fn round3_a_fifo_sidecar_is_skipped_without_blocking() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b4";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        let side_dir = source.parent().unwrap().join(session);
        std::fs::create_dir_all(&side_dir).expect("side dir");
        nix::unistd::mkfifo(
            &side_dir.join("hose"),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .expect("mkfifo");
        write_file(&side_dir.join("real.jsonl"), "real\n");

        let started = std::time::Instant::now();
        let staged = stage_for_resume(&source, &new_home, &cwd, session)
            .expect("fifo must not abort staging");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a FIFO never blocks"
        );
        assert!(staged.transcript.is_file());
        assert!(
            staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("real.jsonl")
                .is_file()
        );
        assert!(
            !staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("hose")
                .exists(),
            "the FIFO is not reproduced"
        );
        assert!(
            staged.skipped.iter().any(|entry| entry.contains("hose")),
            "the FIFO is reported: {:?}",
            staged.skipped
        );
    }

    /// Item 5: a stale partial sidecar sitting under its FINAL name (a crashed
    /// copy from an older staging design) is a conflict — it is never silently
    /// kept — while a leftover private temp tree is discarded.
    #[test]
    fn round3_partial_final_sidecar_conflicts_and_stale_temp_tree_is_discarded() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b5";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        write_file(
            &source.parent().unwrap().join(session).join("full.jsonl"),
            "the complete sidecar contents\n",
        );
        // Stage once to establish the slug and transcript.
        stage_for_resume(&source, &new_home, &cwd, session).expect("first stage");
        let dest_project = transcript_layout(&new_home, &cwd, session)
            .parent()
            .unwrap()
            .to_path_buf();
        // (a) A crashed old-style copy left a truncated file under its name.
        let final_side = dest_project.join(session).join("full.jsonl");
        std::fs::remove_file(&final_side).expect("remove the good copy");
        write_file(&final_side, "the comp");
        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("a partial same-name sidecar is a conflict, not a kept file");
        assert!(
            error
                .to_string()
                .contains("already exists with different content"),
            "{error}"
        );
        // (b) A leftover private temp tree from a killed attempt is discarded,
        // and the retry then succeeds.
        let stale = dest_project.join(".stage-crashed-attempt");
        std::fs::create_dir_all(stale.join(session)).expect("stale temp tree");
        write_file(&stale.join(session).join("partial.jsonl"), "x");
        write_file(&final_side, "the complete sidecar contents\n");
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("retry succeeds");
        assert!(staged.transcript.is_file());
        assert!(
            !dest_project
                .iter()
                .any(|name| name.to_string_lossy().starts_with(".stage-")),
            "every private temp tree is gone after staging"
        );
    }

    /// Item 6: a second attempt after a byte-limit or file-limit failure is not
    /// given the leftovers' budget — the budget starts at zero each time and
    /// the failed temp tree is gone. Then a retry within limits succeeds.
    #[test]
    fn round3_repeat_staging_after_a_limit_failure_charges_from_scratch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b6";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "ten-bytes\n");
        write_file(
            &source.parent().unwrap().join(session).join("a.jsonl"),
            "aaaaa\n",
        );
        write_file(
            &source.parent().unwrap().join(session).join("b.jsonl"),
            "bbbbb\n",
        );

        // Byte limit smaller than the 10-byte transcript fails before publish.
        let tight_bytes = StageLimits {
            max_bytes: 5,
            max_files: 100,
            max_depth: 32,
        };
        let error = stage_for_resume_with_limits(&source, &new_home, &cwd, session, tight_bytes)
            .expect_err("byte cap");
        assert!(error.to_string().contains("size limit exceeded"), "{error}");
        let dest_project = transcript_layout(&new_home, &cwd, session)
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(
            !dest_project
                .iter()
                .any(|name| name.to_string_lossy().starts_with(".stage-")),
            "the failed attempt's temp tree was discarded"
        );
        assert!(
            !transcript_layout(&new_home, &cwd, session).is_file(),
            "the transcript was never published"
        );

        // File cap: transcript + 2 sidecars = 3 files, cap 2 fails.
        let tight_files = StageLimits {
            max_bytes: 1 << 20,
            max_files: 2,
            max_depth: 32,
        };
        let error = stage_for_resume_with_limits(&source, &new_home, &cwd, session, tight_files)
            .expect_err("file cap");
        assert!(
            error.to_string().contains("file count limit exceeded"),
            "{error}"
        );

        // Retry within limits succeeds.
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("retry");
        assert_eq!(
            std::fs::read_to_string(&staged.transcript).unwrap(),
            "ten-bytes\n"
        );
        assert!(
            staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("a.jsonl")
                .is_file()
        );
    }

    /// Item 7: an oversized transcript is refused before its bytes are read —
    /// and no private temp tree survives the refusal.
    #[test]
    fn round3_an_oversized_transcript_is_refused_before_streaming() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b7";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, &"x".repeat(4096));
        let tight = StageLimits {
            max_bytes: 16,
            max_files: 10,
            max_depth: 32,
        };
        let error = stage_for_resume_with_limits(&source, &new_home, &cwd, session, tight)
            .expect_err("oversized refused");
        assert!(error.to_string().contains("size limit exceeded"), "{error}");
        let dest_project = transcript_layout(&new_home, &cwd, session)
            .parent()
            .unwrap()
            .to_path_buf();
        if dest_project.exists() {
            assert!(
                !dest_project
                    .iter()
                    .any(|name| name.to_string_lossy().starts_with(".stage-")),
                "no temp tree survives an oversized-transcript refusal"
            );
        }
    }

    /// Round 4 item 2: a HARDlink to the predecessor placed in a DIFFERENT
    /// destination directory is not an inherited-home no-op — it is streamed
    /// from the open source fd into a fresh O_EXCL file and lands as an
    /// independent regular file (the source stays untouched).
    #[cfg(unix)]
    #[test]
    fn round4_cross_home_hardlink_is_copied_not_treated_as_same_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b7";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "hardlink-content\n");

        // First stage so the destination directory + marker exist, then replace
        // the destination transcript with a hardlink into the predecessor file
        // located in a DIFFERENT directory.
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("first stage");
        let dest = staged.transcript;
        std::fs::remove_file(&dest).expect("remove staged transcript");
        // Create the hardlink by the low-level linkat-less std path: std::fs::hard_link.
        std::fs::hard_link(&source, &dest).expect("cross-dir hardlink");

        // The hardlink now shares the source inode, but the destination and
        // source project DIRECTORIES differ. A second stage must not no-op; it
        // must replace the link with an independent copy.
        let staged2 = stage_for_resume(&source, &new_home, &cwd, session).expect("second stage");
        assert_eq!(
            std::fs::read_to_string(&staged2.transcript).unwrap(),
            "hardlink-content\n"
        );
        // Breaking the link: mutate the copy destination; the source inode must
        // be independent (the staged file is no longer a hardlink).
        let src_ino_before = inode_of(&source);
        let dst_ino_after = inode_of(&staged2.transcript);
        assert_ne!(
            src_ino_before, dst_ino_after,
            "a cross-directory hardlink must be replaced by an independent file, not no-op'd"
        );
    }

    /// Same-file no-op still applies when the destination directory IS the
    /// source project directory (the inherited-home case).
    #[cfg(unix)]
    #[test]
    fn round4_same_directory_same_inode_is_still_a_noop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b8";
        let source = transcript_layout(&home, &cwd, session);
        write_file(&source, "inherited\n");
        let staged = stage_for_resume(&source, &home, &cwd, session).expect("inherited no-op");
        assert_eq!(
            std::fs::read_to_string(&staged.transcript).unwrap(),
            "inherited\n"
        );
        // No marker is written for the no-op.
        assert!(
            !source.parent().unwrap().join(".remuda-staging").exists(),
            "inherited-home no-op needs no staging marker"
        );
    }

    /// Round 4 item 5: a unix socket sidecar is classified Other, skipped and
    /// reported, and staging still succeeds. Uses a SHORT scratch dir and short
    /// cwd so the socket bind fits in macOS `sun_path` (104 bytes).
    #[cfg(unix)]
    #[test]
    fn round4_socket_and_device_sidecars_are_skipped_and_reported() {
        use std::os::unix::net::UnixListener;
        let tmp = ShortTmp::new();
        let old_home = tmp.path.join("o");
        let new_home = tmp.path.join("n");
        let cwd = tmp.path.join("w");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000b9";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        let side = source.parent().unwrap().join(session);
        std::fs::create_dir_all(&side).expect("side dir");
        let socket_path = side.join("sock");
        assert!(
            socket_path.as_os_str().len() <= 100,
            "socket path must fit macOS sun_path: {}",
            socket_path.display()
        );
        let _listener = UnixListener::bind(&socket_path).expect("bind socket");
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");
        assert!(staged.transcript.is_file());
        assert!(
            !staged
                .transcript
                .parent()
                .unwrap()
                .join(session)
                .join("sock")
                .exists(),
            "socket sidecar is never copied"
        );
        assert!(
            staged.skipped.iter().any(|entry| entry.contains("sock")),
            "socket reported: {:?}",
            staged.skipped
        );
    }

    /// Round 4 item 9: a crash between publishing the transcript and writing
    /// the marker leaves a stranded, byte-identical transcript; the retry
    /// validates the bytes and recovers instead of refusing.
    #[test]
    fn round4_interrupted_publish_without_marker_is_recoverable() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000ba";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "recoverable-turn\n");
        // First attempt injects the post-publish/pre-marker failure.
        // First attempt injects the post-publish/pre-marker failure.
        PUBLISH_NO_MARKER_SEAM.with(|seam| seam.set(true));
        let first = stage_for_resume(&source, &new_home, &cwd, session);
        PUBLISH_NO_MARKER_SEAM.with(|seam| seam.set(false));
        assert!(first.is_err(), "seam forces a failure");
        // The transcript landed, the marker did not.
        let dest_project = new_home.join("projects").join(slug_for(&cwd));
        let dest = dest_project.join(format!("{session}.jsonl"));
        assert!(dest.is_file(), "transcript published before the crash seam");
        assert!(
            !dest_project.join(".remuda-staging").exists(),
            "marker not written at the seam"
        );
        // Retry recovers the identical transcript and writes the marker.
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("retry recovers");
        assert_eq!(
            std::fs::read_to_string(&staged.transcript).unwrap(),
            "recoverable-turn\n"
        );
        assert!(
            dest_project.join(".remuda-staging").exists(),
            "marker written on recovery"
        );
    }

    /// Round 4 item 10: a sidecar that grows after enumeration cannot push the
    /// copy over the aggregate byte cap. The transcript fills most of a tiny
    /// cap; an enumerated sidecar slightly over the remainder is refused
    /// during the real stream.
    #[test]
    fn round4_a_sidecar_growing_past_remaining_budget_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000bb";
        let source = transcript_layout(&old_home, &cwd, session);
        // 10-byte transcript.
        write_file(&source, "0123456789");
        // A 6-byte sidecar.
        write_file(&source.parent().unwrap().join("memory/MEMORY.md"), "abcdef");
        // Cap of 12 bytes: transcript (10) + sidecar (6) exceeds it, although
        // EACH file is under the cap on its own.
        let tight = StageLimits {
            max_bytes: 12,
            max_files: 10_000,
            max_depth: 32,
        };
        let error = stage_for_resume_with_limits(&source, &new_home, &cwd, session, tight)
            .expect_err("aggregate cap enforced on live stream");
        assert!(
            error.to_string().contains("size limit exceeded"),
            "unexpected: {error}"
        );
    }

    /// Round 4 item 11: an injected failure after files are staged (but before
    /// publish) leaves NOTHING in the slug directory — the populated private
    /// stage tree is removed by the caller.
    #[test]
    fn round4_failed_attempt_removes_its_populated_stage_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000bc";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "the transcript\n");
        write_file(
            &source.parent().unwrap().join("memory/MEMORY.md"),
            "project memory\n",
        );
        CRASH_AFTER_STAGE_SEAM.with(|seam| seam.set(true));
        let error = stage_for_resume(&source, &new_home, &cwd, session);
        CRASH_AFTER_STAGE_SEAM.with(|seam| seam.set(false));
        assert!(error.is_err(), "seam forces a failure");
        let slug_dir = new_home.join("projects").join(slug_for(&cwd));
        let leftover: Vec<_> = std::fs::read_dir(&slug_dir)
            .expect("slug dir exists")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(STAGING_TMP_PREFIX)
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "the populated private stage dir was removed after failure: {leftover:?}"
        );
        // Nothing was published either.
        assert!(
            !slug_dir.join(format!("{session}.jsonl")).exists(),
            "no transcript published before the seam"
        );
        // A normal retry then succeeds.
        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("retry succeeds");
        assert!(staged.transcript.is_file());
    }

    #[cfg(unix)]
    fn inode_of(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).unwrap().ino()
    }

    /// Round 5 part 2 item 3: a markerless byte-identical destination that is
    /// a HARDLINK to an unrelated outside file must be REPLACED by the staged
    /// O_EXCL copy (the published inode is ours), not kept just because the
    /// bytes match.
    #[cfg(unix)]
    #[test]
    fn round5_markerless_identical_hardlink_is_replaced_with_our_inode() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let outside = tmp.path().join("outside.jsonl");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000c1";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "identical-bytes\n");
        write_file(&outside, "identical-bytes\n");
        // Stage once, then replace the staged transcript with a hardlink to
        // the outside file and remove the marker (a markerless foreign link).
        stage_for_resume(&source, &new_home, &cwd, session).expect("first stage");
        let dest = transcript_layout(&new_home, &cwd, session);
        let marker = dest
            .parent()
            .unwrap()
            .join(".remuda-staging")
            .join(format!("{}.json", session));
        std::fs::remove_file(&dest).expect("remove staged transcript");
        std::fs::remove_file(&marker).expect("remove marker");
        std::fs::hard_link(&outside, &dest).expect("plant hardlink");
        assert_eq!(inode_of(&dest), inode_of(&outside), "setup: shared inode");

        stage_for_resume(&source, &new_home, &cwd, session).expect("recovered by replacement");
        assert_ne!(
            inode_of(&dest),
            inode_of(&outside),
            "the destination is now an independent inode, not the outside hardlink"
        );
        // The outside file is untouched.
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "identical-bytes\n"
        );
    }

    /// Round 5 part 2 item 4: a cross-home SIDECAR hardlink with identical
    /// bytes is replaced by the independent staged copy on retry.
    #[cfg(unix)]
    #[test]
    fn round5_cross_home_sidecar_hardlink_is_replaced() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let outside = tmp.path().join("side-outside.jsonl");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let session = "01993ab0-0000-7000-8000-0000000000c2";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "{}\n");
        write_file(
            &source.parent().unwrap().join("memory/MEMORY.md"),
            "shared sidecar\n",
        );
        write_file(&outside, "shared sidecar\n");
        // First stage establishes the sidecar.
        stage_for_resume(&source, &new_home, &cwd, session).expect("first stage");
        let side_dest = transcript_layout(&new_home, &cwd, session)
            .parent()
            .unwrap()
            .join("memory/MEMORY.md");
        // Replace the staged sidecar with a hardlink to the outside file.
        std::fs::remove_file(&side_dest).expect("remove staged sidecar");
        std::fs::hard_link(&outside, &side_dest).expect("sidecar hardlink");
        assert_eq!(inode_of(&side_dest), inode_of(&outside));

        stage_for_resume(&source, &new_home, &cwd, session).expect("retry replaces hardlink");
        assert_ne!(
            inode_of(&side_dest),
            inode_of(&outside),
            "cross-home sidecar hardlink replaced by an independent copy"
        );
        assert_eq!(
            std::fs::read_to_string(&side_dest).unwrap(),
            "shared sidecar\n"
        );
    }

    /// Round 5 part 2 item 5: the inherited-home same-file no-op refuses when
    /// an inherited sidecar root (`<S>/`) is a symlink.
    #[cfg(unix)]
    #[test]
    fn round5_inherited_noop_validates_sidecar_roots() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("ws");
        let sink = tmp.path().join("sink");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&sink).expect("sink");
        let session = "01993ab0-0000-7000-8000-0000000000c3";
        let source = transcript_layout(&home, &cwd, session);
        write_file(&source, "inherited\n");
        // A symlinked session sidecar root under the same inherited home.
        let side = source.parent().unwrap().join(session);
        symlink(&sink, &side).expect("side root link");
        let error = stage_for_resume(&source, &home, &cwd, session)
            .expect_err("inherited no-op must validate sidecar roots");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    /// Round 5 part 1 (macOS slug): staging under a cwd reached through a
    /// symlinked ancestor uses the PHYSICAL slug on both source and
    /// destination, so sidecars and the transcript line up.
    /// Round 5 part 2 item 1 (source): a predecessor home whose `projects` is a
    /// symlink to /outside is never followed — staging refuses rather than
    /// copying an outside tree, and the outside bytes are untouched.
    #[cfg(unix)]
    #[test]
    fn round5_source_symlinked_projects_dir_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let outside = tmp.path().join("outside");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&old_home).expect("home");
        std::fs::create_dir_all(&outside).expect("outside");
        let session = "01993ab0-0000-7000-8000-0000000000c5";
        // The transcript lives THROUGH the symlinked projects.
        let stolen = outside
            .join(slug_for(&cwd))
            .join(format!("{session}.jsonl"));
        write_file(&stolen, "outside bytes\n");
        std::os::unix::fs::symlink(&outside, old_home.join("projects")).expect("projects link");
        let recorded = transcript_layout(&old_home, &cwd, session);
        let error = stage_for_resume(&recorded, &new_home, &cwd, session)
            .expect_err("staging must refuse a symlinked predecessor projects");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&stolen).unwrap(),
            "outside bytes\n",
            "the outside transcript is never opened for copying"
        );
    }

    /// Round 5 part 2 item 1 (destination): a symlinked native-home target is
    /// refused; mkdir/create never descends into it.
    #[cfg(unix)]
    #[test]
    fn round5_destination_symlinked_native_home_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let old_home = tmp.path().join("old");
        let new_home = tmp.path().join("new");
        let elsewhere = tmp.path().join("elsewhere");
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        let session = "01993ab0-0000-7000-8000-0000000000c6";
        let source = transcript_layout(&old_home, &cwd, session);
        write_file(&source, "x\n");
        // The configured native home is itself a symlink.
        std::os::unix::fs::symlink(&elsewhere, &new_home).expect("native-home link");
        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("a symlinked native home is not created through");
        assert!(
            error.kind() == std::io::ErrorKind::PermissionDenied
                || error.to_string().contains("symlink"),
            "{error}"
        );
        // Nothing created in the symlink target.
        assert!(!elsewhere.join("projects").exists(), "elsewhere untouched");
    }

    #[cfg(unix)]
    #[test]
    fn round5_staging_uses_the_physical_cwd_slug_under_a_symlinked_ancestor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("cwd ancestor link");
        // The LOGICAL cwd goes through the symlink; Claude/getcwd sees the
        // physical path.
        let logical_cwd = link.join("ws");
        let physical_cwd = real.join("ws");
        std::fs::create_dir_all(&physical_cwd).expect("physical cwd");
        let old_home = real.join("old");
        let new_home = real.join("new");
        let session = "01993ab0-0000-7000-8000-0000000000c4";
        // Build the source using the PHYSICAL slug (as Claude writes it).
        let source = transcript_layout(&old_home, &physical_cwd, session);
        write_file(&source, "physical-slug\n");
        // Stage passing the LOGICAL cwd (as a spawn request does).
        let staged =
            stage_for_resume(&source, &new_home, logical_cwd.as_path(), session).expect("stage");
        let physical_slug = slug_for(&logical_cwd);
        assert!(
            staged.transcript.to_string_lossy().contains(&physical_slug),
            "destination uses the physical slug: {}",
            staged.transcript.display()
        );
        assert_eq!(
            std::fs::read_to_string(&staged.transcript).unwrap(),
            "physical-slug\n"
        );
        // Nothing was created under the LOGICAL slug.
        assert!(
            !new_home
                .join("projects")
                .join(encode_project_dir(&logical_cwd))
                .exists(),
            "no logical-slug tree is created"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_staging_refuses_a_symlink_source_transcript() {
        {
            use std::os::unix::fs::symlink;
            let tmp = tempfile::tempdir().expect("tmp");
            let old_home = tmp.path().join("old");
            let new_home = tmp.path().join("new");
            let cwd = tmp.path().join("ws");
            std::fs::create_dir_all(&cwd).expect("cwd");
            let session = "01993ab0-0000-7000-8000-0000000000ab";
            let real = tmp.path().join("real-conversation.jsonl");
            write_file(&real, "{}\n");
            let link = transcript_layout(&old_home, &cwd, session);
            std::fs::create_dir_all(link.parent().unwrap()).expect("mkdir");
            symlink(&real, &link).expect("symlink");

            let error = stage_for_resume(&link, &new_home, &cwd, session)
                .expect_err("a symlinked transcript must not be staged verbatim");
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "an fd-walk symlink refusal surfaces as PermissionDenied: {error}"
            );
            assert!(error.to_string().contains("symlink"), "{error}");
        }
    }
}
