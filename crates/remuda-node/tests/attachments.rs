//! D-027: the Node pulls staged attachment bytes, writes them privately next
//! to the instance, and removes them when the instance is gone.

use anyhow::{Context, Result, bail};
use remuda_node::{
    DevNode, DevServerConfig, MaterializedAttachment, ObjectSource, ServeConfig, compose,
};
use remuda_protocol::CommandState;
use remuda_protocol::InstanceId;
use remuda_protocol::hubnode::AttachmentRef;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn png(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(len.max(bytes.len()), 0x42);
    bytes
}

/// Stands in for the Hub's `GET /v1/objects/{id}`.
#[derive(Debug, Default)]
struct FakeObjects {
    bodies: Mutex<BTreeMap<String, Vec<u8>>>,
    fetches: AtomicUsize,
    fail: Mutex<Option<String>>,
}

impl FakeObjects {
    fn with(entries: &[(&str, Vec<u8>)]) -> Arc<Self> {
        let source = Arc::new(Self::default());
        let mut bodies = source.bodies.lock().expect("lock");
        for (id, body) in entries {
            bodies.insert((*id).to_owned(), body.clone());
        }
        drop(bodies);
        source
    }
}

impl ObjectSource for FakeObjects {
    fn fetch(
        &self,
        object_id: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<u8>, remuda_node::NodeError>> + Send + '_>>
    {
        Box::pin(async move {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = self.fail.lock().expect("lock").clone() {
                return Err(remuda_node::NodeError::InvalidRequest(message));
            }
            self.bodies
                .lock()
                .expect("lock")
                .get(&object_id)
                .cloned()
                .ok_or_else(|| {
                    remuda_node::NodeError::InvalidRequest(format!("no such object {object_id}"))
                })
        })
    }
}

fn attachment(object_id: &str, media_type: &str) -> AttachmentRef {
    AttachmentRef {
        object_id: object_id.to_owned(),
        media_type: media_type.to_owned(),
        name: Some(format!("{object_id}.png")),
        size: None,
    }
}

struct Fixture {
    node: DevNode,
    data_dir: PathBuf,
    _dir: tempfile::TempDir,
}

fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("node-data");
    std::fs::create_dir_all(&data_dir)?;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let node = compose(&ServeConfig::fake(
        DevServerConfig::loopback(0)
            .with_workspace_root(workspace)
            .with_workspace_roots(remuda_testing::test_workspace_roots!()),
        data_dir.clone(),
    ))?;
    Ok(Fixture {
        node,
        data_dir,
        _dir: dir,
    })
}

async fn create_instance(node: &DevNode) -> Result<InstanceId> {
    let request = serde_json::from_value(json!({
        "origin": "human",
        "kind": "claude",
        "driver": "claude-print",
        "prompt": "",
    }))?;
    let created = node.create_instance(request).await?;
    Ok(created.instance.meta.id)
}

async fn send(
    node: &DevNode,
    instance: &InstanceId,
    attachments: Vec<AttachmentRef>,
) -> Result<Value> {
    let mut request: remuda_node::InstanceCommandRequest = serde_json::from_value(json!({
        "origin": "human",
        "operation": "send",
        "prompt": "what colour is the image?",
    }))?;
    request.attachments = attachments;
    let result = node.submit_command(instance, request).await?;
    Ok(serde_json::to_value(result)?)
}

/// Pull the durable settlement error for an accepted command.
///
/// Since the fast-ack change, the RPC returns as soon as the command is
/// durably accepted; attachment materialization happens in the instance
/// worker and settles the command afterwards. A failure reads back here.
async fn rejection_message(node: &DevNode, command_id: &str) -> Result<String> {
    for _ in 0..100 {
        if let Some(command) = node
            .get_command(&command_id.parse()?)
            .ok()
            .filter(|command| command.state == CommandState::Settled)
        {
            return match command.settlement {
                remuda_protocol::Knowledge::Known { value } => Ok(value
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "command settled without an error".to_owned())),
                _ => bail!("settled command carried no settlement"),
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    bail!("command {command_id} never settled")
}

/// The happy path: bytes land under the instance, named from the object id.
#[tokio::test]
async fn a_send_pulls_its_attachments_to_the_instance_directory() -> Result<()> {
    let fixture = fixture()?;
    let objects = FakeObjects::with(&[("obj_red", png(512)), ("obj_blue", png(256))]);
    fixture.node.set_object_source(objects.clone());
    let instance = create_instance(&fixture.node).await?;

    send(
        &fixture.node,
        &instance,
        vec![
            attachment("obj_red", "image/png"),
            attachment("obj_blue", "image/jpeg"),
        ],
    )
    .await?;

    let dir = remuda_node::attachments_dir(&fixture.data_dir, &instance);
    let red = dir.join("obj_red.png");
    // The extension follows the media type, not the sender's claimed name.
    let blue = dir.join("obj_blue.jpg");
    for _ in 0..100 {
        if red.is_file() && blue.is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(red.is_file(), "missing {}", red.display());
    assert!(blue.is_file(), "missing {}", blue.display());
    assert_eq!(std::fs::read(&red)?.len(), 512);
    assert_eq!(std::fs::read(&blue)?.len(), 256);
    assert_eq!(objects.fetches.load(Ordering::SeqCst), 2);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&red)?.permissions().mode() & 0o777,
            0o600,
            "an attachment must not be world readable"
        );
        assert_eq!(
            std::fs::metadata(&dir)?.permissions().mode() & 0o777,
            0o700,
            "the attachments directory must not be world readable"
        );
    }
    Ok(())
}

