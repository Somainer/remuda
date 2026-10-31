//! D-060 (cb-memory-inject): the main-agent.md §12.3 launch-path table, as a
//! test over the real materializer.
//!
//! Every (driver, kind) pair this Node admits ([`super::validate_kind_driver`])
//! must have exactly one row, and every row must be admitted: a new
//! `DriverKind` arm or a newly admitted pair fails here until its row is
//! added. Each row materializes a launch carrying `memoryContext` and asserts
//! the mechanism the recipe actually got — the materializer's own
//! `--append-system-prompt` pair, the first-input data block, or nothing.

use remuda_driver::{
    APPEND_SYSTEM_PROMPT, BinaryPin, BinarySource, Delegation, LaunchOrigin, MaterializeRequest,
    MemoryContext, ProviderHealth, ProviderKind, ProviderProfile, RecipeMemory, SessionAction,
};
use remuda_protocol::{AgentKind, DriverKind, Id, InstanceSpec, U64};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mechanism {
    Flag,
    DataBlock,
    NotApplicable,
}

use AgentKind as K;
use DriverKind as D;
use Mechanism::{DataBlock, Flag, NotApplicable};

/// main-agent.md §12.3, rows 1-18, in the document's order.
const TABLE: [(DriverKind, AgentKind, Mechanism); 18] = [
    (D::ClaudePrint, K::Claude, Flag),
    (D::ClaudeSdk, K::Claude, Flag),
    (D::ClaudePty, K::Claude, Flag),
    (D::ClaudeBg, K::Claude, Flag),
    (D::CodexAppserver, K::Codex, DataBlock),
    (D::GrokAcp, K::Grok, DataBlock),
    (D::AgyPrint, K::Agy, DataBlock),
    (D::GenericPty, K::Claude, DataBlock),
    (D::GenericPty, K::Codex, DataBlock),
    (D::GenericPty, K::Grok, DataBlock),
    (D::GenericPty, K::Agy, DataBlock),
    (D::GenericPty, K::Generic, NotApplicable),
    (D::ShellPty, K::Claude, Flag),
    (D::ShellPty, K::Codex, DataBlock),
    (D::ShellPty, K::Grok, DataBlock),
    (D::ShellPty, K::Agy, DataBlock),
    (D::ShellPty, K::Terminal, NotApplicable),
    (D::ShellPty, K::Generic, NotApplicable),
];

/// Every wire value of a protocol enum, read from the generated schema so a
/// new variant shows up here without anyone remembering to list it.
fn every<T: serde::de::DeserializeOwned>(name: &str) -> Vec<T> {
    let document = remuda_protocol::schema::schema_document();
    document["$defs"][name]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("{name} is not a oneOf enum in the schema"))
        .iter()
        .map(|variant| {
            serde_json::from_value(variant["const"].clone())
                .unwrap_or_else(|error| panic!("{name} {variant}: {error}"))
        })
        .collect()
}

fn spec(driver: DriverKind, kind: AgentKind, args: &[&str]) -> InstanceSpec {
    let mut spec: InstanceSpec = serde_json::from_value(serde_json::json!({
        "schemaVersion": 1,
        "host": "hst_01993ab0-0000-7000-8000-000000000001",
        "workspaceId": "wsp_01993ab0-0000-7000-8000-000000000001",
        "kind": "claude",
        "driver": "claude-print",
        "binaryRef": "obj_01993ab0-0000-7000-8000-000000000002",
        "cwd": "/work/project",
        "providerProfile": {"id": "pvp_01993ab0-0000-7000-8000-000000000001", "revision": "1"},
        "modelId": null,
        "permissionMode": {"kind": "claude", "mode": "manual", "interaction": "host"},
        "env": {},
        "args": [],
        "settingsOverlay": {"format": "none", "objectRef": null, "revision": "1"},
        "nativeHome": {"mode": "registered", "storeId": "obj_01993ab0-0000-7000-8000-000000000002"},
        "carrier": {"type": "stdio"},
        "capabilities": [],
        "requiredCapabilities": [],
        "completionScope": "native-turn",
        "parent": null
    }))
    .expect("spec fixture");
    spec.driver = driver;
    spec.kind = kind;
    spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    spec
}

