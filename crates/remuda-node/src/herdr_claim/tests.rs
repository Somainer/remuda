//! D-062 claim tests against the fake herdr.
//!
//! The "external" session is a fake herdr server bound at
//! `<root>/sessions/work/herdr.sock`, seeded the way a human would leave it:
//!
//! * `w1` / tab `w1:t1` / pane `w1:p1` running a claude agent;
//! * `w1` / tab `w1:t2` with two panes, `w1:p2` and `w1:p3`;
//! * `w2` / tab `w2:t3` / pane `w2:p4`.
//!
//! The fake's opt-in call log (`<socket>.calls.jsonl`) records the method and
//! the ids of every RPC and terminal session, never any text, so each test can
//! prove which panes the Node touched and that it closed nothing.

use super::*;
use crate::{DevServerConfig, DriverRegistry, MemoryStore, NativeDriverConfig};
use remuda_herdr::{
    AgentStartParams, PaneSplitParams, SplitDirection, TabCreateParams, WorkspaceCreateParams,
};
use remuda_testing::{FakeHerdrOptions, FakeHerdrServer, ShortTempDir, calls_neighbor};
use std::time::Instant;

const WAIT: Duration = Duration::from_secs(15);

struct Harness {
    data: tempfile::TempDir,
    root: ShortTempDir,
    store: Arc<MemoryStore>,
    node: DevNode,
    socket: PathBuf,
    calls: PathBuf,
    _server: FakeHerdrServer,
    _own: FakeHerdrServer,
}

