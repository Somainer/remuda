//! In-session effort switching and transcript read-back (D-028 §9.1).
//!
//! Two directions, one rule: **the transcript is the only authority on what is
//! actually in effect**.
//!
//! Remuda → Claude writes `/effort <level>` into the native composer (body,
//! then CR as a separate write; the confirmation dialog Claude opens because
//! the conversation is cached at another level gets a second CR), then waits
//! for an assistant record carrying the new level. If that read-back does not
//! arrive in a bounded window the outcome is `degraded`, never `applied`.
//!
//! Claude → Remuda is the same read applied to observations: every assistant
//! record's `effort` / `perTurnEffort`, deduped when unchanged, with a
//! preceding `/effort` slash record attributing the source.
//!
//! Measured against claude 2.1.221 (see `docs/design/evidence/effort-sync-1.md`):
//! in-session `/effort` accepts `low | medium | high | xhigh | max | ultracode
//! | auto`; every switch while a cached conversation exists opens a
//! "Change effort level?" confirm dialog; assistant records carry a top-level
//! `effort` string (ultracode spells as `xhigh`).

use remuda_protocol::{EffortName, Severity};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use tokio::sync::{Notify, oneshot};

/// Bounded wait for the post-switch assistant record.
///
/// The switch only affects the *next* turn, and read-back therefore arrives
/// when the user next prompts. 45 s covers the "switch, immediate prompt"
/// path on a slow network while bounding the blocking configure call; a
/// switch made while the agent is working is queued instead (see
/// [`SwitchOutcome`]) and is not subject to this timeout.
pub(crate) const EFFORT_READBACK_TIMEOUT_MS: u64 = 45_000;
/// Poll cadence while waiting for read-back.
pub(crate) const EFFORT_READBACK_POLL_MS: u64 = 250;
/// Settle pause between writes (composer body, the submitting CR, the confirm
/// dialog CR). The ready ladder already gates the first write; these small
/// gaps keep keystrokes out of one TTY read.
pub(crate) const EFFORT_WRITE_SETTLE_MS: u64 = 120;

/// The exact `/effort` vocabulary measured in-session on claude 2.1.221.
#[allow(dead_code)] // documentation constant; also used by the fake harness
pub(crate) const IN_SESSION_LEVELS: &[&str] =
    &["low", "medium", "high", "xhigh", "max", "ultracode", "auto"];

/// What the switch was asked to change to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortRequest {
    /// Level to type (`ultracode` spelled out so `/effort ultracode` works).
    pub(crate) name: EffortName,
    pub(crate) ultracode: bool,
}

impl EffortRequest {
    pub(crate) fn from_level(level: &str) -> Option<Self> {
        match level.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self {
                name: EffortName::Low,
                ultracode: false,
            }),
            "medium" => Some(Self {
                name: EffortName::Medium,
                ultracode: false,
            }),
            "high" => Some(Self {
                name: EffortName::High,
                ultracode: false,
            }),
            "xhigh" => Some(Self {
                name: EffortName::Xhigh,
                ultracode: false,
            }),
            "max" => Some(Self {
                name: EffortName::Max,
                ultracode: false,
            }),
            "ultracode" => Some(Self {
                name: EffortName::Xhigh,
                ultracode: true,
            }),
            _ => None,
        }
    }

    /// The native spelling to type after `/effort ` — `ultracode` is its own
    /// word; the five levels spell themselves.
    pub(crate) fn command_word(&self) -> &'static str {
        if self.ultracode {
            "ultracode"
        } else {
            match self.name {
                EffortName::Low => "low",
                EffortName::Medium => "medium",
                EffortName::High => "high",
                EffortName::Xhigh => "xhigh",
                EffortName::Max => "max",
            }
        }
    }

    /// The whole slash command body, without the submitting CR.
    pub(crate) fn command_body(&self) -> String {
        format!("/effort {}", self.command_word())
    }

    /// Level the transcript must report for this switch to count as observed.
    ///
    /// `ultracode` reads back as `xhigh` — Claude does not repeat the workflow
    /// flag on assistant records.
    pub(crate) fn observed_name(&self) -> EffortName {
        self.name
    }
}

/// Terminal state of a switch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchOutcome {
    /// `/effort …` was typed and an assistant record observed the new level.
    Applied,
    /// The agent was working; the change is held for the next idle moment.
    Queued,
    /// The command was typed but read-back never matched; effective stays put.
    Degraded,
    /// The requested word is not in the in-session vocabulary.
    #[allow(dead_code)] // produced by the documented reject path
    Unsupported,
    /// Control was not available (closed pane, booting TUI, …).
    ControlUnavailable,
}

