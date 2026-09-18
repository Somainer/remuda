//! Hub↔Node operational wire fixtures (`hubnode` module).

use remuda_protocol::hubnode::{
    self, HubNodeMethod, HubNodeRequest, InstanceCreateParams, JournalAppendParams,
    METHOD_INSTANCE_CREATE, METHOD_INSTANCE_KEYS, METHOD_JOURNAL_APPEND, METHOD_NODE_AUTH,
    METHOD_NODE_HELLO, METHOD_TTY_ATTACH, METHOD_TTY_FRAME, METHOD_TTY_RESIZE, METHOD_TTY_WRITE,
    NodeAuthParams, NodeHelloParams, TTY_BINARY_HEADER_LEN, TtyBinaryEnvelopeSpec, TtyFrameParams,
    TtyResizeParams, TtyWriteParams,
};
use remuda_protocol::{BINARY_HEADER_LEN, PROTOCOL_VERSION, from_json_slice};
use serde_json::{Value, json};
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
    assert_eq!(mode.alt_screen, Some(false));
    assert_eq!(mode.progress, None);
    assert_eq!(serde_json::to_value(mode).unwrap(), value);
    value["altScreen"] = Value::Null;
    assert!(serde_json::from_value::<TtyModeParams>(value.clone()).is_err());
    value["altScreen"] = json!(true);
    value.as_object_mut().unwrap().remove("streamId");
    assert!(serde_json::from_value::<TtyModeParams>(value).is_err());
}

#[test]
fn renderer_mode_carries_osc94_progress_as_an_independent_additive_field() {
    use remuda_protocol::hubnode::{METHOD_TTY_MODE, TtyModeParams};
    use serde_json::json;

    let _ = METHOD_TTY_MODE;
    // A progress-only edge omits altScreen entirely.
    let edge = json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "streamId": "tty_01993ab0-0000-7000-8000-000000000007",
        "progress": {"state": "indeterminate"}
    });
    let mode: TtyModeParams = serde_json::from_value(edge.clone()).unwrap();
    assert_eq!(mode.alt_screen, None);
    assert_eq!(
        mode.progress.unwrap().state,
        remuda_protocol::hubnode::TtyProgressState::Indeterminate
    );
    assert_eq!(serde_json::to_value(mode).unwrap(), edge);

    // Percent rides alongside; malformed states are rejected.
    let mode: TtyModeParams = serde_json::from_value(json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "streamId": "tty_01993ab0-0000-7000-8000-000000000007",
        "altScreen": false,
        "progress": {"state": "percent", "percent": 50}
    }))
    .unwrap();
    assert_eq!(mode.alt_screen, Some(false));
    let progress = mode.progress.unwrap();
    assert_eq!(
        progress.state,
        remuda_protocol::hubnode::TtyProgressState::Percent
    );
    assert_eq!(progress.percent, Some(50));
    for bad in [
        json!({"state": "napping"}),
        json!({"percent": 10}),
        json!("3"),
    ] {
        assert!(
            serde_json::from_value::<TtyModeParams>(json!({
                "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
                "streamId": "tty_01993ab0-0000-7000-8000-000000000007",
                "progress": bad
            }))
            .is_err()
        );
    }
}

// ── api.* stream class (D-048, 2026-09-19) ─────────────────────────────────

/// The seven `api.*` frames exist, parse, and are recognized as a class.
///
/// They are notifications in practice, but the parser has one entry point for
/// both frame kinds, so `parse`/`as_str` must round-trip each one regardless.
#[test]
fn api_method_constants_parse_and_round_trip() {
    use remuda_protocol::hubnode::{
        METHOD_API_BODY, METHOD_API_CANCEL, METHOD_API_CHUNK, METHOD_API_CREDIT, METHOD_API_END,
        METHOD_API_HEAD, METHOD_API_OPEN,
    };
    for (method, kind) in [
        (METHOD_API_OPEN, HubNodeMethod::ApiOpen),
        (METHOD_API_BODY, HubNodeMethod::ApiBody),
        (METHOD_API_HEAD, HubNodeMethod::ApiHead),
        (METHOD_API_CHUNK, HubNodeMethod::ApiChunk),
        (METHOD_API_END, HubNodeMethod::ApiEnd),
        (METHOD_API_CANCEL, HubNodeMethod::ApiCancel),
        (METHOD_API_CREDIT, HubNodeMethod::ApiCredit),
    ] {
        assert_eq!(HubNodeMethod::parse(method), Some(kind));
        assert_eq!(kind.as_str(), method);
        assert!(kind.is_api(), "{method} must be in the api.* class");
        // The class exists so a carrier routes these to the stream registry
        // instead of the RPC table; that separation is the whole D-048 point.
        assert!(
            !kind.is_instance(),
            "{method} must not be dispatched as instance control"
        );
        assert!(!kind.is_hello() && !kind.is_auth());
    }
    // A non-api method is not in the class.
    assert!(!HubNodeMethod::ObjectPull.is_api());
    assert!(!HubNodeMethod::InstanceCreate.is_api());
}

