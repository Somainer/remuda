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
    /// Test-only rendezvous carrying a TYPED producer event: [`ProducerEvent::Parked`]
    /// the moment a producer parks for capacity, [`ProducerEvent::Done`] as the
    /// producer's final action once every frame is queued (or discarded after a
    /// close). Receiving `Done` IS completion - a consumer never has to infer
    /// it from a flag a caller stores only after the push returns
    /// (c-ctxusage r11 item 4).
    event_barrier: std::sync::Mutex<Option<std::sync::mpsc::Sender<ProducerEvent>>>,
    capacity: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProducerEvent {
    /// A producer parked waiting for capacity; it still holds frames.
    Parked,
    /// The producer finished: every frame was queued, or the remainder was
    /// discarded after [`BoundedPending::close_and_clear`]. Fired as the last
    /// action of [`BoundedPending::push_all_blocking`].
    Done,
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
            event_barrier: std::sync::Mutex::new(None),
            capacity,
        }
    }

    #[cfg(test)]
    /// Install the test-only typed producer-event acknowledgement channel.
    pub(super) fn set_event_barrier(&self, tx: std::sync::mpsc::Sender<ProducerEvent>) {
        *self
            .event_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
    }

    #[cfg(test)]
    fn note_event(&self, event: ProducerEvent) {
        if let Some(tx) = self
            .event_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(event);
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
                self.note_event(ProducerEvent::Parked);
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
                self.note_event(ProducerEvent::Done);
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
        self.note_event(ProducerEvent::Done);
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

    /// Every test wait is bounded: a producer/consumer deadlock must fail the
    /// test loudly and promptly, never hang the binary (c-ctxusage r11 item 4).
    const TEST_WAIT: Duration = Duration::from_secs(30);

    fn recv_event(rx: &std::sync::mpsc::Receiver<ProducerEvent>, what: &str) -> ProducerEvent {
        rx.recv_timeout(TEST_WAIT)
            .unwrap_or_else(|err| panic!("{what} within {TEST_WAIT:?}: {err:?}"))
    }

    fn join_bounded<T: Send + 'static>(handle: std::thread::JoinHandle<T>, what: &str) -> T {
        let (tx, rx) = channel();
        // The handle is moved onto a watcher so a join that would block
        // forever is observed through the bounded channel instead.
        std::thread::spawn(move || {
            let _ = tx.send(handle.join());
        });
        match rx.recv_timeout(TEST_WAIT) {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(err) => panic!("{what} within {TEST_WAIT:?}: {err:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_consumer_caps_the_queue_and_drain_resumes_collection() {
        let notify = Arc::new(Notify::new());
        let queue = Arc::new(BoundedPending::<usize>::new(3, Arc::clone(&notify)));
        // One TYPED acknowledgement channel: Parked when the producer wants
        // capacity, Done as its final action. Receiving Done IS completion -
        // no separate finished flag a caller could store only afterwards.
        let (event_tx, event_rx) = channel();
        queue.set_event_barrier(event_tx);

        // Start ONLY the producer: with no consumer it fills the queue and
        // parks on the capacity wait, making the first Parked a live snapshot.
        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(0..6);
        });

        // The FIRST Parked is the only event asserted against the producer's
        // CURRENT state: it is sent while the producer holds the queue lock at
        // capacity, so waiting == 1 and len == capacity when it is received.
        // No consumer drains before this snapshot.
        assert_eq!(
            recv_event(&event_rx, "producer never parked or finished"),
            ProducerEvent::Parked
        );
        assert!(
            queue.producer_parked(),
            "the first Parked means the producer holds frames at capacity"
        );
        assert_eq!(queue.len(), 3, "parked at exactly the capacity");

        // Only now start a real flusher (woken solely by the queue-integrated
        // Notify); the producer runs to completion. Later Parked events may
        // already be BUFFERED in the channel by then - they are HISTORY: a
        // Parked only proves the producer parked at some point in the past,
        // never that it is parked now. Joining before reading them makes that
        // buffering deterministic instead of a load-dependent race (the old
        // test asserted producer_parked()/len() on every buffered Parked and
        // could fail on correct behaviour, c-ctxusage r12 item 4).
        let drainer_queue = Arc::clone(&queue);
        let drainer_notify = Arc::clone(&notify);
        let drainer = tokio::spawn(async move {
            let mut delivered = Vec::new();
            while delivered.len() < 6 {
                drainer_notify.notified().await;
                while let Some(frame) = drainer_queue.pop_for_send() {
                    delivered.push(frame);
                }
            }
            delivered
        });
        join_bounded(producer, "producer thread never returned");
        let mut parks = 1u32;
        loop {
            match recv_event(&event_rx, "producer event stream ended without Done") {
                ProducerEvent::Parked => parks += 1,
                ProducerEvent::Done => break,
            }
        }
        let delivered = tokio::time::timeout(Duration::from_secs(10), drainer)
            .await
            .expect("drainer task never finished")
            .expect("drainer panicked");
        assert!(parks >= 1, "the producer never exercised the capacity park");
        assert_eq!(delivered, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn close_releases_a_parked_producer_and_discards_the_remainder() {
        let notify = Arc::new(Notify::new());
        let queue = Arc::new(BoundedPending::<usize>::new(2, notify));
        // Prefill FIRST, with no hook installed: its completion emits nothing,
        // so the Done awaited after close_and_clear can only be the parked
        // producer's own completion (c-ctxusage r11 item 4).
        queue.push_all_blocking(0..2);

        let (event_tx, event_rx) = channel();
        queue.set_event_barrier(event_tx);
        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(2..6);
        });
        assert_eq!(
            recv_event(&event_rx, "producer never parked for capacity"),
            ProducerEvent::Parked
        );
        assert_eq!(queue.len(), 2);

        // Simulates the flusher exiting after the event channel closes.
        queue.close_and_clear();
        // The parked producer's own completion acknowledgement.
        assert_eq!(
            recv_event(&event_rx, "producer never returned after close"),
            ProducerEvent::Done
        );
        join_bounded(producer, "producer thread never returned");
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
        // Bounded rendezvous: a consumer that never reaches the
        // notified().await park must fail here instead of hanging the binary
        // (c-ctxusage r12 item 4).
        tokio::time::timeout(Duration::from_secs(10), entered.wait())
            .await
            .expect("consumer never reached its notified().await park");

        let producer_queue = Arc::clone(&queue);
        let producer = std::thread::spawn(move || {
            producer_queue.push_all_blocking(0..50);
        });

        let delivered = tokio::time::timeout(Duration::from_secs(15), consumer)
            .await
            .expect("producer and consumer deadlocked on the bounded queue")
            .expect("consumer task panicked");
        join_bounded(producer, "producer thread never returned");
        assert_eq!(delivered, (0..50).collect::<Vec<_>>());
        assert_eq!(queue.len(), 0);
    }
}
