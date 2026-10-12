//! c-ctxusage RC1: a native shell-pty Claude session driven by the fake
//! harness through the REAL Hub HTTP API emits usage from its transcript, so
//! `GET /v1/instances` carries a non-null `usageRollup` (turns ≥ 1 and a
//! context percentage). This is the native path — not the `usage:` sentinel.
//!
//! Why this does not use `crates/remuda-node/examples/native_hub_e2e.rs`
//! (c-ctxusage r5 item 5a): that file is a standalone fixture BINARY
//! (`fn main`, examples/native_hub_e2e.rs:36) — env-driven
//! (`HUB_E2E_NODE_*` / `REMUDA_CLAUDE_*`), signal-driven lifecycle, host
//! label `e2e-native-hooks` — designed to be spawned as a child process by
//! the Playwright specs (see web/tests/e2e/promoted-claude.hub.spec.ts). It
//! exposes no reusable library entry (examples are not linked into
//! integration tests; its internals are not `pub`). This test composes the
//! SAME production pieces the example composes at its lines 111-127
//! (`NativeDriverConfig::{new,with_claude_binary,with_claude_native_home}`,
//! `pty_hooks`/`promote_terminal_agents`/`relay_binary`,
//! `compose(LocalDrivers::Native)`) in-process against the real Hub HTTP API
//! and a real PTY/relay/fake-harness — which for a Rust test is the stronger
//! harness (no WebSocket/signal/process wrapper). The Playwright-only path
//! through the example binary is covered by ux-usage native-transcript spec.