#[test]
fn api_open_round_trips_with_and_without_a_body() {
    use remuda_protocol::hubnode::{ApiHeader, ApiOpenParams};

    let inline = json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "streamId": "stream-1",
        "method": "POST",
        "path": "/v1/messages",
        "query": "beta=true",
        "headers": [
            {"name": "content-type", "value": "application/json"},
            {"name": "anthropic-version", "value": "2023-06-01"}
        ],
        "bodyBase64": "eyJtb2RlbCI6ImNsYXVkZSJ9",
        "bodyChunked": false,
        "deadlineMs": 30000
    });
    let open: ApiOpenParams = serde_json::from_value(inline.clone()).unwrap();
    assert_eq!(open.method, "POST");
    assert_eq!(open.path, "/v1/messages");
    assert_eq!(open.query, "beta=true");
    assert_eq!(open.headers.len(), 2);
    assert_eq!(
        open.headers[0],
        ApiHeader {
            name: "content-type".into(),
            value: "application/json".into()
        }
    );
    assert!(open.body_base64.is_some());
    assert!(!open.body_chunked);
    assert_eq!(open.deadline_ms, 30_000);
    assert_eq!(serde_json::to_value(&open).unwrap(), inline);

    // A GET with no body omits both body fields rather than sending nulls.
    // `query` stays explicit: an empty string is the wire's "no query string".
    let get = json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "streamId": "stream-1",
        "method": "GET",
        "path": "/v1/models",
        "query": "",
        "bodyChunked": false,
        "deadlineMs": 10000
    });
    let open: ApiOpenParams = serde_json::from_value(get.clone()).unwrap();
    assert_eq!(open.query, "");
    assert!(open.headers.is_empty());
    assert_eq!(open.body_base64, None);
    assert_eq!(serde_json::to_value(&open).unwrap(), get);
}

/// A header list, not a map: HTTP allows repeats and folding them would change
/// the request the gateway sees.
#[test]
fn api_headers_keep_repeated_names() {
    use remuda_protocol::hubnode::ApiOpenParams;

    let value = json!({
        "instanceId": "ins_01993ab0-0000-7000-8000-000000000006",
        "streamId": "stream-1",
        "method": "POST",
        "path": "/v1/messages",
        "query": "",
        "headers": [
            {"name": "x-stainless-retry-count", "value": "0"},
            {"name": "x-stainless-retry-count", "value": "1"}
        ],
        "bodyChunked": true,
        "deadlineMs": 1000
    });
    let open: ApiOpenParams = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(open.headers.len(), 2);
    assert!(open.body_chunked);
    assert_eq!(serde_json::to_value(&open).unwrap(), value);
}

