//! Source: `protocol.md` §§1.3, 4.1, 7.2, 7.4, 9.1, 12; all data are synthetic fixtures.

use remuda_protocol::*;
use serde_json::{Value, json};
use std::path::Path;

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn background_job_and_herdr_identity_do_not_require_a_known_claude_session() {
    let mut value = fixture("native-bg-herdr.json");
    value["sessionId"] = json!({"state":"unknown","reason":"not-emitted","evidenceEventIds":[]});
    value.as_object_mut().unwrap().remove("claude");
    let native: NativeRef = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(native.session_id, Knowledge::Unknown { .. }));
    assert_eq!(
        native.claude_bg.as_ref().unwrap().job_id,
        "native-job-fixture-1"
    );
    assert_eq!(native.herdr.as_ref().unwrap().session, "remuda-test");
    assert_eq!(native.herdr.as_ref().unwrap().pane_id, "w5:p1");
    assert_eq!(serde_json::to_value(native).unwrap(), value);
    for field in [
        "session",
        "paneId",
        "serverEpoch",
        "binaryPath",
        "digest",
        "protocolVersion",
    ] {
        let mut incomplete = value.clone();
        incomplete["herdr"].as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<NativeRef>(incomplete).is_err(),
            "{field}"
        );
    }
    let mut wrong = value;
    wrong["herdr"]["representation"] = json!("pty-bytes");
    assert!(serde_json::from_value::<NativeRef>(wrong).is_err());
    assert!(
        serde_json::from_value::<ClaudeRef>(
            json!({"sessionId":"native-session","backgroundJobId":"legacy-job"})
        )
        .is_err()
    );
}

#[test]
fn opening_background_terminal_requires_a_distinct_explicit_wake_command() {
    let value = fixture("open-terminal.json");
    let request: RpcRequest = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(request.call.method(), MethodName::InstanceOpenTerminal);
    assert_eq!(
        serde_json::to_value(CommandOperation::InstanceOpenTerminal).unwrap(),
        "instance.open_terminal"
    );
    let MethodCall::InstanceOpenTerminal(command) = request.call else {
        panic!("wrong command type")
    };
    assert_eq!(command.payload.background_job_id, "native-job-fixture-1");
    assert_eq!(command.payload.carrier.backend, PtyBackend::Herdr);
    for wake in [Value::Null, json!(false), json!("true")] {
        let mut invalid = value.clone();
        invalid["params"]["payload"]["allowWake"] = wake;
        assert!(serde_json::from_value::<RpcRequest>(invalid).is_err());
    }
    let mut missing = value;
    missing["params"]["payload"]
        .as_object_mut()
        .unwrap()
        .remove("allowWake");
    assert!(serde_json::from_value::<RpcRequest>(missing).is_err());
    let requests = fixture("requests.json");
    let read_attach = requests
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "tty.attach")
        .unwrap();
    let mut injected = read_attach.clone();
    injected["params"]["allowWake"] = json!(true);
    assert!(serde_json::from_value::<RpcRequest>(injected).is_err());
    assert_eq!(
        serde_json::from_value::<RpcRequest>(read_attach.clone())
            .unwrap()
            .call
            .method(),
        MethodName::TtyAttach
    );
}

#[test]
fn bg_carrier_and_capability_evidence_preserve_the_selected_transport() {
    let mut bg = fixture("claude-bg-carrier.json");
    let carrier: CarrierSpec = serde_json::from_value(bg.clone()).unwrap();
    assert!(matches!(carrier, CarrierSpec::ClaudeBg(_)));
    bg["inputDelivery"] = json!("stdio");
    assert!(serde_json::from_value::<CarrierSpec>(bg).is_err());
    let mut pty = fixture("herdr-carrier.json");
    pty["backend"] = json!("node");
    assert!(serde_json::from_value::<CarrierSpec>(pty).is_err());
    assert!(
        serde_json::from_value::<CarrierSpec>(
            json!({"type":"stdio","argvInputPolicy":"explicit-non-secret"})
        )
        .is_err()
    );
    let mut caps = fixture("instance.json")["capabilities"].clone();
    let rust: CapabilitySnapshot = serde_json::from_value(caps.clone()).unwrap();
    caps["adapterTransport"] = json!("claude-sdk-sidecar");
    let sidecar: CapabilitySnapshot = serde_json::from_value(caps.clone()).unwrap();
    assert_ne!(rust, sidecar);
    caps.as_object_mut().unwrap().remove("adapterTransport");
    assert!(serde_json::from_value::<CapabilitySnapshot>(caps).is_err());
}

