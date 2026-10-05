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

/// Normalize a path lexically (`.` removed, `..` pops), never touching the
/// filesystem, so symlinks cannot influence a containment verdict.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Verify `child` stays inside `base` using lexical components only — no
/// `canonicalize`, so an untrusted symlink at either path is never followed
/// (c-resumehome, review item 1). Both paths must be absolute; a verdict on
/// relative paths would depend on the caller's cwd.
fn ensure_within(base: &Path, child: &Path) -> std::io::Result<()> {
    if !base.is_absolute() || !child.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "resume staging containment needs absolute paths (base {}, child {})",
                base.display(),
                child.display()
            ),
        ));
    }
    let base = normalize_lexical(base);
    let child = normalize_lexical(child);
    if child.starts_with(&base) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "resume staging path {} escapes its root {}",
                child.display(),
                base.display()
            ),
        ))
    }
}

/// True when `path` itself is a symlink (its target is not followed).
fn is_symlink(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Construct an [`std::io::Error`] with kind [`std::io::ErrorKind::InvalidInput`].
fn invalid_input(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
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

/// What [`stage_for_resume`] made visible inside the resume launch's home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedResume {
    /// The transcript path the resumed process opens (`--resume <id>` target).
    pub transcript: PathBuf,
    /// Side directories copied beside it (subagent transcripts, memory, …).
    pub sidecar_dirs: Vec<PathBuf>,
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
/// Copy semantics, deliberately not a hardlink: the resumed process appends
/// to the transcript, and a hardlink would append those turns into the
/// predecessor's file too (and links fail across filesystems). The predecessor
/// stays byte-frozen.
///
/// Copied when present beside the transcript:
/// - `<session>/` — subagent transcripts and tool-result payloads;
/// - `memory/` — the project memory Claude reloads on SessionStart.
///
/// The copy is bounded (review item 7): at most 256 MiB of regular-file
/// bytes, 10 000 files and 32 directory levels, transcript included.
/// Exceeding any bound is a clear error before the transcript is published.
///
/// Existing destination files are kept ONLY with provenance (review item 4):
/// an existing `<session>.jsonl` is preserved when a sibling staging marker
/// proves this launch's predecessor staged it (source path plus size/sha256); a
/// file without that proof is a foreign conversation and aborts staging with a
/// clear conflict. The transcript is published through a same-directory temp
/// file plus rename, so a crashed copy is never seen as a complete
/// conversation; sidecars merge first and never overwrite.
///
/// The destination file is always named `<session_id>.jsonl` — the exact name
/// `claude --resume <session_id>` looks for — even when the recorded source
/// path carries a different file name (a promoted session moved outside the
/// managed home keeps whatever name the hook reported).
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
    // `symlink_metadata`, not `metadata`: a staged transcript must be a real
    // regular file, never a link the resume could follow somewhere else. The
    // original error kind is preserved (a missing file stays `NotFound`).
    let metadata = source_transcript.symlink_metadata().map_err(|err| {
        std::io::Error::new(
            err.kind(),
            format!(
                "resume transcript not found at {}: {err}",
                source_transcript.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(invalid_input(format!(
            "resume transcript is not a regular file: {}",
            source_transcript.display()
        )));
    }
    let source_name = source_transcript.file_name().ok_or_else(|| {
        invalid_input(format!(
            "resume transcript path has no file name: {}",
            source_transcript.display()
        ))
    })?;
    let dest_dir = project_dir(target_home, target_cwd);
    // Lexical containment only: the managed home (and any partial retry state
    // inside it) must not be able to redirect staging through a symlink.
    ensure_within(target_home, &dest_dir)?;
    let dest_transcript = dest_dir.join(format!("{session_id}.jsonl"));
    ensure_within(&dest_dir, &dest_transcript)?;
    std::fs::create_dir_all(&dest_dir)?;
    if is_symlink(&dest_dir)? {
        return Err(invalid_input(format!(
            "resume staging directory {} is a symlink, not a real directory",
            dest_dir.display()
        )));
    }

    // Inherited-home resume (or a replayed build): the conversation already
    // lives where the new process will look. Nothing to stage, and no
    // provenance marker is required — it is not a Remuda-made copy.
    if same_path(&dest_transcript, source_transcript) {
        return Ok(StagedResume {
            transcript: dest_transcript,
            sidecar_dirs: Vec::new(),
        });
    }

    let (source_size, source_sha256) = sha256_file(source_transcript)?;
    let marker_path = provenance_marker_path(&dest_dir, session_id);

    // Classify any existing destination transcript without following it.
    match std::fs::symlink_metadata(&dest_transcript) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(invalid_input(format!(
                "resume destination {} is a symlink, not a real file",
                dest_transcript.display()
            )));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid_input(format!(
                "resume destination {} is not a regular file",
                dest_transcript.display()
            )));
        }
        Ok(_) => {
            // Review item 4: keep an existing destination ONLY when provenance
            // proves this launch staged it from exactly this predecessor.
            // Anything else (a stale same-session file, a foreign home that
            // already held this id) is a conflict, never a silent overwrite.
            let Some(provenance) = read_staging_provenance(&marker_path)? else {
                return Err(invalid_input(format!(
                    "resume destination {} already exists without Remuda staging provenance for \
                     predecessor {}; refusing to overwrite a conversation this launch did not \
                     stage (resume in a fresh native home or remove the stale file)",
                    dest_transcript.display(),
                    source_transcript.display()
                )));
            };
            if !provenance.covers(source_transcript, source_size, &source_sha256) {
                return Err(invalid_input(format!(
                    "resume destination {} exists but its staging provenance does not match \
                     predecessor {} (size {source_size}, sha256 {source_sha256}); refusing to \
                     overwrite",
                    dest_transcript.display(),
                    source_transcript.display()
                )));
            }
            // Proven: the child's own staged copy (possibly already appended
            // to by an earlier launch attempt). Merge any missing sidecars,
            // never touch the transcript. The merge stays bounded even though
            // the transcript itself is not recopied.
            let mut budget = CopyBudget::default();
            let sidecar_dirs = copy_resume_sidecars(
                source_transcript,
                source_name,
                &dest_dir,
                session_id,
                &limits,
                &mut budget,
            )?;
            return Ok(StagedResume {
                transcript: dest_transcript,
                sidecar_dirs,
            });
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    // Fresh staging. The transcript counts against the budget first (review
    // item 7). Sidecars merge next (idempotent: a crashed earlier attempt may
    // have left parts), provenance is published after them, and the transcript
    // is published LAST through a same-directory temp file + rename — until
    // that rename lands, the conversation is visibly absent and
    // `claude --resume` fails its lookup rather than reading a partial copy.
    let mut budget = CopyBudget::default();
    budget.charge(source_size, &limits)?;
    let sidecar_dirs = copy_resume_sidecars(
        source_transcript,
        source_name,
        &dest_dir,
        session_id,
        &limits,
        &mut budget,
    )?;
    write_staging_provenance(
        &marker_path,
        &StagingProvenance::new(source_transcript, source_size, &source_sha256),
    )?;
    publish_transcript_atomic(source_transcript, &dest_transcript, session_id)?;

    Ok(StagedResume {
        transcript: dest_transcript,
        sidecar_dirs,
    })
}

/// Directory (inside the destination project slug) holding per-session
/// provenance markers. Dot-prefixed so Claude's transcript globs never read
/// it; the fake and the real CLI only open `<session>.jsonl`.
const STAGING_MARKER_DIR: &str = ".remuda-staging";

/// Provenance recorded next to a staged transcript (review item 4).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StagingProvenance {
    /// Marker format version.
    version: u32,
    /// Absolute path of the predecessor transcript this copy came from.
    source: String,
    /// Source size in bytes at staging time.
    source_size: u64,
    /// Lower-hex SHA-256 of the source at staging time.
    source_sha256: String,
}

impl StagingProvenance {
    fn new(source: &Path, source_size: u64, source_sha256: &str) -> Self {
        Self {
            version: 1,
            source: source.to_string_lossy().into_owned(),
            source_size,
            source_sha256: source_sha256.to_owned(),
        }
    }

    /// Whether this marker proves the destination was staged from `source`
    /// with exactly the predecessor content currently on disk.
    fn covers(&self, source: &Path, source_size: u64, source_sha256: &str) -> bool {
        self.version == 1
            && self.source == source.to_string_lossy()
            && self.source_size == source_size
            && self.source_sha256 == source_sha256
    }
}

fn provenance_marker_path(dest_dir: &Path, session_id: &str) -> PathBuf {
    dest_dir
        .join(STAGING_MARKER_DIR)
        .join(format!("{session_id}.json"))
}

/// Read and validate a provenance marker; `None` means absent (a corrupt or
/// older-format marker is treated as absent — fail closed on the existing
/// destination).
fn read_staging_provenance(path: &Path) -> std::io::Result<Option<StagingProvenance>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(serde_json::from_str::<StagingProvenance>(&body)
            .ok()
            .filter(|marker| marker.version == 1)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Write the marker via a same-directory temp file + rename, so a crash can
/// leave either the old marker or the new one, never a torn write.
fn write_staging_provenance(path: &Path, provenance: &StagingProvenance) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        invalid_input(format!(
            "provenance path has no directory: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(dir)?;
    if is_symlink(dir)? {
        return Err(invalid_input(format!(
            "provenance directory {} is a symlink",
            dir.display()
        )));
    }
    let tmp = dir.join(format!(".marker-tmp-{}", uuid::Uuid::new_v4()));
    let body = serde_json::to_vec_pretty(provenance).map_err(std::io::Error::other)?;
    let outcome = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
    if outcome.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

/// Copy `source` to `dest` via a same-directory temp file plus atomic rename.
fn publish_transcript_atomic(source: &Path, dest: &Path, session_id: &str) -> std::io::Result<()> {
    let dir = dest.parent().ok_or_else(|| {
        invalid_input(format!(
            "transcript destination has no directory: {}",
            dest.display()
        ))
    })?;
    let tmp = dir.join(format!(
        ".transcript-tmp-{session_id}-{}",
        uuid::Uuid::new_v4()
    ));
    let outcome = (|| {
        std::fs::copy(source, &tmp)?;
        // Flush the staged bytes before the rename makes them visible.
        if let Ok(file) = std::fs::OpenOptions::new().read(true).open(&tmp) {
            let _ = file.sync_all();
        }
        std::fs::rename(&tmp, dest)
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

/// SHA-256 (lower hex) and byte size of a regular file read in bounded chunks.
fn sha256_file(path: &Path) -> std::io::Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher)?;
    Ok((size, format!("{:x}", hasher.finalize())))
}

/// Copy the per-session sidecar dir (`<dir>/<S>/` first, file stem fallback)
/// and the project-global `memory/` next to the staged transcript.
fn copy_resume_sidecars(
    source_transcript: &Path,
    source_name: &std::ffi::OsStr,
    dest_dir: &Path,
    session_id: &str,
    limits: &StageLimits,
    budget: &mut CopyBudget,
) -> std::io::Result<Vec<PathBuf>> {
    let source_dir = source_transcript.parent().unwrap_or_else(|| Path::new("/"));
    let mut sidecar_dirs = Vec::new();
    // The per-session directory is keyed by the native session id: a
    // promoted/renamed transcript can carry a different file name while its
    // `<S>/` sidecar dir keeps the native id (review item 3), so `<dir>/<S>/`
    // is looked up FIRST and the file stem is only a fallback for older
    // promoted layouts.
    let mut per_session_sources: Vec<std::ffi::OsString> = vec![session_id.into()];
    if let Some(stem) = Path::new(source_name)
        .file_stem()
        .map(std::ffi::OsStr::to_os_string)
        .filter(|stem| stem != session_id)
    {
        per_session_sources.push(stem);
    }
    for source_side_name in per_session_sources {
        let source_side = source_dir.join(&source_side_name);
        match std::fs::symlink_metadata(&source_side) {
            Ok(metadata) if metadata.is_dir() => {
                let dest_side = dest_dir.join(session_id);
                ensure_within(dest_dir, &dest_side)?;
                copy_dir_merge(source_dir, &source_side, &dest_side, 0, limits, budget)?;
                sidecar_dirs.push(dest_side);
                break;
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_input(format!(
                    "resume sidecar {} is a symlink, not a real directory",
                    source_side.display()
                )));
            }
            _ => continue,
        }
    }
    let memory_side = source_dir.join("memory");
    if let Ok(metadata) = std::fs::symlink_metadata(&memory_side)
        && metadata.is_dir()
    {
        let dest_side = dest_dir.join("memory");
        ensure_within(dest_dir, &dest_side)?;
        copy_dir_merge(source_dir, &memory_side, &dest_side, 0, limits, budget)?;
        sidecar_dirs.push(dest_side);
    } else if let Ok(metadata) = std::fs::symlink_metadata(&memory_side)
        && metadata.file_type().is_symlink()
    {
        return Err(invalid_input(format!(
            "resume sidecar {} is a symlink, not a real directory",
            memory_side.display()
        )));
    }
    Ok(sidecar_dirs)
}

/// Path equality after canonicalization, falling back to lexical equality.
fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Maximum symlink hops followed while resolving one staged link, so a link
/// cycle cannot hang the walk.
const MAX_LINK_HOPS: usize = 32;

/// Bounds on a resume staging copy (review item 7). A predecessor home is
/// untrusted input: without caps, a huge transcript or an enormous sidecar
/// tree could fill the child instance's disk or pin the staging worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StageLimits {
    /// Total regular-file bytes staged (transcript included).
    max_bytes: u64,
    /// Total regular files staged (transcript included).
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

/// Running tally charged against [`StageLimits`].
#[derive(Debug, Default)]
struct CopyBudget {
    files: u64,
    bytes: u64,
}

impl CopyBudget {
    fn charge(&mut self, size: u64, limits: &StageLimits) -> std::io::Result<()> {
        let next_files = self
            .files
            .checked_add(1)
            .ok_or_else(|| limit_exceeded("file count overflow while staging"))?;
        let next_bytes = self
            .bytes
            .checked_add(size)
            .ok_or_else(|| limit_exceeded("byte count overflow while staging"))?;
        if next_files > limits.max_files {
            return Err(limit_exceeded(format!(
                "resume staging file count limit exceeded: more than {} files",
                limits.max_files
            )));
        }
        if next_bytes > limits.max_bytes {
            return Err(limit_exceeded(format!(
                "resume staging size limit exceeded: more than {} bytes",
                limits.max_bytes
            )));
        }
        self.files = next_files;
        self.bytes = next_bytes;
        Ok(())
    }
}

fn limit_exceeded(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

/// Recursively merge `src` into `dst`, never overwriting destination files.
///
/// Symlinks are never recreated (c-resumehome, review item 2): a link recreated
/// in the child's home could point back at the predecessor's files or outside
/// the managed home, and the resumed process would then write through it. A
/// link is followed hop by hop — every hop containment-checked against `root`
/// — and its target is copied as an independent file only when the final
/// target is a regular file. Escaping links, directory links, broken links and
/// cycles are hard errors, never silently skipped.
fn copy_dir_merge(
    root: &Path,
    src: &Path,
    dst: &Path,
    depth: u32,
    limits: &StageLimits,
    budget: &mut CopyBudget,
) -> std::io::Result<()> {
    if depth > limits.max_depth {
        return Err(limit_exceeded(format!(
            "resume staging depth limit exceeded: more than {} nested directories",
            limits.max_depth
        )));
    }
    // Check before `create_dir_all`: on a broken directory symlink
    // `create_dir_all` follows the link and creates its external target.
    if is_symlink(dst)? {
        return Err(invalid_input(format!(
            "resume staging destination {} is a symlink, not a real directory",
            dst.display()
        )));
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let source = entry.path();
        let dest = dst.join(entry.file_name());
        // Classify with `symlink_metadata`: on some filesystems a read_dir
        // d_type of "file" can hide a link, and `is_dir`/`exists` follow links.
        let file_type = std::fs::symlink_metadata(&source)?.file_type();
        if file_type.is_symlink() {
            copy_linked_file(root, &source, &dest, limits, budget)?;
        } else if file_type.is_dir() {
            copy_dir_merge(root, &source, &dest, depth + 1, limits, budget)?;
        } else if !lexically_exists(&dest)? {
            let size = std::fs::symlink_metadata(&source)?.len();
            budget.charge(size, limits)?;
            std::fs::copy(&source, &dest)?;
        }
    }
    Ok(())
}

/// Copy one symlink entry as an independent file, when and only when every hop
/// of its target chain stays inside `root` and ends at a regular file.
fn copy_linked_file(
    root: &Path,
    link: &Path,
    dest: &Path,
    limits: &StageLimits,
    budget: &mut CopyBudget,
) -> std::io::Result<()> {
    if lexically_exists(dest)? {
        // Never overwrite a destination entry, including a destination link.
        return Ok(());
    }
    let target = resolve_link_within(root, link)?;
    let metadata = std::fs::symlink_metadata(&target)?;
    if !metadata.is_file() {
        return Err(invalid_input(format!(
            "symlink {} resolves to {} inside the staged project, which is not a regular file",
            link.display(),
            target.display()
        )));
    }
    budget.charge(metadata.len(), limits)?;
    std::fs::copy(&target, dest)?;
    Ok(())
}

/// Resolve `start` without trusting any single `canonicalize`, enforcing that
/// every link in the chain stays lexically inside `root`.
fn resolve_link_within(root: &Path, start: &Path) -> std::io::Result<PathBuf> {
    let root = normalize_lexical(root);
    let mut current = start.to_path_buf();
    for _ in 0..=MAX_LINK_HOPS {
        let normalized = normalize_lexical(&current);
        if !normalized.is_absolute() || !normalized.starts_with(&root) {
            return Err(invalid_input(format!(
                "symlink {} escapes the staged project directory {} via {}",
                start.display(),
                root.display(),
                current.display()
            )));
        }
        let metadata = match std::fs::symlink_metadata(&normalized) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "symlink {} is broken (dangling target {})",
                        start.display(),
                        current.display()
                    ),
                ));
            }
            Err(err) => return Err(err),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(normalized);
        }
        let target = std::fs::read_link(&normalized)?;
        current = if target.is_absolute() {
            target
        } else {
            normalized
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(target)
        };
    }
    Err(invalid_input(format!(
        "symlink {} is a link cycle (more than {MAX_LINK_HOPS} hops)",
        start.display()
    )))
}

/// Whether a path exists, checked without following a terminal symlink.
fn lexically_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Compare two directories the way Claude resolves them (canonical, no symlinks).
fn same_dir(a: &Path, b: &Path) -> bool {
    same_path(a, b)
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

    fn transcript_layout(home: &std::path::Path, cwd: &std::path::Path, session: &str) -> PathBuf {
        project_dir(home, cwd).join(format!("{session}.jsonl"))
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
                .join(encode_project_dir(&cwd))
                .join(session)
                .join("subagents")
                .join("side.jsonl"),
            "{\"side\":true}\n",
        );
        write_file(
            &old_home
                .join("projects")
                .join(encode_project_dir(&cwd))
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
            provenance_marker_path(dest.parent().unwrap(), session).is_file(),
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
                .join(encode_project_dir(&cwd))
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
    fn resume_staging_symlink_sidecars_are_copied_as_independent_files() {
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

        // Internal relative link (subagents dir → memory file) and an internal
        // link chain (chain → inner → real file). Both stay in the project.
        let subagents = project.join(session).join("subagents");
        std::fs::create_dir_all(&subagents).expect("mkdir");
        write_file(&project.join("memory/MEMORY.md"), "project memory\n");
        symlink("../../memory/MEMORY.md", subagents.join("memory-link.md")).expect("internal link");
        write_file(&project.join(session).join("inner.jsonl"), "inner-target\n");
        symlink("inner.jsonl", project.join(session).join("mid.jsonl")).expect("mid link");
        symlink("mid.jsonl", project.join(session).join("chain.jsonl")).expect("chain link");

        let staged = stage_for_resume(&source, &new_home, &cwd, session).expect("stage");

        let copied = subagents.strip_prefix(project).unwrap().to_path_buf();
        let dest_memory_link = staged
            .transcript
            .parent()
            .unwrap()
            .join(&copied)
            .join("memory-link.md");
        let metadata = std::fs::symlink_metadata(&dest_memory_link).expect("copied link entry");
        assert!(
            metadata.is_file(),
            "the link becomes a regular file, not a link"
        );
        assert_eq!(
            std::fs::read_to_string(&dest_memory_link).unwrap(),
            "project memory\n",
            "target content is copied as an independent file"
        );
        for name in ["chain.jsonl", "mid.jsonl"] {
            let path = staged.transcript.parent().unwrap().join(session).join(name);
            assert!(
                std::fs::symlink_metadata(&path).unwrap().is_file(),
                "{name}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(
                staged
                    .transcript
                    .parent()
                    .unwrap()
                    .join(session)
                    .join("chain.jsonl")
            )
            .unwrap(),
            "inner-target\n",
            "internal chains resolve to their final in-tree target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_staging_rejects_escaping_broken_and_directory_symlinks() {
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

        // Outside the project tree: relative traversal and an absolute link.
        let outside = tmp.path().join("outside.txt");
        write_file(&outside, "outside\n");
        symlink(&outside, side.join("escaping-abs")).expect("abs link");
        symlink("../../../../outside.txt", side.join("escaping-rel")).expect("rel link");
        // Multi-hop escape: the first hop looks internal (into the project
        // root), the second leaves via an absolute target.
        symlink("../../hop2", side.join("hop1")).expect("hop1");
        symlink(&outside, project.join("hop2")).expect("hop2 leaves tree");
        // Broken link.
        symlink("does-not-exist", side.join("broken")).expect("broken link");
        // Directory link, internal.
        std::fs::create_dir_all(project.join("realdir")).expect("realdir");
        symlink("realdir", side.join("dir-link")).expect("dir link");

        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("any unsafe sidecar link aborts staging with an error, never a skip");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
            ),
            "escaping/dir links are InvalidInput; a dangling link is NotFound: {error}"
        );
        // The dangerous links are never reproduced in the child home.
        for name in ["escaping-abs", "escaping-rel", "hop1", "broken", "dir-link"] {
            assert!(
                !new_home
                    .join("projects")
                    .join(encode_project_dir(&cwd))
                    .join(session)
                    .join(name)
                    .symlink_metadata()
                    .is_ok_and(|meta| meta.file_type().is_symlink()),
                "{name} must not exist as a symlink in the staged tree"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn resume_staging_refuses_a_symlinked_sidecar_root() {
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
        write_file(&elsewhere.join("SECRET.md"), "x\n");
        symlink(&elsewhere, project.join("memory")).expect("symlinked memory root");

        let error = stage_for_resume(&source, &new_home, &cwd, session)
            .expect_err("a symlinked memory dir must not be read through");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn resume_staging_refuses_a_symlink_source_transcript() {
        #[cfg(unix)]
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
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("not a regular file"), "{error}");
        }
    }
}
