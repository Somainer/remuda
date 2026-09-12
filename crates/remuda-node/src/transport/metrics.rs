//! Counters for bounded journal queues and reconnects.

use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Shared atomic counters for one outbound WSS session.
#[derive(Debug, Clone, Default)]
pub struct TransportMetrics {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    journal_enqueued: AtomicU64,
    journal_acked: AtomicU64,
    journal_backpressure_waits: AtomicU64,
    journal_duplicate_acks: AtomicU64,
    reconnects: AtomicU64,
    hello_rejected: AtomicU64,
    clock_skew_events: AtomicU64,
    pending_dropped: AtomicU64,
}

/// Point-in-time copy of [`TransportMetrics`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportMetricsSnapshot {
    /// Jobs accepted onto the bounded `journal.append` queue.
    pub journal_enqueued: u64,
    /// Hub acks (including already-durable duplicates).
    pub journal_acked: u64,
    /// Times a sender waited because the queue was full.
    pub journal_backpressure_waits: u64,
    /// Hub `journal gap` treated as already durable (dedupe).
    pub journal_duplicate_acks: u64,
    /// Reconnect attempts (commands are never replayed).
    pub reconnects: u64,
    /// `node.hello` RPC errors.
    pub hello_rejected: u64,
    /// Hello `serverTime` differed from local clock by more than one minute.
    pub clock_skew_events: u64,
    /// In-flight journal waiters dropped on disconnect.
    pub pending_dropped: u64,
}

impl TransportMetrics {
    /// Zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy current values.
    #[must_use]
    pub fn snapshot(&self) -> TransportMetricsSnapshot {
        TransportMetricsSnapshot {
            journal_enqueued: self.inner.journal_enqueued.load(Ordering::Relaxed),
            journal_acked: self.inner.journal_acked.load(Ordering::Relaxed),
            journal_backpressure_waits: self
                .inner
                .journal_backpressure_waits
                .load(Ordering::Relaxed),
            journal_duplicate_acks: self.inner.journal_duplicate_acks.load(Ordering::Relaxed),
            reconnects: self.inner.reconnects.load(Ordering::Relaxed),
            hello_rejected: self.inner.hello_rejected.load(Ordering::Relaxed),
            clock_skew_events: self.inner.clock_skew_events.load(Ordering::Relaxed),
            pending_dropped: self.inner.pending_dropped.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn enqueue(&self, waiting: bool) {
        self.inner.journal_enqueued.fetch_add(1, Ordering::Relaxed);
        if waiting {
            self.inner
                .journal_backpressure_waits
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn acked(&self) {
        self.inner.journal_acked.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn duplicate_ack(&self) {
        self.inner
            .journal_duplicate_acks
            .fetch_add(1, Ordering::Relaxed);
        self.inner.journal_acked.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reconnect(&self) {
        self.inner.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn hello_rejected(&self) {
        self.inner.hello_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn clock_skew(&self) {
        self.inner.clock_skew_events.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn drop_pending(&self, n: u64) {
        if n > 0 {
            self.inner.pending_dropped.fetch_add(n, Ordering::Relaxed);
        }
    }
}
