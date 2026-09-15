//! Hook discovery and execution for the three harness dialects.
//!
//! The fake honors the configuration surface Remuda's P1/P5 tests use, not the
//! real binaries' full loader:
//!
//! - **claude** — isolated user/project/local/CLI settings layers in the shape
//!   `{"hooks": {"EventName": [{"hooks": [{"type":"command","command":"…",
//!   "timeout": N}]}]}}`.
//! - **codex** — `$CODEX_HOME/hooks.json` with the same shape. The real 0.154
//!   client persists per-handler trust hashes; the fake deliberately does
//!   **not** emulate the trust gate (documented in `testing-fake-harness.md`).
//! - **grok** — every `*.json` directly under `$GROK_HOME/hooks/`, merged.
//!
//! Hook commands run as child processes with the payload JSON on stdin. Their
//! stdout is parsed for a decision object; a non-decision payload is ignored.
//! Environment variables carry the same identity names the real Grok hooks
//! receive (`GROK_HOOK_EVENT`, `GROK_SESSION_ID`, …).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// Hook event name (PascalCase, as written in settings files).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookEvent {
    /// Fires once at startup (and once for `--resume`).
    SessionStart,
    /// Fires when a prompt is admitted, including queue admission on claude.
    UserPromptSubmit,
    /// Fires before a tool call; may allow / deny / ask.
    PreToolUse,
    /// Blocking permission decision. Unsupported by grok, whose loader
    /// silently ignores the name exactly like the real binary.
    PermissionRequest,
    /// Fires after a tool result is recorded.
    PostToolUse,
    /// Fires per streamed assistant text line (claude dialect only).
    MessageDisplay,
    /// Fires at end of turn.
    Stop,
    /// Fires when the binary exits.
    SessionEnd,
}

impl HookEvent {
    /// PascalCase name used in settings files and as `hook_event_name`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::MessageDisplay => "MessageDisplay",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
        }
    }

    /// snake_case form Grok reports as `hookEventName`.
    #[must_use]
    pub fn snake(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::PostToolUse => "post_tool_use",
            Self::MessageDisplay => "message_display",
            Self::Stop => "stop",
            Self::SessionEnd => "session_end",
        }
    }
}

/// Harness dialect selecting config locations and accepted decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookKind {
    /// Claude Code `--settings` overlay.
    Claude,
    /// Codex `$CODEX_HOME/hooks.json`.
    Codex,
    /// Grok `$GROK_HOME/hooks/*.json`.
    Grok,
}

/// One command hook with its timeout.
#[derive(Clone, Debug)]
pub struct HookHandler {
    /// Shell command executed through `/bin/sh -c`.
    pub command: String,
    /// Maximum wait before the hook is treated as non-decisive.
    pub timeout: Duration,
    /// Source file, for diagnostics.
    pub source: PathBuf,
}

/// Resolved hook table: event name (PascalCase) → handlers in load order.
#[derive(Clone, Debug, Default)]
pub struct HookTable {
    handlers: BTreeMap<String, Vec<HookHandler>>,
}

impl HookTable {
    /// Load hooks for a dialect. Missing files/directories yield an empty table;
    /// malformed JSON is an error so a typo never silently disables a hook.
    pub fn load(
        kind: HookKind,
        settings: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<Self, String> {
        let mut table = Self::default();
        match kind {
            HookKind::Claude => {
                if let Some(path) = settings {
                    table.merge_file(path)?;
                }
            }
            HookKind::Codex => {
                if let Some(home) = home {
                    let path = home.join("hooks.json");
                    if path.is_file() {
                        table.merge_file(&path)?;
                    }
                }
            }
            HookKind::Grok => {
                if let Some(home) = home {
                    let dir = home.join("hooks");
                    if dir.is_dir() {
                        let mut files = Vec::new();
                        for entry in std::fs::read_dir(&dir)
                            .map_err(|err| format!("{}: {err}", dir.display()))?
                        {
                            let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
                            let path = entry.path();
                            if path.is_file()
                                && path.extension().and_then(|ext| ext.to_str()) == Some("json")
                            {
                                files.push(path);
                            }
                        }
                        files.sort();
                        for file in files {
                            table.merge_file(&file)?;
                        }
                    }
                }
            }
        }
        // Grok silently ignores PermissionRequest on registration, per the
        // captured evidence (grok-signals-1.md §A3).
        if kind == HookKind::Grok {
            table.handlers.remove(HookEvent::PermissionRequest.as_str());
        }
        Ok(table)
    }

    /// Load Claude fixture hooks from the same layers as renderer settings.
    /// Managed settings use an isolated fixture file, never system config.
    pub fn load_claude(home: &Path, cwd: &Path, overlay: Option<&Path>) -> Result<Self, String> {
        let settings = super::settings::merged_claude_settings(home, cwd, overlay)?;
        let mut table = Self::default();
        if settings.get("hooks").is_some() {
            table.merge_document(&settings, overlay.unwrap_or(home))?;
        }
        Ok(table)
    }

    fn merge_file(&mut self, path: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let doc: Value =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        self.merge_document(&doc, path)
    }

    fn merge_document(&mut self, doc: &Value, path: &Path) -> Result<(), String> {
        let Some(events) = doc.get("hooks").and_then(Value::as_object) else {
            return Err(format!(
                "{}: missing top-level \"hooks\" object",
                path.display()
            ));
        };
        for (event, groups) in events {
            let Some(groups) = groups.as_array() else {
                return Err(format!(
                    "{}: hooks.{event} must be an array",
                    path.display()
                ));
            };
            for group in groups {
                let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                    return Err(format!(
                        "{}: hooks.{event} group missing \"hooks\"",
                        path.display()
                    ));
                };
                for handler in handlers {
                    let command =
                        handler
                            .get("command")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                format!("{}: hook without string command", path.display())
                            })?;
                    let timeout_secs = handler.get("timeout").and_then(Value::as_u64).unwrap_or(60);
                    self.handlers
                        .entry(event.clone())
                        .or_default()
                        .push(HookHandler {
                            command: command.to_owned(),
                            timeout: Duration::from_secs(timeout_secs.clamp(1, 600)),
                            source: path.to_path_buf(),
                        });
                }
            }
        }
        Ok(())
    }

    /// Handlers registered for an event, in load order.
    #[must_use]
    pub fn handlers_for(&self, event: HookEvent) -> &[HookHandler] {
        self.handlers
            .get(event.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether any handler is registered for an event.
    #[must_use]
    pub fn has(&self, event: HookEvent) -> bool {
        !self.handlers_for(event).is_empty()
    }

    /// Number of registered handlers, used by the round-trip test.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handlers.values().map(Vec::len).sum()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handlers.values().all(Vec::is_empty)
    }
}

