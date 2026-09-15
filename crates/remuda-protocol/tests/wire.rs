//! Wire compatibility and rejection fixtures from `protocol.md` §§1–9, 12.

use remuda_protocol::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Debug};

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: Value) -> T {
    let decoded: T = from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    let wire = serde_json::to_value(&decoded).unwrap();
    assert_eq!(
        wire, value,
        "wire names, explicit nulls, and enum tags must survive"
    );
    let decoded_again: T = from_json_slice(&serde_json::to_vec(&decoded).unwrap()).unwrap();
    assert_eq!(decoded, decoded_again);
    decoded
}

#[test]
fn entities_preserve_independent_state_dimensions() {
    round_trip::<Host>(fixture("host.json"));
    round_trip::<Workspace>(fixture("workspace.json"));
    round_trip::<WorktreeRecord>(fixture("worktree.json"));
    let instance = round_trip::<Instance>(fixture("instance.json"));
    assert_eq!(instance.lifecycle, InstanceLifecycle::Unknown);
    assert_eq!(instance.connectivity, Connectivity::Disconnected);
    assert!(matches!(instance.activity, Knowledge::Unknown { .. }));
    let run = round_trip::<Run>(fixture("run.json"));
    assert_eq!(run.state, RunState::Unknown);
    assert_eq!(run.state_confidence, StateConfidence::Unknown);
    let command = round_trip::<Command>(fixture("command.json"));
    assert_eq!(command.state, CommandState::Queued);
    assert_eq!(command.dispatch, DispatchState::TransportWritten);
    assert_eq!(command.resolution, ResolutionState::Unknown);
    round_trip::<Interaction>(fixture("interaction.json"));
    round_trip::<InstanceSpec>(fixture("instance-spec.json"));
}

#[test]
fn every_observation_family_has_the_specified_discriminant() {
    let values = fixture("observations.json").as_array().unwrap().clone();
    let mut kinds = BTreeSet::new();
    for value in values {
        let observation = round_trip::<Observation>(value);
        kinds.insert(observation.body.kind());
    }
    assert_eq!(kinds.len(), 15);
    for value in fixture("lifecycle-entities.json").as_array().unwrap() {
        round_trip::<LifecyclePayload>(value.clone());
    }
    round_trip::<JournalEvent>(fixture("registry-event.json"));
}

#[test]
fn protocol_examples_and_all_methods_round_trip() {
    let hello = round_trip::<RpcRequest>(fixture("hello.json"));
    assert_eq!(hello.call.method(), MethodName::RuntimeHello);
    assert_eq!(PROTOCOL_VERSION, ProtocolVersion { major: 1, minor: 0 });
    round_trip::<InteractionRequest>(fixture("question-request.json"));
    let mut methods = BTreeSet::new();
    for value in fixture("requests.json").as_array().unwrap() {
        let request = round_trip::<RpcRequest>(value.clone());
        methods.insert(request.call.method());
    }
    assert_eq!(methods.len(), 46);
}

#[test]
fn request_unions_refuse_incompatible_operations() {
    let mut requests = fixture("requests.json");
    let requests = requests.as_array_mut().unwrap();
    let mut attach = requests
        .iter()
        .find(|r| r["method"] == "instance.attach")
        .unwrap()
        .clone();
    attach["params"]["payload"]["ref"]["allowWake"] = json!(true);
    assert!(serde_json::from_value::<RpcRequest>(attach).is_err());
    let mut close = requests
        .iter()
        .find(|r| r["method"] == "instance.close")
        .unwrap()
        .clone();
    close["params"]["payload"]["retainNativeSession"] = json!(false);
    assert!(serde_json::from_value::<RpcRequest>(close).is_err());
    let mut send = requests
        .iter()
        .find(|r| r["method"] == "instance.send")
        .unwrap()
        .clone();
    send["params"]["payload"]["input"] =
        json!({"type":"model-switch","modelId":"other","effective":"next-turn"});
    assert!(serde_json::from_value::<RpcRequest>(send).is_err());
    assert!(
        serde_json::from_value::<CodexExecution>(
            json!({"sandbox":"read-only","permissions":"both"})
        )
        .is_err()
    );
}

