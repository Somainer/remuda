//! effort-sync-4 scenarios: the decoupled ultracode model (claude >= 2.1.284).
//!
//! `SCENARIO=walk` runs one session launched with `--effort high` through every
//! `/effort` form plus the bare slider and one `/model`, with a short prompt
//! after each step. `SCENARIO=launch` runs the launch matrix, one session per
//! case (`CASES=a,b,c1,c2,d,e,f1,f2,g` selects; default all).
//!
//! Every session gets its own `CLAUDE_CONFIG_DIR` under `PROBE_DIR`, seeded
//! with the onboarding flag, folder trust for its cwd and logging hooks, so a
//! level "saved as your default" never touches the operator's own config and
//! the transcript carries no personal skills or MCP listings. One claude runs
//! at a time, and the next case starts only after the previous claude and its
//! process group are confirmed gone.
//!
//! Every process the probe starts directly is in the ledger
//! `PROBE_DIR/pids.txt`: the probe itself, each claude session (at spawn and
//! at confirmed termination), and every short helper (`claude --version`,
//! `hostname`, `scutil`, the pre-kill `ps`/`lsof`), which all go through
//! [`run_logged`]. Processes claude starts itself, such as its hook scripts,
//! are not in the ledger.
//!
//! Output per session, under `PROBE_DIR/<case>/`: the raw transcript copy,
//! the rendered vt100 screens and `actions.json` (per-step verdicts, dialog
//! text, attachments, next-assistant effort, CR-to-verdict latency). The
//! scrubbed copies a fixture is made from go to `PROBE_DIR/out/`; the run
//! panics if any operator identity survives the scrub.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use remuda_screen::Emulator;
use serde_json::{Value, json};

const COLS: u16 = 120;
const ROWS: u16 = 40;
const PROMPT: &str = "reply with exactly the two characters: ok";
const DIALOG_MARKERS: [&str; 2] = ["Change effort level?", "Change model?"];
const TRUST_MARKER: &str = "Is this a project you created or one you trust?";
/// Neutral prefix every scrubbed path is rewritten to.
const SCRUB_ROOT: &str = "/tmp/remuda-c-effortev";

#[derive(Clone, Debug)]
enum Action {
    Prompt,
    Cmd(String),
    /// Bare `/effort`, then Right, Tab, Enter in the slider.
    Slider,
}

struct Case {
    id: &'static str,
    /// Session directory; `f1`/`f2` share one so `--resume` finds the session.
    dir: &'static str,
    model: String,
    args: Vec<String>,
    actions: Vec<Action>,
}

struct Ctx {
    root: PathBuf,
    /// `pids.txt`: every process this probe starts directly.
    ledger: PathBuf,
    model: String,
    version: String,
    digits: String,
}

fn sleep_ms(ms: u64) -> tokio::time::Sleep {
    tokio::time::sleep(Duration::from_millis(ms))
}

pub async fn run(scenario: &str) {
    let given = PathBuf::from(
        std::env::var("PROBE_DIR").unwrap_or_else(|_| format!("/tmp/remuda-c-effortev-{scenario}")),
    );
    assert!(
        given
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("remuda-c-effortev"))
            && given.parent().is_some_and(|p| p == Path::new("/tmp"))
            && !given.is_symlink(),
        "refusing to clean {given:?}"
    );
    let model = std::env::var("REMUDA_PROBE_MODEL")
        .expect("REMUDA_PROBE_MODEL must name the model id to record with");
    let _ = std::fs::remove_dir_all(&given);
    std::fs::create_dir_all(given.join("out")).unwrap();
    let root = std::fs::canonicalize(&given).unwrap();
    // The ledger exists before the first subprocess, the version query.
    let ledger = root.join("pids.txt");
    append_pid(&ledger, std::process::id(), "probe", "self");
    let version = cli_version(&ledger);
    let digits: String = version.chars().filter(char::is_ascii_digit).collect();
    let ctx = Ctx {
        root,
        ledger,
        model,
        version,
        digits,
    };
    println!("probe claude {} model {}", ctx.version, ctx.model);

    let cases = match scenario {
        "walk" => vec![walk_case(&ctx)],
        _ => launch_cases(&ctx),
    };
    let only: Option<Vec<String>> = std::env::var("CASES")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_owned()).collect());
    let mut resume_id: Option<String> = None;
    for mut case in cases {
        if only
            .as_ref()
            .is_some_and(|o| !o.iter().any(|c| c == case.id))
        {
            continue;
        }
        if case.id == "f2" {
            let Some(sid) = resume_id.clone() else {
                println!("skip f2: f1 did not run");
                continue;
            };
            case.args = vec!["--resume".into(), sid];
        }
        let sid = run_case(&ctx, &case).await;
        if case.id == "f1" {
            resume_id = Some(sid);
        }
    }
    println!("DONE {}", ctx.root.display());
}