fn profile(driver: DriverKind) -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001"
            .parse()
            .expect("profile id"),
        kind: match driver {
            D::CodexAppserver => ProviderKind::OpenaiResponses,
            D::GrokAcp => ProviderKind::Xai,
            D::AgyPrint => ProviderKind::Google,
            _ => ProviderKind::Anthropic,
        },
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["model-under-test".into()],
        health: ProviderHealth::Healthy,
    }
}

const INDEX: &str = "<remuda-memory lineage=\"ins_t\" rev=\"7\">\n## Index\n\
                     - [user] lang: 语言偏好 — 中文\n</remuda-memory>";

/// Materialize `(driver, kind)` with the chapter's memory, fresh or resumed.
fn materialize(
    driver: DriverKind,
    kind: AgentKind,
    args: &[&str],
    resume: bool,
) -> remuda_driver::LaunchRecipe {
    let tmp = tempfile::tempdir().expect("tempdir");
    let binary = tmp.path().join("agent");
    std::fs::write(&binary, b"#!/bin/sh\n").expect("stub binary");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let spec = spec(driver, kind, args);
    let profile = profile(driver);
    let context = MemoryContext {
        rev: U64(7),
        text: INDEX.into(),
    };
    let session_id = "01993ab0-0000-7000-8000-000000000003".to_owned();
    let request = MaterializeRequest {
        spec: &spec,
        profile: &profile,
        launch_dir: tmp.path().join("launch"),
        native_home: home,
        session: if resume {
            SessionAction::Resume { session_id }
        } else {
            SessionAction::New { session_id }
        },
        launch_id: Id::new("launch").expect("launch id"),
        binary: BinarySource::Pinned(BinaryPin {
            abs_path: binary.to_string_lossy().into_owned(),
            version: "2.1.291 (Claude Code)".into(),
            sha256: remuda_driver::hash_file(&binary).expect("hash"),
        }),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: None,
        settings_overlay_path: None,
        secret_policy: None,
        memory_context: Some(&context),
    };
    remuda_driver::materialize(&request)
        .unwrap_or_else(|error| panic!("{driver:?}/{kind:?}: {error}"))
}

fn flag_pairs(argv: &[String]) -> Vec<usize> {
    argv.iter()
        .enumerate()
        .filter(|(_, token)| token.starts_with(APPEND_SYSTEM_PROMPT))
        .map(|(index, _)| index)
        .collect()
}

#[test]
fn memory_launch_table_has_one_row_per_admitted_pair() {
    let drivers: Vec<DriverKind> = every("DriverKind");
    let kinds: Vec<AgentKind> = every("AgentKind");
    let rows: BTreeSet<(DriverKind, AgentKind)> = TABLE
        .iter()
        .map(|(driver, kind, _)| (*driver, *kind))
        .collect();
    assert_eq!(rows.len(), TABLE.len(), "a (driver, kind) row is repeated");
    assert_eq!(TABLE.len(), 18, "main-agent.md §12.3 has 18 rows");
    for driver in &drivers {
        for kind in &kinds {
            let admitted = super::validate_kind_driver(*kind, *driver).is_ok();
            assert_eq!(
                admitted,
                rows.contains(&(*driver, *kind)),
                "({driver:?}, {kind:?}): admitted={admitted} — the §12.3 table must have a \
                 row for exactly the pairs the Node admits"
            );
        }
        // Every arm of the materializer's driver match is reachable by a row.
        assert!(
            rows.iter().any(|(row_driver, _)| row_driver == driver),
            "{driver:?} has no row in the §12.3 table"
        );
    }
}

