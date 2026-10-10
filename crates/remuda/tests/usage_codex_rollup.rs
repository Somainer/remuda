//! c-usagefu (b): replay the captured real Codex 0.154.0 session through the
//! driver adapter, project every emitted Usage observation through the Hub's
//! journal projector into usage_events, and roll it up. A naive replay (turn
//! sums double-counting retries, context read from a cumulative snapshot)
//! inflated this fixture; the rollup must report 6 turns and 900 uncached
//! input tokens, with the context basket taken from the NEWEST single
//! per-response (Message) row rather than the cumulative turn total.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use remuda_driver::adapters::{
    AdapterHome, AdapterObservation, CodexAdapter, FileSignalAdapter, StampCtx, stamp,
};
use remuda_hub::usage_store_test_support::{
    JournalRecord, RollupRequest, insert_usage_event, migrated_memory, project_usage_event,
    rollup_instance,
};
use remuda_protocol::{HostId, Id, InstanceId, ObservationPayload, RunId};

const CODEX_ROLLOUT: &str =
    include_str!("../../remuda-driver/tests/fixtures/codex/interactive-0.154.0.jsonl");

const INSTANCE: &str = "ins_codex_154";

fn run_codex_adapter(dir: &Path) -> Vec<AdapterObservation> {
    // Lay the real rollout into a shadow CODEX_HOME exactly as a launched
    // session would leave it (same layout as p6_adapter_parity).
    let home = dir.join("codex-home");
    let session_dir = home.join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("rollout-2026-09-14T02-21-40-01a09c00-64d-7ce1-8537-984e896b8e8a.jsonl"),
        CODEX_ROLLOUT,
    )
    .unwrap();
    std::fs::write(
        home.join("session_index.jsonl"),
        "{\"id\":\"01a09c00-64d4-7ce1-8537-984e896b8e8a\",\"thread_name\":\"p6\",\"updated_at\":\"2026-09-14T02:21:06Z\"}\n",
    )
    .unwrap();
    let mut adapter = CodexAdapter::new(AdapterHome {
        home,
        cwd: dir.to_path_buf(),
        pid: None,
        launched_at: None,
    });
    // Files are fully present: discovery on the first poll, the complete read on
    // the next; keep polling until a tick yields nothing.
    let mut all = Vec::new();
    for _ in 0..4 {
        let observed = adapter.poll().expect("codex poll");
        if observed.is_empty() && !all.is_empty() {
            break;
        }
        all.extend(observed);
    }
    all
}

#[test]
fn codex_0_154_replay_rolls_up_six_turns_and_900_uncached_input() {
    let dir = tempfile::tempdir().unwrap();
    let observed = run_codex_adapter(dir.path());

    let usage: Vec<&AdapterObservation> = observed
        .iter()
        .filter(|o| matches!(o.payload, ObservationPayload::Usage(_)))
        .collect();
    assert!(
        !usage.is_empty(),
        "the codex adapter emits usage observations"
    );

    let conn = migrated_memory().expect("migrated in-memory hub db");
    let seq = AtomicU64::new(0);
    let stamp_ctx = StampCtx {
        instance_id: InstanceId::new(),
        host_id: HostId::new(),
        journal_id: Id::new("obj").unwrap(),
        run_id: RunId::new(),
        session_id: "codex-154".into(),
    };
    let mut scopes = std::collections::BTreeMap::new();
    for adapter_obs in usage {
        let number = seq.fetch_add(1, Ordering::SeqCst) + 1;
        let stamped = stamp(&stamp_ctx, number, adapter_obs).expect("stamp usage observation");
        if let ObservationPayload::Usage(payload) = &stamped.body {
            *scopes.entry(format!("{:?}", payload.scope)).or_insert(0u32) += 1;
        }
        let event = serde_json::to_value(&stamped).expect("serialize observation");
        let record = JournalRecord {
            instance_id: INSTANCE.into(),
            seq: number as i64,
            event_id: stamped.event_id.as_id().as_str().to_owned(),
            event,
            observed_at: "2026-09-14T02:21:40.000Z".into(),
        };
        let row = project_usage_event(&record, None, None).expect("usage row projects");
        insert_usage_event(&conn, &row).expect("usage row persists");
    }

    let rollup = rollup_instance(
        &conn,
        &RollupRequest {
            instance_id: INSTANCE,
            kind: "codex",
            spec_model: None,
            effective_model: None,
            profile_id: None,
        },
    )
    .expect("rollup query")
    .expect("the session has usage");

    // Per-response Message rows (10 responses) drive the context basket; the 6
    // turn snapshots drive the additive totals; session rows are the stock.
    assert_eq!(scopes.get("Message"), Some(&10));
    assert_eq!(scopes.get("Turn"), Some(&6));

    assert_eq!(rollup.turns, 6, "one turn row per of the 6 completed turns");
    assert_eq!(
        rollup.session_input_tokens,
        Some(900),
        "uncached input: additive turn sums, never the cumulative snapshots"
    );
    // The context ring reads the NEWEST single per-response (Message) basket,
    // not the cumulative turn total. The fixture's final per-response call
    // reports input 100 incl. 10 cached: uncached 90 + cache read 10 + cache
    // write 0 = 100 tokens of context.
    assert_eq!(
        rollup.context_used_tokens,
        Some(100),
        "context basket is the latest per-response (Message) call"
    );
}
