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
        "InstanceCancelParams",
        "InstanceRespondParams",
        "JournalAppendParams",
        "JournalSeqWatermark",
        "TtyFrameParams",
        "TtyWriteParams",
        "TtyResizeParams",
        "TtyAttachParams",
        "TtyBinaryEnvelopeSpec",
        // §9.1: pure transcript-mapper state shared between the driver and the
        // journal tailer; it is never serialized on the wire.
        "EffortTracker",
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