#[test]
fn unknown_tags_and_versions_do_not_become_success() {
    let mut message = fixture("observations.json")[0].clone();
    message["kind"] = json!("future.success");
    assert!(serde_json::from_value::<Observation>(message.clone()).is_err());
    assert!(serde_json::from_value::<JournalEvent>(message).is_err());
    let mut message = fixture("observations.json")[0].clone();
    message["schemaVersion"] = json!(2);
    assert!(serde_json::from_value::<Observation>(message).is_err());
    let mut request = fixture("hello.json");
    request["method"] = json!("instance.retry-everything");
    assert!(serde_json::from_value::<RpcRequest>(request).is_err());
    assert!(serde_json::from_value::<CommandState>(json!("unknown")).is_err());
    assert!(serde_json::from_value::<DriverKind>(json!("claude-future")).is_err());
    assert_eq!(
        serde_json::from_value::<DriverKind>(json!("claude-bg")).unwrap(),
        DriverKind::ClaudeBg
    );
}

#[test]
fn missing_nullable_fields_are_not_treated_as_known_absence() {
    let mut message = fixture("observations.json")[0].clone();
    message["runId"] = Value::Null;
    round_trip::<Observation>(message.clone());
    message.as_object_mut().unwrap().remove("runId");
    assert!(serde_json::from_value::<Observation>(message).is_err());
    let instance = fixture("instance.json");
    let mut caps = instance["capabilities"].clone();
    caps["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("resume");
    assert!(serde_json::from_value::<CapabilitySnapshot>(caps).is_err());
    for knowledge in [
        json!({"state":"known","value":0}),
        json!({"state":"unknown","reason":"not-emitted","evidenceEventIds":[]}),
        json!({"state":"not-applicable"}),
    ] {
        round_trip::<Knowledge<u32>>(knowledge);
    }
    assert!(serde_json::from_value::<Knowledge<u32>>(Value::Null).is_err());
}

#[test]
fn event_batches_require_one_contiguous_journal_without_duplicate_ids() {
    let notification = round_trip::<RpcNotification>(fixture("events-batch.json"));
    let NotificationBody::EventsBatch(batch) = notification.event;
    batch.validate().unwrap();
    let mut gap = serde_json::to_value(&batch).unwrap();
    gap["events"][1]["seq"] = json!("3");
    assert!(
        serde_json::from_value::<EventsBatch>(gap)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut wrong_journal = (*batch).clone();
    wrong_journal.journal_id = Id::new("obj").unwrap();
    assert!(wrong_journal.validate().is_err());
    let mut duplicate = serde_json::to_value(&batch).unwrap();
    duplicate["events"][1]["eventId"] = duplicate["events"][0]["eventId"].clone();
    assert!(
        serde_json::from_value::<EventsBatch>(duplicate)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut empty = serde_json::to_value(&batch).unwrap();
    empty["events"] = json!([]);
    assert!(serde_json::from_value::<EventsBatch>(empty).is_err());
    let mut watermark = (*batch).clone();
    watermark.durable_seq = U64(14);
    assert!(watermark.validate().is_err());
}

#[test]
fn rpc_responses_have_exactly_one_outcome() {
    round_trip::<RpcResponse<Value>>(json!({"jsonrpc":"2.0","id":"r1","result":null}));
    round_trip::<RpcResponse<Value>>(
        json!({"jsonrpc":"2.0","id":"r1","error":{"code":-32601,"message":"Unknown method"}}),
    );
    for value in [
        json!({"jsonrpc":"2.0","id":"r1"}),
        json!({"jsonrpc":"2.0","id":"r1","result":{},"error":{"code":-32601,"message":"No"}}),
    ] {
        assert!(serde_json::from_value::<RpcResponse<Value>>(value).is_err());
    }
    let error = RuntimeError {
        code: ErrorCode::CommandOutcomeUnknown,
        rpc_code: -32026,
        message: "Delivery is unknown".into(),
        retry: RetryAction::SameCommandQuery,
        execution: ExecutionState::PossiblyDispatched,
        details: serde_json::from_value(json!({})).unwrap(),
    };
    round_trip::<RuntimeError>(serde_json::to_value(&error).unwrap());
    let rpc = RpcError::from(error);
    assert_eq!(rpc.code, -32026);
    assert_eq!(
        serde_json::to_value(&rpc).unwrap()["data"]["code"],
        "COMMAND_OUTCOME_UNKNOWN"
    );
}

#[test]
fn counters_and_identity_brands_preserve_wire_precision() {
    assert_eq!(
        round_trip::<U64>(json!("18446744073709551615")),
        U64(u64::MAX)
    );
    for value in [
        json!(1),
        json!("01"),
        json!("-1"),
        json!("1.0"),
        json!("18446744073709551616"),
    ] {
        assert!(serde_json::from_value::<U64>(value).is_err());
    }
    let host = HostId::new();
    round_trip::<HostId>(serde_json::to_value(&host).unwrap());
    assert!(serde_json::from_value::<InstanceId>(serde_json::to_value(host).unwrap()).is_err());
    for value in [
        "ins_01993ab0-0000-4000-8000-000000000001",
        "not_01993ab0-0000-7000-8000-000000000001",
        "ins_01993AB0-0000-7000-8000-000000000001",
    ] {
        assert!(serde_json::from_value::<Id>(json!(value)).is_err());
    }
    round_trip::<Timestamp>(json!("2026-09-12T10:00:00.000Z"));
    for value in [
        "2026-09-12T10:00:00Z",
        "2026-09-12T10:00:00.000+00:00",
        "2026-13-12T10:00:00.000Z",
    ] {
        assert!(serde_json::from_value::<Timestamp>(json!(value)).is_err());
    }
    assert!(serde_json::from_value::<Digest>(json!("sha256:BAD")).is_err());
}

#[test]
fn ingress_rejects_duplicate_keys_invalid_utf8_and_trailing_documents() {
    for bytes in [
        br#"{"state":"known","state":"unknown","value":1}"#.as_slice(),
        br#"{"a":[{"answer":"allow","answer":"deny"}]}"#,
        b"{} {}",
        b"\xff",
    ] {
        assert!(from_json_slice::<Value>(bytes).is_err());
    }
}

#[test]
fn error_codes_match_the_specification_table() {
    let spec = include_str!("../../../docs/design/protocol.md");
    let mut count = 0;
    for line in spec.lines() {
        let columns: Vec<_> = line.split('|').map(str::trim).collect();
        if columns.len() < 4 || !columns[1].starts_with('`') {
            continue;
        }
        let Ok(expected) = columns[2].parse::<i32>() else {
            continue;
        };
        let name = columns[1].trim_matches('`');
        let code = round_trip::<ErrorCode>(json!(name));
        assert_eq!(code.rpc_code(), expected, "{name}");
        count += 1;
    }
    assert_eq!(count, 47);
}

#[test]
fn interaction_answer_and_source_cursor_variants_keep_native_values() {
    let digest = format!("sha256:{}", "0".repeat(64));
    for answer in [
        json!({"kind":"approval","optionId":"allow-once","inputDigest":digest}),
        json!({"kind":"question","answers":{"beverage":{"optionIds":["tea"],"text":null}}}),
        json!({"kind":"plan-review","optionId":"decline","planRevision":"7","planDigest":digest,"feedback":"Change the plan"}),
        json!({"kind":"elicitation","action":"decline","content":null}),
    ] {
        round_trip::<InteractionAnswer>(answer);
    }
    let id = "obj_01993ab0-0000-7000-8000-000000000001";
    for cursor in [
        json!({"type":"stream","connectionEpoch":id,"frame":"2"}),
        json!({"type":"file","fileIdentity":id,"fileGeneration":"1","offset":"0","length":"9","digest":digest}),
        json!({"type":"hook","invocationId":id,"frame":"1"}),
        json!({"type":"tty","streamId":id,"offset":"9007199254740993","length":"2"}),
        json!({"type":"runtime","ledgerRevision":"1"}),
    ] {
        round_trip::<SourceCursor>(cursor);
    }
    let string_id =
        round_trip::<NativeRequestKey>(json!({"type":"rpc","valueType":"string","value":"1"}));
    let number_id =
        round_trip::<NativeRequestKey>(json!({"type":"rpc","valueType":"number","value":"1"}));
    assert_ne!(string_id, number_id);
}

/// D-028 additive-increment compatibility. Every assertion here answers one
/// question: **can a peer that predates D-028 still talk to us?** A payload
/// built before this decision must parse, and must keep the meaning it had.
mod d028 {
    use super::*;

    /// An Instance row written before D-028 has no `launchedBy`. It must still
    /// parse, and `derived_launched_by` must recover the provenance from the
    /// `mode` / `promotedAt` pair that D-025 already stored.
    #[test]
    fn instance_without_launched_by_parses_and_derives_it() {
        let legacy = fixture("instance.json");
        assert!(
            legacy.get("launchedBy").is_none(),
            "the compatibility fixture must stay pre-D-028"
        );
        let instance = round_trip::<Instance>(legacy.clone());
        assert_eq!(instance.launched_by, None);
        // No mode at all: Remuda's launch path is what creates instances.
        assert_eq!(instance.derived_launched_by(), LaunchedBy::Remuda);

        let mut promoted = legacy.clone();
        promoted["mode"] = json!("promoted");
        promoted["promotedAt"] = json!("2026-09-14T10:00:00.000Z");
        let promoted = round_trip::<Instance>(promoted);
        // A promoted terminal is one a person typed `claude` into.
        assert_eq!(promoted.derived_launched_by(), LaunchedBy::User);

        // An explicit value always wins over the derivation.
        let mut explicit = legacy;
        explicit["mode"] = json!("promoted");
        explicit["launchedBy"] = json!("remuda");
        let explicit = round_trip::<Instance>(explicit);
        assert_eq!(explicit.derived_launched_by(), LaunchedBy::Remuda);
    }

    /// `signalTier` / `capabilities` are absent on every pre-D-028 NativeRef.
    /// Absent must mean "nobody reported", not `none` — the difference decides
    /// whether the static driver matrix still applies (§4.3).
    #[test]
    fn native_ref_without_runtime_capabilities_parses() {
        let legacy = fixture("instance.json")["nativeRef"].clone();
        let native_ref = round_trip::<NativeRef>(legacy);
        assert_eq!(native_ref.signal_tier, None);
        assert!(native_ref.capabilities.is_empty());

        let runtime = json!({
            "hostId": "hst_01993ab0-0000-7000-8000-000000000001",
            "nativeStoreId": "obj_01993ab0-0000-7000-8000-000000000002",
            "kind": "claude",
            "sessionId": {"state": "known", "value": "s-1"},
            "transcript": {"state": "unknown", "reason": "not-emitted", "evidenceEventIds": []},
            "signalTier": "hook",
            "capabilities": [
                {"name": "steer", "state": "unknown", "provision": "unknown",
                 "tier": "hook", "reasonCode": "unmeasured"},
                {"name": "queue", "state": "supported", "provision": "emulated",
                 "tier": "hook", "reasonCode": "remuda-ledger"},
                {"name": "resume", "state": "supported", "provision": "native",
                 "tier": "hook", "reasonCode": "session-meta"}
            ]
        });
        let runtime = round_trip::<NativeRef>(runtime);
        assert_eq!(runtime.signal_tier, Some(SignalTier::Hook));
        assert_eq!(runtime.capabilities.len(), 3);
        assert_eq!(runtime.capabilities[0].state, CapabilityState::Unknown);
        // §6 requires the three-way native/emulated/unknown distinction to be
        // expressible per capability: an emulated queue lives in Remuda's
        // ledger, a native one inside the harness where we cannot edit it.
        assert_eq!(
            runtime.capabilities[1].provision,
            CapabilityProvision::Emulated
        );
        assert_eq!(
            runtime.capabilities[2].provision,
            CapabilityProvision::Native
        );

        // An entry written before `provision` existed reads as `unknown`
        // rather than silently claiming the harness provides it.
        let legacy_entry = json!({
            "name": "steer", "state": "supported", "tier": "hook", "reasonCode": "old-peer"
        });
        let decoded: RuntimeCapability =
            from_json_slice(&serde_json::to_vec(&legacy_entry).unwrap()).unwrap();
        assert_eq!(decoded.provision, CapabilityProvision::Unknown);
    }

    /// A CapabilitySnapshot serialized before `queue` / `interrupt` existed
    /// must parse, and the missing names must read as `unknown` — "the old
    /// peer did not mention it" is not evidence that it is unsupported.
    #[test]
    fn capability_set_without_queue_and_interrupt_reads_as_unknown() {
        let mut legacy = fixture("instance.json")["capabilities"].clone();
        let names = legacy["capabilities"].as_object_mut().unwrap();
        // Roll the current fixture back to the pre-D-028 shape: a peer that
        // predates these two names simply does not send them.
        assert!(names.remove("queue").is_some());
        assert!(names.remove("interrupt").is_some());
        let snapshot: CapabilitySnapshot = from_json_slice(&serde_json::to_vec(&legacy).unwrap())
            .expect("pre-D-028 snapshot must still parse");
        assert_eq!(snapshot.capabilities.queue.state, CapabilityState::Unknown);
        assert_eq!(
            snapshot.capabilities.interrupt.state,
            CapabilityState::Unknown
        );
        // §6: an old peer said nothing about who provides a capability, so
        // neither do we. `provision` is orthogonal to `state`, and defaulting
        // it to `native` would invent a claim the peer never made.
        assert_eq!(
            snapshot.capabilities.queue.provision,
            CapabilityProvision::Unknown
        );
        assert_eq!(
            snapshot.capabilities.resume.provision,
            CapabilityProvision::Unknown
        );
        // Re-serializing adds the two names; that is the additive change, and
        // it round-trips from there.
        let reserialized = serde_json::to_value(&snapshot).unwrap();
        assert!(reserialized["capabilities"]["queue"].is_object());
        round_trip::<CapabilitySnapshot>(reserialized);
    }

    /// §6: the three input modes serialize as themselves, and the pre-D-028
    /// `new-turn` still parses.
    #[test]
    fn prompt_mode_gains_steer_and_queue_without_moving_new_turn() {
        for (wire, mode) in [
            ("new-turn", PromptMode::NewTurn),
            ("steer", PromptMode::Steer),
            ("queue", PromptMode::Queue),
        ] {
            assert_eq!(round_trip::<PromptMode>(json!(wire)), mode);
        }
        assert!(serde_json::from_value::<PromptMode>(json!("interrupt")).is_err());
    }

    /// §4.3: the new SourceChannel variants parse and the existing ones are
    /// untouched, so an Observation written by a pre-D-028 Node still loads.
    #[test]
    fn source_channel_gains_file_osc_screen() {
        for (wire, channel) in [
            ("file", SourceChannel::File),
            ("osc", SourceChannel::Osc),
            ("screen", SourceChannel::Screen),
            ("hook", SourceChannel::Hook),
            ("pty", SourceChannel::Pty),
            ("herdr", SourceChannel::Herdr),
        ] {
            assert_eq!(round_trip::<SourceChannel>(json!(wire)), channel);
        }
    }

    /// §9.1: legacy tier names normalize to levels by NAME. Index is ignored
    /// on read — it was per-harness, so carrying it would change the tier.
    #[test]
    fn effort_selection_normalizes_legacy_tier_names() {
        for (legacy, name, ultracode) in [
            ("default", EffortName::Low, false),
            ("low", EffortName::Low, false),
            ("medium", EffortName::Medium, false),
            ("think", EffortName::High, false),
            ("high", EffortName::High, false),
            ("think-hard", EffortName::Xhigh, false),
            ("xhigh", EffortName::Xhigh, false),
            ("max", EffortName::Max, false),
            ("ultracode", EffortName::Xhigh, true),
            // Unrecognized spellings fall to the documented default rather
            // than failing a launch over a stale UI string.
            ("ultra", EffortName::High, false),
            ("whatever-the-ui-said", EffortName::High, false),
        ] {
            let expected = EffortSelection { name, ultracode };
            assert_eq!(
                EffortSelection::from_legacy_name(legacy),
                expected,
                "{legacy}"
            );
            // Three spellings a client might send, all landing on one value:
            // the pre-D-028 object, a bare string, and the D-028 object.
            for payload in [
                json!({"index": 3, "name": legacy}),
                json!(legacy),
                json!({"name": legacy}),
            ] {
                let decoded: EffortSelection =
                    from_json_slice(&serde_json::to_vec(&payload).unwrap()).unwrap();
                assert_eq!(decoded, expected, "{legacy} via {payload}");
            }
        }
        // The canonical shape round-trips; `index` is not written back.
        let value = json!({"name": "xhigh", "ultracode": true});
        let decoded = round_trip::<EffortSelection>(value);
        assert_eq!(decoded.flag_value(), "ultracode");
        assert_eq!(decoded.level_name(), "xhigh");
        assert_eq!(EffortSelection::DEFAULT.flag_value(), "high");
        assert!(serde_json::from_value::<EffortSelection>(json!({"index": 3})).is_err());
    }

    /// An InstanceSpec written before D-028 has no `effort`, and absent must
    /// stay absent: it means "the harness decides", which is not the same as
    /// requesting the default level.
    #[test]
    fn instance_spec_without_effort_parses_and_stays_absent() {
        let legacy = fixture("instance-spec.json");
        assert!(legacy.get("effort").is_none());
        let spec = round_trip::<InstanceSpec>(legacy.clone());
        assert_eq!(spec.effort, None);

        let mut with_effort = legacy;
        with_effort["effort"] = json!({"name": "max", "ultracode": false});
        let spec = round_trip::<InstanceSpec>(with_effort);
        assert_eq!(
            spec.effort,
            Some(EffortSelection {
                name: EffortName::Max,
                ultracode: false
            })
        );
    }
}

#[test]
fn renderer_launch_preference_preserves_omission_and_both_modes() {
    let mut value = fixture("instance-spec.json");
    let omitted = round_trip::<InstanceSpec>(value.clone());
    assert_eq!(omitted.tui, None);
    assert_eq!(omitted.tui.unwrap_or_default(), TuiMode::Fullscreen);
    for (wire, mode) in [
        ("fullscreen", TuiMode::Fullscreen),
        ("default", TuiMode::Default),
    ] {
        value["tui"] = json!(wire);
        assert_eq!(round_trip::<InstanceSpec>(value.clone()).tui, Some(mode));
    }
    value["tui"] = json!("auto");
    assert!(serde_json::from_value::<InstanceSpec>(value).is_err());
}
