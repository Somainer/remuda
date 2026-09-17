//! Live repro for c-effort3 — the SECOND in-session `/effort` switch fails.
//!
//!   PROBE_DIR=/tmp/remuda-c-effort3-repro1 \
//!     cargo run -p remuda-driver --example effort_repro3 --features test-stub
//!
//! One real claude PTY (2.1.273 on this host), one cached conversation, then
//! the owner's switch chain with NO prompt between the switches:
//!
//!   S1 /effort ultracode
//!   S2 /effort high
//!   S3 /effort ultracode
//!   S4 /effort max
//!
//! The write cadence and dialog gating are copied verbatim from
//! [`remuda_driver::effort::perform_switch`] (120 ms settle, submit CR, 1.5 s
//! dialog poll at 40 ms, 120 ms settle, confirm CR), and the transcript is fed
//! through the REAL [`remuda_driver::TranscriptMapper`] + `EffortBridge` at the
//! real 75 ms pump cadence, so a missing bridge verdict here is the same
//! no-readback-within-window the hub toast reported. An independent raw-line
//! scrape records the exact slash/stdout text and arrival offsets per switch.

#[cfg(feature = "test-stub")]
mod repro {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use remuda_driver::TranscriptMapper;
    use remuda_driver::claude_transcript::TranscriptTail;
    use remuda_driver::test_support::Bridge;

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    }

    /// Full byte stream since boot; the visible 40-row approximation is taken
    /// from the tail (the driver reads `ReadSource::Visible, lines: 40`).
    #[derive(Clone)]
    struct Screen(Arc<Mutex<Vec<u8>>>);

    impl Screen {
        fn visible_text(&self) -> String {
            let raw = self.0.lock().unwrap();
            let stripped = strip_ansi(&raw);
            let text = String::from_utf8_lossy(&stripped).into_owned();
            // Approximate the 40 visible rows: last 40 newlines worth, bounded
            // by 16 KiB, so a stale dialog phrase in scrollback cannot match.
            let mut lines: Vec<&str> = text.lines().collect();
            let keep = lines.len().min(40);
            lines.drain(..lines.len() - keep);
            lines.join("\n")
        }
        fn save(&self, path: &Path) {
            let raw = self.0.lock().unwrap();
            std::fs::write(path, strip_ansi(&raw)).unwrap();
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

    #[derive(serde::Serialize, Default)]
    struct SwitchRecord {
        word: String,
        submit_ms: u128,
        dialog_seen: bool,
        dialog_seen_ms: Option<u128>,
        confirm_cr: bool,
        slash_ms: Option<u128>,
        slash_word: Option<String>,
        stdout_ms: Option<u128>,
        stdout_text: Option<String>,
        ultra_attachment_ms: Option<u128>,
        verdict: String,
        verdict_ms: Option<u128>,
        mapper_edges: Vec<String>,
    }

    struct Probe {
        writer: Box<dyn Write + Send>,
        screen: Screen,
        dir: PathBuf,
        t0: Instant,
    }

    impl Probe {
        async fn sleep_ms(ms: u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }

        fn write_bytes(&mut self, bytes: &[u8]) {
            self.writer.write_all(bytes).unwrap();
            self.writer.flush().unwrap();
        }

        /// One pump tick, exactly like `spawn_transcript_pump`: tail poll then
        /// line maps then flush. Raw records are also returned for the
        /// independent scrape.
        fn pump_tick(
            mapper: &mut TranscriptMapper,
            tail: &mut TranscriptTail,
        ) -> (Vec<serde_json::Value>, Vec<String>) {
            let lines = tail.poll().unwrap_or_default();
            let mut raws = Vec::new();
            let mut edges = Vec::new();
            for line in &lines {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    raws.push(value);
                }
                for obs in mapper.map_line(line).unwrap_or_default() {
                    if let remuda_protocol::ObservationPayload::Effort(payload) = &obs.body {
                        edges.push(format!(
                            "{:?}/ultracode={:?}/source={:?}",
                            payload.effective.name,
                            payload.effective.ultracode,
                            payload.effective.source
                        ));
                    }
                }
            }
            for obs in mapper.flush().unwrap_or_default() {
                if let remuda_protocol::ObservationPayload::Effort(payload) = &obs.body {
                    edges.push(format!(
                        "{:?}/ultracode={:?}/source={:?}",
                        payload.effective.name,
                        payload.effective.ultracode,
                        payload.effective.source
                    ));
                }
            }
            (raws, edges)
        }

        async fn wait_assistant(
            &mut self,
            mapper: &mut TranscriptMapper,
            tail: &mut TranscriptTail,
        ) {
            let deadline = now_ms() + 120_000;
            loop {
                let (raws, _) = Self::pump_tick(mapper, tail);
                for value in raws {
                    if value.get("type").and_then(|v| v.as_str()) == Some("assistant")
                        && value.get("effort").and_then(|v| v.as_str()).is_some()
                    {
                        return;
                    }
                }
                assert!(now_ms() < deadline, "baseline assistant never arrived");
                Self::sleep_ms(75).await;
            }
        }

        /// Perform one switch the way `perform_switch` does, while pumping the
        /// real mapper/bridge at the production cadence.
        async fn switch(
            &mut self,
            n: usize,
            word: &str,
            bridge: &Arc<Bridge>,
            mapper: &mut TranscriptMapper,
            tail: &mut TranscriptTail,
        ) -> SwitchRecord {
            let mut rec = SwitchRecord {
                word: word.to_owned(),
                ..Default::default()
            };
            let generation = bridge.arm_word(word);
            assert!(bridge.has_pending(), "arm must leave a pending generation");

            self.screen
                .save(&self.dir.join(format!("screen-{n}-0-pre.txt")));
            rec.submit_ms = self.t0.elapsed().as_millis();
            // Body write, 120 ms settle, submitting CR.
            self.write_bytes(format!("/effort {word}").as_bytes());
            Self::sleep_ms(120).await;
            self.write_bytes(b"\r");

            // Confirmation dialog gating: 1.5 s deadline polled at 40 ms.
            let dialog_deadline = std::time::Duration::from_millis(1_500);
            let dialog_start = std::time::Instant::now();
            while dialog_start.elapsed() < dialog_deadline {
                let (_, edges) = Self::pump_tick(mapper, tail);
                rec.mapper_edges.extend(edges);
                let visible = self.screen.visible_text();
                if visible.contains("Change effort level") {
                    rec.dialog_seen = true;
                    rec.dialog_seen_ms = Some(self.t0.elapsed().as_millis() - rec.submit_ms);
                    break;
                }
                if visible.contains("Invalid argument") {
                    break;
                }
                Self::sleep_ms(40).await;
            }
            if rec.dialog_seen {
                Self::sleep_ms(120).await;
                self.write_bytes(b"\r");
                rec.confirm_cr = true;
            }

            // Read-back window: 10 s, pump at 75 ms, scrape raw records.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let outcome = loop {
                let (raws, edges) = Self::pump_tick(mapper, tail);
                rec.mapper_edges.extend(edges);
                for value in raws {
                    let dt = self.t0.elapsed().as_millis() - rec.submit_ms;
                    Self::scrape(&mut rec, &value, dt);
                }
                match bridge.wait(generation, Duration::from_millis(75)).await {
                    Some(verdict) => break Some(verdict),
                    None if std::time::Instant::now() >= deadline => break None,
                    None => {}
                }
            };
            rec.verdict_ms = Some(self.t0.elapsed().as_millis() - rec.submit_ms);
            match outcome {
                Some(remuda_driver::effort::Readback::Applied(observed)) => {
                    rec.verdict = format!(
                        "Applied(name={:?}, ultracode={:?})",
                        observed.name, observed.ultracode
                    );
                }
                Some(remuda_driver::effort::Readback::Rejected { reason }) => {
                    // Mirror perform_switch, which fails the gen after a reject.
                    bridge.fail(generation);
                    rec.verdict = format!("Rejected({reason})");
                }
                None => {
                    // Mirror perform_switch's bounded-timeout give-up.
                    bridge.fail(generation);
                    rec.verdict = "NONE".into();
                }
            }
            assert!(
                !bridge.has_pending(),
                "settled switch must clear the pending generation"
            );
            self.screen
                .save(&self.dir.join(format!("screen-{n}-1-post.txt")));
            // Drain anything landing just after the window close.
            let (late_raws, late_edges) = Self::pump_tick(mapper, tail);
            rec.mapper_edges.extend(late_edges);
            for value in late_raws {
                let dt = self.t0.elapsed().as_millis() - rec.submit_ms;
                Self::scrape(&mut rec, &value, dt);
            }
            rec
        }

        fn scrape(rec: &mut SwitchRecord, value: &serde_json::Value, dt: u128) {
            match value.get("type").and_then(|v| v.as_str()) {
                Some("user") => {
                    let text = value
                        .pointer("/message/content")
                        .map(|c| match c {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Array(blocks) => blocks
                                .iter()
                                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    if text.contains("<command-name>/effort</command-name>")
                        && rec.slash_ms.is_none()
                    {
                        rec.slash_ms = Some(dt);
                        rec.slash_word = Some(
                            text.split("<command-args>")
                                .nth(1)
                                .and_then(|s| s.split("</command-args>").next())
                                .unwrap_or("")
                                .trim()
                                .to_owned(),
                        );
                    } else if let Some(start) = text.find("<local-command-stdout>")
                        && rec.stdout_ms.is_none()
                    {
                        let body = &text[start + "<local-command-stdout>".len()..];
                        let end = body.find("</local-command-stdout>").unwrap_or(body.len());
                        rec.stdout_ms = Some(dt);
                        rec.stdout_text = Some(body[..end].trim().to_owned());
                    }
                }
                Some("attachment") => {
                    if rec.ultra_attachment_ms.is_none()
                        && value
                            .pointer("/attachment/type")
                            .and_then(|v| v.as_str())
                            .is_some_and(|t| t.starts_with("ultra_effort"))
                    {
                        rec.ultra_attachment_ms = Some(dt);
                    }
                }
                _ => {}
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

    pub(crate) async fn run() {
        let dir = PathBuf::from(
            std::env::var("PROBE_DIR").unwrap_or_else(|_| "/tmp/remuda-c-effort3-repro1".into()),
        );
        assert!(
            dir.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("remuda-c-effort3-repro"))
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
        for event in ["SessionStart", "UserPromptSubmit", "Stop"] {
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
        // This session is its own top-level claude (child sessions write no
        // transcript) and must not inherit my shell's effort/session env.
        cmd.env("CLAUDE_CODE_CHILD_SESSION", "");
        cmd.env("CLAUDE_CODE_EFFORT_LEVEL", "");
        cmd.env("CLAUDE_EFFORT", "");
        cmd.env("CLAUDE_CODE_SESSION_ID", "");

        let mut child = pair.slave.spawn_command(cmd).unwrap();
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
        loop {
            if screen.visible_text().contains('❯') {
                break;
            }
            if t0.elapsed() > Duration::from_secs(60) {
                screen.save(&dir.join("screen-boot-fail.txt"));
                panic!("no composer within 60s");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!(
            "boot +{}ms; transcript {}",
            t0.elapsed().as_millis(),
            transcript.display()
        );

        let bridge = Arc::new(Bridge::new());
        let mut mapper = remuda_driver::test_support::mapper_with_bridge(
            Arc::clone(&bridge),
            "c-effort3-repro",
            "2.1.273",
        );
        let mut tail = TranscriptTail::new(transcript.clone());

        let mut probe = Probe {
            writer,
            screen: screen.clone(),
            dir: dir.clone(),
            t0,
        };
        Probe::sleep_ms(800).await;

        // One real turn so the switches happen in a cached conversation, the
        // state in which the owner saw the second-switch failure.
        probe.write_bytes(b"reply with exactly the two characters: ok");
        Probe::sleep_ms(120).await;
        probe.write_bytes(b"\r");
        probe.wait_assistant(&mut mapper, &mut tail).await;
        println!("baseline assistant +{}ms", t0.elapsed().as_millis());
        Probe::sleep_ms(1_200).await;

        /// One scenario step.
        enum Step {
            Switch(&'static str),
            /// Real prompt; wait for the assistant effort record, then drain so
            /// the `ultra_effort_enter|exit` attachment has ridden the prompt.
            Prompt(&'static str),
        }

        let turns = vec![
            Step::Switch("ultracode"),
            Step::Prompt("reply with exactly the two characters: ok"),
            Step::Switch("high"),
            Step::Prompt("reply with exactly the two characters: ok"),
            Step::Switch("ultracode"),
            Step::Prompt("reply with exactly the two characters: ok"),
            Step::Switch("max"),
        ];
        let back_to_back = vec![
            Step::Switch("ultracode"),
            Step::Switch("high"),
            Step::Switch("ultracode"),
            Step::Switch("max"),
        ];
        let scenario = std::env::var("SCENARIO").unwrap_or_else(|_| "turns".into());
        let steps = if scenario == "back-to-back" {
            &back_to_back
        } else {
            &turns
        };

        let mut records = Vec::new();
        let mut switch_n = 0;
        for step in steps {
            match step {
                Step::Switch(word) => {
                    switch_n += 1;
                    let rec = probe
                        .switch(switch_n, word, &bridge, &mut mapper, &mut tail)
                        .await;
                    println!(
                        "S{} {}: dialog={:?} confirm={} slash={:?}ms stdout={:?}ms verdict={} @ \
                         {:?}ms text={:?}",
                        switch_n,
                        word,
                        rec.dialog_seen_ms,
                        rec.confirm_cr,
                        rec.slash_ms,
                        rec.stdout_ms,
                        rec.verdict,
                        rec.verdict_ms,
                        rec.stdout_text,
                    );
                    records.push(rec);
                    Probe::sleep_ms(1_500).await;
                }
                Step::Prompt(text) => {
                    println!("PROMPT {text}");
                    probe.write_bytes(text.as_bytes());
                    Probe::sleep_ms(120).await;
                    probe.write_bytes(b"\r");
                    probe.wait_assistant(&mut mapper, &mut tail).await;
                    // Let the ultra attachment and post-turn records land.
                    Probe::sleep_ms(2_500).await;
                    let (_, edges) = Probe::pump_tick(&mut mapper, &mut tail);
                    println!(
                        "  turn done +{}ms edges={edges:?}",
                        t0.elapsed().as_millis()
                    );
                }
            }
        }

        let mut timing = BTreeMap::new();
        for (i, rec) in records.iter().enumerate() {
            timing.insert(
                format!("S{}-{}", i + 1, rec.word),
                serde_json::to_value(rec).unwrap(),
            );
        }
        timing.insert(
            "boot_ms".into(),
            serde_json::json!(t0.elapsed().as_millis()),
        );
        std::fs::write(
            dir.join("timing.json"),
            serde_json::to_vec_pretty(&timing).unwrap(),
        )
        .unwrap();
        if let Ok(body) = std::fs::read_to_string(&transcript) {
            std::fs::write(dir.join("transcript.jsonl"), body).unwrap();
        }
        let _ = child.kill();
        let failed = records.iter().any(|r| !r.verdict.starts_with("Applied"));
        if failed {
            println!(
                "REPRODUCED: a switch did not settle Applied — see {}",
                dir.display()
            );
        } else {
            println!(
                "ALL SETTLED — chain did not reproduce here; see {}",
                dir.display()
            );
        }
    }
}

#[cfg(feature = "test-stub")]
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(repro::run());
}

#[cfg(not(feature = "test-stub"))]
fn main() {
    panic!("run with: cargo run -p remuda-driver --example effort_repro3 --features test-stub");
}
