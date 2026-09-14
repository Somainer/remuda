//! PTY harness for the `fake-harness` integration tests.
//!
//! Spawns the binary under a real `portable-pty` master at a chosen size,
//! drives it with separate writes (body and Enter are *never* coalesced
//! except where a test explicitly wants the paste-burst case), and waits on
//! the binary's semantic `--events-out` JSONL rather than racing repaints.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use serde_json::Value;

/// One running fake-harness binary in a PTY.
pub struct Harness {
    /// Dialect label for diagnostics.
    pub kind: &'static str,
    /// Temp home directory (harness artifacts).
    pub home: PathBuf,
    /// Semantic event log path.
    pub events_path: PathBuf,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    seen: usize,
    closed: bool,
    owns_home: bool,
}

/// Builder for [`Harness`].
pub struct HarnessBuilder {
    kind: &'static str,
    scenario: Option<String>,
    home: Option<PathBuf>,
    settings: Option<PathBuf>,
    envs: HashMap<String, String>,
    extra_args: Vec<String>,
    cols: u16,
    rows: u16,
}

impl HarnessBuilder {
    /// New builder for a dialect.
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            scenario: None,
            home: None,
            settings: None,
            envs: HashMap::new(),
            extra_args: Vec::new(),
            cols: 80,
            rows: 24,
        }
    }

    /// Bundled scenario file name (under `fixtures/fake-harness/scenarios/`).
    pub fn scenario(mut self, name: &str) -> Self {
        self.scenario = Some(name.to_owned());
        self
    }

    /// Reuse a home directory (resume tests).
    pub fn home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Claude settings overlay.
    pub fn settings(mut self, path: PathBuf) -> Self {
        self.settings = Some(path);
        self
    }

    /// Extra environment variable for the child.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.envs.insert(key.to_owned(), value.to_owned());
        self
    }

    /// Extra CLI argument.
    pub fn arg(mut self, arg: &str) -> Self {
        self.extra_args.push(arg.to_owned());
        self
    }

    /// Terminal size.
    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Spawn the binary.
    pub fn spawn(self) -> Harness {
        let bin = env!("CARGO_BIN_EXE_fake-harness");
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: self.rows,
                cols: self.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(bin);
        let owns_home = self.home.is_none();
        let home = self.home.unwrap_or_else(temp_home);
        std::fs::create_dir_all(&home).expect("mkdir home");
        let events_path = home.join("events.jsonl");
        cmd.env("HOME", home.clone());
        cmd.env("TERM", "xterm-256color");
        cmd.env("CLAUDE_CONFIG_DIR", &home);
        cmd.env("CODEX_HOME", &home);
        cmd.env("GROK_HOME", &home);
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        cmd.arg("--kind");
        cmd.arg(self.kind);
        cmd.arg("--home");
        cmd.arg(&home);
        cmd.arg("--events-out");
        cmd.arg(&events_path);
        if let Some(name) = &self.scenario {
            cmd.arg("--script");
            cmd.arg(scenario_path(name));
        }
        if let Some(settings) = &self.settings {
            cmd.arg("--settings");
            cmd.arg(settings);
        }
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn fake-harness");
        drop(pair.slave);
        let master = pair.master;
        let writer = master.take_writer().expect("pty writer");
        // Drain screen bytes for the whole run so a filled PTY buffer can never
        // stall the binary's repaints; tests assert on artifacts/events.
        let drain_reader = master.try_clone_reader().expect("clone reader");
        std::thread::spawn(move || {
            let mut reader = drain_reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        Harness {
            kind: self.kind,
            home,
            events_path,
            child,
            master,
            writer,
            seen: 0,
            closed: false,
            owns_home,
        }
    }
}

/// Fresh temp home under the target temp dir.
pub fn temp_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fake-harness-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Fixture root.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake-harness")
}

/// Bundled scenario path.
pub fn scenario_path(name: &str) -> PathBuf {
    fixtures_dir().join("scenarios").join(name)
}

/// Hooks fixture directory.
pub fn hooks_dir() -> PathBuf {
    fixtures_dir().join("hooks")
}

impl Harness {
    /// Raw write into the PTY (one write call).
    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    /// Type body text in its own write.
    pub fn type_text(&mut self, text: &str) {
        std::thread::sleep(Duration::from_millis(30));
        self.write_raw(text.as_bytes());
        std::thread::sleep(Duration::from_millis(220));
    }

    /// Press Enter in its own write (the verified safe submit recipe).
    pub fn press_enter(&mut self) {
        std::thread::sleep(Duration::from_millis(80));
        self.write_raw(b"\r");
        std::thread::sleep(Duration::from_millis(80));
    }

    /// Type then submit with two separate writes.
    pub fn submit(&mut self, text: &str) {
        self.type_text(text);
        self.press_enter();
    }

