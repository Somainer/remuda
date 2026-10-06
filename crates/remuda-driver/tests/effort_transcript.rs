//! §9.1 transcript effort read-back: `effort` observations, slash detection,
//! and dedupe — driven through the real [`TranscriptMapper`] with synthetic
//! records shaped exactly like claude 2.1.221 writes them, plus verbatim
//! replays of real claude PTY sessions:
//!
//! - 2.1.272/2.1.273 coupled fixtures (`fixtures/effort-21272/`,
//!   `fixtures/effort-21273/`, evidence effort-sync-2/3): ultracode is the
//!   xhigh level plus the workflow flag;
//! - 2.1.289 decoupled fixtures (`fixtures/effort-21289/`, evidence
//!   effort-sync-4, ADR D-056): ultracode is its own toggle that latches at
//!   every level.

use remuda_driver::TranscriptMapper;
use remuda_protocol::{
    DriverKind, EffortSource, HostId, Id, InstanceId, ObservationPayload, RunId,
};
use serde_json::json;

fn mapper() -> TranscriptMapper {
    mapper_version("2.1.221")
}

fn mapper_version(version: &str) -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ShellPty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "effort-session".into(),
        version.into(),
    )
}

fn assistant(effort: Option<&str>, n: u64) -> String {
    let mut record = json!({
        "type": "assistant",
        "uuid": format!("msg-{n}"),
        "sessionId": "effort-session",
        "message": {
            "id": format!("msg-{n}"),
            "role": "assistant",
            "type": "message",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
        },
        "effort": effort,
        "perTurnEffort": null,
    });
    if effort.is_none() {
        record.as_object_mut().unwrap().remove("effort");
        record.as_object_mut().unwrap().remove("perTurnEffort");
    }
    record.to_string()
}

fn slash(word: &str, n: u64) -> String {
    json!({
        "type": "user",
        "uuid": format!("cmd-{n}"),
        "sessionId": "effort-session",
        "message": {
            "role": "user",
            "content": format!(
                "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n\
                 <command-args>{word}</command-args>"
            )
        }
    })
    .to_string()
}

fn effort_edges(out: &[remuda_protocol::Observation]) -> Vec<(String, EffortSource, Option<bool>)> {
    out.iter()
        .filter_map(|obs| match &obs.body {
            ObservationPayload::Effort(payload) => Some((
                payload.effective.name.wire().to_string(),
                payload.effective.source,
                payload.effective.ultracode,
            )),
            _ => None,
        })
        .collect()
}

trait WireName {
    fn wire(&self) -> &'static str;
}
impl WireName for remuda_protocol::EffortName {
    fn wire(&self) -> &'static str {
        match self {
            remuda_protocol::EffortName::Low => "low",
            remuda_protocol::EffortName::Medium => "medium",
            remuda_protocol::EffortName::High => "high",
            remuda_protocol::EffortName::Xhigh => "xhigh",
            remuda_protocol::EffortName::Max => "max",
            remuda_protocol::EffortName::Ultra => "ultra",
            // Legacy input word; never observed on a Claude record.
            remuda_protocol::EffortName::Minimal => "minimal",
        }
    }
}

#[test]
fn the_first_assistant_record_emits_once_then_identical_records_are_deduped() {
    let mut mapper = mapper();
    let out = mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("high".to_string(), EffortSource::Unknown, None)]
    );
    // Two more identical high records emit nothing.
    assert!(
        mapper
            .map_line(&assistant(Some("high"), 2))
            .expect("map")
            .iter()
            .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_)))
    );
    assert!(
        mapper
            .map_line(&assistant(Some("high"), 3))
            .expect("map")
            .iter()
            .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_)))
    );
}

#[test]
fn a_typed_effort_slash_marks_the_next_new_level_as_source_slash() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    mapper.map_line(&slash("xhigh", 1)).expect("map");
    let out = mapper.map_line(&assistant(Some("xhigh"), 2)).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("xhigh".to_string(), EffortSource::Slash, None)]
    );
}

#[test]
fn ultracode_slash_reads_back_as_xhigh_with_the_flag() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    mapper.map_line(&slash("ultracode", 1)).expect("map");
    let out = mapper.map_line(&assistant(Some("xhigh"), 2)).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("xhigh".to_string(), EffortSource::Slash, Some(true))]
    );
}

#[test]
fn per_turn_effort_is_the_fallback_field() {
    let mut record = serde_json::from_str::<serde_json::Value>(&assistant(None, 1)).unwrap();
    record["perTurnEffort"] = json!("medium");
    let out = mapper().map_line(&record.to_string()).expect("map");
    assert_eq!(
        effort_edges(&out),
        vec![("medium".to_string(), EffortSource::Unknown, None)]
    );
}

