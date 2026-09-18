//! `fake-claude` process: flag parse, init frame, script playback, transcript.

use crate::flags::ClaudeFlags;
use crate::paths::{FIXED_SESSION_ID, init_template};
use crate::script::{ScriptStep, load_script_from_env, strip_helper_keys, when_filter};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Failures while impersonating `claude -p`.
#[derive(Debug, Error)]
pub enum FakeClaudeError {
    /// Invalid JSON on stdin or in a script line.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// IO on stdin/stdout/transcript.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Script path or contents.
    #[error("script: {0}")]
    Script(String),
    /// Host closed stdin while a `can_use_tool` was outstanding.
    #[error("stdin closed while waiting for control_response {0}")]
    UnexpectedEof(String),
}

/// Run the fake until stdin EOF. Returns the process exit code.
pub fn run_fake_claude() -> Result<i32, FakeClaudeError> {
    // `FAKE_CLAUDE_IGNORE_SIGTERM=1` must take effect **before any thread is
    // spawned**. SIGTERM is blocked on this thread, so the parent-watch thread
    // (below) inherits the blocked mask.
    //
    // Doing this only after `parent_watch::install` is too late: that watcher
    // thread would start with SIGTERM unblocked, and the process-directed
    // SIGTERM the close ladder sends at rung 2 would be delivered *there*,
    // killing the fake on the default disposition and making the SIGKILL-rung
    // test vacuous. Real parent death is still caught by the watcher's
    // `getppid()` poll; blocking the kernel's PDEATHSIG does not disable that.
    //
    // Gated by `cfg(unix)` along with the binding, since `block_sigterm` exists
    // only on unix: on another platform an unconditional binding would be an
    // unused-variable warning.
    #[cfg(unix)]
    if std::env::var("FAKE_CLAUDE_IGNORE_SIGTERM").is_ok_and(|value| value == "1") {
        block_sigterm();
    }

    // Exit with the spawner if the test is killed. The harness hands us a
    // piped stdin, so its death shows up as EOF anyway; pdeathsig and the
    // parent-pid poll cover the cases where stdin was redirected elsewhere.
    let _parent_watch = crate::parent_watch::install();
    // `FAKE_CLAUDE_STOP_READING=1`: after answering the `initialize` handshake,
    // never read stdin again. A child that stopped draining wedges the driver's
    // 64-slot writer channel, which is what made `close_stdin` itself able to
    // hang — a behaviour the close ladder has to bound.
    let stop_reading = std::env::var("FAKE_CLAUDE_STOP_READING").is_ok_and(|value| value == "1");
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
    {
        println!("2.1.268 (Claude Code)");
        return Ok(0);
    }
    let flags = ClaudeFlags::parse(std::env::args().skip(1));
    // `FAKE_CLAUDE_ARGV_FILE=<path>`: record the argv this process was actually
    // launched with, one token per line.
    //
    // A golden-argv test that asserts against a literal the driver never reads
    // proves only that the literal is self-consistent. This lets a test assert
    // on what the child *received*, which is the thing that would regress if
    // `-p` ever came back.
    if let Ok(path) = std::env::var("FAKE_CLAUDE_ARGV_FILE") {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let _ = std::fs::write(&path, argv.join("\n"));
    }
    // `FAKE_CLAUDE_GRANDCHILD_PID_FILE=<path>`: spawn one long-lived child in
    // **this process's group** and write its pid to the file.
    //
    // The close ladder's SIGKILL signals the whole group, but `Child::kill` /
    // `kill_on_drop` reach only the direct child. A grandchild therefore only
    // dies if rung 3's group signal fires, which makes that rung's coverage
    // structural rather than relying on `kill_on_drop` to hide a missing
    // `killpg`. The grandchild deliberately uses no `setsid`, so it inherits
    // the group `spawn_command` put the fake in; its handle is leaked (std does
    // not kill-on-drop by default), so the grandchild outlives a direct-child
    // kill and is reaped only by the group signal.
    #[cfg(unix)]
    let _grandchild = spawn_group_grandchild();
    let session_id = flags
        .session_id
        .clone()
        .unwrap_or_else(|| FIXED_SESSION_ID.to_string());
    let steps = load_script_from_env().map_err(FakeClaudeError::Script)?;
    let transcript = transcript_file(&session_id)?;
    let mut session = Session {
        flags,
        session_id,
        steps,
        cursor: 0,
        last_behavior: None,
        interrupted: false,
        saw_initialize: false,
        transcript,
    };
    session.emit_init()?;
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(line) = lines.next() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let incoming: Value = serde_json::from_str(trimmed)?;
        session.handle_incoming(incoming, &mut lines)?;
        // Handshake done; from here the child never drains stdin. The parent
        // stdin watch is suppressed for this knob (see parent_watch); the
        // getppid() poll still reaps us when the test really goes away.
        if stop_reading && session.saw_initialize {
            std::thread::sleep(std::time::Duration::from_secs(300));
        }
    }
    // `FAKE_CLAUDE_IGNORE_EOF=1`: do not leave when stdin closes.
    //
    // A real child can ignore stdin EOF indefinitely — mid-turn, or blocked on a
    // `can_use_tool` nobody answered — which is what made an unbounded `wait()`
    // in `Driver::close` hang forever. The close ladder has to be tested against
    // a child that actually behaves that way, so this makes the fake one.
    // Deliberately not driven by a script line: EOF handling is process
    // behaviour, not conversation.
    if std::env::var("FAKE_CLAUDE_IGNORE_EOF").is_ok_and(|value| value == "1") {
        // Park, but never forever: the parent-death watch installed above exits
        // with the spawner, and this cap keeps a leaked fake from outliving a
        // test run on a shared host.
        std::thread::sleep(std::time::Duration::from_secs(300));
    }
    Ok(0)
}

