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
//!
//! c-ctxusage r9 item 1: the consumer wake is integrated with the queue. The
//! producer now notifies as frames become available — including BEFORE it
//! parks for capacity. Deferring the wake until the whole batch fit deadlocked
//! a residual batch larger than the capacity: the producer blocked for room
//! while the flusher slept in `notified().await`, and no wake could ever arrive.

use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

/// Generic over the frame type so the backpressure behaviour is unit-testable
/// without constructing protocol observations.
pub(super) struct BoundedPending<T> {
    queue: Mutex<VecDeque<T>>,
    /// Producers wait here while the queue is full; the flusher notifies after
    /// every pop.
    space: Condvar,
    /// Consumer wake, notified the moment frames may need delivery: on an
    /// empty→non-empty transition and BEFORE a producer parks for capacity.
    /// Owning it here makes the notify-before-wait ordering impossible for a
    /// caller to forget.
    notify: Arc<Notify>,
    /// Set once delivery can never resume (event channel gone); waiting
    /// producers unblock and discard their remainder.
    closed: AtomicBool,
    /// Producers currently parked waiting for capacity. The flusher treats a
    /// parked producer as pending work: stop + empty queue is not retirement
    /// while a producer still holds frames.
    waiting: AtomicUsize,
    #[cfg(test)]
    /// Test-only rendezvous: a producer signals here the moment it parks for
    /// capacity and again when its push completes, so tests block on
    /// acknowledgements instead of sleep-polling `producer_parked`/`finished`.
    park_barrier: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    #[cfg(test)]
    done_barrier: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    capacity: usize,
}

