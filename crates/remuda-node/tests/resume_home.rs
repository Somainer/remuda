//! c-resumehome: a resume launches in a NEW instance with a fresh native
//! config home, but `claude --resume <id>` only finds the conversation under
//! `<home>/projects/<encoded cwd>/<id>.jsonl`. The Node must stage the
//! predecessor transcript (and its sidecars) into the new home before launch.
//!
//! The fake `claude` enforces the real lookup: `--resume` dies with
//! "No conversation found with session ID" when the file is absent, so a
//! green test here is the transcript being found, not our own assertion that
//! we copied a file.
//!
//! Each launch pins its own `claudeConfigDir` (a managed home), matching the
//! production shape where gateway-delegated instances each own
//! `<data>/instances/<id>/native-home`.

#![cfg(unix)]

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig,
    compose,
};
use remuda_protocol::{AgentKind, CommandState, DriverKind, InstanceId, Knowledge};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn serve_config(workspace: &Path, data: &Path, binary: &Path) -> ServeConfig {
    let native =
        NativeDriverConfig::new(data.to_path_buf()).with_claude_binary(binary.to_path_buf());
    let mut http = DevServerConfig::loopback(0).with_workspace_root(workspace.to_path_buf());
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    ServeConfig {
        http,
        data_dir: data.to_path_buf(),
        drivers: LocalDrivers::Native(native),
    }
}

/// Install a 0755 fake claude outside the workspace cwd: the binary override
/// guard rejects group-writable target-dir files and anything inside the cwd.
fn install_fake_claude(root: &Path) -> PathBuf {
    let install = root.join("opt/bin");
    std::fs::create_dir_all(&install).expect("bin dir");
    let source = remuda_testing::ensure_workspace_bin("fake-claude");
    let dest = install.join("claude");
    std::fs::copy(&source, &dest).expect("copy fake claude");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dest
}

fn request(
    driver: DriverKind,
    prompt: &str,
    resume: Option<(&InstanceId, &str)>,
    binary: &Path,
    config_dir: &Path,
) -> CreateInstanceRequest {
    CreateInstanceRequest {
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Claude,
        driver,
        model: "fake".to_owned(),
        args: Vec::new(),
        binary_path: Some(binary.to_string_lossy().into_owned()),
        binary_sha256: None,
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: if driver == DriverKind::ClaudeSdk {
            "default".to_owned()
        } else {
            "dontAsk".to_owned()
        },
        sandbox: None,
        prompt: prompt.to_owned(),
        cwd: None,
        delegation: None,
        settings_overlay_path: None,
        claude_config_dir: Some(config_dir.to_string_lossy().into_owned()),
        max_budget_usd: None,
        provider_overlay: None,
        provider_auth_token: None,
        resume_session_id: resume.map(|(_, session)| session.to_owned()),
        resumed_from: resume.map(|(parent, _)| (*parent).clone()),
        effort: None,
        tui: None,
        extra_env: std::collections::BTreeMap::new(),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}

async fn wait_settled(node: &DevNode, command_id: &remuda_protocol::CommandId) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if node.get_command(command_id).expect("command").state == CommandState::Settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("command settles (fake claude answers)");
}

/// Wait until the transcript on disk contains `needle`.
///
/// Command settlement on stream-json carriers means "written to stdin"; the
/// fake appends its records only after reading and playing the turn, so file
/// assertions keyed on settlement race the process scheduler. The transcript
/// content is the honest gate.
async fn wait_contains(path: &Path, needle: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if std::fs::read_to_string(path)
                .unwrap_or_default()
                .contains(needle)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{}: {needle:?} never written", path.display()));
}

/// `<home>/projects/<encoded cwd>/<session>.jsonl`, the exact layout
/// `claude --resume` reads.
fn transcript_in(home: &Path, workspace: &Path, session: &str) -> PathBuf {
    remuda_driver::claude_transcript::project_dir(home, workspace).join(format!("{session}.jsonl"))
}

