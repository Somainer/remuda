//! c-ctxusage r9 item 4: Grok usage observations carry no native time of
//! their own, so the Hub's native-only rate/freshness queries dropped every
//! Grok row — no TPM and no lastTurnAt during live activity. The real Grok
//! adapter now stamps its Session usage snapshot with the native turn-end
//! timestamp from `events.jsonl` (`ts` on `turn_ended`).
//!
//! This drives the REAL adapter through a historical replay and a live turn,
//! stamps the observations, then projects/inserts/rolls them through the REAL
//! Hub pipeline.

use remuda_driver::adapters::{AdapterHome, FileSignalAdapter, GrokAdapter, StampCtx, stamp};
use remuda_hub::usage_store_test_support::{
    JournalRecord, RollupRequest, insert_usage_event, migrate, project_usage_event, rollup_instance,
};
use remuda_protocol::{HostId, Id, InstanceId, Knowledge, ObservationPayload, RunId};
use rusqlite::Connection;
use std::fs;
use std::path::Path;

const SESSION_ID: &str = "01a09c24-46ef-7a03-9c89-88f1bc00bd0c";
const INSTANCE_ID: &str = "ins_grok_native_ts";

fn now_rfc3339() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

fn session_dir<'a>(home: &'a Path, cwd: &'a Path) -> std::path::PathBuf {
    home.join("sessions")
        .join(remuda_driver::grok_session::encode_session_cwd(cwd))
        .join(SESSION_ID)
}

fn turn_ended(ts: &str) -> String {
    format!("{{\"ts\":\"{ts}\",\"type\":\"turn_ended\",\"outcome\":\"completed\"}}\n")
}

fn usage_json(input: u64, output: u64) -> String {
    // Real grok usage.json reports the cache buckets as structural zeros;
    // they are reported zeros, not omitted Unknown buckets (c-ctxusage r15 item 5).
    format!(
        "{{\"modelId\":\"grok-1\",\"inputTokens\":{input},\"outputTokens\":{output},\"cacheReadInputTokens\":0,\"cacheCreationInputTokens\":0}}\n"
    )
}

/// Poll the adapter and return the single stamped Session usage observation
/// in the batch.
fn poll_usage(adapter: &mut GrokAdapter, seq: &mut u64) -> remuda_protocol::Observation {
    let ctx = StampCtx {
        instance_id: InstanceId::new(),
        host_id: HostId::new(),
        journal_id: Id::new("obj").expect("journal id"),
        run_id: RunId::new(),
        session_id: SESSION_ID.into(),
    };
    let observed = adapter.poll().expect("grok poll");
    let mut stamped = Vec::new();
    for observation in observed {
        *seq += 1;
        if let Some(observation) = stamp(&ctx, *seq, &observation)
            && matches!(observation.body, ObservationPayload::Usage(_))
        {
            stamped.push(observation);
        }
    }
    assert_eq!(stamped.len(), 1, "exactly one session usage per turn end");
    stamped.pop().expect("usage observation")
}

fn to_journal_record(seq: i64, observation: &remuda_protocol::Observation) -> JournalRecord {
    JournalRecord {
        instance_id: INSTANCE_ID.into(),
        seq,
        event_id: format!("evt_grok_{seq}"),
        observed_at: String::from(observation.observed_at.clone()),
        event: serde_json::to_value(observation).expect("serialize observation"),
    }
}

#[test]
fn grok_turn_end_native_ts_stamps_usage_and_projects_to_live_rates() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let dir = session_dir(&home, &cwd);
    fs::create_dir_all(&dir).unwrap();

    // Batch 1: a historical replay after restart — an old turn_ended with the
    // cumulative usage.json as it stood then.
    let old_ts = "2026-09-01T12:00:00.000Z";
    fs::write(dir.join("updates.jsonl"), "").unwrap();
    fs::write(dir.join("events.jsonl"), turn_ended(old_ts)).unwrap();
    fs::write(dir.join("usage.json"), usage_json(100, 10)).unwrap();

    let mut adapter = GrokAdapter::new(AdapterHome {
        home: home.clone(),
        cwd: cwd.clone(),
        pid: None,
        launched_at: None,
        resume_session_id: None,
    });
    adapter.bind_session_dir(SESSION_ID, dir.clone());

    let mut seq = 0u64;
    let historical = poll_usage(&mut adapter, &mut seq);
    let Knowledge::Known {
        value: historical_at,
    } = &historical.native_at
    else {
        panic!(
            "historical usage must carry the native turn-end ts, got {:?}",
            historical.native_at
        );
    };
    assert_eq!(
        String::from(historical_at.clone()),
        old_ts,
        "the session snapshot is stamped with turn_ended.ts"
    );

    // Batch 2: a live turn ending NOW. Each new usage.json holds that turn's
    // counters; the adapter folds them into the session total (110/11).
    let live_ts = now_rfc3339();
    let mut events = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("events.jsonl"))
        .unwrap();
    use std::io::Write;
    events.write_all(turn_ended(&live_ts).as_bytes()).unwrap();
    drop(events);
    fs::write(dir.join("usage.json"), usage_json(10, 1)).unwrap();

    let live = poll_usage(&mut adapter, &mut seq);
    let Knowledge::Known { value: live_at } = &live.native_at else {
        panic!(
            "live usage must carry the native turn-end ts, got {:?}",
            live.native_at
        );
    };
    assert_eq!(String::from(live_at.clone()), live_ts);

    // Project + persist BOTH through the real Hub pipeline.
    let conn = Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    for (record_seq, observation) in [(1, &historical), (2, &live)] {
        let record = to_journal_record(record_seq, observation);
        let row = project_usage_event(&record, None, None).expect("projects usage");
        assert_eq!(row.observed_at_source, "native");
        assert_eq!(row.scope, "session");
        assert_eq!(row.scope_id.as_deref(), Some(SESSION_ID));
        assert!(insert_usage_event(&conn, &row).expect("insert usage row"));
    }

    let rollup = rollup_instance(
        &conn,
        &RollupRequest {
            instance_id: INSTANCE_ID,
            kind: "grok",
            spec_model: None,
            effective_model: None,
            profile_id: None,
        },
    )
    .expect("rollup")
    .expect("rollup exists");
    // Current cumulative stock.
    assert_eq!(rollup.session_input_tokens, Some(110));
    assert_eq!(rollup.session_output_tokens, Some(11));
    // Only the live 110-100 interval is throughput right now: the historical
    // point with no predecessor authors no increment (unknown interval), and
    // the window is native-time gated.
    assert_eq!(
        rollup.tpm_in_60s,
        Some(10),
        "live native Grok turn drives TPM; historical replay does not"
    );
    assert_eq!(rollup.tpm_out_60s, Some(1));
    assert_eq!(
        rollup.last_turn_at.as_deref(),
        Some(live_ts.as_str()),
        "lastTurnAt follows the native live turn end"
    );
}
