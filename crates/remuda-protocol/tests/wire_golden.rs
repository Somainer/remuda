//! Source: every JSON example in `docs/design/protocol.md`; see wire_golden/manifest.json.
//! Native frames remain raw JSON; normalized frames use concrete protocol types and schema.

use remuda_protocol::*;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeSet, fmt::Debug, path::Path};

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    NativeJson,
    RpcRequest,
    InteractionRequest,
    NativeRef,
    CarrierSpec,
    BinaryHeader,
}
#[derive(Deserialize)]
struct Entry {
    file: String,
    source: String,
    section: String,
    block: usize,
    frame: usize,
    kind: Kind,
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn examples() -> Vec<(usize, usize, Value)> {
    let text = include_str!("../../../docs/design/protocol.md");
    let mut result = Vec::new();
    let mut block = String::new();
    let mut fence: Option<&str> = None;
    let mut ordinal = 0;
    for line in text.lines() {
        if let Some(end) = fence {
            if line == end {
                let values: Vec<Value> = match from_json_slice(block.as_bytes()) {
                    Ok(value) => vec![value],
                    Err(_) => block
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| from_json_slice(line.as_bytes()).unwrap())
                        .collect(),
                };
                result.extend(
                    values
                        .into_iter()
                        .enumerate()
                        .map(|(i, value)| (ordinal, i + 1, value)),
                );
                fence = None;
            } else {
                block.push_str(line);
                block.push('\n');
            }
        } else if line == "~~~json" || line == "```json" {
            ordinal += 1;
            fence = Some(if line.starts_with('~') { "~~~" } else { "```" });
            block.clear();
        }
    }
    assert!(fence.is_none(), "unclosed JSON example");
    result
}

fn typed<T: DeserializeOwned + Serialize + JsonSchema + PartialEq + Debug>(value: Value) {
    let first: T = from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    let serialized = serde_json::to_value(&first).unwrap();
    assert_eq!(serialized, value);
    let second: T = from_json_slice(&serde_json::to_vec(&first).unwrap()).unwrap();
    assert_eq!(first, second);
    let schema = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>();
    let schema = serde_json::to_value(schema).unwrap();
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    let errors: Vec<_> = validator
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{}: {errors:?}",
        std::any::type_name::<T>()
    );
}

#[test]
fn every_specification_json_frame_has_one_lossless_golden() {
    let directory = root().join("tests/wire_golden");
    let entries: Vec<Entry> =
        from_json_slice(&std::fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    let examples = examples();
    assert_eq!(entries.len(), examples.len());
    let mut files = BTreeSet::new();
    for (entry, (block, frame, value)) in entries.iter().zip(examples) {
        assert_eq!(entry.source, "docs/design/protocol.md");
        assert!(!entry.section.is_empty());
        assert_eq!((entry.block, entry.frame), (block, frame));
        assert!(files.insert(entry.file.clone()), "duplicate golden");
        let golden: Value =
            from_json_slice(&std::fs::read(directory.join(&entry.file)).unwrap()).unwrap();
        assert_eq!(
            golden, value,
            "{} differs from the specification",
            entry.file
        );
        match entry.kind {
            Kind::NativeJson => {
                assert!(serde_json::from_value::<RpcRequest>(golden.clone()).is_err());
                let wire = serde_json::to_vec(&golden).unwrap();
                assert_eq!(from_json_slice::<Value>(&wire).unwrap(), golden);
            }
            Kind::RpcRequest => typed::<RpcRequest>(golden),
            Kind::InteractionRequest => typed::<InteractionRequest>(golden),
            Kind::NativeRef => typed::<NativeRef>(golden),
            Kind::CarrierSpec => typed::<CarrierSpec>(golden),
            Kind::BinaryHeader => typed::<BinaryHeader>(golden),
        }
    }
    let actual: BTreeSet<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".json") && name != "manifest.json")
        .collect();
    assert_eq!(files, actual, "untracked or missing golden fixture");
}
