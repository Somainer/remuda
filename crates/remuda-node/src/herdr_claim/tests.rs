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
        (4242, "fake-herdr")
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
    // The TTY bridge is a control session on the claimed pane, opened at claim.
    harness.wait_call("terminal.control", "w1:p1").await;

    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.send",
        json!({"instanceId": wire_id, "input": {"type": "prompt", "text": "hello pane"}}),
    )
    .await
    .expect("send");
    harness.wait_call("agent.prompt", "w1:p1").await;

    crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.keys",
        json!({"instanceId": wire_id, "keys": ["esc"]}),
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
        json!({"instanceId": id.as_id().as_str()}),
    )
    .await
    .expect("release");
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    harness.wait_journal(&id, REASON_RELEASED).await;
    // A second release is harmless and still touches nothing.
    let _ = crate::transport::hubnode::dispatch_method(
        &harness.node,
        "instance.close",
        json!({"instanceId": id.as_id().as_str()}),
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

#[tokio::test]
async fn process_gone_needs_both_herdr_and_the_kernel() {
    let harness = Harness::new().await;
    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("stand-in agent process");
    let pid = child.id();
    harness
        .client()
        .call_raw(
            "fake.set_pane_process",
            json!({"pane_id": "w1:p2", "pid": pid, "name": "codex"}),
        )
        .await
        .expect("pane runs the child");
    let claimed = harness
        .claim(HerdrClaimTarget {
            kind: Some("codex".into()),
            ..harness.target("w1", "w1:t2", Some("w1:p2"))
        })
        .await
        .expect("claim");
    let id = claimed.meta.id.clone();
    let process = claimed
        .herdr_claim
        .and_then(|claim| claim.process)
        .expect("process");
    assert_eq!((process.pid, process.command.as_str()), (pid, "codex"));

    // herdr stops listing the process while it is alive (it handed the
    // terminal to a child): the kernel says alive, so the claim stays live.
    harness
        .client()
        .call_raw("fake.clear_pane_process", json!({"pane_id": "w1:p2"}))
        .await
        .expect("foreground moves");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        harness.store.get_instance(&id).expect("instance").lifecycle,
        InstanceLifecycle::Ready,
        "absent from the foreground list alone is not process end"
    );

    child.kill().expect("process exits");
    child.wait().expect("reaped");
    harness.wait_lifecycle(&id, InstanceLifecycle::Exited).await;
    let journal = harness.journal(&id);
    assert!(journal.contains(REASON_PROCESS_GONE), "{journal}");
    assert!(
        harness.destructive_calls().is_empty(),
        "{:?}",
        harness.calls()
    );
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
