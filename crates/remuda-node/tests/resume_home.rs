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
    let external_dir = root.join("elsewhere");
    let external = external_dir.join("promoted-conversation.jsonl");
    std::fs::create_dir_all(&external_dir).expect("external dir");
    std::fs::copy(&original, &external).expect("stage external copy");
    std::fs::remove_file(&original).expect("remove the home copy");
    // Review item 3: the renamed transcript's sidecars keep the native session
    // id and live at <dir>/<S>/, not under the file stem. They must still be
    // staged into the child home.
    std::fs::create_dir_all(external_dir.join(&session).join("subagents")).expect("sidecar dir");
    std::fs::write(
        external_dir.join(&session).join("subagents/side.jsonl"),
        b"{\"side\":true}\n",
    )
    .expect("sidecar");
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
    assert!(
        staged
            .parent()
            .unwrap()
            .join(&session)
            .join("subagents/side.jsonl")
            .is_file(),
        "the renamed transcript's <dir>/<S>/ sidecars are staged by session id"
    );
}

/// Review item 4: a destination transcript that already exists but carries no
/// Remuda staging provenance is a conflict at the staging boundary — a stale
/// same-session file must never silently replace (or be replaced by) the
/// predecessor's conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_into_a_home_holding_an_unmarked_same_session_file_is_rejected() {
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

    // Pre-populate the child home as if a foreign launch once held this id.
    let child_home = harness.fresh_home();
    let stale = transcript_in(&child_home, &harness.workspace, &session);
    std::fs::create_dir_all(stale.parent().unwrap()).expect("mkdir");
    std::fs::write(&stale, b"a conversation this launch never staged\n").expect("stale file");

    let child = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "second turn",
            Some((&parent_id, session.as_str())),
            &harness.binary,
            &child_home,
        ))
        .await
        .expect("the create is accepted; staging happens at build time");
    let child_id = child.instance.meta.id.clone();

    // Staging fails inside the build, which fails the materialization and
    // records the reason on the instance (the command is marked unknown).
    let failure = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let instance = harness.node.get_instance(&child_id).expect("instance");
            if let Some(last_error) = &instance.last_error
                && last_error.contains("staging provenance")
            {
                return last_error.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "build failure with the provenance conflict was not recorded: {:?}",
            harness.node.get_instance(&child_id).map(|i| i.last_error)
        )
    });
    assert!(failure.contains("staging provenance"), "{failure}");
    assert_eq!(
        std::fs::read_to_string(&stale).unwrap(),
        "a conversation this launch never staged\n",
        "the unmarked destination is never overwritten"
    );
}

/// Review item 6: a transcript that exists but cannot be read is refused
/// BEFORE acceptance — the failure must not surface only inside the staging
/// factory after the instance row exists. Skipped under root, which bypasses
/// permission bits.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_with_an_unreadable_predecessor_transcript_is_refused_before_acceptance() {
    use std::os::unix::fs::PermissionsExt;
    if nix::unistd::geteuid().is_root() {
        eprintln!("skipping permission test when running as root");
        return;
    }
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
    let transcript = transcript_in(&parent_home, &harness.workspace, &session);
    wait_contains(&transcript, "first turn").await;

    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o000))
        .expect("deny read on the predecessor transcript");
    let child_home = harness.fresh_home();
    let before = harness.node.list_instances().expect("list").items.len();
    let error = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "second turn",
            Some((&parent_id, session.as_str())),
            &harness.binary,
            &child_home,
        ))
        .await
        .expect_err("an unreadable transcript is refused before acceptance");
    // Restore before asserting so the temp dir cleanup and any diagnostics are
    // unaffected; unlink does not require the file itself to be readable.
    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o644))
        .expect("restore perms");
    let message = error.to_string();
    assert!(
        message.contains("not readable before acceptance"),
        "unexpected refusal: {message}"
    );
    assert_eq!(
        harness.node.list_instances().expect("list").items.len(),
        before,
        "a refused resume creates no instance row"
    );
}

