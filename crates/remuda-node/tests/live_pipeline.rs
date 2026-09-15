//! End-to-end live pipeline against a real PTY (live-view design §4.2).
//!
//! `fake-harness --dialect-version modern` stands in for claude 2.1.270: a
//! 20 s tool, a native approval, the hook overlay the driver generates, and
//! the `--events-out` ground-truth channel. It runs through the full Node
//! runtime — native shell-pty factory, observation pump, message fold,
//! hook-gated activity — re-execing this test binary under the native-carrier
//! env, the same pattern as the WSS driver-inventory tests (process-wide env
//! is unsafe to mutate in-process, and the shell-pty agent arm reads
//! `REMUDA_PTY_CARRIER` at launch).
//!
//! The measurements answer the §1.1 target column:
//!
//! - harness submit edge → hook `UserPromptSubmit` committed to the journal
//! - harness tool-start (`PreToolUse`) → committed observation
//! - first `MessageDisplay` → the derived streaming message committed
//! - harness `Stop` edge → the hook turn-end observation
//! - a tight tail burst commits within 1.5× a single event's latency (the
//!   local-pump half of the relative-link guard; the Node→Hub uplink half is
//!   r-p2-createlag's `wss.rs` gate)
//! - rule 6: with hooks healthy the instance stays working until `Stop`; with
//!   no hook tier the screen's idle edge is allowed to end the turn

#![cfg(unix)]

use remuda_node::{
    CommandAction, CreateInstanceRequest, DevNode, DevServerConfig, InstanceCommandRequest,
    MemoryStore, NativeDriverConfig, ServeConfig,
};
use remuda_protocol::DriverKind;
use remuda_protocol::{
    Activity, AgentKind, Instance, InstanceLifecycle, Knowledge, LifecyclePayload,
    ObservationPayload,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Local-journal p50 budgets. The anchor lands immediately before the relay
/// spawns, so the number covers the ~22 ms command-hook floor plus scheduler
/// slack. Regression guards, not machine benchmarks.
const PROMPT_BUDGET_MS: i64 = 400;
const FIRST_FRAME_BUDGET_MS: i64 = 400;
const TURN_END_BUDGET_MS: i64 = 400;
/// The relative-link multiple for a tight emission burst.
const RELATIVE_LINK: f64 = 1.5;
/// Local commit slack on the relative bound.
const BURST_SLACK_MS: i64 = 300;
/// Consecutive journal commits closer than this are one source burst.
const BURST_GAP_MS: i64 = 100;

const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";
const EMULATOR_ENV: &str = "REMUDA_PTY_EMULATOR";
const HOOKS_ENV: &str = "REMUDA_PTY_HOOKS";
const RUN_MARKER: &str = "REMUDA_LIVE_PIPELINE_CHILD";
const HOOKS_CASE: &str = "REMUDA_LIVE_PIPELINE_HOOKS";

// ---------------------------------------------------------------------------
// Ground-truth anchors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Anchor {
    name: String,
    wall_ms: i64,
}

fn read_anchors(path: &Path) -> Vec<Anchor> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|row| Anchor {
            name: row
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            wall_ms: row.get("wallMs").and_then(Value::as_i64).unwrap_or(0),
        })
        .collect()
}