#[test]
fn an_unrelated_slash_command_does_not_attribute_effort() {
    let mut mapper = mapper();
    mapper.map_line(&assistant(Some("high"), 1)).expect("map");
    mapper
        .map_line(
            &json!({
                "type": "user",
                "uuid": "cmd-x",
                "sessionId": "effort-session",
                "message": {"role": "user", "content": "<command-name>/clear</command-name>"}
            })
            .to_string(),
        )
        .expect("map");
    // A later high record is unchanged and stays silent.
    let out = mapper.map_line(&assistant(Some("high"), 2)).expect("map");
    assert!(effort_edges(&out).is_empty());
}

// ───────── Real claude 2.1.272 PTY session, replayed verbatim ───────────────

const WALK_21272: &str = include_str!("fixtures/effort-21272/effort-walk-21272.jsonl");
const REJECT_21272: &str = include_str!("fixtures/effort-21272/effort-reject-21272.jsonl");

#[derive(Debug, PartialEq)]
struct EffortEdge {
    name: String,
    source: EffortSource,
    ultracode: Option<bool>,
}

fn replay(path: &str) -> Vec<EffortEdge> {
    replay_version(path, "2.1.221")
}

fn replay_version(path: &str, version: &str) -> Vec<EffortEdge> {
    let mut mapper = mapper_version(version);
    let mut edges = Vec::new();
    for line in path.lines().filter(|l| !l.trim().is_empty()) {
        for obs in mapper.map_line(line).expect("map") {
            if let ObservationPayload::Effort(payload) = &obs.body {
                edges.push(EffortEdge {
                    name: payload.effective.name.wire().to_string(),
                    source: payload.effective.source,
                    ultracode: payload.effective.ultracode,
                });
            }
        }
    }
    edges
}

/// `(name, source, ultracode)` triples; the exact observation sequence the
/// 2.1.272/2.1.273 and 2.1.289 replays must emit.
fn triples(edges: &[EffortEdge]) -> Vec<(&str, EffortSource, Option<bool>)> {
    edges
        .iter()
        .map(|edge| (edge.name.as_str(), edge.source, edge.ultracode))
        .collect()
}

#[test]
fn real_21272_walk_emits_the_exact_coupled_observation_sequence() {
    let edges = replay(WALK_21272);
    // Byte-for-byte the same list the coupled fixtures produced before D-056:
    // low baseline; xhigh accepted (flag off); ultracode accepted at xhigh
    // (flag on); high accepted clears the flag. The Esc-on-dialog max never
    // settles.
    assert_eq!(
        triples(&edges),
        vec![
            ("low", EffortSource::Unknown, None),
            ("xhigh", EffortSource::Slash, Some(false)),
            ("xhigh", EffortSource::Slash, Some(true)),
            ("high", EffortSource::Slash, Some(false)),
        ]
    );
}

#[test]
fn real_21272_walk_settles_every_level_from_the_stdout_verdict() {
    // The real walk: low baseline assistant; xhigh accepted; max dismissed
    // (Kept); ultracode accepted; high accepted (ultra exits).
    let edges = replay(WALK_21272);
    let words: Vec<_> = edges
        .iter()
        .map(|e| (e.name.as_str(), e.ultracode))
        .collect();
    // The xhigh edge arrives from the command verdict — before the next
    // assistant record exists — and the ultracode edge carries the flag.
    assert!(
        words.contains(&("xhigh", Some(false))),
        "xhigh stdout accept: {words:?}"
    );
    assert!(
        words.contains(&("xhigh", Some(true))),
        "ultracode stdout accept: {words:?}"
    );
    assert!(
        words.contains(&("high", Some(false))),
        "high stdout accept clears the flag: {words:?}"
    );
    // A dismissed dialog must never emit max.
    assert!(
        !words.iter().any(|(name, _)| name == &"max"),
        "the Esc-on-dialog max never takes effect: {words:?}"
    );
    // The post-ultracode assistant record carries xhigh and the flag stays
    // latched (it was already emitted at the verdict, so this is deduped — the
    // latched flag is what keeps a later xhigh record honest).
}

#[test]
fn real_21272_reject_records_emit_no_effort_edges() {
    // Esc on the "Change effort to max" dialog → Kept; /effort bogus → Invalid.
    // Neither changes the effective level, so no effort observation is emitted.
    assert_eq!(replay(REJECT_21272), Vec::<EffortEdge>::new());
}

