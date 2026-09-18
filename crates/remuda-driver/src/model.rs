//! Live two-way `/model` sync for the native Claude TUI (D-028 §9.1).
//!
//! Sibling of [`crate::effort`]: a Remuda model switch types
//! `/model <id>` into the composer and the transcript command verdict proves
//! what actually took effect. Measurements on claude 2.1.221 and 2.1.272 in
//! `docs/design/evidence/model-sync-1.md`.
//!
//! Differences from `/effort`:
//! - on 2.1.272 a valid id applies with **no confirmation dialog** — the slash
//!   record and the `<local-command-stdout>` verdict land with one timestamp;
//!   2.1.221 showed a `Switch model?` dialog, so the screen gate stays;
//! - the verdict spells the *resolved* id (an alias resolves to a concrete
//!   gateway id), not necessarily the requested word;
//! - rejects (`Model '<id>' not found`) and dismissed pickers (`Kept model
//!   as <id>`) are `system` transcript records, not `user` records.

use remuda_protocol::{EffortSource, ModelSelectionPath, ObservedModel, Severity};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use tokio::sync::{Notify, oneshot};

/// Reuse the effort switch's exact PTY I/O surface (readiness, split
/// body/Enter writes, screen read, configure-lifecycle journal).
pub(crate) use crate::effort::EffortSwitchIo as SwitchIo;

pub(crate) const MODEL_READBACK_TIMEOUT_MS: u64 = crate::effort::EFFORT_READBACK_TIMEOUT_MS;
pub(crate) const MODEL_READBACK_POLL_MS: u64 = crate::effort::EFFORT_READBACK_POLL_MS;
pub(crate) const MODEL_WRITE_SETTLE_MS: u64 = crate::effort::EFFORT_WRITE_SETTLE_MS;

/// A requested `/model <id>` switch. Ids are free-form (gateway ids carry
/// `/` and `[1m]`), so — unlike effort tiers — there is no closed vocabulary
/// to validate against here; the native command is the authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRequest {
    /// The raw id/alias to type after `/model `.
    pub(crate) id: String,
}

impl ModelRequest {
    pub(crate) fn new(id: &str) -> Option<Self> {
        let id = id.trim();
        // A bare `/model` opens the interactive picker, which the driver
        // cannot drive to a choice; never type an empty argument.
        (!id.is_empty() && !id.contains(['\n', '\r'])).then(|| Self { id: id.to_owned() })
    }

    pub(crate) fn command_body(&self) -> String {
        format!("/model {}", self.id)
    }
}

/// Terminal state of a model switch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSwitchOutcome {
    Applied,
    Queued,
    Degraded,
    ControlUnavailable,
}

impl ModelSwitchOutcome {
    pub(crate) fn journal_status(self, id: &str, detail: &str) -> String {
        match self {
            Self::Applied => format!("model-applied:{id}"),
            Self::Queued => format!("model-queued:{id}"),
            Self::Degraded => format!("model-degraded:{id}:{detail}"),
            Self::ControlUnavailable => format!("model-control-unavailable:{detail}"),
        }
    }
}

/// Terminal verdict of a model switch's read-back wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelReadback {
    /// The stdout verdict accepted the switch; this is the resolved id now in
    /// effect.
    Applied(ObservedModel),
    /// Claude refused (`not found`, dismissed dialog/picker).
    Rejected {
        /// Stable reason code (`not-found`, `dialog-kept`).
        reason: String,
    },
}

/// Rendezvous between the driver typing `/model` and the transcript pump
/// mapping its command verdict. Mirrors [`crate::effort::EffortBridge`].
pub(crate) struct ModelBridge {
    state: Mutex<BridgeState>,
    notify: Notify,
}

