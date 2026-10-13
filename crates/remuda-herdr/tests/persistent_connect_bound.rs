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
//!
//! c-ctxusage r17 item 9: the carrier listens with a DELIBERATELY SMALL
//! backlog (re-`listen`ed to 1) so saturation needs a handful of connections
//! instead of thousands of probes. Only the saturation conditions — the
//! immediate `EAGAIN`/`WouldBlock` refusal or a connect parked past the
//! probe bound — end the ramp; ANY other error (fd exhaustion, a vanished
//! socket, …) is reported as a test failure rather than misread as
//! "saturated".

use std::io::ErrorKind;
use std::time::{Duration, Instant};

use remuda_herdr::Client;
use tempfile::tempdir;
use tokio::net::{UnixListener, UnixStream};

/// Re-`listen` the bound socket with a deliberately tiny backlog so the
/// accept queue saturates deterministically with one or two held
/// connections (c-ctxusage r17 item 9). `bind` already called `listen` with
/// the library default; POSIX permits changing the backlog on an existing
/// listening socket. The call goes through nix's safe wrapper (the
/// workspace forbids unsafe in tests).
#[cfg(unix)]
fn listener_with_small_backlog(path: &std::path::Path) -> UnixListener {
    let listener = UnixListener::bind(path).expect("bind");
    nix::sys::socket::listen(
        &listener,
        nix::sys::socket::Backlog::new(1).expect("backlog 1 is valid"),
    )
    .expect("shrinking the listen backlog must succeed");
    listener
}

/// Fill `listener`'s unaccepted-connection queue by holding every completed
/// connection; stop ONLY at the bounded pending-connect conditions:
///
/// * an immediate `EAGAIN`/`WouldBlock` — the nonblocking connect was
///   refused because the queue is full; or
/// * a connect parked past the probe bound — it is queued behind the full
///   backlog and will not complete while nothing is accepted.
///
/// Any OTHER error is reported: opening thousands of fds used to hit
/// `EMFILE`/`ENFILE`, which the old ramp mistook for saturation.
async fn saturate_backlog(path: &std::path::Path) -> Vec<UnixStream> {
    let mut held = Vec::new();
    // The small backlog saturates within a couple of connections; this cap
    // only guards a platform that ignores the re-listen.
    for attempt in 0..64u32 {
        match tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(path)).await {
            Ok(Ok(stream)) => held.push(stream),
            Ok(Err(err)) => {
                // EAGAIN/EWOULDBLOCK and (on some platforms) EINPROGRESS all
                // surface as WouldBlock on a nonblocking unix connect: the
                // queue-full refusal.
                assert_eq!(
                    err.kind(),
                    ErrorKind::WouldBlock,
                    "connect attempt {attempt} failed with a NON-saturation error \
                     (not a small-backlog EAGAIN): {err:?}"
                );
                return held;
            }
            // Parked past the probe bound with the held established streams
            // keeping the queue full: the bounded pending-connect condition.
            Err(_) => return held,
        }
    }
    panic!("the small listen backlog never saturated (64 connects all completed)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_dial_to_a_saturated_carrier_is_bounded_and_retryable() {
    let dir = tempdir().expect("tempdir");
    let socket = dir.path().join("wedged.sock");
    // Bound with a tiny backlog but never accept: every completed connect
    // piles up in the one- or two-slot queue.
    let _listener = listener_with_small_backlog(&socket);

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

/// A connect error that is NOT the bounded saturation condition must be
/// surfaced as a test failure: connecting at a path with no listener yields
/// `NotFound`/`ConnectionRefused`, which the ramp may not swallow as
/// "backlog full". The old ramp returned on `Ok(Err(_))` indiscriminately —
/// an `EMFILE`/`ENFILE` from probing thousands of fds masqueraded as
/// saturation, hiding the real harness failure (c-ctxusage r17 item 9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "NON-saturation error")]
async fn a_non_saturation_connect_error_is_not_taken_as_saturation() {
    let dir = tempdir().expect("tempdir");
    let unbound = dir.path().join("nothing-listening.sock");
    saturate_backlog(&unbound).await;
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
