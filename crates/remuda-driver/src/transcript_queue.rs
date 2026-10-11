//! Bounded queue between the blocking transcript producers (map/finish
//! closures on `spawn_blocking`, and abort-guard threads) and the async
//! streaming flusher task.
//!
//! c-ctxusage r8 item 5: the producer → flusher queue used to be an unbounded
//! `VecDeque`; a stalled consumer let a fast replay grow it without limit. The
//! queue now has a fixed capacity and producers block ON THEIR BLOCKING
//! THREAD for room, so memory is capped while a stalled consumer applies
//! natural backpressure to collection. Blocking is cancellation-safe: when the
//! event channel is gone (the flusher exits) [`BoundedPending::close_and_clear`]
//! releases every waiting producer, whose frames are dropped because nothing
//! can deliver them.

use std::collections::VecDeque;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Generic over the frame type so the backpressure behaviour is unit-testable
/// without constructing protocol observations.
pub(super) struct BoundedPending<T> {
    queue: Mutex<VecDeque<T>>,
    /// Producers wait here while the queue is full; the flusher notifies after
    /// every pop.
    space: Condvar,
    /// Set once delivery can never resume (event channel gone); waiting
    /// producers unblock and discard their remainder.
    closed: AtomicBool,
    /// Producers currently parked waiting for capacity. A precise barrier for
    /// tests and an observable backpressure diagnostic.
    waiting: AtomicUsize,
    capacity: usize,
}

impl<T> BoundedPending<T> {
    #[must_use]
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            space: Condvar::new(),
            closed: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
            capacity,
        }
    }

    /// Frames currently queued.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether no frames are queued.
    pub(super) fn is_empty(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Whether a producer is parked waiting for capacity right now.
    #[cfg(test)]
    pub(super) fn producer_waiting(&self) -> bool {
        self.waiting.load(Ordering::SeqCst) > 0
    }

    /// Whether delivery has been permanently shut down.
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Queue frames from a BLOCKING thread, waiting for capacity as needed.
    /// Returns after every frame is queued, or early (remainder dropped) once
    /// the queue closes because the event channel went away.
    pub(super) fn push_all_blocking<I>(&self, frames: I)
    where
        I: IntoIterator<Item = T>,
    {
        for frame in frames {
            let mut guard = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.len() >= self.capacity {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                while guard.len() >= self.capacity && !self.closed.load(Ordering::SeqCst) {
                    guard = self
                        .space
                        .wait(guard)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                self.waiting.fetch_sub(1, Ordering::SeqCst);
            }
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            guard.push_back(frame);
        }
    }

    /// Pop the frame at the front for delivery. The flusher pops BEFORE it
    /// awaits send (c-ctxusage r8 item 4) and requeues with
    /// [`Self::push_front_requeue`] on failure.
    pub(super) fn pop_for_send(&self) -> Option<T> {
        let mut guard = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let frame = guard.pop_front()?;
        // One slot freed: wake exactly one parked producer.
        self.space.notify_one();
        Some(frame)
    }

    /// Put an unsendable frame back at the front (channel failed mid-send).
    /// Does not wait for capacity: the frame previously occupied a slot that
    /// its pop freed.
    pub(super) fn push_front_requeue(&self, frame: T) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_front(frame);
    }

    /// Discard every queued frame (used after the event channel closes) and
    /// release waiting producers.
    pub(super) fn close_and_clear(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.space.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Block until the producer parks and its park is observed, bounded so a
    /// regression fails instead of hanging the test binary.
    fn wait_for_waiting(queue: &BoundedPending<usize>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !queue.producer_waiting() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            queue.producer_waiting(),
            "producer never parked for capacity"
        );
        // Let the observed parked state settle (the waiting counter is set
        // just before the predicate re-check).
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn stalled_consumer_caps_the_queue_and_drain_resumes_collection() {
        let queue = Arc::new(BoundedPending::<usize>::new(3));
        let producer_queue = Arc::clone(&queue);
        let finished = Arc::new(AtomicBool::new(false));
        let producer_finished = Arc::clone(&finished);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(0..6);
            producer_finished.store(true, Ordering::SeqCst);
        });

        // The producer must block at exactly the bound with a stalled
        // consumer, having parked frame 3.
        wait_for_waiting(&queue);
        assert_eq!(queue.len(), 3);
        assert!(!finished.load(Ordering::SeqCst));

        // Drain as the producer refills; it can park temporarily after each
        // pop, so a transiently-empty queue is not the end — producer
        // completion is. Every frame arrives once and in order.
        let mut delivered = Vec::new();
        while !finished.load(Ordering::SeqCst) {
            while let Some(frame) = queue.pop_for_send() {
                delivered.push(frame);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        while let Some(frame) = queue.pop_for_send() {
            delivered.push(frame);
        }
        producer.join().expect("producer thread");
        assert!(finished.load(Ordering::SeqCst));
        assert_eq!(delivered, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn close_releases_a_parked_producer_and_discards_the_remainder() {
        let queue = Arc::new(BoundedPending::<usize>::new(2));
        queue.push_all_blocking(0..2);

        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(2..6);
        });
        wait_for_waiting(&queue);
        assert_eq!(queue.len(), 2);

        // Simulates the flusher exiting after the event channel closes.
        queue.close_and_clear();
        producer.join().expect("producer thread");
        assert_eq!(queue.len(), 0);
        assert!(queue.is_closed());
    }
}
