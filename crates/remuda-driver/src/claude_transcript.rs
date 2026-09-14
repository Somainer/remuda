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
}
