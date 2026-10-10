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
    if session_id.trim().is_empty() {
        return None;
    }
    let path = project_dir(claude_home, cwd).join(format!("{}.jsonl", session_id.trim()));
    path.is_file().then(|| TranscriptBinding {
        session_id: session_id.trim().to_owned(),
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
    if record.session_id.trim().is_empty() {
        return None;
    }
    let path = project_dir(claude_home, &record.cwd).join(format!("{}.jsonl", record.session_id));
    if !path.is_file() {
        return None;
    }
    // The pid file's own cwd must be the terminal's cwd: a pid collision across
    // dirs must never hydrate the wrong slug.
    if !same_dir(&record.cwd, terminal_cwd) {
        tracing::warn!(
            pid,
            session_id = %record.session_id,
            claimed = %record.cwd.display(),
            terminal = %terminal_cwd.display(),
            "pid session cwd does not match the promoted terminal; refusing transcript"
        );
        return None;
    }
    Some(TranscriptBinding {
        session_id: record.session_id,
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
    /// Claude Code SessionStart hook `source` when present:
    /// `"startup" | "clear" | "compact" | "resume"` (c-effortread r8 item 2).
    /// A `/clear` or startup/compact source starts a NEW empty session, so its
    /// mode is Fresh AT HOOK DELIVERY even if the prompt reaches Z.jsonl before
    /// the next poll tick sizes it; `"resume"` carries existing history.
    /// `None` = older hook payload / unknown — callers fall back to sizing.
    pub start_source: Option<String>,
}

impl SessionStartReport {
    /// Whether this hook's source proves the session started EMPTY (a `/clear`,
    /// startup or compact) rather than resuming prior history.
    /// c-effortread r8 item 2.
    #[must_use]
    pub fn source_is_fresh_start(&self) -> bool {
        matches!(
            self.start_source.as_deref(),
            Some("startup" | "clear" | "compact")
        )
    }

    /// Whether this hook's source is an explicit resume of an existing
    /// session (history must be treated as unverified).
    #[must_use]
    pub fn source_is_resume(&self) -> bool {
        self.start_source.as_deref() == Some("resume")
    }

    /// Parse one hook stdin payload (`{hook_event_name, session_id,
    /// transcript_path, cwd, ppid, source}`; camelCase tolerated, `ppid` and
    /// `source` optional).
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
            #[serde(default)]
            source: Option<String>,
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
            start_source: raw.source,
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
    if session_id.is_empty() {
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

/// Stable filesystem identity of one transcript file (device + inode).
///
/// A byte length alone cannot tell a same-path *replacement* (Claude recreates
/// the file, a rotation, an editor save) from an append: the replacement may be
/// longer, shorter or equal. Identity makes that distinction verifiable, which
/// is what keeps a resumed process from replaying a rotated file as current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    /// `st_dev` on Unix; 0 where the platform exposes no stable device id.
    pub dev: u64,
    /// `st_ino` on Unix; 0 where the platform exposes no stable inode.
    pub ino: u64,
}

impl FileIdentity {
    /// Identity of an existing file.
    #[cfg(unix)]
    #[must_use]
    pub fn of(path: &Path) -> Option<Self> {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| Self::from_metadata(&metadata))
    }

    /// Identity from already-read metadata.
    #[cfg(unix)]
    #[must_use]
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    /// Identity of an existing file (non-Unix fallback: no stable identity).
    #[cfg(not(unix))]
    #[must_use]
    pub fn of(_path: &Path) -> Option<Self> {
        Some(Self { dev: 0, ino: 0 })
    }

    /// Identity from metadata (non-Unix fallback: no stable identity).
    #[cfg(not(unix))]
    #[must_use]
    pub fn from_metadata(_metadata: &std::fs::Metadata) -> Self {
        Self { dev: 0, ino: 0 }
    }
}

/// How many leading bytes of the transcript are fingerprinted to tell a
/// same-path *replacement* (rotate/swap) from a genuine append. Inode + length
/// alone cannot: on tmpfs a delete+create hands the freed inode straight back,
/// and a longer replacement passes every length check. Claude transcripts are
/// append-only, so the head never changes on a real resume; a swapped file
/// rewrites it from record one.
const HEAD_PROBE: u64 = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadFingerprint {
    /// Number of leading bytes hashed (`min(file_len, HEAD_PROBE)` at capture).
    len: u64,
    hash: u64,
}

fn fingerprint_bytes(buf: &[u8], file_len: u64) -> HeadFingerprint {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let n = (file_len.min(HEAD_PROBE) as usize).min(buf.len());
    if n == 0 {
        return HeadFingerprint { len: 0, hash: 0 };
    }
    let mut hasher = DefaultHasher::new();
    hasher.write(&buf[..n]);
    HeadFingerprint {
        len: n as u64,
        hash: hasher.finish(),
    }
}

fn hash_head(path: &Path, file_len: u64) -> Option<HeadFingerprint> {
    use std::io::Read;
    let n = file_len.min(HEAD_PROBE);
    if n == 0 {
        return Some(HeadFingerprint { len: 0, hash: 0 });
    }
    let mut buf = vec![0_u8; n as usize];
    let mut file = std::fs::File::open(path).ok()?;
    file.read_exact(&mut buf).ok()?;
    Some(fingerprint_bytes(&buf, file_len))
}

/// The verified byte boundary at which a *resumed* process's own records begin.
///
/// Captured either before the resumed child is spawned (the transcript's EOF at
/// that instant — its device/inode and length) or, when a hand-typed resume is
/// only discovered later, from process-start provenance (the first record
/// written at/after the foreground process started). Everything before
/// `start` on `identity` is pre-launch history and must never set current
/// effort/ultracode or settle a fresh switch (D-056 (4)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeBoundary {
    /// File identity the boundary was measured on.
    pub identity: FileIdentity,
    /// First byte offset that may belong to the new process.
    pub start: u64,
    /// Whether the boundary is proven (pre-exec snapshot / process-start scan)
    /// versus a known-but-unproven resume anchored at current EOF.
    pub verified: bool,
    /// Fingerprint of the file head at capture, to reject a same-path
    /// replacement that reused the inode and grew the file.
    head: Option<HeadFingerprint>,
}

impl ResumeBoundary {
    /// Snapshot `path` as it is RIGHT BEFORE a resumed child spawns: its
    /// identity plus current EOF. `None` when the file does not exist yet (a
    /// resume that creates its transcript — the pump then treats every byte of
    /// the file that first appears as current, because there is no history).
    #[must_use]
    pub fn snapshot(path: &Path) -> Option<Self> {
        let identity = FileIdentity::of(path)?;
        let start = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let head = hash_head(path, start);
        Some(Self {
            identity,
            start,
            verified: true,
            head,
        })
    }

    /// Deterministic transcript path Claude writes for `session_id` in `cwd`.
    #[must_use]
    pub fn session_path(claude_home: &Path, cwd: &Path, session_id: &str) -> PathBuf {
        project_dir(claude_home, cwd).join(format!("{session_id}.jsonl"))
    }

    /// Build the resume mode for a deterministic `<id>.jsonl` resume: a proven
    /// boundary when the transcript already exists, otherwise [`ResumeMode::Unverified`]
    /// (the file the resumed process will create is not history to trust).
    #[must_use]
    pub fn mode_for(claude_home: &Path, cwd: &Path, session_id: &str) -> ResumeMode {
        let path = Self::session_path(claude_home, cwd, session_id);
        match Self::snapshot(&path) {
            Some(boundary) => ResumeMode::Boundary(boundary),
            None => ResumeMode::Unverified,
        }
    }

    /// Pre-spawn snapshot of the deterministic `<id>.jsonl` for a resume.
    #[must_use]
    pub fn for_resume(claude_home: &Path, cwd: &Path, session_id: &str) -> Option<Self> {
        Self::snapshot(&Self::session_path(claude_home, cwd, session_id))
    }

    /// Current-EOF snapshot for a resume whose boundary is known (it IS a
    /// resume) but cannot be proven: `claude --resume` with no usable uuid, a
    /// platform without process-start evidence (macOS), an unreadable
    /// transcript at process start, or a `exec`-based resume that kept the
    /// shell's pid/start time. The tail starts after every byte currently on
    /// disk so history never replays, but the resume is reported
    /// [`TailProvenance::Unverified`] for the whole epoch — the effort gate
    /// never opens on time-based evidence alone. `None` when the file does not
    /// exist yet (there is nothing to anchor to; the caller treats that as
    /// unverifiable too).
    #[must_use]
    pub fn unverified_eof(path: &Path) -> Option<Self> {
        let identity = FileIdentity::of(path)?;
        let start = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Some(Self {
            identity,
            start,
            verified: false,
            head: hash_head(path, start),
        })
    }
}

/// Incremental reader over one transcript file.
///
/// Holds a byte offset and hands back whole lines only, so a half-written tail
/// is read on the next poll rather than parsed as truncated JSON.
///
/// A tail is either:
/// - **live** (`new`): a non-resume process owns the file from byte 0; every
///   read line is a current-process record.
/// - **resumed** (`resumed`): it carries a [`ResumeBoundary`]. Reads at/after
///   the boundary on the SAME file identity are current. If the file is
///   replaced (different dev/inode) or shrunk below the cursor, the boundary is
///   unverifiable from then on: the tail keeps following appends so
///   conversation hydration survives, but reports them [`TailProvenance::Unverified`]
///   and never re-opens the current-process gate (no effort applied, no fresh
///   switch settled) — read-back degrades to unknown rather than trusting a
///   rotated file. There is no respawn inside one driver run (D-026 resume is a
///   new driver/instance), so no later event can re-prove provenance here.
#[derive(Debug)]
pub struct TranscriptTail {
    path: PathBuf,
    offset: u64,
    partial: String,
    resume: Option<ResumeState>,
}

/// r7 item 4(c): a re-openable continuation point captured from a running
/// [`TranscriptTail`], keyed per (pid, session) by the promotion poller so a
/// re-promotion resumes at the last read offset instead of replaying the
/// sticky boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TailAnchor {
    /// File identity the offset is valid on.
    identity: FileIdentity,
    /// First unread byte at capture time.
    start: u64,
    /// The tail was reading Current (gate open). False = Unverified.
    current: bool,
    /// The tracked resume had already been displaced.
    displaced: bool,
    /// Head fingerprint at capture (None for a live tail).
    head: Option<HeadFingerprint>,
}

#[derive(Debug)]
struct ResumeState {
    /// File identity that must hold for reads past `start` to be trusted.
    identity: FileIdentity,
    /// Fingerprint of the file head at boundary capture.
    head: Option<HeadFingerprint>,
    /// The resume boundary is proven (pre-exec snapshot or other verifiable
    /// evidence). When false the resume is known but its boundary could not be
    /// proven: the tail follows current EOF and reports every batch
    /// [`TailProvenance::Unverified`] (conversation hydrates, effort gate
    /// stays closed) until a discontinuity — never Current.
    verified: bool,
    /// Set once the tracked file shrank, was replaced, or vanished.
    displaced: bool,
    /// The previous poll was an ENOENT: on the next poll an identity change is
    /// EXPECTED (a delete-and-recreate gets a new inode), so an intact head
    /// fingerprint means "same content restored" and the tail continues at the
    /// old offset instead of anchoring (r5 item 4).
    absent: bool,
}

/// Whether a batch of lines is provenanced to the current process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailProvenance {
    /// Lines are records of the current process; the read-back gate may open.
    Current,
    /// Resume tail displaced by a shrink/replacement: provenance unknown, the
    /// gate must stay closed. Lines (if any) hydrate messages only.
    Unverified,
}

