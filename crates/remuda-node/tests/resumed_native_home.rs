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
use std::time::{Duration, Instant};

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
        skill_context: None,
        skills: Vec::new(),
        revoked_skills: Vec::new(),
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
fn close_request() -> remuda_node::InstanceCommandRequest {
    serde_json::from_value(json!({"operation": "close"})).expect("close parses")
}

async fn close_and_wait(node: &DevNode, id: &InstanceId) -> InstanceLifecycle {
    if let Ok(instance) = node.get_instance(id)
        && !is_terminal(instance.lifecycle)
    {
        node.submit_command(id, close_request())
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
    // A re-forwarded create keeps its client command id. Using one here is what
    // makes the second call a genuine RETRY of the same command, not a fresh
    // intent that happens to reuse the instance id.
    let command_id = remuda_protocol::CommandId::new();
    b_request.command_id = Some(command_id.clone());
    let first = node
        .create_instance(b_request.clone())
        .await
        .expect("the retried resume is accepted");
    assert_eq!(first.instance.meta.id, b_id);
    assert_eq!(first.command.command_id, command_id);
    let transcript = wait_for_transferred_slot(&b_slot).await;
    assert!(transcript.is_file());
    let recipe = wait_for_recipe(&node, &b_id).await;
    assert_eq!(Path::new(&recipe.native_home), b_slot);

    // A RETRY of the SAME command (same instance AND command id, as a
    // re-forwarded create is) echoes the chapter already materialized: no
    // second command ledger row, no second home and no second launch, and the
    // transcript stays.
    let retried = node
        .create_instance(b_request)
        .await
        .expect("the same successor create is replayed");
    assert_eq!(retried.instance.meta.id, b_id);
    assert_eq!(
        retried.command.command_id, command_id,
        "the retried command id is echoed, not re-minted"
    );
    assert_ne!(
        node.get_instance(&b_id).expect("chapter B row").lifecycle,
        InstanceLifecycle::Failed,
        "the replay did not fail the chapter"
    );
    assert_eq!(retried.instance.meta.id, first.instance.meta.id);
    assert!(
        transcript.is_file(),
        "the retry kept the transcript in place"
    );
    assert_eq!(
        Path::new(&wait_for_recipe(&node, &b_id).await.native_home),
        b_slot
    );
    // Exactly one `instance.create` command for this chapter on the durable
    // ledger: the retry replayed the first, it did not admit a second.
    let db = rusqlite::Connection::open(data.path().join("node.sqlite")).expect("node db");
    let mut statement = db
        .prepare("SELECT json FROM commands WHERE instance_id = ?1")
        .expect("command query");
    let commands: Vec<serde_json::Value> = statement
        .query_map([b_id.as_id().as_str()], |row| row.get::<_, String>(0))
        .expect("command rows")
        .map(|raw| serde_json::from_str(&raw.expect("raw command")).expect("command json"))
        .collect();
    let creates = commands
        .iter()
        .filter(|command| command["operation"] == json!("instance.create"))
        .count();
    assert_eq!(creates, 1, "one create command, got {commands:?}");
    let launches = std::fs::read_dir(b_slot.parent().expect("instance dir"))
        .expect("instance dir")
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.file_name().to_string_lossy() == "launch")
        })
        .count();
    assert_eq!(launches, 1, "exactly one launch directory");
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
        error.contains("no close was requested"),
        "the refusal says the predecessor is live and no close is in flight: {error}"
    );
    assert!(a_slot.is_dir(), "A's home was not moved");
    assert!(!slot(data.path(), &b_id).exists(), "B owns no slot");
    close_and_wait(&node, &a).await;
}

// ── 7. Pause, then an immediate Resume: wait for the in-flight close ──────

/// A Hub pause commits `paused` at close admission and only ENQUEUES the
/// close, so a quick Resume forwards the successor while the predecessor's
/// row still reads ready/closing. The successor must wait (bounded) for the
/// process-end row, then take the home — not refuse and strand the transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quick_resume_waits_for_the_in_flight_close_and_takes_the_home() {
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

    // The pause's durable effect: a close on the ledger, process still live.
    node.test_record_close_in_flight(&a)
        .expect("close recorded");
    assert!(!is_terminal(
        node.get_instance(&a).expect("chapter A row").lifecycle
    ));
    // The close is enqueued by the pause; it lands while the Resume runs.
    let closer = node.clone();
    let owner = a.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let _ = closer.submit_command(&owner, close_request()).await;
    });

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    let started = std::time::Instant::now();
    node.create_instance(b_request)
        .await
        .expect("the successor create is accepted");
    let b_slot = slot(data.path(), &b_id);
    wait_for_transferred_slot(&b_slot).await;
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the successor waited for the in-flight close (took {:?})",
        started.elapsed()
    );
    assert!(!a_slot.exists(), "A's slot is empty after the transfer");
    assert_ne!(
        node.get_instance(&b_id).expect("chapter B row").lifecycle,
        InstanceLifecycle::Failed,
        "chapter B adopted the home instead of being refused"
    );
}

// ── 8. Refusal, then retry: walk back to the real owner ───────────────────