impl<T> BoundedPending<T> {
    #[must_use]
    pub(super) fn new(capacity: usize, notify: Arc<Notify>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            space: Condvar::new(),
            notify,
            closed: AtomicBool::new(false),
            waiting: AtomicUsize::new(0),
            #[cfg(test)]
            park_barrier: std::sync::Mutex::new(None),
            #[cfg(test)]
            done_barrier: std::sync::Mutex::new(None),
            capacity,
        }
    }

    #[cfg(test)]
    /// Install the test-only "producer parked for capacity" acknowledgement.
    pub(super) fn set_park_barrier(&self, tx: std::sync::mpsc::Sender<()>) {
        *self
            .park_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }

    #[cfg(test)]
    /// Install the test-only "push completed" acknowledgement.
    pub(super) fn set_done_barrier(&self, tx: std::sync::mpsc::Sender<()>) {
        *self
            .done_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }

    #[cfg(test)]
    fn note_parked(&self) {
        if let Some(tx) = self
            .park_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            // Rendezvous: blocks until the test acknowledges the parked state.
            let _ = tx.send(());
        }
    }

    #[cfg(test)]
    fn note_done(&self) {
        if let Some(tx) = self
            .done_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(());
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

    /// Whether a producer is parked waiting for capacity right now. The
    /// flusher must not retire while this is true: the parked producer still
    /// holds the residual frames.
    pub(super) fn producer_parked(&self) -> bool {
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
                // Wake the consumer BEFORE parking: it may be asleep in
                // notified().await, and waiting until this batch fully fit
                // circularly waited on a wake that could only come after the
                // wait returned.
                self.notify.notify_one();
                self.waiting.fetch_add(1, Ordering::SeqCst);
                #[cfg(test)]
                self.note_parked();
                while guard.len() >= self.capacity && !self.closed.load(Ordering::SeqCst) {
                    guard = self
                        .space
                        .wait(guard)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                self.waiting.fetch_sub(1, Ordering::SeqCst);
            }
            if self.closed.load(Ordering::SeqCst) {
                #[cfg(test)]
                self.note_done();
                return;
            }
            let was_empty = guard.is_empty();
            guard.push_back(frame);
            drop(guard);
            if was_empty {
                // The flusher drains to empty before it sleeps; this wake (or
                // the stored permit it leaves) hands it the new batch.
                self.notify.notify_one();
            }
        }
        #[cfg(test)]
        self.note_done();
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
        let was_empty = {
            let mut guard = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_empty = guard.is_empty();
            guard.push_front(frame);
            was_empty
        };
        if was_empty {
            self.notify.notify_one();
        }
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
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn stalled_consumer_caps_the_queue_and_drain_resumes_collection() {
        let notify = Arc::new(Notify::new());
        let queue = Arc::new(BoundedPending::<usize>::new(3, Arc::clone(&notify)));
        // One acknowledgement channel: the producer rendezvous on every park
        // for capacity and once more when the push completes, so the consumer
        // drains strictly on acknowledgements (no idle-gap polling).
        let (event_tx, event_rx) = channel();
        queue.set_park_barrier(event_tx.clone());
        queue.set_done_barrier(event_tx);
        let finished = Arc::new(AtomicBool::new(false));
        let producer_finished = Arc::clone(&finished);
        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(0..6);
            producer_finished.store(true, Ordering::SeqCst);
        });

        let mut delivered = Vec::new();
        while delivered.len() < 6 {
            // Block until the producer says it parked (capacity wanted) or
            // finished; the very first rendezvous proves the bound.
            event_rx.recv().expect("producer barrier closed");
            assert!(queue.producer_parked() || finished.load(Ordering::SeqCst));
            if delivered.is_empty() {
                assert_eq!(queue.len(), 3, "parked at exactly the capacity");
            }
            while let Some(frame) = queue.pop_for_send() {
                delivered.push(frame);
            }
        }
        producer.join().expect("producer thread");
        assert!(finished.load(Ordering::SeqCst));
        assert_eq!(delivered, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn close_releases_a_parked_producer_and_discards_the_remainder() {
        let notify = Arc::new(Notify::new());
        let queue = Arc::new(BoundedPending::<usize>::new(2, notify));
        let (park_tx, park_rx) = channel();
        let (done_tx, done_rx) = channel();
        queue.set_park_barrier(park_tx);
        queue.set_done_barrier(done_tx);
        queue.push_all_blocking(0..2);

        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(2..6);
        });
        park_rx.recv().expect("producer never parked for capacity");
        assert_eq!(queue.len(), 2);

        // Simulates the flusher exiting after the event channel closes.
        queue.close_and_clear();
        // The early-return completion acknowledgement (remainder discarded).
        done_rx.recv().expect("producer never returned after close");
        producer.join().expect("producer thread");
        assert_eq!(queue.len(), 0);
        assert!(queue.is_closed());
    }

    /// c-ctxusage r9 item 1: a consumer that sleeps in `notified().await`
    /// must still receive a residual batch LARGER than the capacity. Before
    /// the fix the producer parked for room without waking the consumer and
    /// the wait could never return.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_batch_wakes_an_idle_consumer_before_parking() {
        let notify = Arc::new(Notify::new());
        let queue = Arc::new(BoundedPending::<usize>::new(4, Arc::clone(&notify)));

        // The consumer starts drained and parks exactly like the production
        // flusher: it only ever wakes on the queue-integrated Notify. The
        // barrier proves it has REACHED the notified().await park (an
        // acknowledgement, replacing the r9 settling sleep).
        let entered = Arc::new(tokio::sync::Barrier::new(2));
        let consumer_entered = Arc::clone(&entered);
        let consumer_queue = Arc::clone(&queue);
        let consumer = tokio::spawn(async move {
            let mut delivered = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            consumer_entered.wait().await;
            while delivered.len() < 50 && std::time::Instant::now() < deadline {
                notify.notified().await;
                while let Some(frame) = consumer_queue.pop_for_send() {
                    delivered.push(frame);
                }
            }
            delivered
        });
        entered.wait().await;

        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(0..50);
        });

        let delivered = tokio::time::timeout(Duration::from_secs(15), consumer)
            .await
            .expect("producer and consumer deadlocked on the bounded queue")
            .expect("consumer task panicked");
        producer.join().expect("producer thread");
        assert_eq!(delivered, (0..50).collect::<Vec<_>>());
        assert_eq!(queue.len(), 0);
    }
}