/// The native session id the driver reported for an instance after start.
async fn recorded_session(node: &DevNode, id: &InstanceId) -> String {
    // The session lifecycle that corrects the create-time placeholder lands
    // just after command settlement; wait for the driver's own id, not the
    // `ins_…` placeholder.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = match node
                .get_instance(id)
                .expect("instance")
                .native_ref
                .session_id
            {
                Knowledge::Known { value } => value,
                other => panic!("expected a recorded native session id, got {other:?}"),
            };
            if !value.starts_with("ins_") {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let instance = node.get_instance(id).expect("instance");
        panic!(
            "session not recorded; lifecycle={:?} last_error={:?}",
            instance.lifecycle, instance.last_error
        )
    })
}

struct ChainHarness {
    _root_dir: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    binary: PathBuf,
    node: DevNode,
    home_seq: AtomicU64,
}

impl ChainHarness {
    fn new() -> Self {
        let root_dir = tempfile::tempdir().expect("tempdir");
        let root = root_dir.path().to_path_buf();
        let data = root.join("data");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let binary = install_fake_claude(&root);
        let config = serve_config(&workspace, &data, &binary);
        let node = compose(&config).expect("compose");
        Self {
            _root_dir: root_dir,
            root,
            workspace,
            binary,
            node,
            home_seq: AtomicU64::new(0),
        }
    }

    fn fresh_home(&self) -> PathBuf {
        let seq = self.home_seq.fetch_add(1, Ordering::Relaxed);
        let home = self.root.join(format!("homes/gen-{seq}"));
        std::fs::create_dir_all(&home).expect("home");
        home
    }

