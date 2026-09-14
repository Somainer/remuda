//! Restart race against a Herdr server that is shutting down.
//!
//! Herdr derives a session's socket from the Node data dir, so a restarted
//! Node dials the socket the *previous* server still owns. That server answers
//! `ping` while refusing every other method with
//! `server_unavailable: server is shutting down`, which is how a dying server
//! used to pass for a healthy one and kill the Node on its first real call.

use std::time::{Duration, Instant};

use remuda_herdr::{Client, EnsureOptions, HerdrServer, RetryPolicy};
use remuda_testing::{FakeHerdrOptions, FakeHerdrServer};

/// Short steps so the test measures behaviour, not wall-clock patience.
fn fast(max_wait: Duration) -> RetryPolicy {
    RetryPolicy::with_max_wait(max_wait)
        .with_backoff(Duration::from_millis(5), Duration::from_millis(20))
}

#[tokio::test]
async fn server_unavailable_then_ok_is_retried_not_fatal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("herdr.sock");
    // Refuse three calls, then serve normally — the shutdown window closing
    // without the process actually exiting.
    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket).shutting_down_for(3))
        .expect("fake herdr");

    let client = Client::connect(&socket).with_timeout(Duration::from_secs(2));
    let first = client
        .session_snapshot()
        .await
        .expect_err("shutdown window refuses the first call");
    assert!(
        first.is_server_unavailable(),
        "expected server_unavailable, got {first}"
    );
    assert!(
        first.is_transient_carrier(),
        "a refused (undispatched) call must be retryable: {first}"
    );

    let policy = fast(Duration::from_secs(5));
    let started = Instant::now();
    let mut attempt = 0u32;
    let snapshot = loop {
        match client.session_snapshot().await {
            Ok(snapshot) => break snapshot,
            Err(error) if error.is_server_unavailable() => {
                let backoff = policy
                    .backoff(attempt, started.elapsed())
                    .expect("budget not exhausted by a 3-call window");
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(error) => panic!("unexpected error: {error}"),
        }
    };
    assert!(
        snapshot.workspaces.is_empty(),
        "fresh fake server starts empty"
    );
    assert!(attempt >= 1, "the retry loop must have actually retried");
}

#[tokio::test]
async fn ping_alone_cannot_tell_a_dying_server_from_a_healthy_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("herdr.sock");
    let _fake = FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket).shutting_down_for(2))
        .expect("fake herdr");
    let client = Client::connect(&socket).with_timeout(Duration::from_secs(2));

    // This is the whole trap: ping says yes while real work is refused.
    client.ping().await.expect("ping answers during shutdown");
    let error = client
        .session_snapshot()
        .await
        .expect_err("real work is refused during shutdown");
    assert!(error.is_server_unavailable(), "{error}");
}

#[tokio::test]
async fn ensure_waits_for_a_shutting_down_predecessor_then_starts_fresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_dir = dir.path().join("herdr");
    std::fs::create_dir_all(&socket_dir).expect("socket dir");
    let socket = socket_dir.join("herdr.sock");

    // A predecessor that refuses two calls and then genuinely exits, dropping
    // its socket — the real restart race.
    let fake = FakeHerdrServer::spawn(
        FakeHerdrOptions::new(&socket)
            .shutting_down_for(2)
            .exiting_after_shutdown(),
    )
    .expect("fake herdr");

    // `ensure` must not attach to it, and must not fail: it waits, then spawns
    // a server of its own. The fake stands in for the `herdr` binary, so no
    // real herdr process is involved.
    let started = Instant::now();
    let server = HerdrServer::ensure_with(
        EnsureOptions::new("remuda-node-retry-test", Some(socket_dir.clone()))
            .with_policy(fast(Duration::from_secs(20)))
            .with_binary(remuda_testing::fake_herdr_bin())
            .with_kill_on_drop(true),
    )
    .await
    .expect("ensure recovers instead of failing");

    assert!(
        server.renamed_from().is_none(),
        "the predecessor exited, so the original session name is reusable"
    );
    assert_eq!(server.socket_path(), socket.as_path());
    // It really waited rather than racing straight through.
    assert!(
        started.elapsed() >= Duration::from_millis(5),
        "expected at least one backoff step"
    );
    // And the endpoint it returns actually serves work.
    Client::connect(server.socket_path())
        .with_timeout(Duration::from_secs(2))
        .session_snapshot()
        .await
        .expect("the replacement server serves real requests");
    drop(fake);
}

#[tokio::test]
async fn a_predecessor_that_never_exits_forces_a_suffixed_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_dir = dir.path().join("herdr");
    std::fs::create_dir_all(&socket_dir).expect("socket dir");
    let socket = socket_dir.join("herdr.sock");

    // Stuck forever: refuses far more calls than the budget allows.
    let _stuck =
        FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket).shutting_down_for(usize::MAX))
            .expect("fake herdr");

    let server = HerdrServer::ensure_with(
        EnsureOptions::new("remuda-node-stuck-test", Some(socket_dir.clone()))
            .with_policy(fast(Duration::from_millis(120)))
            .with_binary(remuda_testing::fake_herdr_bin())
            .with_kill_on_drop(true),
    )
    .await
    .expect("a stuck predecessor must not fail the Node");

    assert_eq!(
        server.renamed_from(),
        Some("remuda-node-stuck-test"),
        "the fallback must record what it was renamed from"
    );
    assert_ne!(
        server.socket_path(),
        socket.as_path(),
        "the fallback must not reuse the stuck predecessor's socket"
    );
    Client::connect(server.socket_path())
        .with_timeout(Duration::from_secs(2))
        .session_snapshot()
        .await
        .expect("the fallback server serves real requests");
    // The stuck predecessor is deliberately left alone for manual recovery.
    assert!(socket.exists(), "the stuck server keeps its own socket");
}
