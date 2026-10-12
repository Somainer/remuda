//! c-ctxusage r16 item 2: establishing a [`PersistentClient`] connection is
//! BOUNDED and retryable. A carrier whose listen backlog is saturated (it
//! accepts no connections at all) must not park the dial forever and wedge
//! the close phase holding it.
//!
//! On Linux a nonblocking `connect(2)` to a unix socket with a full accept
//! queue returns `EAGAIN` immediately; on other platforms it parks and is
//! completed later. Both outcomes are bounded: the immediate refusal and the
//! timeout map to a transient [`remuda_herdr::Error::Unreachable`] — no
//! request byte has been sent, so this is never an uncertain post-dispatch
//! outcome.

use std::time::{Duration, Instant};

use remuda_herdr::Client;
use tempfile::tempdir;
use tokio::net::{UnixListener, UnixStream};

/// Fill `listener`'s unaccepted-connection queue by holding every completed
/// connection; stop at the first connect that does not complete within a
/// short bound (the queue is full and the carrier is effectively wedged).
/// All held streams stay open for the test's lifetime.
async fn saturate_backlog(path: &std::path::Path) -> Vec<UnixStream> {
    let mut held = Vec::new();
    // listen() backlog is 1024 (capped by somaxconn); this is far beyond any
    // real queue size.
    for _ in 0..12_000 {
        match tokio::time::timeout(Duration::from_millis(60), UnixStream::connect(path)).await {
            Ok(Ok(stream)) => held.push(stream),
            // Saturated: EAGAIN before a connection slot is available.
            Ok(Err(_)) => return held,
            // Saturated: the connect parked past the probe bound. All held
            // established streams keep the queue full, so the dial under test
            // parks too and its own bound must fire.
            Err(_) => return held,
        }
    }
    panic!("listen backlog never saturated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_dial_to_a_saturated_carrier_is_bounded_and_retryable() {
    let dir = tempdir().expect("tempdir");
    let socket = dir.path().join("wedged.sock");
    // Bound but never accept: every completed connect piles up in the queue.
    let _listener = UnixListener::bind(&socket).expect("bind");

    let _held = saturate_backlog(&socket).await;

    let client = Client::connect(&socket).with_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let err = match client.persistent().await {
        Ok(_) => panic!("the bounded dial must fail against a saturated carrier"),
        Err(err) => err,
    };
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "connection establishment was not bounded: waited {elapsed:?}"
    );
    assert!(
        matches!(err, remuda_herdr::Error::Unreachable { .. }),
        "a pre-write dial failure (immediate EAGAIN or bounded timeout) must \
         be a transient, safely-retryable Unreachable, never uncertain; got \
         {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_dial_to_an_accepting_carrier_opens() {
    let dir = tempdir().expect("tempdir");
    let socket = dir.path().join("healthy.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    // Drain every connection, then drop the accepted streams immediately.
    tokio::spawn(async move {
        loop {
            if listener.accept().await.is_err() {
                break;
            }
        }
    });

    let client = Client::connect(&socket).with_timeout(Duration::from_secs(2));
    let started = Instant::now();
    if client.persistent().await.is_err() {
        panic!("an accepting carrier must open a persistent connection");
    }
    assert!(started.elapsed() < Duration::from_secs(2));
}