/// How a transcript pump should treat one launch/detection epoch.
///
/// The three D-056 (4) states: a fresh session (tail from byte 0, current); a
/// verified resume (a proven [`ResumeBoundary`]); and an unverifiable resume
/// (a known `--resume` whose boundary could not be proven — no uuid, no
/// process-start evidence on macOS, an exec keeping the shell pid, a clock
/// step, or a transcript missing at spawn). The unverifiable resume hydrates
/// conversation from current EOF but NEVER opens the effort gate and never
/// takes the fresh byte-0/current path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// A brand-new session: every record on the file is current.
    Fresh,
    /// A resume with a proven boundary (pre-exec snapshot / process-start
    /// evidence trusted by the caller). `boundary.verified` is normally true;
    /// a `false` boundary is treated like [`ResumeMode::Unverified`].
    Boundary(ResumeBoundary),
    /// A known resume whose boundary could not be proven (no uuid, macOS with
    /// no start-time, an exec keeping the shell pid, a backward clock step, or
    /// a transcript missing at spawn). The tail anchors at current EOF and
    /// reports every batch unverified — conversation hydrates, the effort gate
    /// never opens.
    Unverified,
}

impl ResumeMode {
    /// Whether this is any kind of resume (verified or not).
    #[must_use]
    pub fn is_resume(&self) -> bool {
        !matches!(self, ResumeMode::Fresh)
    }

