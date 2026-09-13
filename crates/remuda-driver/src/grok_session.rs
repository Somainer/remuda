//! Read Grok's native session files without launching or controlling a session.
//!
//! This pre-spike is deliberately not connected to a driver. The eventual
//! `SignalAdapter` integration belongs to `docs/design/native-pty-first.md` §4.2.
//! ACP chunks are streaming evidence, not an approval transport; neither an
//! unknown event nor a tool result is interpreted as turn completion.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use remuda_acp_wire::{SessionUpdateKind, WireEvent, classify_session_update};
use serde::Deserialize;
use serde_json::Value;

/// A malformed source record. Unknown event tags are not errors.
#[derive(Debug, thiserror::Error)]
pub enum GrokParseError {
    /// Invalid JSON syntax.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The record cannot establish its expected envelope or identity.
    #[error("invalid Grok session record: {0}")]
    InvalidRecord(&'static str),
}

/// One persisted ACP frame, retaining the original JSON for future adapters.
#[derive(Debug, Clone, PartialEq)]
pub struct GrokUpdateRecord {
    /// Native Unix-seconds timestamp, when present.
    pub timestamp: Option<u64>,
    /// Native session ID, when supplied; never inferred from neighboring lines.
    pub session_id: Option<String>,
    /// Recognized update or unknown tag/method.
    pub update: GrokSessionUpdate,
    /// Complete source frame, including extension fields and metadata.
    pub frame: Value,
}

/// The subset of Grok ACP updates needed by the native-session pre-spike.
#[derive(Debug, Clone, PartialEq)]
pub enum GrokSessionUpdate {
    /// Assistant text or non-text content chunk, without lossy text conversion.
    AgentMessageChunk {
        /// ACP content block.
        content: Value,
    },
    /// Assistant reasoning chunk.
    AgentThoughtChunk {
        /// ACP content block.
        content: Value,
    },
    /// Native user-message echo; this does not establish queue provenance.
    UserMessageChunk {
        /// ACP content block.
        content: Value,
    },
    /// Tool invocation; original fields stay in `GrokUpdateRecord::frame`.
    ToolCall {
        /// Native tool correlation ID, never synthesized from a message ID.
        tool_call_id: Option<String>,
    },
    /// Tool progress/result; completion applies to the tool, not the turn.
    ToolCallUpdate {
        /// Native tool correlation ID.
        tool_call_id: Option<String>,
    },
    /// Grok extension outside the stable ACP update enum.
    HookExecution,
    /// Future update tag, or a non-`session/update` RPC method.
    Unknown {
        /// Unmodified native tag/method.
        kind: String,
    },
}

/// Decode one complete `updates.jsonl` frame using the wire crate's ACP types.
///
/// Unknown methods and update tags survive unchanged. Invalid JSON and missing
/// method/update discriminators return an error; callers can record that error
/// and continue with the next line. Permission RPCs remain unknown methods,
/// never an inferred permission decision. Missing session IDs stay unknown.
pub fn parse_update_line(line: &str) -> Result<GrokUpdateRecord, GrokParseError> {
    let frame: Value = serde_json::from_str(line)?;
    let method = frame["method"]
        .as_str()
        .filter(|method| !method.is_empty())
        .ok_or(GrokParseError::InvalidRecord("missing RPC method"))?;
    let params = &frame["params"];
    let session_id = string(params, "sessionId");
    let update = if matches!(method, "session/update" | "_x.ai/session/update") {
        if params["update"]["sessionUpdate"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            return Err(GrokParseError::InvalidRecord("missing sessionUpdate tag"));
        }
        let WireEvent::SessionUpdate {
            update_kind,
            update,
            ..
        } = classify_session_update(params.clone())
        else {
            return Err(GrokParseError::InvalidRecord("not a session update"));
        };
        match update_kind {
            SessionUpdateKind::AgentMessageChunk => GrokSessionUpdate::AgentMessageChunk {
                content: update["content"].clone(),
            },
            SessionUpdateKind::AgentThoughtChunk => GrokSessionUpdate::AgentThoughtChunk {
                content: update["content"].clone(),
            },
            SessionUpdateKind::UserMessageChunk => GrokSessionUpdate::UserMessageChunk {
                content: update["content"].clone(),
            },
            SessionUpdateKind::ToolCall => GrokSessionUpdate::ToolCall {
                tool_call_id: string(&update, "toolCallId"),
            },
            SessionUpdateKind::ToolCallUpdate => GrokSessionUpdate::ToolCallUpdate {
                tool_call_id: string(&update, "toolCallId"),
            },
            SessionUpdateKind::Unknown(ref kind) if kind == "hook_execution" => {
                GrokSessionUpdate::HookExecution
            }
            kind => GrokSessionUpdate::Unknown {
                kind: kind.tag().to_owned(),
            },
        }
    } else {
        GrokSessionUpdate::Unknown {
            kind: method.to_owned(),
        }
    };
    Ok(GrokUpdateRecord {
        timestamp: frame["timestamp"].as_u64(),
        session_id,
        update,
        frame,
    })
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// One native `events.jsonl` record, without inferred time or turn identity.
#[derive(Debug, Clone, PartialEq)]
pub struct GrokEventRecord {
    /// Original `ts` string, when supplied.
    pub timestamp: Option<String>,
    /// Session identity from this record only.
    pub session_id: Option<String>,
    /// Recognized lifecycle signal, or its unknown native type.
    pub event: GrokSessionEvent,
    /// Complete source record, including fields on unknown events.
    pub data: Value,
}

/// Native lifecycle signals; unrecognized terminal events stay unknown.
#[derive(Debug, Clone, PartialEq)]
pub enum GrokSessionEvent {
    /// A native turn began. Missing fields stay unknown.
    TurnStarted {
        /// Native turn number, including zero for the initial turn.
        turn_number: Option<u64>,
        /// Native model identifier.
        model_id: Option<String>,
        /// Whether Grok reports auto-approval for this turn.
        yolo_mode: Option<bool>,
    },
    /// Native phase text; no working/blocked mapping is inferred.
    PhaseChanged {
        /// Native phase value, when supplied.
        phase: Option<String>,
    },
    /// Native first-token signal, without a synthesized turn identity.
    FirstToken,
    /// An event outside the requested pre-spike subset.
    Unknown {
        /// Unmodified `type`, including `turn_ended` and future queue tags.
        kind: String,
    },
}

/// Decode one complete event line. Unknown event names and extra fields are
/// accepted; invalid JSON and absent event discriminators remain errors.
pub fn parse_event_line(line: &str) -> Result<GrokEventRecord, GrokParseError> {
    let data: Value = serde_json::from_str(line)?;
    let kind = data["type"]
        .as_str()
        .filter(|kind| !kind.is_empty())
        .ok_or(GrokParseError::InvalidRecord("missing event type"))?;
    let event = match kind {
        "turn_started" => GrokSessionEvent::TurnStarted {
            turn_number: data["turn_number"].as_u64(),
            model_id: string(&data, "model_id"),
            yolo_mode: data["yolo_mode"].as_bool(),
        },
        "phase_changed" => GrokSessionEvent::PhaseChanged {
            phase: string(&data, "phase"),
        },
        "first_token" => GrokSessionEvent::FirstToken,
        kind => GrokSessionEvent::Unknown {
            kind: kind.to_owned(),
        },
    };
    Ok(GrokEventRecord {
        timestamp: string(&data, "ts"),
        session_id: string(&data, "session_id"),
        event,
        data,
    })
}

/// An entry in Grok's native `active_sessions.json` registry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ActiveSession {
    /// Native session directory name.
    pub session_id: String,
    /// Native process ID; registry membership alone does not prove liveness.
    pub pid: u32,
    /// Native resolved working directory used for session-directory encoding.
    pub cwd: PathBuf,
    /// Original timestamp string, without parsing or freshness inference.
    pub opened_at: String,
}

/// Decode a registry snapshot. Extra fields are tolerated, but one malformed
/// entry rejects the snapshot so it cannot conceal an ambiguous selection.
pub fn parse_active_sessions(input: &str) -> Result<Vec<ActiveSession>, GrokParseError> {
    let sessions: Vec<ActiveSession> = serde_json::from_str(input)?;
    for session in &sessions {
        if session.pid == 0
            || !session.cwd.is_absolute()
            || session.opened_at.is_empty()
            || session.session_id.is_empty()
            || session.session_id.contains(['/', '\\'])
            || matches!(session.session_id.as_str(), "." | "..")
        {
            return Err(GrokParseError::InvalidRecord(
                "invalid active-session identity",
            ));
        }
    }
    Ok(sessions)
}

/// Read an explicit shadow/native home's registry. Missing files remain I/O
/// errors; malformed snapshots become `InvalidData` rather than empty results.
pub fn read_active_sessions(grok_home: &Path) -> std::io::Result<Vec<ActiveSession>> {
    let input = std::fs::read_to_string(grok_home.join("active_sessions.json"))?;
    parse_active_sessions(&input)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Exact native PID or working-directory lookup, never a newest-file guess.
#[derive(Debug, Clone, Copy)]
pub enum SessionSelector<'a> {
    /// Match the PID reported by Grok.
    Pid(u32),
    /// Match cwd, resolving filesystem aliases when both paths exist.
    Cwd(&'a Path),
}

/// A unique registry entry and its existing native session directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedSession {
    /// Native identity; consumers must separately establish process ownership.
    pub session: ActiveSession,
    /// Directory containing `updates.jsonl`, `events.jsonl`, and history files.
    pub directory: PathBuf,
}

/// Percent-encode the native cwd's UTF-8 bytes as Grok's session directory name.
/// The unreserved ASCII characters remain literal; all other bytes use uppercase
/// hex. Use the registry's cwd spelling, not an observer's path alias.
#[must_use]
pub fn encode_session_cwd(cwd: &Path) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in cwd.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// Locate only a unique matching registry entry. PID/cwd ambiguity yields
/// `None`, including when just one ambiguous entry has files. No PID liveness,
/// process reuse, or registry freshness claim is made. Directories containing
/// symlinks under `grok_home` are rejected rather than following them elsewhere.
pub fn locate_session(
    grok_home: &Path,
    selector: SessionSelector<'_>,
) -> std::io::Result<Option<LocatedSession>> {
    let mut found = None;
    for session in read_active_sessions(grok_home)? {
        let matches = match selector {
            SessionSelector::Pid(pid) => session.pid == pid,
            SessionSelector::Cwd(cwd) => {
                session.cwd == cwd
                    || std::fs::canonicalize(cwd)
                        .and_then(|cwd| std::fs::canonicalize(&session.cwd).map(|path| path == cwd))
                        .unwrap_or(false)
            }
        };
        if matches {
            if found.is_some() {
                return Ok(None);
            }
            found = Some(session);
        }
    }
    let Some(session) = found else {
        return Ok(None);
    };
    let mut directory = grok_home.to_path_buf();
    for part in [
        "sessions",
        &encode_session_cwd(&session.cwd),
        &session.session_id,
    ] {
        directory.push(part);
        if !std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Ok(None);
        }
    }
    Ok(Some(LocatedSession { session, directory }))
}