#[derive(Default)]
struct BridgeState {
    pending: Option<(u64, ModelRequest)>,
    verdict_gen: u64,
    verdict: Option<ModelReadback>,
    requested: Option<ModelRequest>,
    /// Whether the current `requested` id was offered by the session's own
    /// resolved catalog (`listed`) or typed verbatim (`typed`). Absent for
    /// launch seeds and terminal-side switches.
    requested_path: Option<ModelSelectionPath>,
    generation: u64,
    /// The session's own switchable ids. `None` while the only answer was the
    /// host-fallback cache (the CLI's own list is not known then).
    own_catalog: Option<Vec<String>>,
    /// The latest effective model, with what established it, so a catalog
    /// refresh can name the model in effect without inventing an edge.
    last_effective: Option<(String, EffortSource, Option<ModelSelectionPath>)>,
}

impl ModelBridge {
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

    /// Seed/update the session's own resolved catalog. The scoped cache,
    /// settings and builtin answers describe ids the CLI itself will accept;
    /// the host-fallback answer does not, so it is seeded as `None`.
    pub(crate) fn set_own_catalog(&self, ids: Option<Vec<String>>) {
        self.lock().own_catalog = ids;
    }

    /// The latest effective model, its attribution and selection path.
    pub(crate) fn last_effective(
        &self,
    ) -> Option<(String, EffortSource, Option<ModelSelectionPath>)> {
        self.lock().last_effective.clone()
    }

    pub(crate) fn arm(&self, request: ModelRequest) -> u64 {
        let mut state = self.lock();
        let path = match &state.own_catalog {
            Some(ids) => {
                if ids.iter().any(|id| id == &request.id) {
                    ModelSelectionPath::Listed
                } else {
                    ModelSelectionPath::Typed
                }
            }
            // The session's own list is unknown (host-fallback answer): any
            // switch is a verbatim `/model <id>` the verdict must judge.
            None => ModelSelectionPath::Typed,
        };
        state.generation += 1;
        let generation = state.generation;
        state.pending = Some((generation, request.clone()));
        state.requested = Some(request);
        state.requested_path = Some(path);
        generation
    }

    pub(crate) fn note_launch_request(&self, id: String) {
        let mut state = self.lock();
        state.requested = Some(ModelRequest { id: id.clone() });
        state.requested_path = None;
        state.last_effective = Some((id, EffortSource::Launch, None));
    }

    pub(crate) fn pending(&self) -> Option<ModelRequest> {
        self.lock()
            .pending
            .as_ref()
            .map(|(_, request)| request.clone())
    }

    pub(crate) fn pending_with_gen(&self) -> Option<(u64, ModelRequest)> {
        self.lock().pending.clone()
    }

    /// Whether a switch is currently awaiting read-back.
    #[allow(dead_code)] // parity with the effort bridge; used by resume paths
    pub(crate) fn has_pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    pub(crate) fn requested(&self) -> Option<ModelRequest> {
        self.lock().requested.clone()
    }

    /// Selection path recorded for the current `requested` switch, if any.
    pub(crate) fn requested_path(&self) -> Option<ModelSelectionPath> {
        self.lock().requested_path
    }

    /// Record the latest model the transcript proved in effect, with its
    /// attribution and the selection path that established it.
    pub(crate) fn note_effective(
        &self,
        id: String,
        source: EffortSource,
        path: Option<ModelSelectionPath>,
    ) {
        self.lock().last_effective = Some((id, source, path));
    }

    pub(crate) fn resolve(&self, generation: u64, observed: ObservedModel) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.last_effective = Some((
                observed.id.clone(),
                EffortSource::Remuda,
                state.requested_path,
            ));
            state.verdict = Some(ModelReadback::Applied(observed));
            state.verdict_gen = generation;
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn reject(&self, generation: u64, reason: impl Into<String>) {
        {
            let mut state = self.lock();
            if let Some((pending_gen, _)) = state.pending
                && pending_gen == generation
            {
                state.pending = None;
            }
            state.verdict = Some(ModelReadback::Rejected {
                reason: reason.into(),
            });
            state.verdict_gen = generation;
        }
        self.notify.notify_waiters();
    }

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

