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

/// c-ctxusage r15 item 4 / r16 item 3 (T1'): positions are SESSION-QUALIFIED
/// and the watermark is kept PER LINEAGE. Two native sessions can both contain
/// an assistant message with the SAME id (the Hub keeps one
/// revision-replaced row per `(message id, source session id)`), and every
/// transcript starts at record position 1. A snapshot from a DIFFERENT session
/// must not be frozen against the prior session's high position — accepting it
/// inserts its own row and never erases the other lineage's mark — while the
/// strict watermark still holds WITHIN each session. The native session id
/// travels on the usage payload and is persisted next to the watermark.
#[test]
fn two_sessions_with_overlapping_message_ids_keep_independent_position_watermarks() {
    fn session_mapper(session: &str) -> TranscriptMapper {
        TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            session.into(),
            "2.1.289".into(),
        )
    }
    fn shared_record(position: u64, output: u64) -> serde_json::Value {
        json!({
            "type": "assistant",
            "uuid": format!("rec-{position}"),
            "timestamp": format!("2026-10-09T00:00:{:02}.000Z", position * 5),
            "isSidechain": false,
            "requestId": "req-shared",
            "message": {
                "id": "msg_shared_across_sessions",
                "role": "assistant",
                "type": "message",
                "model": "claude-opus-5-5",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "shared"}],
                "usage": { "input_tokens": 100, "output_tokens": output }
            }
        })
    }
    fn session_of(obs: &Observation) -> Option<String> {
        match &obs.body {
            ObservationPayload::Usage(payload) => payload.source_session_id.clone(),
            _ => panic!("usage payload"),
        }
    }
    let mut seq = 0i64;
    let mut apply = |conn: &rusqlite::Connection, obs: &Observation| -> bool {
        seq += 1;
        let rec = JournalRecord {
            instance_id: "ins_two_sessions".into(),
            seq,
            event_id: format!("evt_two_{seq}"),
            observed_at: String::from(obs.observed_at.clone()),
            event: serde_json::to_value(obs).expect("serialize"),
        };
        let row = project_usage_event(&rec, None, None).expect("projects");
        insert_usage_event(conn, &row).expect("insert")
    };
    let stored =
        |conn: &rusqlite::Connection, session: &str| -> (i64, Option<String>, Option<i64>) {
            conn.query_row(
                "SELECT metric_revision, source_session_id, output_tokens FROM usage_events
             WHERE instance_id = 'ins_two_sessions'
               AND scope = 'turn' AND scope_id = 'msg_shared_across_sessions'
               AND source_session_id = ?1",
                [session],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap(), r.get(2).unwrap())),
            )
            .expect("the scoped row exists")
        };

    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");

    // Session alpha reaches position 3 (revision 3) / output 20 for the shared
    // message id.
    let mut alpha = session_mapper("sess-alpha");
    let alpha_3 = poll_batch(
        &mut alpha,
        &[
            (0, shared_record(0, 30)),
            (1, shared_record(1, 25)),
            (2, shared_record(2, 20)),
        ],
    )
    .expect("alpha finalises");
    assert_eq!(session_of(&alpha_3).as_deref(), Some("sess-alpha"));
    assert!(apply(&conn, &alpha_3));
    assert_eq!(
        stored(&conn, "sess-alpha"),
        (3, Some("sess-alpha".into()), Some(20))
    );

    // Session beta is a DIFFERENT transcript with the same message id,
    // replayed from position 1. Its low revision is a different lineage, so
    // the snapshot applies rather than being frozen against alpha's 3.
    let mut beta = session_mapper("sess-beta");
    let beta_1 = poll_batch(&mut beta, &[(0, shared_record(0, 9))]).expect("beta rev1");
    assert_eq!(session_of(&beta_1).as_deref(), Some("sess-beta"));
    assert!(
        apply(&conn, &beta_1),
        "a different session at a low position is a different authority"
    );
    assert_eq!(
        stored(&conn, "sess-beta"),
        (1, Some("sess-beta".into()), Some(9))
    );
    // beta's acceptance never erased alpha's own mark.
    assert_eq!(
        stored(&conn, "sess-alpha"),
        (3, Some("sess-alpha".into()), Some(20)),
        "accepting another lineage never erases a mark (T1')"
    );

    // WITHIN beta the strict watermark still holds: re-delivering rev 1 with a
    // growth changes nothing...
    assert!(
        !apply(&conn, &beta_1),
        "an equal-position replay within the same session changes nothing"
    );
    assert_eq!(
        stored(&conn, "sess-beta"),
        (1, Some("sess-beta".into()), Some(9))
    );
    // ...and beta rev 2 applies.
    let beta_2 = poll_batch(&mut beta, &[(1, shared_record(1, 12))]).expect("beta rev2");
    assert!(apply(&conn, &beta_2), "beta rev 2 is newer within beta");
    assert_eq!(
        stored(&conn, "sess-beta"),
        (2, Some("sess-beta".into()), Some(12))
    );

    // An alpha frame at revision 2 is STALE within alpha (alpha already
    // reached 3): it must not roll backwards against alpha's OWN mark, even
    // while beta holds its separate row.
    let alpha_2 = poll_batch(
        &mut session_mapper("sess-alpha"),
        &[(0, shared_record(0, 30)), (1, shared_record(1, 99))],
    )
    .expect("alpha rev2 replay");
    assert!(
        !apply(&conn, &alpha_2),
        "alpha rev 2 is stale against alpha's own watermark 3 — beta never erased it"
    );
    assert_eq!(
        stored(&conn, "sess-alpha"),
        (3, Some("sess-alpha".into()), Some(20)),
        "the stale alpha frame never writes 99"
    );
    // A genuinely newer alpha frame (rev 4) still advances its own lineage.
    let alpha_4 = poll_batch(&mut alpha, &[(3, shared_record(3, 18))]).expect("alpha rev4");
    assert!(
        apply(&conn, &alpha_4),
        "alpha rev 4 advances the alpha lineage"
    );
    assert_eq!(
        stored(&conn, "sess-alpha"),
        (4, Some("sess-alpha".into()), Some(18))
    );
    assert_eq!(
        stored(&conn, "sess-beta"),
        (2, Some("sess-beta".into()), Some(12)),
        "the beta lineage survives untouched"
    );
}