#[tokio::test]
async fn real_21272_ultracode_switch_resolves_its_bridge_from_stdout_not_the_turn() {
    // Wire the mapper to a bridge the way the driver does, arm an ultracode
    // switch, and replay ONLY the slash + stdout pair: the generation resolves
    // Applied(xhigh, ultracode:true) without any assistant record.
    use std::sync::Arc;
    use std::time::Duration;
    let bridge = Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = mapper_with_bridge(&bridge);
    let generation = bridge.arm_ultracode();
    let slash = WALK_21272
        .lines()
        .find(|l| l.contains("<command-args>ultracode</command-args>"))
        .expect("ultracode slash record");
    mapper.map_line(slash).expect("map");
    let stdout = WALK_21272
        .lines()
        .find(|l| l.contains("Set effort level to ultracode"))
        .expect("ultracode stdout record");
    mapper.map_line(stdout).expect("map");
    let verdict = bridge.wait(generation, Duration::from_secs(1)).await;
    match verdict {
        Some(remuda_driver::effort::Readback::Applied(observed)) => {
            assert_eq!(observed.name, remuda_protocol::EffortName::Xhigh);
            assert_eq!(observed.ultracode, Some(true));
        }
        other => panic!("expected Applied ultracode, got {other:?}"),
    }
}

#[tokio::test]
async fn real_21272_dismissed_dialog_rejects_the_bridge_with_a_reason() {
    use std::sync::Arc;
    use std::time::Duration;
    let bridge = Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = mapper_with_bridge(&bridge);
    let generation = bridge.arm_max();
    for line in REJECT_21272.lines().take(2) {
        mapper.map_line(line).expect("map");
    }
    match bridge.wait(generation, Duration::from_secs(1)).await {
        Some(remuda_driver::effort::Readback::Rejected { reason }) => {
            assert_eq!(reason, "dialog-kept");
        }
        other => panic!("expected Kept rejection, got {other:?}"),
    }
}

fn mapper_with_bridge(
    bridge: &std::sync::Arc<remuda_driver::test_support::Bridge>,
) -> TranscriptMapper {
    remuda_driver::test_support::mapper_with_bridge(bridge.clone(), "effort-session", "2.1.272")
}

// ───────── Real claude 2.1.273 PTY session, four switches in one run ────────

const WALK_21273: &str = include_str!("fixtures/effort-21273/effort-multi-switch-21273.jsonl");

#[tokio::test]
async fn real_21273_four_consecutive_switches_each_resolve_their_own_generation() {
    // The owner's c-effort3 report: the first switch (ultracode) read back, but
    // the next switch ended with no-readback-within-window. Replay the verbatim
    // 2.1.273 transcript — low baseline, ultracode → high → ultracode → max with
    // real turns between them — arming a fresh generation before each slash
    // record exactly as `perform_switch` does, and assert EVERY generation
    // resolves Applied with the tier and ultracode flag the verdict carries.
    use std::time::Duration;
    let bridge = std::sync::Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = remuda_driver::test_support::mapper_with_bridge(
        bridge.clone(),
        "effort-session",
        "2.1.273",
    );

    // (slash word, expected tier, expected ultracode flag), in fixture order.
    let expected: [(&str, &str, Option<bool>); 4] = [
        ("ultracode", "xhigh", Some(true)),
        ("high", "high", Some(false)),
        ("ultracode", "xhigh", Some(true)),
        ("max", "max", Some(false)),
    ];
    let mut switch = 0usize;
    let mut pending: Option<(u64, &str, Option<bool>)> = None;
    let mut resolved: Vec<(&str, Option<bool>)> = Vec::new();
    let mut edges: Vec<(String, Option<bool>)> = Vec::new();
    for line in WALK_21273.lines().filter(|l| !l.trim().is_empty()) {
        // Arm before the slash record is mapped: perform_switch arms before it
        // types, and the slash record is the first record the command produces.
        if line.contains("<command-name>/effort</command-name>") {
            let (word, tier, flag) = expected[switch];
            assert!(line.contains(&format!("<command-args>{word}</command-args>")));
            let generation = bridge.arm_word(word);
            pending = Some((generation, tier, flag));
            switch += 1;
        }
        for obs in mapper.map_line(line).expect("map") {
            if let ObservationPayload::Effort(payload) = &obs.body {
                edges.push((
                    payload.effective.name.wire().to_string(),
                    payload.effective.ultracode,
                ));
            }
        }
        // The slash record is immediately followed by its stdout verdict line.
        if line.contains("<local-command-stdout>")
            && let Some((generation, tier, flag)) = pending.take()
        {
            match bridge.wait(generation, Duration::from_secs(1)).await {
                Some(remuda_driver::effort::Readback::Applied(observed)) => {
                    assert_eq!(observed.name.wire(), tier, "stdout line: {line}");
                    assert_eq!(observed.ultracode, flag);
                    resolved.push((tier, flag));
                }
                other => panic!("switch {switch} did not resolve Applied: {other:?}\n{line}"),
            }
        }
    }
    assert_eq!(resolved.len(), 4, "all four switches resolved");
    assert!(
        !bridge.has_pending(),
        "no generation left pending after the walk"
    );
    // Edges include every accepted tier, the flag on → off → on, then max, so
    // no switch is silently dropped by dedup.
    for (tier, flag) in [
        ("xhigh", Some(true)),
        ("high", Some(false)),
        ("xhigh", Some(true)),
        ("max", Some(false)),
    ] {
        assert!(
            edges
                .iter()
                .any(|(name, edge_flag)| name == tier && *edge_flag == flag),
            "missing edge ({tier}, {flag:?}): {edges:?}"
        );
    }
}

