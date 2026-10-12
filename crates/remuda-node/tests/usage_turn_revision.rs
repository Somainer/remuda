//! c-ctxusage r14 item 2: turn revisions are REPLAY-STABLE source positions,
//! not per-mapper publication counters.
//!
//! The old revision was "number of usage snapshots this mapper published for
//! the message". A fresh mapper replaying the same transcript reset the
//! counter, so it replayed A(20)/B(15)/C(10) at revisions 1.. even though the
//! live mapper had reached revision 3 — and a genuinely new D(5), published by
//! the fresh mapper at its second publication (revision 2), was then REJECTED
//! AT THE HUB as stale. Downward corrections after a re-hydration were lost.
//!
//! The revision now carries the record's position in the session transcript
//! (record index + 1). It is a property of the file: any mapper, any poll
//! batching hands the same record the same revision. The Hub's watermark then
//! lets the fresh replay fold harmlessly while D, at a strictly newer
//! position, applies (10 -> 5, downward).
//!
//! Cumulative SESSION monotonicity is a separate path and is not exercised
//! here.

use remuda_driver::TranscriptMapper;
use remuda_hub::usage_store_test_support::{
    JournalRecord, insert_usage_event, migrate, project_usage_event,
};
use remuda_protocol::{
    DriverKind, HostId, Id, InstanceId, Knowledge, Observation, ObservationPayload, RunId, U64,
};
use serde_json::json;

const CORRECTED_MSG_ID: &str = "msg_corrected_turn";

fn mapper() -> TranscriptMapper {
    TranscriptMapper::new(
        DriverKind::ClaudePty,
        InstanceId::new(),
        RunId::new(),
        Id::new("obj").expect("journal id"),
        HostId::new(),
        "revision-session".into(),
        "2.1.289".into(),
    )
}

fn assistant_record(uuid: &str, position: u64, output: u64) -> serde_json::Value {
    json!({
        "type": "assistant",
        "uuid": uuid,
        // Distinct, ordered native timestamps at 5 s spacing.
        "timestamp": format!("2026-10-09T00:00:{:02}.000Z", position * 5),
        "isSidechain": false,
        "requestId": "req-corrected",
        "message": {
            "id": CORRECTED_MSG_ID,
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "corrected"}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": output
            }
        }
    })
}

/// Feed one poll batch at explicit file positions, returning the batch's usage
/// snapshot (if the poll finalised one), as the transcript pump does via
/// `map_line_at` + `flush`.
fn poll_batch(
    mapper: &mut TranscriptMapper,
    batch: &[(u64, serde_json::Value)],
) -> Option<Observation> {
    let mut found = None;
    for (position, record) in batch {
        for obs in mapper
            .map_record_at(record.clone(), *position)
            .expect("record maps")
        {
            if matches!(obs.body, ObservationPayload::Usage(_)) {
                found = Some(obs);
            }
        }
    }
    for obs in mapper.flush().expect("poll flush") {
        if matches!(obs.body, ObservationPayload::Usage(_)) {
            found = Some(obs);
        }
    }
    found
}

fn known_u64(value: &Knowledge<U64>) -> Option<u64> {
    match value {
        Knowledge::Known { value } => Some(value.0),
        _ => None,
    }
}

fn revision_and_output(obs: &Observation) -> (u64, u64) {
    match &obs.body {
        ObservationPayload::Usage(payload) => (
            payload.metric_revision.0,
            known_u64(&payload.output_tokens).expect("reported output"),
        ),
        _ => panic!("usage payload"),
    }
}

fn to_journal_record(seq: i64, observation: &Observation) -> JournalRecord {
    JournalRecord {
        instance_id: "ins_turn_revision".into(),
        seq,
        event_id: format!("evt_rev_{seq}"),
        observed_at: String::from(observation.observed_at.clone()),
        event: serde_json::to_value(observation).expect("serialize observation"),
    }
}

