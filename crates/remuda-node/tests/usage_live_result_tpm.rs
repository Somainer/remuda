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

/// c-ctxusage r12 item 2 / r15 items 2-3: a PARTIAL later frame (the real
/// mapper treats a missing input bucket as unknown, not zero) merges over the
/// retained counters only when its source position is STRICTLY NEWER: input
/// stays 100, output advances 10 -> 20, the derived total moves 110 -> 120,
/// and a token-only frame with no cost keeps the known bill.
///
/// Under the r15 turn-authority ruling an EQUAL-position delivery changes
/// NOTHING: the print result frames below all carry revision 1
/// (result_index 0), so the re-delivered partial frame and the re-delivered
/// cost-only frame are both frozen until re-stamped (as a transcript mapper
/// would) at a strictly newer record position. A newer COST-ONLY frame
/// corrects the bill and advances the watermark while keeping the counters
/// and the turn clock; a counter frame at an older position is then rejected.
#[test]
fn a_partial_result_frame_merges_over_known_counters_end_to_end() {
    use remuda_protocol::U64;

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

    // Promote a real mapper observation to a strictly newer transcript source
    // position, exactly as the transcript mapper stamps later records.
    let at_revision = |mut observation: Observation, revision: u64| -> Observation {
        if let ObservationPayload::Usage(payload) = &mut observation.body {
            payload.metric_revision = U64(revision);
        }
        observation
    };
    // Stamp a deterministic receipt time so the turn-clock assertions do not
    // depend on the frames being mapped in different wall-clock milliseconds.
    let with_native_at = |mut observation: Observation, at: &str| -> Observation {
        observation.native_at = remuda_protocol::Knowledge::Known {
            value: remuda_protocol::Timestamp::try_from(at.to_string()).unwrap(),
        };
        observation
    };

    // First frame: full counters AND a reported cost (revision 1).
    let first = with_native_at(
        usage_for(
            Some(serde_json::json!({
                "input_tokens": 100,
                "output_tokens": 10,
            })),
            Some(0.5),
        ),
        "2026-10-10T00:00:00.000Z",
    );
    // The real mapper accepts a result with no input_tokens bucket: it simply
    // does not report that bucket — and a frame with no cost leaves the cost
    // unreported rather than reporting zero. Print frames pin the revision at
    // result_index 0 -> 1.
    let second = with_native_at(
        usage_for(Some(serde_json::json!({"output_tokens": 20})), None),
        "2026-10-10T00:00:02.000Z",
    );

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

    // Cost-only frame: no usage buckets at all, just a corrected bill, at a
    // receipt time LATER than the partial counter frame.
    let third = with_native_at(usage_for(None, Some(0.75)), "2026-10-10T00:00:03.000Z");
    const FIRST_CLOCK: &str = "2026-10-10T00:00:00.000Z";
    const SECOND_CLOCK: &str = "2026-10-10T00:00:02.000Z";
    const THIRD_CLOCK: &str = "2026-10-10T00:00:03.000Z";

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();

    let mut seq = 0i64;
    let mut next_seq = || {
        seq += 1;
        seq
    };
    let insert_observation = |seq: i64, observation: &Observation| -> bool {
        let record = JournalRecord {
            instance_id: "ins_partial_merge".into(),
            seq,
            event_id: format!("evt_partial_{seq}"),
            observed_at: "2020-01-01T00:00:00.000Z".into(),
            event: serde_json::to_value(observation).unwrap(),
        };
        let row = project_usage_event(&record, None, None).expect("projects");
        assert_eq!(row.scope_id.as_deref(), Some("1:0"));
        insert_usage_event(&conn, &row).expect("insert")
    };

    // Baseline at revision 1: input 100 / output 10, bill 0.5.
    assert!(insert_observation(next_seq(), &first));
    let clock: String = conn
        .query_row(
            "SELECT observed_at FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        clock, FIRST_CLOCK,
        "the baseline turn clock is the first frame"
    );

    // The re-delivered partial frame at the SAME position fills nothing:
    // output stays 10 even though 20 is a growth (c-ctxusage r15 item 2).
    assert!(
        !insert_observation(next_seq(), &second),
        "an equal-position partial frame must change nothing"
    );
    let output: Option<i64> = conn
        .query_row(
            "SELECT output_tokens FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(output, Some(10), "the equal-position 10→20 fill is frozen");

    // The same real-mapper partial frame at a STRICTLY NEWER position fills:
    // input retained, output advances, total derived, bill kept.
    assert!(
        insert_observation(next_seq(), &at_revision(second.clone(), 2)),
        "a strictly newer partial frame is applied"
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
        .expect("the scoped row exists");
    assert_eq!(input, Some(100), "the unreported input must be retained");
    assert_eq!(output, Some(20), "the reported output advances");
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

    // The cost-only frame at the OLD revision 1 is position-gated: the bill
    // stays 0.5 (c-ctxusage r15 item 3).
    assert!(
        !insert_observation(next_seq(), &third),
        "a stale equal-position cost-only frame must change nothing"
    );
    let cost: Option<String> = conn
        .query_row(
            "SELECT cost_usd FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cost.as_deref(),
        Some("0.5"),
        "the stale bill correction is frozen"
    );

    // At a strictly newer position the cost-only frame is accepted on its own
    // authority: bill corrected, watermark advanced, counters and clock kept.
    assert!(
        insert_observation(next_seq(), &at_revision(third.clone(), 3)),
        "a newer cost-only frame corrects the bill"
    );
    let (input, output, total, cost, clock): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        String,
    ) = conn
        .query_row(
            "SELECT input_tokens, output_tokens, total_tokens, cost_usd, observed_at FROM \
             usage_events WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND \
             scope_id = '1:0'",
            [],
            |r| {
                Ok((
                    r.get(0).unwrap(),
                    r.get(1).unwrap(),
                    r.get(2).unwrap(),
                    r.get(3).unwrap(),
                    r.get(4).unwrap(),
                ))
            },
        )
        .unwrap();
    assert_eq!(
        cost.as_deref(),
        Some("0.75"),
        "the newer cost-only frame updates the bill"
    );
    assert_eq!(input, Some(100), "cost-only keeps the retained input");
    assert_eq!(output, Some(20), "cost-only keeps the retained output");
    assert_eq!(total, Some(120), "cost-only keeps the derived total");
    assert_eq!(
        clock, SECOND_CLOCK,
        "a cost-only frame at {THIRD_CLOCK} never rewrites the turn clock"
    );

    // Replay the FIRST frame after the cost-only update: an equal/older
    // position changes NOTHING — no counter, no cost, no clock.
    let replayed_first = JournalRecord {
        instance_id: "ins_partial_merge".into(),
        seq: next_seq(),
        event_id: "evt_partial_replay_first".into(),
        observed_at: "2020-01-01T00:00:00.000Z".into(),
        event: serde_json::to_value(&first).unwrap(),
    };
    let replay_row = project_usage_event(&replayed_first, None, None).expect("projects replay");
    assert_eq!(
        replay_row.metric_revision, 1,
        "the print result replays at revision 1"
    );
    assert!(
        !insert_usage_event(&conn, &replay_row).expect("replay insert"),
        "replaying the first frame at an older position must change nothing"
    );

    // The cost-only watermark (3) also rejects the otherwise-growing rev-2
    // counter snapshot arriving afterwards.
    assert!(
        !insert_observation(next_seq(), &at_revision(second.clone(), 2)),
        "an older-position counter snapshot is rejected against the cost-only watermark"
    );
    let (input, output, total, cost, clock): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        String,
    ) = conn
        .query_row(
            "SELECT input_tokens, output_tokens, total_tokens, cost_usd, observed_at FROM \
             usage_events WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND \
             scope_id = '1:0'",
            [],
            |r| {
                Ok((
                    r.get(0).unwrap(),
                    r.get(1).unwrap(),
                    r.get(2).unwrap(),
                    r.get(3).unwrap(),
                    r.get(4).unwrap(),
                ))
            },
        )
        .unwrap();
    assert_eq!(input, Some(100), "input retained");
    assert_eq!(output, Some(20), "the older-position growth is frozen");
    assert_eq!(total, Some(120), "derived total retained");
    assert_eq!(cost.as_deref(), Some("0.75"), "nothing rolls the bill back");
    assert_eq!(clock, SECOND_CLOCK, "nothing moves the clock");
}
