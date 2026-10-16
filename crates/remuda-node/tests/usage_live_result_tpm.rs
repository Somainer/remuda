//! c-ctxusage r7 item 8: a LIVE claude print/sdk `result` frame maps to a usage
//! observation whose receipt-time `nativeAt` makes it project as a `native`
//! row, and that row counts in the Hub's 60 s TPM window. This is the
//! end-to-end link that item 2's synthetic-record hub unit test does not
//! cover: real Mapper -> Observation -> JournalRecord -> project/insert ->
//! rollup TPM.

use remuda_driver::StdoutMapper;
use remuda_hub::usage_store_test_support::{
    JournalRecord, RollupRequest, aggregate_instance, aggregate_supply, insert_usage_event,
    migrate, project_usage_event, rollup_instance,
};
use remuda_protocol::{
    DriverKind, InputAccounting, Knowledge, Observation, ObservationPayload, U64,
};

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
        .find(|o| {
            matches!(
                &o.body,
                ObservationPayload::Usage(u) if u.scope == remuda_protocol::UsageScope::Turn
            )
        })
        .expect("a result frame yields a Turn usage observation");
    let native_at = match &usage.native_at {
        remuda_protocol::Knowledge::Known { value } => String::from(value.clone()),
        other => panic!("live result native_at must be Known, got {other:?}"),
    };
    (usage, native_at)
}

#[test]
fn live_print_result_flows_into_the_tpm_window_end_to_end() {
    let (observation, native_at) = feed_live_result();

    // r37 item 8: the fallback result reports input 250 / output 25 / cache
    // read 1_000 / cache creation 300. input_tokens is UNCACHED input, so the
    // declared total is the all-bucket sum 1_575 — never input+output 275 —
    // and the input accounting is Uncached. That declared total is what the
    // provider aggregate sums initially.
    if let ObservationPayload::Usage(payload) = &observation.body {
        assert_eq!(
            payload.total_tokens,
            Knowledge::Known { value: U64(1_575) },
            "250 + 25 + 1000 + 300 across every reported bucket"
        );
        assert_eq!(
            payload.input_accounting,
            InputAccounting::Uncached,
            "result input_tokens excludes cached context"
        );
        assert_eq!(payload.input_tokens, Knowledge::Known { value: U64(250) });
        assert_eq!(
            payload.cache_read_tokens,
            Knowledge::Known { value: U64(1_000) }
        );
        assert_eq!(
            payload.cache_write_tokens,
            Knowledge::Known { value: U64(300) }
        );
    } else {
        panic!("feed_live_result must yield a usage observation");
    }

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
    // Project with profile/model so the PROVIDER aggregate can select the row.
    let row = project_usage_event(&record, Some("pvp_live"), Some("m")).expect("projects");
    // Live receipt time -> native source, never ingest.
    assert_eq!(row.observed_at_source, "native");
    assert_eq!(row.observed_at, native_at);
    assert_eq!(
        row.total_tokens,
        Some(1_575),
        "the initial stored row declares the all-bucket total, not 275"
    );
    assert!(insert_usage_event(&conn, &row).unwrap());

    let rollup = rollup_instance(
        &conn,
        &RollupRequest {
            instance_id: "ins_live_result",
            kind: "claude",
            spec_model: None,
            effective_model: None,
            profile_id: None,
        },
    )
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

    // The provider aggregate sums the declared per-row total: 1_575, once.
    let supply = aggregate_supply(&conn, "pvp_live", "m").unwrap();
    assert!(!supply.unavailable);
    assert_eq!(
        supply.total_tokens,
        Some(1_575),
        "provider aggregation sums the correct declared total"
    );
}