struct Session {
    flags: ClaudeFlags,
    session_id: String,
    steps: Vec<ScriptStep>,
    cursor: usize,
    last_behavior: Option<String>,
    interrupted: bool,
    /// The `initialize` control request has been answered; after this the
    /// `FAKE_CLAUDE_STOP_READING` knob parks without reading more stdin.
    saw_initialize: bool,
    transcript: Option<File>,
}

impl Session {
    fn emit_init(&mut self) -> Result<(), FakeClaudeError> {
        let mut init: Value = serde_json::from_str(init_template())?;
        if let Some(obj) = init.as_object_mut() {
            obj.insert("session_id".into(), json!(&self.session_id));
            obj.insert(
                "cwd".into(),
                json!(self.flags.cwd.to_string_lossy().into_owned()),
            );
            obj.insert("permissionMode".into(), json!(&self.flags.permission_mode));
            if let Some(model) = &self.flags.model {
                obj.insert("model".into(), json!(model));
            }
            obj.remove("messaging_socket_path");
            obj.remove("memory_paths");
            if let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut)
                && !tools.iter().any(|t| t.as_str() == Some("AskUserQuestion"))
            {
                tools.insert(1, json!("AskUserQuestion"));
            }
        }
        emit(&init)?;
        Ok(())
    }

    fn handle_incoming(
        &mut self,
        incoming: Value,
        lines: &mut impl Iterator<Item = io::Result<String>>,
    ) -> Result<(), FakeClaudeError> {
        let ty = incoming.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "keep_alive" | "control_cancel_request" => Ok(()),
            "control_request" => self.handle_control_request(&incoming),
            "control_response" => Ok(()),
            "user" => {
                if self.flags.replay_user_messages {
                    emit(&incoming)?;
                }
                self.append_transcript(&incoming)?;
                self.play_turn(lines)
            }
            _ => Ok(()),
        }
    }

    fn handle_control_request(&mut self, incoming: &Value) -> Result<(), FakeClaudeError> {
        let request_id = incoming
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let subtype = incoming
            .pointer("/request/subtype")
            .and_then(Value::as_str)
            .unwrap_or("");
        match subtype {
            "interrupt" => {
                self.interrupted = true;
                emit(&interrupt_ack(&request_id))?;
                self.emit_interrupt_result()?;
                Ok(())
            }
            "initialize" => {
                self.saw_initialize = true;
                emit(&initialize_success(&request_id))?;
                Ok(())
            }
            "set_permission_mode" | "set_model" => {
                emit(&control_success(&request_id, json!({})))?;
                Ok(())
            }
            _ => {
                emit(&control_success(&request_id, json!({})))?;
                Ok(())
            }
        }
    }

    fn play_turn(
        &mut self,
        lines: &mut impl Iterator<Item = io::Result<String>>,
    ) -> Result<(), FakeClaudeError> {
        self.interrupted = false;
        while self.cursor < self.steps.len() {
            if self.interrupted {
                self.emit_interrupt_result()?;
                return Ok(());
            }
            let step = self.steps[self.cursor].clone();
            self.cursor += 1;
            match step {
                ScriptStep::ExpectControlResponse { request_id } => {
                    let response = self.wait_control_response(&request_id, lines)?;
                    if self.interrupted {
                        self.emit_interrupt_result()?;
                        return Ok(());
                    }
                    self.last_behavior = permission_behavior(&response);
                }
                ScriptStep::EndTurn => return Ok(()),
                ScriptStep::Emit(frame) => {
                    if let Some(when) = when_filter(&frame) {
                        let got = self.last_behavior.as_deref().unwrap_or("allow");
                        if when != got {
                            continue;
                        }
                    }
                    let emitted = rewrite_placeholders(
                        strip_helper_keys(frame),
                        &self.session_id,
                        &self.flags.cwd,
                    );
                    emit(&emitted)?;
                    self.append_transcript(&emitted)?;
                }
            }
        }
        Ok(())
    }

    fn wait_control_response(
        &mut self,
        request_id: &str,
        lines: &mut impl Iterator<Item = io::Result<String>>,
    ) -> Result<Value, FakeClaudeError> {
        for line in &mut *lines {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let incoming: Value = serde_json::from_str(trimmed)?;
            let ty = incoming.get("type").and_then(Value::as_str).unwrap_or("");
            match ty {
                "keep_alive" | "control_cancel_request" => continue,
                "control_request" => {
                    let subtype = incoming
                        .pointer("/request/subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if subtype == "interrupt" {
                        let id = incoming
                            .get("request_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.interrupted = true;
                        emit(&interrupt_ack(id))?;
                        return Ok(incoming);
                    }
                    self.handle_control_request(&incoming)?;
                }
                "control_response" => {
                    let echoed = incoming
                        .pointer("/response/request_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if echoed == request_id {
                        return Ok(incoming);
                    }
                }
                _ => {}
            }
        }
        Err(FakeClaudeError::UnexpectedEof(request_id.to_string()))
    }

    fn emit_interrupt_result(&mut self) -> Result<(), FakeClaudeError> {
        let frame = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 0,
            "duration_api_ms": 0,
            "num_turns": 0,
            "result": "Interrupted",
            "stop_reason": "interrupt",
            "total_cost_usd": 0,
            "usage": { "input_tokens": 0, "output_tokens": 0 },
            "modelUsage": {},
            "permission_denials": [],
            "session_id": self.session_id,
            "uuid": Uuid::new_v4().to_string(),
            "result_index": 0
        });
        emit(&frame)?;
        self.append_transcript(&frame)?;
        Ok(())
    }

    fn append_transcript(&mut self, frame: &Value) -> Result<(), FakeClaudeError> {
        let Some(file) = self.transcript.as_mut() else {
            return Ok(());
        };
        let ty = frame.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(ty, "user" | "assistant" | "result") {
            return Ok(());
        }
        let mut record = frame.clone();
        if let Some(obj) = record.as_object_mut() {
            obj.insert("sessionId".into(), json!(&self.session_id));
            obj.entry("timestamp")
                .or_insert_with(|| json!("2026-09-12T00:00:00.000Z"));
        }
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        file.flush()?;
        Ok(())
    }
}