impl SwitchOutcome {
    /// Stable journal status string for the `instance.configure` lifecycle.
    pub(crate) fn journal_status(self, word: &str, detail: &str) -> String {
        match self {
            Self::Applied => format!("effort-applied:{word}"),
            Self::Queued => format!("effort-queued:{word}"),
            Self::Degraded => format!("effort-degraded:{word}:{detail}"),
            Self::Unsupported => format!("effort-unsupported-in-session:{word}"),
            Self::ControlUnavailable => format!("effort-control-unavailable:{detail}"),
        }
    }
}

/// Coordination between the driver's switch call and the transcript pump.
///
/// The driver arming a switch is synchronous with typing `/effort`; the
/// read-back only arrives later, when the next assistant record is mapped.
/// The bridge is that rendezvous: the mapper arms its [`EffortTracker`] from
/// [`pending`](Self::pending), and resolves the generation once the level is
/// observed. The switch call waits bounded; on timeout it fails the
/// generation so a much later natural change to the same level is not
/// mis-attributed to Remuda.
pub(crate) struct EffortBridge {
    state: Mutex<BridgeState>,
    notify: Notify,
}

#[derive(Default)]
struct BridgeState {
    /// A switch waiting for its assistant-record read-back.
    pending: Option<(u64, EffortRequest)>,
    /// Generation whose read-back has arrived.
    observed_gen: u64,
    observed: Option<remuda_protocol::ObservedEffort>,
    /// The latest thing any side asked for (payload `requested` field).
    requested: Option<EffortRequest>,
    generation: u64,
}

impl EffortBridge {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(BridgeState::default()),
            notify: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BridgeState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Arm a new Remuda switch; returns its generation token.
    pub(crate) fn arm(&self, request: EffortRequest) -> u64 {
        let mut state = self.lock();
        state.generation += 1;
        let generation = state.generation;
        state.pending = Some((generation, request));
        state.requested = Some(request);
        generation
    }

    /// Record the launch-time requested selection (payload provenance only).
    pub(crate) fn note_launch_request(&self, request: EffortRequest) {
        self.lock().requested = Some(request);
    }

    /// The pending switch the mapper should arm its tracker with, if any.
    pub(crate) fn pending(&self) -> Option<EffortRequest> {
        self.lock().pending.map(|(_, request)| request)
    }

    /// Pending switch together with its generation, for the mapper to arm once.
    pub(crate) fn pending_with_gen(&self) -> Option<(u64, EffortRequest)> {
        self.lock().pending
    }

    /// Whether a switch is currently awaiting read-back.
    pub(crate) fn has_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    /// Latest requested selection, for the emitted payload.
    pub(crate) fn requested(&self) -> Option<EffortRequest> {
        self.lock().requested
    }

    /// Mapper side: the level `generation` was waiting for has been observed.
    pub(crate) fn resolve(&self, generation: u64, observed: remuda_protocol::ObservedEffort) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.observed = Some(observed);
            state.observed_gen = generation;
        }
        self.notify.notify_waiters();
    }

    /// Give up on a generation without an observation (bounded timeout).
    pub(crate) fn fail(&self, generation: u64) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
        }
        self.notify.notify_waiters();
    }

    /// Wait until generation `generation` reads back, bounded by `timeout`.
    pub(crate) async fn wait(
        &self,
        generation: u64,
        timeout: std::time::Duration,
    ) -> Option<remuda_protocol::ObservedEffort> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let state = self.lock();
                if state.observed_gen == generation {
                    return state.observed;
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep(remaining) => return None,
            }
        }
    }
}

/// The driver-side I/O a switch needs: readiness, two-segment writes, and a
/// screen read for the post-submit confirmation dialog. One adapter per PTY
/// carrier (Herdr pane, local PTY).
#[async_trait::async_trait]
pub(crate) trait EffortSwitchIo: Send + Sync {
    /// True when the native composer can accept the slash command right now.
    async fn is_idle(&self) -> bool;

    /// Type the slash-command body (no submitting CR).
    async fn type_body(&self, body: &str) -> crate::DriverResult<()>;

    /// Send Enter as its own write.
    async fn press_enter(&self) -> crate::DriverResult<()>;

    /// Current visible screen text, ANSI stripped.
    async fn screen_text(&self) -> crate::DriverResult<String>;

    /// Journal an `instance.configure` effort lifecycle from the worker.
    async fn journal(&self, status: String, severity: Severity);
}