#[test]
fn api_body_head_chunk_cancel_and_credit_round_trip() {
    use remuda_protocol::hubnode::{
        ApiBodyParams, ApiCancelParams, ApiChunkParams, ApiCreditParams, ApiHeadParams,
    };

    let body = json!({"streamId": "s1", "seq": 0, "dataBase64": "AAEC", "last": true});
    let parsed: ApiBodyParams = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(parsed.seq, 0);
    assert!(parsed.last);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), body);

    let head = json!({
        "streamId": "s1",
        "status": 200,
        "headers": [{"name": "content-type", "value": "text/event-stream"}]
    });
    let parsed: ApiHeadParams = serde_json::from_value(head.clone()).unwrap();
    assert_eq!(parsed.status, 200);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), head);

    // `set-cookie` is dropped by the egress allowlist, not by this type; what
    // matters here is that a header list survives intact either way.
    let chunk = json!({"streamId": "s1", "seq": 3, "dataBase64": "ZXZlbnQ6", "last": false});
    let parsed: ApiChunkParams = serde_json::from_value(chunk.clone()).unwrap();
    assert_eq!(parsed.seq, 3);
    assert!(!parsed.last);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), chunk);

    let cancel = json!({"streamId": "s1", "reason": "client-disconnect"});
    let parsed: ApiCancelParams = serde_json::from_value(cancel.clone()).unwrap();
    assert_eq!(parsed.reason, "client-disconnect");
    assert_eq!(serde_json::to_value(&parsed).unwrap(), cancel);

    let credit = json!({"streamId": "s1", "chunks": 4});
    let parsed: ApiCreditParams = serde_json::from_value(credit.clone()).unwrap();
    assert_eq!(parsed.chunks, 4);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), credit);
}

/// `api.end` is the audit frame: counters always, error only on failure, and
/// it must never be a carrier for a body or a header.
#[test]
fn api_end_carries_counters_and_an_optional_error_only() {
    use remuda_protocol::hubnode::{
        API_ERROR_CANCELLED, API_ERROR_HUB_LINK_LOST, API_ERROR_VIA_HOST_OFFLINE, ApiEndError,
        ApiEndParams,
    };

    let ok = json!({"streamId": "s1", "bytesUp": 128, "bytesDown": 4096, "ms": 812});
    let parsed: ApiEndParams = serde_json::from_value(ok.clone()).unwrap();
    assert_eq!(parsed.error, None);
    assert_eq!(parsed.bytes_up, 128);
    assert_eq!(parsed.bytes_down, 4096);
    assert_eq!(parsed.ms, 812);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), ok);

    let failed = json!({
        "streamId": "s1",
        "error": {"code": api_error_offline(), "message": "proxy host went offline"},
        "bytesUp": 0,
        "bytesDown": 2048,
        "ms": 5100
    });
    let parsed: ApiEndParams = serde_json::from_value(failed.clone()).unwrap();
    assert_eq!(
        parsed.error,
        Some(ApiEndError {
            code: "via-host-offline".into(),
            message: "proxy host went offline".into()
        })
    );
    assert_eq!(serde_json::to_value(&parsed).unwrap(), failed);

    // The codes are the stable vocabulary a listener maps to an HTTP error.
    assert_eq!(API_ERROR_VIA_HOST_OFFLINE, "via-host-offline");
    assert_eq!(API_ERROR_HUB_LINK_LOST, "hub-link-lost");
    assert_eq!(API_ERROR_CANCELLED, "cancelled");

    // No frame in this class may carry a body or a header past `api.end`.
    let wire = serde_json::to_string(&parsed).unwrap().to_lowercase();
    for forbidden in ["authorization", "x-api-key", "set-cookie", "bodybase64"] {
        assert!(!wire.contains(forbidden), "api.end leaked {forbidden}");
    }
}

fn api_error_offline() -> String {
    remuda_protocol::hubnode::API_ERROR_VIA_HOST_OFFLINE.to_string()
}

/// The relay frames are notifications: a carrier that framed one as a request
/// would put it in the 32-slot pending map D-048 exists to keep it out of.
#[test]
fn api_frames_are_stateless_notifications() {
    use remuda_protocol::hubnode::{ApiChunkParams, METHOD_API_CHUNK};

    let frame = HubNodeRequest::notification(
        METHOD_API_CHUNK,
        serde_json::to_value(ApiChunkParams {
            stream_id: "s1".into(),
            seq: 0,
            data_base64: "AA==".into(),
            last: true,
        })
        .unwrap(),
    );
    let value = serde_json::to_value(&frame).unwrap();
    assert!(value.get("id").is_none(), "api.* frames carry no id");
    assert_eq!(value["method"], json!(METHOD_API_CHUNK));
    assert_eq!(
        frame.method_kind(),
        Some(HubNodeMethod::ApiChunk),
        "the api.* class is reachable from a frame"
    );
}