#[test]
fn unknown_recovery_states_and_m0_error_codes_are_fixed() {
    for value in ["unknown", "reconciling"] {
        let wire = json!(value);
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<ResolutionState>(wire.clone()).unwrap())
                .unwrap(),
            wire
        );
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<RunState>(wire.clone()).unwrap())
                .unwrap(),
            wire
        );
        assert_eq!(
            serde_json::to_value(
                serde_json::from_value::<InstanceLifecycle>(wire.clone()).unwrap()
            )
            .unwrap(),
            wire
        );
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<InteractionState>(wire.clone()).unwrap())
                .unwrap(),
            wire
        );
    }
    for obsolete in [
        "unknown",
        "dispatch_unknown",
        "decision_unknown",
        "completed",
    ] {
        assert!(serde_json::from_value::<CommandState>(json!(obsolete)).is_err());
    }
    assert_eq!(M0_REQUIRED_ERROR_CODES.len(), 24);
    for (code, number) in [
        (ErrorCode::OwnerFenced, -32003),
        (ErrorCode::AttachWouldWake, -32022),
        (ErrorCode::CommandOutcomeUnknown, -32026),
        (ErrorCode::NativeResponseUnknown, -32034),
        (ErrorCode::StateUnknown, -32039),
        (ErrorCode::TtyHistoryGap, -32042),
    ] {
        assert!(M0_REQUIRED_ERROR_CODES.contains(&code));
        assert_eq!(code.rpc_code(), number);
    }
    let mut command = fixture("command.json");
    command["dispatch"] = json!("intent-durable");
    command["resolution"] = json!("unknown");
    let decoded: Command = serde_json::from_value(command.clone()).unwrap();
    assert_eq!(decoded.state, CommandState::Queued);
    assert_eq!(serde_json::to_value(decoded).unwrap(), command);
}

#[test]
fn binary_header_preserves_uuid_big_endian_offset_and_split_utf8() {
    let header: BinaryHeader = serde_json::from_value(fixture("binary-header.json")).unwrap();
    let expected = [
        1, 1, 0, 0, 1, 153, 58, 176, 0, 0, 112, 0, 128, 0, 0, 0, 0, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0,
        1, 0, 0, 0, 3,
    ];
    assert_eq!(header.encode().unwrap(), expected);
    // A UTF-8 prefix and ANSI escape may cross frames; framing does not decode either.
    let payload = [0xf0, 0x9f, 0x1b];
    let mut frame = expected.to_vec();
    frame.extend_from_slice(&payload);
    let (decoded, bytes) = decode_binary_frame(&frame, 3).unwrap();
    assert_eq!(decoded, header);
    assert_eq!(bytes, payload);
    frame[1] = 2;
    assert_eq!(
        decode_binary_frame(&frame, 3).unwrap().0.channel,
        BinaryChannel::ObjectChunk
    );
    frame[1] = 3;
    assert_eq!(
        decode_binary_frame(&frame, 3).unwrap().0.channel,
        BinaryChannel::TtyInput
    );
}

#[test]
fn binary_ingress_rejects_invalid_lengths_versions_ids_and_ranges() {
    let header: BinaryHeader = serde_json::from_value(fixture("binary-header.json")).unwrap();
    let mut frame = header.encode().unwrap().to_vec();
    frame.extend_from_slice(b"abc");
    for length in 0..BINARY_HEADER_LEN {
        assert_eq!(
            decode_binary_frame(&frame[..length], 3).unwrap_err(),
            BinaryFrameError::Truncated
        );
    }
    assert_eq!(
        decode_binary_frame(&frame, 2).unwrap_err(),
        BinaryFrameError::PayloadLimit
    );
    assert_eq!(
        decode_binary_frame(&frame[..34], 3).unwrap_err(),
        BinaryFrameError::LengthMismatch
    );
    let mut extra = frame.clone();
    extra.push(0);
    assert_eq!(
        decode_binary_frame(&extra, 3).unwrap_err(),
        BinaryFrameError::LengthMismatch
    );
    for (index, value) in [(0, 2), (1, 4), (2, 1), (3, 1)] {
        let mut bad = frame.clone();
        bad[index] = value;
        assert_eq!(
            decode_binary_frame(&bad, 3).unwrap_err(),
            BinaryFrameError::UnsupportedHeader
        );
    }
    let mut bad = frame.clone();
    bad[10] = 0x40;
    assert_eq!(
        decode_binary_frame(&bad, 3).unwrap_err(),
        BinaryFrameError::InvalidStream
    );
    let mut bad = frame;
    bad[20..28].copy_from_slice(&u64::MAX.to_be_bytes());
    assert_eq!(
        decode_binary_frame(&bad, 3).unwrap_err(),
        BinaryFrameError::OffsetOverflow
    );
    let overflow = BinaryHeader {
        offset: U64(u64::MAX),
        ..header
    };
    assert_eq!(
        overflow.encode().unwrap_err(),
        BinaryFrameError::OffsetOverflow
    );
}
