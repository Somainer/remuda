//! Hub↔Node operational wire fixtures (`hubnode` module).

use remuda_protocol::hubnode::{
    self, HubNodeMethod, HubNodeRequest, InstanceCreateParams, JournalAppendParams,
    METHOD_INSTANCE_CREATE, METHOD_INSTANCE_KEYS, METHOD_JOURNAL_APPEND, METHOD_NODE_AUTH,
    METHOD_NODE_HELLO, METHOD_TTY_ATTACH, METHOD_TTY_FRAME, METHOD_TTY_RESIZE, METHOD_TTY_WRITE,
    NodeAuthParams, NodeHelloParams, TTY_BINARY_HEADER_LEN, TtyBinaryEnvelopeSpec, TtyFrameParams,
    TtyResizeParams, TtyWriteParams,
};
use remuda_protocol::{BINARY_HEADER_LEN, PROTOCOL_VERSION, from_json_slice};
use serde_json::Value;
use std::path::Path;

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn auth_hello_journal_tty_fixtures_round_trip() {
    let auth = HubNodeRequest::from_value(&fixture("hubnode-auth.json")).unwrap();
    assert_eq!(auth.method, METHOD_NODE_AUTH);
    assert_eq!(auth.version, Some(PROTOCOL_VERSION));
    let auth_params: NodeAuthParams = serde_json::from_value(auth.params.clone().unwrap()).unwrap();
    assert_eq!(auth_params.token, "bootstrap-token");
    assert_eq!(auth_params.scheme.as_deref(), Some("bearer"));

    let hello = HubNodeRequest::from_value(&fixture("hubnode-hello.json")).unwrap();
    assert_eq!(hello.method_kind(), Some(HubNodeMethod::NodeHello));
    assert_eq!(hello.method, METHOD_NODE_HELLO);
    let hello_params: NodeHelloParams =
        serde_json::from_value(hello.params.clone().unwrap()).unwrap();
    assert_eq!(
        hello_params.persisted_host_id(),
        Some("hst_01993ab0-0000-7000-8000-000000000004")
    );
    assert_eq!(hello_params.transport.as_deref(), Some("ssh-stdio"));
    assert_eq!(hello_params.enrollment_token.as_deref(), Some("host-token"));

    let journal = HubNodeRequest::from_value(&fixture("hubnode-journal-batch.json")).unwrap();
    assert_eq!(journal.method, METHOD_JOURNAL_APPEND);
    let append: JournalAppendParams =
        serde_json::from_value(journal.params.clone().unwrap()).unwrap();
    assert_eq!(append.events_to_append().len(), 2);
    assert_eq!(append.seq_i64(), Some(2));

    let tty = HubNodeRequest::from_value(&fixture("hubnode-tty-frame.json")).unwrap();
    assert_eq!(tty.method, METHOD_TTY_FRAME);
    let tty_params: TtyFrameParams = serde_json::from_value(tty.params.clone().unwrap()).unwrap();
    assert_eq!(tty_params.channel, Some(1));
}

#[test]
fn instance_create_accepts_hub_forward_shape() {
    let value = serde_json::json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "spec": { "kind": "claude", "driver": "claude-print", "prompt": "hi" },
        "initialInput": { "type": "prompt", "text": "hi" }
    });
    let params: InstanceCreateParams = serde_json::from_value(value).unwrap();
    assert_eq!(
        params.instance_id.as_deref(),
        Some("ins_01993ab0-0000-7000-8000-000000000006")
    );
    assert!(
        HubNodeMethod::parse(METHOD_INSTANCE_CREATE)
            .unwrap()
            .is_instance()
    );
}

#[test]
fn tty_binary_envelope_is_32_bytes() {
    let spec = TtyBinaryEnvelopeSpec::v1();
    assert_eq!(spec.header_len as usize, BINARY_HEADER_LEN);
    assert_eq!(TTY_BINARY_HEADER_LEN, 32);
}