/// A failed pull must reject the send. Degrading to a text-only prompt would
/// leave the agent answering about an image it never got. The reject now
/// settles the accepted command asynchronously — fast ack first, worker-side
/// pull second.
#[tokio::test]
async fn a_failed_pull_rejects_the_whole_send() -> Result<()> {
    let fixture = fixture()?;
    let objects = FakeObjects::with(&[("obj_present", png(64))]);
    fixture.node.set_object_source(objects.clone());
    let instance = create_instance(&fixture.node).await?;

    let result = send(
        &fixture.node,
        &instance,
        vec![attachment("obj_missing", "image/png")],
    )
    .await?;
    let command_id = result["command"]["commandId"].as_str().unwrap();
    let message = rejection_message(&fixture.node, command_id).await?;
    assert!(
        message.contains("obj_missing"),
        "{message} should name the object"
    );

    *objects.fail.lock().expect("lock") = Some("hub unreachable".into());
    let result = send(
        &fixture.node,
        &instance,
        vec![attachment("obj_present", "image/png")],
    )
    .await?;
    let command_id = result["command"]["commandId"].as_str().unwrap();
    let message = rejection_message(&fixture.node, command_id).await?;
    assert!(message.contains("hub unreachable"), "{message}");

    // Nothing partial is left behind for the caller to trip over.
    let dir = remuda_node::attachments_dir(&fixture.data_dir, &instance);
    let staged = std::fs::read_dir(&dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(staged, 0, "a failed send must not leave a partial file");
    Ok(())
}

/// Unsupported types never reach the disk, even if a Hub somehow offered one.
#[tokio::test]
async fn unsupported_media_types_are_refused_before_any_fetch() -> Result<()> {
    let fixture = fixture()?;
    let objects = FakeObjects::with(&[("obj_doc", b"%PDF-1.7".to_vec())]);
    fixture.node.set_object_source(objects.clone());
    let instance = create_instance(&fixture.node).await?;

    let result = send(
        &fixture.node,
        &instance,
        vec![attachment("obj_doc", "application/pdf")],
    )
    .await?;
    let command_id = result["command"]["commandId"].as_str().unwrap();
    let message = rejection_message(&fixture.node, command_id).await?;
    assert!(message.contains("unsupported media type"), "{message}");
    assert_eq!(
        objects.fetches.load(Ordering::SeqCst),
        0,
        "the type is checked before the bytes are pulled"
    );
    Ok(())
}

/// Without a Hub link there is nowhere to pull from, so the send is rejected
/// rather than quietly stripped of its images.
#[tokio::test]
async fn a_node_with_no_object_source_refuses_attachments_but_still_sends_text() -> Result<()> {
    let fixture = fixture()?;
    let instance = create_instance(&fixture.node).await?;

    let result = send(
        &fixture.node,
        &instance,
        vec![attachment("obj_x", "image/png")],
    )
    .await?;
    let command_id = result["command"]["commandId"].as_str().unwrap();
    let message = rejection_message(&fixture.node, command_id).await?;
    assert!(message.contains("attachment source"), "{message}");

    // A text-only send on the same instance is unaffected.
    send(&fixture.node, &instance, Vec::new()).await?;
    Ok(())
}

#[tokio::test]
async fn more_attachments_than_the_cap_are_refused() -> Result<()> {
    let fixture = fixture()?;
    let entries: Vec<(String, Vec<u8>)> = (0..5)
        .map(|index| (format!("obj_{index}"), png(32)))
        .collect();
    let borrowed: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(id, body)| (id.as_str(), body.clone()))
        .collect();
    fixture.node.set_object_source(FakeObjects::with(&borrowed));
    let instance = create_instance(&fixture.node).await?;

    let refs: Vec<AttachmentRef> = entries
        .iter()
        .map(|(id, _)| attachment(id, "image/png"))
        .collect();
    let result = send(&fixture.node, &instance, refs).await?;
    let command_id = result["command"]["commandId"].as_str().unwrap();
    let message = rejection_message(&fixture.node, command_id).await?;
    assert!(message.contains("per-message limit"), "{message}");
    Ok(())
}