/// Outcome of firing one event.
#[derive(Clone, Debug)]
pub struct HookOutcome {
    /// Decoded decision, when a handler returned one.
    pub decision: Option<Value>,
    /// Raw stdout of the decisive handler, retained for diagnostics.
    pub stdout: String,
}

/// Context used to build identity environment variables for hook children.
#[derive(Clone, Debug)]
pub struct HookContext<'a> {
    /// Harness dialect.
    pub kind: HookKind,
    /// Session id.
    pub session_id: &'a str,
    /// Working directory.
    pub cwd: &'a Path,
}

impl HookTable {
    /// Fire every handler for `event` with `payload` on stdin. The first
    /// handler that prints a decision object wins; remaining handlers still
    /// run in order (observational hooks must never be skipped).
    pub fn fire(&self, event: HookEvent, payload: &Value, ctx: &HookContext<'_>) -> HookOutcome {
        let mut outcome = HookOutcome {
            decision: None,
            stdout: String::new(),
        };
        for handler in self.handlers_for(event) {
            if let Some(output) = run_handler(handler, event, payload, ctx) {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if outcome.decision.is_none()
                    && let Some(decision) = parse_decision(&stdout)
                {
                    outcome.decision = Some(decision);
                    outcome.stdout = stdout;
                }
            }
        }
        outcome
    }
}

#[cfg(unix)]
fn exited_ok() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(0)
}

fn run_handler(
    handler: &HookHandler,
    event: HookEvent,
    payload: &Value,
    ctx: &HookContext<'_>,
) -> Option<std::process::Output> {
    let mut child = Command::new("/bin/sh");
    child
        .arg("-c")
        .arg(&handler.command)
        .current_dir(ctx.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    child.env("CLAUDE_PROJECT_DIR", ctx.cwd);
    if ctx.kind == HookKind::Grok {
        child.env("GROK_HOOK_EVENT", event.snake());
        child.env("GROK_HOOK_NAME", handler.source.display().to_string());
        child.env("GROK_SESSION_ID", ctx.session_id);
        child.env("GROK_WORKSPACE_ROOT", ctx.cwd);
    }
    let mut child = child.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    match wait_with_timeout(&mut child, handler.timeout) {
        Some(output) => Some(output),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Some(std::process::Output {
                status: exited_ok(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }
}

#[cfg(unix)]
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => return None,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    };
    let mut stdout = Vec::new();
    if let Some(mut handle) = child.stdout.take() {
        use std::io::Read;
        let _ = handle.read_to_end(&mut stdout);
    }
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

#[cfg(not(unix))]
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    let _ = timeout;
    child.wait_with_output().ok()
}

/// Parse a hook's stdout for a decision. Both the claude `behavior` form and
/// the codex `hookSpecificOutput` wrapper are accepted; grok's `decision` form
/// (`allow` / `deny` / `ask`) is accepted too.
#[must_use]
pub fn parse_decision(stdout: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let looks_like_decision = value.get("behavior").is_some()
        || value.get("permissionDecision").is_some()
        || value.get("decision").is_some()
        || value.get("hookSpecificOutput").is_some();
    looks_like_decision.then_some(value)
}

/// Normalize a decision payload to `allow` / `deny` / `ask` / none.
#[must_use]
pub fn decision_behavior(value: &Value) -> Option<&str> {
    if let Some(behavior) = value.get("behavior").and_then(Value::as_str) {
        return Some(behavior);
    }
    if let Some(decision) = value.get("permissionDecision").and_then(Value::as_str) {
        return Some(match decision {
            "allow" | "allowThis" | "alwaysAllow" => "allow",
            "deny" => "deny",
            "ask" => "ask",
            other => other,
        });
    }
    if let Some(decision) = value.get("decision").and_then(Value::as_str) {
        return Some(decision);
    }
    value
        .pointer("/hookSpecificOutput/decision/behavior")
        .and_then(Value::as_str)
}

/// Build the common stdin payload fields every hook receives.
#[must_use]
pub fn base_payload(
    event: HookEvent,
    session_id: &str,
    cwd: &Path,
    timestamp: &str,
    extra: Value,
) -> Value {
    let mut payload = json!({
        "session_id": session_id,
        "sessionId": session_id,
        "cwd": cwd.to_string_lossy(),
        "hook_event_name": event.as_str(),
        "hookEventName": event.snake(),
        "timestamp": timestamp,
    });
    if let Some(obj) = payload.as_object_mut()
        && let Some(extra) = extra.as_object()
    {
        for (key, value) in extra {
            obj.insert(key.clone(), value.clone());
        }
    }
    payload
}
