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

    // Wait for the transcript-derived usage rollup to land.
    let rollup = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let (_, body) = http(hub.addr, "GET", "/v1/instances", &cookie, None).await;
            let page: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let items = page["items"].as_array().cloned().unwrap_or_default();
            if let Some(found) = items
                .iter()
                .find(|item| item["instanceId"].as_str() == Some(instance_id.as_str()))
                .or_else(|| items.first())
                .cloned()
            {
                eprintln!(
                    "INSTANCE lifecycle={} activity={} rollup={}",
                    found["lifecycle"],
                    found["activity"],
                    if found["usageRollup"].is_object() {
                        "yes"
                    } else {
                        "no"
                    }
                );
                if found["usageRollup"].is_object() {
                    break found["usageRollup"].clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    let rollup = match rollup {
        Ok(r) => r,
        Err(_) => {
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
    };

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