    /// Press a named key.
    pub fn press(&mut self, key: Key) {
        let bytes: &[u8] = match key {
            Key::Esc => b"\x1b",
            Key::Tab => b"\x09",
            Key::CtrlC => b"\x03",
            Key::CtrlQ => b"\x11",
            Key::Enter => b"\r",
        };
        std::thread::sleep(Duration::from_millis(30));
        self.write_raw(bytes);
    }

    /// Digit row key (`1`..`9`).
    pub fn press_digit(&mut self, n: u8) {
        assert!((1..=9).contains(&n));
        std::thread::sleep(Duration::from_millis(30));
        self.write_raw(&[b'0' + n]);
    }

    /// Drain the PTY output (keeps the kernel buffer healthy); not asserted on.
    pub fn pump_io(&mut self) {
        // Output is continuously consumed by the drainer thread; this is a
        // deliberate pause point for tests that want a settle delay.
        std::thread::sleep(Duration::from_millis(10));
    }

    /// Read and discard all flushed events (used only for draining).
    pub fn events(&mut self) -> Vec<Value> {
        let parsed = self.parse_new();
        self.seen += parsed.len();
        parsed
    }

    /// Fully-parsed events beyond the cursor; the cursor is untouched.
    fn parse_new(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(&self.events_path).unwrap_or_default();
        let mut events = Vec::new();
        for line in text.lines().skip(self.seen) {
            match serde_json::from_str::<Value>(line) {
                Ok(value) => events.push(value),
                // Line caught mid-flush: stop and retry next poll.
                Err(_) => break,
            }
        }
        events
    }

    /// Wait until an event with `name` matching the predicate appears. Only
    /// lines through the match are consumed; later flushed events stay queued
    /// for the next waiter.
    pub fn wait_event(
        &mut self,
        name: &str,
        pred: impl Fn(&Value) -> bool,
        timeout: Duration,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let pending = self.parse_new();
            for (offset, event) in pending.iter().enumerate() {
                if event.get("event").and_then(Value::as_str) == Some(name) && pred(event) {
                    self.seen += offset + 1;
                    return event.clone();
                }
            }
            if Instant::now() >= deadline {
                let body = std::fs::read_to_string(&self.events_path).unwrap_or_default();
                let alive = self.child.try_wait().expect("try_wait").is_none();
                panic!(
                    "timed out waiting for event {name} (child alive={alive})\n--- events so far ---\n{body}"
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait until any matching event appears (any name), consuming through it.
    pub fn wait_any(&mut self, want: &[&str], timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let pending = self.parse_new();
            for (offset, event) in pending.iter().enumerate() {
                if let Some(name) = event.get("event").and_then(Value::as_str)
                    && want.contains(&name)
                {
                    self.seen += offset + 1;
                    return event.clone();
                }
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for any of {want:?}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// True if a matching event is currently buffered (does not consume it).
    pub fn saw_event(&mut self, name: &str) -> bool {
        self.parse_new()
            .iter()
            .any(|event| event.get("event").and_then(Value::as_str) == Some(name))
    }

    /// Wait for the process to exit (scenarios with `quit_after_turns`).
    pub fn wait_exit(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self.child.try_wait().expect("wait").is_some() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("fake-harness ({}) did not exit", self.kind);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Terminate the process and wait (keeps the harness for artifact reads).
    pub fn shutdown(&mut self) {
        if !self.closed {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.closed = true;
        }
    }

    /// Read a file under the harness home as a string.
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.home.join(rel)).expect("read artifact")
    }

    /// Whether a relative artifact exists.
    pub fn exists(&self, rel: &str) -> bool {
        self.home.join(rel).is_file()
    }

    /// Spawn the fake child's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.process_id().unwrap_or(0)
    }

    /// Home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Locate the dialect's main artifact by walking the home tree.
    pub fn find_file(&self, name: &str) -> Option<PathBuf> {
        walk(&self.home, name)
    }

    /// Full path of the first directory/file with the given name.
    pub fn find_dir(&self, name: &str) -> Option<PathBuf> {
        walk_dir(&self.home, name)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown();
        if self.owns_home {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

fn walk(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = walk(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|f| f == name) {
            return Some(path);
        }
    }
    None
}

fn walk_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|f| f == name) {
                return Some(path);
            }
            if let Some(found) = walk_dir(&path, name) {
                return Some(found);
            }
        }
    }
    None
}

/// First file whose name starts with `prefix` under `dir`.
pub fn find_walk(dir: &Path, prefix: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_walk(&path, prefix) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.starts_with(prefix))
        {
            return Some(path);
        }
    }
    None
}

/// Serialize the PTY timing tests: each spawns a binary in a real PTY with
/// real sleeps, so parallel scheduling only adds flake risk.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Guard held for one test; tests run one at a time.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keys understood by [`Harness::press`].
pub enum Key {
    /// Lone Esc.
    Esc,
    /// Tab.
    Tab,
    /// Ctrl+C.
    CtrlC,
    /// Ctrl+Q.
    CtrlQ,
    /// Enter.
    Enter,
}