/// Incremental byte-offset tail for either native Grok JSONL file.
///
/// Only newline-terminated lines are returned, matching `TranscriptTail`.
/// Partial UTF-8 remains bytes until its complete line arrives. Shrinking a
/// file resets the offset and buffered partial line. Replacing it with a file
/// of equal or greater size requires constructing a new tail; no identity or
/// inode-based replacement detection is claimed.
#[derive(Debug)]
pub struct SessionTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl SessionTail {
    /// Start at byte zero of an updates or events file.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// File being followed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes read, including the incomplete line retained for the next poll.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read appended whole lines, skip empty lines, and trim CRLF endings.
    /// Malformed JSON is returned intact for the caller's per-line parser.
    /// Invalid UTF-8 in complete lines is replaced with the replacement char.
    pub fn poll(&mut self) -> std::io::Result<Vec<String>> {
        let mut file = std::fs::File::open(&self.path)?;
        if file.metadata()?.len() < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut appended = Vec::new();
        self.offset += file.read_to_end(&mut appended)? as u64;
        self.partial.extend_from_slice(&appended);
        let mut lines = Vec::new();
        let mut consumed = 0;
        for (index, byte) in self.partial.iter().enumerate() {
            if *byte == b'\n' {
                let line = String::from_utf8_lossy(&self.partial[consumed..index]);
                let line = line.trim_end_matches('\r');
                if !line.is_empty() {
                    lines.push(line.to_owned());
                }
                consumed = index + 1;
            }
        }
        self.partial.drain(..consumed);
        Ok(lines)
    }
}