// ───────── Real claude 2.1.289 recordings (ultracode decoupled) ─────────────

const WALK_21289: &str = include_str!("fixtures/effort-21289/effort-walk-21289.jsonl");

#[test]
fn real_21273_walk_emits_the_exact_coupled_observation_sequence() {
    let edges = replay_version(WALK_21273, "2.1.273");
    assert_eq!(
        triples(&edges),
        vec![
            ("low", EffortSource::Unknown, None),
            ("xhigh", EffortSource::Slash, Some(true)),
            ("high", EffortSource::Slash, Some(false)),
            ("xhigh", EffortSource::Slash, Some(true)),
            ("max", EffortSource::Slash, Some(false)),
        ]
    );
}

#[test]
fn real_21289_walk_emits_the_full_decoupled_observation_sequence() {
    // effort-sync-4 walk, acceptance order (table §3):
    //  0 launch baseline                         high / flag unknown
    //  1 /effort ultracode                       high, flag ON (stays high)
    //  2 /effort high                            no edge (level, flag same)
    //  3 /effort max                             max, flag stays on
    //  4 /effort ultracode off                   max, flag OFF
    //  5 /effort ultracode on                    max, flag ON
    //  6 /effort xhigh                           xhigh, flag stays on
    //  7 /effort auto                            resolves to medium, flag on
    //  8 /effort status                          observation only (no edge)
    //  9 /effort bogus                           invalid (no edge; sparse
    //                                            enter repeat is no edge)
    // 10 /effort ultracode bogus                 invalid (no edge)
    // 11 bare slider → high · Ultracode off      high, flag off
    // 12 /model                                  no effort suffix on 2.1.289
    let edges = replay_version(WALK_21289, "2.1.289");
    assert_eq!(
        triples(&edges),
        vec![
            ("high", EffortSource::Unknown, None),
            ("high", EffortSource::Slash, Some(true)),
            ("max", EffortSource::Slash, Some(true)),
            ("max", EffortSource::Slash, Some(false)),
            ("max", EffortSource::Slash, Some(true)),
            ("xhigh", EffortSource::Slash, Some(true)),
            ("medium", EffortSource::Unknown, Some(true)),
            ("high", EffortSource::Slash, Some(false)),
        ]
    );
}

/// One expected effort edge: `(level wire word, source, ultracode)`.
type ExpectedEdge = (&'static str, EffortSource, Option<bool>);

#[test]
fn real_21289_launch_fixtures_each_emit_their_exact_sequence() {
    // (fixture, expected (level, source, ultracode) edges). The status line
    // appears in every case but emits only when it adds flag/level evidence.
    let cases: [(&str, &[ExpectedEdge]); 8] = [
        // a: --effort high + overlay ultracode; the enter attachment latches
        // before the first assistant record and rides its high edge.
        (
            "effort-launch-a-21289.jsonl",
            &[("high", EffortSource::Unknown, Some(true))],
        ),
        // b: --effort ultracode = xhigh + flag.
        (
            "effort-launch-b-21289.jsonl",
            &[("xhigh", EffortSource::Unknown, Some(true))],
        ),
        // c1: second --settings wins whole: only its ultracode applied;
        // auto resolves to medium.
        (
            "effort-launch-c1-21289.jsonl",
            &[("medium", EffortSource::Unknown, Some(true))],
        ),
        // c2: the file --settings won, so the JSON's ultracode did NOT apply;
        // first medium edge carries no flag, status positively reports off.
        (
            "effort-launch-c2-21289.jsonl",
            &[
                ("medium", EffortSource::Unknown, None),
                ("medium", EffortSource::Unknown, Some(false)),
            ],
        ),
        // d: workflows disabled — the refusal changes nothing; the status
        // line reports flag off.
        (
            "effort-launch-d-21289.jsonl",
            &[
                ("medium", EffortSource::Unknown, None),
                ("medium", EffortSource::Unknown, Some(false)),
            ],
        ),
        // e: cap clamp — /effort max accepted at high; status reports off.
        (
            "effort-launch-e-21289.jsonl",
            &[
                ("high", EffortSource::Slash, None),
                ("high", EffortSource::Unknown, Some(false)),
            ],
        ),
        // f2: a resumed process (first f1's session): ultracode stays off,
        // /effort ultracode on turns it on at high; the resume-exit after
        // /exit then clears it as the level reverts to auto→medium.
        (
            "effort-launch-f2-21289.jsonl",
            &[
                ("high", EffortSource::Slash, Some(true)),
                ("high", EffortSource::Slash, Some(false)),
                ("medium", EffortSource::Unknown, Some(false)),
            ],
        ),
        // g: model without xhigh — ultracode refused, xhigh verbally accepted
        // but the assistant runs at high; status confirms the flag off.
        (
            "effort-launch-g-21289.jsonl",
            &[
                ("high", EffortSource::Unknown, None),
                ("xhigh", EffortSource::Slash, None),
                ("high", EffortSource::Unknown, None),
                ("high", EffortSource::Unknown, Some(false)),
            ],
        ),
    ];
    for (name, expected) in cases {
        let body = fixture_21289(name);
        assert_eq!(
            triples(&replay_version(&body, "2.1.289")),
            expected,
            "{name}"
        );
    }
}

fn fixture_21289(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/effort-21289")
            .join(name),
    )
    .expect("fixture")
}

