//! c-ctxusage r13 item 3: turn snapshot revision authority, mapper to Hub.
//!
//! The real TranscriptMapper republishes the SAME assistant message id with a
//! higher metric revision when a later record for the run carries a corrected
//! `message.usage` (Group::take_usage advances the revision per publish). A
//! newer revision is the source's authoritative correction and must replace
//! explicitly reported buckets even when a value moves DOWN (output 20 → 15),
//! keeping the other baskets and recomputing the total. An older or equal
//! revision never overrides: a stale rev-1 frame (or an equal-revision
//! decrease) is frozen, independent of cumulative SESSION monotonicity.
//!
//! Before the fix the scoped upsert rejected ANY reported decrease
//! (`grew && !decreased`), so the newer correction was dropped and the row
//! stayed at the provisional 20.

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

fn assistant_record(uuid: &str, output: u64) -> serde_json::Value {
    json!({
        "type": "assistant",
        "uuid": uuid,
        "timestamp": "2026-10-09T00:01:00.000Z",
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

fn publish_on_poll(mapper: &mut TranscriptMapper, record: serde_json::Value) -> Observation {
    mapper
        .map_record(record)
        .expect("record maps")
        .into_iter()
        .chain(mapper.flush().expect("poll flush"))
        .find(|obs| matches!(obs.body, ObservationPayload::Usage(_)))
        .expect("the finalised group publishes one usage observation")
}

fn known_u64(value: &Knowledge<U64>) -> Option<u64> {
    match value {
        Knowledge::Known { value } => Some(value.0),
        _ => None,
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

/// A stale/equal revision frame built straight from the wire projection
/// shape, carrying only the baskets this correction asserts on.
fn revisioned_record(seq: i64, revision: u64, total: u64, output: u64) -> JournalRecord {
    JournalRecord {
        instance_id: "ins_turn_revision".into(),
        seq,
        event_id: format!("evt_rev_{seq}"),
        observed_at: format!("2026-10-09T00:01:0{seq}.000Z"),
        event: json!({
            "kind": "usage",
            "payload": {
                "scope": "turn",
                "scopeId": CORRECTED_MSG_ID,
                "mode": "snapshot",
                "metricRevision": revision.to_string(),
                "totalTokens": {"state": "known", "value": total.to_string()},
                "inputTokens": {"state": "known", "value": "100"},
                "outputTokens": {"state": "known", "value": output.to_string()},
            }
        }),
    }
}

#[test]
fn a_newer_revision_corrects_output_downward_older_and_equal_are_frozen() {
    let mut mapper = mapper();

    // Provisional publish: output 20 at revision 1.
    let provisional = publish_on_poll(&mut mapper, assistant_record("rec-a", 20));
    // The same message id, a later transcript record carrying the CORRECTED
    // smaller counter: revision 2.
    let corrected = publish_on_poll(&mut mapper, assistant_record("rec-b", 15));

    let rev_and_output = |obs: &Observation| match &obs.body {
        ObservationPayload::Usage(payload) => (
            payload.metric_revision.0,
            known_u64(&payload.output_tokens).expect("reported output"),
        ),
        _ => panic!("usage payload"),
    };
    assert_eq!(rev_and_output(&provisional), (1, 20));
    assert_eq!(
        rev_and_output(&corrected),
        (2, 15),
        "the correction publishes a newer revision"
    );

    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    migrate(&conn).expect("migrate");

    let stored = |conn: &rusqlite::Connection| -> (i64, Option<i64>, Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT metric_revision, input_tokens, output_tokens, total_tokens
             FROM usage_events
             WHERE instance_id = 'ins_turn_revision'
               AND scope = 'turn' AND scope_id = ?1",
            rusqlite::params![CORRECTED_MSG_ID],
            |r| {
                Ok((
                    r.get(0).unwrap(),
                    r.get(1).unwrap(),
                    r.get(2).unwrap(),
                    r.get(3).unwrap(),
                ))
            },
        )
        .expect("the scoped row exists")
    };

    let row1 = project_usage_event(&to_journal_record(1, &provisional), None, None)
        .expect("projects rev 1");
    assert!(insert_usage_event(&conn, &row1).expect("insert rev 1"));
    assert_eq!(stored(&conn), (1, Some(100), Some(20), Some(120)));

    // Newer revision with a DOWNWARD correction replaces the reported basket,
    // keeps input, recomputes the derived total.
    let row2 =
        project_usage_event(&to_journal_record(2, &corrected), None, None).expect("projects rev 2");
    assert!(
        insert_usage_event(&conn, &row2).expect("insert rev 2"),
        "a newer authoritative revision must be accepted despite the decrease"
    );
    assert_eq!(
        stored(&conn),
        (2, Some(100), Some(15), Some(115)),
        "output corrected 20 -> 15 and the total recomputed"
    );

    // An OLDER revision (rev 1) carrying a larger output never overrides.
    let stale = project_usage_event(&revisioned_record(3, 1, 199, 99), None, None)
        .expect("projects stale rev");
    assert!(
        !insert_usage_event(&conn, &stale).expect("stale insert"),
        "an older revision must be frozen even with larger counters"
    );

    // An EQUAL revision (rev 2) moving a reported basket down is frozen too.
    let equal_down = project_usage_event(&revisioned_record(4, 2, 110, 10), None, None)
        .expect("projects equal rev");
    assert!(
        !insert_usage_event(&conn, &equal_down).expect("equal-down insert"),
        "an equal-revision decrease must not override"
    );
    assert_eq!(
        stored(&conn),
        (2, Some(100), Some(15), Some(115)),
        "the corrected row is unchanged after stale/equal frames"
    );
}
