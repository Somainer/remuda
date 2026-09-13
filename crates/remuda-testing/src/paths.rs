//! Fixture and bundled-script locations.

use std::path::{Path, PathBuf};

/// Session id tests should pass to `--session-id` unless they need another one.
pub const FIXED_SESSION_ID: &str = "00000000-0000-4000-8000-000000000001";

/// Built-in playback scripts shipped with `fake-claude`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptKind {
    /// Assistant text `OK` plus one `result`.
    Ok,
    /// `can_use_tool` Bash, then allow/deny branches.
    Approval,
    /// `AskUserQuestion` round-trip.
    AskUser,
    /// Workflow `task_*` frames and two `result` lines.
    Workflow,
}

impl ScriptKind {
    /// File stem under `fixtures/scripts/`.
    pub fn file_stem(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Approval => "approval",
            Self::AskUser => "askuser",
            Self::Workflow => "workflow",
        }
    }

    /// Every bundled script, in the order tests usually run them.
    pub fn all() -> [Self; 4] {
        [Self::Ok, Self::Approval, Self::AskUser, Self::Workflow]
    }
}

/// Map `ok` / `approval` / `askuser` / `workflow` (and aliases) to a kind.
pub fn script_kind_from_name(name: &str) -> Option<ScriptKind> {
    match name.trim() {
        "ok" => Some(ScriptKind::Ok),
        "approval" => Some(ScriptKind::Approval),
        "askuser" | "ask" => Some(ScriptKind::AskUser),
        "workflow" | "wf" => Some(ScriptKind::Workflow),
        _ => None,
    }
}

/// Crate fixture root (`crates/remuda-testing/fixtures`).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Hook payloads recorded from a real `claude` run (D-028 §4.2).
///
/// One JSON object per line, `{event, ppid, payload}`, in the order a session
/// emits them. Host paths and session ids are scrubbed; the field *shapes* are
/// verbatim, which is the part tests depend on.
pub fn hook_session_fixture() -> &'static str {
    include_str!("../fixtures/hooks/claude-hook-session.jsonl")
}

/// Path to the recorded hook session, for tests that want to read it at runtime.
pub fn hook_session_path() -> PathBuf {
    fixtures_dir()
        .join("hooks")
        .join("claude-hook-session.jsonl")
}

/// Absolute path to a bundled script JSONL.
pub fn script_path(kind: ScriptKind) -> PathBuf {
    fixtures_dir()
        .join("scripts")
        .join(format!("{}.jsonl", kind.file_stem()))
}

/// Bundled NDJSON for a script kind.
pub fn script_source(kind: ScriptKind) -> &'static str {
    match kind {
        ScriptKind::Ok => include_str!("../fixtures/scripts/ok.jsonl"),
        ScriptKind::Approval => include_str!("../fixtures/scripts/approval.jsonl"),
        ScriptKind::AskUser => include_str!("../fixtures/scripts/askuser.jsonl"),
        ScriptKind::Workflow => include_str!("../fixtures/scripts/workflow.jsonl"),
    }
}

/// Captured `system/init` used as the fake's first stdout frame.
pub fn init_template() -> &'static str {
    include_str!("../fixtures/claude/claude-p-init.json")
}