/// Feed one fixture through a mapper attached to `bridge`, arming before each
/// `/effort` slash record exactly like `perform_switch` does. Returns the
/// `(slash args, readback)` pairs for every command in file order, and whether
/// any armed generation was left pending.
async fn armed_readbacks(
    body: &str,
    version: &str,
) -> (Vec<(String, Option<remuda_driver::effort::Readback>)>, bool) {
    use std::time::Duration;
    let bridge = std::sync::Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper =
        remuda_driver::test_support::mapper_with_bridge(bridge.clone(), "effort-session", version);
    let mut results = Vec::new();
    // The slash record arms; the verdict is the NEXT record, so carry the
    // armed generation across iterations and wait after the stdout line maps.
    let mut pending: Option<(String, u64)> = None;
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let arm = if line.contains("<command-name>/effort</command-name>") {
            let args = remuda_protocol::slash_effort_args(
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("json")
                    .pointer("/message/content")
                    .and_then(|c| c.as_str())
                    .expect("content"),
            )
            .expect("args");
            match remuda_protocol::classify_effort_slash(
                &args,
                remuda_protocol::EffortSemantics::Decoupled,
            ) {
                // Only level words and flag toggles are Remuda-armable; auto,
                // status and invalid arguments never get a generation.
                remuda_protocol::EffortSlash::Level(_)
                | remuda_protocol::EffortSlash::UltracodeOn { .. }
                | remuda_protocol::EffortSlash::UltracodeOff => {
                    bridge.arm_args(&args).map(|generation| (args, generation))
                }
                remuda_protocol::EffortSlash::Slider | remuda_protocol::EffortSlash::NotASwitch => {
                    None
                }
            }
        } else {
            None
        };
        if let Some(armed) = arm {
            pending = Some(armed);
        }
        mapper.map_line(line).expect("map");
        if line.contains("<local-command-stdout>")
            && let Some((args, generation)) = pending.take()
        {
            let readback = bridge.wait(generation, Duration::from_secs(1)).await;
            results.push((args, readback));
        }
    }
    let still_pending = bridge.has_pending();
    (results, still_pending)
}

#[tokio::test]
async fn real_21289_walk_flag_and_level_verdict_settle_every_armed_switch() {
    // The acceptance read-backs, replayed from the verdict records — no 10 s
    // timeout path, and refusals reject immediately with a stable reason.
    let (got, pending) = armed_readbacks(WALK_21289, "2.1.289").await;
    use remuda_driver::effort::Readback;
    let applied = |word: &str,
                   name: remuda_protocol::EffortName,
                   flag: Option<bool>|
     -> (String, Option<Readback>) {
        (
            word.to_string(),
            Some(Readback::Applied(remuda_protocol::ObservedEffort {
                name,
                ultracode: flag,
            })),
        )
    };
    assert_eq!(
        got,
        vec![
            // 1 Remuda-armed `/effort ultracode` resolves Applied, flag on, at
            // the current high level — the string that used to parse as Other.
            applied("ultracode", remuda_protocol::EffortName::High, Some(true)),
            // 2 /effort high with ultracode on yields {high, ultracode:true}:
            // the plain verdict preserves the latched flag.
            applied("high", remuda_protocol::EffortName::High, Some(true)),
            applied("max", remuda_protocol::EffortName::Max, Some(true)),
            // 4 /effort ultracode off yields {max, false}.
            applied(
                "ultracode off",
                remuda_protocol::EffortName::Max,
                Some(false)
            ),
            applied("ultracode on", remuda_protocol::EffortName::Max, Some(true)),
            applied("xhigh", remuda_protocol::EffortName::Xhigh, Some(true)),
        ]
    );
    // Steps 7 (auto), 8 (status), 9/10 (invalid words) and 11 (bare slider)
    // are not Remuda-armable, so they never got a generation — and status/
    // invalid verdicts never touch a pending bridge.
    assert!(!pending, "no generation left pending after the walk");
}