/// Review item 5: when the NEWEST predecessor's transcript is missing, resume
/// is refused naming that instance — the resolver must not fall back to an
/// older ancestor (whose transcript would silently drop the newest chapter's
/// turns).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_refuses_when_the_newest_chapter_transcript_is_missing() {
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

    let child1_home = harness.fresh_home();
    let child1 = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "second turn",
            Some((&parent_id, session.as_str())),
            &harness.binary,
            &child1_home,
        ))
        .await
        .expect("gen1 resume accepted");
    wait_settled(&harness.node, &child1.command.command_id).await;
    let child1_id = child1.instance.meta.id.clone();
    let child1_transcript = transcript_in(&child1_home, &harness.workspace, &session);
    wait_contains(&child1_transcript, "second turn").await;

    // The newest chapter's conversation disappears (retention / disk prune);
    // the grandparent transcript still exists and must NOT be substituted.
    std::fs::remove_file(&child1_transcript).expect("prune newest transcript");
    let grandparent = transcript_in(&parent_home, &harness.workspace, &session);
    assert!(
        grandparent.is_file(),
        "the older ancestor transcript still exists"
    );

    let child2_home = harness.fresh_home();
    let instances_before = harness.node.list_instances().expect("list").items.len();
    let error = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "third turn",
            Some((&child1_id, session.as_str())),
            &harness.binary,
            &child2_home,
        ))
        .await
        .expect_err("a missing newest transcript is refused, not substituted");
    let message = error.to_string();
    assert!(
        message.contains("predecessor") && message.contains("not found"),
        "clear refusal: {message}"
    );
    assert!(
        message.contains(child1_id.as_id().as_str()),
        "the message names the missing NEWEST predecessor: {message}"
    );
    assert!(
        !message.contains(parent_id.as_id().as_str()),
        "the refusal is about the newest chapter, not the grandparent: {message}"
    );
    assert_eq!(
        harness.node.list_instances().expect("list").items.len(),
        instances_before,
        "a refused resume creates no instance row"
    );
}

/// Review item 1: a `resumeSessionId` that is not one safe file-name component
/// is refused before acceptance — it is interpolated into transcript paths in
/// both the resolver and the staging factory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_with_a_traversal_session_id_is_refused_before_acceptance() {
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
    let real_session = recorded_session(&harness.node, &parent_id).await;
    let before = harness.node.list_instances().expect("list").items.len();

    for evil in ["../../../../tmp/evil-session", "a/b", "..", "x.jsonl"] {
        let child_home = harness.fresh_home();
        let error = harness
            .node
            .create_instance(request(
                DriverKind::ClaudePrint,
                "continue",
                Some((&parent_id, evil)),
                &harness.binary,
                &child_home,
            ))
            .await
            .expect_err("a traversal-shaped resume id must be refused");
        let message = error.to_string();
        assert!(
            message.contains("not a valid Claude session id"),
            "clear validation refusal for {evil:?}: {message}"
        );
    }
    assert_eq!(
        harness.node.list_instances().expect("list").items.len(),
        before,
        "refused ids create no instance rows; real predecessor session was {real_session}"
    );
}

/// Review item 1: the requested resume session must be the predecessor's
/// recorded native session. A different valid UUID names a different
/// conversation and must not be resumed off this predecessor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_session_id_must_match_the_predecessor_recorded_session() {
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
    let real_session = recorded_session(&harness.node, &parent_id).await;
    let stranger = "01993ab0-0000-7000-8000-0000000000ff";
    assert_ne!(stranger, real_session);

    let child_home = harness.fresh_home();
    let error = harness
        .node
        .create_instance(request(
            DriverKind::ClaudePrint,
            "continue",
            Some((&parent_id, stranger)),
            &harness.binary,
            &child_home,
        ))
        .await
        .expect_err("a foreign session id on this predecessor must be refused");
    let message = error.to_string();
    assert!(
        message.contains("cannot resume session")
            && message.contains(stranger)
            && message.contains(&real_session),
        "the refusal names both the requested and the recorded session: {message}"
    );
    assert!(
        !child_home.exists()
            || std::fs::read_dir(&child_home)
                .expect("read home")
                .next()
                .is_none(),
        "nothing is staged for a refused resume"
    );
}

/// Review item 1 follow-up: at create time an instance records its own
/// `ins_…` id as the native session placeholder until the driver reports the
/// real session. A resume that races that window must treat the placeholder as
/// inconclusive (like an unknown recording), not refuse it as a mismatched
/// predecessor session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_is_accepted_while_the_predecessor_still_records_the_ins_placeholder() {
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
    let transcript = transcript_in(&parent_home, &workspace, &session);
    assert!(transcript.is_file());

    // Rewind the predecessor to its create-time recording: the native session
    // is still the instance's own `ins_…` placeholder (driver evidence has not
    // landed yet), while the transcript path is already recorded.
    let placeholder = parent_id.as_id().to_string();
    assert!(placeholder.starts_with("ins_"));
    store
        .set_native_session(
            &parent_id,
            &placeholder,
            Some(transcript.to_str().unwrap()),
            None,
        )
        .expect("rewind to the create-time placeholder recording");

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
        .expect("the ins_ placeholder is inconclusive, not a mismatched session");
    wait_settled(&node, &child.command.command_id).await;
    assert!(
        transcript_in(&child_home, &workspace, &session).is_file(),
        "the predecessor transcript was staged despite the placeholder recording"
    );
}
