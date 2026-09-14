//! Synthetic verifiers count expensive auth work without running Argon2 for
//! every fixture row. Production always passes hash_secret / verify_secret.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn count_verifies(calls: &Arc<AtomicUsize>) -> impl Fn(&str, &str) -> bool + Send + 'static {
    let calls = calls.clone();
    move |token, hash| {
        calls.fetch_add(1, Ordering::SeqCst);
        token == hash
    }
}

fn fixture_token(i: usize) -> String {
    format!("{i:016x}{}", "f".repeat(48))
}

#[tokio::test]
async fn device_verification_yields_writer_and_rechecks_revocation_hash_and_scope() {
    for mutation in ["none", "delete", "replace-hash", "change-scope"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let token = fixture_token(7);
        let device = store
            .insert_device("original".into(), token.clone(), token[..16].into())
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let entered_verifier = entered.clone();
        let (release, paused) = std::sync::mpsc::channel();
        let auth_store = store.clone();
        let authentication = tokio::spawn(async move {
            auth_store
                .find_device_by_token(token, None, move |token, hash| {
                    entered_verifier.notify_one();
                    paused.recv_timeout(Duration::from_secs(5)).is_ok() && token == hash
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("verifier entered");

        let id = device.id.clone();
        let progress = tokio::time::timeout(Duration::from_secs(1), async {
            // This uses the same actor queue as native journal appends.
            assert_eq!(store.list_devices().await?.len(), 1);
            match mutation {
                "delete" => {
                    store.delete_device(id).await?;
                }
                "replace-hash" => {
                    store
                        .run(move |conn| {
                            conn.execute(
                                "UPDATE devices SET token_hash = 'replacement' WHERE id = ?1",
                                params![id],
                            )?;
                            Ok(())
                        })
                        .await?;
                }
                "change-scope" => {
                    store.run(move |conn| {
                        conn.execute(
                            "UPDATE devices SET name = 'current', kind = 'agent', instance_id = 'ins_current_scope' WHERE id = ?1",
                            params![id],
                        )?;
                        Ok(())
                    }).await?;
                }
                _ => {}
            }
            Ok::<_, StoreError>(())
        })
        .await;
        // Unblock before asserting so the old writer-bound implementation
        // fails promptly instead of leaving a blocked thread during cleanup.
        release.send(()).unwrap();
        let authenticated = authentication.await.unwrap().unwrap();
        progress
            .expect("paused verification must not block store work")
            .unwrap();
        match mutation {
            "delete" | "replace-hash" => assert!(authenticated.is_none(), "{mutation}"),
            "change-scope" => {
                let current = authenticated.unwrap();
                assert_eq!(current.id, device.id);
                assert_eq!(current.name, "current");
                assert_eq!(current.kind, "agent");
                assert_eq!(current.instance_id.as_deref(), Some("ins_current_scope"));
            }
            _ => assert_eq!(authenticated.unwrap().id, device.id),
        }
        store.close().await;
    }
}

#[tokio::test]
async fn opening_legacy_schema_preserves_records_and_adds_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let token = fixture_token(7);
    {
        let conn = Connection::open(dir.path().join("hub.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE devices (id TEXT PRIMARY KEY, name TEXT NOT NULL, token_hash TEXT NOT NULL, created_at TEXT NOT NULL, last_seen_at TEXT NOT NULL);
            CREATE TABLE pair_codes (code_hash TEXT PRIMARY KEY, created_by TEXT NOT NULL, expires_at TEXT NOT NULL, used INTEGER NOT NULL DEFAULT 0);").unwrap();
        conn.execute(
            "INSERT INTO devices VALUES ('dev_legacy', 'desk', ?1, '2020', '2020')",
            params![token],
        )
        .unwrap();
        conn.execute("INSERT INTO pair_codes VALUES ('ABCDEFGH', 'dev_legacy', '2099-01-01T00:00:00.000Z', 0)", []).unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.list_devices().await.unwrap().len(), 1);
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        store
            .find_device_by_token(token.clone(), None, count_verifies(&calls))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .consume_pair_code("ABCDEFGH".into(), now_rfc3339(), count_verifies(&calls))
            .await
            .unwrap(),
        "old pairing codes require reissue"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .find_device_by_token(token, Some("dev_legacy".into()), count_verifies(&calls))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    store.close().await;
}

#[tokio::test]
async fn device_lookup_is_indexed_and_legacy_migration_verifies_only_the_named_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut devices = Vec::new();
    for i in 0..64 {
        let token = fixture_token(i);
        devices.push(
            store
                .insert_device(format!("device-{i}"), token.clone(), token[..16].into())
                .await
                .unwrap(),
        );
    }
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        store
            .find_device_by_token(fixture_token(999), None, count_verifies(&calls))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "unknown selector must not scan hashes"
    );
    let token = fixture_token(37);
    let wrong = format!("{}e", &token[..63]);
    assert!(
        store
            .find_device_by_token(wrong, None, count_verifies(&calls))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "known selector still requires the full secret"
    );
    let found = store
        .find_device_by_token(
            token.clone(),
            Some(devices[0].id.clone()),
            count_verifies(&calls),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, devices[37].id);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let legacy_id = devices[37].id.clone();
    let id = legacy_id.clone();
    store
        .run(move |conn| {
            conn.execute(
                "UPDATE devices SET token_prefix = NULL WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        store
            .find_device_by_token(token.clone(), None, count_verifies(&calls))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .find_device_by_token(
                token.clone(),
                Some(devices[0].id.clone()),
                count_verifies(&calls)
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "legacy auth must not fall back to scanning"
    );
    assert_eq!(
        store
            .find_device_by_token(
                token.clone(),
                Some(legacy_id.clone()),
                count_verifies(&calls)
            )
            .await
            .unwrap()
            .unwrap()
            .id,
        legacy_id
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    store.close().await;
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store
            .find_device_by_token(token, None, count_verifies(&calls))
            .await
            .unwrap()
            .unwrap()
            .id,
        legacy_id
    );
    assert_eq!(store.list_devices().await.unwrap().len(), 64);
    store.close().await;
}

fn host_request(token: String, host_id: Option<String>) -> HostAuthRequest {
    HostAuthRequest {
        presented: token,
        hello_host_id: host_id,
        label: None,
        node_version: None,
    }
}

/// D-018: mint a single-use enroll token whose stored "hash" is its plaintext,
/// so these tests keep counting only the verifier calls they care about.
async fn mint_enroll(store: &Store, label: &str) -> String {
    // Real tokens are 64 hex chars; the prefix index only applies to those.
    let mut hex: String = label.bytes().map(|b| format!("{b:02x}")).collect();
    hex.truncate(64);
    let plaintext = format!("{hex:0<64}");
    let plaintext = plaintext.as_str();
    store
        .insert_enroll_token(
            plaintext.to_string(),
            crate::auth::token_prefix(plaintext).map(str::to_string),
            "dev_test".into(),
            "2099-01-01T00:00:00.000Z".into(),
        )
        .await
        .unwrap();
    plaintext.to_string()
}

#[tokio::test]
async fn host_lookup_and_legacy_host_id_migration_are_bounded_and_bound_to_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut hosts = Vec::new();
    for i in 0..32 {
        let enroll = mint_enroll(&store, &format!("enroll-{i}")).await;
        let HostAuthOutcome::Authenticated {
            host,
            node_token: Some(token),
        } = store
            .authenticate_host(
                host_request(enroll, None),
                count_verifies(&calls),
                |token| Ok(token.into()),
            )
            .await
            .unwrap()
        else {
            panic!("new host");
        };
        hosts.push((host.host_id, token));
    }
    // Exactly one Argon2 verify per enrollment: the presented enroll token is
    // found by its prefix index. A scan of live tokens or enrolled hosts would
    // be quadratic here (0+1+…+31 = 496), not linear (A4).
    assert_eq!(
        calls.load(Ordering::SeqCst),
        32,
        "enrollment must verify once, not scan"
    );
    calls.store(0, Ordering::SeqCst);
    assert!(matches!(
        store
            .authenticate_host(
                host_request(fixture_token(999), None),
                count_verifies(&calls),
                |_| panic!("must not enroll")
            )
            .await
            .unwrap(),
        HostAuthOutcome::Rejected
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (id, token) = hosts[17].clone();
    let HostAuthOutcome::Authenticated {
        host,
        node_token: None,
    } = store
        .authenticate_host(
            host_request(token.clone(), Some(hosts[0].0.clone())),
            count_verifies(&calls),
            |_| panic!("must not enroll"),
        )
        .await
        .unwrap()
    else {
        panic!("host token");
    };
    assert_eq!(
        host.host_id, id,
        "claimed id must not override token ownership"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let legacy_id = id.clone();
    store
        .run(move |conn| {
            conn.execute(
                "UPDATE hosts SET token_prefix = NULL WHERE id = ?1",
                params![legacy_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(
        store
            .authenticate_host(
                host_request(token.clone(), Some(hosts[0].0.clone())),
                count_verifies(&calls),
                |_| panic!("must not enroll")
            )
            .await
            .unwrap(),
        HostAuthOutcome::Rejected
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let HostAuthOutcome::Authenticated {
        host,
        node_token: None,
    } = store
        .authenticate_host(
            host_request(token.clone(), Some(id.clone())),
            count_verifies(&calls),
            |_| panic!("must not enroll"),
        )
        .await
        .unwrap()
    else {
        panic!("legacy host token");
    };
    assert_eq!(host.host_id, id);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    store.close().await;
    let store = Store::open(dir.path()).unwrap();
    assert!(matches!(
        store
            .authenticate_host(
                host_request(token, None),
                count_verifies(&calls),
                |_| panic!("must not enroll")
            )
            .await
            .unwrap(),
        HostAuthOutcome::Authenticated { .. }
    ));
    assert_eq!(store.list_hosts().await.unwrap().len(), 32);
    store.close().await;
}

#[tokio::test]
async fn pair_codes_use_one_candidate_expire_lock_out_and_consume_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let expires = "2099-01-01T00:00:00.000Z";
    let now = now_rfc3339();
    assert!(
        store
            .insert_pair_code(
                "ABCDEFGH".into(),
                "ABCD".into(),
                "device".into(),
                expires.into()
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .insert_pair_code(
                "ABCDZZZZ".into(),
                "ABCD".into(),
                "device".into(),
                expires.into()
            )
            .await
            .unwrap(),
        "selector collisions must be retried, not create multiple candidates"
    );
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        !store
            .consume_pair_code("ZZZZZZZZ".into(), now.clone(), count_verifies(&calls))
            .await
            .unwrap()
    );
    assert!(
        !store
            .consume_pair_code("bad".into(), now.clone(), count_verifies(&calls))
            .await
            .unwrap()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for _ in 0..10 {
        assert!(
            !store
                .consume_pair_code("ABCDZZZZ".into(), now.clone(), count_verifies(&calls))
                .await
                .unwrap()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 10);
    assert!(
        !store
            .consume_pair_code("ABCDEFGH".into(), now.clone(), count_verifies(&calls))
            .await
            .unwrap(),
        "locked pairing code must stay denied even with its secret"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 10);
    assert!(
        store
            .insert_pair_code(
                "JKLMPQRS".into(),
                "JKLM".into(),
                "device".into(),
                expires.into()
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .consume_pair_code("JKLMPQRS".into(), expires.into(), count_verifies(&calls))
            .await
            .unwrap(),
        "expiry is inclusive"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 10);
    assert!(
        store
            .consume_pair_code("JKLMPQRS".into(), now.clone(), count_verifies(&calls))
            .await
            .unwrap()
    );
    assert!(
        !store
            .consume_pair_code("JKLMPQRS".into(), now, count_verifies(&calls))
            .await
            .unwrap()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 11);
    store.close().await;
}

#[tokio::test]
async fn sqlite_query_plans_use_auth_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .run(|conn| {
            for (table, column, index) in [
                ("devices", "token_prefix", "devices_token_prefix"),
                ("hosts", "token_prefix", "hosts_token_prefix"),
                ("pair_codes", "code_prefix", "pair_codes_prefix"),
            ] {
                let plan: String = conn.query_row(
                    &format!("EXPLAIN QUERY PLAN SELECT * FROM {table} WHERE {column} = ?1"),
                    ["fixture"],
                    |row| row.get(3),
                )?;
                assert!(plan.contains("SEARCH") && plan.contains(index), "{plan}");
            }
            Ok(())
        })
        .await
        .unwrap();
    store.close().await;
}
