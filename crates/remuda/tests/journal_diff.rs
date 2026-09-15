//! `remuda journal diff` against the D-028 parity fixtures.
//!
//! The fixture pair is one 3-turn scenario (text, tool call + result,
//! approval) recorded the way each carrier would actually record it, plus a
//! negative variant whose tool result never lands. Both dumps are generated
//! from a single scenario description by
//! `tests/fixtures/journal-parity/generate.py`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// Repo-relative path to a parity fixture.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/journal-parity")
        .join(name)
}

/// Repository root, so the default whitelist resolves the way the gate runs it.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Run the subcommand from the repo root and return `(exit code, stdout)`.
fn diff(args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .current_dir(repo_root())
        .args(["journal", "diff"])
        .args(args)
        // `--color auto` reads `NO_COLOR`, so a runner that exports it would
        // make the "piped output is plain" assertion pass for the wrong reason
        // and could only ever weaken the test. Removed rather than set, so the
        // tool decides from the pipe alone — which is what is being asserted.
        .env_remove("NO_COLOR")
        .output()
        .expect("run remuda journal diff");
    (
        output.status.code().expect("exit code"),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
    )
}

/// Parse `--json` output.
fn json(args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    let (_, stdout) = diff(&args);
    serde_json::from_str(&stdout).expect("valid JSON report")
}

#[test]
fn print_and_pty_runs_of_one_scenario_agree_modulo_the_whitelist() {
    let left = fixture("print-3turn.json");
    let right = fixture("pty-3turn.json");
    let report = json(&[left.to_str().unwrap(), right.to_str().unwrap()]);

    assert_eq!(
        report["equal"],
        true,
        "blocking differences: {}",
        serde_json::to_string_pretty(&report["differences"]).unwrap()
    );
    assert!(report["differences"].as_array().unwrap().is_empty());

    let whitelisted = report["whitelisted"].as_array().unwrap();
    assert!(
        !whitelisted.is_empty(),
        "the fixtures must exercise the whitelist, not merely match"
    );
    for difference in whitelisted {
        assert!(
            difference["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "every whitelisted difference carries its reason: {difference}"
        );
    }

    let (code, _) = diff(&[left.to_str().unwrap(), right.to_str().unwrap()]);
    assert_eq!(code, 0, "a whitelisted-only diff exits 0");
}

#[test]
fn the_whitelisted_differences_are_exactly_the_expected_granularity_ones() {
    let report = json(&[
        fixture("print-3turn.json").to_str().unwrap(),
        fixture("pty-3turn.json").to_str().unwrap(),
    ]);
    let mut seen: Vec<String> = report["whitelisted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|difference| {
            format!(
                "{}.{}",
                difference["kind"].as_str().unwrap_or_default(),
                difference["field"].as_str().unwrap_or("<event>")
            )
        })
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen,
        vec![
            "interaction.answered.delivery",
            "interaction.requested.carrier",
            "message.granularity",
            "tool_call.granularity",
            "usage.accounting",
            "usage.cost.amount",
        ],
        "an unexpected difference slipped into the whitelisted set"
    );
}

#[test]
fn a_missing_tool_result_fails_the_gate() {
    let left = fixture("print-3turn.json");
    let right = fixture("pty-3turn-missing-tool-result.json");
    let report = json(&[left.to_str().unwrap(), right.to_str().unwrap()]);

    assert_eq!(report["equal"], false);
    let differences = report["differences"].as_array().unwrap();
    assert_eq!(
        differences.len(),
        1,
        "only the dropped tool result should block: {}",
        serde_json::to_string_pretty(differences).unwrap()
    );
    assert_eq!(differences[0]["kind"], "tool_result");
    assert_eq!(differences[0]["side"], "left");
    assert!(
        differences[0]["reason"].is_null(),
        "a blocking difference must not carry a whitelist reason"
    );

    let (code, stdout) = diff(&[left.to_str().unwrap(), right.to_str().unwrap()]);
    assert_eq!(code, 1, "a blocking diff exits 1");
    assert!(stdout.contains("parity FAILED"), "{stdout}");
    assert!(stdout.contains("tool_result"), "{stdout}");
}

