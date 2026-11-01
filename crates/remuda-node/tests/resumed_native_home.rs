//! resume-config r2: a resumed chapter OWNS its managed Claude home.
//!
//! The Node transfers ownership by moving the predecessor's
//! `instances/<id>/native-home` into the successor's own slot before launch.
//! These tests drive the real Node (native factories) with the fake Claude
//! binary, so no inference is involved: what matters is which slot holds the
//! predecessor's transcript afterwards.

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig,
    compose,
};
use remuda_protocol::{AgentKind, DriverKind, InstanceId, InstanceLifecycle};
use remuda_testing::ensure_workspace_bin;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SESSION: &str = "01993ab0-0000-7000-8000-0000000000a1";
const SECOND_SESSION: &str = "01993ab0-0000-7000-8000-0000000000a2";

fn loopback_config(root: &Path) -> DevServerConfig {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = root.join("workspace");
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    http
}

fn native_node(data: &Path) -> DevNode {
    let fake = ensure_workspace_bin("fake-claude");
    let mut native = NativeDriverConfig::new(data.to_path_buf()).with_claude_binary(fake);
    // The test must exercise the Remuda-managed slot, whatever the ambient
    // Node-wide home environment says.
    native.claude_native_home = None;
    compose(&ServeConfig {
        http: loopback_config(data),
        data_dir: data.to_path_buf(),
        drivers: LocalDrivers::Native(native),
    })
    .expect("compose native node")
}

/// A CLI-carried Claude seat on `direct` delegation: no explicit config dir
/// and no inherited operator default, so it lands in the managed per-instance
/// home — the slot ownership transfer is about.
fn request(prompt: &str) -> CreateInstanceRequest {
    CreateInstanceRequest {
        creator_permission_mode: None,
        memory_context: None,
        initiator: None,
        origin: remuda_protocol::InputOrigin::Human,
        agent_credential: None,
        command_id: None,
        instance_id: None,
        host_id: None,
        workspace_id: None,
        kind: AgentKind::Claude,
        driver: DriverKind::ClaudePrint,
        model: "fake".to_owned(),
        args: Vec::new(),
        binary_path: None,
        binary_sha256: None,
        provider_profile_id: "dev-fake".to_owned(),
        permission_mode: "manual".to_owned(),
        sandbox: None,
        prompt: prompt.to_owned(),
        cwd: None,
        // Gateway delegation (with a delivered overlay) is the seat shape that
        // runs in a Remuda-managed per-instance home: no explicit
        // `claudeConfigDir`, no inherited operator default.
        delegation: Some("gateway".to_owned()),
        settings_overlay_path: None,
        claude_config_dir: None,
        max_budget_usd: None,
        provider_overlay: Some(json!({
            "profileId": "pvp_01993ab0-0000-7000-8000-0000000000ff",
            "kind": "gateway",
            "baseUrl": "https://gateway.example.invalid/v1",
            "model": "fake"
        })),
        provider_auth_token: Some("fake-provider-token".to_owned()),
        resume_session_id: None,
        resumed_from: None,
        effort: None,
        tui: None,
        extra_env: Default::default(),
        capabilities: Default::default(),
        api_route: None,
        api_relay_endpoint: None,
    }
}

fn successor(parent: &InstanceId, session: &str) -> CreateInstanceRequest {
    let mut request = request("continue the conversation");
    request.resume_session_id = Some(session.to_owned());
    request.resumed_from = Some(parent.clone());
    request.instance_id = Some(InstanceId::new());
    request
}

fn slot(data: &Path, id: &InstanceId) -> PathBuf {
    data.join("instances")
        .join(id.as_id().as_str())
        .join("native-home")
}

fn transcript(home: &Path) -> PathBuf {
    home.join("projects").join("slug").join("session.jsonl")
}

/// The predecessor's home is created while its driver is materialized; plant
/// the transcript once the directory exists.
async fn wait_for_slot(home: &Path) -> PathBuf {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !home.is_dir() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("managed home {} never appeared", home.display()));
    let file = transcript(home);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("projects dir");
    std::fs::write(&file, "{\"type\":\"user\"}\n").expect("transcript");
    file
}