/// r37 item 8: a result frame that omits a required bucket must declare an
/// UNKNOWN total rather than a partial input+output sum (U1).
#[test]
fn a_result_without_cache_buckets_declares_an_unknown_total() {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
    let observation = mapper
        .map(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "num_turns": 1,
            "result_index": 0,
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 10}
        }))
        .expect("result frame maps");
    let turn = observation
        .iter()
        .find(|o| {
            matches!(&o.body, ObservationPayload::Usage(u)
                if u.scope == remuda_protocol::UsageScope::Turn)
        })
        .expect("turn usage observation");
    if let ObservationPayload::Usage(payload) = &turn.body {
        assert!(
            matches!(payload.total_tokens, Knowledge::Unknown { .. }),
            "100+10 without the cache baskets is not a declared total"
        );
        assert_eq!(payload.input_accounting, InputAccounting::Uncached);
    } else {
        panic!("turn observation must be usage");
    }
}

/// r37 item 10: a native `total_cost_usd` on the result is the provider's own
/// bill, so the token-less Session cost carrier that copies it verbatim is
/// `accounting: reported` — not estimated. A carrier emitted with no cost
/// (modelUsage window only) has no native bill to label.
#[test]
fn a_result_with_native_cost_labels_the_session_carrier_reported() {
    use remuda_protocol::{Accounting, UsageScope};
    let session_carrier = |frame: serde_json::Value| {
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        mapper
            .map(frame)
            .expect("result maps")
            .into_iter()
            .find_map(|o| match o.body {
                ObservationPayload::Usage(u) if u.scope == UsageScope::Session => Some(u),
                _ => None,
            })
            .expect("a result yields a Session carrier")
    };

    // Native cost present -> reported, copied verbatim.
    let billed = session_carrier(serde_json::json!({
        "type": "result",
        "subtype": "success",
        "num_turns": 1,
        "result_index": 0,
        "stop_reason": "end_turn",
        "total_cost_usd": 0.004,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }));
    assert_eq!(billed.accounting, Accounting::Reported);
    assert_eq!(
        billed.cost,
        Knowledge::Known {
            value: remuda_protocol::Cost {
                amount: "0.004".into(),
                currency: "USD".into()
            }
        },
        "the native bill is copied unchanged"
    );

    // No cost, only a model window -> no native bill to call reported.
    let window_only = session_carrier(serde_json::json!({
        "type": "result",
        "subtype": "success",
        "num_turns": 1,
        "result_index": 0,
        "stop_reason": "end_turn",
        "modelUsage": {"claude-opus-4-7": {"contextWindow": 200000}}
    }));
    assert_eq!(
        window_only.accounting,
        Accounting::Estimated,
        "a cost-less window carrier carries no reported bill"
    );
    assert!(matches!(window_only.cost, Knowledge::Unknown { .. }));
}

/// r38 item 3: on a result-ONLY stream the native `total_cost_usd` is the
/// CUMULATIVE bill for the session. It must ride only the session-level
/// carrier (folded to its latest value once), never be copied onto each
/// additive Turn — two results with $0.01 then $0.03 bill $0.03, not $0.04.
#[test]
fn two_result_only_frames_bill_the_latest_cumulative_session_cost_once() {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
    let mut all = Vec::new();
    for (num_turns, result_index, cost) in [(1u64, 0u64, 0.01f64), (2u64, 1u64, 0.03f64)] {
        let obs = mapper
            .map(serde_json::json!({
                "type": "result",
                "subtype": "success",
                "num_turns": num_turns,
                "result_index": result_index,
                "stop_reason": "end_turn",
                "total_cost_usd": cost,
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 10,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }
            }))
            .expect("result frame maps");
        all.extend(obs);
    }

    // The fallback Turn rows carry NO bill: cumulative cost is session-level.
    for observation in &all {
        if let ObservationPayload::Usage(u) = &observation.body
            && u.scope == remuda_protocol::UsageScope::Turn
        {
            assert!(
                matches!(u.cost, Knowledge::Unknown { .. }),
                "a result-only Turn never copies the cumulative session bill"
            );
        }
    }

    // Project and insert EVERY observation, as the Node journal path would.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    for (seq, observation) in all.iter().enumerate() {
        let record = JournalRecord {
            instance_id: "ins_cumcost".into(),
            seq: seq as i64 + 1,
            event_id: format!("evt_cum_{seq}"),
            observed_at: "2026-10-13T00:00:00.000Z".into(),
            event: serde_json::to_value(observation).unwrap(),
        };
        let Some(row) = project_usage_event(&record, None, None) else {
            continue;
        };
        insert_usage_event(&conn, &row).unwrap();
    }

    let agg = aggregate_instance(&conn, "ins_cumcost").unwrap();
    assert!(
        ((agg.cost_usd.unwrap_or(-1.0)) - 0.03).abs() < 1e-9,
        "the bill is the latest cumulative reported cost once, got {:?}",
        agg.cost_usd
    );
}