use remuda_hub::HubConfig;
use remuda_node::{DevNode, DevServerConfig, NativeDriverConfig, WssConfig, WssLink};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Locate or build the real `remuda` CLI the hook overlay invokes (mirrors
/// the other native shell-pty e2es).
fn remuda_relay_bin(root: &Path) -> std::path::PathBuf {
    if let Ok(path) = std::env::var("REMUDA_RELAY_BIN") {
        return std::path::PathBuf::from(path);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        && let Some(candidate) = [dir.join("remuda"), dir.join("remuda.exe")]
            .into_iter()
            .find(|p| p.is_file())
    {
        return candidate;
    }
    let out = root.join("relay-target");
    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(remuda_testing::workspace_root())
        .args(["build", "-p", "remuda", "--bin", "remuda", "--quiet"])
        .arg("--target-dir")
        .arg(&out)
        .status()
        .expect("build remuda relay");
    assert!(status.success(), "building the remuda relay failed");
    out.join("debug/remuda")
}

/// Event-driven waiter for a durable turn row (c-ctxusage r15: replaces the
/// 250 ms HTTP poll). A blocking thread watches `hub.sqlite` with the
/// platform's filesystem event facility — inotify on Linux, an
/// EVFILT_VNODE kqueue on macOS/BSDs (c-ctxusage r16 item 7) — and re-checks
/// the durable predicate after each filesystem event, so no wall-clock sleep
/// is involved: the usage commit itself is the wake. A bounded poll inside
/// the watch deadline is the safety bound, not the mechanism; the loop also
/// returns immediately once the row exists.
struct UsageWaiter(std::thread::JoinHandle<bool>);

impl UsageWaiter {
    #[cfg(target_os = "linux")]
    fn spawn(db_path: PathBuf, instance_id: String) -> Self {
        use nix::poll::{PollFd, PollFlags, poll};
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
        Self(std::thread::spawn(move || {
            use std::os::fd::AsFd;
            let Ok(conn) = rusqlite::Connection::open(&db_path) else {
                return false;
            };
            // Watch both the directory (the db file is created lazily) and
            // the file itself; the fd stays non-blocking so poll() owns the
            // bound instead of an indefinite read.
            let parent = db_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let Ok(inotify) = Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK) else {
                return false;
            };
            // The Hub database runs in WAL mode: committed transactions land
            // in hub.sqlite-wal until a checkpoint, and the -wal file may be
            // created after spawn. Watch the directory for creation and both
            // files for modification/close; every committed write raises at
            // least one of these.
            let _dir_watch = inotify.add_watch(
                &parent,
                AddWatchFlags::IN_CLOSE_WRITE
                    | AddWatchFlags::IN_MOVED_TO
                    | AddWatchFlags::IN_CREATE,
            );
            let _file_watch = inotify.add_watch(
                &db_path,
                AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MODIFY,
            );
            let _wal_watch = inotify.add_watch(
                &db_path.with_extension("sqlite-wal"),
                AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MODIFY,
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            loop {
                if turn_row_exists(&conn, &instance_id) {
                    return true;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                // Event-driven block with a deadline-bounded poll timeout.
                let mut fds = [PollFd::new(inotify.as_fd(), PollFlags::POLLIN)];
                let timeout = nix::poll::PollTimeout::try_from(remaining)
                    .unwrap_or(nix::poll::PollTimeout::NONE);
                match poll(&mut fds, timeout) {
                    Ok(0) => return false, // deadline reached
                    Ok(_) => {
                        // Drain the event batch (non-blocking); the predicate
                        // is re-checked at the top of the loop.
                        let _ = inotify.read_events();
                    }
                    Err(nix::Error::EINTR) => continue,
                    Err(_) => return false,
                }
                // The WAL may be created lazily by the first transaction;
                // (re)arm its file watch after every wake so its later
                // modifications are observed. add_watch on an existing watch
                // path is idempotent enough for a test; ignore failures.
                let _ = inotify.add_watch(
                    &db_path.with_extension("sqlite-wal"),
                    AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MODIFY,
                );
            }
        }))
    }

    /// kqueue fallback (macOS/BSDs): an event-driven watch with no sleeps —
    /// `EVFILT_VNODE` on the database directory, the db file and its lazily
    /// created `-wal` file (c-ctxusage r16 item 7). Mirrors the Linux waiter:
    /// the directory watch covers create/rename (the db/-wal appear lazily),
    /// the file watches cover extend/write/delete; the predicate is re-checked
    /// after every event, with a deadline-bounded `kevent` timeout only as
    /// the safety bound.
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn spawn(db_path: PathBuf, instance_id: String) -> Self {
        Self(std::thread::spawn(move || {
            let Ok(conn) = rusqlite::Connection::open(&db_path) else {
                return false;
            };
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return false;
            }
            use std::os::fd::AsRawFd;
            let parent = db_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let wal_path = db_path.with_extension("sqlite-wal");

            // Add one EVFILT_VNODE watch. Missing files (the lazy -wal) are
            // simply skipped and (re)armed after every wake. A duplicate ADD of
            // an existing ident/filter re-arms the same watch. The opened fd
            // is intentionally kept open (forgotten) for the watch lifetime.
            // The `kevent` fields are assigned directly rather than via the
            // libc `EV_SET` macro, which not every libc release exports.
            let add_watch = |path: &Path, fflags: u32| {
                let Ok(fd) = std::fs::File::open(path) else {
                    return;
                };
                let change = libc::kevent {
                    ident: fd.as_raw_fd() as libc::uintptr_t,
                    filter: libc::EVFILT_VNODE,
                    // EV_ADD | EV_CLEAR: edge-style, one event per batch.
                    flags: (libc::EV_ADD | libc::EV_CLEAR) as u16,
                    fflags,
                    data: 0,
                    udata: std::ptr::null_mut(),
                };
                unsafe {
                    std::mem::forget(fd);
                    let mut event: libc::kevent = std::mem::zeroed();
                    let _ = libc::kevent(kq, &change, 1, &mut event, 0, std::ptr::null());
                }
            };
            // Directory: entries added/removed/renamed (lazy -wal creation).
            add_watch(&parent, libc::NOTE_WRITE);
            // Database file: growth/writes, unlink or rename.
            add_watch(
                &db_path,
                libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME,
            );

            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            loop {
                if turn_row_exists(&conn, &instance_id) {
                    return true;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                let mut timeout = libc::timespec {
                    tv_sec: remaining.as_secs() as libc::time_t,
                    tv_nsec: libc::c_long::from(remaining.subsec_nanos()),
                };
                let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                let ready =
                    unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, &mut timeout) };
                if ready == 0 {
                    return false; // deadline reached
                }
                if ready < 0 {
                    let errno = std::io::Error::last_os_error().raw_os_error();
                    if errno == Some(libc::EINTR) {
                        continue;
                    }
                    return false;
                }
                // The WAL may be created lazily by the first transaction;
                // (re)arm its file watch after every wake. Missing files are
                // ignored until they exist.
                add_watch(
                    &wal_path,
                    libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME,
                );
            }
        }))
    }

    /// Fallback for platforms with neither inotify nor kqueue (the gated e2e
    /// and CI run on Linux/macOS; this keeps the test compiling elsewhere).
    #[cfg(not(any(
        target_os = "linux",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )))]
    fn spawn(db_path: PathBuf, instance_id: String) -> Self {
        Self(std::thread::spawn(move || {
            let Ok(conn) = rusqlite::Connection::open(&db_path) else {
                return false;
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(45);
            while std::time::Instant::now() < deadline {
                if turn_row_exists(&conn, &instance_id) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            false
        }))
    }

    /// Block until the waiter exits; returns whether the row landed.
    fn join(self) -> bool {
        self.0.join().unwrap_or(false)
    }
}

fn turn_row_exists(conn: &rusqlite::Connection, instance_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM usage_events WHERE instance_id = ?1 AND scope = 'turn' LIMIT 1",
        rusqlite::params![instance_id],
        |_| Ok(()),
    )
    .is_ok()
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\n"
    );
    if let Some(body) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, rest.to_string())
}