/// Type the slash command, clear the confirmation dialog Claude opens for a
/// cached conversation, then wait for the transcript read-back.
///
/// The body and both Enter presses are separate writes with a settle gap,
/// matching [`crate::shell_pty::send`]'s measured requirement.
pub(crate) async fn perform_switch(
    request: EffortRequest,
    bridge: &EffortBridge,
    io: &dyn EffortSwitchIo,
) -> SwitchOutcome {
    let word = request.command_word();
    // Arm before typing so the mapper correlates the slash record the command
    // produces, even when the read-back assistant record is still a turn away.
    let generation = bridge.arm(request);

    if let Err(error) = io.type_body(&request.command_body()).await {
        tracing::warn!(%error, "effort switch: body write failed");
        bridge.fail(generation);
        io.journal(
            SwitchOutcome::ControlUnavailable.journal_status(word, &error.to_string()),
            Severity::Error,
        )
        .await;
        return SwitchOutcome::ControlUnavailable;
    }
    tokio::time::sleep(std::time::Duration::from_millis(EFFORT_WRITE_SETTLE_MS)).await;
    if let Err(error) = io.press_enter().await {
        tracing::warn!(%error, "effort switch: submit write failed");
        bridge.fail(generation);
        io.journal(
            SwitchOutcome::ControlUnavailable.journal_status(word, &error.to_string()),
            Severity::Error,
        )
        .await;
        return SwitchOutcome::ControlUnavailable;
    }

    // A cached conversation makes Claude confirm ("Change effort level? …
    // 1. Yes, switch …"). Read, accept only when the dialog is really there.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    match io.screen_text().await {
        Ok(text) if text.contains("Change effort level") => {
            tokio::time::sleep(std::time::Duration::from_millis(EFFORT_WRITE_SETTLE_MS)).await;
            if let Err(error) = io.press_enter().await {
                tracing::warn!(%error, "effort switch: confirm write failed");
            }
        }
        Ok(text) if text.contains("Invalid argument") => {
            bridge.fail(generation);
            io.journal(
                SwitchOutcome::Degraded.journal_status(word, "invalid-argument"),
                Severity::Warning,
            )
            .await;
            return SwitchOutcome::Degraded;
        }
        Ok(_) => {}
        Err(error) => {
            // The dialog read failing must not stop the switch: it may already
            // be applied. Read-back is the authority, not this screen probe.
            tracing::debug!(%error, "effort switch: dialog screen read failed; continuing");
        }
    }

    match bridge
        .wait(
            generation,
            std::time::Duration::from_millis(EFFORT_READBACK_TIMEOUT_MS),
        )
        .await
    {
        Some(_) => SwitchOutcome::Applied,
        None => {
            // Never claim applied without an observation. A switch the user
            // does not prompt after simply has no assistant record to read.
            bridge.fail(generation);
            io.journal(
                SwitchOutcome::Degraded.journal_status(word, "no-readback-within-window"),
                Severity::Warning,
            )
            .await;
            SwitchOutcome::Degraded
        }
    }
}

/// A switch held for the next idle moment (D-028 §9.1 ready ladder).
struct QueuedSwitch {
    request: EffortRequest,
    /// When present the caller is awaiting the outcome (idle fast path); when
    /// absent the worker journals the terminal status itself (working path).
    done: Option<oneshot::Sender<SwitchOutcome>>,
}

/// Queue of at most one pending switch (a newer switch replaces an older one).
pub(crate) struct EffortQueue {
    pending: Mutex<Option<QueuedSwitch>>,
    notify: Notify,
    closed: AtomicBool,
}