#[tokio::test]
async fn invalid_argument_in_either_order_rejects_an_armed_switch_immediately() {
    // Defensive path: Remuda only ever types valid words, but a future CLI
    // vocabulary change could answer Invalid to one. The mapper rejects the
    // generation immediately instead of waiting out the read-back window.
    use std::time::Duration;
    for verdict in [
        "Invalid argument: max. Valid options are: low, medium, high, xhigh, max, auto, ultracode \
         [on|off]",
        // The new 2.1.289 list order keeps `ultracode bogus` verbatim too.
        "Invalid argument: ultracode bogus. Valid options are: low, medium, high, xhigh, max, \
         auto, ultracode [on|off]",
    ] {
        let bridge = std::sync::Arc::new(remuda_driver::test_support::Bridge::new());
        let mut mapper = remuda_driver::test_support::mapper_with_bridge(
            bridge.clone(),
            "effort-session",
            "2.1.289",
        );
        let generation = bridge.arm_word("max");
        mapper.map_line(&slash("max", 1)).expect("slash");
        mapper
            .map_line(&stdout_record_version(verdict, 2, "2.1.289"))
            .expect("stdout");
        match bridge.wait(generation, Duration::from_secs(1)).await {
            Some(remuda_driver::effort::Readback::Rejected { reason }) => {
                assert_eq!(reason, "invalid-argument");
            }
            other => panic!("{verdict} -> {other:?}"),
        }
    }
}

fn stdout_record_version(stdout: &str, n: u64, version: &str) -> String {
    json!({
        "type": "user",
        "uuid": format!("out-{n}"),
        "sessionId": "effort-session",
        "version": version,
        "message": {
            "role": "user",
            "content": format!("<local-command-stdout>{stdout}</local-command-stdout>")
        }
    })
    .to_string()
}

#[tokio::test]
async fn resumed_history_ultracode_on_starts_off_and_only_the_new_verdict_settles() {
    // D-056 (4): resume a session whose history says "Ultracode on". The
    // replayed records set no state; the current state starts off; and a fresh
    // switch armed during the replay is settled ONLY by its own post-launch
    // verdict — never by a same-words verdict from history.
    use std::time::Duration;
    let bridge = std::sync::Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = remuda_driver::test_support::resume_mapper(
        remuda_driver::test_support::mapper_with_bridge(
            bridge.clone(),
            "effort-session",
            "2.1.289",
        ),
    );

    // Prior session: assistant high, `/effort ultracode`, its on verdict, and
    // the next assistant record still at high with the flag latched.
    let history = [
        assistant(Some("high"), 1),
        slash("ultracode", 2),
        stdout_record_version(
            "Ultracode on (this session only): dynamic workflows on every task. Effort stays high.",
            3,
            "2.1.289",
        ),
        assistant(Some("high"), 4),
    ];
    let mut edges = Vec::new();
    for line in &history {
        edges.extend(effort_edges(&mapper.map_line(line).expect("map")));
    }
    assert!(
        edges.is_empty(),
        "replayed history emitted effort edges: {edges:?}"
    );

    // Arm the NEW switch before the process boundary. A replayed high slash
    // and its verdict arrive while still in history — they must not claim it.
    let generation = bridge.arm_word("high");
    mapper.map_line(&slash("high", 5)).expect("replayed slash");
    mapper
        .map_line(&stdout_record_version(
            "Set effort level to high (saved as your default for new sessions): Comprehensive \
             implementation with extensive testing and documentation",
            6,
            "2.1.289",
        ))
        .expect("replayed verdict");
    assert!(
        bridge.has_pending(),
        "a replayed verdict must not settle the fresh switch"
    );
    assert!(
        bridge
            .wait(generation, Duration::from_millis(100))
            .await
            .is_none(),
        "history produced no readback for the fresh generation"
    );

    // First post-launch record flips the gate; the fresh switch's OWN slash
    // and verdict then settle it.
    remuda_driver::test_support::mark_current_process(&mut mapper, true);
    mapper.map_line(&slash("high", 7)).expect("fresh slash");
    mapper
        .map_line(&stdout_record_version(
            "Set effort level to high (saved as your default for new sessions): Comprehensive \
             implementation with extensive testing and documentation",
            8,
            "2.1.289",
        ))
        .expect("fresh verdict");
    match bridge.wait(generation, Duration::from_secs(1)).await {
        Some(remuda_driver::effort::Readback::Applied(observed)) => {
            assert_eq!(observed.name, remuda_protocol::EffortName::High);
        }
        other => panic!("the fresh switch was not settled by its own verdict: {other:?}"),
    }
    assert!(!bridge.has_pending());
}

