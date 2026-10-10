//! c-ctxusage r7 item 8: a LIVE claude print/sdk `result` frame maps to a usage
//! observation whose receipt-time `nativeAt` makes it project as a `native`
//! row, and that row counts in the Hub's 60 s TPM window. This is the
//! end-to-end link that item 2's synthetic-record hub unit test does not
//! cover: real Mapper -> Observation -> JournalRecord -> project/insert ->
//! rollup TPM.

use remuda_driver::StdoutMapper;
use remuda_hub::usage_store_test_support::{
    JournalRecord, insert_usage_event, migrate, project_usage_event, rollup_instance,
};
use remuda_protocol::{DriverKind, Observation, ObservationPayload};

fn feed_live_result() -> (Observation, String) {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
    let frame = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "num_turns": 1,
        "result_index": 0,
        "stop_reason": "end_turn",
        "total_cost_usd": 0.004,
        "usage": {
            "input_tokens": 250,
            "output_tokens": 25,
            "cache_read_input_tokens": 1_000,
            "cache_creation_input_tokens": 300,
        }
    });
    let observations = mapper.map(frame).expect("result frame maps");
    let usage = observations
        .into_iter()
        .find(|o| matches!(o.body, ObservationPayload::Usage(_)))
        .expect("a result frame yields one usage observation");
    let native_at = match &usage.native_at {
        remuda_protocol::Knowledge::Known { value } => String::from(value.clone()),
        other => panic!("live result native_at must be Known, got {other:?}"),
    };
    (usage, native_at)
}

#[test]
fn live_print_result_flows_into_the_tpm_window_end_to_end() {
    let (observation, native_at) = feed_live_result();

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();

    let record = JournalRecord {
        instance_id: "ins_live_result".into(),
        seq: observation.seq.0 as i64,
        event_id: "evt_live_1".into(),
        // The Hub uses the JournalRecord's observed_at as ingest fallback; the
        // observation carries its own receipt nativeAt on event.nativeAt.
        observed_at: "2020-01-01T00:00:00.000Z".into(),
        event: serde_json::to_value(&observation).unwrap(),
    };
    let row = project_usage_event(&record, None, None).expect("projects");
    // Live receipt time -> native source, never ingest.
    assert_eq!(row.observed_at_source, "native");
    assert_eq!(row.observed_at, native_at);
    assert!(insert_usage_event(&conn, &row).unwrap());

    let rollup = rollup_instance(&conn, "ins_live_result", "claude", None)
        .unwrap()
        .unwrap();
    // The native receipt time is "now", so it is inside the 60 s window: the
    // live result's input and output both count.
    assert_eq!(
        rollup.tpm_in_60s,
        Some(250),
        "live print result input counts in the current TPM window"
    );
    assert_eq!(rollup.tpm_out_60s, Some(25));
    assert_eq!(rollup.last_turn_at.as_deref(), Some(native_at.as_str()));
}