fn walk_case(ctx: &Ctx) -> Case {
    let mut actions = vec![Action::Prompt];
    for cmd in [
        "/effort ultracode",
        "/effort high",
        "/effort max",
        "/effort ultracode off",
        "/effort ultracode on",
        "/effort xhigh",
        "/effort auto",
        "/effort status",
        "/effort bogus",
        "/effort ultracode bogus",
    ] {
        actions.push(Action::Cmd(cmd.into()));
        actions.push(Action::Prompt);
    }
    actions.push(Action::Slider);
    actions.push(Action::Prompt);
    actions.push(Action::Cmd(format!("/model {}", ctx.model)));
    actions.push(Action::Prompt);
    Case {
        id: "walk",
        dir: "walk",
        model: ctx.model.clone(),
        args: vec!["--effort".into(), "high".into()],
        actions,
    }
}

fn launch_cases(ctx: &Ctx) -> Vec<Case> {
    let ultra = r#"{"ultracode":true}"#.to_owned();
    let status = || Action::Cmd("/effort status".into());
    let overlay = |dir: &str| {
        ctx.root
            .join(dir)
            .join("overlay.json")
            .display()
            .to_string()
    };
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    let mut cases = vec![
        Case {
            id: "a",
            dir: "a",
            model: ctx.model.clone(),
            args: vec![
                "--effort".into(),
                "high".into(),
                "--settings".into(),
                ultra.clone(),
            ],
            actions: vec![Action::Prompt, status()],
        },
        Case {
            id: "b",
            dir: "b",
            model: ctx.model.clone(),
            args: s(&["--effort", "ultracode"]),
            actions: vec![Action::Prompt, status()],
        },
        Case {
            id: "c1",
            dir: "c1",
            model: ctx.model.clone(),
            args: vec![
                "--settings".into(),
                overlay("c1"),
                "--settings".into(),
                ultra.clone(),
            ],
            actions: vec![Action::Prompt, status()],
        },
        Case {
            id: "c2",
            dir: "c2",
            model: ctx.model.clone(),
            args: vec![
                "--settings".into(),
                ultra.clone(),
                "--settings".into(),
                overlay("c2"),
            ],
            actions: vec![Action::Prompt, status()],
        },
        Case {
            id: "d",
            dir: "d",
            model: ctx.model.clone(),
            args: s(&["--settings", r#"{"disableWorkflows":true}"#]),
            actions: vec![
                Action::Cmd("/effort ultracode".into()),
                Action::Prompt,
                status(),
            ],
        },
        Case {
            id: "e",
            dir: "e",
            model: ctx.model.clone(),
            args: s(&["--settings", r#"{"maxEffortLevel":"high"}"#]),
            actions: vec![Action::Cmd("/effort max".into()), Action::Prompt, status()],
        },
        Case {
            id: "f1",
            dir: "f",
            model: ctx.model.clone(),
            args: s(&["--effort", "high"]),
            actions: vec![
                Action::Cmd("/effort ultracode on".into()),
                Action::Prompt,
                status(),
            ],
        },
        Case {
            id: "f2",
            dir: "f",
            model: ctx.model.clone(),
            args: Vec::new(),
            actions: vec![Action::Prompt, status()],
        },
    ];
    if let Ok(model) = std::env::var("REMUDA_PROBE_MODEL_NO_XHIGH") {
        // `/effort xhigh` after the refusal shows whether this model really
        // lacks xhigh, or only ultracode.
        cases.push(Case {
            id: "g",
            dir: "g",
            model,
            args: Vec::new(),
            actions: vec![
                Action::Prompt,
                Action::Cmd("/effort ultracode".into()),
                Action::Prompt,
                Action::Cmd("/effort xhigh".into()),
                Action::Prompt,
                status(),
            ],
        });
    }
    cases
}

fn cli_version(ledger: &Path) -> String {
    let out = run_logged(ledger, "claude", &["--version"]).expect("claude --version");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("version token")
        .to_owned()
}

fn append_pid(ledger: &Path, pid: u32, what: &str, label: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
        .unwrap();
    writeln!(file, "{pid} {what} {label}").unwrap();
}

/// The one way this probe runs a short helper: spawn, write its pid to the
/// ledger, wait for it, then write its exit status. Its output is returned,
/// never logged.
fn run_logged(
    ledger: &Path,
    program: &str,
    args: &[&str],
) -> std::io::Result<std::process::Output> {
    let label = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let pid = child.id();
    append_pid(ledger, pid, "helper", &label);
    let out = child.wait_with_output()?;
    append_pid(ledger, pid, "exited", &format!("{label} {}", out.status));
    Ok(out)
}

fn quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// Seed `<dir>/config` (onboarding done, cwd trusted, logging hooks) and the
/// overlay file case c uses. Idempotent so f2 reuses f1's directory as is.
fn prepare_dir(ctx: &Ctx, dir: &Path) {
    if dir.join("config").exists() {
        return;
    }
    let cwd = dir.join("ws");
    let config = dir.join("config");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let hook_script = dir.join("hook.sh");
    let hook_log = dir.join("hook.log");
    std::fs::write(
        &hook_script,
        format!(
            "#!/bin/bash\ninput=$(cat)\nprintf '%s %s\\n' \"$1\" \"$(printf '%s' \"$input\" | tr -d '\\n')\" >> {}\n",
            quote(&hook_log)
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let hook = |event: &str| {
        json!([{ "hooks": [{ "type": "command",
            "command": format!("{} {event}", quote(&hook_script)) }] }])
    };
    let mut hooks = serde_json::Map::new();
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "Notification"] {
        hooks.insert(event.into(), hook(event));
    }
    std::fs::write(
        config.join("settings.json"),
        serde_json::to_vec_pretty(&json!({ "theme": "dark", "hooks": hooks })).unwrap(),
    )
    .unwrap();
    let trusted = json!({
        "hasTrustDialogAccepted": true,
        "hasCompletedProjectOnboarding": true,
        "projectOnboardingSeenCount": 1,
    });
    let mut projects = serde_json::Map::new();
    projects.insert(cwd.display().to_string(), trusted);
    std::fs::write(
        config.join(".claude.json"),
        serde_json::to_vec_pretty(&json!({
            "hasCompletedOnboarding": true,
            "lastOnboardingVersion": ctx.version,
            "lastReleaseNotesSeen": ctx.version,
            "autoUpdates": false,
            "projects": projects,
        }))
        .unwrap(),
    )
    .unwrap();
    // Case c: a settings FILE that is observable on its own — a SessionStart
    // hook tagged OverlayFile, and an effort level no other layer sets.
    std::fs::write(
        dir.join("overlay.json"),
        serde_json::to_vec_pretty(&json!({
            "effortLevel": "medium",
            "hooks": { "SessionStart": hook("OverlayFileSessionStart") },
        }))
        .unwrap(),
    )
    .unwrap();
}

struct Session {
    label: String,
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    emulator: Arc<Mutex<Emulator>>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    transcript: PathBuf,
    session_id: String,
    pos: usize,
    hook_log: PathBuf,
    hook_pos: usize,
    hooks_seen: Vec<String>,
    t0: Instant,
    screens: Vec<(String, String)>,
}

impl Session {
    fn ms(&self) -> u128 {
        self.t0.elapsed().as_millis()
    }
    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
    fn grid(&self) -> String {
        self.emulator.lock().unwrap().grid().text()
    }
    fn capture(&mut self, label: &str) -> String {
        let text = self.grid();
        let trimmed = text.trim_end_matches('\n').to_owned();
        self.screens.push((label.to_owned(), trimmed.clone()));
        trimmed
    }
    /// New complete transcript records since the last call.
    fn drain(&mut self) -> Vec<Value> {
        let Ok(bytes) = std::fs::read(&self.transcript) else {
            return Vec::new();
        };
        if bytes.len() <= self.pos {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut advance = 0;
        for seg in bytes[self.pos..].split_inclusive(|b| *b == b'\n') {
            if !seg.ends_with(b"\n") {
                break;
            }
            advance += seg.len();
            if let Ok(value) = serde_json::from_slice::<Value>(seg) {
                out.push(value);
            }
        }
        self.pos += advance;
        out
    }
    /// New hook events (`<event> <payload>`) since the last call.
    fn hook_events(&mut self) -> Vec<(String, Value)> {
        let Ok(bytes) = std::fs::read(&self.hook_log) else {
            return Vec::new();
        };
        if bytes.len() <= self.hook_pos {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut advance = 0;
        for seg in bytes[self.hook_pos..].split_inclusive(|b| *b == b'\n') {
            if !seg.ends_with(b"\n") {
                break;
            }
            advance += seg.len();
            let line = String::from_utf8_lossy(seg);
            let (event, payload) = line.trim_end().split_once(' ').unwrap_or((&line, ""));
            out.push((
                event.to_owned(),
                serde_json::from_str(payload).unwrap_or(Value::Null),
            ));
            self.hooks_seen.push(event.to_owned());
        }
        self.hook_pos += advance;
        out
    }
    async fn type_line(&mut self, text: &str) -> u128 {
        self.write(text.as_bytes());
        sleep_ms(150).await;
        let cr = self.ms();
        self.write(b"\r");
        cr
    }

    /// After a submit CR: confirm a "Change …?" dialog if one paints, then
    /// wait for the `<local-command-stdout|stderr>` verdict record.
    async fn await_verdict(&mut self, command: &str, cr: u128, mut records: Vec<Value>) -> Value {
        let mut dialog: Option<(u128, String)> = None;
        let mut verdict: Option<(u128, Value)> = None;
        let mut slash: Option<(u128, String)> = None;
        while self.ms() < cr + 15_000 {
            for record in self.drain() {
                let text = content_text(&record);
                if slash.is_none() && text.contains("<command-name>/") {
                    slash = Some((self.ms() - cr, text.clone()));
                }
                if verdict.is_none()
                    && (text.contains("<local-command-stdout>")
                        || text.contains("<local-command-stderr>"))
                {
                    verdict = Some((self.ms() - cr, record.clone()));
                }
                records.push(record);
            }
            if verdict.is_some() {
                break;
            }
            let grid = self.grid();
            if dialog.is_none() && DIALOG_MARKERS.iter().any(|m| grid.contains(m)) {
                let seen = self.ms() - cr;
                sleep_ms(200).await;
                let full = self.capture(&format!("{command} — confirmation dialog"));
                dialog = Some((seen, dialog_excerpt(&full)));
                self.write(b"\r");
            }
            sleep_ms(10).await;
        }
        let confirm_at = dialog.as_ref().map(|(at, _)| *at + 200);
        sleep_ms(800).await;
        records.extend(self.drain());
        let after = self.capture(&format!("{command} — after verdict"));
        if verdict.is_none() && (after.contains("Esc to") || after.contains("Enter to")) {
            self.write(b"\x1b");
            sleep_ms(500).await;
        }
        let verdict_json = verdict.as_ref().map(|(at, record)| {
            json!({
                "cr_to_verdict_ms": at,
                "confirm_to_verdict_ms": confirm_at.map(|c| at.saturating_sub(c)),
                "text": content_text(record),
            })
        });
        println!(
            "[{}] {command}: dialog={} verdict={}",
            self.label,
            dialog.is_some(),
            verdict_json
                .as_ref()
                .and_then(|v| v["text"].as_str())
                .unwrap_or("MISSING")
        );
        json!({
            "action": command,
            "slash": slash.map(|(at, text)| json!({ "cr_to_record_ms": at, "text": text })),
            "dialog": dialog.map(|(at, text)| json!({ "cr_to_paint_ms": at, "text": text })),
            "verdict": verdict_json,
            "records": summarize(&records),
        })
    }

    async fn cmd(&mut self, command: &str) -> Value {
        let records = self.drain();
        self.hook_events();
        let cr = self.type_line(command).await;
        self.await_verdict(command, cr, records).await
    }

    async fn slider(&mut self) -> Value {
        let records = self.drain();
        self.hook_events();
        self.type_line("/effort").await;
        sleep_ms(1_500).await;
        self.capture("/effort (bare) — slider open");
        self.write(b"\x1b[C");
        sleep_ms(700).await;
        self.capture("/effort (bare) — after Right");
        self.write(b"\t");
        sleep_ms(700).await;
        self.capture("/effort (bare) — after Tab");
        let cr = self.ms();
        self.write(b"\r");
        self.await_verdict("/effort (bare) → Right, Tab, Enter", cr, records)
            .await
    }

    async fn prompt(&mut self) -> Value {
        let mut records = self.drain();
        self.hook_events();
        let cr = self.type_line(PROMPT).await;
        let mut first_assistant: Option<u128> = None;
        let mut stopped = false;
        while self.ms() < cr + 240_000 {
            for record in self.drain() {
                if first_assistant.is_none() && record["type"] == "assistant" {
                    first_assistant = Some(self.ms() - cr);
                }
                records.push(record);
            }
            if self.hook_events().iter().any(|(event, _)| event == "Stop") {
                stopped = true;
                break;
            }
            sleep_ms(20).await;
        }
        sleep_ms(800).await;
        records.extend(self.drain());
        self.capture("prompt — after turn");
        let assistant = records.iter().find(|r| r["type"] == "assistant");
        let next = assistant.map(|r| {
            json!({
                "effort": r.get("effort"),
                "perTurnEffort": r.get("perTurnEffort"),
                "version": r.get("version"),
                "model": r.pointer("/message/model"),
            })
        });
        println!(
            "[{}] prompt: stop={stopped} assistant={}",
            self.label,
            next.as_ref().map_or("MISSING".into(), Value::to_string)
        );
        json!({
            "action": "prompt",
            "cr_to_first_assistant_ms": first_assistant,
            "stop_hook": stopped,
            "next_assistant": next,
            "records": summarize(&records),
        })
    }

    /// End the session and return only once it is confirmed gone: `/exit`,
    /// then, if claude still runs after 15 s, a logged kill followed by a
    /// reap. claude leads its own process group (the PTY spawn runs
    /// `setsid`), so the group must drain too, which means no hook it started
    /// can still be writing. `Err` means termination could not be established
    /// and the caller aborts the matrix.
    async fn exit(mut self, ledger: &Path) -> Result<Self, String> {
        self.drain();
        self.write(b"/exit");
        sleep_ms(150).await;
        self.write(b"\r");
        let pid = self
            .pid
            .ok_or_else(|| format!("{}: claude pid unknown", self.label))?;
        let mut status = self.wait_exit(Duration::from_secs(15)).await?;
        if status.is_none() {
            // Ours, by pid from our own spawn: show what it is before the kill.
            describe_pid(ledger, pid);
            // SIGHUP, then SIGKILL after a short grace; it does not reap.
            match self.child.kill() {
                Ok(()) => append_pid(ledger, pid, "kill-sent", &self.label),
                Err(err) => {
                    append_pid(ledger, pid, "kill-failed", &format!("{} {err}", self.label))
                }
            }
            status = self.wait_exit(Duration::from_secs(10)).await?;
        }
        let Some(status) = status else {
            append_pid(ledger, pid, "unconfirmed", &self.label);
            return Err(format!(
                "claude pid {pid} ({}) still running after the kill",
                self.label
            ));
        };
        append_pid(ledger, pid, "exited", &format!("{} {status:?}", self.label));
        let deadline = Instant::now() + Duration::from_secs(10);
        while group_alive(pid) {
            if Instant::now() >= deadline {
                append_pid(ledger, pid, "group-alive", &self.label);
                return Err(format!(
                    "process group {pid} ({}) still has members after claude exited",
                    self.label
                ));
            }
            sleep_ms(100).await;
        }
        append_pid(ledger, pid, "group-drained", &self.label);
        Ok(self)
    }

    /// Poll (and so reap) the claude child for up to `within`.
    async fn wait_exit(&mut self, within: Duration) -> Result<Option<ExitStatus>, String> {
        let deadline = Instant::now() + within;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(Some(status)),
                Ok(None) if Instant::now() < deadline => sleep_ms(100).await,
                Ok(None) => return Ok(None),
                Err(err) => return Err(format!("{}: waiting for claude: {err}", self.label)),
            }
        }
    }
}

/// Whether any process is left in group `pgid`. Signal 0 delivers nothing;
/// only `ESRCH` proves the group empty, so any other answer counts as alive.
fn group_alive(pgid: u32) -> bool {
    let pgid = nix::unistd::Pid::from_raw(i32::try_from(pgid).unwrap_or(i32::MAX));
    !matches!(
        nix::sys::signal::killpg(pgid, None),
        Err(nix::errno::Errno::ESRCH)
    )
}

fn describe_pid(ledger: &Path, pid: u32) {
    let pid = pid.to_string();
    for (tool, args) in [
        ("ps", ["-o", "pid=,command=", "-p", pid.as_str()].as_slice()),
        (
            "lsof",
            ["-a", "-d", "cwd", "-Fn", "-p", pid.as_str()].as_slice(),
        ),
    ] {
        if let Ok(out) = run_logged(ledger, tool, args) {
            println!(
                "before kill ({tool}): {}",
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
    }
}

async fn launch(ctx: &Ctx, case: &Case, dir: &Path) -> Session {
    let cwd = dir.join("ws");
    let config = dir.join("config");
    let hook_log = dir.join("hook.log");
    let hook_pos = std::fs::metadata(&hook_log).map_or(0, |m| m.len() as usize);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let emulator = Arc::new(Mutex::new(Emulator::new(COLS, ROWS)));
    {
        let mut reader = pair.master.try_clone_reader().unwrap();
        let emulator = Arc::clone(&emulator);
        std::thread::spawn(move || {
            let mut chunk = vec![0_u8; 8192];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                emulator.lock().unwrap().feed(&chunk[..n]);
            }
        });
    }
    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(&cwd);
    cmd.arg("--model");
    cmd.arg(&case.model);
    for arg in &case.args {
        cmd.arg(arg);
    }
    cmd.env_clear();
    for name in ["PATH", "HOME", "LANG", "TMPDIR", "SHELL", "USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "C.UTF-8");
    }
    // The gateway credentials pass through by name only; their values never
    // reach this program's output.
    for name in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    ] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("CLAUDE_CONFIG_DIR", &config);
    cmd.env("DISABLE_AUTOUPDATER", "1");

    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let pid = child.process_id();
    append_pid(&ctx.ledger, pid.unwrap_or(0), "claude", case.id);
    println!(
        "[{}] spawned claude pid {pid:?} args {:?}",
        case.id, case.args
    );
    let writer = pair.master.take_writer().unwrap();
    let mut session = Session {
        label: case.id.to_owned(),
        _master: pair.master,
        writer,
        emulator,
        child,
        pid,
        transcript: PathBuf::new(),
        session_id: String::new(),
        pos: 0,
        hook_log,
        hook_pos,
        hooks_seen: Vec::new(),
        t0: Instant::now(),
        screens: Vec::new(),
    };
    while session.transcript.as_os_str().is_empty() {
        for (event, payload) in session.hook_events() {
            if event == "SessionStart"
                && let Some(path) = payload["transcript_path"].as_str()
            {
                session.transcript = PathBuf::from(path);
                session.session_id = payload["session_id"].as_str().unwrap_or("").to_owned();
            }
        }
        let grid = session.grid();
        assert!(
            !grid.contains(TRUST_MARKER),
            "folder-trust dialog despite the seeded config"
        );
        assert!(
            session.t0.elapsed() < Duration::from_secs(60),
            "no SessionStart within 60s:\n{grid}"
        );
        sleep_ms(100).await;
    }
    while !session.grid().contains('❯') {
        assert!(
            session.t0.elapsed() < Duration::from_secs(60),
            "no composer within 60s"
        );
        sleep_ms(100).await;
    }
    sleep_ms(1_500).await;
    // A resumed session appends to the same file: start reading at its end.
    session.pos = std::fs::metadata(&session.transcript).map_or(0, |m| m.len() as usize);
    session.capture("boot");
    session
}

async fn run_case(ctx: &Ctx, case: &Case) -> String {
    let dir = ctx.root.join(case.dir);
    prepare_dir(ctx, &dir);
    let mut session = launch(ctx, case, &dir).await;
    let mut steps = Vec::new();
    for action in &case.actions {
        let step = match action {
            Action::Prompt => session.prompt().await,
            Action::Cmd(command) => session.cmd(command).await,
            Action::Slider => session.slider().await,
        };
        steps.push(step);
    }
    let session = session
        .exit(&ctx.ledger)
        .await
        .unwrap_or_else(|err| panic!("aborting the matrix before the next case: {err}"));
    // Only a confirmed termination gets here, so the transcript is final.
    let report = json!({
        "case": case.id,
        "cli_version": ctx.version,
        "model": case.model,
        "launch_args": case.args,
        "hooks_seen": session.hooks_seen,
        "steps": steps,
    });
    let raw_transcript = std::fs::read_to_string(&session.transcript).unwrap_or_default();
    let screens = render_screens(&session.screens);
    let report_text = serde_json::to_string_pretty(&report).unwrap();
    std::fs::write(
        dir.join(format!("{}.transcript.jsonl", case.id)),
        &raw_transcript,
    )
    .unwrap();
    std::fs::write(dir.join(format!("{}.screens.txt", case.id)), &screens).unwrap();
    std::fs::write(dir.join(format!("{}.actions.json", case.id)), &report_text).unwrap();

    let mut scrub = Scrub::new(ctx, &dir, case.dir, &session.session_id);
    for text in [&raw_transcript, &report_text, &screens] {
        scrub.collect(text);
    }
    let stem = format!("effort-{}-{}", fixture_name(case.id), ctx.digits);
    let out = ctx.root.join("out");
    let transcript = scrub.apply(&redact_bundled(&raw_transcript));
    for (n, line) in transcript.lines().enumerate() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("{stem}:{} not JSON after scrub: {e}", n + 1));
    }
    let screens = scrub.apply(&screens);
    let report_text = scrub.apply(&report_text);
    for (name, body) in [
        (format!("{stem}.jsonl"), &transcript),
        (format!("{stem}.txt"), &screens),
        (format!("{stem}.actions.json"), &report_text),
    ] {
        scrub.check(&name, body);
        std::fs::write(out.join(name), body).unwrap();
    }
    println!("[{}] wrote {stem}.*", case.id);
    session.session_id
}

fn fixture_name(id: &str) -> String {
    if id == "walk" {
        "walk".into()
    } else {
        format!("launch-{id}")
    }
}

fn render_screens(screens: &[(String, String)]) -> String {
    let mut out = String::new();
    for (n, (label, text)) in screens.iter().enumerate() {
        out.push_str(&format!("===== {:02} {label} =====\n", n + 1));
        for line in text.lines() {
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// The lines of a rendered "Change …?" dialog, from its title down to the
/// key hint (or twelve rows), whitespace-trimmed.
fn dialog_excerpt(grid: &str) -> String {
    let lines: Vec<&str> = grid.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|l| DIALOG_MARKERS.iter().any(|m| l.contains(m)))
    else {
        return String::new();
    };
    let mut out = Vec::new();
    for line in lines.iter().skip(start).take(12) {
        let line = line.trim();
        if !line.is_empty() {
            out.push(line);
        }
        if line.contains("Esc to") || line.contains("Enter to confirm") {
            break;
        }
    }
    out.join("\n")
}

/// Bundled CLI text is not ours to publish and says nothing about effort:
/// cut the system prompt and tool schemas out of `prompt_snapshot`
/// attachments, and the body of a bundled skill a turn expands (`isMeta`).
/// Each value's exact raw JSON span is replaced, so every other byte of the
/// line stays as the CLI wrote it.
fn redact_bundled(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let Ok(record) = serde_json::from_str::<Value>(body) else {
            out.push_str(line);
            continue;
        };
        let snapshot = record["type"] == "attachment"
            && record.pointer("/attachment/type") == Some(&json!("prompt_snapshot"));
        let skill_body = record["type"] == "user"
            && record["isMeta"] == true
            && record
                .pointer("/message/content")
                .is_some_and(Value::is_array);
        let mut redacted = body.to_owned();
        if snapshot {
            redacted = redact_key(&redacted, "systemPrompt", "Claude Code system prompt");
            redacted = redact_key(&redacted, "tools", "tool definitions");
        }
        if skill_body {
            redacted = redact_key(&redacted, "text", "bundled skill body");
        }
        out.push_str(&redacted);
        if line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Replace every value of `"key":` in one JSON line with a same-typed
/// placeholder naming what was cut and its size.
fn redact_key(line: &str, key: &str, what: &str) -> String {
    let needle = format!("\"{key}\":");
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(&needle) {
        let start = at + needle.len();
        let mut stream = serde_json::Deserializer::from_str(&rest[start..]).into_iter::<Value>();
        let Some(Ok(value)) = stream.next() else {
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            continue;
        };
        let end = start + stream.byte_offset();
        let note = format!("[redacted: {what}, {} bytes]", end - start);
        let placeholder = match value {
            Value::Array(items) => json!([format!("{note}, {} items", items.len())]),
            _ => json!(note),
        };
        out.push_str(&rest[..start]);
        out.push_str(&placeholder.to_string());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn content_text(record: &Value) -> String {
    match record.pointer("/message/content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Compact per-record view for `actions.json`: type, and the fields that
/// carry effort state (attachments verbatim, user text, assistant effort).
fn summarize(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .map(|r| {
            let kind = r["type"].as_str().unwrap_or("?");
            match kind {
                "attachment" => {
                    let att = &r["attachment"];
                    let sub = att["type"].as_str().unwrap_or("?").to_ascii_lowercase();
                    if ["ultra", "effort", "workflow", "model"]
                        .iter()
                        .any(|k| sub.contains(k))
                    {
                        json!({ "type": kind, "attachment": att })
                    } else {
                        json!({ "type": kind, "attachment_type": sub })
                    }
                }
                "user" => {
                    let text = content_text(r);
                    json!({ "type": kind, "text": text.chars().take(400).collect::<String>() })
                }
                "assistant" => json!({
                    "type": kind,
                    "effort": r.get("effort"),
                    "perTurnEffort": r.get("perTurnEffort"),
                    "content": r.pointer("/message/content/0/type"),
                }),
                "system" => json!({
                    "type": kind,
                    "subtype": r.get("subtype"),
                    "content": r.get("content"),
                }),
                _ => json!({ "type": kind }),
            }
        })
        .collect()
}

/// Scrubber: identity needles → neutral placeholders, deterministic ids.
struct Scrub {
    pairs: Vec<(String, String)>,
    forbidden: Vec<String>,
    tag: String,
    uuids: Vec<String>,
    msg_ids: Vec<String>,
    req_ids: Vec<String>,
    signatures: Vec<String>,
}

impl Scrub {
    fn new(ctx: &Ctx, dir: &Path, tag: &str, session_id: &str) -> Self {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut forbidden: Vec<String> = Vec::new();
        let neutral_dir = format!("{SCRUB_ROOT}/{tag}");
        let encode = |p: &str| p.replace(['/', '.'], "-");
        // Probe paths: canonical and as given, plus the encoded project name.
        let canonical = dir.display().to_string();
        let given = canonical.trim_start_matches("/private").to_owned();
        for path in [&canonical, &given] {
            pairs.push((path.clone(), neutral_dir.clone()));
            pairs.push((
                encode(&format!("{path}/ws")),
                encode(&format!("{neutral_dir}/ws")),
            ));
        }
        if let Ok(home) = std::env::var("HOME") {
            pairs.push((encode(&home), "-home-user".into()));
            pairs.push((home.clone(), "/home/user".into()));
            forbidden.push(home);
        }
        if let Ok(tmp) = std::env::var("TMPDIR") {
            let tmp = tmp.trim_end_matches('/').to_owned();
            if tmp.len() > 5 {
                pairs.push((format!("/private{tmp}"), "/tmp".into()));
                pairs.push((tmp.clone(), "/tmp".into()));
                forbidden.push(tmp);
            }
        }
        for host in hostnames(&ctx.ledger) {
            pairs.push((host.clone(), "devbox".into()));
            forbidden.push(host);
        }
        if let Ok(user) = std::env::var("USER")
            && user.len() >= 4
        {
            pairs.push((user.clone(), "user".into()));
            forbidden.push(user);
        }
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            let url = url.trim_end_matches('/').to_owned();
            let host = url
                .split("://")
                .nth(1)
                .unwrap_or(&url)
                .split(['/', ':'])
                .next()
                .unwrap_or("")
                .to_owned();
            pairs.push((url.clone(), "https://gateway.invalid".into()));
            forbidden.push(url);
            if host.len() >= 4 {
                pairs.push((host.clone(), "gateway.invalid".into()));
                forbidden.push(host);
            }
        }
        for name in ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"] {
            if let Ok(token) = std::env::var(name)
                && token.len() >= 8
            {
                pairs.push((token.clone(), "[redacted-token]".into()));
                forbidden.push(token);
            }
        }
        if !session_id.is_empty() {
            forbidden.push(session_id.to_owned());
        }
        // Gateway-side model ids the operator's own cache knows: never in a
        // fixture except the public id this run was asked to record with.
        for (n, id) in gateway_model_ids(&ctx.model).into_iter().enumerate() {
            pairs.push((id.clone(), format!("acme_hub/model_x_{}", n + 1)));
            forbidden.push(id);
        }
        pairs.retain(|(needle, _)| needle.len() >= 3);
        Self {
            pairs,
            forbidden,
            tag: tag.replace(|c: char| !c.is_ascii_alphanumeric(), ""),
            uuids: Vec::new(),
            msg_ids: Vec::new(),
            req_ids: Vec::new(),
            signatures: Vec::new(),
        }
    }

    /// Learn the per-record ids to replace: uuids anywhere, message ids,
    /// request ids and thinking signatures. Call for every text before the
    /// first `apply`, so all outputs share one numbering.
    fn collect(&mut self, raw: &str) {
        fn add(list: &mut Vec<String>, value: &str) {
            if !list.iter().any(|v| v == value) {
                list.push(value.to_owned());
            }
        }
        for line in raw.lines() {
            for uuid in find_uuids(line) {
                add(&mut self.uuids, &uuid);
            }
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(id) = record.pointer("/message/id").and_then(Value::as_str) {
                add(&mut self.msg_ids, id);
            }
            if let Some(id) = record.get("requestId").and_then(Value::as_str) {
                add(&mut self.req_ids, id);
            }
            let signatures = &mut self.signatures;
            walk_strings(&record, &mut |key, value| {
                if key == "signature" && value.len() > 16 {
                    add(signatures, value);
                }
            });
        }
    }

    fn apply(&self, text: &str) -> String {
        let tag = &self.tag;
        let mut pairs: Vec<(String, String)> = self.pairs.clone();
        for (n, uuid) in self.uuids.iter().enumerate() {
            pairs.push((
                uuid.clone(),
                format!("e4e4e4e4-effc-4000-8000-{:012x}", n + 1),
            ));
        }
        for (n, id) in self.msg_ids.iter().enumerate() {
            pairs.push((
                id.clone(),
                format!("msg_recorded_effort4_{tag}_{:02}", n + 1),
            ));
        }
        for (n, id) in self.req_ids.iter().enumerate() {
            pairs.push((
                id.clone(),
                format!("req_recorded_effort4_{tag}_{:02}", n + 1),
            ));
        }
        for sig in &self.signatures {
            pairs.push((sig.clone(), "recorded-signature-redacted".into()));
        }
        pairs.sort_by_key(|(needle, _)| std::cmp::Reverse(needle.len()));
        let mut out = text.to_owned();
        for (needle, replacement) in pairs {
            out = out.replace(needle.as_str(), &replacement);
        }
        out
    }

    /// Panic (naming only the needle index) if any identity survived.
    fn check(&self, name: &str, body: &str) {
        let lower = body.to_lowercase();
        for (n, needle) in self.forbidden.iter().chain(&self.uuids).enumerate() {
            if lower.contains(&needle.to_lowercase()) {
                panic!("scrub check: forbidden needle #{n} survives in {name}");
            }
        }
    }
}

fn hostnames(ledger: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for (program, args) in [
        ("hostname", [].as_slice()),
        ("hostname", ["-s"].as_slice()),
        ("scutil", ["--get", "LocalHostName"].as_slice()),
        ("scutil", ["--get", "ComputerName"].as_slice()),
    ] {
        if let Ok(o) = run_logged(ledger, program, args) {
            let host = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if host.len() >= 4 && !out.contains(&host) {
                out.push(host);
            }
        }
    }
    out
}

/// Model ids from the operator's gateway model cache (read, never printed).
fn gateway_model_ids(allowed: &str) -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let cache = PathBuf::from(home).join(".claude").join("cache");
    let mut ids: Vec<String> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&cache) else {
        return ids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("gateway-models") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        walk_strings(&value, &mut |key, v| {
            if key == "id"
                && v.len() >= 4
                && v != allowed
                && !v.contains(' ')
                && !ids.iter().any(|i| i == v)
            {
                ids.push(v.to_owned());
            }
        });
    }
    // Public first-party ids are not identifying; keep them readable.
    ids.retain(|id| id.contains('/') || !id.starts_with("claude-"));
    ids
}

fn walk_strings(value: &Value, f: &mut dyn FnMut(&str, &str)) {
    fn go(key: &str, value: &Value, f: &mut dyn FnMut(&str, &str)) {
        match value {
            Value::String(s) => f(key, s),
            Value::Array(items) => {
                for item in items {
                    go(key, item, f);
                }
            }
            Value::Object(map) => {
                for (k, v) in map {
                    go(k, v, f);
                }
            }
            _ => {}
        }
    }
    go("", value, f);
}

/// Every 8-4-4-4-12 hex uuid in `text`, in order of appearance.
fn find_uuids(text: &str) -> Vec<String> {
    const SHAPE: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 36 <= bytes.len() {
        let mut j = i;
        let mut ok = i == 0 || !bytes[i - 1].is_ascii_hexdigit();
        for (n, len) in SHAPE.iter().enumerate() {
            if !ok {
                break;
            }
            ok = bytes[j..j + len].iter().all(u8::is_ascii_hexdigit);
            j += len;
            if n < 4 {
                ok = ok && bytes[j] == b'-';
                j += 1;
            }
        }
        ok = ok && (j == bytes.len() || !bytes[j].is_ascii_hexdigit());
        if ok {
            out.push(text[i..j].to_owned());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}