    pub(crate) async fn wait(
        &self,
        generation: u64,
        timeout: std::time::Duration,
    ) -> Option<ModelReadback> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let state = self.lock();
                if state.verdict_gen == generation {
                    return state.verdict.clone();
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

/// Type `/model <id>`, accept the confirmation dialog when the screen shows
/// one (2.1.221; 2.1.272 applies directly), and wait for the command verdict.
pub(crate) async fn perform_model_switch(
    request: ModelRequest,
    bridge: &ModelBridge,
    io: &dyn SwitchIo,
) -> ModelSwitchOutcome {
    let id = request.id.clone();
    let body = request.command_body();
    let generation = bridge.arm(request);

    if let Err(error) = io.type_body(&body).await {
        tracing::warn!(%error, "model switch: body write failed");
        bridge.fail(generation);
        io.journal(
            ModelSwitchOutcome::ControlUnavailable.journal_status(&id, &error.to_string()),
            Severity::Error,
        )
        .await;
        return ModelSwitchOutcome::ControlUnavailable;
    }
    tokio::time::sleep(std::time::Duration::from_millis(MODEL_WRITE_SETTLE_MS)).await;
    if let Err(error) = io.press_enter().await {
        tracing::warn!(%error, "model switch: submit write failed");
        bridge.fail(generation);
        io.journal(
            ModelSwitchOutcome::ControlUnavailable.journal_status(&id, &error.to_string()),
            Severity::Error,
        )
        .await;
        return ModelSwitchOutcome::ControlUnavailable;
    }

    // 2.1.221 confirms a cached-conversation switch ("Switch model? …
    // Yes, switch to …"). 2.1.272 applies a valid id with no dialog, and an
    // unknown id renders a "not found" error that must NOT get a confirming
    // CR. Poll for the dialog exactly like the effort path; the transcript
    // verdict is the acceptance authority either way.
    let dialog_deadline = std::time::Duration::from_millis(1_500);
    let dialog_start = std::time::Instant::now();
    let mut dialog_seen = false;
    while dialog_start.elapsed() < dialog_deadline {
        match io.screen_text().await {
            Ok(text) => {
                let low = text.to_lowercase();
                if low.contains("switch model") {
                    dialog_seen = true;
                    break;
                }
                if low.contains("not found") {
                    break;
                }
            }
            Err(error) => {
                tracing::debug!(%error, "model switch: dialog screen read failed; continuing");
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    if dialog_seen {
        tokio::time::sleep(std::time::Duration::from_millis(MODEL_WRITE_SETTLE_MS)).await;
        if let Err(error) = io.press_enter().await {
            tracing::warn!(%error, "model switch: confirm write failed");
        }
    }

    match bridge
        .wait(
            generation,
            std::time::Duration::from_millis(MODEL_READBACK_TIMEOUT_MS),
        )
        .await
    {
        Some(ModelReadback::Applied(_)) => ModelSwitchOutcome::Applied,
        Some(ModelReadback::Rejected { reason }) => {
            bridge.fail(generation);
            io.journal(
                ModelSwitchOutcome::Degraded.journal_status(&id, &reason),
                Severity::Warning,
            )
            .await;
            ModelSwitchOutcome::Degraded
        }
        None => {
            bridge.fail(generation);
            io.journal(
                ModelSwitchOutcome::Degraded.journal_status(&id, "no-readback-within-window"),
                Severity::Warning,
            )
            .await;
            ModelSwitchOutcome::Degraded
        }
    }
}

/// A model switch held for the next idle moment.
struct QueuedModelSwitch {
    request: ModelRequest,
    done: Option<oneshot::Sender<ModelSwitchOutcome>>,
}

/// Queue of at most one pending model switch (a newer one replaces an older).
pub(crate) struct ModelQueue {
    pending: Mutex<Option<QueuedModelSwitch>>,
    notify: Notify,
    closed: AtomicBool,
}

impl ModelQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn enqueue(
        &self,
        request: ModelRequest,
        done: Option<oneshot::Sender<ModelSwitchOutcome>>,
    ) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(QueuedModelSwitch { request, done });
        }
        self.notify.notify_one();
    }

    fn take(&self) -> Option<QueuedModelSwitch> {
        self.pending.lock().ok().and_then(|mut guard| guard.take())
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, AtomicOrdering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// Serialization point for model switches, mirroring the effort worker: idle
/// fast path hands the outcome straight back; working path gets its terminal
/// lifecycle journaled here.
pub(crate) fn spawn_model_worker(
    bridge: Arc<ModelBridge>,
    queue: Arc<ModelQueue>,
    io: Arc<dyn SwitchIo>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !queue.closed.load(AtomicOrdering::SeqCst) {
            let notified = queue.notify.notified();
            let queued = queue.take();
            match queued {
                Some(QueuedModelSwitch { request, done }) => {
                    let id = request.id.clone();
                    let mut replaced = false;
                    while !io.is_idle().await {
                        if queue.closed.load(AtomicOrdering::SeqCst) {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            MODEL_READBACK_POLL_MS,
                        ))
                        .await;
                        if queue.pending.lock().map(|p| p.is_some()).unwrap_or(false) {
                            replaced = true;
                            break;
                        }
                    }
                    if replaced || queue.pending.lock().map(|p| p.is_some()).unwrap_or(false) {
                        queue.notify.notify_one();
                        if let Some(done) = done {
                            let _ = done.send(ModelSwitchOutcome::Queued);
                        }
                        continue;
                    }
                    let outcome = perform_model_switch(request, &bridge, io.as_ref()).await;
                    match done {
                        Some(done) => {
                            let _ = done.send(outcome);
                        }
                        None => {
                            let severity = match outcome {
                                ModelSwitchOutcome::Applied | ModelSwitchOutcome::Queued => {
                                    Severity::Info
                                }
                                ModelSwitchOutcome::Degraded => Severity::Warning,
                                ModelSwitchOutcome::ControlUnavailable => Severity::Error,
                            };
                            io.journal(outcome.journal_status(&id, ""), severity).await;
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
    use crate::DriverResult;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn request_validation() {
        assert_eq!(
            ModelRequest::new("  model_hub/es1_orange_o50 ").unwrap().id,
            "model_hub/es1_orange_o50"
        );
        assert!(ModelRequest::new("").is_none());
        assert!(ModelRequest::new("   ").is_none());
        assert!(ModelRequest::new("a\nb").is_none());
        assert_eq!(
            ModelRequest::new("sonnet").unwrap().command_body(),
            "/model sonnet"
        );
    }

    #[test]
    fn bridge_resolve_and_reject() {
        let bridge = ModelBridge::new();
        let g = bridge.arm(ModelRequest::new("x").unwrap());
        bridge.resolve(g, ObservedModel { id: "y".into() });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match rt.block_on(bridge.wait(g, std::time::Duration::from_secs(1))) {
            Some(ModelReadback::Applied(o)) => assert_eq!(o.id, "y"),
            other => panic!("{other:?}"),
        }
        let g2 = bridge.arm(ModelRequest::new("bad").unwrap());
        bridge.reject(g2, "not-found");
        match rt.block_on(bridge.wait(g2, std::time::Duration::from_secs(1))) {
            Some(ModelReadback::Rejected { reason }) => assert_eq!(reason, "not-found"),
            other => panic!("{other:?}"),
        }
        assert!(bridge.pending().is_none());
    }

    #[derive(Default)]
    struct MockIo {
        writes: Mutex<Vec<String>>,
        journals: Mutex<Vec<(String, Severity)>>,
        idle: AtomicBool,
        screen: Mutex<String>,
        bridge: Mutex<Option<Arc<ModelBridge>>>,
        resolve_after: usize,
        /// When set, reject the pending generation with this reason instead
        /// of resolving (models the transcript "not found" verdict).
        reject_with: Mutex<Option<String>>,
    }

    impl MockIo {
        fn maybe_resolve(&self) {
            let n = self.writes.lock().unwrap().len();
            if self.resolve_after != 0
                && n >= self.resolve_after
                && let Some(bridge) = self.bridge.lock().unwrap().clone()
                && let Some((generation, request)) = bridge.pending_with_gen()
            {
                if let Some(reason) = self.reject_with.lock().unwrap().clone() {
                    bridge.reject(generation, reason);
                } else {
                    bridge.resolve(
                        generation,
                        ObservedModel {
                            id: request.id.clone(),
                        },
                    );
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl SwitchIo for MockIo {
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
    async fn no_dialog_on_21272_means_body_plus_one_cr() {
        let bridge = Arc::new(ModelBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 2,
            ..Default::default()
        });
        io.idle.store(true, Ordering::SeqCst);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let outcome = perform_model_switch(
            ModelRequest::new("model_hub/es1_orange_o50").unwrap(),
            &bridge,
            io.as_ref(),
        )
        .await;
        assert_eq!(outcome, ModelSwitchOutcome::Applied);
        assert_eq!(
            *io.writes.lock().unwrap(),
            vec![
                "body:/model model_hub/es1_orange_o50".to_string(),
                "cr".to_string(),
            ]
        );
        assert!(io.journals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dialog_on_21221_gets_a_confirm_cr() {
        let bridge = Arc::new(ModelBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 3,
            ..Default::default()
        });
        io.idle.store(true, Ordering::SeqCst);
        io.screen
            .lock()
            .unwrap()
            .push_str("Switch model?\n1. Yes, switch to x");
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let outcome =
            perform_model_switch(ModelRequest::new("x").unwrap(), &bridge, io.as_ref()).await;
        assert_eq!(outcome, ModelSwitchOutcome::Applied);
        assert_eq!(io.writes.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn rejected_switch_degrades_with_the_reason() {
        let bridge = Arc::new(ModelBridge::new());
        let io = Arc::new(MockIo {
            resolve_after: 2,
            reject_with: Mutex::new(Some("not-found".into())),
            ..Default::default()
        });
        io.idle.store(true, Ordering::SeqCst);
        *io.bridge.lock().unwrap() = Some(Arc::clone(&bridge));
        let outcome =
            perform_model_switch(ModelRequest::new("bogus").unwrap(), &bridge, io.as_ref()).await;
        assert_eq!(outcome, ModelSwitchOutcome::Degraded);
        assert!(
            io.journals
                .lock()
                .unwrap()
                .iter()
                .any(|(s, sev)| s.starts_with("model-degraded:bogus:not-found")
                    && *sev == Severity::Warning)
        );
    }

    #[test]
    fn arm_marks_listed_ids_and_typed_fallback() {
        let bridge = ModelBridge::new();
        bridge.set_own_catalog(Some(vec!["e2e/auto".to_owned(), "e2e/fast".to_owned()]));
        bridge.arm(ModelRequest::new("e2e/fast").unwrap());
        assert_eq!(bridge.requested_path(), Some(ModelSelectionPath::Listed));
        bridge.arm(ModelRequest::new("claude-grok-4.6").unwrap());
        assert_eq!(bridge.requested_path(), Some(ModelSelectionPath::Typed));
    }

    #[test]
    fn host_fallback_answer_makes_every_switch_typed() {
        // Only the host-fallback cache answered: the session's own list is
        // unknown, so even ids the fallback offered must go through the
        // verbatim /model path and let the verdict decide.
        let bridge = ModelBridge::new();
        bridge.set_own_catalog(None);
        bridge.arm(ModelRequest::new("claude-grok-4.6").unwrap());
        assert_eq!(bridge.requested_path(), Some(ModelSelectionPath::Typed));
    }

    #[test]
    fn resolve_records_the_effective_model() {
        let bridge = ModelBridge::new();
        bridge.note_launch_request("e2e/auto".to_owned());
        let generation = bridge.arm(ModelRequest::new("e2e/fast").unwrap());
        bridge.resolve(
            generation,
            ObservedModel {
                id: "e2e/fast".to_owned(),
            },
        );
        let (id, source, path) = bridge.last_effective().expect("effective recorded");
        assert_eq!(id, "e2e/fast");
        assert_eq!(source, EffortSource::Remuda);
        assert_eq!(path, Some(ModelSelectionPath::Typed));
    }
}
