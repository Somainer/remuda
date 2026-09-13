//! Real PTY mouse-sequence round-trip on `shell-pty`.

use remuda_node::{DevNode, DevServerConfig};
use remuda_protocol::{AgentKind, DriverKind};
use serde_json::json;
use std::time::Duration;

const MOUSE_ECHO: &str = r#"
import os, tty
tty.setraw(0)
os.write(1, b"\x1b[?1000h\x1b[?1002h\x1b[?1006h")
while True:
    chunk = os.read(0, 4096)
    if not chunk:
        break
    os.write(1, chunk)
"#;

const SGR_PRESS: &[u8] = b"\x1b[<0;10;20M";

#[tokio::test]
async fn mouse_escape_round_trips_on_shell_pty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = DevServerConfig::loopback(0);
    config.workspace_root = dir.path().to_path_buf();
    config.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let node = DevNode::new(&config).expect("node");
    let created = node
        .create_instance(
            serde_json::from_value(json!({
                "kind": "terminal",
                "driver": "shell-pty",
                "args": ["python3", "-u", "-c", MOUSE_ECHO],
                "prompt": ""
            }))
            .expect("request"),
        )
        .await
        .expect("create shell-pty");
    let instance_id = created.instance.meta.id.clone();

    let attached = wait_attach(&node, &instance_id).await;
    assert!(
        attached
            .snapshot
            .windows(SGR_PRESS.len().min(6))
            .any(|w| w == b"\x1b[?100" || w.starts_with(b"\x1b[?")),
        "DECSET mouse tracking should appear in snapshot: {:?}",
        String::from_utf8_lossy(&attached.snapshot)
    );

    let mut events = node.tty().subscribe();
    node.tty()
        .write_bytes(&instance_id, SGR_PRESS)
        .await
        .expect("write mouse sequence");

    let mut collected = attached.snapshot.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(remuda_node::TtyEvent::Bytes { payload, .. })) => {
                collected.extend_from_slice(&payload);
                if collected.windows(SGR_PRESS.len()).any(|w| w == SGR_PRESS) {
                    return;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    panic!(
        "mouse sequence did not round-trip; collected={:?}",
        String::from_utf8_lossy(&collected)
    );
}

#[tokio::test]
async fn snapshot_on_attach_replays_prior_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = DevServerConfig::loopback(0);
    config.workspace_root = dir.path().to_path_buf();
    config.workspace_roots = Some(remuda_testing::test_workspace_roots!());
    let node = DevNode::new(&config).expect("node");
    let created = node
        .create_instance(
            serde_json::from_value(json!({
                "kind": "terminal",
                "driver": "shell-pty",
                "args": ["python3", "-u", "-c", MOUSE_ECHO],
                "prompt": ""
            }))
            .expect("request"),
        )
        .await
        .expect("create");
    let instance_id = created.instance.meta.id.clone();
    let first = wait_attach(&node, &instance_id).await;
    node.tty()
        .write_bytes(&instance_id, b"hello-snapshot")
        .await
        .expect("write");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut second = first;
    while tokio::time::Instant::now() < deadline {
        second = node.tty().attach(&instance_id).await.expect("reattach");
        if second
            .snapshot
            .windows(b"hello-snapshot".len())
            .any(|w| w == b"hello-snapshot")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "reattach snapshot missing hello-snapshot: {:?}",
        String::from_utf8_lossy(&second.snapshot)
    );
}

async fn wait_attach(
    node: &DevNode,
    instance_id: &remuda_protocol::InstanceId,
) -> remuda_node::TtyAttach {
    let _ = (AgentKind::Terminal, DriverKind::ShellPty);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match node.tty().attach(instance_id).await {
            Ok(attached) if !attached.snapshot.is_empty() || attached.next_offset > 0 => {
                return attached;
            }
            _ => tokio::time::sleep(Duration::from_millis(40)).await,
        }
    }
    node.tty()
        .attach(instance_id)
        .await
        .expect("tty bridge never started")
}