    /// Mode for a session that arrived through an AUTHENTICATED in-TUI
    /// SessionStart rebind (a `/resume <older-id>` or `/clear` in a live
    /// Claude): the new foreground must be bounded from ITS OWN transcript as
    /// it exists at the rebind, never from the launch argv and never byte 0.
    ///
    /// - absent or EMPTY (`len == 0`) → [`ResumeMode::Fresh`]: it is a brand
    ///   new session (`/clear`), every record it will write is current;
    /// - otherwise → an unverified EOF anchor: the existing bytes are another
    ///   session's history and must not replay as current, while bytes
    ///   appended after the rebind hydrate conversation only (the effort gate
    ///   stays closed for the epoch).
    #[must_use]
    pub fn rebound_mode(path: &Path) -> ResumeMode {
        let non_empty = std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && metadata.len() > 0)
            .unwrap_or(false);
        if non_empty && let Some(boundary) = ResumeBoundary::unverified_eof(path) {
            ResumeMode::Boundary(boundary)
        } else {
            ResumeMode::Fresh
        }
    }

    /// Open the tail for a bound transcript `path`. For an [`ResumeMode::Unverified`]
    /// resume, anchors at the file's CURRENT EOF unverified; if the file does
    /// not exist yet returns `None` so the pump retries next tick (never a live
    /// byte-0 tail).
    #[must_use]
    /// Open the tail for a bound transcript `path`.
    ///
    /// A captured unverified [`ResumeBoundary`] (r6 item 5) is used AS-IS: its
    /// anchor start and head were measured at the SessionStart/detection
    /// instant, so records appended between that instant and the first poll
    /// are read from the anchor (Unverified) instead of being skipped by a
    /// second EOF snapshot. A bare [`ResumeMode::Unverified`] (no captured
    /// boundary) snapshots the current EOF now; if the file is still absent
    /// the hydrator stays unbound and retries next tick (never byte 0).
    pub fn open_tail(&self, path: &Path) -> Option<TranscriptTail> {
        match self {
            ResumeMode::Fresh => Some(TranscriptTail::new(path.to_path_buf())),
            ResumeMode::Boundary(boundary) => {
                Some(TranscriptTail::resumed(path.to_path_buf(), *boundary))
            }
            ResumeMode::Unverified => {
                let boundary = ResumeBoundary::unverified_eof(path)?;
                Some(TranscriptTail::resumed(path.to_path_buf(), boundary))
            }
        }
    }
}

