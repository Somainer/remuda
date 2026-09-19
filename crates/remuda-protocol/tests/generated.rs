//! Source: Rust wire declarations and schema/TS outputs specified by `protocol.md` §12.

use remuda_protocol::{
    schema::{schema_document, typescript},
    *,
};
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::Path;

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn validator<T: JsonSchema>() -> jsonschema::Validator {
    let schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>();
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&serde_json::to_value(schema).unwrap())
        .unwrap()
}
fn parity<T: JsonSchema + DeserializeOwned + Serialize>(values: Vec<Value>) {
    let validator = validator::<T>();
    for value in values {
        let decoded = serde_json::from_value::<T>(value.clone());
        assert_eq!(
            validator.is_valid(&value),
            decoded.is_ok(),
            "{}: {value}",
            std::any::type_name::<T>()
        );
        if let Ok(decoded) = decoded {
            assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        }
    }
}

#[test]
fn generated_files_are_current_without_writing_to_the_workspace() {
    let document = schema_document();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let expected = [
        (
            "crates/remuda-protocol/schema/protocol.schema.json",
            serde_json::to_string_pretty(&document).unwrap() + "\n",
        ),
        ("web/src/types/generated.ts", typescript(&document).unwrap()),
    ];
    for (path, expected) in expected {
        let actual = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(actual == expected, "{path} is stale; run just gen-types");
    }
    assert!(
        !document["$defs"]
            .as_object()
            .unwrap()
            .contains_key("MaterializedLaunch")
    );
}

#[test]
fn public_wire_types_are_registered_for_generation() {
    let document = schema_document();
    let definitions = document["$defs"].as_object().unwrap();
    let exempt = [
        "WireValueError",
        "BinaryFrameError",
        "SchemaExportError",
        // Local path-containment rejection reason (`pub mod path_guard`); a
        // host-side validation error, never serialized to the wire.
        "PathGuardError",
        // M1 Hub↔Node operational frames (`pub mod hubnode`); not protocol.md §12 catalog.
        "HubNodeRequest",
        "HubNodeResponse",
        "JsonRpcErrorObject",
        "HubNodeMethod",
        "NodeAuthParams",
        "NodeHelloParams",
        "NodeHostInventory",
        "NodeHeartbeatParams",
        "WorkspaceMutationPhase",
        "WorkspaceMutationParams",
        "RegisteredWorkspace",
        "WorkspaceRegistryResult",
        "InstanceCreateParams",
        "InstanceSendParams",
        "AttachmentRef",
        "AttachmentKind",
        "InstanceCancelParams",
        "InstanceRespondParams",
        "JournalAppendParams",
        "JournalSeqWatermark",
        "TtyFrameParams",
        "TtyModeParams",
        // Additive tty.mode companion (native-config, 2026-09-16): same
        // Node→Hub JSON-RPC param family as TtyModeParams; the web tty client
        // parses this channel by hand rather than from generated types.
        "TtyProgress",
        "TtyProgressState",
        "TtyWriteParams",
        "TtyResizeParams",
        "TtyAttachParams",
        "TtyBinaryEnvelopeSpec",
        "ObjectPullParams",
        "ObjectChunkParams",
        // D-048 `api.*` relay frames: same M1 Hub↔Node operational family as
        // `object.pull`. They are documented in `protocol.md` §7.6 rather than
        // the §12 catalog the generated types are built from, because the
        // WebSocket/stdio carrier routes them to the stream registry by hand
        // (they are notifications with their own flow control, never RPC).
        "ApiOpenParams",
        "ApiHeader",
        "ApiBodyParams",
        "ApiHeadParams",
        "ApiChunkParams",
        "ApiEndParams",
        "ApiEndError",
        "ApiCancelParams",
        "ApiCreditParams",
        // §9.1: pure transcript-mapper state shared between the driver and the
        // journal tailer; it is never serialized on the wire.
        "EffortTracker",
        "LivePermissionTracker",
        // §9.1: parsed verdict of an /effort stdout line — mapper output, not
        // a wire type.
        "EffortStdout",
        // M1 5b `remuda watch`: borrowed classifier input / classified output
        // / key-encoding result — pure rule logic shared by Hub and tests,
        // never serialized on the wire (the roster carries `WorkerWatch`).
        "ScreenSignals",
        "ScreenClass",
        "EncodedKeys",
        // §9.1: pure transcript-mapper state shared between the driver and the
        // journal tailer; never serialized on the wire.
        "ModelTracker",
        // §9.1: parsed verdict of a /model stdout line — mapper output, not a
        // wire type.
        "ModelStdout",
        // model-pin-1: pure pin-vs-observed comparison shared by the Node gate
        // and the Hub watch classification; never serialized on the wire.
        "ModelPinVerdict",
        // D-045 §6.2: producer-side outcome of staging a tool-result image
        // through the object route; a mapper error, never serialized.
        "ToolMediaError",
        // D-045 §6.2: producer-side fold output and per-image staging
        // outcome used to scrub the sidecar; never serialized on the wire.
        "ImageOutcome",
        "FoldedToolResult",
    ];
    for file in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap() {
        let file = file.unwrap().path();
        if file.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        for line in std::fs::read_to_string(file).unwrap().lines() {
            let rest = line
                .strip_prefix("pub struct ")
                .or_else(|| line.strip_prefix("pub enum "))
                .or_else(|| line.strip_prefix("wire_enum!("))
                .or_else(|| line.strip_prefix("branded_id!("));
            if let Some(rest) = rest {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() || exempt.contains(&name.as_str()) {
                    continue;
                }
                assert!(
                    definitions
                        .keys()
                        .any(|key| key == &name || key.starts_with(&(name.clone() + "_"))),
                    "{name} is absent from schema::register"
                );
            }
        }
    }
}