/// Closing an instance takes its attachments with it.
#[tokio::test]
async fn closing_an_instance_removes_its_attachments() -> Result<()> {
    let fixture = fixture()?;
    fixture
        .node
        .set_object_source(FakeObjects::with(&[("obj_keep", png(128))]));
    let instance = create_instance(&fixture.node).await?;
    send(
        &fixture.node,
        &instance,
        vec![attachment("obj_keep", "image/png")],
    )
    .await?;

    let dir = remuda_node::attachments_dir(&fixture.data_dir, &instance);
    for _ in 0..100 {
        if dir.join("obj_keep.png").is_file() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(dir.join("obj_keep.png").is_file());

    let close: remuda_node::InstanceCommandRequest = serde_json::from_value(json!({
        "origin": "human",
        "operation": "close",
    }))?;
    fixture.node.submit_command(&instance, close).await?;

    // The worker removes the directory as it unwinds.
    for _ in 0..100 {
        if !dir.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        !dir.exists(),
        "a closed instance must not leave attachments at {}",
        dir.display()
    );
    Ok(())
}

/// A hard kill leaves directories behind; the sweeper is what reclaims them.
#[test]
fn the_sweeper_removes_directories_for_instances_that_are_gone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().to_path_buf();
    let live = InstanceId::new();
    let dead = InstanceId::new();
    for id in [&live, &dead] {
        let attachments = remuda_node::attachments_dir(&data_dir, id);
        std::fs::create_dir_all(&attachments)?;
        std::fs::write(attachments.join("obj_a.png"), png(16))?;
    }

    let mut keep = std::collections::BTreeSet::new();
    keep.insert(live.as_id().as_str().to_owned());
    let removed = remuda_node::sweep_attachment_orphans(&data_dir, &keep)?;
    assert_eq!(removed, 1);
    assert!(remuda_node::attachments_dir(&data_dir, &live).is_dir());
    assert!(!remuda_node::attachments_dir(&data_dir, &dead).is_dir());

    // Re-running is a no-op rather than an error.
    assert_eq!(remuda_node::sweep_attachment_orphans(&data_dir, &keep)?, 0);
    Ok(())
}

/// Paths are always rebuilt from the object id, so a returned path cannot
/// escape the instance directory.
#[tokio::test]
async fn a_traversal_shaped_object_id_never_becomes_a_path() -> Result<()> {
    let fixture = fixture()?;
    fixture
        .node
        .set_object_source(FakeObjects::with(&[("../../escape", png(16))]));
    let instance = create_instance(&fixture.node).await?;

    let result = send(
        &fixture.node,
        &instance,
        vec![attachment("../../escape", "image/png")],
    )
    .await?;
    let command_id = result["command"]["commandId"].as_str().unwrap();
    let message = rejection_message(&fixture.node, command_id).await?;
    assert!(message.contains("bare identifier"), "{message}");
    assert!(
        !fixture
            .data_dir
            .parent()
            .map(|p| p.join("escape.png").exists())
            .unwrap_or(false),
        "nothing may be written outside the instance directory"
    );
    Ok(())
}

/// Every materialized attachment reports the path the drivers will use.
#[tokio::test]
async fn materialized_paths_are_absolute_and_inside_the_instance_directory() -> Result<()> {
    let fixture = fixture()?;
    let source: Arc<dyn ObjectSource> = FakeObjects::with(&[("obj_one", png(48))]);
    let instance = InstanceId::new();
    let materialized: Vec<MaterializedAttachment> = remuda_node::materialize_attachments(
        &source,
        &fixture.data_dir,
        &instance,
        &[attachment("obj_one", "image/png")],
    )
    .await?;
    let first = materialized.first().context("one attachment")?;
    assert_eq!(first.object_id, "obj_one");
    assert_eq!(first.media_type, "image/png");
    assert_eq!(first.byte_len, 48);
    assert!(first.path.is_absolute() || first.path.starts_with(&fixture.data_dir));
    assert!(
        first
            .path
            .starts_with(remuda_node::attachments_dir(&fixture.data_dir, &instance)),
        "{} must live under the instance directory",
        first.path.display()
    );
    Ok(())
}