impl EffortQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Queue a switch. `done` is handed back when it has been applied.
    pub(crate) fn enqueue(
        &self,
        request: EffortRequest,
        done: Option<oneshot::Sender<SwitchOutcome>>,
    ) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(QueuedSwitch { request, done });
        }
        self.notify.notify_one();
    }

    /// Take the queued switch, if any.
    fn take(&self) -> Option<QueuedSwitch> {
        self.pending.lock().ok().and_then(|mut guard| guard.take())
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, AtomicOrdering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// Spawn the worker that applies queued switches once the composer is idle.
///
/// The fast path (configure while idle) still goes through here so there is a
/// single serialization point; its `oneshot` carries the outcome straight
/// back, so no final lifecycle is journaled. The working path journaled
/// `queued` at enqueue time and gets its terminal `applied` / `degraded`
/// lifecycle from this worker.
pub(crate) fn spawn_worker(
    bridge: Arc<EffortBridge>,
    queue: Arc<EffortQueue>,
    io: Arc<dyn EffortSwitchIo>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !queue.closed.load(AtomicOrdering::SeqCst) {
            // Arm the waiter before checking, so an enqueue cannot be missed.
            let notified = queue.notify.notified();
            let queued = queue.take();
            match queued {
                Some(QueuedSwitch { request, done }) => {
                    let word = request.command_word();
                    // Wait out a running turn on the ready ladder. A newer
                    // switch arriving meanwhile replaces this one, which never
                    // goes out and is reported back as still `queued`.
                    let mut replaced = false;
                    while !io.is_idle().await {
                        if queue.closed.load(AtomicOrdering::SeqCst) {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            EFFORT_READBACK_POLL_MS,
                        ))
                        .await;
                        if queue.pending.lock().map(|p| p.is_some()).unwrap_or(false) {
                            replaced = true;
                            break;
                        }
                    }
                    if replaced || queue.pending.lock().map(|p| p.is_some()).unwrap_or(false) {
                        // The enqueue's notify_one was consumed by us; wake the
                        // outer iteration for the replacement explicitly.
                        queue.notify.notify_one();
                        if let Some(done) = done {
                            let _ = done.send(SwitchOutcome::Queued);
                        }
                        continue;
                    }
                    let outcome = perform_switch(request, &bridge, io.as_ref()).await;
                    match done {
                        Some(done) => {
                            let _ = done.send(outcome);
                        }
                        None => {
                            let severity = match outcome {
                                SwitchOutcome::Applied | SwitchOutcome::Queued => Severity::Info,
                                SwitchOutcome::Unsupported => Severity::Warning,
                                SwitchOutcome::Degraded => Severity::Warning,
                                SwitchOutcome::ControlUnavailable => Severity::Error,
                            };
                            io.journal(outcome.journal_status(word, ""), severity).await;
                        }
                    }
                }
                None => {
                    notified.await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_round_trips_through_the_command_word() {
        for (level, word, observed) in [
            ("low", "/effort low", EffortName::Low),
            ("high", "/effort high", EffortName::High),
            ("xhigh", "/effort xhigh", EffortName::Xhigh),
            ("max", "/effort max", EffortName::Max),
            ("ultracode", "/effort ultracode", EffortName::Xhigh),
        ] {
            let request = EffortRequest::from_level(level).expect(level);
            assert_eq!(request.command_body(), word);
            assert_eq!(request.observed_name(), observed);
        }
        assert!(EffortRequest::from_level("bogus").is_none());
        assert!(EffortRequest::from_level("auto").is_none());
    }

    #[tokio::test]
    async fn bridge_resolves_when_the_mapper_reports_the_level() {
        let bridge = EffortBridge::new();
        let request = EffortRequest::from_level("xhigh").unwrap();
        let generation = bridge.arm(request);
        let observed = remuda_protocol::ObservedEffort {
            name: EffortName::Xhigh,
            ultracode: None,
        };
        bridge.resolve(generation, observed);
        let got = bridge
            .wait(generation, std::time::Duration::from_secs(2))
            .await
            .expect("read-back resolves");
        assert_eq!(got.name, EffortName::Xhigh);
        assert!(bridge.pending().is_none());
    }

    #[tokio::test]
    async fn bridge_times_out_when_nothing_is_observed() {
        let bridge = EffortBridge::new();
        let request = EffortRequest::from_level("max").unwrap();
        let generation = bridge.arm(request);
        assert!(
            bridge
                .wait(generation, std::time::Duration::from_millis(20))
                .await
                .is_none()
        );
        bridge.fail(generation);
        assert!(bridge.pending().is_none());
    }
}

/// Mock switch I/O recording the exact write order for unit tests.
#[cfg(test)]
mod switch_tests {
    use super::*;
    use crate::DriverResult;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockIo {
        writes: Mutex<Vec<String>>,
        journals: Mutex<Vec<(String, Severity)>>,
        idle: AtomicBool,
        screen: Mutex<String>,
        bridge: Mutex<Option<Arc<EffortBridge>>>,
        /// Resolve the read-back once this many writes have happened.
        resolve_after: usize,
        journal_count: AtomicUsize,
    }

    impl MockIo {
        fn set_idle(&self, idle: bool) {
            self.idle.store(idle, Ordering::SeqCst);
        }
        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
        fn journals(&self) -> Vec<(String, Severity)> {
            self.journals.lock().unwrap().clone()
        }
        fn maybe_resolve(&self) {
            let n = self.writes.lock().unwrap().len();
            if self.resolve_after != 0
                && n >= self.resolve_after
                && let Some(bridge) = self.bridge.lock().unwrap().clone()
                && let Some((generation, request)) = bridge.pending_with_gen()
            {
                bridge.resolve(
                    generation,
                    remuda_protocol::ObservedEffort {
                        name: request.observed_name(),
                        ultracode: None,
                    },
                );
            }
        }
    }

    #[async_trait::async_trait]
    impl EffortSwitchIo for MockIo {
        async fn is_idle(&self) -> bool {
            self.idle.load(Ordering::SeqCst)
        }
        async fn type_body(&self, body: &str) -> DriverResult<()> {
            self.writes.lock().unwrap().push(format!("body:{body}"));
            self.maybe_resolve();
            Ok(())
        }
        async fn press_enter(&self) -> DriverResult<()> {
            self.writes.lock().unwrap().push("cr".into());
            self.journal_count.fetch_add(1, Ordering::SeqCst);
            self.maybe_resolve();
            Ok(())
        }
        async fn screen_text(&self) -> DriverResult<String> {
            Ok(self.screen.lock().unwrap().clone())
        }
        async fn journal(&self, status: String, severity: Severity) {
            self.journals.lock().unwrap().push((status, severity));
        }
    }

    #[tokio::test]
    async fn an_idle_switch_types_body_then_two_crs_and_reports_applied() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 2, // resolve read-back at the confirmation CR
            ..Default::default()
        });
        io.set_idle(true);
        io.screen
            .lock()
            .unwrap()
            .push_str("Change effort level? 1. Yes, switch to xhigh");
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let request = EffortRequest::from_level("xhigh").unwrap();

        let outcome = perform_switch(request, &bridge, io.as_ref()).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        // Body, submit CR, then the confirmation dialog CR — three writes.
        assert_eq!(
            io.writes(),
            vec![
                "body:/effort xhigh".to_string(),
                "cr".to_string(),
                "cr".to_string(),
            ]
        );
        assert!(
            io.journals().is_empty(),
            "applied emits no terminal journal"
        );
    }

    #[tokio::test]
    async fn no_confirmation_dialog_means_one_cr() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 2, // body + the single submit CR
            ..Default::default()
        });
        io.set_idle(true);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let request = EffortRequest::from_level("low").unwrap();
        let outcome = perform_switch(request, &bridge, io.as_ref()).await;
        assert_eq!(outcome, SwitchOutcome::Applied);
        assert_eq!(
            io.writes(),
            vec!["body:/effort low".to_string(), "cr".to_string()]
        );
    }

    #[tokio::test]
    async fn missing_read_back_degrades_and_never_claims_applied() {
        // Short timeout is not configurable per call, so this test runs the real
        // bounded window: the contract is that a silent transcript cannot be
        // claimed as applied.
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo::default());
        io.set_idle(true);
        let start = std::time::Instant::now();
        let outcome = perform_switch(
            EffortRequest::from_level("max").unwrap(),
            &bridge,
            io.as_ref(),
        )
        .await;
        assert_eq!(outcome, SwitchOutcome::Degraded);
        assert!(
            start.elapsed().as_millis() >= 40_000,
            "waits the bounded window"
        );
        assert!(io.journals().iter().any(|(status, severity)| {
            status.starts_with("effort-degraded:max") && *severity == Severity::Warning
        }));
    }

    #[tokio::test]
    async fn a_switch_while_working_is_queued_until_idle_then_applied() {
        let bridge = Arc::new(EffortBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 3,
            ..Default::default()
        });
        io.set_idle(false);
        io.screen.lock().unwrap().push_str("Change effort level?");
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let queue = Arc::new(EffortQueue::new());
        let _worker = spawn_worker(
            Arc::clone(&bridge),
            Arc::clone(&queue),
            Arc::clone(&io) as Arc<dyn EffortSwitchIo>,
        );
        let request = EffortRequest::from_level("xhigh").unwrap();
        io.journal(
            SwitchOutcome::Queued.journal_status("xhigh", ""),
            Severity::Info,
        )
        .await;
        queue.enqueue(request, None);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(io.writes().is_empty(), "nothing goes out while working");
        io.set_idle(true);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2_000);
        while io.writes().len() < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            io.writes(),
            vec![
                "body:/effort xhigh".to_string(),
                "cr".to_string(),
                "cr".to_string(),
            ]
        );
    }
}