/// The recipe is written after the driver starts; the transfer/launch itself
/// happens before that.
async fn wait_for_recipe(node: &DevNode, id: &InstanceId) -> remuda_driver::LaunchRecipe {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(Some(recipe)) = node.launch_recipe(id) {
                return recipe;
            }
            if let Ok(instance) = node.get_instance(id)
                && instance.lifecycle == InstanceLifecycle::Failed
            {
                panic!(
                    "{id:?} failed before materializing: {}",
                    instance.last_error.unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{id:?} never materialized a launch recipe"))
}

fn is_terminal(lifecycle: InstanceLifecycle) -> bool {
    matches!(
        lifecycle,
        InstanceLifecycle::Exited | InstanceLifecycle::Failed
    )
}

/// Close the chapter and wait until it has process-end evidence; the transfer
/// refuses a predecessor that is still running.
async fn close_and_wait(node: &DevNode, id: &InstanceId) -> InstanceLifecycle {
    if let Ok(instance) = node.get_instance(id)
        && !is_terminal(instance.lifecycle)
    {
        node.submit_command(
            id,
            serde_json::from_value(json!({"operation": "close"})).expect("close parses"),
        )
        .await
        .expect("close accepted");
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let lifecycle = node.get_instance(id).expect("instance row").lifecycle;
            if is_terminal(lifecycle) {
                return lifecycle;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chapter reaches process-end evidence")
}

/// Wait for the successor's slot to carry the predecessor's transcript.
async fn wait_for_transferred_slot(home: &Path) -> PathBuf {
    let file = transcript(home);
    tokio::time::timeout(Duration::from_secs(60), async {
        while !file.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the transcript never arrived in {} (slot exists: {})",
            home.display(),
            home.is_dir()
        )
    });
    file
}

// ── 1. A→B→C: each chapter owns its home ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chain_hands_the_transcript_forward_and_only_the_last_slot_holds_it() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let a = node
        .create_instance(request("hello"))
        .await
        .expect("chapter A")
        .instance
        .meta
        .id
        .clone();
    let a_slot = slot(data.path(), &a);
    wait_for_slot(&a_slot).await;
    close_and_wait(&node, &a).await;

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request).await.expect("chapter B");
    let b_slot = slot(data.path(), &b_id);
    wait_for_transferred_slot(&b_slot).await;
    assert!(
        !a_slot.exists(),
        "A's slot is empty after handing its home to B: {}",
        a_slot.display()
    );
    close_and_wait(&node, &b_id).await;

    let c_id = InstanceId::new();
    let mut c_request = successor(&b_id, SESSION);
    c_request.instance_id = Some(c_id.clone());
    node.create_instance(c_request).await.expect("chapter C");
    let c_slot = slot(data.path(), &c_id);
    wait_for_transferred_slot(&c_slot).await;
    assert!(
        !b_slot.exists(),
        "B's slot is empty after handing its home to C: {}",
        b_slot.display()
    );

    // Only the latest chapter owns the home, and its recipe records that slot
    // as the native home the resumed process runs in.
    let mut holders = Vec::new();
    for entry in std::fs::read_dir(data.path().join("instances")).expect("instances dir") {
        let dir = entry.expect("entry").path();
        if dir.join("native-home").is_dir() {
            holders.push(dir.join("native-home"));
        }
    }
    assert_eq!(holders, vec![c_slot.clone()], "exactly one owner");
    let recipe = wait_for_recipe(&node, &c_id).await;
    assert_eq!(Path::new(&recipe.native_home), c_slot);
    assert!(
        recipe.argv.windows(2).any(|pair| pair[0] == "--resume"),
        "chapter C resumes the native session: {:?}",
        recipe.argv
    );
}

// ── 2. Deleting the predecessor does not touch the successor's home ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_the_predecessor_leaves_the_successors_home_intact() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let a = node
        .create_instance(request("hello"))
        .await
        .expect("chapter A")
        .instance
        .meta
        .id
        .clone();
    let a_slot = slot(data.path(), &a);
    wait_for_slot(&a_slot).await;
    close_and_wait(&node, &a).await;

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request).await.expect("chapter B");
    let b_slot = slot(data.path(), &b_id);
    let b_transcript = wait_for_transferred_slot(&b_slot).await;
    close_and_wait(&node, &b_id).await;

    let purged = node.purge_instance(&a).await.expect("purge A");
    assert_eq!(purged["purged"], json!(true));
    assert!(
        !data
            .path()
            .join("instances")
            .join(a.as_id().as_str())
            .exists(),
        "the predecessor's instance directory is gone"
    );
    assert!(
        b_transcript.is_file(),
        "the live successor's transferred home survived A's purge: {}",
        b_transcript.display()
    );
    assert_eq!(
        std::fs::read_to_string(&b_transcript).expect("transcript"),
        "{\"type\":\"user\"}\n"
    );
}

// ── 3. A second successor of the same parent is refused ───────────────────

