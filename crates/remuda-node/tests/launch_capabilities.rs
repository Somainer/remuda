//! cb-seat-profile (D-063, main-agent.md §3.4.2): the Node journals ONE
//! `launch.capabilities` event per agent launch, before the harness starts.
//!
//! A configured skill directory is installed into the launch's managed home
//! with the digest the event names; one that exists but cannot be read
//! refuses the launch, the event is still journaled listing it, and nothing is
//! installed. The event passes the shared secret-shape lint even when its
//! inputs carry secrets.

use remuda_node::{
    CreateInstanceRequest, DevNode, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig,
    compose,
};
use remuda_protocol::{
    CommandState, JournalEvent, LaunchCapabilitiesPayload, LaunchItemClass,
    LaunchUnavailableReason, ObservationPayload,
};
use remuda_testing::{ScriptKind, ensure_workspace_bin, script_path};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn node(data: &Path) -> DevNode {
    let mut http = DevServerConfig::loopback(0);
    http.workspace_root = data.join("workspace");
    http.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    std::fs::create_dir_all(&http.workspace_root).expect("workspace");
    let mut native = NativeDriverConfig::new(data.join("node"))
        .with_claude_binary(ensure_workspace_bin("fake-claude"));
    native.extra_env.insert(
        "FAKE_CLAUDE_SCRIPT".to_owned(),
        script_path(ScriptKind::TwoTurn)
            .to_string_lossy()
            .into_owned(),
    );
    compose(&ServeConfig {
        http,
        data_dir: data.join("node"),
        drivers: LocalDrivers::Native(native),
    })
    .expect("compose")
}

fn request(home: &Path, skills: Vec<String>, args: Vec<String>) -> CreateInstanceRequest {
    let mut request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
        "kind": "claude",
        "driver": "claude-sdk",
        "model": "fake",
        "permissionMode": "manual",
        "prompt": "",
    }))
    .expect("request");
    request.origin = remuda_protocol::InputOrigin::Human;
    // An explicit config dir: a Remuda-managed (shared) home the skill lands in.
    request.claude_config_dir = Some(home.to_string_lossy().into_owned());
    request.skills = skills;
    request.args = args;
    request
}

fn skill_source(at: &Path, name: &str) -> PathBuf {
    let dir = at.join("sources").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\n---\nbody\n"),
    )
    .unwrap();
    dir
}

/// The journal's events, in order, as `(kind, payload)`.
fn journal(node: &DevNode, instance: &remuda_protocol::Instance) -> Vec<ObservationPayload> {
    node.read_journal(&instance.journal_id, None, 4096)
        .expect("journal")
        .events
        .into_iter()
        .filter_map(|event| match event {
            JournalEvent::Instance(observation) => Some(observation.body),
            _ => None,
        })
        .collect()
}

