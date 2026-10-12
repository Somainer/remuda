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

/// c-ctxusage r12 item 2: two print result frames for the SAME scope
/// (num_turns/result_index identical) where the later frame reports only
/// `output_tokens` (the real mapper treats a missing input bucket as
/// unknown, not zero). The scoped UPDATE must merge over the retained
/// counters: input stays 100, output advances 10 -> 20, and the derived total
/// moves 110 -> 120. Before the fix the second UPDATE wrote every incoming
/// bucket verbatim and erased input (and the total) to NULL.
///
/// c-ctxusage r13 item 4: that token-only partial frame also carries NO cost,
/// so the first frame's reported cost survives the merge (NULL never erases a
/// known cost); a following COST-ONLY result frame (total_cost_usd, no token
/// buckets) updates the bill on its own authority, with no token growth.
#[test]
fn a_partial_result_frame_merges_over_known_counters_end_to_end() {
    fn usage_for(usage: Option<serde_json::Value>, total_cost_usd: Option<f64>) -> Observation {
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        let mut frame = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "num_turns": 1,
            "result_index": 0,
            "stop_reason": "end_turn",
        });
        if let Some(usage) = usage {
            frame["usage"] = usage;
        }
        if let Some(cost) = total_cost_usd {
            frame["total_cost_usd"] = serde_json::json!(cost);
        }
        mapper
            .map(frame)
            .expect("result frame maps")
            .into_iter()
            .find(|o| matches!(o.body, ObservationPayload::Usage(_)))
            .expect("a result frame yields one usage observation")
    }

    // First frame: full counters AND a reported cost.
    let first = usage_for(
        Some(serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 10,
        })),
        Some(0.5),
    );
    // The real mapper accepts a result with no input_tokens bucket: it simply
    // does not report that bucket — and a frame with no cost leaves the cost
    // unreported rather than reporting zero.
    let second = usage_for(Some(serde_json::json!({"output_tokens": 20})), None);

    if let ObservationPayload::Usage(payload) = &second.body {
        assert_eq!(payload.scope_id, "1:0");
        assert!(
            matches!(
                payload.input_tokens,
                remuda_protocol::Knowledge::Unknown { .. }
            ),
            "missing input projects as unknown, not 0"
        );
        assert!(
            matches!(payload.cost, remuda_protocol::Knowledge::Unknown { .. }),
            "a missing cost projects as unknown, not a reported zero"
        );
    } else {
        panic!("second frame must map to usage");
    }

    // Cost-only frame: no usage buckets at all, just a corrected bill.
    let third = usage_for(None, Some(0.75));

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();

    for (seq, observation) in [first, second].into_iter().enumerate() {
        let record = JournalRecord {
            instance_id: "ins_partial_merge".into(),
            seq: seq as i64 + 1,
            event_id: format!("evt_partial_{seq}"),
            observed_at: "2020-01-01T00:00:00.000Z".into(),
            event: serde_json::to_value(&observation).unwrap(),
        };
        let row = project_usage_event(&record, None, None).expect("projects");
        assert_eq!(row.scope_id.as_deref(), Some("1:0"));
        assert!(insert_usage_event(&conn, &row).unwrap());
    }

    let (input, output, total, cost): (Option<i64>, Option<i64>, Option<i64>, Option<String>) =
        conn.query_row(
            "SELECT input_tokens, output_tokens, total_tokens, cost_usd FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| {
                Ok((
                    r.get(0).unwrap(),
                    r.get(1).unwrap(),
                    r.get(2).unwrap(),
                    r.get(3).unwrap(),
                ))
            },
        )
        .expect("the scoped row exists");
    assert_eq!(input, Some(100), "the unreported input must be retained");
    assert_eq!(output, Some(20), "the reported output must advance");
    assert_eq!(
        total,
        Some(120),
        "the total is derived from the merged basket"
    );
    assert_eq!(
        cost.as_deref(),
        Some("0.5"),
        "the token-only partial frame must not erase the known cost"
    );

    // The cost-only frame carries no counters, so it never reaches the row by
    // token growth; it must be accepted on its own authority, and every
    // retained counter (and the derived total) survives.
    let cost_only = JournalRecord {
        instance_id: "ins_partial_merge".into(),
        seq: 3,
        event_id: "evt_partial_cost_only".into(),
        observed_at: "2020-01-01T00:00:00.000Z".into(),
        event: serde_json::to_value(&third).unwrap(),
    };
    let cost_only_row = project_usage_event(&cost_only, None, None).expect("projects");
    assert!(
        cost_only_row.input_tokens.is_none() && cost_only_row.output_tokens.is_none(),
        "the cost-only frame reports no token buckets"
    );
    assert!(
        insert_usage_event(&conn, &cost_only_row).expect("cost-only insert"),
        "an authoritative cost-only update must be accepted without token growth"
    );
    let (input, output, total, cost): (Option<i64>, Option<i64>, Option<i64>, Option<String>) =
        conn.query_row(
            "SELECT input_tokens, output_tokens, total_tokens, cost_usd FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| {
                Ok((
                    r.get(0).unwrap(),
                    r.get(1).unwrap(),
                    r.get(2).unwrap(),
                    r.get(3).unwrap(),
                ))
            },
        )
        .unwrap();
    assert_eq!(
        cost.as_deref(),
        Some("0.75"),
        "the cost-only frame updates the bill"
    );
    assert_eq!(
        input,
        Some(100),
        "cost-only update keeps the retained input"
    );
    assert_eq!(
        output,
        Some(20),
        "cost-only update keeps the retained output"
    );
    assert_eq!(total, Some(120), "cost-only update keeps the derived total");
}