/// ma-fence allows one successor per generation, so a second successor of the
/// same predecessor is a duplicate continuation of one conversation. Starting
/// it in a fresh home would silently discard that conversation (the bug this
/// round fixes), so the Node refuses with a clear reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_successor_of_one_parent_is_refused_not_started_blank() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let a = node
        .create_instance(request("hello"))
        .await
        .expect("chapter A")
        .instance
        .meta
        .id
        .clone();
    let a_slot = slot(data.path(), &a);
    wait_for_slot(&a_slot).await;
    close_and_wait(&node, &a).await;

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request).await.expect("chapter B");
    let b_slot = slot(data.path(), &b_id);
    wait_for_transferred_slot(&b_slot).await;
    close_and_wait(&node, &b_id).await;

    // A second successor addressed at the same parent: its managed home was
    // already moved, and a fresh empty one would lose the transcript.
    let c_id = InstanceId::new();
    let mut c_request = successor(&a, SECOND_SESSION);
    c_request.instance_id = Some(c_id.clone());
    node.create_instance(c_request)
        .await
        .expect("the create is accepted before materialization");
    let failed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let instance = node.get_instance(&c_id).expect("chapter C row");
            if instance.lifecycle == InstanceLifecycle::Failed {
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chapter C is refused at materialization");
    let error = failed.last_error.unwrap_or_default();
    assert!(
        error.contains("already transferred") || error.contains("already"),
        "the refusal names the lost home: {error}"
    );
    assert!(!slot(data.path(), &c_id).exists(), "no blank home was made");
    assert!(transcript(&b_slot).is_file(), "B's home is untouched");
}

// ── 4. An explicit config dir is shared on purpose and never moved ────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_claude_config_dir_is_never_moved() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let explicit = data.path().join("operator-claude-home");
    std::fs::create_dir_all(&explicit).expect("explicit home");
    let marker = transcript(&explicit);
    std::fs::create_dir_all(marker.parent().expect("parent")).expect("projects");
    std::fs::write(&marker, "{\"type\":\"user\"}\n").expect("marker");

    let mut a_request = request("hello");
    a_request.claude_config_dir = Some(explicit.to_string_lossy().into_owned());
    let a = node
        .create_instance(a_request)
        .await
        .expect("chapter A")
        .instance
        .meta
        .id
        .clone();
    close_and_wait(&node, &a).await;

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    // The Hub carries the recorded explicit dir onto the successor spec.
    b_request.claude_config_dir = Some(explicit.to_string_lossy().into_owned());
    node.create_instance(b_request).await.expect("chapter B");
    let _ = wait_for_recipe(&node, &b_id).await;

    assert!(marker.is_file(), "the explicit home keeps its transcript");
    assert!(
        !slot(data.path(), &a).exists() && !slot(data.path(), &b_id).exists(),
        "an explicit dir never creates or moves a managed slot"
    );
    let recipe = wait_for_recipe(&node, &b_id).await;
    assert_eq!(Path::new(&recipe.native_home), explicit);
}

// ── 5. A crash between the rename and the launch is recoverable ───────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_resume_finds_the_home_already_in_the_successor_slot() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let a = node
        .create_instance(request("hello"))
        .await
        .expect("chapter A")
        .instance
        .meta
        .id
        .clone();
    let a_slot = slot(data.path(), &a);
    wait_for_slot(&a_slot).await;
    close_and_wait(&node, &a).await;

    // Simulate a crash after the ownership rename but before the launch: the
    // home already sits in the successor's slot (nothing launched yet).
    let b_id = InstanceId::new();
    let b_slot = slot(data.path(), &b_id);
    std::fs::create_dir_all(b_slot.parent().expect("instances dir")).expect("successor dir");
    std::fs::rename(&a_slot, &b_slot).expect("ownership rename");
    assert!(!slot(data.path(), &a).exists());

    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    // The retry must neither refuse nor look for a predecessor home.
    node.create_instance(b_request)
        .await
        .expect("the retried resume is accepted");
    let transcript = wait_for_transferred_slot(&b_slot).await;
    assert!(transcript.is_file());
    let recipe = wait_for_recipe(&node, &b_id).await;
    assert_eq!(Path::new(&recipe.native_home), b_slot);
}

// ── 6. A still-running predecessor is refused, never moved ────────────────

/// ma-fence resumes only a paused/closed chapter; a Node asked to take over a
/// home whose chapter still runs must refuse rather than move a directory a
/// live process may be writing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_still_running_predecessor_is_refused_with_a_clear_error() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let a = node
        .create_instance(request("hello"))
        .await
        .expect("chapter A")
        .instance
        .meta
        .id
        .clone();
    let a_slot = slot(data.path(), &a);
    wait_for_recipe(&node, &a).await;
    let live = node.get_instance(&a).expect("chapter A row");
    assert!(
        !is_terminal(live.lifecycle),
        "chapter A is still live: {:?}",
        live.lifecycle
    );

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request)
        .await
        .expect("the create is accepted before materialization");
    let failed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let instance = node.get_instance(&b_id).expect("chapter B row");
            if instance.lifecycle == InstanceLifecycle::Failed {
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chapter B is refused while A runs");
    let error = failed.last_error.unwrap_or_default();
    assert!(
        error.contains("not exited"),
        "the refusal says the predecessor is still live: {error}"
    );
    assert!(a_slot.is_dir(), "A's home was not moved");
    assert!(!slot(data.path(), &b_id).exists(), "B owns no slot");
    close_and_wait(&node, &a).await;
}