#[test]
fn memory_launch_table_mechanism_per_row() {
    for (number, (driver, kind, mechanism)) in TABLE.iter().enumerate() {
        let row = number + 1;
        // A caller-derived argument the flag must come after.
        let args: &[&str] = if *mechanism == Flag {
            &["--add-dir", "/work/extra"]
        } else {
            &[]
        };
        let recipe = materialize(*driver, *kind, args, false);
        let pairs = flag_pairs(&recipe.argv);
        match mechanism {
            Flag => {
                assert_eq!(pairs.len(), 1, "row {row}: {:?}", recipe.argv);
                let at = pairs[0];
                assert_eq!(recipe.argv[at], APPEND_SYSTEM_PROMPT, "row {row}");
                assert_eq!(recipe.argv.get(at + 1).map(String::as_str), Some(INDEX));
                let caller = recipe
                    .argv
                    .iter()
                    .position(|token| token == "--add-dir")
                    .expect("caller argument");
                assert!(at > caller, "row {row}: the pair precedes caller args");
                assert_eq!(
                    recipe.memory,
                    Some(RecipeMemory::Flag {
                        rev: 7,
                        text: INDEX.into(),
                        fresh_prompt: false,
                    }),
                    "row {row}"
                );
                assert!(
                    !recipe
                        .argv
                        .iter()
                        .any(|token| token == "--system-prompt-snapshot"),
                    "row {row}: a fresh session keeps Claude's prompt recording"
                );
                assert_eq!(recipe.own_system_prompt(), Some(INDEX), "row {row}");
            }
            DataBlock => {
                assert!(pairs.is_empty(), "row {row}: {:?}", recipe.argv);
                assert_eq!(
                    recipe.memory,
                    Some(RecipeMemory::DataBlock {
                        rev: 7,
                        text: INDEX.into()
                    }),
                    "row {row}"
                );
                assert_eq!(recipe.memory_data_block(), Some(INDEX), "row {row}");
            }
            NotApplicable => {
                assert!(pairs.is_empty(), "row {row}: {:?}", recipe.argv);
                assert_eq!(recipe.memory, None, "row {row}");
            }
        }
        assert!(
            !recipe
                .argv
                .iter()
                .any(|token| token.contains("--system-prompt")
                    && !token.starts_with(APPEND_SYSTEM_PROMPT)),
            "row {row}: --system-prompt is never emitted"
        );
    }
}

/// The flag is per process, so a native resume emits it again — with
/// `--system-prompt-snapshot off`, because Claude 2.1.265+ would otherwise
/// reuse the prompt the conversation recorded on its first request; the data
/// block is part of the conversation a resume carries over, so it is never
/// sent twice.
#[test]
fn a_native_resume_gets_the_flag_again_but_no_second_data_block() {
    for (driver, kind, mechanism) in TABLE {
        if mechanism == NotApplicable {
            continue;
        }
        let recipe = materialize(driver, kind, &[], true);
        match mechanism {
            Flag => {
                let pairs = flag_pairs(&recipe.argv);
                assert_eq!(pairs.len(), 1, "{driver:?}/{kind:?}");
                assert_eq!(recipe.own_system_prompt(), Some(INDEX));
                assert!(
                    recipe
                        .memory
                        .as_ref()
                        .is_some_and(RecipeMemory::fresh_prompt)
                );
                assert_eq!(
                    recipe.argv[pairs[0] - 2..pairs[0]],
                    ["--system-prompt-snapshot", "off"],
                    "{driver:?}/{kind:?}: {:?}",
                    recipe.argv
                );
            }
            DataBlock => {
                assert!(flag_pairs(&recipe.argv).is_empty(), "{driver:?}/{kind:?}");
                assert_eq!(recipe.memory, None, "{driver:?}/{kind:?}");
            }
            NotApplicable => unreachable!(),
        }
    }
}
