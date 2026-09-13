//! D-026 Node behaviour: record the driver's session id, then resume it onto a
//! new Instance instead of reviving the exited one.

use remuda_node::{
    DevNode, DevServerConfig, DriverRegistry, LocalStore, MemoryStore, dispatch_hub_rpc,
};
use remuda_protocol::{
    Completeness, DriverKind, Instance, InstanceId, Knowledge, LifecyclePayload, LifecycleTopic,
    NativeLifecycle, ObservationPayload, Severity,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn config(root: &std::path::Path) -> DevServerConfig {
    let mut config = DevServerConfig::loopback(0);
    config.workspace_root = root.join("workspace");
    config.workspace_roots = Some(vec![std::env::temp_dir()]);
    std::fs::create_dir_all(&config.workspace_root).expect("workspace");
    config
}

/// The lifecycle a Claude driver emits once it knows its own session.
fn session_observation(session_id: &str, transcript: &str) -> ObservationPayload {
    ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
        NativeLifecycle {
            topic: LifecycleTopic::Session,
            native_name: "session".into(),
            native_id: Knowledge::Known {
                value: session_id.into(),
            },
            status: Knowledge::Known {
                value: "started".into(),
            },
            related_ids: BTreeMap::from([("transcriptPath".into(), transcript.to_owned())]),
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        },
    ))))
}

fn session_id_of(instance: &Instance) -> Option<&str> {
    match &instance.native_ref.session_id {
        Knowledge::Known { value } => Some(value.as_str()),
        _ => None,
    }
}

#[tokio::test]
async fn store_records_the_session_a_driver_reports() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = config(dir.path());
    let store = Arc::new(MemoryStore::new(8));
    let node = DevNode::with_parts(
        &config,
        store.clone(),
        DriverRegistry::with_fake().expect("registry"),
    )
    .expect("node");
    let created = node
        .create_instance(serde_json::from_value(json!({"prompt":"hi"})).expect("request"))
        .await
        .expect("create");
    let instance_id = created.instance.meta.id.clone();

    // Before the driver reports anything, nativeRef holds the placeholder the
    // Node minted, which `claude --resume` would reject.
    let before = store.get_instance(&instance_id).expect("before");
    assert_eq!(
        session_id_of(&before),
        Some(instance_id.as_id().as_str()),
        "placeholder stands in until the driver reports"
    );

    let session = "01993ab0-0000-7000-8000-0000000000dd";
    store
        .append_observation(
            &instance_id,
            None,
            Completeness::Structured,
            session_observation(session, "/tmp/resume-test.jsonl"),
        )
        .expect("append");
    let updated = store
        .set_native_session(&instance_id, session, Some("/tmp/resume-test.jsonl"))
        .expect("record")
        .expect("changed");
    assert_eq!(session_id_of(&updated), Some(session));
    assert_eq!(
        updated
            .native_ref
            .claude
            .as_ref()
            .map(|c| c.session_id.as_str()),
        Some(session),
    );
    match &updated.native_ref.transcript {
        Knowledge::Known { value } => assert_eq!(value.source_path, "/tmp/resume-test.jsonl"),
        other => panic!("transcript not recorded: {other:?}"),
    }

    // Repeating the same evidence must not churn revisions or journal noise.
    assert!(
        store
            .set_native_session(&instance_id, session, Some("/tmp/resume-test.jsonl"))
            .expect("repeat")
            .is_none(),
        "unchanged evidence reports no update"
    );
}

#[tokio::test]
async fn resume_rpc_builds_a_linked_child_and_requires_a_session() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = config(dir.path());
    let node = DevNode::new(&config).expect("node");
    let parent = node
        .create_instance(serde_json::from_value(json!({"prompt":"hi"})).expect("request"))
        .await
        .expect("create")
        .instance;
    let parent_id = parent.meta.id.as_id().as_str().to_owned();
    let session = "01993ab0-0000-7000-8000-0000000000ee";

    let created: Value = dispatch_hub_rpc(
        &node,
        "instance.resume",
        json!({
            "spec": { "kind": "claude", "driver": "claude-print", "prompt": "" },
            "resumeSessionId": session,
            "resumedFrom": parent_id,
        }),
    )
    .await
    .expect("resume");

    let child_id = created["instance"]["id"]
        .as_str()
        .expect("child id")
        .to_owned();
    assert_ne!(child_id, parent_id, "resume creates a new instance");
    assert_eq!(
        created["instance"]["parent"]["instanceId"],
        json!(parent_id),
        "the child records where its conversation came from"
    );
    // A resumed instance already knows its native identity: it is the one being
    // continued, not a placeholder awaiting the driver's first report.
    assert_eq!(
        created["instance"]["nativeRef"]["sessionId"]["value"],
        json!(session)
    );

    let child = node
        .get_instance(&InstanceId::try_from(child_id).expect("id"))
        .expect("child");
    assert_eq!(child.driver, DriverKind::ClaudePrint);
    assert_eq!(child.host_id, parent.host_id);
    assert_eq!(child.workspace_id, parent.workspace_id);

    let refused = dispatch_hub_rpc(
        &node,
        "instance.resume",
        json!({ "spec": { "kind": "claude", "driver": "claude-print" } }),
    )
    .await;
    assert!(
        refused.is_err(),
        "resume without a session id must not silently start a fresh conversation"
    );
}
