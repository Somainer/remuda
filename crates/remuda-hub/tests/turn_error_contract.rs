//! Contract test (ma-sdk-state r8 item 5): the mobile home-rows UI consumes a
//! REAL Hub-serialized [`InstanceRecord`] carrying `lastTurnError` after a
//! settled root-turn API error (and none after the next turn start).
//!
//! The golden JSON committed at
//! `web/src/features/mobile/__fixtures__/last-turn-error-instance.json` is read
//! by `homeRows.test.ts`. It is produced here by driving the real store
//! (`ensure_instance` + `append_journal` through the production derive/update
//! path), then normalizing the volatile columns (timestamps/ids) so the byte
//! comparison is stable. If the Hub stops persisting or renames
//! `lastTurnError`, this test fails and the web fixture cannot silently drift.
//!
//! Regenerate: `UPDATE_LAST_TURN_ERROR_FIXTURE=1 cargo test -p remuda-hub
//! --test turn_error_contract`

use remuda_hub::store_test_support::open_with_host;
use serde_json::{Value, json};
use std::path::PathBuf;

const HOST: &str = "hst-turn-error-contract";
const INSTANCE: &str = "ins-turn-error-contract";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/features/mobile/__fixtures__/last-turn-error-instance.json")
}

/// Replace volatile per-run values with fixed placeholders so the golden is
/// byte-stable while preserving every structural/serde field.
fn normalize(mut v: Value) -> Value {
    let o = v.as_object_mut().expect("instance is an object");
    for (key, placeholder) in [
        ("id", INSTANCE),
        ("hostId", HOST),
        ("journalId", "obj-journal"),
        ("createdAt", "2026-01-01T00:00:00.000Z"),
        ("updatedAt", "2026-01-01T00:00:01.000Z"),
        ("durableSeq", "3"),
    ] {
        o.insert(key.to_string(), json!(placeholder));
    }
    v
}

#[tokio::test]
async fn serialized_instance_with_last_turn_error_matches_web_golden() {
    let dir = tempfile::tempdir().unwrap();
    let (store, host_id) = open_with_host(dir.path(), HOST).await.unwrap();
    store
        .ensure_instance(host_id.clone(), INSTANCE.to_string())
        .await
        .unwrap();

    let turn = |name: &str, status: &str, related: Value| {
        json!({
            "kind": "lifecycle",
            "payload": {
                "type": "native",
                "topic": "turn",
                "nativeName": name,
                "nativeId": { "state": "known", "value": "sess" },
                "status": { "state": "known", "value": status },
                "relatedIds": related,
                "dataRef": null,
                "severity": "info",
                "affectsCompletion": false
            }
        })
    };

    // A turn starts (working), then ends in a SETTLED API error: the durable
    // lastTurnError marker must be set while the instance keeps running.
    let start = turn(
        "turn_started",
        "working",
        json!({"nativeClientMessageId": "m1"}),
    );
    let settled_error = turn(
        "result",
        "error",
        json!({
            "resultIndex": "1",
            "queuedTurnCount": "0",
            "settledRootTurn": "true",
            "lastError": "API Error: 429 no eligible upstream"
        }),
    );
    store
        .append_journal(host_id.clone(), INSTANCE.into(), None, start)
        .await
        .unwrap();
    store
        .append_journal(host_id.clone(), INSTANCE.into(), None, settled_error)
        .await
        .unwrap();

    let row = store
        .get_instance(INSTANCE.to_string())
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        row.lifecycle, "running",
        "a turn error is not a process exit"
    );
    assert_eq!(row.activity, "idle");
    let marker = row
        .last_turn_error
        .as_ref()
        .expect("a settled error persists lastTurnError");
    assert_eq!(marker.text, "API Error: 429 no eligible upstream");

    let mut actual = normalize(serde_json::to_value(&row).unwrap());
    // Normalize the marker timestamp too (observedAt/now derived at append).
    if let Some(obj) = actual.as_object_mut()
        && let Some(m) = obj.get_mut("lastTurnError")
        && let Some(mo) = m.as_object_mut()
    {
        mo.insert("at".into(), json!("2026-01-01T00:00:01.000Z"));
    }

    let actual_str = serde_json::to_string_pretty(&actual).unwrap();
    let path = fixture_path();

    if std::env::var_os("UPDATE_LAST_TURN_ERROR_FIXTURE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{actual_str}\n")).unwrap();
        panic!("fixture regenerated at {}", path.display());
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("missing web golden {path:?}: {e}; regenerate with UPDATE_LAST_TURN_ERROR_FIXTURE=1")
    });
    assert_eq!(
        actual_str.trim(),
        expected.trim(),
        "Hub-serialized InstanceRecord drifted from the web golden; \
         regenerate with UPDATE_LAST_TURN_ERROR_FIXTURE=1"
    );
}