async fn wait_snapshot(
    node: &DevNode,
    instance: &remuda_protocol::Instance,
) -> (LaunchCapabilitiesPayload, Vec<ObservationPayload>) {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let events = journal(node, instance);
            let found = events.iter().find_map(|payload| match payload {
                ObservationPayload::LaunchCapabilities(snapshot) => Some((**snapshot).clone()),
                _ => None,
            });
            if let Some(snapshot) = found {
                return (snapshot, events);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a launch.capabilities event")
}

/// The index of the first `lifecycle` entity row for the instance reaching
/// `state`.
fn lifecycle_index(events: &[ObservationPayload], state: &str) -> Option<usize> {
    events.iter().position(|payload| match payload {
        ObservationPayload::Lifecycle(lifecycle) => match lifecycle.as_ref() {
            remuda_protocol::LifecyclePayload::Entity(entity) => entity.state == state,
            _ => false,
        },
        _ => false,
    })
}

async fn close(node: &DevNode, id: &remuda_protocol::InstanceId) {
    let _ = node
        .submit_command(
            id,
            serde_json::from_value(serde_json::json!({"operation": "close"})).expect("close"),
        )
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_skill_is_journaled_before_start_and_installed_with_its_digest() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = node(dir.path());
    let home = dir.path().join("home");
    let source = skill_source(dir.path(), "review-kit");
    let created = node
        .create_instance(request(
            &home,
            vec![source.to_string_lossy().into_owned()],
            Vec::new(),
        ))
        .await
        .expect("create");
    let (snapshot, _) = wait_snapshot(&node, &created.instance).await;
    let ready = tokio::time::timeout(TIMEOUT, async {
        loop {
            let events = journal(&node, &created.instance);
            if lifecycle_index(&events, "ready").is_some() {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the launch starts");
    let event_at = ready
        .iter()
        .position(|payload| matches!(payload, ObservationPayload::LaunchCapabilities(_)))
        .unwrap();
    let starting_at = lifecycle_index(&ready, "starting").expect("starting");
    assert!(
        event_at < starting_at,
        "the snapshot precedes the harness start ({event_at} vs {starting_at})"
    );
    assert_eq!(
        ready
            .iter()
            .filter(|payload| matches!(payload, ObservationPayload::LaunchCapabilities(_)))
            .count(),
        1,
        "exactly one event per launch"
    );
    assert!(snapshot.refusal.is_none());
    let resolved = snapshot
        .resolved
        .iter()
        .find(|item| item.name == "review-kit")
        .expect("resolved");
    assert_eq!(resolved.path.as_deref(), Some("skills/review-kit"));
    let installed = std::fs::read(home.join("skills/review-kit/SKILL.md")).expect("installed");
    let digest =
        remuda_driver::launch::capabilities::tree_digest(&[("SKILL.md".to_owned(), installed)]);
    assert_eq!(resolved.digest.as_deref(), Some(digest.as_str()));
    close(&node, &created.instance.meta.id).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_configured_skill_refuses_the_launch_and_is_still_journaled() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tmp");
    let node = node(dir.path());
    let home = dir.path().join("home");
    let source = skill_source(dir.path(), "locked-kit");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&source).is_ok() {
        // A root Node reads every mode: nothing to refuse.
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: running as root");
        return;
    }
    let created = node
        .create_instance(request(
            &home,
            vec![source.to_string_lossy().into_owned()],
            Vec::new(),
        ))
        .await
        .expect("the create is accepted; the launch is refused");
    let (snapshot, _) = wait_snapshot(&node, &created.instance).await;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(snapshot.unavailable.len(), 1);
    assert_eq!(snapshot.unavailable[0].class, LaunchItemClass::Skill);
    assert_eq!(snapshot.unavailable[0].name, "locked-kit");
    assert_eq!(
        snapshot.unavailable[0].reason,
        LaunchUnavailableReason::Unreadable
    );
    let refusal = snapshot.refusal.expect("refused");
    assert_eq!(refusal.reason, "launch-capability-unreadable");
    assert_eq!(refusal.next.action, "ask-owner");

    // The create settles rejected with the reason, and the harness never ran.
    let command = tokio::time::timeout(TIMEOUT, async {
        loop {
            let command = node
                .get_command(&created.command.command_id)
                .expect("command");
            if command.state == CommandState::Settled {
                return command;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the create settles");
    let text = serde_json::to_string(&command).unwrap();
    assert!(text.contains("rejected"), "{text}");
    assert!(text.contains("launch-capability-unreadable"), "{text}");
    let events = journal(&node, &created.instance);
    assert!(
        lifecycle_index(&events, "ready").is_none(),
        "the harness never started"
    );
    assert!(
        !home.join("skills/locked-kit").exists(),
        "nothing installed for a refused launch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journaled_event_passes_the_shared_secret_lint() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = node(dir.path());
    let home = dir.path().join("home");
    let token = "sk-ant-api03-SEATPROFxSECRETxVALUEx0123456789abcdefABCDEF0123456789";
    assert!(
        !remuda_hub::secret_lint::secret_rules(token).is_empty(),
        "the lint flags the raw token, so a clean event is meaningful"
    );
    let source = skill_source(dir.path(), "kit");
    std::fs::write(source.join("token.txt"), token).unwrap();
    let config = dir.path().join("servers.json");
    std::fs::write(
        &config,
        format!(r#"{{"mcpServers":{{"docs":{{"command":"x","env":{{"API_KEY":"{token}"}}}}}}}}"#),
    )
    .unwrap();
    let created = node
        .create_instance(request(
            &home,
            vec![source.to_string_lossy().into_owned()],
            vec![
                "--mcp-config".to_owned(),
                config.to_string_lossy().into_owned(),
            ],
        ))
        .await
        .expect("create");
    let (snapshot, _) = wait_snapshot(&node, &created.instance).await;
    let row = serde_json::to_string(&ObservationPayload::LaunchCapabilities(Box::new(
        snapshot.clone(),
    )))
    .unwrap();
    assert_eq!(
        remuda_hub::secret_lint::secret_rules(&row),
        Vec::<&str>::new(),
        "{row}"
    );
    assert!(!row.contains(token));
    assert!(row.contains("\"docs\""), "the server is named: {row}");
    close(&node, &created.instance.meta.id).await;
}

/// Kinds `terminal` and `generic` launch a shell: no event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_shell_journals_no_snapshot() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = node(dir.path());
    let mut request: CreateInstanceRequest = serde_json::from_value(serde_json::json!({
        "kind": "terminal",
        "driver": "shell-pty",
        "model": "fake",
        "permissionMode": "manual",
        "prompt": "",
    }))
    .expect("request");
    request.origin = remuda_protocol::InputOrigin::Human;
    let created = node.create_instance(request).await.expect("create");
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if lifecycle_index(&journal(&node, &created.instance), "ready").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the shell starts");
    assert!(
        !journal(&node, &created.instance)
            .iter()
            .any(|payload| matches!(payload, ObservationPayload::LaunchCapabilities(_)))
    );
    close(&node, &created.instance.meta.id).await;
}

/// r2 item 2: a skill dropped from the list leaves the shared home before the
/// next launch starts, and the event names it; a tree without Remuda's
/// marker is the user's and stays byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_skill_leaves_the_shared_home_and_a_user_tree_stays() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = node(dir.path());
    let home = dir.path().join("home");
    let source = skill_source(dir.path(), "kit-a");
    let first = node
        .create_instance(request(
            &home,
            vec![source.to_string_lossy().into_owned()],
            Vec::new(),
        ))
        .await
        .expect("create a");
    let (snapshot, _) = wait_snapshot(&node, &first.instance).await;
    assert!(snapshot.removed.is_empty());
    // The materializer writes the tree when the driver starts, just after
    // the event is journaled.
    tokio::time::timeout(TIMEOUT, async {
        while !home.join("skills/kit-a/SKILL.md").is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("installed");
    close(&node, &first.instance.meta.id).await;

    // The user's own tree beside it, without the marker.
    std::fs::create_dir_all(home.join("skills/kit-b")).unwrap();
    std::fs::write(home.join("skills/kit-b/SKILL.md"), b"mine").unwrap();

    let second = node
        .create_instance(request(&home, Vec::new(), Vec::new()))
        .await
        .expect("create without skills");
    let (snapshot, _) = wait_snapshot(&node, &second.instance).await;
    assert_eq!(snapshot.removed, vec!["kit-a".to_owned()]);
    assert!(
        !home.join("skills/kit-a").exists(),
        "the revoked tree is gone"
    );
    assert_eq!(
        std::fs::read(home.join("skills/kit-b/SKILL.md")).unwrap(),
        b"mine",
        "a marker-less tree is never touched"
    );
    close(&node, &second.instance.meta.id).await;
}