#[test]
fn tty_resize_and_attach_are_instance_methods() {
    assert_eq!(
        HubNodeMethod::parse(METHOD_TTY_RESIZE),
        Some(HubNodeMethod::TtyResize)
    );
    assert_eq!(
        HubNodeMethod::parse(METHOD_TTY_ATTACH),
        Some(HubNodeMethod::TtyAttach)
    );
    assert!(HubNodeMethod::TtyResize.is_instance());
    assert!(HubNodeMethod::TtyAttach.is_instance());
    let resize: TtyResizeParams = serde_json::from_value(serde_json::json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "cols": 120,
        "rows": 40
    }))
    .unwrap();
    assert_eq!(resize.cols, Some(120));
    assert_eq!(resize.rows, Some(40));
}

#[test]
fn tty_write_is_an_instance_method() {
    assert_eq!(
        HubNodeMethod::parse(METHOD_TTY_WRITE),
        Some(HubNodeMethod::TtyWrite)
    );
    assert_eq!(
        HubNodeMethod::parse(METHOD_INSTANCE_KEYS),
        Some(HubNodeMethod::InstanceKeys)
    );
    assert!(HubNodeMethod::TtyWrite.is_instance());
    let params: TtyWriteParams = serde_json::from_value(serde_json::json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "keys": ["enter", "esc"]
    }))
    .unwrap();
    assert_eq!(params.key_names(), vec!["enter", "esc"]);
}

#[test]
fn ws_bearer_and_stdio_auth_are_documented() {
    assert_eq!(hubnode::WS_AUTHORIZATION_SCHEME, "Bearer");
    assert_eq!(hubnode::METHOD_NODE_AUTH, "node.auth");
    assert_eq!(
        hubnode::bearer_from_authorization("Bearer secret"),
        Some("secret")
    );
}

#[test]
fn workspace_methods_require_explicit_mutation_phase() {
    use remuda_protocol::hubnode::{
        WorkspaceMutationParams, WorkspaceMutationPhase, WorkspaceRegistryResult,
    };
    for method in [
        "workspace.list",
        "workspace.register",
        "workspace.unregister",
    ] {
        let kind = HubNodeMethod::parse(method).unwrap();
        assert_eq!(kind.as_str(), method);
        assert!(!kind.is_instance());
    }
    let params: WorkspaceMutationParams = serde_json::from_value(serde_json::json!({
        "commandId":"cmd-workspace", "path":"/home/dev/project", "phase":"prepare"
    }))
    .unwrap();
    assert_eq!(params.phase, WorkspaceMutationPhase::Prepare);
    assert!(
        serde_json::from_value::<WorkspaceMutationParams>(serde_json::json!({
            "commandId":"cmd-workspace", "path":"/home/dev/project"
        }))
        .is_err()
    );
    let result: WorkspaceRegistryResult = serde_json::from_value(serde_json::json!({
        "workspaceRevision":2, "workspaces":[], "commandId":"cmd-workspace", "phase":"settled"
    }))
    .unwrap();
    assert_eq!(result.workspace_revision, 2);
    assert_eq!(result.phase.as_deref(), Some("settled"));
}

#[test]
fn renderer_mode_requires_observed_boolean_and_both_stream_identities() {
    use remuda_protocol::hubnode::{METHOD_TTY_MODE, TtyModeParams};
    use serde_json::json;

    assert_eq!(
        HubNodeMethod::parse(METHOD_TTY_MODE),
        Some(HubNodeMethod::TtyMode)
    );
    assert_eq!(HubNodeMethod::TtyMode.as_str(), "tty.mode");
    assert!(!HubNodeMethod::TtyMode.is_instance());
    let mut value = json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "streamId": "tty_01993ab0-0000-7000-8000-000000000007",
        "altScreen": false,
    });
    let mode: TtyModeParams = serde_json::from_value(value.clone()).unwrap();
    assert!(!mode.alt_screen);
    assert_eq!(serde_json::to_value(mode).unwrap(), value);
    value["altScreen"] = Value::Null;
    assert!(serde_json::from_value::<TtyModeParams>(value.clone()).is_err());
    value["altScreen"] = json!(true);
    value.as_object_mut().unwrap().remove("streamId");
    assert!(serde_json::from_value::<TtyModeParams>(value).is_err());
}