/// r39 item 4 (AA3): cost presence is independent of token presence. A
/// token-bearing result under model A ($0.01), followed by a /model switch and
/// a COST-ONLY result under model B ($0.03), must leave B a provider row with
/// the reported bill and Unknown token buckets — the Ruling-B flow override
/// must not hide the cost-only carrier, and A keeps its own tokens and bill.
#[test]
fn a_cost_only_result_after_a_model_switch_still_bills_the_new_model() {
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
    // Result 1: tokens + cumulative $0.01.
    let first = mapper
        .map(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "num_turns": 1,
            "result_index": 0,
            "stop_reason": "end_turn",
            "total_cost_usd": 0.01,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 10,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }))
        .expect("result 1 maps");
    // Result 2 after the configure/model switch: cost-only, cumulative $0.03.
    let second = mapper
        .map(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "num_turns": 2,
            "result_index": 1,
            "stop_reason": "end_turn",
            "total_cost_usd": 0.03
        }))
        .expect("result 2 maps");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    let mut seq = 0i64;
    let mut put = |obs: &Observation, model: &str| {
        seq += 1;
        let record = JournalRecord {
            instance_id: "ins_model_switch".into(),
            seq,
            event_id: format!("evt_switch_{seq}"),
            observed_at: "2026-10-13T00:00:00.000Z".into(),
            event: serde_json::to_value(obs).unwrap(),
        };
        // The Hub projects with the instance's CURRENT (post-switch) model.
        let Some(row) = project_usage_event(&record, Some("pvp_switch"), Some(model)) else {
            return;
        };
        insert_usage_event(&conn, &row).unwrap();
    };
    for obs in &first {
        put(obs, "model-a");
    }
    for obs in &second {
        put(obs, "model-b");
    }

    // Model A owns the token-bearing Turn and its 0.01 snapshot.
    let supply_a = aggregate_supply(&conn, "pvp_switch", "model-a").unwrap();
    assert_eq!(
        supply_a.input_tokens,
        Some(100),
        "A keeps its token contribution"
    );
    assert!(supply_a.events >= 1);
    assert!(
        (supply_a.cost_usd.unwrap_or(-1.0) - 0.01).abs() < 1e-9,
        "A carries its reported 0.01 snapshot, got {:?}",
        supply_a.cost_usd
    );

    // Model B has only the cost-only carrier: a visible row, bill present,
    // every token bucket Unknown.
    let supply_b = aggregate_supply(&conn, "pvp_switch", "model-b").unwrap();
    assert!(
        supply_b.events >= 1,
        "cost-only reported authority emits a provider row"
    );
    assert!(
        (supply_b.cost_usd.unwrap_or(-1.0) - 0.03).abs() < 1e-9,
        "B shows the cumulative 0.03 bill, got {:?}",
        supply_b.cost_usd
    );
    assert_eq!(supply_b.total_tokens, None, "B reported no tokens");
    assert_eq!(
        supply_b.input_tokens, None,
        "B token fields stay Unknown, not zero"
    );
    assert_eq!(supply_b.output_tokens, None);
    assert_eq!(
        format!("{:?}", supply_b.budget_status(Some(0.02))),
        "Stop",
        "the 0.03 B bill Stops a 0.02 model budget"
    );

    // The instance bill is the latest cumulative reported value.
    let agg = aggregate_instance(&conn, "ins_model_switch").unwrap();
    assert!(
        (agg.cost_usd.unwrap_or(-1.0) - 0.03).abs() < 1e-9,
        "instance bills the newest cumulative 0.03, got {:?}",
        agg.cost_usd
    );
}