/// One poll: the whole lines appended since the last call and their provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailRead {
    /// Whole newline-terminated records appended since the last poll.
    pub lines: Vec<String>,
    /// Whether those lines are provenanced to the current process.
    pub provenance: TailProvenance,
}

impl TranscriptTail {
    /// Start at the beginning of `path`, treating every record as current.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: String::new(),
            resume: None,
        }
    }

    /// Follow a resumed transcript; provenance comes from `boundary.verified`.
    #[must_use]
    pub fn resumed(path: PathBuf, boundary: ResumeBoundary) -> Self {
        let verified = boundary.verified;
        Self {
            path,
            offset: boundary.start,
            partial: String::new(),
            resume: Some(ResumeState {
                identity: boundary.identity,
                head: boundary.head,
                verified,
                displaced: false,
                absent: false,
            }),
        }
    }

    /// r7 item 4(c): the last position a prior hydrator read this file to,
    /// captured per (pid, session) so a re-promotion can continue there
    /// instead of re-reading the sticky anchor and re-emitting the whole
    /// conversation.
    ///
    /// Returns `None` when the file is currently absent or its on-disk
    /// identity no longer matches the tracked one (a replacement/rotation):
    /// the caller reopens through the [`ResumeMode`] path, whose own
    /// shrink/replacement rules then apply.
    #[must_use]
    pub(crate) fn anchor(&self) -> Option<TailAnchor> {
        let on_disk = FileIdentity::of(&self.path)?;
        let tracked = match self.resume.as_ref() {
            Some(state) => {
                if state.identity != on_disk {
                    return None;
                }
                state.identity
            }
            None => on_disk,
        };
        let len = std::fs::metadata(&self.path).ok()?.len();
        if len < self.offset {
            return None;
        }
        Some(TailAnchor {
            identity: tracked,
            start: self.offset,
            // A live tail, or a verified, undisplaced resume tail, was reading
            // Current; everything else was reading Unverified.
            current: self
                .resume
                .as_ref()
                .is_none_or(|state| state.verified && !state.displaced),
            displaced: self.resume.as_ref().is_some_and(|state| state.displaced),
            head: self.resume.as_ref().and_then(|state| state.head),
        })
    }

    /// Reopen a tail continuing at a previously captured [`TailAnchor`].
    ///
    /// `None` when the file changed identity or shrank below the anchor; the
    /// caller falls back to [`ResumeMode::open_tail`]. A current anchor keeps
    /// the gate open past the offset; an unverified/displaced anchor keeps
    /// following appends as Unverified.
    #[must_use]
    pub(crate) fn continued(path: PathBuf, anchor: TailAnchor) -> Option<Self> {
        let on_disk = FileIdentity::of(&path)?;
        if on_disk != anchor.identity {
            return None;
        }
        let len = std::fs::metadata(&path).ok()?.len();
        if len < anchor.start {
            return None;
        }
        // The captured head (first HEAD_PROBE bytes at capture) keeps guarding
        // replacements after the reopen; a live tail captured none, so hash
        // the present head now.
        let head = anchor.head.or_else(|| hash_head(&path, len));
        Some(Self {
            path,
            offset: anchor.start,
            partial: String::new(),
            resume: Some(ResumeState {
                identity: anchor.identity,
                head,
                verified: anchor.current,
                displaced: anchor.displaced || !anchor.current,
                absent: false,
            }),
        })
    }

    /// Whether the resume boundary was proven (false for an unverified resume).
    #[must_use]
    pub fn verified(&self) -> bool {
        self.resume
            .as_ref()
            .is_none_or(|s| s.verified && !s.displaced)
    }

    /// File being followed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this is a resume tail whose boundary was invalidated by a shrink
    /// or replacement (read-back must stay unknown for the rest of the run).
    #[must_use]
    pub fn displaced(&self) -> bool {
        self.resume.as_ref().is_some_and(|state| state.displaced)
    }

    /// Read whatever whole lines have been appended since the last call.
    ///
    /// A live tail whose file shrank restarts at 0. A resume tail that shrinks
    /// or changes identity becomes [`TailProvenance::Unverified`] and follows
    /// only future appends; it never replays the restarted bytes as current.
    /// An open error (the bound file was deleted) is surfaced to the caller,
    /// which marks the binding degraded rather than silently rebinding.
    pub fn poll(&mut self) -> std::io::Result<TailRead> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A one-poll ENOENT (the bound file is being rotated/recreated)
                // is NOT the end of the transcript for a resume tail: keep the
                // hydrator alive at the stored anchor and return no lines. The
                // resume is already unproven (or becomes unverified) so a
                // restored file can never replay history as current.
                if let Some(state) = self.resume.as_mut() {
                    state.displaced = true;
                    state.absent = true;
                    return Ok(TailRead {
                        lines: Vec::new(),
                        provenance: TailProvenance::Unverified,
                    });
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let len = metadata.len();

        // A resume tail keeps the boundary honest.
        if let Some(state) = self.resume.as_mut() {
            // r6 item 5: being UNVERIFIED is NOT displacement. The captured
            // boundary (anchor start + head fingerprint, possibly an
            // unverified EOF from the SessionStart instant) is trusted as a
            // position; bytes appended between capture and the first poll are
            // read from the anchor and reported Unverified — they hydrate.
            // Only an actual change to the tracked file displaces: a rewritten
            // head, a different identity, or a shrink below the cursor.
            let identity = FileIdentity::from_metadata(&metadata);
            let head_intact = match state.head {
                Some(probe) => {
                    use std::io::{Read, Seek, SeekFrom};
                    let mut buf = vec![0_u8; probe.len as usize];
                    file.seek(SeekFrom::Start(0))
                        .ok()
                        .and_then(|_| file.read_exact(&mut buf).ok())
                        .is_some_and(|()| fingerprint_bytes(&buf, probe.len) == probe)
                }
                None => true,
            };
            if !state.displaced && (!head_intact || identity != state.identity || len < self.offset)
            {
                // Boundary genuinely lost. Re-anchor at the present EOF so a
                // recreated file's bytes are not replayed; from here appends
                // hydrate conversation only. Adopt the replacement's
                // identity/head so a later append is read as an append rather
                // than as yet another replacement.
                state.displaced = true;
                state.identity = identity;
                state.head = hash_head(&self.path, len);
                self.offset = len;
                self.partial.clear();
                return Ok(TailRead {
                    lines: Vec::new(),
                    provenance: TailProvenance::Unverified,
                });
            }
            // A displaced/unverified tail follows appends for message
            // hydration only, skipping any gap/truncation, never as Current.
            if state.displaced || !state.verified {
                let identity_now = FileIdentity::from_metadata(&metadata);
                // The head fingerprint guards every branch: on tmpfs a freed
                // inode is reused after a vanish AND a same-path rewrite can
                // keep the same identity, so identity/length alone cannot tell
                // a replacement from an append. After a vanish the SAME head
                // would mean the original file merely reappeared.
                let head_matches = match state.head {
                    Some(probe) => {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut buf = vec![0_u8; probe.len as usize];
                        file.seek(SeekFrom::Start(0))
                            .ok()
                            .and_then(|_| file.read_exact(&mut buf).ok())
                            .is_some_and(|()| fingerprint_bytes(&buf, probe.len) == probe)
                    }
                    None => true,
                };
                // After a vanish the inode may legitimately differ for an
                // identical restore; elsewhere an identity change is itself a
                // replacement signal.
                let replaced = !head_matches || (!state.absent && identity_now != state.identity);
                if replaced {
                    // A replacement — whether the file stayed present or
                    // returned after an ENOENT with DIFFERENT content — may
                    // carry replayed or already-published bytes. Anchor at the
                    // new file's EOF so nothing is re-emitted; only bytes
                    // appended AFTER this poll hydrate (Unverified).
                    self.offset = len;
                    self.partial.clear();
                    state.identity = identity_now;
                    state.head = hash_head(&self.path, len);
                } else {
                    // Same head — an identical restore after a vanish, or a
                    // plain append: continue at the stored offset so a
                    // delete-and-recreate with identical content re-emits zero
                    // messages. Adopt the restored inode (it legitimately
                    // differs after a recreate) or the next append would be
                    // misread as yet another replacement. A shrink still
                    // cannot seek backwards.
                    state.identity = identity_now;
                    if len < self.offset {
                        self.offset = len;
                        self.partial.clear();
                    }
                }
                state.absent = false;
                let lines = Self::read_lines(&mut file, len, &mut self.offset, &mut self.partial)?;
                return Ok(TailRead {
                    lines,
                    provenance: TailProvenance::Unverified,
                });
            }
            // Verified resume on the same identity/head: bytes past the anchor
            // are current-process records.
            let lines = Self::read_lines(&mut file, len, &mut self.offset, &mut self.partial)?;
            return Ok(TailRead {
                lines,
                provenance: TailProvenance::Current,
            });
        }

        // Live tail: a shorter file was rotated or truncated, so restart at 0.
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        let lines = Self::read_lines(&mut file, len, &mut self.offset, &mut self.partial)?;
        Ok(TailRead {
            lines,
            provenance: TailProvenance::Current,
        })
    }

    /// Seek to `offset`, read through `len`, advance the cursor and split off
    /// the complete newline-terminated lines, holding any partial tail.
    fn read_lines(
        file: &mut std::fs::File,
        len: u64,
        offset: &mut u64,
        partial: &mut String,
    ) -> std::io::Result<Vec<String>> {
        use std::io::{Read, Seek, SeekFrom};
        if len == *offset {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(*offset))?;
        let mut buf = Vec::with_capacity((len - *offset) as usize);
        let read = file.read_to_end(&mut buf)? as u64;
        *offset += read;
        partial.push_str(&String::from_utf8_lossy(&buf));
        let mut lines = Vec::new();
        while let Some(index) = partial.find('\n') {
            let line: String = partial.drain(..=index).collect();
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

    /// Append WITHOUT truncating (the production resume path only appends).
    /// `File::create` truncates and would turn an append test into a rotation
    /// test, so resume-tail tests must go through this.
    fn append(path: &Path, body: &str) {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open append");
        file.write_all(body.as_bytes()).expect("append");
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
        assert_eq!(
            tail.poll().expect("poll").lines,
            vec!["{\"a\":1}", "{\"b\":2}"]
        );
        // The partial line is held back until its newline arrives.
        assert_eq!(tail.poll().expect("poll").lines, Vec::<String>::new());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        file.write_all(b"al\":3}\n").expect("write");
        assert_eq!(tail.poll().expect("poll").lines, vec!["{\"partial\":3}"]);
    }

    #[test]
    fn truncation_restarts_a_live_tail_at_zero() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"a\":1}\n{\"b\":2}\n");
        let mut tail = TranscriptTail::new(path.clone());
        assert_eq!(tail.poll().expect("poll").lines.len(), 2);
        write(&path, "{\"c\":3}\n");
        assert_eq!(tail.poll().expect("poll").lines, vec!["{\"c\":3}"]);
    }

    #[test]
    fn a_resume_tail_reads_only_records_appended_after_the_pre_spawn_boundary() {
        // D-056 (4): the boundary is the transcript's identity + EOF captured
        // BEFORE the resumed child spawns; bytes already on disk stay history
        // and bytes appended after (even before the hydrator binds) are read.
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"old\":1}\n{\"old\":2}\n");
        let boundary = ResumeBoundary::snapshot(&path).expect("boundary");
        let mut tail = TranscriptTail::resumed(path.clone(), boundary);
        assert_eq!(tail.poll().expect("poll").lines, Vec::<String>::new());
        // Records appended between spawn and the (later) hydrator bind are not
        // skipped: the boundary cursor is the spawn-time EOF, not bind-time.
        append(&path, "{\"new\":3}\n");
        let read = tail.poll().expect("poll");
        assert_eq!(read.lines, vec!["{\"new\":3}"]);
        assert_eq!(read.provenance, TailProvenance::Current);
        assert!(!tail.displaced());
    }

    #[test]
    fn a_shorter_replacement_does_not_replay_history_as_current() {
        // Item 2: the file is replaced by something SHORTER (rotation/truncation).
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"old\":1}\n{\"old\":2}\n{\"old\":3}\n");
        let boundary = ResumeBoundary::snapshot(&path).expect("boundary");
        let mut tail = TranscriptTail::resumed(path.clone(), boundary);
        assert!(tail.poll().expect("poll").lines.is_empty());
        // Replace with a shorter file carrying a stale ultracode verdict.
        write(&path, "{\"old\":\"short\"}\n");
        let read = tail.poll().expect("poll");
        assert_eq!(read.lines, Vec::<String>::new(), "restart bytes suppressed");
        assert_eq!(read.provenance, TailProvenance::Unverified);
        assert!(tail.displaced());
        // Even later appends stay unverified: the gate never reopens.
        append(&path, "{\"also\":\"new\"}\n");
        let read = tail.poll().expect("poll");
        assert_eq!(read.lines, vec!["{\"also\":\"new\"}"]);
        assert_eq!(read.provenance, TailProvenance::Unverified);
    }

    #[test]
    fn an_equal_or_longer_replacement_does_not_replay_history_as_current() {
        // Item 2: same-length or LONGER replacement (the dangerous case a byte
        // comparison alone misses). On tmpfs delete+create frequently REUSES
        // the freed inode, so this is caught by the head fingerprint (the
        // swapped file rewrites the first record), not by identity or length.
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"old\":1}\n");
        let boundary = ResumeBoundary::snapshot(&path).expect("boundary");
        let mut tail = TranscriptTail::resumed(path.clone(), boundary);
        assert!(tail.poll().expect("poll").lines.is_empty());

        // Longer replacement at the same path (inode may be reused on tmpfs).
        std::fs::remove_file(&path).expect("remove");
        write(
            &path,
            "{\"replayed\":\"ultracode-on-longer-please\"}\n{\"x\":2}\n",
        );
        let read = tail.poll().expect("poll");
        assert_eq!(read.lines, Vec::<String>::new());
        assert_eq!(read.provenance, TailProvenance::Unverified);
        assert!(tail.displaced());

        // A second, equal-length swap keeps read-back unverified.
        std::fs::remove_file(&path).expect("remove");
        write(&path, "{\"replayed\":\"ultracode-on-equal\"}\n");
        let read = tail.poll().expect("poll");
        assert_eq!(read.provenance, TailProvenance::Unverified);
    }

    #[test]
    fn a_genuine_append_that_keeps_the_head_stays_current_even_if_the_inode_is_reused() {
        // The head fingerprint must NOT false-positive on a normal append: a
        // resumed process only ever appends, leaving the head byte-identical.
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"old\":1}\n");
        let boundary = ResumeBoundary::snapshot(&path).expect("boundary");
        let mut tail = TranscriptTail::resumed(path.clone(), boundary);
        assert!(tail.poll().expect("poll").lines.is_empty());
        append(&path, "{\"new\":2}\n");
        let read = tail.poll().expect("poll");
        assert_eq!(read.lines, vec!["{\"new\":2}"]);
        assert_eq!(read.provenance, TailProvenance::Current);
        assert!(!tail.displaced());
    }

    #[test]
    fn a_one_poll_enoent_keeps_the_resume_tail_and_comes_back_unverified() {
        // Item 6: a one-poll ENOENT (the bound file is being rotated/recreated)
        // does NOT end the tail: the hydrator keeps its anchor, the missing poll
        // returns no lines as Unverified, and only bytes appended AFTER the
        // restore hydrate conversation — never as Current.
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        write(&path, "{\"old\":1}\n");
        let boundary = ResumeBoundary::snapshot(&path).expect("boundary");
        let mut tail = TranscriptTail::resumed(path.clone(), boundary);
        assert!(tail.poll().expect("poll").lines.is_empty());
        std::fs::remove_file(&path).expect("remove");
        let missing = tail.poll().expect("ENOENT keeps the tail alive");
        assert!(missing.lines.is_empty());
        assert_eq!(missing.provenance, TailProvenance::Unverified);
        assert!(tail.displaced(), "the anchor is now unprovable");
        // The file returns with DIFFERENT content (a recreate/restore): anchor
        // at the restored EOF — its bytes are replayed/foreign and emit
        // nothing; only later appends hydrate, still Unverified. tmpfs may even
        // reuse the inode; the head fingerprint makes the replacement visible.
        write(&path, "{\"new\":1}\n{\"new\":2}\n");
        assert!(
            tail.poll().expect("poll after restore").lines.is_empty(),
            "restored bytes never replay"
        );
        append(&path, "{\"new\":3}\n");
        let read = tail.poll().expect("poll");
        assert_eq!(read.provenance, TailProvenance::Unverified);
        assert_eq!(read.lines, vec!["{\"new\":3}"]);
        // A present-file replacement anchors at its EOF too, then appends
        // hydrate.
        write(&path, "{\"rotated\":1}\n");
        assert!(tail.poll().expect("poll").lines.is_empty());
        append(&path, "{\"new\":4}\n");
        assert_eq!(tail.poll().expect("poll").lines, vec!["{\"new\":4}"]);
    }

    #[test]
    fn an_identical_restore_after_enoent_continues_at_the_old_offset_with_zero_dupes() {
        // r5 item 4: a delete-and-recreate (sync tool / copy-back / editor
        // save) that restores the SAME bytes must continue at the stored
        // offset — already-published records are never re-emitted. Only the
        // genuinely new suffix is read.
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("t.jsonl");
        let history = "{\"old\":1}\n";
        let current = "{\"current\":2}\n";
        write(&path, &format!("{history}{current}"));
        // Boundary after the history line: the current line was already read.
        // The head probe spans the file's current length (history + current).
        let snapshot = ResumeBoundary::snapshot(&path).expect("boundary");
        let boundary = ResumeBoundary {
            start: history.len() as u64,
            ..snapshot
        };
        let mut tail = TranscriptTail::resumed(path.clone(), boundary);
        let read = tail.poll().expect("poll");
        assert_eq!(read.lines, vec!["{\"current\":2}"]);
        assert_eq!(read.provenance, TailProvenance::Current);
        let eof = (history.len() + current.len()) as u64;
        // Vanish, then restore the identical content plus a new line.
        std::fs::remove_file(&path).expect("remove");
        assert!(tail.poll().expect("enotent").lines.is_empty());
        let fresh = "{\"current\":3}\n";
        write(&path, &format!("{history}{current}{fresh}"));
        let read = tail.poll().expect("poll after identical restore");
        // Exactly ONE line — neither the history nor the already-published
        // current record is re-emitted.
        assert_eq!(
            read.lines,
            vec!["{\"current\":3}"],
            "zero duplicated messages"
        );
        assert_eq!(read.provenance, TailProvenance::Unverified);
        let _ = eof;
    }
}