/// c-ctxusage r15 item 5: a partial transcript record whose `message.usage`
/// OMITS buckets (e.g. only `output_tokens`) maps end to end to Unknown
/// buckets — never zeros — and is never priced on the missing counters. The
/// turn clock moves with the strictly newer partial snapshot while the
/// retained buckets survive the merge.
#[test]
fn a_partial_transcript_record_keeps_omitted_buckets_unknown_end_to_end() {
    use remuda_protocol::Knowledge;

    fn mapper() -> TranscriptMapper {
        TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "sess-omitted-buckets".into(),
            "2.1.289".into(),
        )
    }
    // Full record: every bucket reported.
    let full = json!({
        "type": "assistant",
        "uuid": "rec-full",
        "timestamp": "2026-10-09T00:00:00.000Z",
        "isSidechain": false,
        "requestId": "req-partial",
        "message": {
            "id": "msg_partial_buckets",
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "full"}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 0
            }
        }
    });
    // Later record for the SAME message id carrying ONLY output (a genuine
    // downward correction) — every other field absent from the native usage.
    let partial = json!({
        "type": "assistant",
        "uuid": "rec-partial",
        "timestamp": "2026-10-09T00:00:10.000Z",
        "isSidechain": false,
        "requestId": "req-partial",
        "message": {
            "id": "msg_partial_buckets",
            "role": "assistant",
            "type": "message",
            "model": "claude-opus-5-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "corrected"}],
            "usage": { "output_tokens": 15 }
        }
    });

    let mut live = mapper();
    let full_obs = poll_batch(&mut live, &[(0, full)]).expect("full publishes");
    let partial_obs = poll_batch(&mut live, &[(1, partial)]).expect("partial publishes");

    // At the mapper the omitted buckets are already Unknown (never zero),
    // the reported output is known, and the partial frame carries no bill.
    let ObservationPayload::Usage(partial_payload) = &partial_obs.body else {
        panic!("usage payload");
    };
    assert!(matches!(
        partial_payload.output_tokens,
        Knowledge::Known { value: U64(15) }
    ));
    for omitted in [
        &partial_payload.input_tokens,
        &partial_payload.cache_read_tokens,
        &partial_payload.cache_write_tokens,
        &partial_payload.total_tokens,
    ] {
        assert!(
            matches!(omitted, Knowledge::Unknown { .. }),
            "an omitted bucket is Unknown on the wire, never zero"
        );
    }
    assert!(
        matches!(partial_payload.cost, Knowledge::Unknown { .. }),
        "a partial record is never priced on missing buckets"
    );
    assert_eq!(
        partial_payload.metric_revision,
        U64(2),
        "strictly newer position"
    );

    // Through the Hub: the first frame lands fully...
    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");
    let full_row = project_usage_event(
        &JournalRecord {
            instance_id: "ins_partial_buckets".into(),
            seq: 1,
            event_id: "evt_full".into(),
            observed_at: "2020-01-01T00:00:00.000Z".into(),
            event: serde_json::to_value(&full_obs).unwrap(),
        },
        None,
        None,
    )
    .expect("projects full");
    assert!(insert_usage_event(&conn, &full_row).expect("insert full"));

    // ...and the strictly newer partial snapshot applies its downward output
    // correction while the retained buckets survive and the omitted ones
    // remain NULL (Unknown), with no cost written.
    let partial_row = project_usage_event(
        &JournalRecord {
            instance_id: "ins_partial_buckets".into(),
            seq: 2,
            event_id: "evt_partial".into(),
            observed_at: "2020-01-01T00:00:00.000Z".into(),
            event: serde_json::to_value(&partial_obs).unwrap(),
        },
        None,
        None,
    )
    .expect("projects partial");
    assert_eq!(partial_row.output_tokens, Some(15));
    assert_eq!(partial_row.input_tokens, None);
    assert_eq!(partial_row.cache_read_tokens, None);
    assert_eq!(partial_row.total_tokens, None);
    assert_eq!(partial_row.cost_usd, None);
    assert!(
        insert_usage_event(&conn, &partial_row).expect("insert partial"),
        "the strictly newer partial snapshot is applied"
    );
    let stored_partial = |column: &str| -> Option<rusqlite::types::Value> {
        conn.query_row(
            &format!(
                "SELECT {column} FROM usage_events
                         WHERE instance_id = 'ins_partial_buckets'
                           AND scope = 'turn' AND scope_id = 'msg_partial_buckets'"
            ),
            [],
            |r| r.get::<_, Option<rusqlite::types::Value>>(0),
        )
        .unwrap()
    };
    let as_int = |value: Option<rusqlite::types::Value>| match value {
        Some(rusqlite::types::Value::Integer(n)) => Some(n),
        Some(_) => panic!("expected INTEGER"),
        None => None,
    };
    assert_eq!(
        as_int(stored_partial("output_tokens")),
        Some(15),
        "the downward correction applies"
    );
    assert_eq!(
        as_int(stored_partial("input_tokens")),
        Some(100),
        "the retained input survives the merge"
    );
    assert_eq!(
        as_int(stored_partial("cache_read_tokens")),
        Some(30),
        "the retained cache_read survives"
    );
    assert_eq!(
        as_int(stored_partial("total_tokens")),
        Some(145),
        "the total is derived from the merged KNOWN baskets (100+30+0+15)"
    );
    let bill: Option<String> = conn
        .query_row(
            "SELECT cost_usd FROM usage_events
             WHERE instance_id = 'ins_partial_buckets'
               AND scope = 'turn' AND scope_id = 'msg_partial_buckets'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bill, full_row.cost_usd,
        "the partial frame reports no bill, so the stored bill is retained \
         (an unreported cost never erases a known one) rather than priced on zeros"
    );
}
