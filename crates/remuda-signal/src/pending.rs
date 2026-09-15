//! The blocking half of tier A: a hook waits here until a human answers
//! (D-028 §4.4).
//!
//! [`PendingDecisions`] is the rendezvous between two things that do not know
//! about each other: a `PermissionRequest` hook parked on the instance socket,
//! and a device somewhere answering an Interaction through the broker. The
//! hook registers a waiter keyed by [`DecisionKey`], the answer arrives later
//! by that key, and the reply is handed back to the still-connected hook.
//!
//! What this type is really for is the honesty rule in §4.4 and §14 risk 1:
//! **a decision is only "applied" if the agent actually took it.** So the
//! resolver reports an [`Outcome`], not a `bool`:
//!
//! - [`Outcome::Answered`] — a human decided and the hook was still there to
//!   receive it. This is the only outcome that may be reported as applied.
//! - [`Outcome::TimedOut`] — nobody decided in time. The hook is told to deny
//!   (fail closed), and the caller knows no human intent was lost.
//! - [`Outcome::Abandoned`] — the hook went away before the answer did (the
//!   agent gave up, the turn was interrupted, the process died). The human's
//!   decision was real but *nothing received it*, so the caller must not claim
//!   it landed — this is the case the screen-key fallback exists for.
//!
//! Collapsing those three into "did it work" is what produces the failure the
//! design doc names: the UI says approved, the agent never heard, and the
//! session sits on a dialog nobody is watching.

use crate::decision::HookDecision;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Identity a waiting hook is filed under.
///
/// The hook payload has no request id of its own — claude's
/// `PermissionRequest` carries no `tool_use_id` (measured; see
/// [`crate::decision`]) — so the key is minted by the Node when it opens the
/// interaction and travels with the Interaction entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DecisionKey(String);

impl DecisionKey {
    /// Wrap an already-minted key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The key as it travels on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DecisionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What became of one blocking request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A human answered and the waiting hook received it.
    Answered,
    /// The wait expired with no answer; the hook was told to deny.
    TimedOut,
    /// An answer arrived but no hook was waiting for it any more.
    ///
    /// The caller must **not** report this as applied (§14 risk 1).
    Abandoned,
}

/// Why a waiter was dropped without a human decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireReason {
    /// The wait ran past its deadline.
    Deadline,
    /// The instance is going away.
    Shutdown,
}

/// The rendezvous table. Cheap to clone; one per instance.
#[derive(Clone, Default)]
pub struct PendingDecisions {
    inner: Arc<Mutex<HashMap<DecisionKey, oneshot::Sender<HookDecision>>>>,
}

impl PendingDecisions {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a hook under `key` and wait up to `timeout` for a decision.
    ///
    /// Returns the decision to print and what actually happened. A timeout
    /// yields [`HookDecision::timed_out`] — a deny — because §4.4 requires an
    /// unanswered approval to fail closed rather than becoming an allow or
    /// hanging the agent indefinitely.
    ///
    /// Registering a second waiter under a live key displaces the first, and
    /// the first is told to deny: two hooks cannot both be the answer to one
    /// interaction, and leaving the loser parked would hang it until its own
    /// deadline.
    pub async fn wait(
        &self,
        key: DecisionKey,
        timeout: std::time::Duration,
    ) -> (HookDecision, Outcome) {
        let rx = self.park(key.clone());
        self.await_parked(key, rx, timeout).await
    }

