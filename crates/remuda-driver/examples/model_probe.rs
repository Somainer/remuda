//! Live probe — in-session `/model` switch channels on the REAL claude TUI
//! (2.1.272), gateway model discovery enabled.
//!
//!   PROBE_DIR=/tmp/remuda-c-modelsync-probe1 \
//!     cargo run -p remuda-driver --example model_probe
//!
//! Scenario (all switches while idle):
//!   boot  scoped CLAUDE_CONFIG_DIR (onboarding pre-seeded, gateway env inherited)
//!   base  prompt (baseline assistant record → message.model)
//!   A     /model sonnet              confirm → prompt   (alias verdict + resolved model)
//!   B     /model model_hub/es1_orange_o50  confirm → prompt (gateway id verdict)
//!   C     /model bogus-xyz                            (error shape)
//!   D     /model haiku  ESC                           (dismiss: kept verdict?)
//!   E     /model (bare)                               (picker entries = real list)
//!
//! Marks are `T<phase> <ms since boot> <detail>`; channel marks are relative
//! to the submit CR: ch-* +Nms. The scoped config dir is dumped at the end so
//! the gateway discovery cache path/shape can be confirmed.

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
                if raw[i] == 0x1b && i + 1 < raw.len() && raw[i + 1] == 0x1b {
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
    /// the /model slash + stdout, the screen needle. Marks each first arrival.
    async fn race(
        &self,
        start: u128,
        window_ms: u64,
        screen_needle: Option<&str>,
        tag: &str,
        labels: &[&str],
    ) -> Vec<(String, u128, String)> {
        let deadline = now_ms() + u128::from(window_ms);
        let mut hits: BTreeMap<String, (u128, String)> = BTreeMap::new();
        let mut pos = std::fs::metadata(&self.transcript)
            .map(|m| m.len())
            .unwrap_or(0);
        while now_ms() < deadline {
            if let Some(needle) = screen_needle
                && !hits.contains_key("screen")
                && self.screen.contains(needle)
            {
                hits.insert(
                    "screen".into(),
                    (self.t0.elapsed().as_millis().saturating_sub(start), needle.into()),
                );
            }
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
                            && value.get("type").and_then(|v| v.as_str()) == Some("user")
                        {
                            let text = record_text(&value);
                            if text.contains("<command-name>/model</command-name>")
                                && !hits.contains_key("slash")
                            {
                                hits.entry("slash".into()).or_insert((
                                    self.t0.elapsed().as_millis().saturating_sub(start),
                                    text.chars().take(300).collect(),
                                ));
                            }
                            if text.contains("<local-command-stdout>")
                                && text.to_lowercase().contains("model")
                                && !hits.contains_key("stdout")
                            {
                                hits.entry("stdout".into()).or_insert((
                                    self.t0.elapsed().as_millis().saturating_sub(start),
                                    text.chars().take(300).collect(),
                                ));
                            }
                        }
                    }
                    pos += advance as u64;
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
                &format!("+{dt}ms {}", detail.replace('\n', "⏎")),
            );
        }
        for label in labels {
            if !out.iter().any(|(l, _, _)| l == label) {
                self.mark(&format!("{tag}:ch-{label}"), "MISSING");
            }
        }
        out
    }

    async fn wait_message_model(&self, timeout_ms: u64) -> Option<(u128, String)> {
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
                            && let Some(model) =
                                v.pointer("/message/model").and_then(|x| x.as_str())
                        {
                            return Some((now_ms() - start, model.to_owned()));
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

    fn dump_screen(&self, tag: &str) {
        let text = self.screen.text();
        // Keep the last 60 screen rows; the dialog/picker renders at the bottom.
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = tail.len().saturating_sub(60);
        self.mark(tag, &tail[start..].join(" ⏎ "));
    }

    /// Poll up to 1.5s for the model confirmation dialog; return whether it was
    /// seen. The dialog box is dumped verbatim so the exact text is measured.
    async fn await_dialog(&mut self, tag: &str) -> bool {
        let dl = now_ms() + 1_500;
        let mut seen = false;
        while now_ms() < dl {
            let text = self.screen.text();
            let low = text.to_lowercase();
            if low.contains("switch to") || low.contains("change model") {
                seen = true;
                self.dump_screen(&format!("{tag}:dialog"));
                break;
            }
            Self::sleep_ms(15).await;
        }
        if seen {
            Self::sleep_ms(80).await;
            self.write_bytes(b"\r");
        }
        seen
    }
}

fn record_text(value: &serde_json::Value) -> String {
    value
        .pointer("/message/content")
        .map(|c| c.as_str().unwrap_or("").to_owned())
        .unwrap_or_default()
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

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let dir = PathBuf::from(
        std::env::var("PROBE_DIR").unwrap_or_else(|_| "/tmp/remuda-c-modelsync-probe1".into()),
    );
    assert!(
        dir.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("remuda-c-modelsync-probe"))
            && dir.parent().is_some_and(|p| p == Path::new("/tmp"))
            && !dir.is_symlink(),
        "refusing to clean {dir:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.join("ws");
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = dir.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();

    // Scoped CLAUDE_CONFIG_DIR: pre-seed onboarding so the TUI mounts the
    // composer (same gates the driver seeds), plus gateway env + discovery.
    std::fs::write(
        cfg.join(".claude.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "hasCompletedOnboarding": true,
            "installMethod": "npm",
            "projects": {
                cwd.to_string_lossy(): {
                    "allowedTools": [],
                    "hasTrustDialogAccepted": true
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

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
    hooks.insert(
        "SessionStart".to_owned(),
        serde_json::json!([{
            "hooks": [{ "type": "command",
                "command": format!("{} {}", quote(&hook_script), "SessionStart") }]
        }]),
    );
    // Mirror the gateway env into settings.json so the scoped config dir is
    // self-contained but talks to the same relay.
    let mut settings = serde_json::json!({
        "hooks": hooks,
        "skipDangerousModePermissionPrompt": true,
        "tui": "fullscreen",
    });
    {
        let obj = settings.as_object_mut().unwrap();
        let mut env = serde_json::Map::new();
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
        ] {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.to_owned(), serde_json::Value::String(value));
            }
        }
        obj.insert("env".to_owned(), serde_json::Value::Object(env));
    }
    std::fs::write(
        cfg.join("settings.json"),
        serde_json::to_vec_pretty(&settings).unwrap(),
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
    cmd.env("CLAUDE_CONFIG_DIR", &cfg);
    // Do not inherit this runner's child-session markers.
    cmd.env("CLAUDE_CODE_CHILD_SESSION", "");
    cmd.env("CLAUDE_CODE_SESSION_ID", "");
    cmd.env("CLAUDE_PID", "");

    let _child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let writer = pair.master.take_writer().unwrap();

    let t0 = Instant::now();
    let transcript = loop {
        if let Some(path) = bind_transcript(&hook_log, &cwd) {
            break path;
        }
        if t0.elapsed() > Duration::from_secs(45) {
            std::fs::write(
                dir.join("boot-screen.txt"),
                String::from_utf8_lossy(&strip_ansi(&screen.0.lock().unwrap())).as_bytes(),
            )
            .unwrap();
            panic!("no SessionStart within 45s (screen dumped to boot-screen.txt)");
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
        t0,
        log: Arc::new(Mutex::new(Vec::new())),
    };
    probe.mark("boot", &format!("composer +{booted}ms"));
    Probe::sleep_ms(800).await;

    // Where did discovery land?
    for rel in [
        "cache/gateway-models.json",
        ".claude/cache/gateway-models.json",
    ] {
        if let Ok(body) = std::fs::read_to_string(cfg.join(rel)) {
            probe.mark(
                "discovery-cache",
                &format!("{rel} {} bytes: {}", body.len(), body.chars().take(200).collect::<String>()),
            );
        }
    }

    // Baseline.
    probe
        .submit("reply with exactly the two characters: ok")
        .await;
    match probe.wait_message_model(90_000).await {
        Some((dt, model)) => probe.mark("base:assistant", &format!("+{dt}ms message.model={model}")),
        None => probe.mark("base:assistant", "MISSING"),
    }
    Probe::sleep_ms(1_500).await;

    // A: /model sonnet + confirm + prompt.
    probe.submit("/model sonnet").await;
    let a_start = probe.t0.elapsed().as_millis();
    let confirmed = probe.await_dialog("a").await;
    probe.mark("a:dialog-confirmed", &confirmed.to_string());
    probe
        .race(a_start, 6_000, Some("sonnet"), "a", &["slash", "stdout"])
        .await;
    Probe::sleep_ms(300).await;
    probe.submit("reply with exactly: ok").await;
    match probe.wait_message_model(120_000).await {
        Some((_, model)) => probe.mark(
            "a:assistant",
            &format!("submit→record {}ms message.model={model}", probe.t0.elapsed().as_millis() - a_start),
        ),
        None => probe.mark("a:assistant", "MISSING"),
    }
    Probe::sleep_ms(1_500).await;

    // B: gateway id + confirm + prompt.
    probe.submit("/model model_hub/es1_orange_o50").await;
    let b_start = probe.t0.elapsed().as_millis();
    let confirmed = probe.await_dialog("b").await;
    probe.mark("b:dialog-confirmed", &confirmed.to_string());
    probe
        .race(b_start, 6_000, Some("o50"), "b", &["slash", "stdout"])
        .await;
    Probe::sleep_ms(300).await;
    probe.submit("reply with exactly: ok").await;
    match probe.wait_message_model(120_000).await {
        Some((dt, model)) => probe.mark(
            "b:assistant",
            &format!("+{dt}ms message.model={model}"),
        ),
        None => probe.mark("b:assistant", "MISSING"),
    }
    Probe::sleep_ms(1_500).await;

    // C: bogus id.
    probe.submit("/model bogus-xyz-123").await;
    let c_start = probe.t0.elapsed().as_millis();
    Probe::sleep_ms(1_200).await;
    probe.dump_screen("c:screen");
    probe
        .race(c_start, 2_000, None, "c", &[])
        .await;
    probe.write_bytes(b"\x1b");
    Probe::sleep_ms(400).await;

    // D: dismiss the dialog with Esc.
    probe.submit("/model haiku").await;
    let d_start = probe.t0.elapsed().as_millis();
    let dl = now_ms() + 1_500;
    let mut saw_dialog = false;
    while now_ms() < dl {
        let text = probe.screen.text().to_lowercase();
        if text.contains("switch to") || text.contains("change model") {
            saw_dialog = true;
            probe.dump_screen("d:dialog");
            break;
        }
        Probe::sleep_ms(15).await;
    }
    Probe::sleep_ms(80).await;
    probe.write_bytes(b"\x1b");
    probe.mark("d:dialog", &saw_dialog.to_string());
    probe
        .race(d_start, 3_000, None, "d", &[])
        .await;
    Probe::sleep_ms(500).await;
    probe.dump_screen("d:screen-after-esc");

    // E: bare /model picker — its entries ARE the real list.
    probe.submit("/model").await;
    Probe::sleep_ms(1_200).await;
    probe.dump_screen("e:picker");
    probe.write_bytes(b"\x1b");
    Probe::sleep_ms(300).await;
    probe.write_bytes(b"\x1b");
    Probe::sleep_ms(500).await;
    probe.dump_screen("e:picker-after-esc");

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
    // Snapshot the scoped config tree (cache path/shape evidence).
    let mut tree = Vec::new();
    fn walk(base: &Path, rel: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = std::fs::read_dir(base.join(rel)) {
            for ent in rd.flatten() {
                let path = rel.join(ent.file_name());
                if ent.path().is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(path.display().to_string());
                }
            }
        }
    }
    walk(&cfg, Path::new(""), &mut tree);
    std::fs::write(dir.join("cfg-tree.txt"), tree.join("\n")).unwrap();
    if let Ok(body) = std::fs::read_to_string(cfg.join("cache/gateway-models.json")) {
        std::fs::write(dir.join("gateway-models.json"), body).unwrap();
    }
    println!("DONE");
}