fn transcript_file(session_id: &str) -> Result<Option<File>, FakeClaudeError> {
    let Ok(dir) = std::env::var("FAKE_CLAUDE_TRANSCRIPT_DIR") else {
        return Ok(None);
    };
    if dir.is_empty() {
        return Ok(None);
    }
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_id}.jsonl"));
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(Some(file))
}

fn emit(value: &Value) -> Result<(), FakeClaudeError> {
    let mut out = io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string(value)?)?;
    out.flush()?;
    Ok(())
}

fn initialize_success(request_id: &str) -> Value {
    control_success(
        request_id,
        json!({
            "commands": [],
            "agents": [],
            "output_style": "default",
            "available_output_styles": ["default"],
            "models": [],
            "hooks_applied": true,
            "session_state": "idle"
        }),
    )
}

fn interrupt_ack(request_id: &str) -> Value {
    control_success(request_id, json!({ "still_queued": [] }))
}

fn control_success(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response
        }
    })
}

fn permission_behavior(response: &Value) -> Option<String> {
    response
        .pointer("/response/response/behavior")
        .or_else(|| response.pointer("/response/behavior"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn rewrite_placeholders(mut value: Value, session_id: &str, cwd: &Path) -> Value {
    rewrite_walk(&mut value, session_id, &cwd.to_string_lossy());
    value
}

fn rewrite_walk(value: &mut Value, session_id: &str, cwd: &str) {
    match value {
        Value::String(s) => {
            *s = s.replace("__SESSION__", session_id).replace("__CWD__", cwd);
        }
        Value::Array(items) => {
            for item in items {
                rewrite_walk(item, session_id, cwd);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                rewrite_walk(item, session_id, cwd);
            }
        }
        _ => {}
    }
}

/// Block SIGTERM for this process (test knob only).
///
/// `nix`'s `signal`/`sigaction` are `unsafe fn` because a *handler function* must
/// be async-signal-safe, and `unsafe` is forbidden workspace-wide. `sigprocmask`
/// is safe and, for a process that never unblocks the signal, observably the
/// same: SIGTERM stays pending and does not kill it, so a close ladder has to
/// escalate to SIGKILL.
#[cfg(unix)]
fn block_sigterm() {
    let mut set = nix::sys::signal::SigSet::empty();
    set.add(nix::sys::signal::Signal::SIGTERM);
    let _ =
        nix::sys::signal::sigprocmask(nix::sys::signal::SigmaskHow::SIG_BLOCK, Some(&set), None);
}

/// Spawn a long-lived `sleep` grandchild in this process's group and record its
/// pid. Unix only. Returns the child handle (kept alive by the caller so it is
/// not reaped early; it does not kill-on-drop). Returns `None` when not asked
/// for or when spawn fails — never fatal to the fake itself.
#[cfg(unix)]
fn spawn_group_grandchild() -> Option<std::process::Child> {
    let pid_path = std::env::var("FAKE_CLAUDE_GRANDCHILD_PID_FILE").ok()?;
    // No `process_group`/`setsid`: stay in the fake's group. `sleep` ignores
    // stdin, so it survives EOF like the IGNORE_EOF/IGNORE_SIGTERM parent.
    let child = std::process::Command::new("sleep")
        .arg("300")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let _ = std::fs::write(&pid_path, child.id().to_string());
    Some(child)
}