    async fn run(&self, driver: DriverKind) {
        // Generation 0: a normal managed-home run. The fake writes the
        // transcript into the instance's own native home, like the real CLI.
        let parent_home = self.fresh_home();
        let parent = self
            .node
            .create_instance(request(
                driver,
                "first turn",
                None,
                &self.binary,
                &parent_home,
            ))
            .await
            .expect("parent create");
        wait_settled(&self.node, &parent.command.command_id).await;
        let parent_id = parent.instance.meta.id.clone();
        // A fresh launch mints its own native session id (`--session-id`);
        // the Node records the id the driver reports from init. That recorded
        // id — not a constant — is what `--resume` must continue.
        let session = recorded_session(&self.node, &parent_id).await;
        let parent_transcript = transcript_in(&parent_home, &self.workspace, &session);
        assert!(
            parent_transcript.is_file(),
            "parent transcript lives only in the OLD native home: {}",
            parent_transcript.display()
        );
        // Command settlement means "written to the child's stdin", not "the
        // turn is on disk" — wait for the transcript itself so the staged copy
        // below carries the complete conversation.
        wait_contains(&parent_transcript, "first turn").await;
        let parent_bytes = std::fs::read(&parent_transcript).expect("read parent");

        // Sidecars the predecessor home also holds: subagent transcripts and
        // project memory. These must arrive in the child home too.
        let project_dir = parent_transcript.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(project_dir.join(&session).join("subagents")).expect("mkdir");
        std::fs::write(
            project_dir.join(&session).join("subagents/side.jsonl"),
            b"{\"side\":true}\n",
        )
        .expect("sidecar");
        std::fs::create_dir_all(project_dir.join("memory")).expect("memory mkdir");
        std::fs::write(project_dir.join("memory/MEMORY.md"), b"project memory\n").expect("memory");

        // Generation 1: resume into a fresh instance/home. Before the fix the
        // fake died immediately: its new native home had no projects/ dir.
        let child1_home = self.fresh_home();
        let child1 = self
            .node
            .create_instance(request(
                driver,
                "second turn",
                Some((&parent_id, session.as_str())),
                &self.binary,
                &child1_home,
            ))
            .await
            .expect("resume is accepted");
        assert_eq!(
            child1
                .instance
                .parent
                .as_ref()
                .map(|link| &link.instance_id),
            Some(&parent_id),
            "the child records where its conversation came from"
        );
        wait_settled(&self.node, &child1.command.command_id).await;
        let child1_id = child1.instance.meta.id.clone();
        let child1_transcript = transcript_in(&child1_home, &self.workspace, &session);
        assert!(
            child1_transcript.is_file(),
            "staged transcript in the child home: {}",
            child1_transcript.display()
        );
        let child1_dir = child1_transcript.parent().unwrap();
        assert!(
            child1_dir
                .join(session.as_str())
                .join("subagents/side.jsonl")
                .is_file(),
            "subagent sidecar staged"
        );
        assert!(
            child1_dir.join("memory/MEMORY.md").is_file(),
            "project memory staged"
        );
        // The resumed turn lands in the child's copy; gate on the prompt text,
        // not on command settlement (which is a stdin-write ack).
        wait_contains(&child1_transcript, "second turn").await;
        let gen1 = std::fs::read(&child1_transcript).expect("read child1");
        assert!(
            gen1.len() > parent_bytes.len(),
            "the resumed turn appends in the child home"
        );
        assert_eq!(
            std::fs::read(&parent_transcript).expect("reread parent"),
            parent_bytes,
            "the predecessor transcript stays frozen"
        );

        // Generation 2: resume of a resume. The newest generation's transcript
        // (generation 1), not the original's, is what gets staged.
        let child2_home = self.fresh_home();
        let child2 = self
            .node
            .create_instance(request(
                driver,
                "third turn",
                Some((&child1_id, session.as_str())),
                &self.binary,
                &child2_home,
            ))
            .await
            .expect("second-generation resume is accepted");
        wait_settled(&self.node, &child2.command.command_id).await;
        let child2_transcript = transcript_in(&child2_home, &self.workspace, &session);
        assert!(child2_transcript.is_file(), "grandchild transcript staged");
        wait_contains(&child2_transcript, "third turn").await;
        let gen2 = std::fs::read(&child2_transcript).expect("read child2");
        assert!(
            gen2.len() >= gen1.len() && gen2.len() > parent_bytes.len(),
            "the grandchild continues from generation 1's transcript"
        );
        assert_eq!(
            std::fs::read(&child1_transcript).expect("reread child1"),
            gen1,
            "generation 1 is untouched by generation 2"
        );
        assert_eq!(
            std::fs::read(&parent_transcript).expect("reread parent 2"),
            parent_bytes,
            "the original instance stays untouched across the chain"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_resume_stages_the_predecessor_transcript_print_and_sdk() {
    ChainHarness::new().run(DriverKind::ClaudePrint).await;
    ChainHarness::new().run(DriverKind::ClaudeSdk).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_without_a_transcript_is_refused_with_a_clear_message() {
    let harness = ChainHarness::new();
    let parent_home = harness.fresh_home();
    let parent = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "first turn",
            None,
            &harness.binary,
            &parent_home,
        ))
        .await
        .expect("parent create");
    wait_settled(&harness.node, &parent.command.command_id).await;
    let parent_id = parent.instance.meta.id.clone();
    let session = recorded_session(&harness.node, &parent_id).await;
    let parent_transcript = transcript_in(&parent_home, &harness.workspace, &session);
    assert!(parent_transcript.is_file());

    // Prune the conversation the same way Claude's retention would.
    std::fs::remove_file(&parent_transcript).expect("prune transcript");

    let child_home = harness.fresh_home();
    let instances_before = harness.node.list_instances().expect("list").items.len();
    let error = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "continue",
            Some((&parent_id, session.as_str())),
            &harness.binary,
            &child_home,
        ))
        .await
        .expect_err("a missing transcript must be refused, not accepted");
    let message = error.to_string();
    assert!(
        message.contains("predecessor transcript not found"),
        "clear refusal naming the cause: {message}"
    );
    assert!(
        message.contains(&session),
        "the refusal names the session id: {message}"
    );
    assert_eq!(
        harness
            .node
            .list_instances()
            .expect("list after")
            .items
            .len(),
        instances_before,
        "a refused resume creates no instance row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_child_carries_the_real_native_session_identity() {
    let harness = ChainHarness::new();
    let parent_home = harness.fresh_home();
    let parent = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "first",
            None,
            &harness.binary,
            &parent_home,
        ))
        .await
        .expect("parent");
    wait_settled(&harness.node, &parent.command.command_id).await;
    let parent_id = parent.instance.meta.id.clone();
    let session = recorded_session(&harness.node, &parent_id).await;

    let child_home = harness.fresh_home();
    let child = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "second",
            Some((&parent_id, session.as_str())),
            &harness.binary,
            &child_home,
        ))
        .await
        .expect("resume");
    wait_settled(&harness.node, &child.command.command_id).await;

    // The resumed instance is seeded with the continued session id at create,
    // and the driver's own init confirms it.
    assert!(matches!(
        child.instance.native_ref.session_id,
        Knowledge::Known { ref value } if value == &session
    ));
    let observed = harness
        .node
        .get_instance(&child.instance.meta.id)
        .expect("child projection");
    assert!(
        matches!(observed.native_ref.session_id, Knowledge::Known { ref value } if value == &session),
        "driver-reported identity stays the resumed id"
    );
}