fn wait_anchor_file(path: &Path, name: &str, timeout: Duration) -> Anchor {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(anchor) = read_anchors(path).into_iter().find(|a| a.name == name) {
            return anchor;
        }
        assert!(
            Instant::now() < deadline,
            "anchor {name} never appeared at {path:?}"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

// ---------------------------------------------------------------------------
// Node runtime
// ---------------------------------------------------------------------------

struct LiveRun {
    node: DevNode,
    instance: Instance,
    events_path: PathBuf,
    _dir: tempfile::TempDir,
}

/// Install a 0755 fake binary outside the workspace cwd (the override guard
/// rejects group-writable target-dir files and anything inside the cwd).
fn install_fake_claude(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let source = remuda_testing::ensure_workspace_bin("fake-harness");
    let dest = install.join("claude");
    std::fs::copy(&source, &dest).expect("copy fake claude");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake claude");
    dest
}

async fn start_run(hooks: bool) -> LiveRun {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let data_dir = root.join("node-data");
    let workspace = root.join("workspace");
    let home = root.join("claude-home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&home).expect("home");
    let binary = install_fake_claude(&root);

    // The scenario plays off the live.json fixture; dialect/event log ride the
    // non-REMUDA env the child allowlist permits.
    let scenario = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../remuda-testing/fixtures/fake-harness/scenarios/live.json");

    let mut native = NativeDriverConfig::new(data_dir.clone());
    native.pty_hooks = hooks;
    // The relay defaults to this process's executable — in the re-exec that is
    // the test binary, so point it at the real CLI.
    native.relay_binary = Some(remuda_relay_bin(&root).into());
    // Test harness env passed through to every child (non-REMUDA names).
    native
        .extra_env
        .insert("FAKE_HARNESS_DIALECT_VERSION".into(), "modern".into());
    native.extra_env.insert(
        "FAKE_HARNESS_SCRIPT".into(),
        scenario.to_string_lossy().into_owned(),
    );
    let events_path = root.join("harness-events.jsonl");
    native.extra_env.insert(
        "FAKE_HARNESS_EVENTS_OUT".into(),
        events_path.to_string_lossy().into_owned(),
    );

    let config = ServeConfig {
        http: DevServerConfig::loopback(0)
            .with_workspace_root(workspace.clone())
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        data_dir: data_dir.clone(),
        drivers: remuda_node::LocalDrivers::Native(native.clone()),
    };
    let registry = remuda_node::native_driver_registry(native).expect("registry");
    let node = DevNode::with_parts(&config.http, Arc::new(MemoryStore::new(256)), registry)
        .expect("dev node");

    let request = CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Claude,
        driver: DriverKind::ShellPty,
        model: "fake".into(),
        args: Vec::new(),
        binary_path: Some(binary.to_string_lossy().into_owned()),
        binary_sha256: None,
        tui: None,
        extra_env: std::collections::BTreeMap::new(),
        provider_profile_id: "dev-fake".into(),
        permission_mode: "manual".into(),
        prompt: "LIVE probe".into(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: Some(home.to_string_lossy().into_owned()),
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
        resume_session_id: None,
        resumed_from: None,
        effort: None,
    };
    let created = node
        .create_instance(request)
        .await
        .expect("create instance");
    LiveRun {
        node,
        instance: created.instance,
        events_path,
        _dir: dir,
    }
}

/// Locate or build the real `remuda` CLI the hook overlay invokes.
fn remuda_relay_bin(root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_RELAY_BIN") {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        && let Some(candidate) = [dir.join("remuda"), dir.join("remuda.exe")]
            .into_iter()
            .find(|p| p.is_file())
    {
        return candidate;
    }
    // Build into a scratch target so a shared-target ETXTBSY cannot stall.
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

// ---------------------------------------------------------------------------
// Journal rows
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Row {
    at_ms: i64,
    kind: &'static str,
    name: Option<String>,
    channel: Option<String>,
    status: Option<String>,
}

fn timestamp_ms(text: &str) -> i64 {
    parse_rfc3339_ms(text).unwrap_or(0)
}

fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once('T')?;
    let mut d = date.split('-');
    let year: i32 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let time = rest.trim_end_matches(['Z']);
    let (hm, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hm.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    let millis: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    Some(
        days_from_civil(year, month, day) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1_000
            + millis,
    )
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

impl LiveRun {
    async fn journal(&self) -> Vec<Row> {
        let page = self
            .node
            .read_journal(&self.instance.journal_id, None, 512)
            .expect("read journal");
        page.events
            .iter()
            .filter_map(|event| {
                let remuda_protocol::JournalEvent::Instance(o) = event else {
                    return None;
                };
                let kind: &'static str = match &o.body {
                    ObservationPayload::Lifecycle(_) => "lifecycle",
                    ObservationPayload::Message(_) => "message",
                    ObservationPayload::ToolCall(_) => "tool_call",
                    ObservationPayload::ToolResult(_) => "tool_result",
                    _ => return None,
                };
                let (name, status) = match &o.body {
                    ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                        LifecyclePayload::Native(n) => (
                            Some(n.native_name.clone()),
                            match &n.status {
                                Knowledge::Known { value } => Some(value.clone()),
                                _ => None,
                            },
                        ),
                        LifecyclePayload::Entity(_) => (None, None),
                    },
                    _ => (None, None),
                };
                Some(Row {
                    at_ms: timestamp_ms(&String::from(o.observed_at.clone())),
                    kind,
                    name,
                    channel: Some(format!("{:?}", o.source.channel).to_lowercase()),
                    status,
                })
            })
            .collect()
    }

    async fn wait_journal<F>(&self, timeout: Duration, pred: F)
    where
        F: Fn(&Row) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if self.journal().await.iter().any(&pred) {
                return;
            }
            assert!(Instant::now() < deadline, "journal condition never met");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn find_row<F>(&self, pred: F) -> Option<Row>
    where
        F: Fn(&Row) -> bool,
    {
        self.journal().await.into_iter().find(pred)
    }

    async fn activity(&self) -> Knowledge<Activity> {
        self.node
            .get_instance(&self.instance.meta.id)
            .expect("instance")
            .activity
    }

    async fn answer_approval(&self, digit: &str) {
        self.node
            .submit_command(
                &self.instance.meta.id,
                InstanceCommandRequest {
                    origin: remuda_protocol::InputOrigin::Human,
                    command_id: None,
                    operation: CommandAction::WriteTty,
                    prompt: None,
                    attachments: Vec::new(),
                    run_id: None,
                    interaction_id: None,
                    answer: None,
                    keys: Some(vec![digit.to_owned()]),
                    model: None,
                    effort_name: None,
                    effort_index: None,
                },
            )
            .await
            .expect("answer approval");
    }

    async fn drive_to_exit(&self) {
        wait_anchor_file(
            &self.events_path,
            "approval_prompt",
            Duration::from_secs(35),
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
        self.answer_approval("1").await;
        wait_anchor_file(&self.events_path, "exit", Duration::from_secs(20));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(
                self.node
                    .get_instance(&self.instance.meta.id)
                    .map(|i| i.lifecycle),
                Ok(InstanceLifecycle::Exited) | Ok(InstanceLifecycle::Failed)
            ) {
                break;
            }
            assert!(Instant::now() < deadline, "instance never exited");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn is_hook(row: &Row, name: &str) -> bool {
    row.channel.as_deref() == Some("hook") && row.name.as_deref() == Some(name)
}

/// The native-carrier-3 acceptance: one PONG-shaped turn through the native
/// shell-pty carrier must populate the same four channels the herdr carrier
/// produced in the macOS reference run — runtime lifecycle, the hook bus, the
/// terminal/screen tier (`pty`, herdr's counterpart), and the transcript.
async fn assert_all_four_native_channels(run: &LiveRun) {
    let rows = run.journal().await;
    let mut present = std::collections::BTreeSet::new();
    for row in &rows {
        if let Some(channel) = row.channel.as_deref() {
            present.insert(channel.to_owned());
        }
    }
    for channel in ["runtime", "hook", "pty", "transcript"] {
        assert!(
            present.contains(channel),
            "the hooked native shell-pty turn never wrote a {channel} event; got {present:?}"
        );
    }
    // The hook channel is the one the macOS retest lost entirely.
    assert!(
        rows.iter().any(|row| is_hook(row, "SessionStart")),
        "SessionStart must bind the session through the hook socket"
    );
    assert!(
        rows.iter()
            .any(|row| row.kind == "message" && row.channel.as_deref() == Some("hook")),
        "MessageDisplay must derive a streaming message on the hook channel"
    );
}

fn is_screen(row: &Row, label: &str) -> bool {
    row.channel.as_deref() == Some("pty")
        && row.name.as_deref() == Some("agent_status")
        && row.status.as_deref() == Some(label)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budgets_and_rule_six_over_a_real_pty() {
    // Re-exec under the native-carrier flags, like the WSS inventory tests.
    if std::env::var(RUN_MARKER).is_err() {
        return re_exec_child();
    }
    let hooks = std::env::var(HOOKS_CASE).as_deref() == Ok("hooks");

    let run = start_run(hooks).await;

    if hooks {
        run.wait_journal(Duration::from_secs(30), |row| is_screen(row, "working"))
            .await;
        run.drive_to_exit().await;
        assert_budgets(&run).await;
        assert_quiet_tool_window(&run).await;
        assert_relative_burst(&run).await;
        assert_no_screen_idle_before_stop(&run).await;
        assert_all_four_native_channels(&run).await;
        assert_eq!(
            run.activity().await,
            Knowledge::Known {
                value: Activity::Idle
            }
        );
    } else {
        run.wait_journal(Duration::from_secs(30), |row| is_screen(row, "working"))
            .await;
        run.drive_to_exit().await;
        let rows = run.journal().await;
        assert!(rows.iter().any(|row| is_screen(row, "idle")));
        assert!(
            !rows
                .iter()
                .any(|row| row.channel.as_deref() == Some("hook"))
        );
        assert_eq!(
            run.activity().await,
            Knowledge::Known {
                value: Activity::Idle
            }
        );
    }
}

fn re_exec_child() {
    let cases = ["hooks", "no-hooks"];
    for case in cases {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "budgets_and_rule_six_over_a_real_pty",
                "--nocapture",
            ])
            .env(RUN_MARKER, "1")
            .env(HOOKS_CASE, case)
            .env(CARRIER_ENV, "native")
            .env(EMULATOR_ENV, "1")
            .env(HOOKS_ENV, "1")
            .output()
            .expect("re-exec live pipeline child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            panic!(
                "{case} child failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // Both cases must finish; the hooks case prints the measured numbers.
        println!("[{case}] {stdout}");
    }
}

async fn assert_budgets(run: &LiveRun) {
    let anchors = read_anchors(&run.events_path);
    let anchor = |name: &str| {
        anchors
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("harness anchor {name} missing"))
    };
    let submit = anchor("submit");
    let tool_start = anchor("tool_start_hook");
    let message_hook = anchor("message_hook");
    let stop_hook = anchor("stop_hook");

    let prompt = run
        .find_row(|row| is_hook(row, "UserPromptSubmit"))
        .await
        .expect("UserPromptSubmit committed");
    let tool = run
        .find_row(|row| is_hook(row, "PreToolUse"))
        .await
        .expect("PreToolUse committed");
    let stop = run
        .find_row(|row| is_hook(row, "Stop"))
        .await
        .expect("Stop committed");
    let message = run
        .find_row(|row| row.kind == "message" && row.channel.as_deref() == Some("hook"))
        .await
        .expect("a streaming message derived from MessageDisplay committed");

    let prompt_ms = prompt.at_ms - submit.wall_ms;
    let tool_ms = tool.at_ms - tool_start.wall_ms;
    let message_ms = message.at_ms - message_hook.wall_ms;
    let stop_ms = stop.at_ms - stop_hook.wall_ms;
    println!(
        "native→journal latencies (ms): prompt={prompt_ms} toolStart={tool_ms} \
         firstChunk={message_ms} stop={stop_ms}"
    );
    assert!(
        prompt_ms <= PROMPT_BUDGET_MS,
        "submit→UserPromptSubmit {prompt_ms} ms"
    );
    assert!(
        tool_ms <= FIRST_FRAME_BUDGET_MS,
        "tool start→journal {tool_ms} ms"
    );
    assert!(
        message_ms <= FIRST_FRAME_BUDGET_MS,
        "first chunk→journal {message_ms} ms"
    );
    assert!(stop_ms <= TURN_END_BUDGET_MS, "Stop→journal {stop_ms} ms");
}

async fn assert_quiet_tool_window(run: &LiveRun) {
    let anchors = read_anchors(&run.events_path);
    let start = anchors
        .iter()
        .find(|a| a.name == "tool_start_hook")
        .expect("tool start anchor");
    let finish = anchors
        .iter()
        .find(|a| a.name == "tool_finish_hook")
        .expect("tool finish anchor");
    assert!(
        finish.wall_ms - start.wall_ms >= 19_000,
        "the scenario tool must really run ~20 s, got {} ms",
        finish.wall_ms - start.wall_ms
    );
    // The single working transition rides the first promotion poll (≤ 800 ms
    // after the turn starts); after that, the whole 20 s interior must carry
    // zero screen events — not the 10 Hz spinner. The tail grace excludes the
    // approval/finish phase edges.
    let interior_start = start.wall_ms + 1_000;
    let interior_end = finish.wall_ms - 300;
    let mut pty_inside = 0;
    for row in run.journal().await {
        if row.at_ms > interior_start
            && row.at_ms < interior_end
            && row.channel.as_deref() == Some("pty")
        {
            pty_inside += 1;
        }
    }
    assert_eq!(
        pty_inside, 0,
        "the 20 s tool interior carried {pty_inside} screen/pty events — the spinner leaked"
    );
}

async fn assert_relative_burst(run: &LiveRun) {
    let anchors = read_anchors(&run.events_path);
    let submit = anchors
        .iter()
        .find(|a| a.name == "submit")
        .expect("submit anchor");
    let rows = run.journal().await;
    let single_ms = run
        .find_row(|row| is_hook(row, "UserPromptSubmit"))
        .await
        .map(|row| (row.at_ms - submit.wall_ms).max(1))
        .unwrap_or(1);

    // Largest run of consecutive commits within BURST_GAP_MS.
    let mut best: Vec<&Row> = Vec::new();
    let mut current: Vec<&Row> = Vec::new();
    let mut prev: Option<&Row> = None;
    for row in &rows {
        match prev {
            Some(p) if row.at_ms - p.at_ms <= BURST_GAP_MS => current.push(row),
            _ => current = vec![row],
        }
        if current.len() > best.len() {
            best = current.clone();
        }
        prev = Some(row);
    }
    assert!(
        best.len() >= 3,
        "expected a tight tail burst, got {} rows",
        best.len()
    );
    let span = best.last().unwrap().at_ms - best.first().unwrap().at_ms;
    let budget = (single_ms as f64 * RELATIVE_LINK) as i64 + BURST_SLACK_MS;
    println!(
        "tail burst: {} events over {span} ms; single-event {single_ms} ms; budget {budget} ms",
        best.len()
    );
    assert!(
        span <= budget,
        "{}-event burst span {span} ms exceeds 1.5x single {single_ms} ms + slack",
        best.len()
    );
}

async fn assert_no_screen_idle_before_stop(run: &LiveRun) {
    let rows = run.journal().await;
    let stop_idx = rows
        .iter()
        .position(|row| is_hook(row, "Stop"))
        .expect("Stop");
    // Rule 6 forbids a *downward* screen transition after the screen raised
    // busy: once working, no idle/blocked replacement may precede Stop. The
    // initial composer idle before any turn (idx 8 in the boot sequence) is
    // not a lowering and is allowed.
    let mut raised = false;
    for (idx, row) in rows.iter().enumerate().take(stop_idx) {
        if is_screen(row, "working") {
            raised = true;
        }
        // `blocked` mid-turn is the dialog edge and also legitimate; only a
        // return to `idle` after a raise is the forbidden premature end.
        if raised && is_screen(row, "idle") {
            panic!(
                "screen tier lowered to idle before Stop idx={idx} stop_idx={stop_idx} row_at={} stop_at={}",
                row.at_ms, rows[stop_idx].at_ms
            );
        }
    }
}
