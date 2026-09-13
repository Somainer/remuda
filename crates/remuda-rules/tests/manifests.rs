//! Every bundled manifest must parse, compile, and hold herdr's invariants.
//!
//! These are the guards on the extracted assets: if a regenerated rule file
//! drifts into a shape this engine cannot honour, it fails here rather than
//! silently never matching at runtime.

use remuda_rules::{BUNDLED, ENGINE_VERSION, Manifest, Region, State, bundled};

#[test]
fn every_bundled_manifest_compiles() {
    for (kind, text) in BUNDLED {
        let manifest =
            Manifest::parse(text).unwrap_or_else(|e| panic!("{kind}: parse failed: {e}"));
        assert_eq!(
            manifest.id, *kind,
            "{kind}: table key must match manifest id"
        );
        assert!(!manifest.version.is_empty(), "{kind}: version must be set");
        assert!(
            !manifest.updated_at.is_empty(),
            "{kind}: updated_at must be set"
        );
        manifest
            .compile()
            .unwrap_or_else(|e| panic!("{kind}: compile failed: {e}"));
    }
}

#[test]
fn bundled_table_is_sorted_and_complete() {
    let kinds: Vec<&str> = BUNDLED.iter().map(|(k, _)| *k).collect();
    let mut sorted = kinds.clone();
    sorted.sort_unstable();
    assert_eq!(kinds, sorted, "BUNDLED must stay sorted by kind");
    sorted.dedup();
    assert_eq!(sorted.len(), kinds.len(), "BUNDLED must not repeat a kind");
    // The four kinds Remuda drives today (D-028 §3) must always be present.
    for required in ["claude", "codex", "grok", "agy"] {
        assert!(kinds.contains(&required), "{required} manifest is missing");
    }
}

#[test]
fn regions_never_outrun_the_declared_engine_version() {
    // herdr's own check: "rule <id> uses top_non_empty_lines but
    // min_engine_version is below N". A manifest that understates its engine
    // level would be accepted by an older engine that cannot honour its rules.
    for (kind, text) in BUNDLED {
        let manifest = Manifest::parse(text).expect("parse");
        for rule in &manifest.rules {
            let region: Region = rule.region.parse().expect("region parses");
            assert!(
                region.min_engine_version() <= manifest.min_engine_version,
                "{kind}: rule {} uses {} but min_engine_version is {}",
                rule.id,
                rule.region,
                manifest.min_engine_version,
            );
        }
        assert!(
            manifest.min_engine_version <= ENGINE_VERSION,
            "{kind}: manifest needs engine {}, this engine is {ENGINE_VERSION}",
            manifest.min_engine_version,
        );
    }
}

#[test]
fn skip_state_update_rules_are_unknown_and_flagless() {
    // Anchor ③: transcript viewers and model pickers must suppress the update,
    // never set a state and never count as visible evidence.
    let mut found = 0;
    for (kind, text) in BUNDLED {
        for rule in &Manifest::parse(text).expect("parse").rules {
            if !rule.skip_state_update {
                continue;
            }
            found += 1;
            assert_eq!(
                rule.state, "unknown",
                "{kind}: rule {} skips the update but is not unknown",
                rule.id
            );
            assert!(
                !(rule.visible_idle || rule.visible_blocker || rule.visible_working),
                "{kind}: rule {} skips the update but carries visible evidence",
                rule.id
            );
        }
    }
    assert!(found > 0, "expected at least one skip_state_update rule");
}

#[test]
fn aliases_resolve_to_their_manifest() {
    let agy = bundled("antigravity").expect("alias resolves");
    assert_eq!(agy.id, "agy");
    assert!(agy.matches("antigravity-cli"));

    let claude = bundled("claude-code").expect("alias resolves");
    assert_eq!(claude.id, "claude");
}

#[test]
fn unknown_kind_is_an_error_not_a_default() {
    // A kind we have no rules for must not silently fall back to some other
    // agent's table.
    assert!(bundled("no-such-agent").is_err());
}

#[test]
fn manifest_versions_are_pinned_to_the_extracted_herdr_build() {
    // These are the versions extracted from herdr 0.9.0. If a regeneration
    // moves them, that is a signal to re-verify the fixtures, not to silently
    // accept new behaviour — see the README.
    for (kind, expected) in [
        ("claude", "2026.09.04.1"),
        ("codex", "2026.09.05.1"),
        ("grok", "2026.07.16.2"),
        ("agy", "2026.06.24.1"),
    ] {
        let manifest = bundled(kind).expect("bundled");
        assert_eq!(manifest.version, expected, "{kind} manifest version moved");
    }
}

#[test]
fn every_state_spelling_round_trips() {
    for state in [State::Idle, State::Working, State::Blocked, State::Unknown] {
        let parsed: State = state.as_str().parse().expect("state parses");
        assert_eq!(parsed, state);
    }
    assert!(
        "done".parse::<State>().is_err(),
        "done is derived, not detected"
    );
}
