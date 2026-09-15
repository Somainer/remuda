//! Live probe v3 — effort switch channels on the REAL claude TUI (2.1.272).
//!
//!   PROBE_DIR=/tmp/remuda-r-effortsync2-probe3 \
//!   cargo run -p remuda-driver --example effort_probe
//!
//! Scenario (all switches while idle except F):
//!   base  prompt (baseline assistant record)
//!   A     /effort xhigh  confirm  → prompt     (read-back channels raced)
//!   B     /effort max    ESC      (dismiss: does anything change/journal?)
//!   C     /effort ultracode confirm → prompt    (slash? ultra attachment?)
//!   D     /effort high   confirm  (ultra_effort_exit?)
//!   E     /effort bogus           (screen toast; transcript record?)
//!   F     prompt then immediate /effort medium while working (native queue)
//!
//! Marks are `T<phase> <ms since boot> <detail>`; channel marks are relative
//! to the submit CR: ch-* +Nms.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[derive(Clone)]
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Screen {
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn text(&self) -> String {
        let raw = self.0.lock().unwrap();
        String::from_utf8_lossy(&strip_ansi(raw.as_slice())).into_owned()
    }
    fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }
}

fn strip_ansi(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b == 0x1b && i + 1 < raw.len() && raw[i + 1] == b'[' {
            i += 2;
            while i < raw.len() {
                let c = raw[i];
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break;
                }
            }
        } else if b == 0x1b && i + 1 < raw.len() && raw[i + 1] == b']' {
            i += 2;
            while i < raw.len() && raw[i] != 0x07 {
                if raw[i] == 0x1b && i + 1 < raw.len() && raw[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            if i < raw.len() && raw[i] == 0x07 {
                i += 1;
            }
        } else if b == 0x1b {
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

struct Probe {
    writer: Box<dyn Write + Send>,
    screen: Screen,
    transcript: PathBuf,
    hook_log: PathBuf,
    t0: Instant,
    log: Arc<Mutex<Vec<(String, u128, String)>>>,
}

impl Probe {
    fn mark(&self, phase: &str, detail: &str) {
        let ms = self.t0.elapsed().as_millis();
        println!("T{phase} {ms} {detail}");
        self.log
            .lock()
            .unwrap()
            .push((phase.to_owned(), ms, detail.to_owned()));
    }
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
    async fn sleep_ms(ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    async fn submit(&mut self, body: &str) {
        self.write_bytes(body.as_bytes());
        Self::sleep_ms(120).await;
        self.write_bytes(b"\r");
    }

    /// Race the read-back channels after `start`: transcript records matching
    /// `kinds`, the screen needle, and hook events. Marks each first arrival.
    #[allow(clippy::too_many_arguments)]
    async fn race(
        &self,
        start: u128,
        window_ms: u64,
        screen_needle: Option<&str>,
        record_match: impl Fn(&serde_json::Value, &str) -> Option<(String, String)>,
        hook_pos: &mut u64,
        tag: &str,
        labels: &[&str],
    ) -> Vec<(String, u128, String)> {
        let deadline = now_ms() + u128::from(window_ms);
        let mut hits: BTreeMap<String, (u128, String)> = BTreeMap::new();
        let mut pos = std::fs::metadata(&self.transcript)
            .map(|m| m.len())
            .unwrap_or(0);
        while now_ms() < deadline {
            // Screen.
            if let Some(needle) = screen_needle
                && !hits.contains_key("screen")
                && self.screen.contains(needle)
            {
                hits.insert(
                    "screen".into(),
                    (self.t0.elapsed().as_millis() - start, needle.into()),
                );
            }
            // Transcript.
            if let Ok(content) = std::fs::read_to_string(&self.transcript) {
                let bytes = content.as_bytes();
                if (bytes.len() as u64) > pos {
                    let tail = &content[pos as usize..];
                    let mut advance = 0;
                    for seg in tail.split_inclusive('\n') {
                        if !seg.ends_with('\n') {
                            break;
                        }
                        advance += seg.len();
                        let body = seg.trim_end_matches(['\n', '\r']);
                        if body.is_empty() {
                            continue;
                        }
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
                            && let Some((label, detail)) = record_match(&value, body)
                        {
                            hits.entry(label)
                                .or_insert((self.t0.elapsed().as_millis() - start, detail));
                        }
                    }
                    pos += advance as u64;
                }
            }
            // Hooks.
            if let Ok(content) = std::fs::read_to_string(&self.hook_log) {
                let bytes = content.as_bytes();
                if (bytes.len() as u64) > *hook_pos {
                    let tail = &content[*hook_pos as usize..];
                    let mut advance = 0;
                    for seg in tail.split_inclusive('\n') {
                        if !seg.ends_with('\n') {
                            break;
                        }
                        advance += seg.len();
                        let rest = seg.splitn(3, ' ').nth(2).unwrap_or("");
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(rest)
                            && let Some(event) =
                                value.get("hook_event_name").and_then(|v| v.as_str())
                        {
                            let label = format!("hook-{event}");
                            hits.entry(label)
                                .or_insert((self.t0.elapsed().as_millis() - start, String::new()));
                        }
                    }
                    *hook_pos += advance as u64;
                }
            }
            if labels.iter().all(|l| hits.contains_key(*l)) {
                break;
            }
            Self::sleep_ms(10).await;
        }
        let mut out: Vec<_> = hits
            .into_iter()
            .map(|(k, (dt, detail))| (k, dt, detail))
            .collect();
        out.sort_by_key(|(_, dt, _)| *dt);
        for (label, dt, detail) in &out {
            self.mark(
                &format!("{tag}:ch-{label}"),
                &format!("+{dt}ms {}", detail.chars().take(150).collect::<String>()),
            );
        }
        for label in labels {
            if !out.iter().any(|(l, _, _)| l == label) {
                self.mark(&format!("{tag}:ch-{label}"), "MISSING");
            }
        }
        out
    }

    async fn wait_assistant_effort(&self, timeout_ms: u64) -> Option<(u128, String)> {
        let start = now_ms();
        let mut pos = std::fs::metadata(&self.transcript)
            .map(|m| m.len())
            .unwrap_or(0);
        loop {
            if let Ok(content) = std::fs::read_to_string(&self.transcript) {
                let bytes = content.as_bytes();
                if (bytes.len() as u64) > pos {
                    let tail = &content[pos as usize..];
                    let mut advance = 0;
                    for seg in tail.split_inclusive('\n') {
                        if !seg.ends_with('\n') {
                            break;
                        }
                        advance += seg.len();
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(seg)
                            && v.get("type").and_then(|x| x.as_str()) == Some("assistant")
                            && let Some(e) = v.get("effort").and_then(|x| x.as_str())
                        {
                            return Some((
                                now_ms() - start,
                                format!(
                                    "effort={e} pte={}",
                                    v.get("perTurnEffort")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("null")
                                ),
                            ));
                        }
                    }
                    pos += advance as u64;
                }
            }
            if now_ms() - start > timeout_ms as u128 {
                return None;
            }
            Self::sleep_ms(20).await;
        }
    }
}

fn bind_transcript(hook_log: &Path, cwd: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(hook_log).ok()?;
    for line in content.lines() {
        let json_part = line.splitn(3, ' ').nth(2).unwrap_or("");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_part)
            && value.get("hook_event_name").and_then(|v| v.as_str()) == Some("SessionStart")
            && value.get("cwd").and_then(|v| v.as_str()) == Some(cwd.to_str().unwrap_or(""))
            && let Some(path) = value.get("transcript_path").and_then(|v| v.as_str())
        {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn record_text(value: &serde_json::Value) -> String {
    value
        .pointer("/message/content")
        .map(|c| c.as_str().unwrap_or("").to_owned())
        .unwrap_or_default()
}

/// Matcher for one `/effort <word>` switch: slash record, stdout, and the
/// `ultra_effort_enter|exit` attachment 2.1.272 writes.
fn effort_record_matcher(
    word: &str,
) -> impl Fn(&serde_json::Value, &str) -> Option<(String, String)> {
    let wanted = word.to_owned();
    move |value: &serde_json::Value, _raw: &str| {
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "user" {
            let text = record_text(value);
            if text.contains("<command-name>/effort</command-name>") {
                let args = text
                    .find("<command-args>")
                    .map(|s| {
                        let a = s + "<command-args>".len();
                        text[a..a + text[a..].find("</command-args>").unwrap_or(0)]
                            .trim()
                            .to_owned()
                    })
                    .unwrap_or_default();
                return Some(("slash".into(), format!("args={args}")));
            }
            if text.contains("<local-command-stdout>") {
                return Some(("stdout".into(), text));
            }
        }
        if kind == "attachment"
            && let Some(att) = value
                .get("attachment")
                .and_then(|a| a.get("type"))
                .and_then(|a| a.as_str())
            && att.starts_with("ultra_effort")
        {
            return Some((att.into(), String::new()));
        }
        let _ = wanted;
        None
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let dir = PathBuf::from(
        std::env::var("PROBE_DIR").unwrap_or_else(|_| "/tmp/remuda-r-effortsync2-probe3".into()),
    );
    assert!(
        dir.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("remuda-r-effortsync2-probe"))
            && dir.parent().is_some_and(|p| p == Path::new("/tmp"))
            && !dir.is_symlink(),
        "refusing to clean {dir:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.join("ws");
    std::fs::create_dir_all(&cwd).unwrap();

    let hook_script = dir.join("hook.sh");
    let hook_log = dir.join("hook.log");
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    std::fs::write(
        &hook_script,
        format!(
            "#!/bin/bash\ninput=$(cat)\nprintf '%s %s %s\\n' \"$(date -u +%s.%N)\" \"$1\" \"$input\" >> {}\n",
            quote(&hook_log)
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut hooks = BTreeMap::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
    ] {
        hooks.insert(
            event.to_owned(),
            serde_json::json!([{
                "hooks": [{ "type": "command",
                    "command": format!("{} {}", quote(&hook_script), event) }]
            }]),
        );
    }
    let settings_path = dir.join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&serde_json::json!({ "hooks": hooks, "tui": "fullscreen" }))
            .unwrap(),
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let screen = Screen(Arc::new(Mutex::new(Vec::new())));
    {
        let mut reader = pair.master.try_clone_reader().unwrap();
        let buf = Arc::clone(&screen.0);
        std::thread::spawn(move || {
            let mut chunk = vec![0_u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        });
    }
    let mut cmd = CommandBuilder::new("claude");
    cmd.cwd(&cwd);
    cmd.arg("--effort");
    cmd.arg("low");
    cmd.arg("--settings");
    cmd.arg(&settings_path);
    cmd.env_clear();
    for name in [
        "PATH", "HOME", "LANG", "TERM", "TMPDIR", "SHELL", "USER", "LOGNAME",
    ] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "C.UTF-8");
    }
    for name in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("CLAUDE_CODE_CHILD_SESSION", "");
    cmd.env("CLAUDE_CODE_EFFORT_LEVEL", "");

    let _child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let writer = pair.master.take_writer().unwrap();

    let t0 = Instant::now();
    let transcript = loop {
        if let Some(path) = bind_transcript(&hook_log, &cwd) {
            break path;
        }
        if t0.elapsed() > Duration::from_secs(45) {
            panic!("no SessionStart within 45s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let booted = loop {
        if screen.contains("❯") {
            break t0.elapsed().as_millis();
        }
        if t0.elapsed() > Duration::from_secs(45) {
            panic!("no composer");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let mut probe = Probe {
        writer,
        screen: screen.clone(),
        transcript,
        hook_log,
        t0,
        log: Arc::new(Mutex::new(Vec::new())),
    };
    probe.mark("boot", &format!("composer +{booted}ms"));
    let mut hook_pos = std::fs::metadata(&probe.hook_log)
        .map(|m| m.len())
        .unwrap_or(0);
    Probe::sleep_ms(800).await;

    // Baseline.
    probe
        .submit("reply with exactly the two characters: ok")
        .await;
    match probe.wait_assistant_effort(90_000).await {
        Some((dt, detail)) => probe.mark("base:assistant", &format!("+{dt}ms {detail}")),
        None => probe.mark("base:assistant", "MISSING"),
    }
    Probe::sleep_ms(1_200).await;

    // A: idle /effort xhigh + confirm + prompt.
    probe.submit("/effort xhigh").await;
    let a_start = probe.t0.elapsed().as_millis();
    probe.mark("a:submit", "");
    // Confirm dialog if it appears within 1.5s.
    let mut confirmed = false;
    let dl = now_ms() + 1_500;
    while now_ms() < dl {
        if probe.screen.contains("Change effort level") {
            Probe::sleep_ms(80).await;
            probe.write_bytes(b"\r");
            confirmed = true;
            break;
        }
        Probe::sleep_ms(15).await;
    }
    probe.mark("a:dialog-confirmed", &confirmed.to_string());
    probe
        .race(
            a_start,
            6_000,
            Some("xhigh"),
            effort_record_matcher("xhigh"),
            &mut hook_pos,
            "a",
            &["slash", "stdout", "screen"],
        )
        .await;
    Probe::sleep_ms(300).await;
    let prompt_at = probe.t0.elapsed().as_millis();
    probe.submit("reply with exactly: ok").await;
    match probe.wait_assistant_effort(120_000).await {
        Some((dt, detail)) => probe.mark(
            "a:assistant",
            &format!(
                "prompt→record +{dt}ms (submit→record {}ms) {detail}",
                probe.t0.elapsed().as_millis() - a_start
            ),
        ),
        None => probe.mark("a:assistant", "MISSING"),
    }
    let _ = prompt_at;
    Probe::sleep_ms(1_200).await;

    // B: /effort max then ESC at the dialog — level must stay xhigh.
    probe.submit("/effort max").await;
    let b_start = probe.t0.elapsed().as_millis();
    let dl = now_ms() + 1_500;
    let mut saw_dialog = false;
    while now_ms() < dl {
        if probe.screen.contains("Change effort level") {
            saw_dialog = true;
            break;
        }
        Probe::sleep_ms(15).await;
    }
    Probe::sleep_ms(80).await;
    probe.write_bytes(b"\x1b");
    probe.mark("b:dialog", &saw_dialog.to_string());
    probe
        .race(
            b_start,
            2_500,
            None,
            effort_record_matcher("max"),
            &mut hook_pos,
            "b",
            &[],
        )
        .await;
    Probe::sleep_ms(500).await;

    // C: idle /effort ultracode + confirm + prompt.
    probe.submit("/effort ultracode").await;
    let c_start = probe.t0.elapsed().as_millis();
    let dl = now_ms() + 1_500;
    let mut confirmed = false;
    while now_ms() < dl {
        if probe.screen.contains("Change effort level")
            || probe.screen.contains("Set effort level to ultracode")
        {
            Probe::sleep_ms(80).await;
            probe.write_bytes(b"\r");
            confirmed = true;
            break;
        }
        Probe::sleep_ms(15).await;
    }
    probe.mark("c:dialog-confirmed", &confirmed.to_string());
    probe
        .race(
            c_start,
            6_000,
            Some("ultracode"),
            effort_record_matcher("ultracode"),
            &mut hook_pos,
            "c",
            &["slash", "stdout", "screen", "ultra_effort_enter"],
        )
        .await;
    Probe::sleep_ms(300).await;
    probe.submit("reply with exactly: ok").await;
    // Wait for the assistant record AND the ultra attachment that rides it.
    let c_deadline = now_ms() + 120_000;
    let mut got_assistant = None;
    let mut pos = std::fs::metadata(&probe.transcript)
        .map(|m| m.len())
        .unwrap_or(0);
    while now_ms() < c_deadline {
        if let Ok(content) = std::fs::read_to_string(&probe.transcript) {
            let bytes = content.as_bytes();
            if (bytes.len() as u64) > pos {
                let tail = &content[pos as usize..];
                let mut advance = 0;
                for seg in tail.split_inclusive('\n') {
                    if !seg.ends_with('\n') {
                        break;
                    }
                    advance += seg.len();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(seg) {
                        if v.get("type").and_then(|x| x.as_str()) == Some("assistant")
                            && let Some(e) = v.get("effort").and_then(|x| x.as_str())
                        {
                            got_assistant = Some(format!("effort={e}"));
                        }
                        if v.get("type").and_then(|x| x.as_str()) == Some("attachment")
                            && v.pointer("/attachment/type").and_then(|x| x.as_str())
                                == Some("ultra_effort_enter")
                        {
                            probe.mark(
                                "c:ultra-enter-at-prompt",
                                &format!("+{}ms", probe.t0.elapsed().as_millis() - c_start),
                            );
                        }
                    }
                }
                pos += advance as u64;
            }
        }
        if got_assistant.is_some() {
            break;
        }
        Probe::sleep_ms(20).await;
    }
    probe.mark(
        "c:assistant",
        &format!(
            "{} (submit→record {}ms)",
            got_assistant.clone().unwrap_or("MISSING".into()),
            probe.t0.elapsed().as_millis() - c_start
        ),
    );
    Probe::sleep_ms(1_200).await;

    // D: back to high — expect ultra_effort_exit.
    probe.submit("/effort high").await;
    let d_start = probe.t0.elapsed().as_millis();
    let dl = now_ms() + 1_500;
    while now_ms() < dl {
        if probe.screen.contains("Change effort level") {
            Probe::sleep_ms(80).await;
            probe.write_bytes(b"\r");
            break;
        }
        Probe::sleep_ms(15).await;
    }
    probe
        .race(
            d_start,
            6_000,
            Some("high"),
            effort_record_matcher("high"),
            &mut hook_pos,
            "d",
            &["slash", "stdout", "screen", "ultra_effort_exit"],
        )
        .await;
    Probe::sleep_ms(800).await;

    // E: bogus — dump what the TUI shows and whether any record lands.
    let e_screen = screen.len();
    probe.submit("/effort bogus").await;
    let e_start = probe.t0.elapsed().as_millis();
    Probe::sleep_ms(900).await;
    let slice: Vec<u8> = {
        let raw = screen.0.lock().unwrap();
        raw[e_screen.min(raw.len())..].to_vec()
    };
    let stripped = strip_ansi(&slice);
    let text = String::from_utf8_lossy(&stripped);
    let tail: String = text.chars().take(1_200).collect();
    probe.mark("e:screen-tail", &tail.replace('\n', "⏎"));
    probe
        .race(
            e_start,
            2_000,
            None,
            effort_record_matcher("bogus"),
            &mut hook_pos,
            "e",
            &[],
        )
        .await;
    probe.write_bytes(b"\x1b");
    Probe::sleep_ms(500).await;

    // F: switch while working. Submit a prompt, then immediately /effort medium.
    probe
        .submit("list the numbers from one to eight, one per line")
        .await;
    Probe::sleep_ms(400).await;
    let f_start = probe.t0.elapsed().as_millis();
    probe.write_bytes(b"/effort medium");
    Probe::sleep_ms(120).await;
    probe.write_bytes(b"\r");
    probe.mark("f:typed-while-working", "/effort medium");
    let f_dl = now_ms() + 3_000;
    let mut dialog_at: Option<i64> = if probe.screen.contains("Change effort level") {
        Some(0)
    } else {
        None
    };
    while now_ms() < f_dl {
        if dialog_at.is_none() && probe.screen.contains("Change effort level") {
            dialog_at = Some((probe.t0.elapsed().as_millis() - f_start) as i64);
        }
        Probe::sleep_ms(15).await;
    }
    if dialog_at.is_some() {
        Probe::sleep_ms(80).await;
        probe.write_bytes(b"\r");
    }
    probe.mark(
        "f:dialog",
        &dialog_at
            .map(|d| format!("+{d}ms"))
            .unwrap_or("none".into()),
    );
    // Now wait for the turn to end and the queued intent to apply.
    let f_deadline = now_ms() + 120_000;
    let mut pos = std::fs::metadata(&probe.transcript)
        .map(|m| m.len())
        .unwrap_or(0);
    let mut events: Vec<(String, String)> = Vec::new();
    while now_ms() < f_deadline {
        if let Ok(content) = std::fs::read_to_string(&probe.transcript) {
            let bytes = content.as_bytes();
            if (bytes.len() as u64) > pos {
                let tail = &content[pos as usize..];
                let mut advance = 0;
                for seg in tail.split_inclusive('\n') {
                    if !seg.ends_with('\n') {
                        break;
                    }
                    advance += seg.len();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(seg) {
                        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                        if t == "queue-operation" {
                            events.push((
                                "queue".into(),
                                v.get("operation")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .into(),
                            ));
                        }
                        if t == "user" {
                            let text = record_text(&v);
                            if text.contains("<command-name>/effort") {
                                events.push(("slash".into(), text));
                            }
                        }
                        if t == "assistant"
                            && let Some(e) = v.get("effort").and_then(|x| x.as_str())
                        {
                            events.push(("assistant".into(), e.into()));
                        }
                    }
                }
                pos += advance as u64;
            }
        }
        if events.iter().any(|(k, _)| k == "assistant") {
            break;
        }
        Probe::sleep_ms(20).await;
    }
    for (k, detail) in &events {
        probe.mark(
            "f:event",
            &format!(
                "+{}ms {k} {}",
                probe.t0.elapsed().as_millis() - f_start,
                detail.chars().take(100).collect::<String>()
            ),
        );
    }
    Probe::sleep_ms(2_000).await;

    let entries = probe.log.lock().unwrap().clone();
    let timing: BTreeMap<String, String> = entries
        .into_iter()
        .map(|(phase, ms, detail)| (phase, format!("{ms} {detail}")))
        .collect();
    std::fs::write(
        dir.join("timing.json"),
        serde_json::to_vec_pretty(&timing).unwrap(),
    )
    .unwrap();
    if let Ok(body) = std::fs::read_to_string(&probe.transcript) {
        std::fs::write(dir.join("transcript.jsonl"), &body).unwrap();
    }
    println!("DONE");
}