impl Harness {
    async fn new() -> Self {
        let data = tempfile::tempdir().expect("data dir");
        let root = ShortTempDir::for_socket_suffix("sessions/work/herdr.sock").expect("short root");
        let socket = root.path().join("sessions/work/herdr.sock");
        std::fs::create_dir_all(socket.parent().expect("parent")).expect("session dir");
        let calls = calls_neighbor(&socket);
        std::fs::write(&calls, "").expect("opt into the call log");
        let server =
            FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).expect("external herdr");
        // The Node's own carrier session lives in the same config dir, so
        // discovery and claims must recognise and refuse it.
        let own_dir = root.path().join("sessions/mine");
        std::fs::create_dir_all(&own_dir).expect("own session dir");
        let own = FakeHerdrServer::spawn(FakeHerdrOptions::new(own_dir.join("herdr.sock")))
            .expect("own herdr");
        let store = Arc::new(MemoryStore::new(256));
        let node = node(&store, data.path(), &own_dir);
        node.set_herdr_claim_root(root.path().to_path_buf());
        let harness = Self {
            data,
            root,
            store,
            node,
            socket,
            calls,
            _server: server,
            _own: own,
        };
        harness.seed().await;
        harness.clear_calls();
        harness
    }

    async fn seed(&self) {
        let client = self.client();
        let created = client
            .workspace_create(WorkspaceCreateParams {
                label: Some("human-work".into()),
                ..Default::default()
            })
            .await
            .expect("w1");
        assert_eq!(created.root_pane.pane_id, "w1:p1");
        client
            .agent_start(AgentStartParams {
                name: "human-claude".into(),
                kind: "claude".into(),
                pane_id: "w1:p1".into(),
                args: Vec::new(),
                timeout_ms: None,
            })
            .await
            .expect("human claude");
        // D-062 r2: the agent is a real foreground process (this test
        // process: alive throughout, with a real kernel start time), never the
        // pane's shell.
        client
            .call_raw(
                "fake.set_pane_process",
                json!({"pane_id": "w1:p1", "pid": std::process::id(), "name": "claude"}),
            )
            .await
            .expect("claude is the pane's foreground");
        let tab = client
            .tab_create(TabCreateParams {
                workspace_id: Some("w1".into()),
                ..Default::default()
            })
            .await
            .expect("w1:t2");
        assert_eq!(tab.root_pane.pane_id, "w1:p2");
        let split = client
            .pane_split(PaneSplitParams {
                direction: SplitDirection::Right,
                workspace_id: Some("w1".into()),
                target_pane_id: Some("w1:p2".into()),
                ratio: None,
                cwd: None,
                env: Default::default(),
                focus: false,
            })
            .await
            .expect("w1:p3");
        assert_eq!(split.pane.pane_id, "w1:p3");
        let other = client
            .workspace_create(WorkspaceCreateParams {
                label: Some("other".into()),
                ..Default::default()
            })
            .await
            .expect("w2");
        assert_eq!(other.tab.tab_id, "w2:t3");
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket).with_timeout(Duration::from_secs(5))
    }

    fn clear_calls(&self) {
        std::fs::write(&self.calls, "").expect("clear call log");
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.calls)
            .expect("call log")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("call line"))
            .collect()
    }

    fn target(&self, workspace: &str, tab: &str, pane: Option<&str>) -> HerdrClaimTarget {
        HerdrClaimTarget {
            session: "work".into(),
            workspace_id: workspace.into(),
            tab_id: tab.into(),
            pane_id: pane.map(str::to_owned),
            kind: None,
        }
    }

    async fn claim(&self, target: HerdrClaimTarget) -> Result<Instance, NodeError> {
        self.node
            .claim_instance(InstanceClaimParams {
                instance_id: InstanceId::new().as_id().to_string(),
                target,
            })
            .await
    }

    fn journal(&self, id: &InstanceId) -> String {
        let instance = self.store.get_instance(id).expect("instance");
        let page = self
            .store
            .read_events(&instance.journal_id, None, 1024)
            .expect("journal");
        serde_json::to_string(&page.events).expect("journal json")
    }

    async fn wait_lifecycle(&self, id: &InstanceId, want: InstanceLifecycle) {
        let start = Instant::now();
        while self.store.get_instance(id).expect("instance").lifecycle != want {
            assert!(
                start.elapsed() < WAIT,
                "instance never reached {want:?}; journal: {}",
                self.journal(id)
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Statuses of every `herdr_claim_contact` event, oldest first.
    fn contact_states(&self, id: &InstanceId) -> Vec<String> {
        let instance = self.store.get_instance(id).expect("instance");
        let page = self
            .store
            .read_events(&instance.journal_id, None, 1024)
            .expect("journal");
        let mut found = Vec::new();
        collect_native(
            &serde_json::to_value(&page.events).expect("json"),
            CONTACT_EVENT,
            &mut found,
        );
        found
    }

    async fn wait_contact(&self, id: &InstanceId, want: &[&str]) {
        let start = Instant::now();
        while self.contact_states(id) != want {
            assert!(
                start.elapsed() < WAIT,
                "contact events {:?} never became {want:?}",
                self.contact_states(id)
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_journal(&self, id: &InstanceId, needle: &str) {
        let start = Instant::now();
        while !self.journal(id).contains(needle) {
            assert!(
                start.elapsed() < WAIT,
                "journal never carried {needle}: {}",
                self.journal(id)
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_call(&self, method: &str, pane: &str) {
        let start = Instant::now();
        while !self
            .calls()
            .iter()
            .any(|call| call["method"] == method && touches(call, pane))
        {
            assert!(
                start.elapsed() < WAIT,
                "no {method} reached {pane}: {:?}",
                self.calls()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Every pane id any logged call named.
    fn touched_panes(&self) -> Vec<String> {
        let mut panes: Vec<String> = self
            .calls()
            .iter()
            .flat_map(|call| {
                ["pane_id", "target", "target_pane_id"]
                    .into_iter()
                    .filter_map(|key| call.get(key).and_then(Value::as_str))
                    .filter(|id| id.contains(":p"))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        panes.sort();
        panes.dedup();
        panes
    }

    fn destructive_calls(&self) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|call| {
                matches!(
                    call["method"].as_str(),
                    Some(
                        "pane.close"
                            | "tab.close"
                            | "workspace.close"
                            | "server.stop"
                            | "agent.start"
                            | "workspace.create"
                            | "tab.create"
                            | "pane.split"
                    )
                )
            })
            .collect()
    }
}

/// Collect the `status` of every native lifecycle named `name`.
fn collect_native(value: &Value, name: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.get("nativeName").and_then(Value::as_str) == Some(name)
                && let Some(status) = map
                    .get("status")
                    .and_then(|status| status.get("value").or(Some(status)))
                    .and_then(Value::as_str)
            {
                out.push(status.to_owned());
            }
            for child in map.values() {
                collect_native(child, name, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_native(item, name, out);
            }
        }
        _ => {}
    }
}

fn touches(call: &Value, pane: &str) -> bool {
    ["pane_id", "target", "target_pane_id"]
        .into_iter()
        .any(|key| call.get(key).and_then(Value::as_str) == Some(pane))
}

fn node(store: &Arc<MemoryStore>, data: &Path, own_dir: &Path) -> DevNode {
    let mut config = NativeDriverConfig::new(data.to_path_buf());
    config.herdr_socket_dir = Some(own_dir.to_path_buf());
    config.herdr_session = "remuda-node-claim-test".into();
    config.herdr_binary = Some(remuda_testing::fake_herdr_bin());
    let node = DevNode::with_parts(
        &DevServerConfig::loopback(0).with_workspace_roots(remuda_testing::test_workspace_roots!()),
        Arc::clone(store) as Arc<dyn LocalStore>,
        DriverRegistry::with_fake().expect("fake registry"),
    )
    .expect("node")
    .with_herdr_config(config)
    .expect("herdr config");
    node.set_herdr_claim_poll(Duration::from_millis(40));
    node
}

fn error_text(result: Result<impl std::fmt::Debug, NodeError>) -> String {
    result.expect_err("refused").to_string()
}

#[tokio::test]
async fn discovery_lists_the_id_tree_and_never_reads_pane_content() {
    let harness = Harness::new().await;
    let found = harness
        .node
        .herdr_discover(HerdrDiscoverParams::default())
        .await
        .expect("discover");
    let names: Vec<&str> = found.sessions.iter().map(|s| s.session.as_str()).collect();
    assert_eq!(
        names,
        vec!["work"],
        "the Node's own session is never listed"
    );
    let work = &found.sessions[0];
    assert!(work.reachable, "{work:?}");
    let tree: Vec<(String, String, Vec<String>)> = work
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().map(|tab| {
                (
                    workspace.workspace_id.clone(),
                    tab.tab_id.clone(),
                    tab.panes.iter().map(|pane| pane.pane_id.clone()).collect(),
                )
            })
        })
        .collect();
    assert_eq!(
        tree,
        vec![
            ("w1".into(), "w1:t1".into(), vec!["w1:p1".into()]),
            (
                "w1".into(),
                "w1:t2".into(),
                vec!["w1:p2".into(), "w1:p3".into()]
            ),
            ("w2".into(), "w2:t3".into(), vec!["w2:p4".into()]),
        ]
    );
    let claude = &work.workspaces[0].tabs[0].panes[0];
    assert_eq!(claude.agent.as_deref(), Some("claude"));
    assert_eq!(claude.agent_status, "idle");
    assert!(claude.process.is_some(), "process command is listed");
    let methods: Vec<String> = harness
        .calls()
        .iter()
        .filter_map(|call| call["method"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !methods
            .iter()
            .any(|method| matches!(method.as_str(), "pane.read" | "agent.read")),
        "discovery must never read pane content: {methods:?}"
    );
    assert!(harness.destructive_calls().is_empty());
}

#[tokio::test]
async fn claim_validates_the_id_chain_and_the_single_pane_rule() {
    let harness = Harness::new().await;
    let refuse = |target| async { error_text(harness.claim(target).await) };

    let missing = refuse(harness.target("w9", "w1:t1", None)).await;
    assert!(
        missing.contains(HERDR_CLAIM_INVALID) && missing.contains("w9"),
        "{missing}"
    );
    let wrong_workspace = refuse(harness.target("w1", "w2:t3", None)).await;
    assert!(
        wrong_workspace.contains(HERDR_CLAIM_INVALID)
            && wrong_workspace.contains("belongs to workspace w2"),
        "{wrong_workspace}"
    );
    let wrong_tab = refuse(harness.target("w1", "w1:t1", Some("w1:p2"))).await;
    assert!(
        wrong_tab.contains(HERDR_CLAIM_INVALID) && wrong_tab.contains("belongs to tab w1:t2"),
        "{wrong_tab}"
    );
    let ambiguous = refuse(harness.target("w1", "w1:t2", None)).await;
    assert!(
        ambiguous.contains(HERDR_CLAIM_AMBIGUOUS) && ambiguous.contains("w1:p2, w1:p3"),
        "a multi-pane tab without a pane id lists the pane ids: {ambiguous}"
    );
    let mut bad_kind = harness.target("w1", "w1:t1", None);
    bad_kind.kind = Some("emacs".into());
    assert!(refuse(bad_kind).await.contains(HERDR_CLAIM_INVALID));
    let mut own = harness.target("w1", "w1:t1", None);
    own.session = "mine".into();
    assert!(refuse(own).await.contains(HERDR_CLAIM_OWN_SESSION));
    assert!(
        harness
            .store
            .list_instances()
            .expect("instances")
            .is_empty(),
        "a refused claim creates nothing"
    );

    let claimed = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("single-pane tab without --pane");
    assert_eq!(claimed.driver, DriverKind::HerdrClaimed);
    assert_eq!(
        claimed.kind,
        AgentKind::Claude,
        "herdr's agent kind is the default"
    );
    assert_eq!(claimed.lifecycle, InstanceLifecycle::Ready);
    assert_eq!(claimed.ownership, Ownership::AdoptedControl);
    assert_eq!(claimed.launched_by, Some(LaunchedBy::User));
    let claim = claimed.herdr_claim.expect("claim record");
    assert_eq!(
        (
            claim.session.as_str(),
            claim.workspace_id.as_str(),
            claim.tab_id.as_str(),
            claim.pane_id.as_str()
        ),
        ("work", "w1", "w1:t1", "w1:p1")
    );
    assert_eq!(claim.agent.as_deref(), Some("claude"));
    let process = claim.process.expect("process info recorded at claim time");
    assert_eq!(
        (process.pid, process.command.as_str()),
        (std::process::id(), "claude")
    );
    assert!(
        process.start_ticks.is_some(),
        "the kernel start time is recorded"
    );
    let explicit = harness
        .claim(HerdrClaimTarget {
            kind: Some("shell".into()),
            ..harness.target("w1", "w1:t2", Some("w1:p3"))
        })
        .await
        .expect("explicit pane in a multi-pane tab");
    assert_eq!(explicit.kind, AgentKind::Terminal);
}

#[tokio::test]
async fn a_second_live_claim_on_the_same_pane_conflicts() {
    let harness = Harness::new().await;
    let first = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("first claim");
    let second = error_text(
        harness
            .claim(harness.target("w1", "w1:t1", Some("w1:p1")))
            .await,
    );
    assert!(
        second.contains(HERDR_CLAIM_CONFLICT) && second.contains(first.meta.id.as_id().as_str()),
        "{second}"
    );
    let replay = harness
        .node
        .claim_instance(InstanceClaimParams {
            instance_id: first.meta.id.as_id().to_string(),
            target: harness.target("w1", "w1:t1", None),
        })
        .await
        .expect("same instance id is idempotent");
    assert_eq!(replay.meta.id, first.meta.id);
    assert_eq!(harness.store.list_instances().expect("instances").len(), 1);
}

#[tokio::test]
async fn send_keys_and_tty_reach_only_the_claimed_pane() {
    let harness = Harness::new().await;
    let claimed = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim");
    let id = claimed.meta.id.clone();
    let wire_id = id.as_id().to_string();
    // The TTY bridge is a control session on the claimed pane, opened at claim
    // at the pane's own size (the fake's viewport is 40 rows), never the 80x24
    // default that would reflow the human's terminal.
    harness.wait_call("terminal.control", "w1:p1").await;
    let control = harness
        .calls()
        .into_iter()
        .find(|call| call["method"] == "terminal.control")
        .expect("control session");
    assert_eq!(control["target"], "80x40", "{control}");

    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.send",
        json!({"instanceId": wire_id, "input": {"type": "prompt", "text": "hello pane"}, "origin": "human"}),
    )
    .await
    .expect("send");
    harness.wait_call("agent.prompt", "w1:p1").await;

    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.keys",
        json!({"instanceId": wire_id, "keys": ["esc"], "origin": "human"}),
    )
    .await
    .expect("keys");
    harness.wait_call("terminal.input", "w1:p1").await;

    // The command path (interrupt) types into the same pane.
    let pane: Arc<dyn Driver> = harness
        .node
        .inner
        .instance_drivers
        .read()
        .await
        .get(&id)
        .cloned()
        .expect("claimed driver");
    pane.execute(DriverRequest::Cancel)
        .await
        .expect("interrupt");
    harness.wait_call("pane.send_keys", "w1:p1").await;

    assert_eq!(
        harness.touched_panes(),
        vec!["w1:p1".to_owned()],
        "every call named the claimed pane and no other: {:?}",
        harness.calls()
    );
    assert!(
        harness.destructive_calls().is_empty(),
        "{:?}",
        harness.calls()
    );
    let screen = pane.screen_read().await.expect("screen").expect("grid");
    assert!(screen.emulated);
}

#[tokio::test]
async fn release_detaches_and_leaves_the_pane_and_its_process_alive() {
    let harness = Harness::new().await;
    let claimed = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim");
    let id = claimed.meta.id.clone();
    harness.wait_call("terminal.control", "w1:p1").await;
    harness.clear_calls();

    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.close",
        json!({"instanceId": id.as_id().as_str(), "origin": "human"}),
    )
    .await
    .expect("release");
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    harness.wait_journal(&id, REASON_RELEASED).await;
    // A second release is harmless and still touches nothing.
    let _ = crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.close",
        json!({"instanceId": id.as_id().as_str(), "origin": "human"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let control_calls: Vec<Value> = harness
        .calls()
        .into_iter()
        .filter(|call| {
            !matches!(
                call["method"].as_str(),
                Some("session.snapshot" | "pane.process_info" | "agent.get" | "ping")
            )
        })
        .collect();
    assert!(
        control_calls.is_empty(),
        "release must not close, kill, type into or restart anything: {control_calls:?}"
    );
    let snapshot = harness.client().session_snapshot().await.expect("snapshot");
    assert!(
        snapshot.panes.iter().any(|pane| pane.pane_id == "w1:p1"),
        "the pane keeps running"
    );
    assert!(
        snapshot.agents.iter().any(|agent| agent.pane_id == "w1:p1"),
        "its process keeps running"
    );
    assert!(
        harness.node.tty().attach(&id).await.is_err(),
        "the TTY bridge is gone after release"
    );
}

#[tokio::test]
async fn pane_gone_ends_the_instance_with_evidence_and_never_touches_it() {
    let harness = Harness::new().await;
    let claimed = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim");
    let id = claimed.meta.id.clone();
    harness.clear_calls();
    // The human closes the pane themselves.
    harness
        .client()
        .pane_close("w1:p1")
        .await
        .expect("human closes pane");
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PANE_GONE), "{journal}");
    assert!(
        journal.contains("pane w1:p1 is no longer in herdr session work"),
        "the evidence names the pane: {journal}"
    );
    let closes = harness
        .calls()
        .into_iter()
        .filter(|call| call["method"] == "pane.close")
        .count();
    assert_eq!(
        closes,
        1,
        "only the human's own close: {:?}",
        harness.calls()
    );
}

/// A stand-in process, killed and reaped on drop so a failing test leaves
/// nothing running.
struct Sleeper(std::process::Child);

impl std::ops::Deref for Sleeper {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Sleeper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sleeper() -> Sleeper {
    Sleeper(
        std::process::Command::new("sleep")
            .arg("600")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("stand-in process"),
    )
}

async fn set_process(harness: &Harness, pane: &str, pid: u32, name: &str, shell: Option<u32>) {
    let mut params = json!({"pane_id": pane, "pid": pid, "name": name});
    if let Some(shell) = shell {
        params["shell_pid"] = json!(shell);
    }
    harness
        .client()
        .call_raw("fake.set_pane_process", params)
        .await
        .expect("set pane process");
}

async fn claim_codex_on_p2(harness: &Harness) -> Instance {
    harness
        .claim(HerdrClaimTarget {
            kind: Some("codex".into()),
            ..harness.target("w1", "w1:t2", Some("w1:p2"))
        })
        .await
        .expect("claim")
}

#[tokio::test]
async fn an_agent_claim_needs_a_foreground_agent_not_the_shell() {
    let harness = Harness::new().await;
    // w1:p2 runs only its shell: an agent claim is not ready, never pinned to
    // the shell pid.
    let refused = error_text(
        harness
            .claim(HerdrClaimTarget {
                kind: Some("claude".into()),
                ..harness.target("w1", "w1:t2", Some("w1:p2"))
            })
            .await,
    );
    assert!(refused.contains(HERDR_CLAIM_NOT_READY), "{refused}");
    assert!(harness.store.list_instances().expect("rows").is_empty());
}

#[tokio::test]
async fn a_different_program_in_the_claimed_pane_ends_the_claim() {
    let harness = Harness::new().await;
    let mut agent = sleeper();
    set_process(&harness, "w1:p2", agent.id(), "codex", None).await;
    let claimed = claim_codex_on_p2(&harness).await;
    let id = claimed.meta.id.clone();
    let recorded = claimed
        .herdr_claim
        .and_then(|claim| claim.process)
        .expect("process");
    assert_eq!(
        (recorded.pid, recorded.command.as_str()),
        (agent.id(), "codex")
    );
    assert!(recorded.start_ticks.is_some());
    // A different program takes the pane's foreground. The agent stays alive
    // until the claim has ended: a watcher probe already in flight may have
    // read the old pid from herdr, and if the agent died before it read the
    // pid's start time it would rightly journal "has exited" instead.
    let mut other = sleeper();
    set_process(&harness, "w1:p2", other.id(), "vim", None).await;
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PROCESS_GONE), "{journal}");
    assert!(
        journal.contains("is no longer the foreground agent"),
        "{journal}"
    );
    agent.kill().expect("agent exits");
    agent.wait().expect("reaped");
    // Nothing more is written to the pane.
    harness.clear_calls();
    let wire = id.as_id().to_string();
    let _ = crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.send",
        json!({"instanceId": wire, "input": {"type": "prompt", "text": "x"}, "origin": "human"}),
    )
    .await;
    let _ = crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.keys",
        json!({"instanceId": wire, "keys": ["enter"], "origin": "human"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let writes: Vec<Value> = harness
        .calls()
        .into_iter()
        .filter(|call| {
            matches!(
                call["method"].as_str(),
                Some("pane.send_text" | "pane.send_keys" | "agent.prompt" | "terminal.input")
            )
        })
        .collect();
    assert!(
        writes.is_empty(),
        "a write reached the replaced program: {writes:?}"
    );
    other.kill().expect("cleanup");
    let _ = other.wait();
}

#[tokio::test]
async fn the_agent_leaving_the_foreground_ends_the_claim() {
    let harness = Harness::new().await;
    let mut agent = sleeper();
    set_process(&harness, "w1:p2", agent.id(), "codex", None).await;
    let id = claim_codex_on_p2(&harness).await.meta.id;
    // The pane is back at its shell prompt (no foreground agent at all).
    harness
        .client()
        .call_raw("fake.clear_pane_process", json!({"pane_id": "w1:p2"}))
        .await
        .expect("back to the shell");
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    assert!(harness.journal(&id).contains(REASON_PROCESS_GONE));
    agent.kill().expect("cleanup");
    let _ = agent.wait();
}

#[tokio::test]
async fn a_reused_pid_with_a_different_start_time_ends_the_claim() {
    let harness = Harness::new().await;
    let mut agent = sleeper();
    set_process(&harness, "w1:p2", agent.id(), "codex", None).await;
    let id = claim_codex_on_p2(&harness).await.meta.id;
    harness.node.shutdown().await.expect("node shutdown");
    // The same pid now names a process that started at another time.
    let mut row = harness.store.get_instance(&id).expect("row");
    let process = row
        .herdr_claim
        .as_mut()
        .and_then(|claim| claim.process.as_mut())
        .expect("process");
    process.start_ticks = process.start_ticks.map(|ticks| U64(ticks.0 + 1));
    harness.store.remove_instance(&id).expect("remove");
    harness.store.insert_instance(row).expect("reinsert");
    let own_dir = harness.root.path().join("sessions/mine");
    let restarted = node(&harness.store, harness.data.path(), &own_dir);
    restarted.set_herdr_claim_root(harness.root.path().to_path_buf());
    restarted
        .reconcile_herdr()
        .await
        .expect("startup reconcile");
    assert_eq!(
        harness.store.get_instance(&id).expect("row").lifecycle,
        InstanceLifecycle::Exited
    );
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PROCESS_GONE), "{journal}");
    assert!(journal.contains("was reused"), "{journal}");
    agent.kill().expect("cleanup");
    let _ = agent.wait();
}

#[tokio::test]
async fn a_shell_claim_survives_a_foreground_job_but_not_the_shell_replaced() {
    let harness = Harness::new().await;
    let mut shell = sleeper();
    let mut job = sleeper();
    set_process(&harness, "w1:p3", job.id(), "vim", Some(shell.id())).await;
    let claimed = harness
        .claim(HerdrClaimTarget {
            kind: Some("shell".into()),
            ..harness.target("w1", "w1:t2", Some("w1:p3"))
        })
        .await
        .expect("claim");
    let id = claimed.meta.id.clone();
    let recorded = claimed
        .herdr_claim
        .and_then(|claim| claim.process)
        .expect("process");
    assert_eq!(recorded.pid, shell.id(), "a shell claim watches the shell");
    // Another foreground job is fine.
    let mut other_job = sleeper();
    set_process(&harness, "w1:p3", other_job.id(), "top", Some(shell.id())).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        harness.store.get_instance(&id).expect("row").lifecycle,
        InstanceLifecycle::Ready,
        "a different foreground job does not end a shell claim"
    );
    // The shell itself is replaced.
    let mut new_shell = sleeper();
    set_process(
        &harness,
        "w1:p3",
        other_job.id(),
        "top",
        Some(new_shell.id()),
    )
    .await;
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    assert!(harness.journal(&id).contains("is no longer the shell"));
    for child in [&mut shell, &mut job, &mut other_job, &mut new_shell] {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[tokio::test]
async fn an_unreachable_server_is_contact_loss_never_an_end() {
    let harness = Harness::new().await;
    let claimed = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim");
    let id = claimed.meta.id.clone();
    // Move the socket path away: the server lives on, but nobody can reach it.
    let away = harness.socket.with_extension("away");
    std::fs::rename(&harness.socket, &away).expect("hide socket");
    harness.wait_contact(&id, &["lost"]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let instance = harness.store.get_instance(&id).expect("instance");
    assert_eq!(
        instance.lifecycle,
        InstanceLifecycle::Ready,
        "contact loss never ends or fails the instance"
    );
    assert!(!harness.journal(&id).contains(REASON_PANE_GONE));
    std::fs::rename(&away, &harness.socket).expect("restore socket");
    harness.wait_contact(&id, &["lost", "ok"]).await;
    assert_eq!(
        harness.store.get_instance(&id).expect("instance").lifecycle,
        InstanceLifecycle::Ready
    );
}

#[tokio::test]
async fn a_node_restart_reattaches_live_claims_and_ends_gone_ones() {
    let harness = Harness::new().await;
    let kept = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim kept pane")
        .meta
        .id;
    let lost = harness
        .claim(harness.target("w2", "w2:t3", None))
        .await
        .expect("claim pane closed while down")
        .meta
        .id;
    harness.node.shutdown().await.expect("node shutdown");
    for id in [&kept, &lost] {
        assert_eq!(
            harness.store.get_instance(id).expect("instance").lifecycle,
            InstanceLifecycle::Ready,
            "a Node shutdown is not a release"
        );
    }
    harness.clear_calls();
    harness
        .client()
        .pane_close("w2:p4")
        .await
        .expect("closed while the Node is down");

    let own_dir = harness.root.path().join("sessions/mine");
    let restarted = node(&harness.store, harness.data.path(), &own_dir);
    restarted.set_herdr_claim_root(harness.root.path().to_path_buf());
    restarted
        .reconcile_herdr()
        .await
        .expect("startup reconcile");

    assert_eq!(
        harness.store.get_instance(&kept).expect("kept").lifecycle,
        InstanceLifecycle::Ready
    );
    assert!(
        restarted
            .inner
            .instance_drivers
            .read()
            .await
            .contains_key(&kept),
        "the live claim is re-attached"
    );
    assert!(
        restarted.tty().attach(&kept).await.is_ok(),
        "with its TTY bridge"
    );
    let lost_row = harness.store.get_instance(&lost).expect("lost");
    assert_eq!(lost_row.lifecycle, InstanceLifecycle::Exited);
    assert!(harness.journal(&lost).contains(REASON_PANE_GONE));
    let closes = harness
        .calls()
        .into_iter()
        .filter(|call| call["method"] == "pane.close")
        .count();
    assert_eq!(closes, 1, "only the human's close: {:?}", harness.calls());
    assert!(
        harness
            .client()
            .session_snapshot()
            .await
            .expect("snapshot")
            .panes
            .iter()
            .any(|pane| pane.pane_id == "w1:p1"),
        "the orphan sweep never reached the external session"
    );
}

#[test]
fn instance_create_refuses_the_claimed_driver() {
    let error = crate::runtime::validate_kind_driver(AgentKind::Claude, DriverKind::HerdrClaimed)
        .expect_err("create refuses herdr-claimed");
    assert!(error.to_string().contains("instance.claim"), "{error}");
}

#[test]
fn claim_errors_never_read_as_loss_of_the_nodes_own_carrier() {
    let error = pane_error(
        "pane.send_keys",
        &remuda_herdr::Error::Api {
            method: "pane.send_keys".into(),
            code: remuda_herdr::SERVER_UNAVAILABLE.into(),
            message: "server is shutting down".into(),
        },
    );
    assert!(
        !crate::carrier_recovery::is_carrier_loss(&error),
        "a claimed pane's server is not the Node's carrier: {error}"
    );
}

#[tokio::test]
async fn the_node_refuses_a_non_human_write_to_a_claimed_pane() {
    let harness = Harness::new().await;
    let claimed = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim");
    let id = claimed.meta.id.as_id().to_string();
    harness.wait_call("terminal.control", "w1:p1").await;
    harness.clear_calls();
    for (method, params) in [
        (
            "instance.keys",
            json!({"instanceId": id, "keys": ["esc"], "origin": "bot"}),
        ),
        (
            "tty.write",
            json!({"instanceId": id, "dataBase64": "eA==", "origin": "agent"}),
        ),
        (
            "instance.send",
            json!({"instanceId": id, "input": {"type": "prompt", "text": "x"}, "origin": "hub"}),
        ),
        ("instance.close", json!({"instanceId": id, "origin": "bot"})),
        // D-062 r3 item 4: a frame with no origin is refused too.
        ("instance.keys", json!({"instanceId": id, "keys": ["esc"]})),
        ("tty.write", json!({"instanceId": id, "dataBase64": "eA=="})),
        (
            "instance.send",
            json!({"instanceId": id, "input": {"type": "prompt", "text": "x"}}),
        ),
        ("instance.close", json!({"instanceId": id})),
    ] {
        let error = crate::transport::hubnode::dispatch_method(&harness.node, method, params)
            .await
            .expect_err("a non-human write is refused");
        assert!(
            error.to_string().contains("human-only"),
            "{method}: {error}"
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let writes: Vec<Value> = harness
        .calls()
        .into_iter()
        .filter(|call| {
            !matches!(
                call["method"].as_str(),
                Some("session.snapshot" | "pane.process_info" | "agent.get" | "pane.read")
            )
        })
        .collect();
    assert!(writes.is_empty(), "nothing reached the pane: {writes:?}");
    assert_eq!(
        harness
            .store
            .get_instance(&claimed.meta.id)
            .expect("row")
            .lifecycle,
        InstanceLifecycle::Ready
    );
    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.keys",
        json!({"instanceId": id, "keys": ["esc"], "origin": "human"}),
    )
    .await
    .expect("a human write passes");
    harness.wait_call("terminal.input", "w1:p1").await;
}

#[tokio::test]
async fn a_lost_contact_flips_back_to_ok_after_a_node_restart() {
    let harness = Harness::new().await;
    let id = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim")
        .meta
        .id;
    let away = harness.socket.with_extension("away");
    std::fs::rename(&harness.socket, &away).expect("hide socket");
    harness.wait_contact(&id, &["lost"]).await;
    harness.node.shutdown().await.expect("node shutdown");
    // The server answers again while the Node is down.
    std::fs::rename(&away, &harness.socket).expect("restore socket");
    let own_dir = harness.root.path().join("sessions/mine");
    let restarted = node(&harness.store, harness.data.path(), &own_dir);
    restarted.set_herdr_claim_root(harness.root.path().to_path_buf());
    restarted
        .reconcile_herdr()
        .await
        .expect("startup reconcile");
    harness.wait_contact(&id, &["lost", "ok"]).await;
    assert_eq!(
        harness.store.get_instance(&id).expect("row").lifecycle,
        InstanceLifecycle::Ready
    );
}

#[tokio::test]
async fn a_claim_files_under_the_registered_workspace_holding_the_pane() {
    let harness = Harness::new().await;
    let registered = harness.node.workspaces().expect("workspaces");
    let root = registered.first().expect("a registered workspace").clone();
    let tab = harness
        .client()
        .tab_create(TabCreateParams {
            workspace_id: Some("w2".into()),
            cwd: Some(root.root_path.clone()),
            ..Default::default()
        })
        .await
        .expect("tab in the workspace");
    let target = HerdrClaimTarget {
        kind: Some("shell".into()),
        ..harness.target("w2", &tab.tab.tab_id, None)
    };
    let resolution = harness
        .node
        .herdr_resolve(&target, None)
        .await
        .expect("resolve");
    assert_eq!(
        resolution.remuda_workspace_id.as_deref(),
        Some(root.meta.id.as_id().as_str())
    );
    let claimed = harness.claim(target).await.expect("claim");
    assert_eq!(claimed.workspace_id, root.meta.id);
    // A pane outside every registered workspace reports none.
    let outside = harness
        .node
        .herdr_resolve(
            &HerdrClaimTarget {
                kind: Some("shell".into()),
                ..harness.target("w2", "w2:t3", None)
            },
            None,
        )
        .await
        .expect("resolve");
    assert_eq!(outside.remuda_workspace_id, None);
}

/// Calls in the log that write to a pane.
fn pane_writes(harness: &Harness) -> Vec<Value> {
    harness
        .calls()
        .into_iter()
        .filter(|call| {
            matches!(
                call["method"].as_str(),
                Some("pane.send_text" | "pane.send_keys" | "agent.prompt" | "terminal.input")
            )
        })
        .collect()
}

/// Wait until the watcher's first probe has read the pane's process (the
/// claim's own resolve read it once before).
async fn wait_first_probe(harness: &Harness, pane: &str) {
    let start = Instant::now();
    while harness
        .calls()
        .iter()
        .filter(|call| call["method"] == "pane.process_info" && touches(call, pane))
        .count()
        < 2
    {
        assert!(start.elapsed() < WAIT, "the watcher never probed {pane}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_write_right_after_the_process_is_replaced_is_dropped_and_ends_the_claim() {
    let harness = Harness::new().await;
    // No probe runs between the replacement and the write: the watcher
    // probes once at claim time, then sleeps for an hour.
    harness.node.set_herdr_claim_poll(Duration::from_secs(3600));
    for write in ["send", "keys", "tty"] {
        let mut agent = sleeper();
        set_process(&harness, "w1:p2", agent.id(), "codex", None).await;
        harness.clear_calls();
        let id = claim_codex_on_p2(&harness).await.meta.id;
        harness.wait_call("terminal.control", "w1:p2").await;
        wait_first_probe(&harness, "w1:p2").await;
        // A shell takes the pane's foreground, and at once a write arrives.
        let mut shell = sleeper();
        set_process(&harness, "w1:p2", shell.id(), "bash", None).await;
        harness.clear_calls();
        let wire = id.as_id().to_string();
        let (method, params) = match write {
            "send" => (
                "instance.send",
                json!({"instanceId": wire, "input": {"type": "prompt", "text": "rm -rf x"}, "origin": "human"}),
            ),
            "keys" => (
                "instance.keys",
                json!({"instanceId": wire, "keys": ["enter"], "origin": "human"}),
            ),
            _ => (
                "tty.write",
                json!({"instanceId": wire, "dataBase64": "eA0=", "origin": "human"}),
            ),
        };
        let _ = crate::transport::hubnode::dispatch_method(&harness.node, method, params).await;
        harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
        let journal = harness.journal(&id);
        assert!(journal.contains(REASON_PROCESS_GONE), "{write}: {journal}");
        assert!(
            journal.contains("is no longer the foreground agent"),
            "{write}: {journal}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            pane_writes(&harness).is_empty(),
            "{write}: a write reached the replacing process: {:?}",
            harness.calls()
        );
        assert!(
            !harness
                .calls()
                .iter()
                .any(|call| call["method"] == "session.snapshot"),
            "{write}: the write's own check ended the claim, not a probe: {:?}",
            harness.calls()
        );
        for child in [&mut agent, &mut shell] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tokio::test]
async fn a_pane_whose_process_info_fails_refuses_every_write() {
    let harness = Harness::new().await;
    harness.node.set_herdr_claim_poll(Duration::from_secs(3600));
    let id = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim")
        .meta
        .id;
    harness.wait_call("terminal.control", "w1:p1").await;
    wait_first_probe(&harness, "w1:p1").await;
    // The server answers, but cannot say what runs in the pane.
    harness
        .client()
        .call_raw("fake.fail_pane_process", json!({"pane_id": "w1:p1"}))
        .await
        .expect("process info fails");
    harness.clear_calls();
    let wire = id.as_id().to_string();
    let refused = crate::transport::hubnode::dispatch_method(
        &harness.node,
        "tty.write",
        json!({"instanceId": wire, "dataBase64": "eA0=", "origin": "human"}),
    )
    .await
    .expect_err("an unknown process takes no TTY input");
    assert!(refused.to_string().contains("refused"), "{refused}");
    for (method, params) in [
        (
            "instance.send",
            json!({"instanceId": wire, "input": {"type": "prompt", "text": "x"}, "origin": "human"}),
        ),
        (
            "instance.keys",
            json!({"instanceId": wire, "keys": ["esc"], "origin": "human"}),
        ),
    ] {
        let _ = crate::transport::hubnode::dispatch_method(&harness.node, method, params).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        pane_writes(&harness).is_empty(),
        "a write reached a pane whose process is unknown: {:?}",
        harness.calls()
    );
    assert_eq!(
        harness.store.get_instance(&id).expect("row").lifecycle,
        InstanceLifecycle::Ready,
        "one refused write is not the end; the probes decide"
    );
}

#[tokio::test]
async fn unknown_process_info_ends_the_claim_after_consecutive_probes() {
    for none in [false, true] {
        let harness = Harness::new().await;
        let id = harness
            .claim(harness.target("w1", "w1:t1", None))
            .await
            .expect("claim")
            .meta
            .id;
        harness
            .client()
            .call_raw(
                "fake.fail_pane_process",
                json!({"pane_id": "w1:p1", "none": none}),
            )
            .await
            .expect("process info fails");
        harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
        let journal = harness.journal(&id);
        assert!(journal.contains(REASON_PROCESS_UNKNOWN), "{journal}");
        assert!(
            journal.contains(&format!("in {UNKNOWN_PROBES_TO_END} probes in a row")),
            "{journal}"
        );
        assert!(
            !journal.contains(REASON_PANE_GONE),
            "the pane is there: {journal}"
        );
        assert!(harness.destructive_calls().is_empty());
    }
}

#[tokio::test]
async fn a_claim_row_with_no_recorded_process_ends_at_reattach() {
    let harness = Harness::new().await;
    let id = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim")
        .meta
        .id;
    harness.wait_call("terminal.control", "w1:p1").await;
    harness.node.shutdown().await.expect("node shutdown");
    // A row claimed before the process identity was recorded (round 1).
    let mut row = harness.store.get_instance(&id).expect("row");
    row.herdr_claim.as_mut().expect("claim").process = None;
    harness.store.remove_instance(&id).expect("remove");
    harness.store.insert_instance(row).expect("reinsert");
    harness.clear_calls();
    let own_dir = harness.root.path().join("sessions/mine");
    let restarted = node(&harness.store, harness.data.path(), &own_dir);
    restarted.set_herdr_claim_root(harness.root.path().to_path_buf());
    restarted
        .reconcile_herdr()
        .await
        .expect("startup reconcile");
    assert_eq!(
        harness.store.get_instance(&id).expect("row").lifecycle,
        InstanceLifecycle::Exited
    );
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PROCESS_UNKNOWN), "{journal}");
    assert!(
        journal.contains("recorded no process identity"),
        "{journal}"
    );
    assert!(
        !restarted
            .inner
            .instance_drivers
            .read()
            .await
            .contains_key(&id),
        "nothing to verify, so nothing is re-attached"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !harness
            .calls()
            .iter()
            .any(|call| call["method"] == "terminal.control"),
        "no bridge is opened on it: {:?}",
        harness.calls()
    );
    assert!(pane_writes(&harness).is_empty());
    assert!(harness.destructive_calls().is_empty());
}

#[tokio::test]
async fn a_process_replaced_after_the_text_gets_no_enter() {
    let harness = Harness::new().await;
    harness.node.set_herdr_claim_poll(Duration::from_secs(3600));
    let agent = sleeper();
    set_process(&harness, "w1:p2", agent.id(), "codex", None).await;
    let id = claim_codex_on_p2(&harness).await.meta.id;
    harness.wait_call("terminal.control", "w1:p2").await;
    wait_first_probe(&harness, "w1:p2").await;
    // The agent exits and a shell takes the pane exactly while the prompt's
    // text is typed: the text has gone, its Enter must not follow.
    let shell = sleeper();
    harness
        .client()
        .call_raw(
            "fake.replace_process_on_text",
            json!({"pane_id": "w1:p2", "pid": shell.id(), "name": "bash"}),
        )
        .await
        .expect("arm the replacement");
    harness.clear_calls();
    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.send",
        json!({"instanceId": id.as_id().as_str(), "input": {"type": "prompt", "text": "rm -rf x"},
               "origin": "human"}),
    )
    .await
    .expect("send accepted");
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PROCESS_GONE), "{journal}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let methods: Vec<String> = harness
        .calls()
        .iter()
        .filter_map(|call| call["method"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "pane.send_text")
            .count(),
        1,
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|method| matches!(
            method.as_str(),
            "pane.send_keys" | "terminal.input" | "agent.prompt"
        )),
        "the Enter reached the replacing process: {methods:?}"
    );
    drop((agent, shell));
}

#[tokio::test]
async fn a_claim_row_with_no_recorded_process_ends_even_with_its_server_unreachable() {
    let harness = Harness::new().await;
    let id = harness
        .claim(harness.target("w1", "w1:t1", None))
        .await
        .expect("claim")
        .meta
        .id;
    harness.wait_call("terminal.control", "w1:p1").await;
    harness.node.shutdown().await.expect("node shutdown");
    let mut row = harness.store.get_instance(&id).expect("row");
    row.herdr_claim.as_mut().expect("claim").process = None;
    harness.store.remove_instance(&id).expect("remove");
    harness.store.insert_instance(row).expect("reinsert");
    // The server is unreachable when the Node starts.
    let away = harness.socket.with_extension("away");
    std::fs::rename(&harness.socket, &away).expect("hide socket");
    let own_dir = harness.root.path().join("sessions/mine");
    let restarted = node(&harness.store, harness.data.path(), &own_dir);
    restarted.set_herdr_claim_root(harness.root.path().to_path_buf());
    restarted
        .reconcile_herdr()
        .await
        .expect("startup reconcile");
    assert_eq!(
        harness.store.get_instance(&id).expect("row").lifecycle,
        InstanceLifecycle::Exited,
        "ended, not re-attached in the contact-lost state"
    );
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PROCESS_UNKNOWN), "{journal}");
    assert!(
        !restarted
            .inner
            .instance_drivers
            .read()
            .await
            .contains_key(&id),
        "nothing is re-attached"
    );
    assert!(restarted.tty().attach(&id).await.is_err(), "no bridge");
    std::fs::rename(&away, &harness.socket).expect("restore socket");
}
