//! Shared tail-only flush policy for every carrier that mirrors journals.
//!
//! All four carriers (`transport::wss`, `stdio`, `daemon`, `runtime_link`)
//! answer the same question on a timer: which observations does the Hub not
//! have yet? They used to answer it by asking the store for everything from
//! the watermark to the tail and dropping all but the first page, which makes
//! one sweep cost O(journal bytes) instead of O(new bytes) — a 20 MB journal
//! paid 20 MB of `serde_json` per 256-event page, and a stuck append re-parsed
//! that tail four times a second.
//!
//! Three things live here so the carriers cannot drift apart again:
//!
//! * [`FlushCursor`] — the last forwarded sequence per Instance, held in the
//!   process for the life of the session. It is an **optimisation only**
//!   (D-019/D-020): the Node journal stays the authority and Hub indexes stay
//!   rebuildable, so a lost, stale, or unknown cursor falls back to a full
//!   replay from the floor, never to a skipped event.
//! * [`flush_plan`] — the "nothing can exist" decision. When the cursor already
//!   covers the journal's durable seq the tick returns without a read at all.
//!   Deliberately lifecycle-blind: a terminal Instance can still be journaled
//!   to, which is why the check is the cursor rather than the lifecycle.
//! * [`FlushBackoff`] — the retry ladder that replaces a flat 250 ms re-entry,
//!   so a failing Instance cannot re-parse its tail four times a second.

use crate::{DevNode, NodeError};
use remuda_protocol::{InstanceId, U64};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[cfg(test)]
use remuda_protocol::InstanceLifecycle;

/// First retry delay after a failed flush.
pub const RETRY_MIN: Duration = Duration::from_millis(250);
/// Ceiling for the retry ladder. A failing Instance re-reads its tail at most
/// this often, which bounds its cost to a fraction of the old 4 Hz.
pub const RETRY_MAX: Duration = Duration::from_secs(5);

/// Last sequence each carrier has successfully forwarded, per Instance.
///
/// Rebuilt on demand and cleared on a Hub hello that carries watermarks
/// (`apply_resume_watermarks`), so a reconnect re-derives it rather than
/// trusting a value the Hub may have regressed past.
#[derive(Debug, Default)]
pub struct FlushCursor {
    last: HashMap<String, u64>,
}

impl FlushCursor {
    /// Empty cursor; the next flush replays from the floor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Last forwarded seq, `None` when this Instance has never been flushed.
    #[must_use]
    pub fn last(&self, instance_id: &InstanceId) -> Option<u64> {
        self.last.get(instance_id.as_id().as_str()).copied()
    }

    /// Record a forwarded seq, never moving backwards.
    pub fn record(&mut self, instance_id: &InstanceId, seq: u64) {
        let slot = self
            .last
            .entry(instance_id.as_id().as_str().to_owned())
            .or_insert(0);
        if *slot < seq {
            *slot = seq;
        }
    }

    /// Promote a Hub-reported watermark (a resume cursor or an ACK).
    #[cfg(test)]
    pub fn observe(&mut self, instance_id: &InstanceId, seq: i64) {
        if seq > 0 {
            self.record(instance_id, seq as u64);
        }
    }

    /// Set the read position to exactly `seq`, press or pull.
    ///
    /// Used when the Hub reports where it thinks it is: if it has fallen
    /// behind this session's position, the cursor must come *back* so the
    /// events it lost are replayed rather than skipped.
    pub fn set(&mut self, instance_id: &InstanceId, seq: u64) {
        self.last
            .insert(instance_id.as_id().as_str().to_owned(), seq);
    }

    /// Drop every cursor, forcing a replay from the floor.
    pub fn clear(&mut self) {
        self.last.clear();
    }

    /// Forget one Instance (purge, or a journal replaced underneath us).
    #[cfg(test)]
    pub fn forget(&mut self, instance_id: &InstanceId) {
        self.last.remove(instance_id.as_id().as_str());
    }

    /// Whether this Instance has a cursor at all.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.last.is_empty()
    }
}

/// What one flush tick should do for one Instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushPlan {
    /// Possibly new events; read one bounded page starting at this seq
    /// (inclusive).
    Read(u64),
    /// The cursor already covers the journal. No read, no pump work, no bytes.
    CaughtUp,
}