#[test]
fn schema_and_serde_agree_on_scalars_and_literal_versions() {
    parity::<U64>(vec![
        json!("0"),
        json!("1"),
        json!("9007199254740993"),
        json!("18446744073709551615"),
        json!("18446744073709551616"),
        json!("18446744073709551619"),
        json!("20000000000000000000"),
        json!("00"),
        json!("01"),
        json!("-1"),
        json!(1),
        json!(null),
    ]);
    parity::<Timestamp>(vec![
        json!("2026-09-12T10:00:00.000Z"),
        json!("2026-09-12T10:00:00.100Z"),
        json!("2026-09-12t10:00:00.000Z"),
        json!("2026-09-12T10:00:60.000Z"),
        json!("2026-02-30T10:00:00.000Z"),
        json!("2026-09-12T10:00:00Z"),
    ]);
    parity::<HostId>(vec![
        json!("hst_01993ab0-0000-7000-8000-000000000001"),
        json!("ins_01993ab0-0000-7000-8000-000000000001"),
        json!("hst_01993ab0-0000-4000-8000-000000000001"),
        json!("hst_01993AB0-0000-7000-8000-000000000001"),
    ]);
    parity::<SchemaVersion>(vec![json!(1), json!(2), json!("1"), json!(null)]);
    parity::<BoolLiteral<true>>(vec![json!(true), json!(false), json!(1)]);
    parity::<BoolLiteral<false>>(vec![json!(true), json!(false), json!(null)]);
}

#[test]
fn emitted_entity_and_observation_fixtures_validate() {
    macro_rules! check {
        ($ty:ty,$file:literal) => {{
            let value = fixture($file);
            let validator = validator::<$ty>();
            let errors: Vec<_> = validator
                .iter_errors(&value)
                .map(|error| error.to_string())
                .collect();
            assert!(errors.is_empty(), "{}: {errors:?}", $file);
        }};
    }
    check!(Host, "host.json");
    check!(Workspace, "workspace.json");
    check!(WorktreeRecord, "worktree.json");
    check!(Instance, "instance.json");
    check!(Run, "run.json");
    check!(Command, "command.json");
    check!(Interaction, "interaction.json");
    check!(InstanceSpec, "instance-spec.json");
    check!(RpcNotification, "events-batch.json");
    check!(JournalEvent, "registry-event.json");
    for value in fixture("observations.json").as_array().unwrap() {
        let errors: Vec<_> = validator::<Observation>()
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }
}

#[test]
fn required_nullable_fields_and_unknown_event_tags_do_not_widen() {
    let mut missing = fixture("observations.json")[0].clone();
    missing.as_object_mut().unwrap().remove("runId");
    let mut unknown = fixture("observations.json")[0].clone();
    unknown["kind"] = json!("future.success");
    parity::<Observation>(vec![
        fixture("observations.json")[0].clone(),
        missing,
        unknown,
    ]);
    let mut malformed = fixture("registry-event.json");
    malformed["instanceId"] = json!("ins_01993ab0-0000-7000-8000-000000000001");
    malformed["schemaVersion"] = json!(2);
    parity::<JournalEvent>(vec![malformed]);
    parity::<RpcResponse<Value>>(vec![
        json!({"jsonrpc":"2.0","id":"1","result":null}),
        json!({"jsonrpc":"2.0","id":"1"}),
        json!({"jsonrpc":"2.0","id":"1","result":null,"error":{"code":-32601,"message":"bad"}}),
    ]);
}

#[test]
fn generator_rejects_unreviewed_schema_keywords() {
    let document = json!({"$defs":{"Future":{"type":"object","if":{"properties":{"x":{"const":1}}},"then":false}}});
    assert!(typescript(&document).is_err());
}