#[test]
fn replay_stable_positions_survive_fresh_mapper_batching_and_admit_a_new_downward_correction() {
    let a = || assistant_record("rec-a", 0, 20);
    let b = || assistant_record("rec-b", 1, 15);
    let c = || assistant_record("rec-c", 2, 10);
    let d = || assistant_record("rec-d", 3, 5);

    // LIVE mapper: one record per poll -> revisions 1, 2, 3.
    let mut live = mapper();
    let live_a = poll_batch(&mut live, &[(0, a())]).expect("A publishes");
    let live_b = poll_batch(&mut live, &[(1, b())]).expect("B publishes");
    let live_c = poll_batch(&mut live, &[(2, c())]).expect("C publishes");
    assert_eq!(revision_and_output(&live_a), (1, 20));
    assert_eq!(revision_and_output(&live_b), (2, 15));
    assert_eq!(revision_and_output(&live_c), (3, 10));

    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");

    let stored = |conn: &rusqlite::Connection| -> (i64, Option<i64>) {
        conn.query_row(
            "SELECT metric_revision, output_tokens FROM usage_events
             WHERE instance_id = 'ins_turn_revision'
               AND scope = 'turn' AND scope_id = ?1",
            rusqlite::params![CORRECTED_MSG_ID],
            |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
        )
        .expect("the scoped row exists")
    };

    for (seq, obs) in [&live_a, &live_b, &live_c].into_iter().enumerate() {
        let row = project_usage_event(&to_journal_record(seq as i64 + 1, obs), None, None)
            .expect("projects");
        assert!(insert_usage_event(&conn, &row).expect("insert live frame"));
    }
    assert_eq!(
        stored(&conn),
        (3, Some(10)),
        "live reaches revision 3 / output 10"
    );

    // FRESH MAPPER replays the same file with DIFFERENT batching: [A, B] in
    // one poll, [C] in the next. Publication counters would have come out as
    // 1, 2 — source positions still come out as 2 (B) and 3 (C).
    let mut replay = mapper();
    let replay_b = poll_batch(&mut replay, &[(0, a()), (1, b())]).expect("[A,B] finalises B");
    let replay_c = poll_batch(&mut replay, &[(2, c())]).expect("C finalises");
    assert_eq!(
        revision_and_output(&replay_b),
        (2, 15),
        "a fresh mapper's batched replay keeps B's source position, not publication #1"
    );
    assert_eq!(revision_and_output(&replay_c), (3, 10));

    // The replay folds against the watermark WITHOUT regressing the row:
    // rev 2 is older than the stored 3 (its 15 must not come back), rev 3 is
    // equal and identical.
    for (seq, obs) in [&replay_b, &replay_c].into_iter().enumerate() {
        let row = project_usage_event(&to_journal_record(seq as i64 + 10, obs), None, None)
            .expect("projects replay frame");
        let applied = insert_usage_event(&conn, &row).expect("insert replay frame");
        if revision_and_output(obs).0 == 3 {
            assert!(
                !applied,
                "an equal-position identical snapshot changes nothing"
            );
        } else {
            assert!(!applied, "an older-position replay (B at 15) is frozen");
        }
    }
    assert_eq!(
        stored(&conn),
        (3, Some(10)),
        "the fresh-mapper replay leaves the row at revision 3 / 10"
    );

    // The genuinely new D(5) is at file position 3 -> revision 4, regardless
    // of which mapper publishes it. The live mapper's next poll produces it.
    let live_d = poll_batch(&mut live, &[(3, d())]).expect("D publishes");
    assert_eq!(
        revision_and_output(&live_d),
        (4, 5),
        "D is revision 4 (position 3 + 1), not the fresh mapper's publication #2"
    );
    let row_d =
        project_usage_event(&to_journal_record(20, &live_d), None, None).expect("projects D");
    assert!(
        insert_usage_event(&conn, &row_d).expect("insert D"),
        "a strictly newer position applies its downward correction"
    );
    assert_eq!(
        stored(&conn),
        (4, Some(5)),
        "the new downward correction 10 -> 5 is applied"
    );

    // A replay of C(10) at revision 3 after D is frozen: older positions never
    // roll a correction back.
    let mut another = mapper();
    let replay_c_again =
        poll_batch(&mut another, &[(0, a()), (1, b()), (2, c())]).expect("[A,B,C] finalises C");
    assert_eq!(revision_and_output(&replay_c_again), (3, 10));
    let stale = project_usage_event(&to_journal_record(21, &replay_c_again), None, None)
        .expect("projects stale C");
    assert!(
        !insert_usage_event(&conn, &stale).expect("insert stale C"),
        "the older C must not roll the row back from D"
    );
    assert_eq!(stored(&conn), (4, Some(5)), "D survives the stale replay");
}