/// Decide whether an Instance can have anything left to forward.
///
/// `last` is the last seq already confirmed to the Hub; `None` (or 0) means
/// "replay from the floor". `CaughtUp` requires *positive* proof that the
/// journal is at its tail: the cursor already covers the journal's durable seq.
///
/// The lifecycle is deliberately not consulted. "Terminal" does not imply "no
/// further events": `reclaim::reconcile_native_pty` marks an Instance failed
/// and only *then* journals the diagnostic explaining why, so a lifecycle-keyed
/// skip would strand the one event a reader needs to explain the exit — and the
/// journal being flushed in the observed demo was exactly that case, an
/// `exited` Instance sitting at 7122 events. The only safe test is the cursor
/// against the journal's own watermark.
///
/// A gap between `last` and `durable` always reads, so a cursor that is stale
/// or ahead of a journal that was replaced replays rather than losing events
/// (D-019/D-020: the journal is the authority, cursors are an optimisation).
pub fn flush_plan(node: &DevNode, instance_id: &InstanceId, last: Option<u64>) -> FlushPlan {
    let durable = match node.current_journal_durable_seq(instance_id) {
        Ok(durable) => durable.0,
        // Unknown durability is not proof of anything; read from the floor.
        Err(_) => return FlushPlan::Read(1),
    };
    let last = last.unwrap_or(0);
    if last >= durable {
        return FlushPlan::CaughtUp;
    }
    FlushPlan::Read(last + 1)
}

/// Retry ladder for a flush that failed and must be re-entered on a timer.
///
/// One Instance backs off on its own; there is no shared clock, so a healthy
/// Instance's flush is never delayed by a stuck neighbour.
#[derive(Debug)]
pub struct FlushBackoff {
    next: Instant,
    delay: Duration,
}

impl FlushBackoff {
    /// A ladder that is ready immediately and grows `RETRY_MIN` → `RETRY_MAX`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: Instant::now(),
            delay: RETRY_MIN,
        }
    }

    /// Whether the next retry is due.
    #[must_use]
    pub fn is_due(&self) -> bool {
        Instant::now() >= self.next
    }

    /// Reset to the base delay after a successful flush.
    pub fn reset(&mut self) {
        self.delay = RETRY_MIN;
        self.next = Instant::now();
    }

    /// Arm the next retry, doubling the delay up to [`RETRY_MAX`].
    pub fn arm(&mut self) {
        self.next = Instant::now() + self.delay;
        self.delay = (self.delay * 2).min(RETRY_MAX);
    }
}

impl Default for FlushBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an Instance has reached a lifecycle it cannot leave.
///
/// Provided for callers that need the classification itself. It is *not* a
/// licence to skip a flush: see [`flush_plan`] for why the lifecycle is the
/// wrong signal there.
#[cfg(test)]
#[must_use]
pub fn is_terminal(lifecycle: InstanceLifecycle) -> bool {
    matches!(
        lifecycle,
        InstanceLifecycle::Exited | InstanceLifecycle::Failed
    )
}