#[test]
fn generator_rejects_missing_definitions() {
    let document = json!({"$defs":{"Request":{"$ref":"#/$defs/Missing"}}});
    assert!(typescript(&document).is_err());
}

/// The D-047 / D-048 wire additions, pinned in the generated schema.
///
/// Task 1 of the api-routing plan exists so every later task can build on these
/// names. A refactor that renames or drops one would otherwise break tasks 2–6
/// at a distance, so the exact spellings are asserted here — and asserted
/// against the *generated* document, not the source, so the check fails if the
/// type stops being registered (a `register!` entry is easy to lose).
#[test]
fn api_routing_types_and_wire_values_are_in_the_generated_schema() {
    let document = schema_document();
    let definitions = document["$defs"].as_object().expect("$defs");

    // `wire_enum!` types emit `enum`; the hand-written D-047 enums are
    // documented per-variant and reach the schema as `oneOf` of `const`s, so
    // read both shapes rather than assuming the macro's.
    fn enum_values(definitions: &serde_json::Map<String, Value>, name: &str) -> Vec<String> {
        let schema = &definitions[name];
        if let Some(values) = schema["enum"].as_array() {
            return values
                .iter()
                .map(|value| value.as_str().expect("enum value is a string").to_owned())
                .collect();
        }
        schema["oneOf"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} is neither an enum nor a oneOf"))
            .iter()
            .map(|variant| {
                variant["const"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name} variant has no const"))
                    .to_owned()
            })
            .collect()
    }

    // Delivery mode and the route sub-mode, with the plan's exact spellings.
    assert_eq!(
        enum_values(definitions, "ProviderDeliveryMode"),
        ["direct", "via"]
    );
    assert_eq!(
        enum_values(definitions, "ApiRouteMode"),
        ["auto", "hub-relay", "direct-net"]
    );
    // The observation type carries only resolved routes: `auto` is a request.
    assert_eq!(
        enum_values(definitions, "ApiRouteKind"),
        ["direct-net", "hub-relay"]
    );
    // Refusals are prefixed `api-via-`, and none of them means "fell back".
    assert_eq!(
        enum_values(definitions, "ApiViaRefusal"),
        [
            "api-via-unknown-host",
            "api-via-host-offline",
            "api-via-unsupported",
            "api-via-unreachable",
        ]
    );

    // `delivery` on the profile, `apiRoute` on the instance spec and the
    // projection, `relayBind` on the host. Each is optional, because every one
    // of them is absent on a payload written before D-047.
    let required = |name: &str| -> Vec<String> {
        definitions[name]["required"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(definitions["ProviderProfile"]["properties"]["delivery"].is_object());
    assert!(!required("ProviderProfile").contains(&"delivery".to_owned()));
    assert!(definitions["InstanceSpec"]["properties"]["apiRoute"].is_object());
    assert!(!required("InstanceSpec").contains(&"apiRoute".to_owned()));
    assert!(definitions["Host"]["properties"]["relayBind"].is_object());
    assert!(!required("Host").contains(&"relayBind".to_owned()));

    // `ProviderDelivery` is strict on the one combination that cannot run:
    // `via` with no host. `route` is always emitted so a stored row is
    // self-describing, while `viaHostId` is not.
    assert!(definitions["ProviderDelivery"]["properties"]["viaHostId"].is_object());
    let delivery_required = required("ProviderDelivery");
    assert!(delivery_required.contains(&"mode".to_owned()));
    assert!(delivery_required.contains(&"route".to_owned()));
    assert!(!delivery_required.contains(&"viaHostId".to_owned()));

    // TransportLimits gained exactly two fields, each with a recorded default.
    //
    // The schema is generated `for_serialize`, so every field of this struct is
    // `required` there — the same as the nine that came before, since the Hub
    // always emits them. What makes an older nine-key `hello.limits` readable
    // is `#[serde(default)]`, which the schema expresses as `default` and the
    // wire test `transport_limits_written_before_d048_still_parse` proves. A
    // field added here without a default would surface as a missing `default`
    // key rather than a missing `required` entry.
    for (field, default) in [("maxApiStreams", 8), ("apiChunkBytes", 65_536)] {
        let schema = &definitions["TransportLimits"]["properties"][field];
        assert!(schema.is_object(), "TransportLimits.{field} missing");
        assert_eq!(
            schema["default"],
            json!(default),
            "TransportLimits.{field} must record the value an absent key means"
        );
        assert_eq!(schema["type"], json!("integer"));
    }

    // The per-dispatch override is a bare string on the wire, not a tagged
    // object: two of its three values are keywords.
    assert_eq!(definitions["ApiViaOverride"]["type"], json!("string"));
}