/// the assistant (rev 1) and message_delta (rev 2) frames, and the following
/// result's Turn is suppressed as a re-sum. Those per-call observations must
/// carry the frame RECEIPT time as `nativeAt` — otherwise the only surviving
/// token rows are native-time Unknown, drop out of the Hub rate windows, and
/// the surviving session carrier is token-less. Mapper -> project -> Hub
/// rollup, end to end.
#[test]
fn live_per_call_assistant_delta_usage_carries_native_time_into_tpm() {
    use remuda_protocol::UsageScope;
    let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");

    // Real order: message_start binds the active message, the assistant
    // record emits revision 1, message_delta the final revision 2.
    mapper
        .map(serde_json::json!({"type":"stream_event","event":{
        "type":"message_start","message":{
            "id":"msg_pc","model":"claude-opus-4-7",
            "usage":{"input_tokens":100,"cache_read_input_tokens":1000,
                     "cache_creation_input_tokens":0,"output_tokens":8}
        }}}))
        .expect("message_start maps");
    let assistant = mapper
        .map(
            serde_json::json!({"type":"assistant","uuid":"u1","message":{
                "id":"msg_pc","role":"assistant",
                "content":[{"type":"text","text":"done"}],
                "model":"claude-opus-4-7",
                "usage":{"input_tokens":100,"cache_read_input_tokens":1000,
                         "cache_creation_input_tokens":0,"output_tokens":8}
            }}),
        )
        .expect("assistant maps");
    let delta = mapper
        .map(serde_json::json!({"type":"stream_event","event":{
            "type":"message_delta","delta":{"stop_reason":"end_turn"},
            "usage":{"input_tokens":100,"cache_read_input_tokens":1000,
                     "cache_creation_input_tokens":0,"output_tokens":30}
        }}))
        .expect("message_delta maps");
    // The terminal result re-sums the same call: its Turn row is suppressed.
    let result = mapper
        .map(serde_json::json!({
            "type":"result","subtype":"success","num_turns":1,
            "result_index":0,"stop_reason":"end_turn","total_cost_usd":0.01,
            "usage":{"input_tokens":100,"output_tokens":30,
                     "cache_read_input_tokens":1000,
                     "cache_creation_input_tokens":0}
        }))
        .expect("result maps");

    fn turn_usage(obs: &[Observation]) -> Vec<&Observation> {
        obs.iter()
            .filter(|o| {
                matches!(&o.body, ObservationPayload::Usage(u)
                    if u.scope == UsageScope::Turn)
            })
            .collect()
    }
    let per_call = turn_usage(&assistant)
        .into_iter()
        .chain(turn_usage(&delta))
        .collect::<Vec<_>>();
    assert_eq!(per_call.len(), 2, "rev 1 assistant + rev 2 delta");
    for observation in &per_call {
        match &observation.native_at {
            remuda_protocol::Knowledge::Known { value } => {
                assert_eq!(
                    String::from(value.clone()),
                    String::from(observation.observed_at.clone()),
                    "live per-call usage is stamped with its receipt time"
                );
            }
            other => panic!("per-call native_at must be Known on live stdout, got {other:?}"),
        }
    }
    assert!(
        turn_usage(&result).is_empty(),
        "the result Turn is suppressed after per-call usage"
    );

    // The final revision 2 observation projects to a NATIVE-timed hub row and
    // its uncached input / output count in the current 60 s rate window.
    let rev2 = per_call.last().expect("rev 2 observation");
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    let record = JournalRecord {
        instance_id: "ins_per_call".into(),
        seq: rev2.seq.0 as i64,
        event_id: "evt_per_call_1".into(),
        // Deliberately ancient ingest time: only nativeAt may place the row.
        observed_at: "2020-01-01T00:00:00.000Z".into(),
        event: serde_json::to_value(*rev2).unwrap(),
    };
    let row = project_usage_event(&record, None, None).expect("projects");
    assert_eq!(row.observed_at_source, "native");
    assert_eq!(
        row.observed_at,
        match &rev2.native_at {
            remuda_protocol::Knowledge::Known { value } => String::from(value.clone()),
            _ => unreachable!(),
        }
    );
    assert!(insert_usage_event(&conn, &row).unwrap());

    let rollup = rollup_instance(
        &conn,
        &RollupRequest {
            instance_id: "ins_per_call",
            kind: "claude",
            spec_model: None,
            effective_model: None,
            profile_id: None,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        rollup.tpm_in_60s,
        Some(100),
        "the per-call uncached input counts in the live TPM window"
    );
    assert_eq!(rollup.tpm_out_60s, Some(30));
}

/// c-ctxusage r12 item 2 / r15 items 2-3, updated r38 item 3: a PARTIAL later
/// frame merges counters over the retained snapshot only when its source
/// position is STRICTLY NEWER. The result's CUMULATIVE bill rides the SESSION
/// carrier (folded to its newest reported value), never the additive Turn, so
/// cost-only result frames carry no Turn at all; a later reported cumulative
/// cost updates the session bill independently of counter revisions.
#[test]
fn a_partial_result_frame_merges_over_known_counters_end_to_end() {
    use remuda_protocol::{U64, UsageScope};

    // Map a result frame and split its Turn (token) and Session (cumulative
    // cost) observations. A cost-ONLY result produces no Turn after r38 item 3.
    fn map_result(
        usage: Option<serde_json::Value>,
        total_cost_usd: Option<f64>,
    ) -> (Option<Observation>, Option<Observation>) {
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
        let obs = mapper.map(frame).expect("result frame maps");
        let mut turn = None;
        let mut session = None;
        for o in obs {
            if let ObservationPayload::Usage(u) = &o.body {
                match u.scope {
                    UsageScope::Turn => turn = Some(o),
                    UsageScope::Session => session = Some(o),
                    _ => {}
                }
            }
        }
        (turn, session)
    }

    let at_revision = |mut observation: Observation, revision: u64| -> Observation {
        if let ObservationPayload::Usage(payload) = &mut observation.body {
            payload.metric_revision = U64(revision);
        }
        observation
    };
    let with_native_at = |mut observation: Observation, at: &str| -> Observation {
        observation.native_at = remuda_protocol::Knowledge::Known {
            value: remuda_protocol::Timestamp::try_from(at.to_string()).unwrap(),
        };
        observation
    };

    // First frame: full counters AND cumulative reported cost 0.5.
    let (first_turn, first_session) = map_result(
        Some(serde_json::json!({"input_tokens": 100, "output_tokens": 10})),
        Some(0.5),
    );
    let first = with_native_at(
        first_turn.expect("full result yields a Turn"),
        "2026-10-10T00:00:00.000Z",
    );
    let first_session = with_native_at(
        first_session.expect("result yields a Session carrier"),
        "2026-10-10T00:00:00.000Z",
    );
    if let ObservationPayload::Usage(payload) = &first.body {
        assert!(
            matches!(payload.cost, remuda_protocol::Knowledge::Unknown { .. }),
            "the fallback Turn never carries the cumulative session bill"
        );
    }
    // A result missing input_tokens (no cost) still yields a partial Turn.
    let (second_opt, _) = map_result(Some(serde_json::json!({"output_tokens": 20})), None);
    let second = with_native_at(second_opt.unwrap(), "2026-10-10T00:00:02.000Z");
    if let ObservationPayload::Usage(payload) = &second.body {
        assert_eq!(payload.scope, remuda_protocol::UsageScope::Turn);
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
        panic!("second frame must map to a Turn");
    }

    // Cost-only frame: ONLY the Session carrier after r38 item 3.
    let (third_turn, third_session) = map_result(None, Some(0.75));
    assert!(
        third_turn.is_none(),
        "a cost-only result emits no additive Turn"
    );
    let third = with_native_at(
        third_session.expect("a cost-only result still updates the session bill"),
        "2026-10-10T00:00:03.000Z",
    );
    const FIRST_CLOCK: &str = "2026-10-10T00:00:00.000Z";
    const SECOND_CLOCK: &str = "2026-10-10T00:00:02.000Z";

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
        let Some(row) = project_usage_event(&record, None, None) else {
            return false;
        };
        insert_usage_event(&conn, &row).expect("insert")
    };

    // Baseline Turn rev1 (100/10) + the cumulative 0.5 session bill.
    assert!(insert_observation(next_seq(), &first));
    assert!(insert_observation(next_seq(), &first_session));
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
    let after_first = aggregate_instance(&conn, "ins_partial_merge").unwrap();
    assert!(
        (after_first.cost_usd.unwrap() - 0.5).abs() < 1e-9,
        "the first cumulative bill is 0.5, never Turn + session 1.0"
    );

    // Equal-position partial frame changes nothing.
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
    assert_eq!(output, Some(10), "the equal-position 10->20 fill is frozen");

    // Strictly NEWER partial frame fills output; total stays U1-Unknown.
    assert!(
        insert_observation(next_seq(), &at_revision(second.clone(), 2)),
        "a strictly newer partial frame is applied"
    );
    let (input, output, total): (Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT input_tokens, output_tokens, total_tokens FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(input, Some(100), "the unreported input must be retained");
    assert_eq!(output, Some(20), "the reported output advances");
    assert_eq!(
        total, None,
        "U1: never-reported cache baskets keep the total Unknown"
    );

    // Corrected cumulative bill on the SESSION carrier — applies independently
    // of the counter watermark; counters/clock untouched; bill becomes 0.75
    // once, never a sum.
    assert!(
        insert_observation(next_seq(), &third),
        "a cost-only session carrier corrects the cumulative bill"
    );
    let (input, output, total, clock): (Option<i64>, Option<i64>, Option<i64>, String) = conn
        .query_row(
            "SELECT input_tokens, output_tokens, total_tokens, observed_at FROM \
             usage_events WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND \
             scope_id = '1:0'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        input,
        Some(100),
        "counters are untouched by the session bill"
    );
    assert_eq!(output, Some(20));
    assert_eq!(
        total, None,
        "the partial basket keeps the total Unknown (U1)"
    );
    assert_eq!(
        clock, SECOND_CLOCK,
        "the session bill never moves the turn clock"
    );

    // Replay the first frame at the older counter position: frozen.
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
    assert!(
        !insert_observation(next_seq(), &at_revision(second.clone(), 2)),
        "an equal-position counter snapshot changes nothing"
    );
    let (input, output, total): (Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT input_tokens, output_tokens, total_tokens FROM usage_events \
             WHERE instance_id = 'ins_partial_merge' AND scope = 'turn' AND scope_id = '1:0'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(input, Some(100), "input retained");
    assert_eq!(output, Some(20), "the equal-position growth is frozen");
    assert_eq!(total, None, "the Unknown derived total is retained (U1)");

    // Latest reported cumulative folded once: 0.5/0.75 are the same running
    // counter, never added.
    let final_agg = aggregate_instance(&conn, "ins_partial_merge").unwrap();
    assert!(
        (final_agg.cost_usd.unwrap() - 0.75).abs() < 1e-9,
        "the bill is the newest cumulative reported value, not a sum: {:?}",
        final_agg.cost_usd
    );
}