#[test]
fn without_the_whitelist_the_carrier_differences_block() {
    let report = json(&[
        fixture("print-3turn.json").to_str().unwrap(),
        fixture("pty-3turn.json").to_str().unwrap(),
        "--no-whitelist",
    ]);
    assert_eq!(report["equal"], false);
    assert!(report["whitelisted"].as_array().unwrap().is_empty());
    // Hook lifecycle events are only dropped by a rule, so they now surface.
    let kinds: Vec<&str> = report["differences"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|difference| difference["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"lifecycle"), "{kinds:?}");
    assert!(kinds.contains(&"usage"), "{kinds:?}");
}

#[test]
fn a_dump_compared_against_itself_is_always_equal() {
    for name in [
        "print-3turn.json",
        "pty-3turn.json",
        "pty-3turn-missing-tool-result.json",
    ] {
        let path = fixture(name);
        let report = json(&[
            path.to_str().unwrap(),
            path.to_str().unwrap(),
            "--no-whitelist",
        ]);
        assert_eq!(report["equal"], true, "{name} differs from itself");
    }
}

#[test]
fn the_human_diff_is_plain_text_unless_color_is_requested() {
    let left = fixture("print-3turn.json");
    let right = fixture("pty-3turn.json");
    let (_, plain) = diff(&[left.to_str().unwrap(), right.to_str().unwrap()]);
    assert!(
        !plain.contains('\u{1b}'),
        "piped output must not be colored"
    );
    assert!(plain.contains("parity ok"), "{plain}");

    let (_, colored) = diff(&[
        left.to_str().unwrap(),
        right.to_str().unwrap(),
        "--color",
        "always",
    ]);
    assert!(
        colored.contains("\u{1b}[33m"),
        "whitelisted lines are amber"
    );
    assert!(colored.contains("\u{1b}[32m"), "the ok verdict is green");
}

#[test]
fn a_hook_derived_turn_edge_is_excused_by_the_turn_live_rule_but_not_other_lifecycles() {
    // The live layer journals UserPromptSubmit/Stop on the `turn` topic;
    // `claude print` emits neither. The whitelist rule must excuse exactly
    // that unmatched turn lifecycle, and the gate must still block an
    // unmatched lifecycle on another topic.
    let dir = tempfile::tempdir().unwrap();
    let baseline = json!({
        "instanceId": "ins_x",
        "observations": [{
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "topic": "turn",
                "nativeName": "turn-completed",
                "status": {"state": "known", "value": "end_turn"},
                "severity": "info",
                "affectsCompletion": true
            }
        }]
    });
    // PTY side additionally carries the hook-derived turn START edge.
    let with_start = json!({
        "instanceId": "ins_x",
        "observations": [
            {
                "kind": "lifecycle",
                "payload": {
                    "type": "native",
                    "topic": "turn",
                    "nativeName": "UserPromptSubmit",
                    "status": {"state": "known", "value": "working"},
                    "severity": "info",
                    "affectsCompletion": false,
                    "relatedIds": {"phase": "prompt-accepted", "tier": "hook"}
                }
            },
            baseline["observations"][0].clone()
        ]
    });
    let base = dir.path().join("base.json");
    let pty = dir.path().join("pty.json");
    std::fs::write(&base, serde_json::to_vec(&baseline).unwrap()).unwrap();
    std::fs::write(&pty, serde_json::to_vec(&with_start).unwrap()).unwrap();

    let report = json(&[base.to_str().unwrap(), pty.to_str().unwrap()]);
    assert_eq!(report["equal"], true, "{report}");
    let whitelisted = report["whitelisted"].as_array().unwrap();
    assert!(
        whitelisted
            .iter()
            .any(|d| d["kind"] == "lifecycle" && d["topic"] == "turn"),
        "the turn.live unmatched edge must be whitelisted: {whitelisted:?}"
    );

    // An unmatched lifecycle on a different topic still blocks.
    let stray = json!({
        "instanceId": "ins_x",
        "observations": [
            {
                "kind": "lifecycle",
                "payload": {
                    "type": "native",
                    "topic": "configuration",
                    "nativeName": "settings-changed",
                    "status": {"state": "known", "value": "idle"},
                    "severity": "info",
                    "affectsCompletion": false
                }
            },
            baseline["observations"][0].clone()
        ]
    });
    let stray_path = dir.path().join("stray.json");
    std::fs::write(&stray_path, serde_json::to_vec(&stray).unwrap()).unwrap();
    let report = json(&[base.to_str().unwrap(), stray_path.to_str().unwrap()]);
    assert_eq!(
        report["equal"], false,
        "an unmatched non-turn lifecycle must not be excused"
    );
}

#[test]
fn the_shipped_whitelist_parses_and_every_rule_states_a_reason() {
    let text = std::fs::read_to_string(repo_root().join("docs/design/parity-whitelist.toml"))
        .expect("read the shipped whitelist");
    let parsed: toml::Value = toml::from_str(&text).expect("whitelist is valid TOML");
    assert_eq!(parsed["version"].as_integer(), Some(1));
    let rules = parsed["rule"].as_array().expect("rules");
    assert!(!rules.is_empty());
    for rule in rules {
        let reason = rule["reason"].as_str().unwrap_or_default();
        assert!(
            reason.trim().len() > 20,
            "a rule needs a real reason, not a placeholder: {rule:?}"
        );
        assert!(
            ["kind", "topic", "lifecycle"]
                .iter()
                .any(|selector| rule.get(selector).is_some()),
            "a rule without a selector matches nothing: {rule:?}"
        );
    }
}

#[test]
fn a_malformed_dump_is_an_error_not_a_silent_pass() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.json");
    std::fs::write(&empty, "[]").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .current_dir(repo_root())
        .args(["journal", "diff"])
        .arg(fixture("print-3turn.json"))
        .arg(&empty)
        .output()
        .expect("run");
    assert_ne!(output.status.code(), Some(0), "an empty dump cannot pass");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no observations"), "{stderr}");
}

#[test]
fn the_fixtures_match_what_the_generator_produces() {
    // Regenerating must be a no-op: a hand-edited fixture would silently
    // decouple the two sides and the gate would stop proving anything.
    //
    // Generated into a temp dir and compared, rather than regenerated in
    // place. Rewriting the checked-in files made this test mutate shared
    // state: every other test that reads those fixtures could catch a
    // truncated write, which is how a colour assertion three tests away ended
    // up failing under the gate's parallel run. A test that proves the
    // fixtures are current has no reason to touch them.
    let generator = fixture("generate.py");
    let out = tempfile::tempdir().expect("temp dir");
    let output = Command::new("python3")
        .arg(&generator)
        .arg("--out")
        .arg(out.path())
        .output()
        .expect("python3 is available");
    assert!(
        output.status.success(),
        "generator failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in [
        "print-3turn.json",
        "pty-3turn.json",
        "pty-3turn-missing-tool-result.json",
    ] {
        let committed = std::fs::read_to_string(fixture(name)).expect("read fixture");
        let generated =
            std::fs::read_to_string(out.path().join(name)).expect("read generated fixture");
        assert_eq!(
            generated, committed,
            "{name} drifted from generate.py; regenerate instead of hand-editing"
        );
    }
}