/// The r2 failure shape: a successor B is refused (predecessor live, no close
/// requested) and owns no slot or recipe. A retry anchored on B must walk
/// B -> A, find A's slot once the owner closes, and take the transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_refusal_walks_back_to_the_real_owner() {
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

    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request)
        .await
        .expect("the create is accepted before materialization");
    let failed_b = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let instance = node.get_instance(&b_id).expect("chapter B row");
            if instance.lifecycle == InstanceLifecycle::Failed {
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chapter B refused while A runs with no close requested");
    assert!(
        failed_b
            .last_error
            .unwrap_or_default()
            .contains("no close was requested")
    );
    assert!(!slot(data.path(), &b_id).exists(), "B owns no slot");
    assert!(a_slot.is_dir(), "A still owns its home");

    close_and_wait(&node, &a).await;

    // The retry a Hub makes after a failed chapter anchors on B, not A.
    let c_id = InstanceId::new();
    let mut c_request = successor(&b_id, SESSION);
    c_request.instance_id = Some(c_id.clone());
    node.create_instance(c_request)
        .await
        .expect("the retry create is accepted");
    let c_slot = slot(data.path(), &c_id);
    wait_for_transferred_slot(&c_slot).await;
    assert!(!a_slot.exists(), "A's home moved to C");
    assert!(!slot(data.path(), &b_id).exists(), "B never had a home");
    let recipe = wait_for_recipe(&node, &c_id).await;
    assert_eq!(Path::new(&recipe.native_home), c_slot);
}

// ── 9. Unknown predecessor with nothing to transfer: fresh start ──────────

/// A `resumedFrom` the Node never saw (no row, no slot, no recipe) has nothing
/// to transfer: the successor starts in a fresh managed slot, which is logged,
/// rather than refusing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_predecessor_with_nothing_to_transfer_starts_fresh() {
    let data = tempfile::tempdir().expect("data dir");
    let node = native_node(data.path());
    let ghost = InstanceId::new();
    let b_id = InstanceId::new();
    let mut b_request = successor(&ghost, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request)
        .await
        .expect("an unknown predecessor is not a refusal");
    let recipe = wait_for_recipe(&node, &b_id).await;
    assert_eq!(
        Path::new(&recipe.native_home),
        slot(data.path(), &b_id),
        "the successor started in its own managed slot"
    );
    assert!(!is_terminal(
        node.get_instance(&b_id).expect("chapter B row").lifecycle
    ));
}

// ── 10. No row, surviving slot: adopt rather than fail ────────────────────

/// A crash mid-purge or a rebuilt entity DB can leave the managed slot on disk
/// without a row for its chapter. There is no process to wait for, so the
/// successor takes the slot instead of failing with a raw lookup error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_predecessor_with_no_row_still_hands_over_its_surviving_slot() {
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

    // Rebuilt entity DB: the row is gone, the Node-owned home is still here.
    node.shutdown().await.expect("shutdown");
    drop(node);
    let db = rusqlite::Connection::open(data.path().join("node.sqlite")).expect("node db");
    db.execute("DELETE FROM instances WHERE id = ?1", [a.as_id().as_str()])
        .expect("drop instance row");
    db.execute(
        "DELETE FROM commands WHERE instance_id = ?1",
        [a.as_id().as_str()],
    )
    .expect("drop command rows");
    drop(db);
    assert!(a_slot.is_dir(), "the slot survived the row's loss");

    let node = native_node(data.path());
    let b_id = InstanceId::new();
    let mut b_request = successor(&a, SESSION);
    b_request.instance_id = Some(b_id.clone());
    node.create_instance(b_request)
        .await
        .expect("a row-less predecessor with a slot is not a refusal");
    let b_slot = slot(data.path(), &b_id);
    wait_for_transferred_slot(&b_slot).await;
    assert!(
        !a_slot.exists(),
        "the surviving slot moved to the successor"
    );
}

// ── 11. A settled close is not "in flight" ────────────────────────────────

/// Only a queued/accepted close can still be stopping the process. A settled
/// one must not turn an immediate refusal into the 30 s wait with a misleading
/// timeout message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_close_is_not_in_flight_and_is_refused_immediately() {
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
    wait_for_slot(&slot(data.path(), &a)).await;
    let command = node.test_record_settled_close(&a).expect("settled close");
    assert_eq!(command.state, remuda_protocol::CommandState::Settled);

    let started = Instant::now();
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
    .expect("chapter B refused");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a settled close must not be waited on (took {:?})",
        started.elapsed()
    );
    assert!(
        failed
            .last_error
            .unwrap_or_default()
            .contains("no close was requested"),
        "the refusal names the situation"
    );
    close_and_wait(&node, &a).await;
}

// ── 12. A close that never completes hits the bounded timeout ─────────────

/// The timeout branch of the bounded wait, with a short test-only bound so the
/// test does not spend the production 30 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_close_that_never_completes_times_out_with_a_clear_error() {
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
    wait_for_slot(&slot(data.path(), &a)).await;
    node.test_record_close_in_flight(&a)
        .expect("close recorded");

    let started = Instant::now();
    let error = node
        .test_wait_for_process_end(&a, Duration::from_millis(300))
        .await
        .expect_err("the bounded wait times out");
    let message = error.to_string();
    assert!(
        message.contains("close was requested but") && message.contains("still"),
        "the timeout names the in-flight close and the still-live chapter: {message}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the test-only bound stays short (took {:?})",
        started.elapsed()
    );
    close_and_wait(&node, &a).await;
}
