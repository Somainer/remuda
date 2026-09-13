//! Locate and tail a native Claude transcript for a promoted terminal (D-025).
//!
//! The native TUI does not speak stream-json, but it writes the same `user` /
//! `assistant` records to `~/.claude/projects/<encoded cwd>/<session>.jsonl`
//! that [`crate::claude_print`] already maps. Finding that file and replaying it
//! is what gives a promoted `shell-pty` instance a real structured view instead
//! of a screen scrape.

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
/// the canonical path (`/tmp/x` → `/private/tmp/x`). Canonicalizing here is what
/// makes the lookup find a real session rather than an empty directory; when the
/// path cannot be resolved the literal spelling is used unchanged.
#[must_use]
pub fn project_dir(claude_home: &Path, cwd: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    claude_home
        .join("projects")
        .join(encode_project_dir(&resolved))
}

/// Resolve the transcript for a session detected at `detected_at`.
///
/// An explicit `session_id` (from the CLI's own `--session-id` / `--resume`) is
/// authoritative. Otherwise the newest `*.jsonl` in the project directory that
/// was last written at or after detection is adopted — a file that predates
/// detection belongs to an earlier session and is never claimed.
#[must_use]
pub fn locate_transcript(
    claude_home: &Path,
    cwd: &Path,
    session_id: Option<&str>,
    detected_at: SystemTime,
) -> Option<PathBuf> {
    let dir = project_dir(claude_home, cwd);
    if let Some(session) = session_id.filter(|value| !value.is_empty()) {
        let path = dir.join(format!("{session}.jsonl"));
        return path.is_file().then_some(path);
    }
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        // Tolerate coarse filesystem timestamps around the detection instant.
        if modified + GRACE < detected_at {
            continue;
        }
        if newest.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Timestamp slack allowed between a transcript's mtime and detection.
const GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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

    #[test]
    fn an_explicit_session_id_wins_over_mtime() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = Path::new("/work/repo");
        let dir = project_dir(tmp.path(), cwd);
        write(&dir.join("aaa.jsonl"), "{}\n");
        write(&dir.join("bbb.jsonl"), "{}\n");
        let found = locate_transcript(tmp.path(), cwd, Some("aaa"), SystemTime::UNIX_EPOCH)
            .expect("located");
        assert_eq!(found, dir.join("aaa.jsonl"));
        // An id with no file on disk resolves to nothing rather than the newest.
        assert_eq!(
            locate_transcript(tmp.path(), cwd, Some("missing"), SystemTime::UNIX_EPOCH),
            None
        );
    }

    #[test]
    fn without_a_session_id_the_newest_file_is_adopted() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = Path::new("/work/repo");
        let dir = project_dir(tmp.path(), cwd);
        write(&dir.join("old.jsonl"), "{}\n");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&dir.join("new.jsonl"), "{}\n");
        let found =
            locate_transcript(tmp.path(), cwd, None, SystemTime::UNIX_EPOCH).expect("located");
        assert_eq!(found, dir.join("new.jsonl"));
    }

    #[test]
    fn a_transcript_older_than_detection_is_not_adopted() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = Path::new("/work/repo");
        write(&project_dir(tmp.path(), cwd).join("stale.jsonl"), "{}\n");
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        assert_eq!(locate_transcript(tmp.path(), cwd, None, future), None);
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