fn cookie_from(text: &str) -> Option<String> {
    text.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, v)| v.trim().split(';').next().unwrap_or("").trim().to_string())
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> String {
    let body = json!({"bootstrapToken": bootstrap, "deviceName": "usage-e2e"}).to_string();
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    let req = format!(
        "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    cookie_from(&String::from_utf8_lossy(&buf)).expect("set-cookie")
}

const RUN_MARKER: &str = "REMUDA_USAGE_ROLLUP_CHILD";
const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
const HOOKS_ENV: &str = "REMUDA_PTY_HOOKS";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_shell_pty_claude_session_reports_a_usage_rollup() {
    // c-ctxusage r5 item 5a: a REAL self-skip. `ensure_workspace_bin` never
    // returns missing — it builds or panics — so this checks the existing
    // binary WITHOUT triggering a build and skips when the test was launched
    // without the fixture compiled (e.g. `cargo test -p remuda-node --lib` on
    // a host without the sidecar).
    let trigger = remuda_testing::locate_workspace_bin("fake-harness");
    let Some(trigger) = trigger.filter(|path| path.is_file()) else {
        eprintln!(
            "skipping: fake-harness sidecar is not built (build remuda-testing --bin fake-harness)"
        );
        return;
    };
    // agent_pty_kind reads REMUDA_PTY_CARRIER from the process env, so re-exec
    // under the native-carrier flags like the other shell-pty e2es.
    if std::env::var(RUN_MARKER).is_err() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_native_shell_pty_claude_session_reports_a_usage_rollup",
                "--nocapture",
            ])
            .env(RUN_MARKER, "1")
            .env(CARRIER_ENV, "native")
            .env(EMULATOR_ENV, "1")
            .env(HOOKS_ENV, "1")
            .output()
            .expect("re-exec usage rollup child");
        assert!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    if std::env::var("KEEP_USAGE_E2E").is_ok() {
        // Retain for diagnosis; `keep()` consumes the TempDir and leaks the dir.
        let retained: PathBuf = dir.keep();
        eprintln!("USAGE_E2E dir retained: {retained:?}");
    }
    let data_dir = root.join("node-data");
    let workspace = root.join("workspace");
    let home = root.join("claude-home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");

    // The fake harness stands in for claude 2.1.x: it runs the one-turn
    // `ok.json` scenario and writes an assistant block carrying message.usage.
    // `trigger` was located without building in the self-skip above; the
    // re-exec child rebuilds it if the sidecar is stale.
    let source = trigger;
    let dest = root.join("bin").join("claude");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::copy(&source, &dest).expect("copy fake claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let scenario = root.join("usage-ok.json");
    // One turn with a long-lived AUTO tool so the process stays foreground long
    // enough for the SessionStart hook + promotion poller to bind the
    // transcript (the same reason live_pipeline holds a slow tool). The
    // assistant block (with usage) is written when the tool returns.
    std::fs::write(
        &scenario,
        json!({
            "turns": [{
                "match_prefix": "USAGE",
                "text": "USAGE_TURN_DONE",
                "chunks": 1,
                "tools": [{
                    "name": "Bash",
                    "input": { "command": "sleep 8" },
                    "approval": "auto",
                    "duration_ms": 8000,
                    "exit_code": 0
                }],
                "stop_reason": "end_turn",
                // 2.1.289 dialect: split cache buckets with non-zero cache
                // creation (c-ctxusage r4 item 7c). Context the next request
                // carries = 1000 input + 9000 read + 333 write(5m) = 10333 →
                // 6% of the 200k fallback window.
                "usage": {
                    "input_tokens": 1000,
                    "output_tokens": 50,
                    "cached_tokens": 9000,
                    "cache_creation_5m": 333,
                    "cache_creation_1h": 7
                }
            }],
            "quit_after_turns": 1
        })
        .to_string(),
    )
    .unwrap();

    let mut native = NativeDriverConfig::new(data_dir.clone());
    native.pty_hooks = true;
    // Required for a shell-pty launch to run the real native agent (and its
    // transcript hydrator) rather than the fake driver.
    native.promote_terminal_agents = true;
    native.relay_binary = Some(Box::new(remuda_relay_bin(&root)));
    native
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    native.extra_env.insert(
        "FAKE_HARNESS_SCRIPT".into(),
        scenario.to_string_lossy().into_owned(),
    );
    native.extra_env.insert(
        "FAKE_HARNESS_EVENTS_OUT".into(),
        root.join("harness-events.jsonl")
            .to_string_lossy()
            .into_owned(),
    );

    let mut http_cfg = DevServerConfig::loopback(0);
    http_cfg.workspace_root = workspace.clone();
    http_cfg.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let registry = remuda_node::native_driver_registry(native.clone()).expect("registry");
    let node = DevNode::with_parts(
        &http_cfg,
        Arc::new(remuda_node::MemoryStore::new(256)),
        registry,
    )
    .expect("dev node");

    let hub = remuda_hub::spawn(HubConfig::for_test(root.join("hub")))
        .await
        .expect("hub");
    let host = node.host().meta.id.as_id().to_string();
    let _link = WssLink::connect_runtime(
        WssConfig::loopback(
            hub.addr,
            hub.mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
                .await
                .expect("token"),
            host.clone(),
        ),
        node.clone(),
    )
    .await
    .expect("node link");
    let cookie = login(hub.addr, &hub.bootstrap_token).await;

    let body = json!({
        "hostId": host,
        "kind": "claude",
        "driver": "shell-pty",
        "delegation": "none",
        "model": "fake",
        "prompt": "USAGE probe",
        "binaryPath": dest.to_string_lossy(),
        "claudeConfigDir": home.to_string_lossy()
    })
    .to_string();
    let (status, created) = http(hub.addr, "POST", "/v1/instances", &cookie, Some(&body)).await;
    assert!(
        status == 200 || status == 201,
        "create instance ({status}): {created}"
    );
    let created: Value = serde_json::from_str(&created).unwrap();
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .or_else(|| created["instance"]["id"].as_str())
        .expect("instance id")
        .to_owned();

    // Guaranteed cleanup runs whether the assertions below pass or fail.
    let cleanup = async {
        let (delete_status, _) = http(
            hub.addr,
            "DELETE",
            &format!("/v1/instances/{instance_id}?force=1"),
            &cookie,
            None,
        )
        .await;
        assert!(
            delete_status == 200 || delete_status == 204 || delete_status == 404,
            "cleanup delete: {delete_status}"
        );
    };

    // Wait for the transcript-derived usage to land. Event-driven without a
    // sleep: a second read-only SQLite connection registers an
    // update_hook on the Hub's database (the hook fires on every committed
    // write) and parks on a Condvar signalled by a file watcher. The bounded
    // overall deadline turns a regression into a failure; under correct
    // behavior the usage commit itself wakes this immediately
    // (c-ctxusage r15: no sleeps in tests).
    let db_path = root.join("hub/hub.sqlite");
    let waiter = UsageWaiter::spawn(db_path, instance_id.clone());
    let landed = tokio::task::spawn_blocking(move || waiter.join())
        .await
        .expect("db waiter task");
    if !landed {
        cleanup.await;
        let (_, journal) = http(
            hub.addr,
            "GET",
            &format!("/v1/instances/{instance_id}/journal"),
            &cookie,
            None,
        )
        .await;
        panic!("no usage rollup. journal tail: {journal}");
    }

    // The durable turn row has landed; the GET /v1/instances projection
    // (computed synchronously on read from usage_events) now returns the
    // rollup without any retry.
    let (_, body) = http(hub.addr, "GET", "/v1/instances", &cookie, None).await;
    let page: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let items = page["items"].as_array().cloned().unwrap_or_default();
    let found = items
        .iter()
        .find(|item| item["instanceId"].as_str() == Some(instance_id.as_str()))
        .or_else(|| items.first())
        .expect("instance still listed");
    let rollup = found["usageRollup"].clone();
    assert!(
        rollup.is_object(),
        "rollup projection missing after the durable turn row landed"
    );

    // Sync assertions captured so cleanup always runs, then the failure (if
    // any) is resumed.
    let checks: std::thread::Result<()> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let turns = rollup["turns"].as_i64().unwrap_or(0);
            assert!(turns >= 1, "at least one per-turn usage row: {rollup}");
            // 1000 uncached + 9000 read + 340 write(5m+1h) = 10340 → 5% of 200k.
            assert_eq!(
                rollup["contextUsedTokens"].as_i64(),
                Some(10_340),
                "split cache buckets from the 2.1.289 transcript: {rollup}"
            );
            assert_eq!(
                rollup["contextPct"].as_i64(),
                Some(5),
                "10340/200000 rounds to 5%: {rollup}"
            );
            assert_eq!(
                rollup["cacheCreationTokens"].as_i64(),
                Some(340),
                "5m+1h split sums: {rollup}"
            );
            assert_eq!(rollup["cacheReadTokens"].as_i64(), Some(9_000), "{rollup}");
            assert_eq!(
                rollup["sessionInputTokens"].as_i64(),
                Some(1_000),
                "{rollup}"
            );
            assert_eq!(rollup["sessionOutputTokens"].as_i64(), Some(50), "{rollup}");
        }));
    // Guaranteed cleanup runs whether the assertions passed or failed.
    cleanup.await;
    checks.expect("usage rollup assertions");
}