    /// Register the waiter for `key` and return the receiver its decision
    /// arrives on. Pair with [`await_parked`](Self::await_parked).
    ///
    /// Split from [`wait`](Self::wait) so a caller can publish the interaction
    /// card *after* the waiter exists: an answer the instant the card appears
    /// must find a registered hook, never be dropped as `Abandoned` and then
    /// time out as a deny. A displaced previous waiter is dropped (denied),
    /// exactly as in [`wait`](Self::wait).
    pub fn park(&self, key: DecisionKey) -> oneshot::Receiver<HookDecision> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut table) = self.inner.lock() {
            // Dropped, not sent on: a displaced waiter was not decided by
            // anyone, and sending it a deny would make it report `Answered`.
            drop(table.insert(key, tx));
        }
        rx
    }

    /// Wait on a receiver handed out by [`park`](Self::park), enforcing the
    /// same fail-closed timeout and table cleanup as [`wait`](Self::wait).
    pub async fn await_parked(
        &self,
        key: DecisionKey,
        rx: oneshot::Receiver<HookDecision>,
        timeout: std::time::Duration,
    ) -> (HookDecision, Outcome) {
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(decision)) => (decision, Outcome::Answered),
            // The sender was dropped: the table was cleared out from under us
            // (retire / shutdown). Same honest answer as a deadline.
            Ok(Err(_)) => (HookDecision::timed_out(), Outcome::TimedOut),
            Err(_) => {
                self.forget(&key);
                (HookDecision::timed_out(), Outcome::TimedOut)
            }
        }
    }

    /// Hand a decision to the hook waiting under `key`.
    ///
    /// [`Outcome::Abandoned`] means the decision was real but arrived after
    /// the hook stopped listening — the caller owes the user either a
    /// fallback or an honest "not applied", never a success.
    #[must_use]
    pub fn resolve(&self, key: &DecisionKey, decision: HookDecision) -> Outcome {
        let Some(waiter) = self.inner.lock().ok().and_then(|mut t| t.remove(key)) else {
            return Outcome::Abandoned;
        };
        // A closed receiver means the hook's own wait elapsed between our
        // lookup and this send: still nobody heard it.
        match waiter.send(decision) {
            Ok(()) => Outcome::Answered,
            Err(_) => Outcome::Abandoned,
        }
    }

    /// True while a hook is parked under `key`.
    #[must_use]
    pub fn is_waiting(&self, key: &DecisionKey) -> bool {
        self.inner.lock().is_ok_and(|table| table.contains_key(key))
    }

    /// Number of parked hooks; for diagnostics and tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |table| table.len())
    }

    /// True when nothing is parked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Release one waiter with a deny, without a human decision.
    ///
    /// The sender is **dropped** rather than sent on. Sending a deny through
    /// the channel would arrive at [`wait`](Self::wait) as an ordinary
    /// decision and be reported [`Outcome::Answered`] — claiming a human
    /// denied something nobody looked at. Dropping it lands on the
    /// `Ok(Err(_))` arm, which is the same honest "nobody decided" as a
    /// deadline, and the waiter still gets [`HookDecision::timed_out`] to
    /// print.
    pub fn retire(&self, key: &DecisionKey, _reason: RetireReason) {
        drop(self.inner.lock().ok().and_then(|mut t| t.remove(key)));
    }

    /// Release every waiter with a deny. Used when the instance stops.
    ///
    /// Without this a stopping instance leaves each parked hook to sit until
    /// its own deadline, holding the agent's turn open for minutes after the
    /// session is gone. Drops the senders for the reason
    /// [`retire`](Self::retire) does.
    pub fn retire_all(&self, _reason: RetireReason) {
        let waiters: Vec<oneshot::Sender<HookDecision>> = self
            .inner
            .lock()
            .map(|mut table| table.drain().map(|(_, tx)| tx).collect())
            .unwrap_or_default();
        drop(waiters);
    }

    fn forget(&self, key: &DecisionKey) {
        if let Ok(mut table) = self.inner.lock() {
            table.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::decision_behavior;

    const WAIT: std::time::Duration = std::time::Duration::from_secs(30);

    fn allow() -> HookDecision {
        HookDecision::Allow {
            updated_input: None,
            updated_permissions: Vec::new(),
        }
    }

    fn behavior(decision: &HookDecision) -> Option<String> {
        decision_behavior(&decision.to_hook_json("PermissionRequest")).map(ToOwned::to_owned)
    }

    #[tokio::test]
    async fn a_human_answer_reaches_the_waiting_hook() {
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-1");
        let waiter = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        // Let the waiter register before answering.
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        assert_eq!(pending.resolve(&key, allow()), Outcome::Answered);
        let (decision, outcome) = waiter.await.unwrap();
        assert_eq!(outcome, Outcome::Answered);
        assert_eq!(behavior(&decision).as_deref(), Some("allow"));
    }

    #[tokio::test]
    async fn a_deny_reaches_the_hook_with_its_message() {
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-deny");
        let waiter = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            pending.resolve(
                &key,
                HookDecision::Deny {
                    message: "not this time".into(),
                },
            ),
            Outcome::Answered
        );
        let (decision, outcome) = waiter.await.unwrap();
        assert_eq!(outcome, Outcome::Answered);
        let HookDecision::Deny { message } = decision else {
            panic!("expected a deny");
        };
        assert_eq!(message, "not this time");
    }

    #[tokio::test]
    async fn an_unanswered_wait_denies_rather_than_allowing() {
        // §4.4: fail closed. An allow-by-default would run tools nobody saw.
        tokio::time::pause();
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-slow");
        let waiter = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(WAIT + std::time::Duration::from_secs(1)).await;
        let (decision, outcome) = waiter.await.unwrap();
        assert_eq!(outcome, Outcome::TimedOut);
        assert_eq!(behavior(&decision).as_deref(), Some("deny"));
    }

    #[tokio::test]
    async fn a_timed_out_wait_leaves_no_entry_behind() {
        // Otherwise every abandoned approval leaks a row for the life of the
        // instance, and a later answer would resolve a hook that is long gone.
        tokio::time::pause();
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-leak");
        let waiter = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(WAIT + std::time::Duration::from_secs(1)).await;
        let _ = waiter.await.unwrap();
        assert!(pending.is_empty(), "a timed-out waiter must be removed");
    }

    #[test]
    fn answering_nobody_is_abandoned_not_success() {
        // The decision was real; nothing received it. Reporting this as
        // applied is exactly the "approved but nothing moved" failure.
        let pending = PendingDecisions::new();
        assert_eq!(
            pending.resolve(&DecisionKey::new("absent"), allow()),
            Outcome::Abandoned
        );
    }

    #[tokio::test]
    async fn an_answer_after_the_hook_gave_up_is_abandoned() {
        tokio::time::pause();
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-late");
        let waiter = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(WAIT + std::time::Duration::from_secs(1)).await;
        let (_, outcome) = waiter.await.unwrap();
        assert_eq!(outcome, Outcome::TimedOut);
        // The human pressed Approve just too late.
        assert_eq!(pending.resolve(&key, allow()), Outcome::Abandoned);
    }

    #[tokio::test]
    async fn retiring_a_waiter_releases_it_with_a_deny() {
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-retire");
        let waiter = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        pending.retire(&key, RetireReason::Deadline);
        let (decision, outcome) = waiter.await.unwrap();
        assert_eq!(behavior(&decision).as_deref(), Some("deny"));
        assert_eq!(outcome, Outcome::TimedOut, "no human decided this");
    }

    #[tokio::test]
    async fn a_stopping_instance_releases_every_parked_hook() {
        // Otherwise each one holds its agent's turn open until its own
        // deadline, long after the session is gone.
        let pending = PendingDecisions::new();
        let mut waiters = Vec::new();
        for index in 0..3 {
            let key = DecisionKey::new(format!("int-{index}"));
            let pending = pending.clone();
            waiters.push(tokio::spawn(async move { pending.wait(key, WAIT).await }));
        }
        while pending.len() < 3 {
            tokio::task::yield_now().await;
        }
        pending.retire_all(RetireReason::Shutdown);
        for waiter in waiters {
            let (decision, outcome) = waiter.await.unwrap();
            assert_eq!(behavior(&decision).as_deref(), Some("deny"));
            assert_eq!(
                outcome,
                Outcome::TimedOut,
                "shutdown is not a human decision"
            );
        }
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn two_hooks_on_one_key_do_not_both_hang() {
        // One interaction has one answer. The displaced waiter is released
        // with a deny rather than parked until its own deadline.
        let pending = PendingDecisions::new();
        let key = DecisionKey::new("int-dup");
        let first = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        let second = {
            let pending = pending.clone();
            let key = key.clone();
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        let (decision, _) = first.await.unwrap();
        assert_eq!(behavior(&decision).as_deref(), Some("deny"));
        while !pending.is_waiting(&key) {
            tokio::task::yield_now().await;
        }
        assert_eq!(pending.resolve(&key, allow()), Outcome::Answered);
        let (decision, outcome) = second.await.unwrap();
        assert_eq!(outcome, Outcome::Answered);
        assert_eq!(behavior(&decision).as_deref(), Some("allow"));
    }

    #[tokio::test]
    async fn two_interactions_wait_independently() {
        let pending = PendingDecisions::new();
        let (a, b) = (DecisionKey::new("a"), DecisionKey::new("b"));
        let wait_a = {
            let (pending, key) = (pending.clone(), a.clone());
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        let wait_b = {
            let (pending, key) = (pending.clone(), b.clone());
            tokio::spawn(async move { pending.wait(key, WAIT).await })
        };
        while pending.len() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(pending.resolve(&b, allow()), Outcome::Answered);
        assert_eq!(
            pending.resolve(
                &a,
                HookDecision::Deny {
                    message: "no".into(),
                },
            ),
            Outcome::Answered
        );
        assert_eq!(behavior(&wait_a.await.unwrap().0).as_deref(), Some("deny"));
        assert_eq!(behavior(&wait_b.await.unwrap().0).as_deref(), Some("allow"));
    }
}