#[tokio::test]
async fn real_21289_workflows_disabled_refuses_immediately_with_the_reason() {
    let body = fixture_21289("effort-launch-d-21289.jsonl");
    let (got, _) = armed_readbacks(&body, "2.1.289").await;
    assert_eq!(
        got.len(),
        1,
        "only the /effort ultracode command is armable"
    );
    assert_eq!(
        got[0],
        (
            "ultracode".to_string(),
            Some(remuda_driver::effort::Readback::Rejected {
                reason: "ultracode-workflows-disabled".to_string(),
            })
        )
    );
}

#[tokio::test]
async fn real_21289_model_unavailable_refuses_immediately_with_the_reason() {
    let body = fixture_21289("effort-launch-g-21289.jsonl");
    let (got, _) = armed_readbacks(&body, "2.1.289").await;
    // ultracode refused for the model; the later xhigh applies verbally.
    assert_eq!(
        got[0],
        (
            "ultracode".to_string(),
            Some(remuda_driver::effort::Readback::Rejected {
                reason: "ultracode-unavailable-for-model".to_string(),
            })
        )
    );
    match &got[1] {
        (word, Some(remuda_driver::effort::Readback::Applied(observed))) => {
            assert_eq!(word, "xhigh");
            assert_eq!(observed.name, remuda_protocol::EffortName::Xhigh);
        }
        other => panic!("xhigh verbal accept: {other:?}"),
    }
}

#[tokio::test]
async fn env_override_verdict_rejects_with_the_env_reason() {
    // D-056 open question 3 for the 2.1.289 wording; the 2.1.277 wording was
    // measured with the throwaway probe. A Remuda-armed /effort max pinned by
    // CLAUDE_CODE_EFFORT_LEVEL must reject immediately, not time out.
    use std::time::Duration;
    let bridge = std::sync::Arc::new(remuda_driver::test_support::Bridge::new());
    let mut mapper = remuda_driver::test_support::mapper_with_bridge(
        bridge.clone(),
        "effort-session",
        "2.1.277",
    );
    let generation = bridge.arm_word("max");
    mapper.map_line(&slash("max", 1)).expect("slash");
    mapper
        .map_line(&stdout_record_version(
            "Not applied: CLAUDE_CODE_EFFORT_LEVEL=high overrides effort this session, and max is \
             session-only (nothing saved)",
            2,
            "2.1.277",
        ))
        .expect("stdout");
    match bridge.wait(generation, Duration::from_secs(1)).await {
        Some(remuda_driver::effort::Readback::Rejected { reason }) => {
            assert_eq!(reason, "env-override");
        }
        other => panic!("expected env-override rejection, got {other:?}"),
    }
}

#[test]
fn model_verdict_with_the_effort_suffix_emits_an_effort_observation() {
    // D-056 §4: `Set model to … with `<level>` effort` is an effort
    // observation attributed like the model switch; the 2.1.289 fixtures carry
    // no such suffix, so this is a synthetic record in the measured shape.
    let mut mapper = mapper_version("2.1.289");
    mapper
        .map_line(&assistant(Some("high"), 1))
        .expect("baseline");
    let out = mapper
        .map_line(
            &json!({
                "type": "user",
                "uuid": "m1",
                "sessionId": "effort-session",
                "version": "2.1.289",
                "message": {
                    "role": "user",
                    "content": "<local-command-stdout>Set model to `Opus 5.5` and saved as your \
                                default for new sessions with `xhigh` effort</local-command-stdout>"
                }
            })
            .to_string(),
        )
        .expect("model stdout");
    let edge = out
        .iter()
        .find_map(|obs| match &obs.body {
            ObservationPayload::Effort(payload) => Some(payload),
            _ => None,
        })
        .expect("effort edge from the /model suffix");
    assert_eq!(edge.effective.name, remuda_protocol::EffortName::Xhigh);
    // Without the suffix the 2.1.289 walk's /model verdict emits no edge:
    let mut mapper2 = mapper_version("2.1.289");
    mapper2
        .map_line(&assistant(Some("high"), 1))
        .expect("baseline");
    let out = mapper2
        .map_line(
            &json!({
                "type": "user",
                "uuid": "m2",
                "sessionId": "effort-session",
                "version": "2.1.289",
                "message": {
                    "role": "user",
                    "content": "<local-command-stdout>Set model to `Opus 5.5` and saved as your \
                                default for new sessions</local-command-stdout>"
                }
            })
            .to_string(),
        )
        .expect("model stdout no suffix");
    assert!(
        out.iter()
            .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
        "no effort edge without the suffix"
    );
}