/// A promoted session's transcript can live OUTSIDE any Remuda-managed native
/// home (the shell-pty hook reports an absolute `nativeTranscriptPath`). The
/// resume resolver must follow that recorded path verbatim instead of guessing
/// one from the launch recipe's home.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_uses_the_recorded_transcript_path_even_outside_the_native_home() {
    use remuda_node::{LocalStore, MemoryStore, native_driver_registry};
    use std::sync::Arc;

    let root_dir = tempfile::tempdir().expect("tempdir");
    let root = root_dir.path().to_path_buf();
    let data = root.join("data");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let binary = install_fake_claude(&root);

    let store: Arc<MemoryStore> = Arc::new(MemoryStore::new(256));
    let native = NativeDriverConfig::new(data.clone()).with_claude_binary(binary.clone());
    let registry = native_driver_registry(native).expect("native registry");
    let node = DevNode::with_parts(
        &serve_config(&workspace, &data, &binary).http,
        store.clone(),
        registry,
    )
    .expect("node");

    let parent_home = root.join("homes/gen-0");
    std::fs::create_dir_all(&parent_home).expect("home0");
    let parent = node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "first turn",
            None,
            &binary,
            &parent_home,
        ))
        .await
        .expect("parent create");
    wait_settled(&node, &parent.command.command_id).await;
    let parent_id = parent.instance.meta.id.clone();
    let session = recorded_session(&node, &parent_id).await;
    let original = transcript_in(&parent_home, &workspace, &session);
    assert!(original.is_file());

    // The promoted hook evidence: transcript now lives at an arbitrary
    // external location. Delete the home copy so a home-derived guess cannot
    // succeed — only the recorded path can.
    let external = root.join("elsewhere/promoted-conversation.jsonl");
    std::fs::create_dir_all(external.parent().unwrap()).expect("external dir");
    std::fs::copy(&original, &external).expect("stage external copy");
    std::fs::remove_file(&original).expect("remove the home copy");
    store
        .set_native_session(&parent_id, &session, Some(external.to_str().unwrap()), None)
        .expect("record external transcript path");

    let child_home = root.join("homes/gen-1");
    std::fs::create_dir_all(&child_home).expect("home1");
    let child = node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "second turn",
            Some((&parent_id, session.as_str())),
            &binary,
            &child_home,
        ))
        .await
        .expect("resume accepted from the recorded external path");
    wait_settled(&node, &child.command.command_id).await;
    let staged = transcript_in(&child_home, &workspace, &session);
    assert!(
        staged.is_file(),
        "the external recorded transcript was staged into the child home"
    );
}
