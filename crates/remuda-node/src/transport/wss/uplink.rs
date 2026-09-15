//! Bounded, in-order pipelining for the Node→Hub journal uplink.
//!
//! The uplink's measured cost is one round trip *per event* when every
//! `journal.append` awaits its ACK before the next frame is written: a batch
//! of six events costs ~6 RTT (2.4 s of a 2.8 s batch on the loaded demo
//! host). TCP allows far more headroom than one frame in flight, and the Hub
//! processes one socket's frames in arrival order and answers in that order.
//!
//! [`UplinkWindow`] keeps up to `window` appends in flight, submitted in
//! journal sequence. Results are consumed back in submission order via
//! [`futures::stream::FuturesOrdered`], so even if the runtime polls a later
//! response first the watermark advances strictly in order. On any error the
//! caller drops the window and replays from the last *acknowledged*
//! watermark — a frame that never got an ACK is simply sent again, and the
//! Hub dedupes by `(instanceId, seq)` and answers "already durable", so the
//! resume/replay contract is unchanged.

use std::future::Future;
use std::pin::Pin;

/// One in-flight append future: owned so it can live in the window.
pub(super) type UplinkFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Fixed-size sliding window of futures whose outputs must be consumed in
/// submission order.
pub(super) struct UplinkWindow<T: Send> {
    inflight: futures::stream::FuturesOrdered<UplinkFuture<T>>,
    window: usize,
}

impl<T: Send> UplinkWindow<T> {
    pub(super) fn new(window: usize) -> Self {
        Self {
            inflight: futures::stream::FuturesOrdered::new(),
            window: window.max(1),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.inflight.is_empty()
    }

    /// Whether another append may be submitted without exceeding the window.
    pub(super) fn has_capacity(&self) -> bool {
        self.inflight.len() < self.window
    }

    /// Submit a future. Callers must check [`Self::has_capacity`] first;
    /// over-subscribing would defeat the bound that keeps memory and Hub-side
    /// queueing in check.
    pub(super) fn push(&mut self, fut: UplinkFuture<T>) {
        debug_assert!(self.has_capacity(), "uplink window over-subscribed");
        self.inflight.push_back(fut);
    }

    /// The next in-order result (resolves only after every earlier future).
    ///
    /// Borrows the window for one poll iteration; `select!` does not retain
    /// the future across iterations, so the returned future need not be
    /// `'static`.
    pub(super) fn next(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        use futures::StreamExt;
        self.inflight.next()
    }
}