// ───────── Live/journal parity on the real 2.1.289 recordings ───────────────

use remuda_journal::{MapContext, NativeIds, map_claude_line};
use remuda_protocol::{FileCursor, SourceChannel, U64};

type EdgeTriple = (String, EffortSource, Option<bool>);

fn journal_edges(body: &str, instance: &InstanceId) -> Vec<(EdgeTriple, String)> {
    let map = MapContext::claude_file(
        instance.clone(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "effort-session",
        SourceChannel::Transcript,
    );
    let mut ids = NativeIds::new(instance.as_id().as_str());
    let mut edges = Vec::new();
    let mut offset = 0u64;
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let cursor = FileCursor {
            file_identity: Id::new("obj").expect("obj"),
            file_generation: U64(1),
            offset: U64(offset),
            length: U64(line.len() as u64),
            digest: remuda_journal::digest_of(line.as_bytes()),
        };
        offset += line.len() as u64 + 1;
        for envelope in map_claude_line(&map, &mut ids, line.as_bytes(), &cursor).expect("map") {
            if let ObservationPayload::Effort(payload) = &envelope.body {
                edges.push((
                    (
                        payload.effective.name.wire().to_string(),
                        payload.effective.source,
                        payload.effective.ultracode,
                    ),
                    envelope
                        .event_id
                        .as_ref()
                        .expect("derived event id")
                        .as_id()
                        .to_string(),
                ));
            }
        }
    }
    edges
}

fn live_edges_with_ids(body: &str, version: &str) -> (InstanceId, Vec<(EdgeTriple, String)>) {
    let instance = InstanceId::new();
    let mut mapper = TranscriptMapper::new(
        DriverKind::ClaudePty,
        instance.clone(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "effort-session".into(),
        version.into(),
    );
    let mut edges = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        for obs in mapper.map_line(line).expect("map") {
            if let ObservationPayload::Effort(payload) = &obs.body {
                edges.push((
                    (
                        payload.effective.name.wire().to_string(),
                        payload.effective.source,
                        payload.effective.ultracode,
                    ),
                    obs.event_id.as_id().to_string(),
                ));
            }
        }
    }
    (instance, edges)
}

#[test]
fn real_21289_fixtures_are_live_journal_parity_with_deterministic_event_ids() {
    for name in [
        "effort-walk-21289.jsonl",
        "effort-launch-a-21289.jsonl",
        "effort-launch-b-21289.jsonl",
        "effort-launch-c1-21289.jsonl",
        "effort-launch-c2-21289.jsonl",
        "effort-launch-d-21289.jsonl",
        "effort-launch-e-21289.jsonl",
        "effort-launch-f2-21289.jsonl",
        "effort-launch-g-21289.jsonl",
    ] {
        let body = fixture_21289(name);
        let (instance, live) = live_edges_with_ids(&body, "2.1.289");
        let journal = journal_edges(&body, &instance);
        assert_eq!(live, journal, "{name}: live and journal edges must match");
    }
}

// ───────── Real claude 2.1.289 recordings (fixture shape guard) ─────────────

#[test]
fn real_21289_recordings_are_jsonl_with_an_effort_slash_and_a_verdict() {
    // effort-sync-4: every recording is valid JSONL with at least one
    // `/effort` slash record and one `<local-command-stdout>` verdict, and its
    // remuda-journal mirror is byte-identical. Behaviour assertions on these
    // recordings belong to the effort tasks that consume them.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = root.join("tests/fixtures/effort-21289");
    let mirror = root.join("../remuda-journal/tests/fixtures/effort-21289");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("effort-21289 fixture dir")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        9,
        "walk + launch a b c1 c2 d e f2 g: {names:?}"
    );
    for name in &names {
        let body = std::fs::read_to_string(dir.join(name)).expect("read fixture");
        let (mut slash, mut verdict) = (0, 0);
        for (n, line) in body.lines().enumerate() {
            let record: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("{name}:{} is not JSON: {err}", n + 1));
            if let Some(serde_json::Value::String(text)) = record.pointer("/message/content") {
                slash += usize::from(text.contains("<command-name>/effort</command-name>"));
                verdict += usize::from(text.starts_with("<local-command-stdout>"));
            }
        }
        assert!(slash >= 1, "{name}: no /effort slash record");
        assert!(verdict >= 1, "{name}: no verdict record");
        let mirrored = std::fs::read_to_string(mirror.join(name)).expect("journal mirror");
        assert!(mirrored == body, "{name}: journal mirror differs");
    }
}