/// Read one bounded page for `instance_id` starting at `from_seq` (inclusive).
///
/// The Node store applies the bound inside the journal reader, so this costs
/// O(events in the page) and never O(journal). Returns the events and the
/// journal's durable seq alongside them.
pub fn read_page(
    node: &DevNode,
    instance_id: &InstanceId,
    from_seq: u64,
    limit: usize,
) -> Result<remuda_protocol::EventsReadResult, NodeError> {
    let journal_id = node.get_instance(instance_id)?.journal_id;
    // `from_seq` is inclusive here; `read_journal` takes the exclusive
    // watermark, so step back one.
    let after = U64(from_seq.saturating_sub(1));
    node.read_journal(&journal_id, Some(after), limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DevServerConfig;

    fn fake_node(dir: &std::path::Path) -> DevNode {
        crate::compose(&crate::ServeConfig::fake(
            DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
            dir.to_path_buf(),
        ))
        .expect("node")
    }

    fn terminal(lifecycle: &str) -> InstanceLifecycle {
        serde_json::from_value(serde_json::Value::String(lifecycle.to_owned())).expect("lifecycle")
    }

    #[test]
    fn terminal_lifecycles_are_exited_and_failed() {
        assert!(is_terminal(terminal("exited")));
        assert!(is_terminal(terminal("failed")));
        for live in ["ready", "starting", "closing", "requested"] {
            assert!(!is_terminal(terminal(live)), "{live} is not terminal");
        }
    }

    #[test]
    fn backoff_grows_to_the_ceiling_then_resets() {
        let mut backoff = FlushBackoff::new();
        assert!(backoff.is_due(), "the first retry is immediate");
        backoff.arm();
        assert!(!backoff.is_due());
        let mut delays = Vec::new();
        for _ in 0..8 {
            delays.push(backoff.delay);
            backoff.arm();
        }
        assert_eq!(delays[0], RETRY_MIN * 2);
        assert_eq!(*delays.last().expect("delays"), RETRY_MAX);
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        backoff.reset();
        assert_eq!(backoff.delay, RETRY_MIN);
        assert!(backoff.is_due());
    }

    #[test]
    fn cursor_never_moves_backwards_and_forgets_on_clear() {
        let mut cursor = FlushCursor::new();
        let instance = InstanceId::new();
        assert_eq!(cursor.last(&instance), None);
        cursor.record(&instance, 5);
        cursor.record(&instance, 3);
        assert_eq!(cursor.last(&instance), Some(5));
        cursor.observe(&instance, 9);
        assert_eq!(cursor.last(&instance), Some(9));
        cursor.observe(&instance, -1);
        assert_eq!(cursor.last(&instance), Some(9));
        cursor.forget(&instance);
        assert_eq!(cursor.last(&instance), None);
        cursor.record(&instance, 2);
        cursor.clear();
        assert!(cursor.is_empty());
    }

    #[tokio::test]
    async fn a_caught_up_instance_plans_no_read_and_an_unknown_cursor_reads() {
        let dir = tempfile::tempdir().expect("data dir");
        let node = fake_node(dir.path());
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({"prompt": "plan"})).expect("request"),
            )
            .await
            .expect("create");
        let instance_id = created.instance.meta.id.clone();
        let durable = node
            .current_journal_durable_seq(&instance_id)
            .expect("durable");

        // Cursor covers the tail: no read.
        assert_eq!(
            flush_plan(&node, &instance_id, Some(durable.0)),
            FlushPlan::CaughtUp
        );
        // Cursor behind the tail: read exactly the gap.
        assert_eq!(
            flush_plan(&node, &instance_id, None),
            FlushPlan::Read(1),
            "an unknown cursor replays from the floor"
        );
        // A ready Instance behind the tail still reads.
        assert_eq!(flush_plan(&node, &instance_id, Some(0)), FlushPlan::Read(1));
        assert_eq!(
            flush_plan(&node, &instance_id, Some(durable.0 - 1)),
            FlushPlan::Read(durable.0)
        );
    }

    #[test]
    fn cursor_is_keyed_per_instance() {
        let mut cursor = FlushCursor::new();
        let a = InstanceId::new();
        let b = InstanceId::new();
        cursor.record(&a, 4);
        assert_eq!(cursor.last(&b), None);
        assert_eq!(cursor.last(&a), Some(4));
    }

    /// A terminal lifecycle alone never skips a read; only the cursor does.
    ///
    /// The Instance here is marked failed with the cursor at the tail it had
    /// when it failed. That is genuinely caught up — but if another event lands
    /// afterwards (the reconcile diagnostic does exactly that), the cursor falls
    /// behind and the plan reads again. Lifecycle is not consulted either way.
    #[tokio::test]
    async fn a_terminal_lifecycle_does_not_skip_a_read_by_itself() {
        let dir = tempfile::tempdir().expect("data dir");
        let node = fake_node(dir.path());
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({"prompt": "fail"})).expect("request"),
            )
            .await
            .expect("create");
        let instance_id = created.instance.meta.id.clone();
        node.store()
            .set_instance_failure(&instance_id, "driver exited")
            .expect("mark failed");
        let durable = node
            .current_journal_durable_seq(&instance_id)
            .expect("durable");

        assert_eq!(
            flush_plan(&node, &instance_id, Some(durable.0)),
            FlushPlan::CaughtUp,
            "a failed Instance at its tail cannot produce more events"
        );
        // But a failed Instance this session never caught up with still
        // replays: the tail holds events the Hub has not seen.
        assert_eq!(
            flush_plan(&node, &instance_id, Some(durable.0.saturating_sub(1))),
            FlushPlan::Read(durable.0)
        );
        assert_eq!(
            flush_plan(&node, &instance_id, None),
            FlushPlan::Read(1),
            "an unknown cursor must not skip a terminal Instance's tail"
        );
        node.shutdown().await.expect("shutdown");
    }

    /// A terminal lifecycle is not proof that a journal is finished.
    ///
    /// `reclaim::reconcile_native_pty` marks an Instance failed and only then
    /// journals the diagnostic that explains why. A flush tick armed with a
    /// current cursor must therefore still read, and the plan must be driven by
    /// the cursor rather than the lifecycle — this test pins that ordering so a
    /// future "skip terminal instances" shortcut cannot silently strand the
    /// diagnostic.
    #[tokio::test]
    async fn a_terminal_lifecycle_that_is_journaled_after_the_fact_still_flushes() {
        let dir = tempfile::tempdir().expect("data dir");
        let node = fake_node(dir.path());
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({"prompt": "reconcile"}))
                    .expect("request"),
            )
            .await
            .expect("create");
        let instance_id = created.instance.meta.id.clone();
        let journal_id = created.instance.journal_id.clone();

        // Mark failed, then journal afterwards — the reconcile order.
        node.store()
            .set_instance_failure(&instance_id, "node epoch changed")
            .expect("mark failed");
        assert!(is_terminal(
            node.get_instance(&instance_id).expect("instance").lifecycle
        ));
        let before = node
            .current_journal_durable_seq(&instance_id)
            .expect("durable");
        let body = crate::driver::message_payload(
            remuda_protocol::MessageRole::Assistant,
            remuda_protocol::MessagePhase::Final,
            "node-epoch-changed".to_owned(),
            Vec::new(),
        )
        .expect("message");
        node.store()
            .append_observation(
                &instance_id,
                None,
                remuda_protocol::Completeness::Structured,
                body,
            )
            .expect("append diagnostic");

        let after = node
            .current_journal_durable_seq(&instance_id)
            .expect("durable");
        assert!(after > before, "the diagnostic landed after the failure");

        // The cursor the session held before the diagnostic is now behind, so
        // the plan reads — a lifecycle-keyed skip would have dropped it.
        assert_eq!(
            flush_plan(&node, &instance_id, Some(before.0)),
            FlushPlan::Read(before.0 + 1),
            "a terminal Instance journaled after the fact must still be read"
        );
        let page = read_page(&node, &instance_id, before.0 + 1, 256).expect("page");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].position().1, after);
        assert_eq!(page.durable_seq, after);

        // Only once the cursor reaches the new tail is it caught up.
        assert_eq!(
            flush_plan(&node, &instance_id, Some(after.0)),
            FlushPlan::CaughtUp
        );
        let _ = journal_id;
        node.shutdown().await.expect("shutdown");
    }

    /// A cursor past the journal's tail (a replaced journal, a Hub watermark
    /// from a different Node) has nothing left to read.
    #[tokio::test]
    async fn a_cursor_past_the_tail_has_nothing_to_read() {
        let dir = tempfile::tempdir().expect("data dir");
        let node = fake_node(dir.path());
        let created = node
            .create_instance(
                serde_json::from_value(serde_json::json!({"prompt": "gap"})).expect("request"),
            )
            .await
            .expect("create");
        let instance_id = created.instance.meta.id.clone();
        let journal_id = created.instance.journal_id.clone();

        // A fresh journal at seq 0: any cursor reads from the floor, including
        // one the Hub claimed to hold.
        let page = node.read_journal(&journal_id, None, 256).expect("page");
        if page.events.is_empty() {
            assert_eq!(flush_plan(&node, &instance_id, Some(7)), FlushPlan::Read(8));
        } else {
            let durable = page.durable_seq.0;
            assert_eq!(
                flush_plan(&node, &instance_id, Some(durable + 7)),
                FlushPlan::CaughtUp,
                "a cursor past the tail has nothing left to read"
            );
        }
        node.shutdown().await.expect("shutdown");
    }
}
