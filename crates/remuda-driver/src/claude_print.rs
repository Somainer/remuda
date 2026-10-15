//! `claude-print` driver: materialize, spawn stream-json, map stdout to Observations.
//!
//! Node writes the journal. This driver exposes Observations on [`RunHandle`].
//! `TODO(M0-08)`: ingest the same events through `remuda-journal::Source`.

use crate::binary::BinaryPin;
use crate::capabilities::{ADAPTER_VERSION, capability_snapshot};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::materializer::{BinarySource, MaterializeRequest, SessionAction, materialize};
use crate::profile::{EnvFileSecretBroker, ProviderProfile, SecretBroker};
use crate::recipe::{EnvAllowlistSource, LaunchRecipe};
use async_trait::async_trait;
use remuda_claude_wire::{
    AssistantMessage, CanUseToolRequest, ClaudeProcess, ControlRequest, ControlRequestEnvelope,
    ControlSuccessPayload, Inbound, InitializeRequest, Outbound, PermissionResult, ProcessWriter,
    ResultMessage, StreamEventMessage, SystemInit, SystemMessage, TaskNotification, TaskProgress,
    TaskStarted, TaskUpdated, UserContent, UserMessage,
};
use remuda_protocol::hubnode::AttachmentKind;
use remuda_protocol::{
    ApprovalRequest, ClaudePermissionMode, Completeness, ContentBlock, ContentStatus, Cost,
    DecisionEffect, DecisionOption, Digest, DriverInput, DriverKind, EffectiveModel,
    EffortEffective, EffortPayload, EffortSource, EventId, HostId, Id, InputAccounting,
    InputOrigin, InstanceId, InstanceSpec, Interaction, InteractionAnswer, InteractionCarrier,
    InteractionId, InteractionKind, InteractionRequest, InteractionRequestKey, InteractionState,
    Knowledge, LifecyclePayload, LifecycleTopic, MessagePayload, MessagePhase, MessageRole,
    ModelPayload, MutationOperation, NativeLifecycle, NativeRef, NativeRequestKey,
    NativeRequestValueType, NodeMutation, Observation, ObservationPayload, ObservationSource,
    OpaqueImpact, OpaquePayload, OpaqueReason, PermissionMode, PlanReviewRequest, QuestionField,
    QuestionInput, QuestionOption, QuestionRequest, RawRef, Redaction, ResultStage, RunId,
    RuntimeCursor, SchemaVersion, Severity, SourceChannel, SourceCursor, SourceDelivery, TextBlock,
    ThoughtPayload, ThoughtRepresentation, Timestamp, ToolCallPayload, ToolCallState, ToolCategory,
    ToolOutcome, ToolResultPayload, U64, UsageMode, UsagePayload, UsageScope, WorkflowEngine,
    WorkflowPhasePayload, WorkflowRunPayload, WorkflowState,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore, TryAcquireError, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

/// How M0 answers `can_use_tool` when the host is not in the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionPolicy {
    /// `--permission-mode bypassPermissions`.
    AutoAllow,
    /// M0 `dontAsk` preset (`TD-M0-PERM-01`).
    AutoDeny,
    /// `default` + host/stdio; wait for [`Driver::respond_interaction`].
    Host,
}

/// Construction options for [`ClaudePrintDriver`].
pub struct ClaudePrintOptions {
    /// Provider profile used at materialize time.
    pub profile: ProviderProfile,
    /// Directory for 0700 launch files.
    pub launch_dir: PathBuf,
    /// Registered `CLAUDE_CONFIG_DIR`.
    pub native_home: PathBuf,
    /// Let Claude resolve its default config directory instead of exporting
    /// `CLAUDE_CONFIG_DIR`.
    ///
    /// This is opt-in because it exposes the host user's native Claude login
    /// and settings to the launched process.
    pub inherit_default_config: bool,
    /// Binary to pin and exec.
    pub binary: BinarySource,
    /// Launch origin; bot/agent cannot request bypass (D-011).
    pub origin: InputOrigin,
    /// Secret resolver.
    pub broker: Arc<dyn SecretBroker>,
    /// Extra env (tests set `FAKE_CLAUDE_SCRIPT` here).
    pub extra_env: std::collections::BTreeMap<String, String>,
    /// Hub-issued instance context; separate from ordinary environment overlays.
    pub agent_mcp: Option<crate::agent_mcp::AgentMcpContext>,
    /// Override `--setting-sources`.
    pub setting_sources: Option<Vec<String>>,
    /// Node-supplied stager for image content blocks a tool result carries
    /// (D-045 §6.2). `None` (default) degrades images to text blocks; bytes
    /// are never inlined into observations.
    pub media_stager: Option<Arc<dyn remuda_protocol::ToolMediaStager>>,
    /// Initialize handshake timeout.
    pub handshake_timeout: Duration,
    /// Total bound for [`Driver::close`]. The ladder spends it in two equal
    /// slices — stdin EOF with a bounded wait, then SIGTERM to the process group
    /// with a bounded wait — before SIGKILL and a short fixed reap (§2.3).
    ///
    /// Stdin EOF is a request, not a guarantee: a child mid-turn, blocked on an
    /// unanswered `can_use_tool`, or one that stopped draining stdin, can ignore
    /// it indefinitely. An unbounded close would hang forever on the long-lived
    /// sdk carrier, so the EOF request *and* every wait are inside this bound.
    pub close_timeout: Duration,
    /// Host-validated `--settings` overlay. Contents are never logged.
    pub settings_overlay_path: Option<PathBuf>,
}

impl ClaudePrintOptions {
    /// Print driver with unknown (Agent) origin and the env/file broker.
    pub fn new(
        profile: ProviderProfile,
        launch_dir: PathBuf,
        native_home: PathBuf,
        binary: BinarySource,
    ) -> Self {
        Self {
            profile,
            launch_dir,
            native_home,
            inherit_default_config: false,
            binary,
            origin: InputOrigin::Agent,
            broker: Arc::new(EnvFileSecretBroker::env_only()),
            extra_env: std::collections::BTreeMap::new(),
            agent_mcp: None,
            setting_sources: None,
            handshake_timeout: Duration::from_secs(30),
            close_timeout: Duration::from_secs(10),
            settings_overlay_path: None,
            media_stager: None,
        }
    }
}

struct PendingTool {
    request_id: String,
    input: Value,
}

/// Which locally-known root turn a background workflow belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkflowOwner {
    /// The workflow was seen with no outstanding locally-written turn (pure
    /// fixture/transcript replay, or a frame reordered ahead of its start).
    Implicit,
    /// The outstanding root turn with this seq.
    Turn(u64),
}

/// One root turn the mapper has evidence for.
struct RootTurn {
    /// Monotonic identity.
    seq: u64,
    /// `local_workflow` task ids this turn opened whose terminal
    /// `task_notification` has not been mapped yet. The turn cannot settle
    /// while this is non-empty, no matter what `result_index` says.
    open_workflows: HashSet<String>,
}

/// Per-root-turn settlement evidence (D-057 OA6 ma-sdk-state r3).
///
/// Root-turn settlement is derived from evidence PER ROOT TURN, never from
/// the process-global `result_index` (which grows across turns on the
/// long-lived sdk child) or one process-global counter:
///
/// * every locally-written user frame opens a [`RootTurn`] (FIFO); the front
///   is the turn the native process is currently emitting frames for;
/// * a workflow is tracked by its TASK ID against the turn that owned it when
///   `task_started` arrived, and is closed by its own terminal
///   `task_notification` (completed/failed/stopped) — not by decrementing a
///   counter on a later result;
/// * a `result` completes the front turn only once all of THAT turn's
///   workflows are closed, and settles the root only when no newer locally
///   written turn is outstanding, so an older buffered result can never settle
///   a newer input.
#[derive(Default)]
struct TurnBook {
    /// Locally-written root turns not yet completed, in write order.
    outstanding: VecDeque<RootTurn>,
    /// Workflow task id -> owning turn.
    workflow_owner: HashMap<String, WorkflowOwner>,
    /// Workflows seen with no locally-known owning turn (replay bucket).
    implicit_workflows: HashSet<String>,
    /// Seq of the turn whose prompt write is in flight (book opened before
    /// the write, ma-sdk-state r5 item 1). The reservation lives IN the
    /// publication queue as a `PublishJob::Reserve` ticket enqueued before
    /// the write: the worker parks at that ticket's position and emits the
    /// turn_started there, so every frame enqueued after it (the turn's own
    /// result included) is published behind its start. `None` once the write
    /// commits or is rolled back.
    reserved: Option<u64>,
    /// ma-sdk-state r6 item 5: a completed turn's settle is deferred while a
    /// NEWER locally-written turn is still outstanding (its result popped but
    /// did not settle the root). If that newer turn's write later fails and it
    /// is rolled back, the deferred settle is released so the root idles
    /// instead of staying working forever.
    deferred_settle: bool,
    next_seq: u64,
}

impl TurnBook {
    fn begin_turn(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.reserved = Some(seq);
        self.outstanding.push_back(RootTurn {
            seq,
            open_workflows: HashSet::new(),
        });
        seq
    }

    /// The reserved turn's write committed. Clear `reserved` AT commit (the
    /// worker does this from the ticket's queue position) so a later turn never
    /// reads a stale reservation for a turn already started. Conditional on the
    /// committing seq: if a NEWER turn has already begun (its reservation now in
    /// flight), clearing the older ticket must not clobber that newer one.
    fn turn_committed(&mut self, seq: u64) {
        if self.reserved == Some(seq) {
            self.reserved = None;
        }
    }

    /// Roll back the reservation for turn `seq` when its prompt write failed
    /// (ma-sdk-state r6 item 1; r7 item 1; r10 item 2). The prompt never
    /// reached the child, so that turn is still empty. Remove the entry
    /// carrying `seq` WHEREVER it sits in the FIFO (not only the back): a
    /// write-failed turn has produced no frames and owns no workflows, so it is
    /// always safe to pull it out, and cancelling only a matching back entry
    /// could leave a phantom turn ahead of a later reservation. Clear
    /// `reserved` only when it names `seq`.
    ///
    /// Returns true when this cancellation RELEASED a deferred root settle
    /// (r6 item 5; r10 item 2): an earlier completed turn's result is owed an
    /// idle. The release fires whenever a recorded deferral exists and the
    /// removed turn was outstanding — NOT only when the book drains. This runs
    /// on the publication worker AT THE CANCELLED TICKET'S POSITION: every
    /// NEWER turn's ticket (and therefore its turn_started) is still queued
    /// behind it, so the older turn's idle belongs here even when a newer turn
    /// is already in the book (its start will follow the correction). The
    /// stashed result is `take`n by the first release, so it is published
    /// exactly once; later cancellations release nothing.
    ///
    /// ma-sdk-state r11 item 1: the release additionally requires that NO
    /// outstanding turn OLDER than the cancelled one remains. The deferral is
    /// owed while a turn that was outstanding when the result was deferred is
    /// still active; cancelling a turn NEWER than that active blocker (reserve
    /// order A committed, B committed, A's result deferred behind B, then C
    /// cancelled) must not emit the idle while B is still mid-turn. Newer
    /// reservations queued behind the cancelled ticket may proceed — their
    /// starts follow this ticket's position in publication order.
    fn cancel_turn(&mut self, seq: u64) -> bool {
        let removed = if let Some(pos) = self.outstanding.iter().position(|turn| turn.seq == seq) {
            let turn = self.outstanding.remove(pos).expect("positioned turn");
            for id in turn.open_workflows {
                self.workflow_owner.remove(&id);
            }
            Some(())
        } else {
            None
        };
        if self.reserved == Some(seq) {
            self.reserved = None;
        }
        if removed.is_some() && self.deferred_settle {
            // An older turn still outstanding keeps the root working: the
            // cancelled turn was NOT the deferral's blocker. Keep the deferral
            // for that older turn's completion or its own later rollback.
            if self.outstanding.iter().any(|turn| turn.seq < seq) {
                return false;
            }
            self.deferred_settle = false;
            true
        } else {
            false
        }
    }

    /// Track a freshly-opened background workflow against the root turn the
    /// native process is currently emitting for (the FIFO front), or the
    /// implicit bucket when no local turn is known (fixture replay).
    fn workflow_started(&mut self, task_id: &str) {
        if task_id.is_empty() || self.workflow_owner.contains_key(task_id) {
            return;
        }
        let owner = match self.outstanding.front_mut() {
            Some(front) => {
                front.open_workflows.insert(task_id.to_owned());
                WorkflowOwner::Turn(front.seq)
            }
            None => {
                self.implicit_workflows.insert(task_id.to_owned());
                WorkflowOwner::Implicit
            }
        };
        self.workflow_owner.insert(task_id.to_owned(), owner);
    }

    /// Close a workflow on its own terminal evidence (`task_notification`
    /// completed/failed/stopped, or a terminal `task_updated` patch), removing
    /// it from the OWNING turn's open set.
    fn workflow_terminated(&mut self, task_id: &str) {
        let Some(owner) = self.workflow_owner.remove(task_id) else {
            return;
        };
        match owner {
            WorkflowOwner::Implicit => {
                self.implicit_workflows.remove(task_id);
            }
            WorkflowOwner::Turn(seq) => {
                if let Some(turn) = self.outstanding.iter_mut().find(|turn| turn.seq == seq) {
                    turn.open_workflows.remove(task_id);
                }
            }
        }
    }
}

/// The effect of mapping one `result` frame on the root-turn book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootSettle {
    /// This result settles the root (the completed turn is popped and nothing
    /// newer/outstanding gates it): publish it with `settledRootTurn`.
    Settled,
    /// The completed turn was popped but the root stays open because a NEWER
    /// locally-written turn is outstanding. The settle is recorded as deferred;
    /// it is released only if that newer turn is rolled back.
    Deferred,
    /// The front turn's workflows are still open, or a native follow-up turn is
    /// queued, or there is no local turn: the root keeps working.
    NotSettled,
}

impl TurnBook {
    /// Whether a `result` frame SETTLES THE ROOT TURN.
    ///
    /// `queued` is the native `queued_turn_count` (0/omitted = nothing queued).
    /// A result with one of the front turn's workflows still open is that
    /// turn's intermediate result: the turn is NOT popped (a workflow turn
    /// emits result 0 then result 1 for the SAME user frame). Otherwise the
    /// front turn completes here; the root settles only when no newer locally
    /// written turn remains outstanding.
    fn result_settles_root(&mut self, queued: u64) -> RootSettle {
        let front_open = self
            .outstanding
            .front()
            .is_some_and(|front| !front.open_workflows.is_empty());
        if front_open {
            return RootSettle::NotSettled;
        }
        if self.outstanding.pop_front().is_some() {
            // Defense in depth: workflows observed before any locally-written
            // turn (pure replay/hydration) live in the implicit bucket; they
            // still gate settlement even when a synthetic turn is popped.
            if queued != 0 || !self.implicit_workflows.is_empty() {
                // r7 item 4: a native queued turn / open workflow owns the next
                // settle — external state the book cannot track — so ALWAYS drop
                // any local deferral here (whether or not a newer local turn is
                // outstanding). Otherwise a later write failure would synthesise
                // settledRootTurn while that native turn is still in flight. The
                // native turn's own result delivers the settle in order.
                self.deferred_settle = false;
                RootSettle::NotSettled
            } else if self.outstanding.is_empty() {
                // Any prior deferral is now superseded by a real settle.
                self.deferred_settle = false;
                RootSettle::Settled
            } else {
                // The turn completed but a newer locally-written turn is still
                // outstanding; remember that its settle is owed (r6 item 5).
                self.deferred_settle = true;
                RootSettle::Deferred
            }
        } else if queued == 0 && self.implicit_workflows.is_empty() {
            // No locally-known turn (pure replay): native queue evidence plus
            // the implicit workflow bucket.
            RootSettle::Settled
        } else {
            RootSettle::NotSettled
        }
    }
}

/// Terminal native workflow statuses: the task will produce no further frames
/// (`system/task_notification` status, or a terminal `task_updated` patch).
fn workflow_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "stopped" | "killed" | "cancelled"
    )
}

struct Live {
    process: ClaudeProcess,
    pending: HashMap<InteractionId, PendingTool>,
    pending_by_native: HashMap<String, InteractionId>,
    recipe: LaunchRecipe,
}

#[path = "claude_print_stream.rs"]
mod stream;

#[path = "claude_transcript_records.rs"]
mod records;

struct Mapper {
    stream: stream::StreamState,
    ids: NativeIds,
    seq: u64,
    instance_id: InstanceId,
    run_id: RunId,
    journal_id: Id,
    host_id: HostId,
    session_id: String,
    pin: BinaryPin,
    /// Driver stamped on every Observation. `claude-print` for the live reader;
    /// `shell-pty` when a promoted terminal replays the same records (D-025).
    driver_kind: DriverKind,
    /// Source channel for the same reason: `stdout` live, `transcript` on replay.
    channel: SourceChannel,
    /// Stages image result bytes into the Hub object store (D-045 §6.2).
    /// `None` carriers degrade every image to an honest text block.
    media_stager: Option<Arc<dyn remuda_protocol::ToolMediaStager>>,
    /// D-057 OA6 (ma-sdk-state r3): per-root-turn settlement evidence. See
    /// [`TurnBook`] for why this is per-turn workflow identities plus a FIFO of
    /// locally-written turns rather than a process-global counter or
    /// `result_index`.
    turns: TurnBook,
    /// True once this run emitted per-call usage from an assistant /
    /// message_delta frame. When set, a `result` frame's summed `usage` is
    /// NOT re-emitted as a Turn snapshot (c-usagefu: per-call frames are the
    /// authority; the result would double count); a result on a stream that
    /// produced no per-call usage (e.g. a bare print result) still emits its
    /// tokens so the TPM window folds them (c-ctxusage).
    saw_per_call_usage: bool,
    /// ma-sdk-state r6 item 5: status/error text of the most recent completed
    /// turn whose settle was deferred behind a newer outstanding turn, used to
    /// emit the deferred settle if that newer turn's write fails.
    deferred_result: Option<DeferredResult>,
}

/// The shape of a completed turn whose root settle is deferred (r6 item 5).
#[derive(Clone)]
struct DeferredResult {
    status: &'static str,
    last_error: Option<String>,
}

#[derive(Default)]
struct NativeIds {
    messages: HashMap<String, (Id, u64)>,
    tools: HashMap<String, Id>,
    workflows: HashMap<String, Id>,
    phases: HashMap<String, Id>,
    /// One result node per native tool id, keyed `tool-result:{native}`. A
    /// tool result is its own node — joined to the call via `tool_call_id`,
    /// not a mutation of the call node — so a transcript replay emits the same
    /// open→(replace) sequence the live stream does (promotion parity,
    /// D-025/D-028). The first record opens it at revision 1; a background
    /// subagent's later `<task-notification>` replaces it at revision 5, which
    /// outranks the hook relay's SubagentStop (revision 4) because the
    /// notification carries the authoritative status (killed/failed).
    tool_results: HashMap<String, (Id, u64)>,
    /// Instance id scope for [`Id::derive`], set when the mapper knows it.
    scope: Option<String>,
}

impl NativeIds {
    fn message(&mut self, native: &str) -> DriverResult<(Id, U64, MutationOperation)> {
        if let Some((id, rev)) = self.messages.get_mut(native) {
            *rev += 1;
            return Ok((id.clone(), U64(*rev), MutationOperation::Append));
        }
        let id = Id::new("obj")?;
        self.messages.insert(native.to_owned(), (id.clone(), 1));
        Ok((id, U64(1), MutationOperation::Open))
    }

    fn tool(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.tools.get(native) {
            return Ok(id.clone());
        }
        // Deterministic on (instance scope, native tool_use_id) so the hook
        // relay's Pre/PostToolUse observations and this transcript replay fold
        // onto one node (live-view design §2.3); random ids would draw a second
        // card whenever both channels are present (promoted shell-pty).
        let id = match &self.scope {
            Some(scope) => Id::derive("obj", scope, native)?,
            None => Id::new("obj")?,
        };
        self.tools.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    /// The result node for a native tool id and its opening mutation: a fresh
    /// node opened at revision 1 on first use. The call is joined via
    /// `tool_call_id`, never mutated.
    fn tool_result_open(&mut self, native: &str) -> DriverResult<(Id, U64, MutationOperation)> {
        if let Some((id, rev)) = self.tool_results.get_mut(native) {
            *rev += 1;
            return Ok((id.clone(), U64(*rev), MutationOperation::Replace));
        }
        let id = match &self.scope {
            Some(scope) => Id::derive("obj", scope, &format!("tool-result:{native}"))?,
            None => Id::derive("obj", "stdout-tool-result", native)?,
        };
        self.tool_results.insert(native.to_owned(), (id.clone(), 1));
        Ok((id, U64(1), MutationOperation::Open))
    }

    /// The result node id for a native tool id (creating an unopened entry so
    /// a completion can replace a launch result even if observed first).
    fn tool_result_id(&mut self, native: &str) -> DriverResult<Id> {
        if let Some((id, _)) = self.tool_results.get(native) {
            return Ok(id.clone());
        }
        let id = match &self.scope {
            Some(scope) => Id::derive("obj", scope, &format!("tool-result:{native}"))?,
            None => Id::derive("obj", "stdout-tool-result", native)?,
        };
        self.tool_results.insert(native.to_owned(), (id.clone(), 1));
        Ok(id)
    }

    /// Build id tables whose tool ids derive deterministically from the instance
    /// scope, matching the hook relay's `Id::derive("obj", instance, native)`
    /// (live-view design §2.3).
    fn scoped(scope: impl Into<String>) -> Self {
        Self {
            scope: Some(scope.into()),
            ..Self::default()
        }
    }

    fn workflow(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.workflows.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.workflows.insert(native.to_owned(), id.clone());
        Ok(id)
    }

    fn phase(&mut self, native: &str) -> DriverResult<Id> {
        if let Some(id) = self.phases.get(native) {
            return Ok(id.clone());
        }
        let id = Id::new("obj")?;
        self.phases.insert(native.to_owned(), id.clone());
        Ok(id)
    }
}

/// Per-launch observation channel state behind [`Launch::emission`]
/// (ma-sdk-state r14 item 1, directive (d)).
struct Emission {
    /// The bounded observation channel for the live launch (`None` once close
    /// sealed it).
    tx: Option<mpsc::Sender<Observation>>,
    /// Set ONLY while holding the emission mutex, immediately before the
    /// terminal observation is pushed through its reserved slot. An ordinary
    /// send that reserved consumer capacity outside the mutex rechecks this
    /// seal under the mutex and abandons the reserved slot, so the terminal is
    /// the last observation every consumer can receive. Reset per launch.
    terminal_sent: bool,
}

/// Everything that belongs to ONE launch and must never be reachable from a
/// task outliving that launch (ma-sdk-state r15 item 1).
///
/// The reader, the publication worker and the detached overflow-shutdown task
/// all hold an [`Arc<Launch>`] for THEIR launch. The shutdown therefore kills
/// a captured pid and seals/emits on this object — it never reads the driver's
/// current `live` slot, so a launch-A shutdown delayed past `close(A)` and
/// `start(B)` cannot kill B's child. `close` takes the driver's launch slot
/// and finishes (bounded join, then abort+join) the tracked shutdown before
/// its own kill ladder runs, and a shutdown that races close either registers
/// (and is finished) or sees the launch already closing and never spawns.
struct Launch {
    /// This launch's publication FIFO behind its admission mutex, created
    /// fresh per launch. `Arc`-wrapped so the test publish hold can take an
    /// owned guard; production callers lock through the launch.
    publish: Arc<tokio::sync::Mutex<Option<PublishQueue>>>,
    /// Observation channel for THIS launch plus its terminal seal, behind one
    /// emission mutex (r14 directive (d)).
    emission: tokio::sync::Mutex<Emission>,
    /// ONE channel slot reserved for the terminal event for this launch; the
    /// overflow shutdown and close both deliver through it past a full
    /// consumer channel.
    exit_permit: Mutex<Option<mpsc::OwnedPermit<Observation>>>,
    /// Admission seal for THIS launch (natural stdout EOF, overflow, close).
    closed: AtomicBool,
    /// Whether this launch's terminal was already emitted.
    exit_emitted: AtomicBool,
    /// The launched child's pid — which `spawn_command` makes its own process
    /// group id — captured at launch so the detached overflow shutdown can
    /// signal the group and its leader without ever looking at the driver's
    /// current `live` slot. `None` only for the in-crate publication tests
    /// that drive no child.
    kill_target: Option<i32>,
    /// The detached overflow-shutdown task, if one spawned. Registration and
    /// close's claim are serialized on this mutex: either the spawn observed
    /// `closed == false` and registered (close finishes it) or close sealed
    /// first and the spawn is dropped (close's own ladder owns the kill).
    shutdown: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// Shared handle to the driver's test barrier slot (test-stub): the
    /// emission path runs from launch-owned tasks, so the launch needs the
    /// same slot `arm_write_commit_barrier` arms. It is an `Arc` to the
    /// driver-scoped slot rather than a copy, so (re)arming after a launch
    /// still reaches the task in flight.
    #[cfg(feature = "test-stub")]
    barrier: test_barrier::Slot,
}

impl Launch {
    fn new(
        publish_queue: PublishQueue,
        tx: Option<mpsc::Sender<Observation>>,
        exit_permit: Option<mpsc::OwnedPermit<Observation>>,
        kill_target: Option<i32>,
        #[cfg(feature = "test-stub")] barrier: test_barrier::Slot,
    ) -> Arc<Self> {
        Arc::new(Self {
            publish: Arc::new(tokio::sync::Mutex::new(Some(publish_queue))),
            emission: tokio::sync::Mutex::new(Emission {
                tx,
                terminal_sent: false,
            }),
            exit_permit: Mutex::new(exit_permit),
            closed: AtomicBool::new(false),
            exit_emitted: AtomicBool::new(false),
            kill_target,
            shutdown: std::sync::Mutex::new(None),
            #[cfg(feature = "test-stub")]
            barrier,
        })
    }
}

/// Shared per-launch driver state. Public only for the `test-stub`
/// Weak-based drop-leak seam; not part of the stable driver API.
pub struct Inner {
    live: Mutex<Option<Live>>,
    mapper: Mutex<Mapper>,
    /// D-057 OA6 r3: publication FIFO. Every stdout frame's map+emit and every
    /// locally-written turn's reservation ticket + `turn_started` map+emit goes
    /// through ONE queue drained by a single worker, so the published order is
    /// the causal order (a turn's frames can never precede its start) while
    /// neither task holds a lock across the other's work. The underlying channel
    /// stays UNBOUNDED so the reader never parks on a queue send (a blocked
    /// prompt write cannot stop stdout draining), but FRAME admission and
    /// RESERVE-ticket admission take OWNED permits from the queue's PER-LAUNCH
    /// [`BacklogBudget`] (item + byte semaphores) before the job becomes
    /// visible. Every admitted job CARRIES its permits, so a job discarded
    /// (refused send), dropped (worker aborted with queued residue), cancelled
    /// or completed releases its charge exactly once by Drop — there are no
    /// shared counters to race or reset. The budget is created fresh per
    /// launch, so nothing from launch A's tasks can touch launch B's accounts.
    /// The terminal EOF and test fences bypass the caps, so natural-EOF sealing
    /// and close never depend on admission; cancelling a queued reservation
    /// enqueues nothing and is independent of admission too. The reader always
    /// keeps draining stdout into the bounded backlog even while a stdin write
    /// is blocked, and `close`/`cancel` stay reachable (neither needs this queue
    /// to make progress).
    ///
    /// The CURRENT launch's scoped state: that publication FIFO and its
    /// per-launch budget handles, the emission channel/seal, admission seal,
    /// captured kill target, and the detached overflow-shutdown task. `close`
    /// takes the slot and every launch task owns its own [`Arc<Launch>`], so
    /// launch A's tasks can never read or write launch B's accounts, channel,
    /// or live process (ma-sdk-state r15 item 1). The mapper stays
    /// driver-scoped but is reset wholesale at every launch.
    launch: tokio::sync::Mutex<Option<Arc<Launch>>>,
    /// Serializes prompt sends: each send enqueues its reservation ticket,
    /// opens the mapper book, awaits its write, and commits strictly one after
    /// another, so two concurrent prompts reserve, write and start in FIFO
    /// order. Held across the write await by design; `close`/`cancel` do not
    /// take it, so a blocked prompt cannot gate them.
    write_seq: tokio::sync::Mutex<()>,
    policy: Mutex<PermissionPolicy>,
    /// Last successfully launched spec; `resume` re-materializes from this.
    last_spec: Mutex<Option<InstanceSpec>>,
    /// The real OS exit status of the most recently reaped child, captured for
    /// tests that must assert the spawned peer exited cleanly. Production code
    /// does not read this.
    #[cfg(any(test, feature = "test-stub"))]
    child_exit_status: Mutex<Option<std::process::ExitStatus>>,
    /// ma-sdk-state r6 item 2: per-DRIVER write-commit barrier slot, so two
    /// tests driving different children in one process never share a barrier.
    #[cfg(feature = "test-stub")]
    barrier: test_barrier::Slot,
    /// ma-sdk-state r6 item 4: test fault seam — make the terminal exit
    /// observation build in `emit_exit` fail AFTER the reserved permit has been
    /// taken, so a test can prove the RunHandle still reaches EOF on close.
    #[cfg(feature = "test-stub")]
    exit_build_fails: std::sync::atomic::AtomicBool,
    /// r7 items 2/5: 1-based write number from which the writer call is forced
    /// to fail (test-stub) WITHOUT touching the live child — a deterministic
    /// write failure the ordered cleanup/fail path runs on. usize::MAX = off.
    #[cfg(feature = "test-stub")]
    fail_from_write: std::sync::atomic::AtomicUsize,
    /// Count of writer calls made by `drive_prompt_write` (test-stub), pairing
    /// with `fail_from_write`.
    #[cfg(feature = "test-stub")]
    write_number: std::sync::atomic::AtomicUsize,
    /// r11 item 3 (test-stub): make the Reserve ticket send fail WITHOUT a
    /// closed channel, driving prepare_prompt_write into fail_without_ticket
    /// deterministically while the live child stays up.
    #[cfg(feature = "test-stub")]
    reserve_send_fails: std::sync::atomic::AtomicBool,
    /// ma-sdk-state r12/14: configured caps for the bounded publication
    /// backlog. Driver-lifetime configuration (tests shrink the caps before
    /// `start`); the ACTIVE accounting is the per-launch [`BacklogBudget`] the
    /// launch builds from these values.
    backlog_config: Arc<BacklogConfig>,
    /// The per-launch budget currently accounting for the publication FIFO, so
    /// the test snapshot reads the launch that owns the live queue. Set at
    /// launch; the budget also lives inside the launch's [`PublishQueue`] and
    /// every charge's permits keep it alive independently.
    active_budget: std::sync::Mutex<Option<Arc<BacklogBudget>>>,
    /// The terminal-evidence latch of the LAST finished launch, copied out by
    /// `close` when it takes the launch slot (r15 item 1 moved the live latch
    /// onto [`Launch`]): callers asking `process_gone` after close (no launch
    /// installed) still get the dead-child answer the r5 contract requires.
    /// Reset to false by the next launch.
    last_process_gone: AtomicBool,
}

/// Build the shared per-launch [`Inner`]. Factored out of the driver
/// constructor so in-crate tests can drive the publication/emission machinery
/// without a child process.
fn build_inner(
    media_stager: Option<Arc<dyn remuda_protocol::ToolMediaStager>>,
    carrier: DriverKind,
) -> Arc<Inner> {
    Arc::new(Inner {
        live: Mutex::new(None),
        mapper: Mutex::new(Mapper {
            stream: stream::StreamState::default(),
            ids: NativeIds::default(),
            seq: 0,
            instance_id: InstanceId::new(),
            run_id: RunId::new(),
            journal_id: fallback_obj(),
            host_id: fallback_host(),
            session_id: String::new(),
            pin: BinaryPin {
                abs_path: String::new(),
                version: String::new(),
                sha256: dummy_digest(),
            },
            driver_kind: carrier,
            channel: SourceChannel::Stdout,
            media_stager,
            turns: TurnBook::default(),
            saw_per_call_usage: false,
            deferred_result: None,
        }),
        launch: tokio::sync::Mutex::new(None),
        write_seq: tokio::sync::Mutex::new(()),
        policy: Mutex::new(PermissionPolicy::Host),
        last_spec: Mutex::new(None),
        #[cfg(any(test, feature = "test-stub"))]
        child_exit_status: Mutex::new(None),
        #[cfg(feature = "test-stub")]
        barrier: Arc::new(std::sync::Mutex::new(None)),
        #[cfg(feature = "test-stub")]
        exit_build_fails: std::sync::atomic::AtomicBool::new(false),
        #[cfg(feature = "test-stub")]
        fail_from_write: std::sync::atomic::AtomicUsize::new(usize::MAX),
        #[cfg(feature = "test-stub")]
        write_number: std::sync::atomic::AtomicUsize::new(0),
        #[cfg(feature = "test-stub")]
        reserve_send_fails: std::sync::atomic::AtomicBool::new(false),
        backlog_config: BacklogConfig::new(),
        active_budget: std::sync::Mutex::new(None),
        last_process_gone: AtomicBool::new(false),
    })
}

/// Native Claude print driver (`claude -p` stream-json).
pub struct ClaudePrintDriver {
    options: ClaudePrintOptions,
    inner: Arc<Inner>,
    reader: Mutex<Option<JoinHandle<()>>>,
    /// Drain task for [`Inner::publish`], joined/aborted by `close` after the
    /// reader so the queued publication order always flushes before `exited`.
    publisher: Mutex<Option<JoinHandle<()>>>,
    /// Carrier this object launches and stamps.
    ///
    /// `claude-print` and `claude-sdk` are the same transport (stream-json over
    /// stdio, same handshake, same mapper) differing by one launch flag, so
    /// `claude_sdk.rs` drives this engine with [`DriverKind::ClaudeSdk`] rather
    /// than forking ~1000 lines of mapper (`print-replacement.md` §2.5, batch 2).
    carrier: DriverKind,
}

/// Test-only (`test-stub`): held publication-mutex guard (r10 item 1).
///
/// The lock belongs to the CURRENT launch (r15 item 1 moved it onto
/// [`Launch`]), so the hold owns the launch Arc and an OWNED guard: the held
/// mutex then cannot be replaced by a relaunch while the test parks a sender
/// on it. The fields exist solely to keep the lock held until this handle
/// drops.
#[cfg(feature = "test-stub")]
#[must_use = "holding the guard is the point; drop it to release the mutex"]
#[allow(dead_code)]
pub struct PublishMutexHold {
    _launch: Arc<Launch>,
    guard: tokio::sync::OwnedMutexGuard<Option<PublishQueue>>,
}

/// Test-only (`test-stub`): held `write_seq` guard (r10 item 3).
/// The field exists solely to keep the lock held until this handle drops.
#[cfg(feature = "test-stub")]
#[must_use = "holding the guard is the point; drop it to release the lock"]
#[allow(dead_code)]
pub struct WriteSeqHold<'a>(tokio::sync::MutexGuard<'a, ()>);

/// Test-only (`test-stub`): held mapper-lock guard (r11 item 3). The field
/// exists solely to keep the mapper lock held so a no-ticket rollback parks
/// on it while a test aborts the cancelling caller.
#[cfg(feature = "test-stub")]
#[must_use = "holding the guard is the point; drop it to release the lock"]
#[allow(dead_code)]
pub struct MapperMutexHold<'a>(tokio::sync::MutexGuard<'a, Mapper>);

/// Test-only (`test-stub`): held `live` process handle guard (r13 item 3).
/// Keeps the process lock held so the overflow shutdown's GUARANTEED kill
/// contends with the test holder; dropping the handle lets that kill proceed.
#[cfg(feature = "test-stub")]
#[must_use = "holding the guard is the point; drop it to release the lock"]
#[allow(dead_code)]
pub struct LiveMutexHold<'a>(tokio::sync::MutexGuard<'a, Option<Live>>);

impl ClaudePrintDriver {
    /// Test-only (`test-stub`): true when the bounded observation channel is
    /// full and the publication worker is parked — the close-under-pressure
    /// test uses it to reach the saturated state deterministically.
    #[cfg(feature = "test-stub")]
    pub async fn publication_is_saturated(&self) -> bool {
        // The exit permit permanently holds one of 256 slots; capacity is
        // therefore 255.
        let Some(launch) = self.inner.launch.lock().await.clone() else {
            return false;
        };
        let Some(tx) = launch.emission.lock().await.tx.clone() else {
            return false;
        };
        tx.capacity() == 0
    }

    /// Test-only (`test-stub`): arm the write-commit publication barrier on
    /// THIS driver. The barrier is scoped to the driver's own publication slot,
    /// so tests driving separate children in one binary never interfere
    /// (ma-sdk-state r6 item 2).
    #[cfg(feature = "test-stub")]
    #[must_use]
    pub fn arm_write_commit_barrier(&self) -> test_barrier::WriteCommitBarrier {
        test_barrier::arm(&self.inner.barrier)
    }

    /// Test-only (`test-stub`, r9 item 3; r12 item 3): a handle on the real
    /// writer task's in-window gate (closes UserWrite inside the writer, after
    /// FIFO accept and before the write/ack). The `live` mutex acquisition is
    /// bounded by a NAMED wait: a stuck launch must fail the test at this
    /// helper's message, not hang the binary somewhere before the assertion.
    #[cfg(feature = "test-stub")]
    #[must_use]
    pub async fn bounded_write_window(
        &self,
        timeout: Duration,
    ) -> remuda_claude_wire::write_gate::WriteGate {
        tokio::time::timeout(timeout, async {
            let live = self.inner.live.lock().await;
            live.as_ref()
                .expect("test access: live process")
                .process
                .write_gate_for_test()
        })
        .await
        .expect("bounded test wait timed out: acquiring live to open the writer window")
    }

    /// Test-only (`test-stub`, r9 item 4): enqueue an ordered publication fence
    /// and return its receiver, which resolves once every earlier publication
    /// (including a deferred settle enqueued ahead of the fence) is drained.
    #[cfg(feature = "test-stub")]
    async fn enqueue_publication_fence(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Some(launch) = self.inner.launch.lock().await.clone()
            && let Some(publisher) = launch.publish.lock().await.clone()
        {
            let _ = publisher.admit_control(PublishJob::Fence(tx));
        }
        rx
    }

    /// Test-only (`test-stub`, r9 item 4; r12 item 3): the
    /// [`Self::enqueue_publication_fence`] acquisition through its NAMED
    /// bounded wait (the enqueue takes the publish mutex). Shared by every
    /// scheduling test and by the bound proofs, so removing the bound here is
    /// the single RED switch for this wait class.
    #[cfg(feature = "test-stub")]
    pub async fn bounded_publication_fence(
        &self,
        timeout: Duration,
    ) -> tokio::sync::oneshot::Receiver<()> {
        tokio::time::timeout(timeout, self.enqueue_publication_fence())
            .await
            .expect("bounded test wait timed out: enqueuing a publication fence (publish mutex)")
    }

    /// Test-only (`test-stub`, r10 item 1; r12 item 3): hold the publication
    /// mutex so a concurrent send parks on it BEFORE opening its book entry.
    /// Bounded by a NAMED wait. Drop the returned hold to release.
    #[cfg(feature = "test-stub")]
    pub async fn bounded_hold_publish_mutex_for_test(&self, timeout: Duration) -> PublishMutexHold {
        tokio::time::timeout(timeout, async {
            // Pin the CURRENT launch: the hold parks a sender of THIS launch
            // on its own admission mutex and survives a driver relaunch.
            let launch_slot = self.inner.launch.lock().await;
            let launch = launch_slot
                .as_ref()
                .expect("test access: a launch is installed")
                .clone();
            drop(launch_slot);
            let mutex = Arc::clone(&launch.publish);
            PublishMutexHold {
                _launch: launch,
                guard: mutex.lock_owned().await,
            }
        })
        .await
        .expect("bounded test wait timed out: acquiring the publication mutex hold")
    }

    /// Test-only (`test-stub`, r10 item 3; r11 item 4b; r12 item 3): hold the
    /// `write_seq` lock directly, so a test can deterministically queue a stale
    /// cleanup BEHIND a later send and prove ordering. Notes a write_seq wait
    /// like the send path, so a test can ACKNOWLEDGE this holder parked behind
    /// queued sends (count includes the holder) before it aborts the suspended
    /// send. The lock acquisition runs under a NAMED bounded wait. Drop the
    /// returned hold to release.
    #[cfg(feature = "test-stub")]
    pub async fn bounded_hold_write_seq_for_test(&self, timeout: Duration) -> WriteSeqHold<'_> {
        tokio::time::timeout(timeout, async {
            test_barrier::note_write_seq_wait(&self.inner.barrier);
            WriteSeqHold(self.inner.write_seq.lock().await)
        })
        .await
        .expect("bounded test wait timed out: acquiring the write_seq hold")
    }

    /// Test-only (`test-stub`, r11 item 3; r12 item 3): hold the mapper lock
    /// directly, so a fail_without_ticket rollback parks deterministically on
    /// its lock await and a test can abort the caller there. Bounded by a
    /// NAMED wait. Drop the hold to release.
    #[cfg(feature = "test-stub")]
    pub async fn bounded_hold_mapper_for_test(&self, timeout: Duration) -> MapperMutexHold<'_> {
        tokio::time::timeout(timeout, async {
            MapperMutexHold(self.inner.mapper.lock().await)
        })
        .await
        .expect("bounded test wait timed out: acquiring the mapper lock hold")
    }

    /// Test-only (`test-stub`, r13 item 3): hold the `live` process handle
    /// lock, so an overflow shutdown provably contends with a holder and must
    /// still guarantee the kill after the holder drops. Bounded by a NAMED
    /// wait. Drop the returned hold to release.
    #[cfg(feature = "test-stub")]
    pub async fn bounded_hold_live_for_test(&self, timeout: Duration) -> LiveMutexHold<'_> {
        tokio::time::timeout(timeout, async {
            LiveMutexHold(self.inner.live.lock().await)
        })
        .await
        .expect("bounded test wait timed out: acquiring the live process hold")
    }

    /// Test-only (`test-stub`, r11 item 3; r12 item 3): the seqs of the turns
    /// the mapper book currently holds outstanding. A cleanup that orphaned a
    /// reservation leaves its turn here forever. The mapper inspection runs
    /// under a NAMED bounded wait.
    #[cfg(feature = "test-stub")]
    pub async fn bounded_outstanding_turn_seqs_for_test(&self, timeout: Duration) -> Vec<u64> {
        tokio::time::timeout(timeout, async {
            let mapper = self.inner.mapper.lock().await;
            mapper
                .turns
                .outstanding
                .iter()
                .map(|turn| turn.seq)
                .collect()
        })
        .await
        .expect("bounded test wait timed out: inspecting outstanding turns (mapper mutex)")
    }

    /// ma-sdk-state r12 item 1 (test-stub): shrink the publication backlog
    /// caps before (or between) launches so a regression can flood past the
    /// bound in milliseconds. `items` bounds queued frames, `bytes` their
    /// summed charge. Applies to the NEXT launch's fresh budget.
    #[cfg(feature = "test-stub")]
    pub fn set_publication_backlog_limits_for_test(&self, items: usize, bytes: usize) {
        self.inner.backlog_config.set(items, bytes);
    }

    /// ma-sdk-state r12 item 1 (test-stub): current and high-water backlog
    /// occupancy plus the active caps, to assert the backlog stays bounded.
    #[cfg(feature = "test-stub")]
    #[must_use]
    pub fn publication_backlog_for_test(&self) -> BacklogSnapshot {
        match self
            .inner
            .active_budget
            .lock()
            .expect("active budget slot")
            .as_ref()
        {
            Some(budget) => budget.snapshot(),
            None => BacklogSnapshot::idle(&self.inner.backlog_config),
        }
    }

    /// ma-sdk-state r14 item 1 (test-stub): identity of the ACTIVE launch's
    /// backlog budget. Every launch builds a fresh budget, so a close/relaunch
    /// while the old publisher is mid-release must show a new id here.
    #[cfg(feature = "test-stub")]
    #[must_use]
    pub fn active_backlog_id_for_test(&self) -> usize {
        self.inner
            .active_budget
            .lock()
            .expect("active budget slot")
            .as_ref()
            .map_or(0, |budget| budget.id())
    }

    /// Test-only (`test-stub`, r10 item 3): wait until `n` Result frames have
    /// been mapped by the publication worker, using the armed barrier slot
    /// directly (for spawned tasks that do not own the [`WriteCommitBarrier`]).
    #[cfg(feature = "test-stub")]
    pub async fn test_wait_results_mapped(&self, n: usize, timeout: Duration) {
        test_barrier::wait_results_mapped(&self.inner.barrier, n, timeout).await;
    }

    /// Test-only (`test-stub`): make the terminal exit observation build fail
    /// inside `emit_exit` (after the reserved channel permit was taken), to
    /// prove the RunHandle still reaches EOF after close (ma-sdk-state r6 item
    /// 4).
    #[cfg(feature = "test-stub")]
    pub fn force_exit_build_to_fail(&self) {
        self.inner
            .exit_build_fails
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Test-only (`test-stub`): force writer call number `n` (1-based) and
    /// every later one to fail without touching the live child (r7 items 2/5).
    #[cfg(feature = "test-stub")]
    pub fn force_write_failure_from(&self, n: usize) {
        self.inner
            .fail_from_write
            .store(n, std::sync::atomic::Ordering::Release);
    }

    /// Test-only (`test-stub`, r11 item 3): make the NEXT reservation's ticket
    /// send fail while the publisher stays open, driving fail_without_ticket
    /// against a live child. The just-opened book entry must be rolled back
    /// even if the caller is aborted while that rollback waits on the mapper.
    #[cfg(feature = "test-stub")]
    pub fn force_reserve_send_failure_for_test(&self) {
        self.inner
            .reserve_send_fails
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Test-only (`test-stub`): a Weak to this driver's [`Inner`], so a test can
    /// prove every Arc is released after a failed write + close (r7 item 3: no
    /// reservation-guard leak).
    #[cfg(feature = "test-stub")]
    #[must_use]
    pub fn weak_inner(&self) -> std::sync::Weak<Inner> {
        std::sync::Arc::downgrade(&self.inner)
    }

    /// Build a driver from explicit options.
    pub fn new(options: ClaudePrintOptions) -> Self {
        Self::with_carrier(options, DriverKind::ClaudePrint)
    }

    /// The real OS exit status of the child reaped during the most recent
    /// [`Driver::close`]. Test-only: lets a driven VCR test assert the spawned
    /// peer's actual disposition rather than a status the peer self-reported.
    #[cfg(any(test, feature = "test-stub"))]
    pub async fn child_exit_status(&self) -> Option<std::process::ExitStatus> {
        *self.inner.child_exit_status.lock().await
    }

    /// Wire name of the carrier this object drives, for user-facing messages and
    /// logs. Sibling drivers pass the same kebab-case spelling.
    fn carrier_name(&self) -> &'static str {
        match self.carrier {
            DriverKind::ClaudeSdk => "claude-sdk",
            _ => "claude-print",
        }
    }

    /// [`Self::new`], stamping and launching `carrier` instead of `claude-print`.
    pub(crate) fn with_carrier(options: ClaudePrintOptions, carrier: DriverKind) -> Self {
        let carrier_name = carrier;
        Self {
            inner: build_inner(options.media_stager.clone(), carrier_name),
            options,
            reader: Mutex::new(None),
            publisher: Mutex::new(None),
            carrier,
        }
    }

    /// SIGKILL the child. Stdout EOF then emits a session-exited lifecycle.
    pub async fn kill(&self) -> DriverResult<DriverAck> {
        let mut live = self.inner.live.lock().await;
        let Some(live) = live.as_mut() else {
            return Err(DriverError::ControlUnavailable);
        };
        live.process.kill().map_err(map_wire)?;
        Ok(DriverAck::transport_written())
    }

    async fn launch(&self, spec: InstanceSpec, session: SessionAction) -> DriverResult<RunHandle> {
        if spec.driver != self.carrier {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "{} driver requires driverKind {}, got {:?}",
                self.carrier_name(),
                self.carrier_name(),
                spec.driver
            )));
        }
        reject_bot_bypass(&spec, self.options.origin)?;
        let session_id = session.session_id().to_string();
        let request = MaterializeRequest {
            spec: &spec,
            profile: &self.options.profile,
            launch_dir: self.options.launch_dir.clone(),
            native_home: self.options.native_home.clone(),
            session,
            launch_id: Id::new("launch")?,
            binary: self.options.binary.clone(),
            setting_sources: self.options.setting_sources.clone(),
            origin: self.options.origin.into(),
            native_home_managed: Some(!self.options.inherit_default_config),
            settings_overlay_path: self.options.settings_overlay_path.clone(),
            secret_policy: None,
        };
        let mut recipe = materialize(&request)?;
        apply_bypass_flag(&mut recipe);
        refuse_prohibited_argv(&recipe.argv)?;
        let policy = policy_from_recipe(&recipe, &spec);
        let env = self.resolve_env(&spec, &recipe).await?;
        let mut command = Command::new(&recipe.binary.abs_path);
        command.args(&recipe.argv).current_dir(&recipe.cwd);
        // Start from nothing: the Node's own environment holds the bootstrap
        // and host tokens, and inheriting it hands them to the model
        // (security-review-2 S1).
        command.env_clear();
        for (key, value) in crate::child_env::base_env() {
            command.env(key, value);
        }
        for (key, value) in &env {
            command.env(key, value);
        }
        for (key, value) in &self.options.extra_env {
            if crate::child_env::is_denied(key) {
                continue;
            }
            command.env(key, value);
        }
        if let Some(context) = &self.options.agent_mcp {
            command.envs(context.environment()?);
        }
        configure_native_home(
            &mut command,
            &recipe.native_home,
            self.options.inherit_default_config,
        );

        let (_inbound, outbound, process) = ClaudeProcess::spawn_command(
            command,
            Path::new(&recipe.binary.abs_path),
            InitializeRequest::default(),
            self.options.handshake_timeout,
            remuda_claude_wire::DEFAULT_MAX_LINE_BYTES,
        )
        .await
        .map_err(map_wire)?;

        let instance_id = InstanceId::new();
        let run_id = RunId::new();
        let journal_id = Id::new("obj")?;
        let (tx, rx) = mpsc::channel(256);
        // r4 item 4: reserve the one terminal-event slot NOW, before the
        // launch proceeds. Effective capacity for observations is 255; the
        // exit permit is never taken by ordinary frames. If the child floods
        // and the publication worker parks (or is aborted by close), the
        // exit still has a guaranteed slot in the same FIFO channel.
        let exit_permit = tx.clone().try_reserve_owned().map_err(|error| {
            DriverError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!("cannot reserve exit slot: {error}"),
            ))
        })?;
        {
            let mut mapper = self.inner.mapper.lock().await;
            *mapper = Mapper {
                stream: stream::StreamState::default(),
                ids: NativeIds::default(),
                seq: 0,
                instance_id: instance_id.clone(),
                run_id,
                journal_id,
                host_id: spec.host.clone(),
                session_id: session_id.clone(),
                pin: recipe.binary.clone(),
                driver_kind: self.carrier,
                channel: SourceChannel::Stdout,
                media_stager: self.options.media_stager.clone(),
                turns: TurnBook::default(),
                saw_per_call_usage: false,
                deferred_result: None,
            };
        }
        *self.inner.policy.lock().await = policy;
        *self.inner.last_spec.lock().await = Some(spec.clone());

        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("sessionId".into(), session_id);
        if let Some(pid) = process.id() {
            ack.native_ids.insert("pid".into(), pid.to_string());
        }
        // Captured NOW, before `process` moves into `live`: the launch-scoped
        // overflow shutdown signals THIS pid/pgid and never re-reads the
        // driver's current live slot (r15 item 1).
        let kill_target = process.id().and_then(|pid| i32::try_from(pid).ok());

        // Fresh publication FIFO AND fresh accounting for this launch (r14
        // directive (a)): this budget's item/byte semaphores are owned nowhere
        // outside this launch — queued jobs hold owned permits and a task from
        // launch A can release only onto A's semaphores, never onto launch B's.
        let budget = BacklogBudget::new(
            self.inner
                .backlog_config
                .limit_items
                .load(Ordering::Acquire),
            self.inner
                .backlog_config
                .limit_bytes
                .load(Ordering::Acquire),
        );
        *self.inner.active_budget.lock().expect("active budget slot") = Some(Arc::clone(&budget));
        let (publish_queue, publish_rx) = PublishQueue::new(budget);

        // Fresh per-launch scope: emission channel, terminal seal, exit
        // permit, admission seal and the shutdown-task slot all start clean
        // and are owned by the Arc every launch task carries (r15 item 1).
        let launch = Launch::new(
            publish_queue.clone(),
            Some(tx),
            Some(exit_permit),
            kill_target,
            #[cfg(feature = "test-stub")]
            Arc::clone(&self.inner.barrier),
        );

        *self.inner.live.lock().await = Some(Live {
            process,
            pending: HashMap::new(),
            pending_by_native: HashMap::new(),
            recipe: recipe.clone(),
        });
        // Install the launch scope only after live is in place; close/start
        // are driver-serialized by the Node and close takes this slot first.
        self.inner.last_process_gone.store(false, Ordering::SeqCst);
        *self.inner.launch.lock().await = Some(Arc::clone(&launch));

        let publisher = {
            let inner = Arc::clone(&self.inner);
            let launch = Arc::clone(&launch);
            tokio::spawn(async move {
                publish_worker(inner, launch, publish_rx).await;
            })
        };
        *self.publisher.lock().await = Some(publisher);

        let inner = Arc::clone(&self.inner);
        let carrier = self.carrier_name();
        let reader = tokio::spawn(async move {
            map_loop(inner, launch, outbound, publish_queue, carrier).await;
        });
        *self.reader.lock().await = Some(reader);
        Ok(RunHandle::new(recipe, ack, rx))
    }

    async fn resolve_env(
        &self,
        spec: &InstanceSpec,
        recipe: &LaunchRecipe,
    ) -> DriverResult<std::collections::BTreeMap<String, String>> {
        let mut env = std::collections::BTreeMap::new();
        for entry in &recipe.env_allowlist {
            // The materializer already refuses these, but this is the last
            // gate before the value reaches a process (security-review-2 S2).
            // Capability entries are driver-computed grants (D-045): the
            // REMUDA_ deny prefix guards spec- and host-supplied names, not
            // values the driver itself attaches only after the grant gates.
            // The hole is narrow to the granted-handshake name only: any other
            // denied (REMUDA_-prefixed) name is refused even when tagged
            // Capability. The value rides the entry from the grant.
            if crate::child_env::is_denied(&entry.name)
                && entry.name != crate::launch::skills::CAPABILITY_COMPUTER_USE_ENV
            {
                debug!(name = %entry.name, "refusing a denied env name");
                continue;
            }
            match entry.source {
                EnvAllowlistSource::NativeHome => {
                    env.insert(entry.name.clone(), recipe.native_home.clone());
                }
                EnvAllowlistSource::ProviderOverlay => {
                    if entry.name == "ANTHROPIC_BASE_URL" && !recipe.provider.base_url.is_empty() {
                        env.insert(entry.name.clone(), recipe.provider.base_url.clone());
                    }
                }
                EnvAllowlistSource::Credential => {
                    let raw = entry.secret_ref.clone().or_else(|| {
                        self.options
                            .profile
                            .secret_ref
                            .as_ref()
                            .map(|secret| secret.as_str().to_string())
                    });
                    let Some(raw) = raw else {
                        continue;
                    };
                    if raw.starts_with("helper:") {
                        continue;
                    }
                    let Ok(secret_ref) = crate::SecretRef::parse(raw) else {
                        continue;
                    };
                    match self.options.broker.resolve(&secret_ref).await {
                        Ok(secret) => {
                            env.insert(entry.name.clone(), secret.expose_str()?.to_owned());
                        }
                        Err(error) => {
                            debug!(%error, name = %entry.name, "credential unresolved");
                        }
                    }
                }
                EnvAllowlistSource::Literal => {
                    if let Some(remuda_protocol::EnvBinding::Literal(lit)) =
                        spec.env.get(&entry.name)
                    {
                        env.insert(entry.name.clone(), lit.value.clone());
                    }
                }
                EnvAllowlistSource::HostEnv => {
                    if let Ok(value) = std::env::var(&entry.name) {
                        env.insert(entry.name.clone(), value);
                    }
                }
                EnvAllowlistSource::Capability => {
                    if let Some(value) = entry.value.as_deref() {
                        env.insert(entry.name.clone(), value.to_owned());
                    }
                }
            }
        }
        Ok(env)
    }
}

fn configure_native_home(command: &mut Command, native_home: &str, inherit_default: bool) {
    // Claude Code's macOS login lookup changes namespaces when this variable is
    // present, even when it names the default ~/.claude directory. Omit it only
    // for the explicit host-login opt-in; isolated homes remain the default.
    if inherit_default {
        command.env_remove("CLAUDE_CONFIG_DIR");
    } else {
        command.env("CLAUDE_CONFIG_DIR", native_home);
    }
}

#[cfg(test)]
mod native_home_tests {
    use super::*;
    use std::ffi::OsStr;

    fn configured_value(command: &Command) -> Option<Option<&OsStr>> {
        command
            .as_std()
            .get_envs()
            .find_map(|(key, value)| (key == "CLAUDE_CONFIG_DIR").then_some(value))
    }

    #[test]
    fn isolated_home_is_exported() {
        let mut command = Command::new("claude");
        configure_native_home(&mut command, "/tmp/isolated-claude", false);
        assert_eq!(
            configured_value(&command).flatten(),
            Some(OsStr::new("/tmp/isolated-claude"))
        );
    }

    #[test]
    fn inherited_default_does_not_override_environment() {
        let mut command = Command::new("claude");
        // The materialized NativeHome allowlist is applied before this final
        // policy step. Ensure the opt-in removes that already-set value.
        command.env("CLAUDE_CONFIG_DIR", "/tmp/materialized-home");
        configure_native_home(&mut command, "/tmp/not-exported", true);
        assert_eq!(configured_value(&command), Some(None));
    }
}

#[async_trait]
impl Driver for ClaudePrintDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let live = self.inner.live.lock().await;
        let pin = live
            .as_ref()
            .map(|live| live.recipe.binary.clone())
            .unwrap_or_else(|| BinaryPin {
                abs_path: String::new(),
                version: "unpinned".into(),
                sha256: dummy_digest(),
            });
        Ok(capability_snapshot(self.carrier, &pin, U64(1), U64(1))?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        let session_id = uuid::Uuid::new_v4().to_string();
        self.launch(spec, SessionAction::New { session_id }).await
    }

    async fn attach(&self, _native_ref: NativeRef) -> DriverResult<DriverAck> {
        Err(DriverError::ControlUnavailable)
    }

    async fn process_gone(&self) -> bool {
        // The launch's reader task sets this (once) when the child's stdout
        // closes and `emit_exit` runs, i.e. the driver's OWN process-end
        // evidence; the close ladder sets it too. It lives on the CURRENT
        // launch's scope, so a launch-A reader outliving close cannot read or
        // reset launch B's answer.
        match self.inner.launch.lock().await.as_ref() {
            Some(launch) => launch.exit_emitted.load(Ordering::Acquire),
            // After close the launch slot is gone; close copied the last
            // launch's latch out so the post-close r5 answer still lands.
            None => self.inner.last_process_gone.load(Ordering::Acquire),
        }
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        let launch_slot = self.inner.launch.lock().await;
        if launch_slot
            .as_ref()
            .is_none_or(|launch| launch.closed.load(Ordering::SeqCst))
        {
            return Err(DriverError::ControlUnavailable);
        }
        let launch = Arc::clone(launch_slot.as_ref().expect("present and open"));
        drop(launch_slot);
        match input {
            DriverInput::Prompt(prompt) => {
                // ma-sdk-state r6 item 1: resolve ALL fallible prompt content
                // (notably reading a staged image attachment) BEFORE opening the
                // turn book or enqueuing its ticket.
                let content = prompt_content(&prompt.blocks)?;
                let client_message_id = prompt.native_client_message_id.clone();
                let writer = {
                    let live = self.inner.live.lock().await;
                    let Some(live) = live.as_ref() else {
                        return Err(DriverError::ControlUnavailable);
                    };
                    live.process.writer()
                };
                // Pre-accept phase runs on THIS future: a caller aborted before
                // the writer accepts UserWrite cancels the write (the prompt
                // never executes). After acceptance the returned future is a
                // detached completion that survives the caller (r8 item 1).
                let completion = prepare_prompt_write(
                    &self.inner,
                    &launch,
                    writer,
                    content,
                    client_message_id,
                    self.carrier_name(),
                )
                .await?;
                completion
                    .await
                    .map_err(|_| DriverError::ControlUnavailable)?
            }
            DriverInput::Steer(_) => Err(DriverError::CapabilityUnknown("steer".into())),
            DriverInput::ModelSwitch(switch) => {
                // §9.1: stream-json has no in-session effort channel. A
                // `set_settings` control request is silently ignored by
                // claude -p (measured 2.1.221, see evidence/effort-sync-1.md),
                // so accepting an effort switch here would be a lie — the
                // level only changes via `--effort` at launch on this carrier.
                if switch
                    .effort
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                {
                    return Err(DriverError::CapabilityUnsupported(format!(
                        "{} cannot switch effort in-session: relaunch with --effort; \
                         use the claude-pty or shell-pty carrier for /effort",
                        self.carrier_name()
                    )));
                }
                if !switch.model_id.is_empty() {
                    let writer = {
                        let live = self.inner.live.lock().await;
                        let Some(live) = live.as_ref() else {
                            return Err(DriverError::ControlUnavailable);
                        };
                        live.process.writer()
                    };
                    writer
                        .set_model(Some(switch.model_id.clone()))
                        .await
                        .map_err(map_wire)?;
                }
                Ok(DriverAck::transport_written())
            }
        }
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        // Clone the writer and release `live` before the enqueue await: a
        // wedged writer channel must not pin `live` and gate close (r3 item 5).
        let writer = {
            let live = self.inner.live.lock().await;
            let Some(live) = live.as_ref() else {
                return Err(DriverError::ControlUnavailable);
            };
            live.process.writer()
        };
        writer.interrupt(true).await.map_err(map_wire)?;
        Ok(DriverAck::transport_written())
    }

    async fn respond_interaction(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        // Mutate pending state under the lock and clone the ProcessWriter;
        // the stdin enqueue happens after the guard is released (r3 item 5).
        // r4 item 5(c): the response rides the SAME writer FIFO as prompts,
        // so a later prompt/EOF can never overtake it.
        let (writer, request_id, payload) = {
            let mut live = self.inner.live.lock().await;
            let Some(live) = live.as_mut() else {
                return Err(DriverError::ControlUnavailable);
            };
            let pending = live
                .pending
                .remove(&id)
                .ok_or(DriverError::ControlUnavailable)?;
            live.pending_by_native.remove(&pending.request_id);
            let payload = permission_from_answer(&pending.input, &answer)?;
            (live.process.writer(), pending.request_id, payload)
        };
        writer
            .respond_control(request_id, payload)
            .await
            .map_err(map_wire)?;
        Ok(DriverAck::transport_written())
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        // Take THIS launch's scope out of the driver before anything else:
        // every later send sees no launch, and every launch task keeps using
        // its own Arc until close joins it (r15 item 1).
        let launch = self.inner.launch.lock().await.take();

        // Finish the launch-scoped overflow shutdown BEFORE the kill ladder.
        // The seal and the claim of the task handle are ONE critical section
        // on the launch's shutdown mutex, so a spawn racing close either
        // registered (this finishes it) or observed the seal and never ran
        // (the ladder below owns A's kill). The shutdown targets only the
        // captured A pid anyway — it can never reach a relaunched child.
        if let Some(launch) = launch.as_ref() {
            let handle = {
                let mut slot = launch.shutdown.lock().expect("overflow shutdown slot");
                launch.closed.store(true, Ordering::SeqCst);
                slot.take()
            };
            join_or_abort_handle(handle, "overflow shutdown").await;
        }

        let mut live_guard = self.inner.live.lock().await;
        let recipe = live_guard.as_ref().map(|live| live.recipe.clone());
        if let Some(live) = live_guard.as_mut() {
            let status = close_ladder(live, self.options.close_timeout, self.carrier_name()).await;
            // Capture the child's REAL OS exit status from the reap for
            // tests that must prove the peer finished cleanly.
            #[cfg(any(test, feature = "test-stub"))]
            if let Some(status) = status {
                *self.inner.child_exit_status.lock().await = Some(status);
            }
            #[cfg(not(any(test, feature = "test-stub")))]
            let _ = status;
        }
        *live_guard = None;
        drop(live_guard);

        // Stop this launch's reader before touching the events channel. The
        // child is already reaped, so its stdout is at EOF and `map_loop` is on
        // its way out; give it a short bound to finish and deliver the `exited`
        // lifecycle itself, then abort it if it is still parked.
        //
        // The abort is the point. The reader owns launch A's Arc, and a later
        // `start()` installs launch B; directive (e): after the abort the
        // handle is AWAITED (bounded), so by the time close returns the task
        // has actually terminated — an abort merely sent could otherwise
        // leave a launch-A task alive across a relaunch.
        join_or_abort(&self.reader, "reader").await;

        // Seal the publication FIFO now that the reader can no longer enqueue,
        // and join its drainer so every already-queued frame (and the reader's
        // trailing StdoutEof) is published before `exited`. Bounded, abort and
        // AWAIT the aborted handle (see `join_or_abort`, directive (e)): a
        // worker parked on a wedged control-response write must not keep close
        // open — the child is already reaped above — but close also may not
        // return while that task still exists: it owns the launch-A budget and
        // its queued jobs' permits must finish dropping onto that dead budget,
        // never onto the next launch's.
        if let Some(launch) = launch.as_ref() {
            launch.publish.lock().await.take();
        }
        join_or_abort(&self.publisher, "publisher").await;

        // Seal the events channel BEFORE covering the exit. Under output
        // pressure (a child that floods stdout while stdin is wedged) the
        // events channel is full and the worker was aborted above; emitting
        // the exit into that full channel would block forever and hang close.
        // On the normal path the worker already drained StdoutEof and emitted
        // `exited` into the receiver before it was joined, so dropping the
        // sender loses nothing; here the once-guard below is then a no-op.
        if let Some(launch) = launch.as_ref() {
            launch.emission.lock().await.tx = None;
            // Whichever path got there first — the worker draining StdoutEof,
            // the finished overflow shutdown, or this call covering an aborted
            // one — records exactly once (`exit_emitted` on THIS launch);
            // emission is skipped once events is sealed.
            let _ = emit_exit(&self.inner, launch, "exited").await;
            // Copy the terminal evidence to the driver-level latch before the
            // launch Arc drops, so process_gone after close answers true.
            self.inner.last_process_gone.store(
                launch.exit_emitted.load(Ordering::Acquire),
                Ordering::SeqCst,
            );
        }
        // S5: the child has exited, so the launch overlays can go. The label is
        // the carrier's wire name, matching the kebab-case the sibling drivers
        // pass (`claude-pty`, `claude-bg`, `generic-pty`).
        let carrier = self.carrier_name();
        if let Some(recipe) = recipe {
            crate::recipe::report_launch_cleanup(&recipe, carrier);
        }
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        let session_id = match native_ref
            .claude
            .as_ref()
            .map(|claude| claude.session_id.clone())
            .or_else(|| match &native_ref.session_id {
                Knowledge::Known { value } => Some(value.clone()),
                _ => None,
            }) {
            Some(id) if !id.is_empty() => id,
            _ => return Err(DriverError::NativeSessionNotFound),
        };
        let mut spec = self
            .inner
            .last_spec
            .lock()
            .await
            .clone()
            .ok_or(DriverError::NativeSessionNotFound)?;
        spec.host = native_ref.host_id.clone();
        spec.driver = self.carrier;
        spec.kind = remuda_protocol::AgentKind::Claude;
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }

    async fn start_resumed(
        &self,
        mut spec: InstanceSpec,
        session_id: String,
    ) -> DriverResult<RunHandle> {
        if session_id.trim().is_empty() {
            return Err(DriverError::NativeSessionNotFound);
        }
        spec.driver = self.carrier;
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }
}

/// Close the child on a bounded ladder: stdin EOF with a bounded wait, SIGTERM
/// to the process group with a bounded wait, then SIGKILL to the group plus a
/// bounded reap (`print-replacement.md` §2.3).
///
/// Stdin EOF is only a request. A child mid-turn, blocked on a `can_use_tool`
/// nobody answered, or one that simply stopped draining stdin, can ignore it
/// for as long as it likes — and on the long-lived sdk carrier the previous
/// unbounded `wait()` turned that into an `instance.close` that never returned.
/// **Every** rung is bounded, including the EOF request itself: when the child
/// stops reading and the 64-slot writer channel fills, enqueuing the EOF blocks
/// exactly the way the subsequent wait could. The function never returns while
/// still holding a live child it could kill.
/// Reap the child on a bounded ladder, returning its real OS exit status when
/// a `wait()` completed (None only if every bounded rung timed out and the
/// process had to be abandoned still alive).
async fn close_ladder(
    live: &mut Live,
    timeout: Duration,
    carrier: &str,
) -> Option<std::process::ExitStatus> {
    // The group id equals the direct child's pid because `spawn_command` puts it
    // in its own group. Read it before any wait: once reaped, `id()` is `None`.
    let pgid = live.process.id().and_then(|pid| i32::try_from(pid).ok());

    // Rung 1: the graceful path. Request stdin EOF, then give the child the
    // whole first slice to leave. The two steps share one bound: `close_stdin`
    // enqueues onto the writer channel, and that enqueue can block when the
    // child stopped draining and the channel is full — so it must sit inside
    // the timeout, not before it. Most closes end here.
    let first_slice = timeout.mul_f32(0.5);
    let request_eof_and_wait = async {
        // Best-effort stdin EOF; if the child already exited (a one-shot VCR
        // peer that finished its frames, say), the write errors and we still
        // MUST reap it below and report its real status.
        let _ = live.process.close_stdin().await;
        live.process.wait().await
    };
    if let Ok(status) = tokio::time::timeout(first_slice, request_eof_and_wait).await {
        return status.ok();
    }

    // Rung 2: SIGTERM to the whole group, then the second slice. The native
    // `interrupt` control request is not used here: it travels over stdin, which
    // rung 1 already asked to close, so nothing further can be written — and
    // interrupting before EOF would cancel in-flight work on every ordinary
    // close, which `instance.close` does not promise. The interrupt path stays
    // in `Driver::cancel`.
    //
    // The group gets the signal so a `Bash` tool or MCP server the child spawned
    // leaves with it instead of outliving the instance. A child with a handler
    // can use this slice to flush its transcript.
    debug!(carrier, "close: stdin EOF ignored, terminating the group");
    terminate_group(pgid, carrier);
    let second_slice = timeout.saturating_sub(first_slice);
    if let Ok(status) = tokio::time::timeout(second_slice, live.process.wait()).await {
        return status.ok();
    }

    // Rung 3: SIGKILL to the group, which nothing can ignore, then reap so a
    // zombie cannot survive close. `kill` reaches only the direct child, hence
    // the group signal first. The reap has a short bound for a child wedged in
    // uninterruptible kernel IO; such a process cannot be killed by userspace,
    // but close still returns rather than hanging.
    warn!(carrier, "close: SIGTERM ignored, killing the process group");
    kill_group(pgid, carrier);
    let _ = live.process.kill();
    tokio::time::timeout(Duration::from_secs(2), live.process.wait())
        .await
        .ok()
        .and_then(|result| result.ok())
}

/// Join a launch task bounded; if it does not finish in the window, abort it
/// and AWAIT its termination (also bounded) before returning (ma-sdk-state r14
/// item 1, directive (e)).
///
/// The second await is load-bearing: `JoinHandle::abort` only SCHEDULES the
/// cancellation, which is delivered at the task's next await point. Returning
/// from close before an aborted publisher actually unwinds could leave a
/// launch-A task — still holding its launch's backlog permits — alive when a
/// relaunch installs launch B. A task blocked in uninterruptible work keeps
/// the second bound from growing without limit; such a task cannot reach any
/// driver state that close replaces before it next hits an await anyway.
async fn join_or_abort(slot: &Mutex<Option<JoinHandle<()>>>, what: &'static str) {
    let handle = slot.lock().await.take();
    join_or_abort_handle(handle, what).await;
}

/// [`join_or_abort`] for a task handle close CLAIMED from somewhere other
/// than a driver slot — the launch-scoped overflow shutdown
/// (ma-sdk-state r15 item 1): close must finish even that detached task
/// before its kill ladder and before a relaunch can install another child.
async fn join_or_abort_handle(handle: Option<JoinHandle<()>>, what: &'static str) {
    let Some(mut handle) = handle else {
        return;
    };
    if tokio::time::timeout(Duration::from_millis(500), &mut handle)
        .await
        .is_err()
    {
        handle.abort();
        if tokio::time::timeout(Duration::from_millis(500), &mut handle)
            .await
            .is_err()
        {
            warn!("close: the {what} task did not unwind within the abort bound");
        }
    }
}

/// SIGTERM every member of the child's process group. Unix only; on other
/// platforms the child has no group to signal and `kill()` in the ladder is the
/// fallback, so this is a no-op there. `None` means the pid was already gone.
#[cfg(unix)]
fn terminate_group(pgid: Option<i32>, carrier: &str) {
    if let Some(pgid) = pgid {
        signal_child_group(pgid, nix::sys::signal::Signal::SIGTERM, carrier);
    }
}

#[cfg(not(unix))]
fn terminate_group(pgid: Option<i32>, carrier: &str) {
    let _ = (pgid, carrier);
}

/// SIGKILL every member of the child's process group. Unix only.
#[cfg(unix)]
fn kill_group(pgid: Option<i32>, carrier: &str) {
    if let Some(pgid) = pgid {
        signal_child_group(pgid, nix::sys::signal::Signal::SIGKILL, carrier);
    }
}

#[cfg(not(unix))]
fn kill_group(pgid: Option<i32>, carrier: &str) {
    let _ = (pgid, carrier);
}

/// SIGKILL the launch child's LEADER directly by its captured pid (r15 item 1).
///
/// The launch-scoped overflow shutdown must kill without acquiring the
/// driver's `live` slot, so unlike the close ladder it signals the captured
/// pid rather than going through the [`ClaudeProcess`] handle. The leader is
/// its own process-group leader, so [`kill_group`] already reaches it; this
/// direct signal is the belt-and-braces twin of `live.process.kill()` for a
/// pid that was reaped-and-recycled away from the group (an `ESRCH` race the
/// group signal would also swallow). No reaping happens here — close's ladder
/// reaps the child.
#[cfg(unix)]
fn kill_child_direct(pid: Option<i32>, carrier: &str) {
    if let Some(pid) = pid.filter(|pid| *pid > 0) {
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            Some(nix::sys::signal::Signal::SIGKILL),
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(errno) => debug!(carrier, pid, %errno, "direct child signal failed"),
        }
    }
}

#[cfg(not(unix))]
fn kill_child_direct(pid: Option<i32>, carrier: &str) {
    let _ = (pid, carrier);
}

/// Send a signal to every member of process group `pgid`.
///
/// `unsafe` is forbidden workspace-wide, so this goes through `nix` rather than
/// a raw `libc::killpg` — the same path `shell_pty/lifecycle.rs` uses. A group
/// that is already gone (`ESRCH`) is the outcome being asked for, and `EPERM`
/// means something in it is not ours to signal; neither is worth failing a close
/// over, so both are logged and swallowed. The `nix` signal type is kept out of
/// the non-unix callers' signatures so the `cfg(not(unix))` stubs compile.
#[cfg(unix)]
fn signal_child_group(pgid: i32, signal: nix::sys::signal::Signal, carrier: &str) {
    if pgid <= 0 {
        return;
    }
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(errno) => debug!(carrier, pgid, %errno, "process-group signal failed"),
    }
}

/// Result receiver for a prompt whose `UserWrite` the writer has accepted.
type WriteCompletion = tokio::sync::oneshot::Receiver<DriverResult<DriverAck>>;

/// CANCELLABLE pre-accept phase of a prompt write (ma-sdk-state r8 item 1).
///
/// Runs on the CALLER's future and holds `write_seq`, so dropping the caller
/// (abort/timeout) before the writer ACCEPTS the `UserWrite` command is still
/// cancellation: the prompt is never written, no `turn_started` is emitted, and
/// the [`ReservationGuard`]'s `Drop` rolls the reservation back in order (it
/// re-acquires `write_seq`). The publication ticket is enqueued before the
/// write.
///
/// The instant the writer FIFO accepts the command, ownership transfers to a
/// DETACHED task that awaits the write ack and commits/fails the reservation;
/// that task survives the caller being dropped, so a prompt the writer already
/// owns is always announced and settled. The returned receiver only observes
/// the result.
async fn prepare_prompt_write(
    inner: &Arc<Inner>,
    launch: &Arc<Launch>,
    writer: ProcessWriter,
    content: UserContent,
    client_message_id: String,
    carrier: &'static str,
) -> DriverResult<WriteCompletion> {
    // test-stub: note a caller reaching the write_seq lock (r10 item 3: a test
    // acknowledges the NEXT caller queued behind a suspended one).
    #[cfg(feature = "test-stub")]
    test_barrier::note_write_seq_wait(&inner.barrier);
    let _seq = inner.write_seq.lock().await;

    // ma-sdk-state r10 item 1: acquire the publisher BEFORE opening a book
    // entry. The publish mutex can be held for a while (close, a publication
    // fence); a caller aborted while parked on it used to leave a reservation
    // no PublishJob owned — Drop only signalled the never-queued ticket, so a
    // later result popped the phantom in result_settles_root and a deferred
    // settle it blocked was never released. Parked here, no book entry exists
    // yet, so an abort leaves nothing behind.
    //
    // ma-sdk-state r11 item 2: this mutex is the ADMISSION GATE. It is held
    // continuously until the book entry is opened AND the Reserve ticket has
    // been inserted — one atomic admission relative to natural-stdout-EOF
    // sealing (which sets `closed` and enqueues StdoutEof from this same
    // critical section in map_loop). A reservation either inserts its ticket
    // BEFORE sealing (everything it emits, including a write failure's
    // rollback correction, is ordered ahead of StdoutEof) or loses the race
    // and is rejected here having opened nothing — including a caller that
    // cloned the sender earlier, since the sender is no longer used outside
    // this section.
    #[cfg(feature = "test-stub")]
    test_barrier::note_publish_lock_wait(&inner.barrier);
    let admission = launch.publish.lock().await;
    if launch.closed.load(Ordering::SeqCst) {
        return Err(DriverError::ControlUnavailable);
    }
    let Some(publisher) = admission.as_ref() else {
        return Err(DriverError::ControlUnavailable);
    };

    // ma-sdk-state r14 item 1 (directives (a)-(c)): Reserve tickets take the
    // SAME owned item/byte permits as frames, from THIS launch's per-launch
    // budget, BEFORE the book entry opens. A child that keeps reading prompts
    // while the publication worker is blocked on an undrained RunHandle
    // otherwise queues one ticket (and one outstanding turn) per prompt
    // without limit. A breach SPAWNS the cancellation-independent,
    // LAUNCH-SCOPED overflow shutdown as its own task BEFORE this function
    // can await anything, then is rejected: aborting this send (even while
    // the shutdown parks at its pre-kill gate) can no longer cancel the kill
    // or the terminal delivery, and close finishes the registered task
    // before a relaunch (r15 item 1). Natural EOF (StdoutEof) and
    // cancellation bypass this gate entirely.
    let budget = Arc::clone(publisher.budget());
    let mut charge = Some(match publisher.reserve_charge() {
        Ok(charge) => charge,
        Err(BacklogAdmitError::Closed) => return Err(DriverError::ControlUnavailable),
        Err(BacklogAdmitError::Overflow {
            items,
            bytes,
            frame_bytes,
        }) => {
            // ma-sdk-state r18 item 1 (RULING): the overflow ownership
            // decision — the latch swap, the closed check, AND the
            // registration of the tracked shutdown task — happens HERE,
            // SYNCHRONOUSLY under the publication gate this admission path
            // has held continuously since it entered the section. There is
            // no await and no drop/re-acquire between detecting the overflow
            // and registering the shutdown: the pre-fix code dropped the gate
            // and re-entered it asynchronously, so a send aborted while parked
            // at that re-entry discarded the whole overflow (latch unset, no
            // tracked task, the live child kept running without the
            // publication_backlog_overflow terminal). The spawned task does
            // every one of its awaited steps (its own seal, the kill, the
            // terminal emit) only AFTER this caller releases the gate; the
            // send never awaits it and close owns it through the launch
            // handle.
            let _registered = admit_backlog_overflow_shutdown(
                inner,
                launch,
                &budget,
                items,
                bytes,
                frame_bytes,
                carrier,
            );
            // test-stub: park at the EXACT point the pre-fix code yielded — it
            // dropped the gate and awaited the spawn function's gate
            // re-entry. The latch swap, the closed check and the tracked task
            // registration all completed ABOVE while the gate stayed held, so
            // aborting the triggering send parked here can no longer discard
            // the overflow: the future's drop releases the gate, and the
            // registered task still seals, kills the captured child and emits
            // the terminal. Open by default.
            #[cfg(feature = "test-stub")]
            test_barrier::hold_overflow_spawn_admission(&inner.barrier).await;
            drop(admission);
            // ma-sdk-state r15 item 6 (test-stub): park the triggering send
            // HERE, with the spawned shutdown known to be in progress, so a
            // test can abort THIS future mid-shutdown (on a multi-thread
            // runtime the abort lands at this await). Open by default.
            #[cfg(feature = "test-stub")]
            test_barrier::hold_after_overflow_spawn(&inner.barrier).await;
            return Err(DriverError::ControlUnavailable);
        }
    });

    // Open the book entry and enqueue its Reserve ticket with NO await between
    // opening and insertion WHILE holding admission: the mapper await is the
    // only await, and EOF sealing cannot enter until this section releases the
    // gate, so once the entry exists the ticket that owns it is already queued
    // ahead of any StdoutEof (or the send failed and the entry is rolled back
    // below before the gate is released).
    let reservation = {
        let mut mapper = inner.mapper.lock().await;
        let seq = mapper.begin_turn_reservation();
        TurnReservation::shared(seq, client_message_id)
    };
    // Armed for the whole pre-accept phase; take() at the accept boundary and
    // move into the detached completion task.
    let mut guard = Some(ReservationGuard::new(inner.clone(), reservation.clone()));

    // test-stub (r11 item 3): force the ticket send to fail with the channel
    // still open, driving fail_without_ticket against a live, well child.
    #[cfg(feature = "test-stub")]
    let force_send_failure = inner.reserve_send_fails.load(Ordering::Acquire);
    let send_failed = {
        #[cfg(feature = "test-stub")]
        if force_send_failure {
            true
        } else {
            match admission
                .as_ref()
                .expect("admission checked above")
                .admit_control(PublishJob::Reserve {
                    reservation: reservation.clone(),
                    charge: charge
                        .take()
                        .expect("the reserved charge rides the Reserve ticket"),
                }) {
                Ok(()) => false,
                // The worker is gone (close dropped the receiver). The
                // returned job carries the permits and releases them on drop.
                Err(mpsc::error::SendError(returned)) => {
                    drop(returned);
                    true
                }
            }
        }
        #[cfg(not(feature = "test-stub"))]
        match admission
            .as_ref()
            .expect("admission checked above")
            .admit_control(PublishJob::Reserve {
                reservation: reservation.clone(),
                charge: charge
                    .take()
                    .expect("the reserved charge rides the Reserve ticket"),
            }) {
            Ok(()) => false,
            Err(mpsc::error::SendError(returned)) => {
                drop(returned);
                true
            }
        }
    };
    // Admission is decided: release the gate before the (mapper-locked)
    // rollback so a sealed/closed channel never parks the EOF sealer or close
    // behind this caller's cleanup. The ticket was never inserted, so ordering
    // owes nothing.
    drop(admission);
    if send_failed {
        // The publisher is closed (close in progress) or the send was forced to
        // fail: no worker will ever visit this ticket. Roll the just-opened
        // entry back; the owned charge is released here too (force-failure kept
        // it local; a closed-send failure released it with the returned job).
        guard.take().unwrap().fail_without_ticket().await;
        drop(charge);
        return Err(DriverError::ControlUnavailable);
    }
    // The ticket is queued and its permits ride in the job: the worker
    // releases them by Drop when the Reserve job drains. Nothing to defuse.
    guard
        .as_mut()
        .expect("guard before accept")
        .mark_ticket_queued();
    #[cfg(feature = "test-stub")]
    test_barrier::note_reserve_enqueued(&inner.barrier);

    // test-stub: deterministic, NUMBERED park immediately before the writer
    // accepts UserWrite — a test aborts the caller here to prove the write never
    // happens (r8 item 1 test a).
    #[cfg(feature = "test-stub")]
    test_barrier::hold_before_user_write_accept(&inner.barrier).await;

    // test-stub: a numbered forced fault fails WITHOUT ever calling the writer
    // (the live child is untouched). Pre-accept, so it rolls back in order.
    #[cfg(feature = "test-stub")]
    let forced_failure = {
        let n = inner.write_number.fetch_add(1, Ordering::AcqRel) + 1;
        n >= inner.fail_from_write.load(Ordering::Acquire)
    };
    #[cfg(not(feature = "test-stub"))]
    let forced_failure = false;
    if forced_failure {
        guard.take().unwrap().fail();
        return Err(map_wire(remuda_claude_wire::Error::StdinClosed));
    }

    // >>> CANCELLATION BOUNDARY: this await is the last point at which dropping
    // the caller can prevent the write. Once it resolves Ok the writer FIFO owns
    // the command.
    let ack_rx = match writer.enqueue_user_write(content).await {
        Ok(ack_rx) => ack_rx,
        Err(error) => {
            // Channel send failed before accept: definitively not written.
            guard.take().unwrap().fail();
            return Err(map_wire(error));
        }
    };

    // Writer accepted: disarm the caller-scoped guard and transfer it into a
    // detached task that owns the ack → commit/fail sequence.
    let guard = guard.take().unwrap();
    #[cfg(feature = "test-stub")]
    let task_inner = inner.clone();
    #[cfg(not(feature = "test-stub"))]
    let _ = &inner;
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = match ack_rx.await {
            Ok(Ok(())) => {
                #[cfg(feature = "test-stub")]
                test_barrier::hold_write_commit(&task_inner.barrier).await;
                guard.commit();
                Ok(DriverAck::transport_written())
            }
            Ok(Err(error)) => {
                // The writer definitively failed after accepting the command.
                guard.fail();
                Err(map_wire(error))
            }
            Err(_) => {
                // Writer task vanished without acking (shutdown mid-write): not
                // provably written; roll the reservation back.
                guard.fail();
                Err(DriverError::ControlUnavailable)
            }
        };
        let _ = done_tx.send(result);
    });
    Ok(done_rx)
}

async fn map_loop(
    inner: Arc<Inner>,
    launch: Arc<Launch>,
    mut outbound: mpsc::Receiver<Outbound>,
    publisher: PublishQueue,
    carrier: &'static str,
) {
    // The loop exits three ways:
    // * `recv()` returns None — the CHILD's stdout reached EOF. That is the
    //   case that seals reservation admission and enqueues StdoutEof, UNLESS
    //   an overflow has already won terminal ownership (r16 item 1): checked
    //   under the admission mutex below, a genuine EOF arriving while the
    //   overflow shutdown is paused before its kill is suppressed so it cannot
    //   publish an ordinary `exited` and steal the overflow task's terminal.
    // * frame admission refuses with Overflow — spawn the overflow shutdown
    //   and stop: its task owns the terminal.
    // * frame admission refuses with Closed — the overflow latch already
    //   tripped elsewhere (e.g. a reservation breach, possibly with its
    //   shutdown still in progress) or close dropped the receiver. This is
    //   NOT child EOF: enqueuing a StdoutEof here (the r15 item 4 defect)
    //   let the worker emit the ordinary exit and suppress the overflow
    //   terminal. Neither close nor a paused-but-alive overflow shutdown may
    //   get a false EOF, so just stop and leave the terminal to its owner.
    // Set when the loop stopped on a frame ADMISSION refusal (Overflow or
    // Closed): the child did not close its stdout, so no StdoutEof.
    let mut refused = false;
    while let Some(frame) = outbound.recv().await {
        if matches!(frame, Outbound::KeepAlive) {
            continue;
        }
        // D-045 §6.2: stage tool-result images BEFORE the publication queue.
        // The staging bridge parks until the Hub answers; running it on a
        // blocking thread keeps the tokio worker moving. Critically this also
        // runs OUTSIDE every write path: a prompt blocked on a full stdin pipe
        // cannot stop the reader draining and pre-folding stdout (r3 item 5).
        let frame = match prefold_user_frame(&inner, frame).await {
            Ok(frame) => frame,
            Err(error) => {
                warn!(carrier, %error, "stream-json prefold failed");
                continue;
            }
        };
        // Bounded admission (r12 item 1; r14 item 1 ownership rewrite): a frame
        // that would cross the byte/item cap is NOT queued — it trips the
        // overflow session shutdown instead of growing Node memory without
        // limit. Control jobs bypass the cap, so the reader parking here
        // (never — admission is an std critical section) could not gate
        // cancellation or close either.
        // test-stub only: detect the turn RESULT frame specifically (not an
        // assistant/keepalive frame) so an armed barrier can wait until that
        // result is queued behind the parked ticket (ma-sdk-state r6 item 3).
        #[cfg(feature = "test-stub")]
        let is_turn_result = matches!(&frame, Outbound::Result(_));
        // test-stub (r12 item 2): optionally withhold a result frame from the
        // queue until the test releases it, so a later turn's result cannot
        // race the ordered processing of an earlier cancelled ticket.
        #[cfg(feature = "test-stub")]
        test_barrier::hold_result_enqueue(&inner.barrier, &frame).await;
        match publisher.admit_frame(Box::new(frame)) {
            Ok(()) => {}
            Err((
                BacklogAdmitError::Overflow {
                    items,
                    bytes,
                    frame_bytes,
                },
                _,
            )) => {
                // SPAWN the cancellation-independent shutdown SYNCHRONOUSLY,
                // before any await (directive (c); ma-sdk-state r18 item 1):
                // the reader going away (or being aborted by close) must not
                // take the kill or the terminal delivery with it. The latch
                // decision is gate-free HERE because the natural-EOF seal
                // runs on THIS SAME task after the loop (it is skipped via
                // `refused` below), so it can never interleave with this
                // swap; a reservation sender inside its publish section
                // neither seals nor reads the latch, a second overflow loses
                // the CAS, and the close race keeps its own recheck under the
                // shutdown mutex. The rejected frame owns nothing and drops
                // here.
                let budget = publisher.budget_clone();
                // Detached from the reader but attached to THIS LAUNCH: the
                // kill and terminal do not depend on the reader staying alive,
                // while close claims the tracked task and it can only touch
                // launch A's captured process/emission state (r15 item 1).
                let _registered = admit_backlog_overflow_shutdown(
                    &inner,
                    &launch,
                    &budget,
                    items,
                    bytes,
                    frame_bytes,
                    carrier,
                );
                // The spawned shutdown owns the terminal: do NOT fall through
                // to the stdout-EOF seal.
                #[cfg(feature = "test-stub")]
                test_barrier::note_overflow_frame_refused(&inner.barrier);
                refused = true;
                break;
            }
            Err((BacklogAdmitError::Closed, _)) => {
                // Overflow already tripped at another admission site (its
                // shutdown — possibly paused at a test gate — owns the
                // terminal; ma-sdk-state r15 item 4 used to make this the
                // false-StdoutEof path after a RESERVATION breach) or close
                // dropped the receiver (close owns it). This is NOT child
                // stdout EOF: mark the refusal so the seal section below is
                // skipped.
                #[cfg(feature = "test-stub")]
                test_barrier::note_closed_frame_refused(&inner.barrier);
                refused = true;
                break;
            }
        }
        #[cfg(feature = "test-stub")]
        if is_turn_result {
            test_barrier::note_frame_enqueued(&inner.barrier);
        }
    }
    if !refused {
        // recv() returned None: the CHILD's stdout reached EOF. Coordinate EOF
        // and overflow-terminal ownership as ONE admission critical section
        // (ma-sdk-state r16 item 1): under the publish mutex the overflow
        // shutdown uses for its own seal, check the overflow latch. Once an
        // overflow has won, suppress the ordinary StdoutEof publication and
        // leave the single terminal to the spawned overflow task — a GENUINE
        // EOF (a driver.kill() landing while the paused shutdown still holds
        // the kill) used to enqueue StdoutEof unconditionally, so the worker
        // emitted an ordinary `exited` that took the terminal from the paused
        // overflow task. With no overflow fired this remains the only path
        // that seals admission and queues StdoutEof (ma-sdk-state r11 item 2):
        // prepare_prompt_write holds that same mutex from its seal check
        // through book creation and ticket insertion, so a reservation can
        // never queue a Reserve ticket BEHIND this StdoutEof. Either the
        // ticket (and therefore its start/write-failure rollback) is fully
        // ahead of the EOF in publication order, or the reservation is
        // rejected having opened nothing. The EOF is a CONTROL job: it
        // bypasses the frame caps so a full backlog cannot delay the terminal
        // seal. `closed` is set here, not only by explicit close: a prompt
        // sent after the child's natural exit can never start a turn.
        let overflow_owns_terminal;
        {
            // ma-sdk-state r19 item 1 (test-stub): POSITIVE proof the natural-
            // EOF reader is blocked ON this guard at the acquisition point.
            // The reader announces it reached the point, then probes with a
            // non-awaiting try_lock: a `WouldBlock` answer reports the gate is
            // held AT THAT INSTANT — unlike the r18 item 2 absence window, a
            // merely-descheduled reader reports nothing. During the proof the
            // only holder there can be is the overflowing send parked in its
            // admission section: the spawned overflow shutdown is itself a
            // queued waiter behind that send (it takes this same gate only
            // after the send releases), and no fence/close acquisition runs.
            // On a free gate the probe acquires and drops a scratch guard and
            // reports nothing; the real acquisition then proceeds gate-free.
            #[cfg(feature = "test-stub")]
            {
                test_barrier::note_eof_seal_lock_wait(&inner.barrier);
                if Arc::clone(&launch.publish).try_lock_owned().is_err() {
                    test_barrier::note_eof_seal_lock_contended(&inner.barrier);
                }
            }
            let gate = launch.publish.lock().await;
            #[cfg(feature = "test-stub")]
            test_barrier::note_eof_seal_entered(&inner.barrier);
            // The reservation overflow decision sets the latch synchronously
            // inside ITS held admission section (r18 item 1), so observing it
            // here means the overflow terminal is already owned (its seal
            // either happened first and `closed` is set, or it happens next
            // once this critical section releases).
            overflow_owns_terminal = gate
                .as_ref()
                .is_some_and(|publisher| publisher.budget().overflowed.load(Ordering::Acquire));
            // ma-sdk-state r17 item 1 (test-stub): park on this exact spot —
            // latch loaded, seal decision not yet made, gate still held. The
            // overflow decision's swap and the EOF seal are one atomic
            // ordering on this gate: either the decision waited for this
            // section and the EOF seals (no shutdown is registered) or the
            // decision already won under its own held gate and this load saw
            // it.
            #[cfg(feature = "test-stub")]
            test_barrier::hold_eof_seal_decision(&inner.barrier).await;
            if !overflow_owns_terminal {
                launch.closed.store(true, Ordering::SeqCst);
                if let Some(publisher) = gate.as_ref() {
                    let _ = publisher.admit_control(PublishJob::StdoutEof);
                }
            }
        }
        #[cfg(feature = "test-stub")]
        if overflow_owns_terminal {
            // Fired from inside the admission critical section's wake: by this
            // point the suppressed EOF did NOT queue a terminal, so a fence
            // enqueued after the acknowledgement cannot be jumped by one.
            test_barrier::note_eof_suppressed_after_overflow(&inner.barrier);
        } else {
            test_barrier::note_stdout_eof_queued(&inner.barrier);
        }
    } else {
        // r15 item 4: the reader is stopping AFTER an admission refusal, with
        // no StdoutEof queued. Fired here, after the seal section, so a test
        // can enqueue a fence knowing no terminal job will jump its queue.
        #[cfg(feature = "test-stub")]
        test_barrier::note_reader_finished_after_refusal(&inner.barrier);
    }
}

/// Decide publication-backlog overflow ownership and REGISTER the tracked
/// shutdown task SYNCHRONOUSLY (ma-sdk-state r18 item 1).
///
/// Returns `true` when this call registered the single launch-scoped
/// shutdown task, `false` when the latch was already set (a second overflow
/// loses) or the launch was already sealed (natural EOF or close won).
///
/// ## Synchronization contract
///
/// The reservation overflow caller invokes this WHILE HOLDING the launch's
/// `publish` admission gate — the same gate it took before `reserve_charge`,
/// and the same gate the natural-EOF reader (`map_loop`) holds across its
/// latch load, `closed` store and `StdoutEof` enqueue. The latch swap, the
/// closed check and the registration therefore happen as ONE synchronous
/// critical section relative to the EOF seal: there is no await (and no
/// drop/re-acquire) between detecting the overflow and storing the task
/// handle, so aborting the triggering send at any await from the reservation
/// site can no longer discard the overflow — no latch, no tracked task, a
/// live child with no terminal (the r17 shape dropped the gate and re-entered
/// it asynchronously, which parked exactly there). The spawned task does ALL
/// of its awaited work — re-taking this gate for its own seal, the pre-kill
/// gate, the kill, the terminal emit — only after the caller released the
/// gate.
///
/// The frame-reader site (`map_loop`) is the single exception and calls this
/// gate-free: the natural-EOF seal runs on that SAME task immediately after
/// the loop sets `refused`, so it cannot interleave with this swap; a
/// reservation sender inside its gate neither seals nor reads the latch; a
/// second overflow loses the CAS; and the close race keeps its independent
/// recheck under the shutdown mutex below.
///
/// Either way the registration is SYNCHRONOUS (directive (c)): a reader or
/// sender that goes away before the next await still leaves a tracked,
/// launch-scoped task. The task owns its [`Arc<Launch>`]; `close` finishes it
/// before its kill ladder and a relaunch, and it signals only the launch's
/// captured child pid, so it can never reach a relaunched process through the
/// driver's current live slot. A caller that loses the race with the natural
/// EOF seal or with close (the launch already sealed) registers nothing,
/// leaving the ordinary EOF terminal or close's own ladder in charge.
fn admit_backlog_overflow_shutdown(
    inner: &Arc<Inner>,
    launch: &Arc<Launch>,
    budget: &Arc<BacklogBudget>,
    items: usize,
    bytes: usize,
    frame_bytes: usize,
    carrier: &'static str,
) -> bool {
    // Exactly one shutdown per LAUNCH (the latch lives on the per-launch
    // budget); a second overflowing frame or ticket loses and does nothing.
    if budget.overflowed.swap(true, Ordering::AcqRel) {
        return false;
    }
    if launch.closed.load(Ordering::Acquire) {
        // Natural EOF sealed first (its load and store are one section on the
        // publish gate): the ordinary StdoutEof owns the terminal. Do NOT
        // spawn — a kill must not follow an exit already being published. The
        // latch stays set; no second shutdown can spawn.
        #[cfg(feature = "test-stub")]
        test_barrier::note_overflow_spawn_stood_down(&inner.barrier);
        return false;
    }
    // Register under the launch's shutdown mutex with the seal rechecked:
    // either close has NOT claimed the launch (we spawn and store the handle
    // for close to finish) or it has (sealed; no spawn — close owns the kill).
    // close seals on THIS mutex rather than the publish gate, so its race has
    // its own recheck here; the latch section above only made the decision
    // atomic against natural EOF.
    let mut slot = launch.shutdown.lock().expect("overflow shutdown slot");
    if launch.closed.load(Ordering::Acquire) {
        return false;
    }
    // Capture the barrier slot before the Arcs move into the spawned task.
    #[cfg(feature = "test-stub")]
    let barrier_slot = Arc::clone(&inner.barrier);
    let launch = Arc::clone(launch);
    let inner = Arc::clone(inner);
    let budget = Arc::clone(budget);
    let handle = tokio::spawn(async move {
        shutdown_for_backlog_overflow(&launch, &inner, &budget, items, bytes, frame_bytes, carrier)
            .await;
    });
    *slot = Some(handle);
    drop(slot);
    // ma-sdk-state r18 item 1 (test-stub): the task handle is now stored in
    // the launch slot SYNCHRONOUSLY at the decision site — before the caller
    // reaches any await it could be aborted at. The acknowledgement is the
    // observable half of the cancellation ruling ("the latch is set AND the
    // tracked shutdown is registered").
    #[cfg(feature = "test-stub")]
    test_barrier::note_overflow_shutdown_registered(&barrier_slot);
    true
}

/// ma-sdk-state r12 item 1: the bounded publication backlog overflowed.
///
/// Overflow policy: SHUT THE SESSION DOWN with a clear diagnostic. The child is
/// the memory source (a flooding stdout producer, or one whose publication is
/// stuck behind a prompt write it refuses to drain), so it is killed directly;
/// the terminal observation rides the reserved exit permit instead of the full
/// publication/events queue.
///
/// Independence from backlog admission: none of the steps here enqueue a frame
/// or wait for the publication worker. Reservation admission is sealed under
/// the launch's publish mutex (the same gate `prepare_prompt_write` takes),
/// the kill targets the launch's captured pid/pgid with synchronous signals,
/// and terminal delivery uses the reserved permit. A concurrent `close`
/// therefore never parks on frame admission, and the blocked prompt write
/// unblocks with an error once the child's pipes close.
///
/// ma-sdk-state r14 item 2: this runs in its OWN spawned task, so the kill and
/// the terminal complete even if the triggering send was aborted while this
/// routine parks. ma-sdk-state r15 item 1: the task is launch-scoped — it
/// never reads the driver's current `live` slot, signals only the captured
/// launch-A pid, and is finished by close(A) before start(B).
async fn shutdown_for_backlog_overflow(
    launch: &Arc<Launch>,
    inner: &Arc<Inner>,
    budget: &BacklogBudget,
    items: usize,
    bytes: usize,
    frame_bytes: usize,
    carrier: &'static str,
) {
    // Seal reservation admission BEFORE the kill: a prompt that wins past this
    // point would write to a child this routine is about to kill. The publish
    // mutex serializes this with prepare_prompt_write's admission section.
    {
        let _admission = launch.publish.lock().await;
        launch.closed.store(true, Ordering::SeqCst);
    }
    #[cfg(feature = "test-stub")]
    test_barrier::note_overflow_kill_waiting(&inner.barrier);
    // ma-sdk-state r14 item 2 (test-stub): a deterministic gate BETWEEN the
    // admission seal and the kill. A regression parks the shutdown here,
    // aborts the triggering send (or runs close), and proves the kill still
    // lands after the gate opens because the task owns its own lifetime —
    // r15 item 1 further proves that after close+relaunch the kill can only
    // reach launch A's captured pid. Open by default.
    #[cfg(feature = "test-stub")]
    test_barrier::hold_before_overflow_kill(&inner.barrier).await;
    // Kill the captured launch-A child directly — SIGKILL the whole group and
    // the leader by pid. No inner.live lookup: the signals are synchronous,
    // cannot block on a child that stopped draining stdin, and a delayed
    // shutdown from a closed launch can never kill a relaunched child.
    let target = launch.kill_target;
    kill_group(target, carrier);
    kill_child_direct(target, carrier);
    let (limit_items, limit_bytes) = budget.caps();
    error!(
        carrier,
        items,
        bytes,
        frame_bytes,
        limit_items,
        limit_bytes,
        "publication backlog overflow: the child produced stdout faster than it \
         could be published; shutting the session down (reason=publication_backlog_overflow)"
    );
    let _ = emit_overflow_exit(inner, launch, budget, items, bytes).await;
    #[cfg(feature = "test-stub")]
    test_barrier::note_backlog_overflow(&inner.barrier);
}

/// Production cap on the publication backlog: at most this many stdout FRAMES
/// wait for the single publication worker (ma-sdk-state r12 item 1). The
/// reservation tickets that can accompany them are bounded separately by the
/// writer's 64-slot command channels and are not counted here. Chosen so a
/// 2 kHz flood takes ~8 s of total publication stall even before the byte cap
/// matters, while a wedged worker can never accumulate more than ~16k frames.
const PUBLICATION_BACKLOG_MAX_ITEMS: usize = 16_384;

/// Production cap on the backlog measured by queued frame bytes (each frame
/// charged its serialized JSON size plus its fixed job overhead — see
/// [`frame_backlog_cost`]). A handful of near-line-limit (8 MiB) frames suffice
/// to trip this long before the item cap, while ordinary stream-json traffic
/// never comes near either.
const PUBLICATION_BACKLOG_MAX_BYTES: usize = 128 * 1024 * 1024;

/// Fixed charge of one queued [`PublishJob::Reserve`] ticket against the SAME
/// item/byte pool as frames (ma-sdk-state r13 item 2): the queued job itself,
/// the [`TurnReservation`] the worker parks on, and the outstanding
/// [`RootTurn`] book entry kept until the turn's result maps. A child that
/// keeps reading prompts (so writes complete) while the publication worker is
/// wedged on an undrained RunHandle would otherwise accumulate tickets and
/// outstanding turns without limit — every one emitting one `turn_started` the
/// consumer never takes.
const RESERVATION_BACKLOG_COST: usize = std::mem::size_of::<PublishJob>()
    + std::mem::size_of::<TurnReservation>()
    + std::mem::size_of::<RootTurn>();

/// Why a frame was refused admission to the publication backlog.
#[derive(Debug, PartialEq, Eq)]
enum BacklogAdmitError {
    /// The queue is closed (close in progress) or overflow already fired.
    Closed,
    /// Admitting the frame would cross the item or byte cap. Carries the
    /// current occupancy and the refused frame's cost so the caller can emit a
    /// diagnostic naming exactly what tripped the limit.
    Overflow {
        items: usize,
        bytes: usize,
        frame_bytes: usize,
    },
}

/// Driver-lifetime CONFIGURATION for the bounded publication backlog
/// (ma-sdk-state r14 item 1, directive (a)). Only the caps live here: tests
/// shrink them before `start`, and every launch builds a fresh
/// [`BacklogBudget`] from them. No live occupancy lives here — that is why a
/// relaunch cannot inherit or corrupt a previous launch's accounting.
struct BacklogConfig {
    limit_items: std::sync::atomic::AtomicUsize,
    limit_bytes: std::sync::atomic::AtomicUsize,
}

impl BacklogConfig {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            limit_items: std::sync::atomic::AtomicUsize::new(PUBLICATION_BACKLOG_MAX_ITEMS),
            limit_bytes: std::sync::atomic::AtomicUsize::new(PUBLICATION_BACKLOG_MAX_BYTES),
        })
    }

    #[cfg(any(test, feature = "test-stub"))]
    fn set(&self, items: usize, bytes: usize) {
        self.limit_items.store(items, Ordering::Release);
        self.limit_bytes.store(bytes, Ordering::Release);
    }

    #[cfg(any(test, feature = "test-stub"))]
    fn caps(&self) -> (usize, usize) {
        (
            self.limit_items.load(Ordering::Acquire),
            self.limit_bytes.load(Ordering::Acquire),
        )
    }
}

/// Driver-local launch sequence for [`BacklogBudget::id`].
static BACKLOG_BUDGET_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// PER-LAUNCH accounting for the bounded publication backlog (ma-sdk-state r14
/// item 1, directives (a)/(b)).
///
/// Two semaphores are the whole accounting: one permit per admitted item, and
/// `frame_backlog_cost` (or [`RESERVATION_BACKLOG_COST`]) permits per admitted
/// byte charge. Admission is `try_acquire_many_owned`; the owned permits move
/// INTO the queued job and are released by `Drop`, so completed, cancelled,
/// aborted and discarded jobs all return their charge exactly once, with no
/// explicit release call. Nothing is shared across launches: the budget is
/// created in `launch`, owned by the launch's [`PublishQueue`], and kept alive
/// by outstanding permits themselves, so a launch-A task that releases after a
/// relaunch touches only launch A's semaphores.
///
/// The high-water gauges and the overflow latch are OBSERVATIONAL, not
/// admission state: admission itself is decided entirely by the semaphores —
/// no read-then-store counter can exist (the r12/r13 races), and the latch only
/// chooses whether to spawn the one shutdown task.
struct BacklogBudget {
    /// Monotonic per-launch identity, so a regression can assert a relaunch
    /// built a fresh budget.
    #[cfg_attr(not(any(test, feature = "test-stub")), allow(dead_code))]
    id: usize,
    items: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    cap_items: usize,
    cap_bytes: usize,
    /// High-water gauges for the life of the launch (regressions assert the
    /// backlog never crossed the configured caps).
    items_high: std::sync::atomic::AtomicUsize,
    bytes_high: std::sync::atomic::AtomicUsize,
    /// Set exactly once, when the detached overflow shutdown task is spawned.
    overflowed: std::sync::atomic::AtomicBool,
}

impl BacklogBudget {
    fn new(cap_items: usize, cap_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            id: BACKLOG_BUDGET_SEQ.fetch_add(1, Ordering::Relaxed),
            items: Arc::new(Semaphore::new(cap_items)),
            bytes: Arc::new(Semaphore::new(cap_bytes)),
            cap_items,
            cap_bytes,
            items_high: std::sync::atomic::AtomicUsize::new(0),
            bytes_high: std::sync::atomic::AtomicUsize::new(0),
            overflowed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    #[cfg(any(test, feature = "test-stub"))]
    fn id(&self) -> usize {
        self.id
    }

    fn caps(&self) -> (usize, usize) {
        (self.cap_items, self.cap_bytes)
    }

    /// Permits currently held by admitted but unreleased jobs.
    fn occupied(&self) -> (usize, usize) {
        (
            self.cap_items
                .saturating_sub(self.items.available_permits()),
            self.cap_bytes
                .saturating_sub(self.bytes.available_permits()),
        )
    }

    /// Try to reserve one item permit and `cost` byte permits (directive (c)).
    /// The returned [`BacklogCharge`] releases by Drop; there is deliberately
    /// no separate release call. A charge larger than the byte cap is reported
    /// as overflow rather than wrapping.
    fn acquire(&self, cost: usize) -> Result<BacklogCharge, BacklogAdmitError> {
        if self.overflowed.load(Ordering::Acquire) {
            return Err(BacklogAdmitError::Closed);
        }
        let cost_u32 = u32::try_from(cost).unwrap_or(u32::MAX);
        // The owned acquire consumes an `Arc<Semaphore>` (the permit keeps it
        // alive); the clone is a refcount bump.
        let items_permit = match self.items.clone().try_acquire_many_owned(1) {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => return Err(self.overflow_error(cost)),
            // Semaphores are never closed: only permit exhaustion exists.
            Err(TryAcquireError::Closed) => {
                unreachable!("backlog item semaphore is never closed")
            }
        };
        let bytes_permit = match self.bytes.clone().try_acquire_many_owned(cost_u32) {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                // The item permit rolls back by Drop before the error returns.
                drop(items_permit);
                return Err(self.overflow_error(cost));
            }
            Err(TryAcquireError::Closed) => {
                unreachable!("backlog byte semaphore is never closed")
            }
        };
        let (items_now, bytes_now) = self.occupied();
        self.items_high.fetch_max(items_now, Ordering::AcqRel);
        self.bytes_high.fetch_max(bytes_now, Ordering::AcqRel);
        Ok(BacklogCharge {
            items: items_permit,
            bytes: bytes_permit,
        })
    }

    fn overflow_error(&self, frame_bytes: usize) -> BacklogAdmitError {
        let (items, bytes) = self.occupied();
        BacklogAdmitError::Overflow {
            items,
            bytes,
            frame_bytes,
        }
    }

    #[cfg(any(test, feature = "test-stub"))]
    fn snapshot(self: &Arc<Self>) -> BacklogSnapshot {
        let (items, bytes) = self.occupied();
        BacklogSnapshot {
            items,
            bytes,
            items_high: self.items_high.load(Ordering::Acquire),
            bytes_high: self.bytes_high.load(Ordering::Acquire),
            overflowed: self.overflowed.load(Ordering::Acquire),
            limit_items: self.cap_items,
            limit_bytes: self.cap_bytes,
        }
    }
}

/// One admitted job's owned budget permits (ma-sdk-state r14 item 1, directive
/// (b)). Carried inside every queued FRAME and RESERVE job; releasing is just
/// dropping the job, so every exit path — worker drain, queue teardown on
/// close/abort, refused enqueue, caller cancellation — gives the charge back
/// exactly once.
#[allow(dead_code)] // the fields exist to be held, not read
struct BacklogCharge {
    items: tokio::sync::OwnedSemaphorePermit,
    bytes: tokio::sync::OwnedSemaphorePermit,
}

/// Test-only snapshot of the bounded publication backlog (r12 item 1).
#[cfg(any(test, feature = "test-stub"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacklogSnapshot {
    /// Frames currently queued.
    pub items: usize,
    /// Bytes currently queued.
    pub bytes: usize,
    /// Largest occupancy reached this launch.
    pub items_high: usize,
    /// Largest byte occupancy reached this launch.
    pub bytes_high: usize,
    /// Whether the overflow shutdown already fired.
    pub overflowed: bool,
    /// Active launch's item cap.
    pub limit_items: usize,
    /// Active launch's byte cap.
    pub limit_bytes: usize,
}

#[cfg(any(test, feature = "test-stub"))]
impl BacklogSnapshot {
    /// Snapshot reported before any launch installed a budget.
    fn idle(config: &BacklogConfig) -> BacklogSnapshot {
        let (limit_items, limit_bytes) = config.caps();
        BacklogSnapshot {
            items: 0,
            bytes: 0,
            items_high: 0,
            bytes_high: 0,
            overflowed: false,
            limit_items,
            limit_bytes,
        }
    }
}

/// Admission gate around the otherwise-unbounded publication channel
/// (ma-sdk-state r12 item 1; r13 item 2; r14 item 1 ownership rewrite).
///
/// FRAME jobs and Reserve tickets take owned permits from the launch's shared
/// byte/item semaphores (tickets reserve theirs BEFORE their turn-book entry
/// opens); the remaining CONTROL jobs (the terminal EOF, test fences) always
/// pass as long as the channel is open so natural-EOF sealing and close never
/// wait on backlog admission — the bounded backlog must not recreate the
/// shutdown deadlock an unbounded queue was chosen to avoid. Cancelling an
/// already-queued reservation enqueues nothing and therefore stays independent
/// of admission too.
#[derive(Clone)]
struct PublishQueue {
    tx: mpsc::UnboundedSender<PublishJob>,
    budget: Arc<BacklogBudget>,
}

impl PublishQueue {
    /// Create the publication channel and fresh accounting for one launch.
    fn new(budget: Arc<BacklogBudget>) -> (Self, mpsc::UnboundedReceiver<PublishJob>) {
        let (tx, rx) = mpsc::unbounded_channel::<PublishJob>();
        (Self { tx, budget }, rx)
    }

    fn budget(&self) -> &Arc<BacklogBudget> {
        &self.budget
    }

    fn budget_clone(&self) -> Arc<BacklogBudget> {
        Arc::clone(&self.budget)
    }

    /// Try to admit a stdout FRAME under both caps. The frame is boxed here so
    /// the admitted job carries it together with its permits. On refusal the
    /// caller owns the frame again (the reader drops it on overflow); no
    /// permits were attached, so nothing needs releasing. On success the
    /// permits move INTO the job, and both the queued job and a send that fails
    /// because the worker is gone release them by Drop.
    fn admit_frame(&self, frame: Box<Outbound>) -> Result<(), (BacklogAdmitError, Box<Outbound>)> {
        let cost = frame_backlog_cost(&frame);
        let charge = match self.budget.acquire(cost) {
            Ok(charge) => charge,
            Err(error) => return Err((error, frame)),
        };
        let charged = PublishJob::Frame { frame, charge };
        // The channel is unbounded: this send only fails once the worker is
        // gone (close dropped the receiver), never on capacity. The returned
        // job carries the permits and releases them as the caller drops it.
        if let Err(mpsc::error::SendError(returned)) = self.tx.send(charged) {
            let PublishJob::Frame { frame, .. } = returned else {
                unreachable!("the channel was just sent a Frame");
            };
            return Err((BacklogAdmitError::Closed, frame));
        }
        Ok(())
    }

    /// Reserve one Reserve-ticket charge under the shared item/byte caps,
    /// BEFORE the turn-book entry exists. On success the caller owns the
    /// permits and must move them into the queued ticket; dropping them gives
    /// the charge back if the ticket never makes it into the queue.
    fn reserve_charge(&self) -> Result<BacklogCharge, BacklogAdmitError> {
        self.budget.acquire(RESERVATION_BACKLOG_COST)
    }

    /// Admit a CONTROL job (terminal EOF / test fence) without consulting the
    /// frame caps. Fails only when the channel is closed. Reserve tickets do
    /// NOT go through here: they take a charge under the shared caps via
    /// [`Self::reserve_charge`] before their book entry is opened.
    fn admit_control(&self, job: PublishJob) -> Result<(), mpsc::error::SendError<PublishJob>> {
        self.tx.send(job)
    }
}

/// Memory a queued frame is charged: its serialized JSON (a lower bound on the
/// parsed heap footprint — the frame can only be larger in memory) plus the
/// fixed size of the queue job that owns it. Serialization failing is only
/// possible for a non-string map key, which the wire parser cannot produce;
/// such a frame is conservatively charged the fixed job cost alone.
fn frame_backlog_cost(frame: &Outbound) -> usize {
    let json_bytes = serde_json::to_vec(frame).map_or(0, |buf| buf.len());
    std::mem::size_of::<PublishJob>() + json_bytes
}

/// One ordered unit of publication work. See [`Inner::publish`].
enum PublishJob {
    /// Map one already-prefolded stdout frame and emit its observations.
    /// Boxed: `Outbound` is ~1 KiB while the other jobs are tiny, and this job
    /// already crosses the unbounded queue boundary. `charge` is the job's
    /// owned backlog permits, released by Drop when the worker finishes the
    /// job (or when a queued job is discarded on close/abort) — ma-sdk-state
    /// r14 item 1 directive (b).
    Frame {
        frame: Box<Outbound>,
        #[allow(dead_code)] // held, released on Drop
        charge: BacklogCharge,
    },
    /// A turn's publication ticket, enqueued by the send task BEFORE its prompt
    /// write. It occupies the turn's exact FIFO position: the worker parks here
    /// until the write resolves, then on commit clears the mapper reservation
    /// and emits `turn_started` AT THIS POSITION (no-op start on cancel). Every
    /// frame enqueued after it — including the turn's own result — is therefore
    /// published behind its start. Its reservation charge rides the same way a
    /// frame's does and is held until the ticket is fully handled.
    Reserve {
        reservation: Arc<TurnReservation>,
        #[allow(dead_code)] // held, released on Drop
        charge: BacklogCharge,
    },
    /// The child's stdout closed; emit exactly one `exited` (deduped).
    StdoutEof,
    /// Ordered publication fence (ma-sdk-state r9 item 4): when the single worker
    /// drains this job (i.e. every job enqueued before it has been published),
    /// it signals the oneshot. A test enqueues it after a rollback and awaits
    /// it under a timeout instead of polling with a quiet-period loop.
    #[cfg(feature = "test-stub")]
    Fence(tokio::sync::oneshot::Sender<()>),
}

/// A prompt write's turn ticket (ma-sdk-state r5 item 1).
///
/// It lives in the publication queue as a [`PublishJob::Reserve`] job, never
/// in a side slot. The mapper book is opened (`begin_turn`) BEFORE the write,
/// so a result the child emits while the write is still in flight maps against
/// the pre-opened turn and can never project idle before the turn starts. The
/// worker parks on [`TurnReservation::committed`] at the ticket's queue
/// position; on commit it emits turn_started there, ahead of every frame that
/// follows the ticket. On cancel it resumes without emitting a start.
struct TurnReservation {
    /// Per-turn book seq, so the worker clearing `reserved` at commit matches
    /// THIS turn and never clobbers a newer turn's in-flight reservation.
    seq: u64,
    client_message_id: String,
    committed: tokio::sync::Notify,
    /// Set BEFORE the notify in BOTH commit and cancel, closing the lost-wakeup
    /// window: a waiter that registers `committed.notified()` after the notify
    /// still observes this flag in [`TurnReservation::wait_for_commit`] instead
    /// of parking forever (ma-sdk-state r5 item 3).
    resolved: std::sync::atomic::AtomicBool,
    /// Whether the resolution was a successful commit (vs. a cancel).
    is_committed: std::sync::atomic::AtomicBool,
    /// Set once the turn_started observation has been emitted (a single Reserve
    /// ticket claims it; the flag is defensive dedupe).
    start_emitted: std::sync::atomic::AtomicBool,
}

impl TurnReservation {
    fn new(seq: u64, client_message_id: String) -> Self {
        Self {
            seq,
            client_message_id,
            committed: tokio::sync::Notify::new(),
            resolved: std::sync::atomic::AtomicBool::new(false),
            is_committed: std::sync::atomic::AtomicBool::new(false),
            start_emitted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Wrap in an [`Arc`] for sharing between the send task and the worker.
    fn shared(seq: u64, client_message_id: String) -> Arc<Self> {
        Arc::new(Self::new(seq, client_message_id))
    }

    /// The write succeeded: unpark a waiter parked at the ticket.
    fn commit(&self) {
        self.is_committed
            .store(true, std::sync::atomic::Ordering::Release);
        self.resolved
            .store(true, std::sync::atomic::Ordering::Release);
        self.committed.notify_waiters();
    }

    /// The write failed: resolve the ticket without a start so a parked worker
    /// resumes and queued frames flow as-is.
    fn cancel(&self) {
        self.resolved
            .store(true, std::sync::atomic::Ordering::Release);
        self.committed.notify_waiters();
    }

    async fn wait_for_commit(&self) -> bool {
        if self.resolved.load(std::sync::atomic::Ordering::Acquire) {
            return self.is_committed.load(std::sync::atomic::Ordering::Acquire);
        }
        let notified = self.committed.notified();
        if self.resolved.load(std::sync::atomic::Ordering::Acquire) {
            return self.is_committed.load(std::sync::atomic::Ordering::Acquire);
        }
        notified.await;
        self.is_committed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Claim the right to emit this turn's turn_started observation. Returns
    /// true exactly once; a single Reserve ticket calls it, so the claim is a
    /// defensive dedupe against a duplicate/direct ticket.
    fn claim_start(&self) -> bool {
        !self
            .start_emitted
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }
}

/// Roll a cancelled reservation's book back AT ITS TICKET POSITION on the
/// single publication worker and emit the root settle it releases right there
/// (ma-sdk-state r9 item 1; r10 item 2). Both cancellation paths — the
/// detached write-completion task (`ReservationGuard::fail`) and caller
/// cancellation pre-accept (`Drop`) — only RESOLVE THE TICKET; the worker runs
/// this when it reaches that ticket, so:
///
/// * the rollback happens before ANY frame/job queued behind the ticket is
///   processed (a later turn C's turn_started can no longer precede the
///   released settle of A);
/// * the correction can never be appended at the queue tail, where a queued
///   `StdoutEof` could overtake it;
/// * there is exactly one rollback path per reservation.
///
/// Does NOT take `write_seq`: the worker never holds it.
async fn rollback_reservation_at_ticket(
    inner: &Arc<Inner>,
    launch: &Launch,
    seq: u64,
) -> DriverResult<()> {
    let released = {
        let mut mapper = inner.mapper.lock().await;
        mapper.cancel_turn_reservation(seq)?
    };
    let Some(observation) = released else {
        return Ok(());
    };
    // Emit HERE, at the cancelled ticket's position — before the worker takes
    // the next job (C's start, a frame, StdoutEof).
    //
    // Defense in depth (ma-sdk-state r11 item 2): the EOF admission seal makes
    // a cancelled ticket behind StdoutEof unreachable, but never publish after
    // the terminal observation regardless of how the ticket got there. The
    // book is still rolled back above; the launch is ending, so the owed
    // correction is dropped the same way rollback_without_ticket drops one.
    if launch.exit_emitted.load(Ordering::Acquire) {
        debug!("ordered publication: deferred settle released after exit; dropped");
        return Ok(());
    }
    emit_all(launch, vec![observation]).await
}

/// Roll back a reservation whose ticket NO worker will ever visit: the Reserve
/// send failed because the publisher is closed (close in progress), or
/// panic-safety Drop fired between opening the book and enqueuing the ticket
/// (r10 item 1). Any deferred settle this releases cannot be published in
/// order — the launch is shutting down and close seals the events channel — so
/// it is dropped.
///
/// Does NOT take `write_seq`: a caller can drop the guard while still parked
/// on `write_seq` itself, so re-acquiring it here could deadlock.
async fn rollback_without_ticket(inner: &Arc<Inner>, seq: u64) {
    // test-stub (r11 item 3): acknowledge a caller parked on the mapper lock
    // inside this rollback, so a test can abort it there and prove the
    // reservation is still cleaned up.
    #[cfg(feature = "test-stub")]
    test_barrier::note_rollback_without_ticket_wait(&inner.barrier);
    let released = {
        let mut mapper = inner.mapper.lock().await;
        match mapper.cancel_turn_reservation(seq) {
            Ok(released) => released,
            Err(error) => {
                warn!(%error, "turn rollback failed during reservation cleanup");
                None
            }
        }
    };
    if released.is_some() {
        debug!("publication gone; released deferred settle dropped on close");
    }
    #[cfg(feature = "test-stub")]
    test_barrier::note_rollback_without_ticket_done(&inner.barrier);
}

/// Owns one turn's [`TurnReservation`] for the lifetime of a prompt write
/// (ma-sdk-state r6/r7). The reservation lifecycle is driven by the detached,
/// ordered write-completion task (not the caller's await), so an aborted caller
/// cannot cancel a prompt the writer still writes. `commit`/`fail` are called
/// exactly once from that task; `Drop` is only panic safety.
struct ReservationGuard {
    inner: Arc<Inner>,
    reservation: Arc<TurnReservation>,
    resolved: bool,
    /// Whether the Reserve ticket was actually enqueued. Between the book
    /// entry opening and the (synchronous) send there is no await, so this is
    /// false only when the send failed (handled explicitly) or a panic fired
    /// in that gap (r10 item 1).
    ticket_queued: bool,
}

impl ReservationGuard {
    fn new(inner: Arc<Inner>, reservation: Arc<TurnReservation>) -> Self {
        Self {
            inner,
            reservation,
            resolved: false,
            ticket_queued: false,
        }
    }

    /// The Reserve send succeeded; this guard's ticket owns the book entry.
    fn mark_ticket_queued(&mut self) {
        self.ticket_queued = true;
    }

    /// The writer committed the prompt: keep the reservation so the worker emits
    /// the start; the matching `Drop` is then a no-op.
    fn commit(mut self) {
        self.resolved = true;
        self.reservation.commit();
    }

    /// The writer definitively did NOT write the prompt (pre-accept abort,
    /// post-accept write failure). ONLY resolve the ticket: the publication
    /// worker rolls the book back at this ticket's FIFO position and emits the
    /// released settle there (r10 item 2), so the correction can never land
    /// behind a later turn's start or a queued StdoutEof. Consumes the guard
    /// and lets it DROP normally (r7 item 3: no `mem::forget`, no Arc leak).
    fn fail(mut self) {
        self.resolved = true;
        self.reservation.cancel();
    }

    /// The Reserve send failed (publisher closed) — no ticket exists for the
    /// just-opened book entry, so roll it back directly and discard any
    /// released settle. Consumes the guard.
    ///
    /// ma-sdk-state r11 item 3: `resolved` is marked ONLY after the rollback
    /// completes, never before its mapper-lock await. A caller cancelled while
    /// parked on that lock drops this guard with `resolved == false`, so the
    /// `Drop` no-ticket cleanup stays armed across the await and hands the
    /// rollback to a cancellation-independent detached task; marking resolved
    /// first made that Drop a no-op and orphaned the open book entry.
    async fn fail_without_ticket(mut self) {
        // test-stub: park here, just before the rollback, so a test can hold
        // the mapper lock and deterministically abort the caller inside the
        // rollback's lock wait. Open by default; no-op without an armed barrier.
        #[cfg(feature = "test-stub")]
        test_barrier::hold_before_rollback_without_ticket(&self.inner.barrier).await;
        rollback_without_ticket(&self.inner, self.reservation.seq).await;
        self.resolved = true;
        self.reservation.cancel();
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        // Pre-accept cancellation (r8 item 1) / panic safety.
        if self.ticket_queued {
            // The Reserve ticket is already in the publication queue, so all
            // this does is wake the parked worker: the WORKER consumes the
            // cancelled ticket at its FIFO position, rolling the book back and
            // emitting the released settle THERE (see the Reserve arm).
            // Keeping the rollback off a detached task here prevents a
            // cancelled B from lingering in the book while a later C result is
            // mapped (r9 item 2), and emitting at the ticket position keeps
            // the correction ahead of C's start/StdoutEof (r10 item 2).
            self.reservation.cancel();
            return;
        }
        // No ticket owns this open book entry (aborted while parked BEFORE the
        // Reserve send — on the publish mutex — r10 item 1; or panic safety in
        // the open→enqueue gap). The worker cannot roll it back from a queue
        // position, so roll it back directly on a detached task. Do NOT take
        // write_seq: the dropping caller may itself be parked on it.
        let inner = self.inner.clone();
        let seq = self.reservation.seq;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Drop the JoinHandle immediately: detach the cleanup task; it only
            // touches the mapper/publish state, both of which outlive this
            // guard (the driver holds them).
            drop(handle.spawn(async move {
                rollback_without_ticket(&inner, seq).await;
            }));
        }
        self.reservation.cancel();
    }
}

/// Test-only publication barrier (feature `test-stub`).
///
/// Deterministically recreates the turn-order race with the ticket IN the
/// publication queue (r5 item 1): a prompt write has acked but the send task
/// has not committed its reservation, while (a) the publication worker is
/// parked at that turn's `Reserve` ticket and (b) the reader has already
/// enqueued the child's result frame BEHIND the ticket.
///
/// ma-sdk-state r6 item 2: the barrier is scoped to ONE driver via its
/// [`Inner`] slot, not a process-global, so tests that arm it run safely in the
/// same binary alongside other tests driving their own children. [`Drop`]
/// releases the gates so a failed assertion never hangs the send/worker.
#[cfg(feature = "test-stub")]
pub mod test_barrier {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Per-driver slot holding the currently armed barrier, if any. `Arc` so a
    /// launch can share the exact slot the driver (re)arms — a launch
    /// outlives its `start` call and emits from its own tasks.
    pub(super) type Slot = Arc<Mutex<Option<Arc<State>>>>;

    pub(super) struct State {
        /// ma-sdk-state r13 item 5: signalled AFTER every hook below updates one
        /// of the acknowledgement counters/flags, so `wait_until_*` helpers park
        /// on this instead of polling the atomics on a 2 ms timer. Waiters always
        /// re-check their predicate after waking, so notify_waiters' lack of a
        /// stored permit is harmless (no predicate is ever lost).
        state_changed: tokio::sync::Notify,
        write_reached: AtomicBool,
        /// The worker dequeued the turn's Reserve ticket and is parked on the
        /// not-yet-committed write.
        reserve_parked: AtomicBool,
        /// The reader enqueued a frame (the child's result) behind the parked
        /// ticket.
        result_queued: AtomicBool,
        /// r11 item 4(a): total RESULT frames the reader enqueued. Unlike the
        /// one-shot `result_queued` flag, this acknowledges specific results
        /// (e.g. C's answer) BEFORE a natural child kill queues StdoutEof.
        result_enqueued_count: AtomicUsize,
        /// Number of Reserve tickets the send task has enqueued.
        reserve_enqueued: AtomicUsize,
        /// Gates the SEND task parked in `hold_write_commit`.
        send_released: AtomicBool,
        send_release: tokio::sync::Notify,
        /// Gates the WORKER just after it emits the parked turn's start, so a
        /// test can register a second turn's reservation before the worker maps
        /// the first turn's buffered result.
        worker_released: AtomicBool,
        worker_resume: tokio::sync::Notify,
        /// r7 item 2: the detached write task has enqueued Reserve and is parked
        /// immediately BEFORE awaiting the writer ack, so a test can abort the
        /// caller while the write is still in flight.
        write_parked: AtomicBool,
        write_gate_open: AtomicBool,
        write_gate: tokio::sync::Notify,
        /// 1-based index of the writer call at/after which the pre-ack gate
        /// parks (`MAX` = never). Lets a test let prompt A's write run but hold
        /// prompt B's failed write until A's result has been mapped (r7 items
        /// 2/5).
        write_number: AtomicUsize,
        park_from_write: AtomicUsize,
        /// r8 item 3: incremented each time the publication worker has MAPPED a
        /// Result frame (after the mapper lock released). A test waits for the
        /// specific count (A's result) before releasing B's failure.
        result_mapped_count: AtomicUsize,
        /// r10 item 1: number of send callers that reached the `write_seq`
        /// lock await. With the previous caller still holding it, count `n`
        /// ACKNOWLEDGES the nth caller parked behind (the lock is held).
        write_seq_wait_count: AtomicUsize,
        /// r10 item 1: number of send callers that reached the publish-mutex
        /// lock await. With the mutex externally held, count `n` acknowledges
        /// the nth caller parked there BEFORE its book entry was opened.
        publish_lock_wait_count: AtomicUsize,
        /// r10 item 2: the worker dequeued a ticket that was already cancelled
        /// and is parked just before the at-position rollback, so a test can
        /// queue a later reservation behind it before the correction is
        /// emitted.
        cancelled_ticket_reached: AtomicBool,
        cancelled_gate_open: AtomicBool,
        cancelled_gate: tokio::sync::Notify,
        /// r12 item 2: number of CANCELLED tickets whose at-position rollback
        /// the worker COMPLETED (not merely reached), so a test can prove a
        /// newer turn's cancellation was fully processed BEFORE releasing an
        /// older turn's result frame.
        cancelled_rollback_done: AtomicUsize,
        /// r12 item 2: reader gate that withholds a turn RESULT frame from the
        /// publication queue. Open by default; a test shuts it to stop a later
        /// turn's result racing ahead of a cancelled ticket's processing.
        result_enqueue_parked: AtomicBool,
        result_enqueue_open: AtomicBool,
        result_enqueue_gate: tokio::sync::Notify,
        /// r11 item 2: the reader observed natural stdout EOF, sealed
        /// reservation admission, and queued StdoutEof from the seal critical
        /// section. A send attempted only after this is set must lose the
        /// admission race deterministically.
        stdout_eof_queued: AtomicBool,
        /// r12 item 1: the bounded publication backlog tripped and the
        /// overflow session-shutdown fired (terminal diagnostic emitted).
        backlog_overflowed: AtomicBool,
        /// r13 item 3: the overflow shutdown sealed admission and is ABOUT TO
        /// acquire the (contended) `live` handle for its guaranteed kill, so a
        /// test holding that handle can acknowledge the kill waiting without
        /// racing it.
        overflow_kill_waiting: AtomicBool,
        /// r11 item 3: a fail_without_ticket caller reached the rollback (set
        /// right before its mapper-lock await). With the mapper externally
        /// held, count n acknowledges n callers parked in that wait.
        rollback_wait_count: AtomicUsize,
        /// r11 item 3: a rollback_without_ticket completed (book rolled back),
        /// including one spawned by the cancellation-independent Drop cleanup.
        rollback_done_count: AtomicUsize,
        /// r11 item 3: fail_without_ticket parks here BEFORE its rollback while
        /// the gate is shut, so a test can first grab the mapper lock and then
        /// release the caller INTO the contended mapper-lock wait.
        rollback_parked: AtomicBool,
        rollback_gate_open: AtomicBool,
        rollback_gate: tokio::sync::Notify,
        /// ma-sdk-state r14 item 1: number of ordinary emissions that found the
        /// observation channel at zero capacity and are parking for a consumer
        /// slot — the acknowledged saturation point (replaces sleep-polling the
        /// channel capacity).
        emit_waiting_count: AtomicUsize,
        /// ma-sdk-state r14 item 2: gate between the overflow admission seal
        /// and the live-handle kill. OPEN by default (the shutdown proceeds); a
        /// regression shuts it to park the detached shutdown, abort the
        /// triggering send, and prove the kill still happens.
        overflow_kill_parked: AtomicBool,
        overflow_kill_gate_open: AtomicBool,
        overflow_kill_gate: tokio::sync::Notify,
        /// ma-sdk-state r14 item 1: the worker has fully handled a charged job
        /// but its owned permits are still held (the pre-release gate is
        /// shut). A regression parks the publisher here across close and
        /// relaunch to prove the release lands on the DEAD launch's budget
        /// only.
        charge_release_parked: AtomicBool,
        charge_release_open: AtomicBool,
        charge_release_gate: tokio::sync::Notify,
        /// ma-sdk-state r15 item 4: number of stdout FRAMES the reader refused
        /// with `Closed` — admission already tripped (e.g. a reservation
        /// breach) or the receiver was dropped — while the child's stdout is
        /// still open. Acknowledges the "one more frame after the paused
        /// reservation overflow" point WITHOUT a sleep; on this refusal the
        /// reader must now stop WITHOUT enqueuing StdoutEof.
        closed_frame_refusals: AtomicUsize,
        /// ma-sdk-state r15 item 4: number of frames the reader refused with
        /// `Overflow` (its own admission site tripped).
        overflow_frame_refusals: AtomicUsize,
        /// ma-sdk-state r15 item 4: the reader task FINISHED after a frame
        /// admission refusal — set at map_loop's end, AFTER any (now
        /// forbidden) StdoutEof enqueue, so a test can enqueue an
        /// acknowledgement fence knowing the reader will no longer insert a
        /// terminal job ahead of it.
        reader_finished_after_refusal: AtomicBool,
        /// ma-sdk-state r16 item 1: a GENUINE stdout EOF (recv() returned None)
        /// reached the admission critical section AFTER an overflow had won
        /// terminal ownership, so ordinary StdoutEof publication was
        /// suppressed — the spawned overflow task delivers the one terminal.
        eof_suppressed_after_overflow: AtomicBool,
        /// ma-sdk-state r15 item 6: number of triggering sends parked on the
        /// post-spawn overflow-refusal gate (the shutdown is in progress
        /// while the send waits here — the exact point an abort must land).
        overflow_refusal_parked_count: AtomicUsize,
        overflow_refusal_gate_open: AtomicBool,
        overflow_refusal_gate: tokio::sync::Notify,
        /// ma-sdk-state r17 item 1: the natural-EOF reader is parked in its
        /// publish-gate seal section AFTER loading the overflow latch but
        /// BEFORE its closed store / StdoutEof decision, still holding the
        /// gate.
        eof_seal_decision_parked: AtomicBool,
        eof_seal_decision_open: AtomicBool,
        eof_seal_decision_gate: tokio::sync::Notify,
        /// ma-sdk-state r17 item 1; relocated by ma-sdk-state r18 item 1:
        /// number of triggering overflow sends parked in the reservation
        /// admission section AFTER the synchronous latch swap/closed check
        /// and the tracked-task registration, still HOLDING the publish gate
        /// — the exact point the pre-fix code dropped the gate and re-entered
        /// it asynchronously.
        overflow_spawn_parked_count: AtomicUsize,
        overflow_spawn_gate_open: AtomicBool,
        overflow_spawn_gate: tokio::sync::Notify,
        /// ma-sdk-state r17 item 1: number of overflow decisions that reached
        /// the latch/closed check and found the launch ALREADY sealed (natural
        /// EOF won), so they stood down without registering a shutdown.
        overflow_spawn_standdowns: AtomicUsize,
        /// ma-sdk-state r18 item 1: number of overflow shutdown tasks
        /// REGISTERED synchronously at the overflow decision site (the handle
        /// is stored in the launch slot before the decision section returns).
        /// With the send parked at `overflow_spawn_gate` this is already 1,
        /// which is how a regression proves aborting the parked send cannot
        /// discard the latch or the tracked task.
        overflow_shutdown_registered: AtomicUsize,
        /// ma-sdk-state r18 item 2: number of natural-EOF readers that
        /// reached the publish-gate acquisition, fired immediately BEFORE the
        /// lock await.
        eof_seal_lock_wait_count: AtomicUsize,
        /// ma-sdk-state r19 item 1: number of natural-EOF readers whose
        /// acquisition-point probe found the publish gate ALREADY HELD (a
        /// `try_lock` answered `WouldBlock`). This POSITIVE contention report
        /// replaces the r18 item 2 absence window: the reader has provably
        /// reached the acquisition (its lock-wait ack fired) and the gate is
        /// held at that same instant, so the reader is blocked ON the guard —
        /// a reader that merely has not been scheduled yet reports nothing.
        /// While the proof runs the only possible holder is the overflowing
        /// send parked in its admission section: the spawned shutdown is a
        /// queued waiter behind it, and fence/close acquisitions are absent.
        eof_seal_lock_contended_count: AtomicUsize,
        /// ma-sdk-state r18 item 2: number of natural-EOF readers that
        /// ENTERED the publish gate for the seal (lock acquired; the latch
        /// load/seal decision has not run yet).
        eof_seal_entered_count: AtomicUsize,
    }

    /// An armed write-commit barrier. Releases on drop.
    pub struct WriteCommitBarrier {
        state: Arc<State>,
    }

    /// Arm the barrier on one driver's slot.
    #[must_use]
    pub(super) fn arm(slot: &Slot) -> WriteCommitBarrier {
        let state = Arc::new(State {
            state_changed: tokio::sync::Notify::new(),
            write_reached: AtomicBool::new(false),
            reserve_parked: AtomicBool::new(false),
            result_queued: AtomicBool::new(false),
            result_enqueued_count: AtomicUsize::new(0),
            reserve_enqueued: AtomicUsize::new(0),
            send_released: AtomicBool::new(false),
            send_release: tokio::sync::Notify::new(),
            worker_released: AtomicBool::new(false),
            worker_resume: tokio::sync::Notify::new(),
            write_parked: AtomicBool::new(false),
            write_gate_open: AtomicBool::new(false),
            write_gate: tokio::sync::Notify::new(),
            write_number: AtomicUsize::new(0),
            park_from_write: AtomicUsize::new(usize::MAX),
            result_mapped_count: AtomicUsize::new(0),
            write_seq_wait_count: AtomicUsize::new(0),
            publish_lock_wait_count: AtomicUsize::new(0),
            cancelled_ticket_reached: AtomicBool::new(false),
            // Open by default: cancellations roll back at the ticket without a
            // hold. A test that must enqueue a later reservation between the
            // cancelled ticket and its rollback shuts this explicitly.
            cancelled_gate_open: AtomicBool::new(true),
            cancelled_gate: tokio::sync::Notify::new(),
            cancelled_rollback_done: AtomicUsize::new(0),
            result_enqueue_parked: AtomicBool::new(false),
            result_enqueue_open: AtomicBool::new(true),
            result_enqueue_gate: tokio::sync::Notify::new(),
            stdout_eof_queued: AtomicBool::new(false),
            backlog_overflowed: AtomicBool::new(false),
            overflow_kill_waiting: AtomicBool::new(false),
            rollback_wait_count: AtomicUsize::new(0),
            rollback_done_count: AtomicUsize::new(0),
            rollback_parked: AtomicBool::new(false),
            rollback_gate_open: AtomicBool::new(true),
            rollback_gate: tokio::sync::Notify::new(),
            emit_waiting_count: AtomicUsize::new(0),
            overflow_kill_parked: AtomicBool::new(false),
            overflow_kill_gate_open: AtomicBool::new(true),
            overflow_kill_gate: tokio::sync::Notify::new(),
            charge_release_parked: AtomicBool::new(false),
            charge_release_open: AtomicBool::new(true),
            charge_release_gate: tokio::sync::Notify::new(),
            closed_frame_refusals: AtomicUsize::new(0),
            overflow_frame_refusals: AtomicUsize::new(0),
            reader_finished_after_refusal: AtomicBool::new(false),
            eof_suppressed_after_overflow: AtomicBool::new(false),
            // r15 item 6: post-spawn overflow-refusal gate.
            overflow_refusal_parked_count: AtomicUsize::new(0),
            overflow_refusal_gate_open: AtomicBool::new(true),
            overflow_refusal_gate: tokio::sync::Notify::new(),
            // r17 item 1: EOF-decision and overflow-spawn gates are open by
            // default; the standdown counter starts clean per arm.
            eof_seal_decision_parked: AtomicBool::new(false),
            eof_seal_decision_open: AtomicBool::new(true),
            eof_seal_decision_gate: tokio::sync::Notify::new(),
            overflow_spawn_parked_count: AtomicUsize::new(0),
            overflow_spawn_gate_open: AtomicBool::new(true),
            overflow_spawn_gate: tokio::sync::Notify::new(),
            overflow_spawn_standdowns: AtomicUsize::new(0),
            // r18 items 1/2; r19 item 1: registration and EOF acquisition
            // counters start clean per arm.
            overflow_shutdown_registered: AtomicUsize::new(0),
            eof_seal_lock_wait_count: AtomicUsize::new(0),
            eof_seal_lock_contended_count: AtomicUsize::new(0),
            eof_seal_entered_count: AtomicUsize::new(0),
        });
        *slot.lock().expect("barrier lock") = Some(state.clone());
        WriteCommitBarrier { state }
    }

    fn current(slot: &Slot) -> Option<Arc<State>> {
        slot.lock().expect("barrier lock").clone()
    }

    /// ma-sdk-state r13 item 5: notification-driven predicate wait shared by
    /// every acknowledgement helper. The predicate is checked first; the waiter
    /// then registers on `state_changed` and re-checks it (a hook always stores
    /// before signalling, so no wakeup is lost), awaiting at most until
    /// `deadline`. On timeout it panics with `timeout_message()`: each helper's
    /// NAMED bound stays the terminator (the r12 bound proofs prove those
    /// bounds), with no polling sleeps.
    async fn wait_until_with(
        state: &State,
        deadline: tokio::time::Instant,
        pred: impl Fn(&State) -> bool,
        timeout_message: impl Fn() -> String,
    ) {
        while !pred(state) {
            let notified = state.state_changed.notified();
            if pred(state) {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                panic!("{}", timeout_message());
            }
        }
    }

    /// r7 item 2 hook: park the DETACHED write task AFTER the Reserve ticket is
    /// enqueued but BEFORE it awaits the writer ack — the precise point where
    /// the caller can be aborted while the prompt is still in flight.
    pub(super) async fn hold_before_user_write_accept(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        let n = state.write_number.fetch_add(1, Ordering::AcqRel) + 1;
        if n < state.park_from_write.load(Ordering::Acquire) {
            // Earlier writes run straight through.
            return;
        }
        state.write_parked.store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.write_gate_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.write_gate.notified();
        if state.write_gate_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// Send-task hook: after the write ack, block until the test releases the
    /// send gate.
    pub(super) async fn hold_write_commit(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state.write_reached.store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.send_released.load(Ordering::Acquire) {
            return;
        }
        let notified = state.send_release.notified();
        if state.send_released.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// Send-task hook: a Reserve ticket was just enqueued (the turn book was
    /// opened immediately before this).
    pub(super) fn note_reserve_enqueued(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.reserve_enqueued.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// Worker hook: reached the Reserve ticket and is about to await the
    /// write's commit/cancel.
    pub(super) fn note_reserve_parked(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.reserve_parked.store(true, Ordering::Release);
            state.state_changed.notify_waiters();
        }
    }

    /// Worker hook: AFTER emitting the parked turn's `turn_started`, optionally
    /// hold the worker (still at that ticket's position) so a test can register
    /// the next turn before this turn's buffered result is mapped. Returns
    /// immediately when no barrier is armed or the worker gate is open.
    pub(super) async fn hold_worker_after_start(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        if state.worker_released.load(Ordering::Acquire) {
            return;
        }
        let notified = state.worker_resume.notified();
        if state.worker_released.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// Reader hook: a frame was enqueued behind the (possibly parked) ticket.
    /// While a barrier is armed the harness child only emits its turn result in
    /// this window, so this marks that result as queued.
    pub(super) fn note_frame_enqueued(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.result_queued.store(true, Ordering::Release);
            state.result_enqueued_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r12 item 2 reader hook: while the result-enqueue gate is shut, park the
    /// READER before admitting a turn `result` frame to the publication queue.
    /// This withholds the later turn's result deterministically (no queue-order
    /// race) until the test releases it. Non-result frames and periods with no
    /// armed/shut gate pass straight through.
    pub(super) async fn hold_result_enqueue(slot: &Slot, frame: &remuda_claude_wire::Outbound) {
        if !matches!(frame, remuda_claude_wire::Outbound::Result(_)) {
            return;
        }
        let Some(state) = current(slot) else {
            return;
        };
        state.result_enqueue_parked.store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.result_enqueue_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.result_enqueue_gate.notified();
        if state.result_enqueue_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// r12 item 2 worker hook: a cancelled ticket's at-position rollback has
    /// COMPLETED (the book was rolled back and any released correction decided
    /// and emitted or suppressed at the ticket position).
    pub(super) fn note_cancelled_rollback_done(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.cancelled_rollback_done.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// Reader hook (r11 item 2): natural stdout EOF was sealed and the
    /// StdoutEof job queued from the same publish-mutex critical section.
    pub(super) fn note_stdout_eof_queued(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.stdout_eof_queued.store(true, Ordering::Release);
            state.state_changed.notify_waiters();
        }
    }

    /// r12 item 1 hook: the publication backlog overflow shutdown fired.
    pub(super) fn note_backlog_overflow(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.backlog_overflowed.store(true, Ordering::Release);
            state.state_changed.notify_waiters();
        }
    }

    /// r15 item 4 hook: the reader refused a stdout FRAME with `Closed`
    /// (admission already tripped elsewhere or the receiver was dropped).
    pub(super) fn note_closed_frame_refused(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.closed_frame_refusals.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r15 item 4 hook: the reader refused a stdout FRAME with `Overflow` at
    /// its own admission site.
    pub(super) fn note_overflow_frame_refused(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.overflow_frame_refusals.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r15 item 4 hook: the reader FINISHED after an admission refusal without
    /// enqueuing StdoutEof (fires after the seal section was skipped).
    pub(super) fn note_reader_finished_after_refusal(slot: &Slot) {
        if let Some(state) = current(slot) {
            state
                .reader_finished_after_refusal
                .store(true, Ordering::Release);
            state.state_changed.notify_waiters();
        }
    }

    /// r16 item 1 hook: a GENUINE reader EOF (recv() returned None) reached the
    /// admission critical section after an overflow had won terminal
    /// ownership, so the ordinary StdoutEof was suppressed and the overflow
    /// task owns the single terminal.
    pub(super) fn note_eof_suppressed_after_overflow(slot: &Slot) {
        if let Some(state) = current(slot) {
            state
                .eof_suppressed_after_overflow
                .store(true, Ordering::Release);
            state.state_changed.notify_waiters();
        }
    }

    /// r13 item 3 hook: the overflow shutdown sealed admission and is about to
    /// await the `live` handle for its guaranteed kill — while a test holds
    /// that handle, this acknowledges the kill step parked on the contention.
    pub(super) fn note_overflow_kill_waiting(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.overflow_kill_waiting.store(true, Ordering::Release);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r14 item 1 hook: an ordinary emission found the observation
    /// channel saturated and is about to park for consumer capacity.
    pub(super) fn note_emit_waiting(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.emit_waiting_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r15 item 6 gate: park the TRIGGER SEND right after it
    /// spawned the overflow shutdown and before the refusal error returns, so
    /// the send task is provably alive while the shutdown is in progress and
    /// aborting it genuinely lands mid-shutdown. Open by default.
    pub(super) async fn hold_after_overflow_spawn(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state
            .overflow_refusal_parked_count
            .fetch_add(1, Ordering::AcqRel);
        state.state_changed.notify_waiters();
        if state.overflow_refusal_gate_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.overflow_refusal_gate.notified();
        if state.overflow_refusal_gate_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// ma-sdk-state r17 item 1 gate; relocated by ma-sdk-state r18 item 1:
    /// park the triggering overflow send in the reservation admission section
    /// AFTER the synchronous latch swap/closed check and the tracked-task
    /// registration, while it is STILL HOLDING the publish gate — the exact
    /// point the pre-fix code dropped the gate and awaited an asynchronous
    /// re-entry. Aborting the send parked here must leave the latch set and
    /// the registered task delivering the overflow terminal. Open by default.
    pub(super) async fn hold_overflow_spawn_admission(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state
            .overflow_spawn_parked_count
            .fetch_add(1, Ordering::AcqRel);
        state.state_changed.notify_waiters();
        if state.overflow_spawn_gate_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.overflow_spawn_gate.notified();
        if state.overflow_spawn_gate_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// ma-sdk-state r17 item 1 hook: an overflow decision ran the latch/closed
    /// check and found the launch already sealed (the natural EOF won), so it
    /// stood down without registering a shutdown.
    pub(super) fn note_overflow_spawn_stood_down(slot: &Slot) {
        if let Some(state) = current(slot) {
            state
                .overflow_spawn_standdowns
                .fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r18 item 1 hook: the overflow shutdown task handle was
    /// stored in the launch's tracked slot SYNCHRONOUSLY at the decision site
    /// (latch swapped, `closed` clear, registration complete).
    pub(super) fn note_overflow_shutdown_registered(slot: &Slot) {
        if let Some(state) = current(slot) {
            state
                .overflow_shutdown_registered
                .fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r18 item 2 hook: the natural-EOF reader is about to await
    /// the publish gate for its seal (fired immediately BEFORE the lock).
    pub(super) fn note_eof_seal_lock_wait(slot: &Slot) {
        if let Some(state) = current(slot) {
            state
                .eof_seal_lock_wait_count
                .fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r19 item 1 hook: the natural-EOF reader's acquisition-
    /// point `try_lock` found the publish gate ALREADY HELD (`WouldBlock`).
    /// The positive counterpart of [`note_eof_seal_lock_wait`]: paired with
    /// it, a regression proves the reader is blocked ON the held guard at the
    /// exact acquisition point rather than merely not yet scheduled.
    pub(super) fn note_eof_seal_lock_contended(slot: &Slot) {
        if let Some(state) = current(slot) {
            state
                .eof_seal_lock_contended_count
                .fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r18 item 2 hook: the natural-EOF reader ENTERED the
    /// publish gate (lock acquired; the latch load/seal decision has not run
    /// yet). With this count still 0 after the lock-wait ack, a bounded check
    /// proves the reader is blocked ON the held guard.
    pub(super) fn note_eof_seal_entered(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.eof_seal_entered_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// ma-sdk-state r17 item 1 gate: park the natural-EOF reader in the seal
    /// section AFTER the overflow-latch load but BEFORE the closed store /
    /// StdoutEof decision, while still holding the publish gate. Open by
    /// default.
    pub(super) async fn hold_eof_seal_decision(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state
            .eof_seal_decision_parked
            .store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.eof_seal_decision_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.eof_seal_decision_gate.notified();
        if state.eof_seal_decision_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// ma-sdk-state r14 item 2 gate: park the detached overflow shutdown
    /// BETWEEN its admission seal and its live-handle kill. Open by default.
    pub(super) async fn hold_before_overflow_kill(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state.overflow_kill_parked.store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.overflow_kill_gate_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.overflow_kill_gate.notified();
        if state.overflow_kill_gate_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// ma-sdk-state r14 item 1 worker gate: the job is fully handled but its
    /// owned backlog permits are still held. Open by default; a regression
    /// shuts it BEFORE close so the publisher is parked mid-release while the
    /// driver relaunches (the charge must then release onto the dead launch's
    /// budget alone).
    pub(super) async fn hold_before_charge_release(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state.charge_release_parked.store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.charge_release_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.charge_release_gate.notified();
        if state.charge_release_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// r11 item 3 hook: fail_without_ticket reached its rollback and is about
    /// to await the mapper lock.
    pub(super) fn note_rollback_without_ticket_wait(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.rollback_wait_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r11 item 3 hook: a rollback_without_ticket call finished rolling the
    /// book back (fired after the mapper-lock critical section).
    pub(super) fn note_rollback_without_ticket_done(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.rollback_done_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r11 item 3 gate: park fail_without_ticket immediately BEFORE its
    /// rollback while the test gate is shut (open by default).
    pub(super) async fn hold_before_rollback_without_ticket(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state.rollback_parked.store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.rollback_gate_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.rollback_gate.notified();
        if state.rollback_gate_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// r8 item 3 worker hook: a Result frame has been mapped by the worker.
    pub(super) fn note_result_mapped(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.result_mapped_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// Wait until `n` Result frames have been mapped by the worker (r8 item 3),
    /// usable without owning a [`WriteCommitBarrier`] (r10 item 3: a spawned
    /// lock-holding task).
    pub(super) async fn wait_results_mapped(slot: &Slot, n: usize, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        let Some(state) = current(slot) else {
            // Callers always arm a barrier first; without one the count never
            // moves — surface the same named failure instead of parking.
            panic!(
                "write_commit barrier: timed out waiting for {n} mapped results (no barrier armed)"
            );
        };
        wait_until_with(
            &state,
            deadline,
            |state| state.result_mapped_count.load(Ordering::Acquire) >= n,
            || {
                format!(
                    "write_commit barrier: timed out waiting for {n} mapped results (mapped={})",
                    state.result_mapped_count.load(Ordering::Acquire)
                )
            },
        )
        .await;
    }

    /// r10 item 1 send hook: a caller is about to await the `write_seq` lock.
    /// With the lock held by a previous caller, the count acknowledges this
    /// caller parked behind it.
    pub(super) fn note_write_seq_wait(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.write_seq_wait_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r10 item 1 send hook: a caller is about to await the publish-mutex lock
    /// (taken BEFORE the book entry is opened). With the mutex externally
    /// held, the count acknowledges this caller parked there with no book
    /// entry opened yet.
    pub(super) fn note_publish_lock_wait(slot: &Slot) {
        if let Some(state) = current(slot) {
            state.publish_lock_wait_count.fetch_add(1, Ordering::AcqRel);
            state.state_changed.notify_waiters();
        }
    }

    /// r10 item 2 worker hook: reached a cancelled ticket's position. Park
    /// before the at-position rollback while the cancel gate is shut, so a
    /// test can enqueue a later reservation behind the cancelled ticket and
    /// prove the correction is emitted ahead of it.
    pub(super) async fn hold_cancelled_ticket(slot: &Slot) {
        let Some(state) = current(slot) else {
            return;
        };
        state
            .cancelled_ticket_reached
            .store(true, Ordering::Release);
        state.state_changed.notify_waiters();
        if state.cancelled_gate_open.load(Ordering::Acquire) {
            return;
        }
        let notified = state.cancelled_gate.notified();
        if state.cancelled_gate_open.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    impl WriteCommitBarrier {
        /// r10 item 1: wait until `n` send callers have reached the `write_seq`
        /// lock await (the nth is parked behind the holder when the previous
        /// caller is suspended holding it).
        pub async fn wait_until_write_seq_waits(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.write_seq_wait_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} write_seq waiters \
                         (reached={})",
                        self.state.write_seq_wait_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r10 item 1: wait until `n` send callers have reached the
        /// publish-mutex lock await.
        pub async fn wait_until_publish_lock_waits(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.publish_lock_wait_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} publish-lock waiters \
                         (reached={})",
                        self.state.publish_lock_wait_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r10 item 2: wait until the worker reaches a cancelled ticket's
        /// position and parks before the at-position rollback.
        pub async fn wait_until_cancelled_ticket_reached(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.cancelled_ticket_reached.load(Ordering::Acquire),
                || "write_commit barrier: timed out waiting for the cancelled ticket".to_owned(),
            )
            .await;
        }

        /// r10 item 2: SHUT the cancelled-ticket rollback gate so the worker
        /// parks at the cancelled ticket before the rollback. Opt-in (the gate
        /// is open by default): lets a test enqueue a later reservation behind
        /// the cancelled ticket deterministically.
        pub fn hold_cancelled_rollback(&self) {
            self.state
                .cancelled_gate_open
                .store(false, Ordering::Release);
        }

        /// r10 item 2: release the worker from a cancelled ticket so the
        /// at-position rollback (and correction) runs.
        pub fn release_cancelled_ticket(&self) {
            if !self.state.cancelled_gate_open.swap(true, Ordering::AcqRel) {
                self.state.cancelled_gate.notify_waiters();
            }
        }

        /// r12 item 2: wait until `n` cancelled tickets have been fully rolled
        /// back AT THEIR TICKET POSITIONS (not merely reached by the worker).
        pub async fn wait_until_cancelled_rollbacks_done(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.cancelled_rollback_done.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} cancelled-ticket \
                         rollbacks to complete (done={})",
                        self.state.cancelled_rollback_done.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r12 item 2: SHUT the reader result-enqueue gate so the next turn
        /// `result` frame is withheld from the publication queue until
        /// [`Self::release_result_enqueue`]. Resets the parked marker so a
        /// result that reached the gate while it was still open cannot satisfy
        /// a later "withheld" acknowledgement.
        pub fn hold_result_enqueue(&self) {
            self.state
                .result_enqueue_parked
                .store(false, Ordering::Release);
            self.state
                .result_enqueue_open
                .store(false, Ordering::Release);
        }

        /// r12 item 2: release a withheld turn result into the publication
        /// queue.
        pub fn release_result_enqueue(&self) {
            if !self.state.result_enqueue_open.swap(true, Ordering::AcqRel) {
                self.state.result_enqueue_gate.notify_waiters();
            }
        }

        /// r12 item 2: wait until the reader reached a result frame and is
        /// parked on the shut enqueue gate — the result is provably withheld.
        pub async fn wait_until_result_enqueue_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.result_enqueue_parked.load(Ordering::Acquire)
                        && !state.result_enqueue_open.load(Ordering::Acquire)
                },
                || "write_commit barrier: timed out waiting for the reader to park on                     the shut result-enqueue gate".to_owned(),
            )
            .await;
        }

        /// r11 item 2: wait until the reader observed natural stdout EOF and
        /// queued StdoutEof under the admission seal.
        pub async fn wait_until_stdout_eof_queued(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.stdout_eof_queued.load(Ordering::Acquire),
                || "write_commit barrier: timed out waiting for stdout EOF to be queued".to_owned(),
            )
            .await;
        }

        /// r12 item 1: wait until the bounded publication backlog tripped and
        /// the overflow session-shutdown fired (terminal diagnostic emitted).
        pub async fn wait_until_backlog_overflow(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.backlog_overflowed.load(Ordering::Acquire),
                || "write_commit barrier: timed out waiting for the publication                     backlog overflow".to_owned(),
            )
            .await;
        }

        /// r15 item 4: wait until the reader refuses `n` stdout frames with
        /// `Closed` — a frame arrived AFTER admission tripped (or after close)
        /// — WITHOUT enqueuing StdoutEof. The acknowledged point the false-EOF
        /// regression parks at.
        pub async fn wait_until_closed_frame_refusals(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| state.closed_frame_refusals.load(Ordering::Acquire) >= n,
                || {
                    "write_commit barrier: timed out waiting for a closed-admission frame refusal"
                        .to_owned()
                },
            )
            .await;
        }

        /// r15 item 4: wait until the reader stops after a frame admission
        /// refusal WITHOUT enqueuing a StdoutEof.
        pub async fn wait_until_reader_finished_after_refusal(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.reader_finished_after_refusal.load(Ordering::Acquire),
                || {
                    "write_commit barrier: timed out waiting for the reader to finish after the \
                     frame admission refusal"
                        .to_owned()
                },
            )
            .await;
        }

        /// r16 item 1: wait until a GENUINE stdout EOF (recv() returned None)
        /// was suppressed under the admission critical section because an
        /// overflow had already won terminal ownership. No ordinary StdoutEof
        /// is queued at this point.
        pub async fn wait_until_eof_suppressed_after_overflow(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.eof_suppressed_after_overflow.load(Ordering::Acquire),
                || {
                    "write_commit barrier: timed out waiting for the genuine EOF to be \
                     suppressed in favour of the overflow terminal"
                        .to_owned()
                },
            )
            .await;
        }

        /// r13 item 3: wait until the overflow shutdown sealed admission and is
        /// parked about to acquire the (test-held) `live` handle for its
        /// guaranteed kill.
        pub async fn wait_until_overflow_kill_waiting(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.overflow_kill_waiting.load(Ordering::Acquire),
                || "write_commit barrier: timed out waiting for the overflow kill to                     contend on the live handle".to_owned(),
            )
            .await;
        }

        /// ma-sdk-state r15 item 6: SHUT the gate at which the triggering send
        /// parks after spawning the overflow shutdown, so the abort lands on a
        /// genuinely in-flight send while the shutdown is running.
        pub fn hold_overflow_refusal(&self) {
            self.state
                .overflow_refusal_parked_count
                .store(0, Ordering::Release);
            self.state
                .overflow_refusal_gate_open
                .store(false, Ordering::Release);
        }

        /// Release a send parked on the post-spawn refusal gate.
        pub fn release_overflow_refusal(&self) {
            if !self
                .state
                .overflow_refusal_gate_open
                .swap(true, Ordering::AcqRel)
            {
                self.state.overflow_refusal_gate.notify_waiters();
            }
        }

        /// Wait until `n` triggering sends are parked after spawning the
        /// overflow shutdown (the shutdown itself is acknowledged in progress
        /// separately via the pre-kill gate).
        pub async fn wait_until_overflow_refusal_parked(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| {
                    state.overflow_refusal_parked_count.load(Ordering::Acquire) >= n
                        && !state.overflow_refusal_gate_open.load(Ordering::Acquire)
                },
                || {
                    "write_commit barrier: timed out waiting for the triggering send to park \
                     after spawning the overflow shutdown"
                        .to_owned()
                },
            )
            .await;
        }

        /// ma-sdk-state r17 item 1: SHUT the overflow-spawn admission gate so
        /// the next overflowing admission (a reservation breach in the r17
        /// regression) parks with its latch swap/closed check NOT yet run.
        pub fn hold_overflow_spawn(&self) {
            self.state
                .overflow_spawn_parked_count
                .store(0, Ordering::Release);
            self.state
                .overflow_spawn_gate_open
                .store(false, Ordering::Release);
        }

        /// Release an overflow spawn parked before its admission section.
        pub fn release_overflow_spawn(&self) {
            if !self
                .state
                .overflow_spawn_gate_open
                .swap(true, Ordering::AcqRel)
            {
                self.state.overflow_spawn_gate.notify_waiters();
            }
        }

        /// Wait until `n` triggering overflow sends are parked at the shut
        /// admission gate, AFTER the synchronous decision and the tracked-task
        /// registration, while still holding the publish gate.
        pub async fn wait_until_overflow_spawn_parked(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| {
                    state.overflow_spawn_parked_count.load(Ordering::Acquire) >= n
                        && !state.overflow_spawn_gate_open.load(Ordering::Acquire)
                },
                || {
                    "write_commit barrier: timed out waiting for the overflow send to park \
                     after registering the tracked shutdown"
                        .to_owned()
                },
            )
            .await;
        }

        /// ma-sdk-state r17 item 1: wait until `n` overflow decisions ran the
        /// latch/closed check, found the launch already sealed by the natural
        /// EOF, and stood down WITHOUT registering a shutdown.
        pub async fn wait_until_overflow_spawn_standdowns(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| state.overflow_spawn_standdowns.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} overflow spawns to \
                         stand down for the sealed EOF (stood down={})",
                        self.state.overflow_spawn_standdowns.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// ma-sdk-state r18 item 1: wait until `n` overflow shutdown tasks are
        /// registered synchronously at the decision site (the handle stored
        /// in the launch slot before the decision section returns).
        pub async fn wait_until_overflow_registered(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| state.overflow_shutdown_registered.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} overflow shutdown \
                         registrations (registered={})",
                        self.state
                            .overflow_shutdown_registered
                            .load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// ma-sdk-state r18 item 2: wait until `n` natural-EOF readers reached
        /// the publish-gate acquisition (the ack fired right before the lock).
        pub async fn wait_until_eof_seal_lock_waits(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| state.eof_seal_lock_wait_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} EOF readers to reach \
                         the publish-gate acquisition (lock waits={})",
                        self.state.eof_seal_lock_wait_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// ma-sdk-state r19 item 1: wait until `n` natural-EOF readers report
        /// the publish gate HELD at their acquisition-point probe (a
        /// `try_lock` that answered `WouldBlock`). This POSITIVE contention
        /// handshake replaces the absence window: it completes only when a
        /// reader actually reached the acquisition and found the guard held.
        pub async fn wait_until_eof_seal_lock_contended(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| state.eof_seal_lock_contended_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} EOF readers to find the \
                         publish gate held at the acquisition point (contended={}, lock waits={})",
                        self.state
                            .eof_seal_lock_contended_count
                            .load(Ordering::Acquire),
                        self.state.eof_seal_lock_wait_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// ma-sdk-state r18 item 2: wait until `n` natural-EOF readers
        /// ENTERED the publish gate (lock acquired, latch not yet loaded).
        pub async fn wait_until_eof_seal_entered(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                move |state| state.eof_seal_entered_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} EOF readers to enter \
                         the publish gate (entered={})",
                        self.state.eof_seal_entered_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// ma-sdk-state r17 item 1: SHUT the natural-EOF seal-decision gate so
        /// the reader parks after its latch load and before the closed store /
        /// StdoutEof queue, holding the publish gate. Resets the parked marker
        /// so an earlier EOF cannot satisfy the acknowledgement.
        pub fn hold_eof_seal_decision(&self) {
            self.state
                .eof_seal_decision_parked
                .store(false, Ordering::Release);
            self.state
                .eof_seal_decision_open
                .store(false, Ordering::Release);
        }

        /// Release the natural-EOF reader into its seal decision.
        pub fn release_eof_seal_decision(&self) {
            if !self
                .state
                .eof_seal_decision_open
                .swap(true, Ordering::AcqRel)
            {
                self.state.eof_seal_decision_gate.notify_waiters();
            }
        }

        /// Wait until the natural-EOF reader is parked at the shut seal
        /// decision, holding the publish gate with its latch loaded.
        pub async fn wait_until_eof_seal_decision_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.eof_seal_decision_parked.load(Ordering::Acquire)
                        && !state.eof_seal_decision_open.load(Ordering::Acquire)
                },
                || {
                    "write_commit barrier: timed out waiting for the EOF reader to park at the \
                     seal decision"
                        .to_owned()
                },
            )
            .await;
        }

        /// ma-sdk-state r14 item 2: SHUT the gate between the overflow
        /// admission seal and the live-handle kill, so the detached shutdown
        /// parks there while a test aborts the triggering send.
        pub fn hold_overflow_kill(&self) {
            self.state
                .overflow_kill_parked
                .store(false, Ordering::Release);
            self.state
                .overflow_kill_gate_open
                .store(false, Ordering::Release);
        }

        /// Release the detached overflow shutdown into its kill step.
        pub fn release_overflow_kill(&self) {
            if !self
                .state
                .overflow_kill_gate_open
                .swap(true, Ordering::AcqRel)
            {
                self.state.overflow_kill_gate.notify_waiters();
            }
        }

        /// Wait until the detached overflow shutdown reached the shut
        /// pre-kill gate (seal done, kill not yet attempted).
        pub async fn wait_until_overflow_kill_gate_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.overflow_kill_parked.load(Ordering::Acquire)
                        && !state.overflow_kill_gate_open.load(Ordering::Acquire)
                },
                || {
                    "write_commit barrier: timed out waiting for the overflow shutdown to park \
                     before its kill"
                        .to_owned()
                },
            )
            .await;
        }

        /// ma-sdk-state r14 item 1: wait until `n` ordinary emissions found the
        /// observation channel saturated and parked for consumer capacity.
        pub async fn wait_until_emit_waiting(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.emit_waiting_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} saturated emissions \
                         (waiting={})",
                        self.state.emit_waiting_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// ma-sdk-state r14 item 1: SHUT the worker's pre-charge-release gate so
        /// the publisher parks after fully handling a charged job while still
        /// holding its permits. Used to park a publisher across close/relaunch.
        pub fn hold_charge_release(&self) {
            self.state
                .charge_release_parked
                .store(false, Ordering::Release);
            self.state
                .charge_release_open
                .store(false, Ordering::Release);
        }

        /// Release the pre-charge-release gate.
        pub fn release_charge_release(&self) {
            if !self.state.charge_release_open.swap(true, Ordering::AcqRel) {
                self.state.charge_release_gate.notify_waiters();
            }
        }

        /// Wait until the worker has handled a charged job and is parked at the
        /// shut pre-release gate with its permits still held.
        pub async fn wait_until_charge_release_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.charge_release_parked.load(Ordering::Acquire)
                        && !state.charge_release_open.load(Ordering::Acquire)
                },
                || {
                    "write_commit barrier: timed out waiting for the publisher to park before \
                     releasing its backlog charge"
                        .to_owned()
                },
            )
            .await;
        }

        /// r11 item 4(a): wait until the READER has enqueued `n` Result frames
        /// (behind the parked/held tickets), so a natural EOF provoked after
        /// this is provably queued behind those results, not ahead of them.
        pub async fn wait_until_results_enqueued(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.result_enqueued_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} results to be enqueued \
                         (enqueued={})",
                        self.state.result_enqueued_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r11 item 3: SHUT the pre-rollback gate so fail_without_ticket parks
        /// after the forced send failure but BEFORE its mapper-lock rollback.
        pub fn hold_rollback_without_ticket(&self) {
            self.state
                .rollback_gate_open
                .store(false, Ordering::Release);
        }

        /// r11 item 3: release a caller parked before its rollback.
        pub fn release_rollback_without_ticket(&self) {
            if !self.state.rollback_gate_open.swap(true, Ordering::AcqRel) {
                self.state.rollback_gate.notify_waiters();
            }
        }

        /// r11 item 3: wait until a fail_without_ticket caller is parked at the
        /// pre-rollback gate.
        pub async fn wait_until_rollback_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.rollback_parked.load(Ordering::Acquire),
                || "write_commit barrier: timed out waiting for the rollback park".to_owned(),
            )
            .await;
        }

        /// r11 item 3: wait until `n` callers reached the rollback's
        /// mapper-lock await (the nth is parked on it while the test holds the
        /// mapper).
        pub async fn wait_until_rollback_lock_waits(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.rollback_wait_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} rollback lock waits \
                         (reached={})",
                        self.state.rollback_wait_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r11 item 3: wait until `n` no-ticket rollbacks have COMPLETED,
        /// including one the cancellation-independent Drop cleanup detached.
        pub async fn wait_until_rollbacks_done(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.rollback_done_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} rollbacks to complete \
                         (done={})",
                        self.state.rollback_done_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r8 item 3: wait until `n` Result frames have been MAPPED by the
        /// publication worker (after the mapper lock, before/with emit).
        pub async fn wait_until_results_mapped(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.result_mapped_count.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} mapped results \
                         (mapped={})",
                        self.state.result_mapped_count.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// Block until the send has parked post-write, the worker is parked at
        /// the turn's Reserve ticket, AND the reader's result frame is queued
        /// behind it — i.e. the race window is provably occupied.
        pub async fn wait_until_result_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.write_reached.load(Ordering::Acquire)
                        && state.reserve_parked.load(Ordering::Acquire)
                        && state.result_queued.load(Ordering::Acquire)
                },
                || {
                    format!(
                        "write_commit barrier: timed out waiting for the race \
                         (write_reached={}, reserve_parked={}, result_queued={})",
                        self.state.write_reached.load(Ordering::Acquire),
                        self.state.reserve_parked.load(Ordering::Acquire),
                        self.state.result_queued.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r7 item 2: park the pre-ack write gate starting at the 1-based write
        /// number `n` (1 = the very next prompt, 2 = the second prompt, …).
        pub fn park_from_write(&self, n: usize) {
            self.state.park_from_write.store(n, Ordering::Release);
        }
        /// Whether the parked (thresholded) write has been reached yet.
        pub async fn wait_until_write_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.write_parked.load(Ordering::Acquire)
                        && state.reserve_enqueued.load(Ordering::Acquire) >= 1
                },
                || {
                    format!(
                        "write_commit barrier: timed out waiting for the pre-ack park \
                         (write_parked={}, enqueued={})",
                        self.state.write_parked.load(Ordering::Acquire),
                        self.state.reserve_enqueued.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// r7 item 2: let the parked detached write task proceed to the writer
        /// ack (and the write) after the caller was aborted.
        pub fn release_write_ack(&self) {
            if !self.state.write_gate_open.swap(true, Ordering::AcqRel) {
                self.state.write_gate.notify_waiters();
            }
        }

        /// Block until the send parked post-write AND the worker parked at the
        /// turn's Reserve ticket, WITHOUT requiring the child's result to be
        /// queued yet — the commit-first variant releases here and lets the
        /// worker map the result after the write committed.
        pub async fn wait_until_reserve_parked(&self, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| {
                    state.write_reached.load(Ordering::Acquire)
                        && state.reserve_parked.load(Ordering::Acquire)
                },
                || {
                    format!(
                        "write_commit barrier: timed out waiting for the ticket \
                         (write_reached={}, reserve_parked={})",
                        self.state.write_reached.load(Ordering::Acquire),
                        self.state.reserve_parked.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// Block until `n` Reserve tickets have been enqueued — i.e. the send
        /// task has opened that many turn books. Used to prove the second turn
        /// is registered before the parked worker maps the first result.
        pub async fn wait_until_reserves_enqueued(&self, n: usize, timeout: Duration) {
            let deadline = tokio::time::Instant::now() + timeout;
            wait_until_with(
                &self.state,
                deadline,
                |state| state.reserve_enqueued.load(Ordering::Acquire) >= n,
                || {
                    format!(
                        "write_commit barrier: timed out waiting for {n} reservations \
                         (enqueued={})",
                        self.state.reserve_enqueued.load(Ordering::Acquire)
                    )
                },
            )
            .await;
        }

        /// Release ONLY the send gate: the write commits and the parked worker
        /// emits turn_started, then stays at the ticket (worker gate still
        /// shut) so a further turn can register before this result is mapped.
        pub fn release_send(&self) {
            if !self.state.send_released.swap(true, Ordering::AcqRel) {
                self.state.send_release.notify_waiters();
            }
        }

        /// Release ONLY the worker gate: it resumes from the parked ticket and
        /// maps the buffered result.
        pub fn resume_worker(&self) {
            if !self.state.worker_released.swap(true, Ordering::AcqRel) {
                self.state.worker_resume.notify_waiters();
            }
        }

        /// Release both gates at once (send commits and the worker proceeds
        /// without an extra hold). Preserves the r4 single-gate behaviour.
        pub fn release(&self) {
            self.release_write_ack();
            self.release_send();
            self.resume_worker();
            self.release_cancelled_ticket();
            self.release_rollback_without_ticket();
            self.release_charge_release();
            self.release_overflow_kill();
            // r17 item 1: a failed assertion must not leave the EOF reader or
            // an overflow spawn parked on their admission gates.
            self.release_overflow_spawn();
            self.release_eof_seal_decision();
        }
    }

    impl Drop for WriteCommitBarrier {
        fn drop(&mut self) {
            // Release the gates so a failed assertion cannot hang the send or
            // the worker. The slot is per-driver and is overwritten by the next
            // arm (or dies with the Inner), so it needs no global clearing.
            self.release();
        }
    }
}

/// Single drainer of THIS launch's publication queue: map + emit strictly in
/// enqueue order (r15 item 1: the launch scope, not driver-wide state).
async fn publish_worker(
    inner: Arc<Inner>,
    launch: Arc<Launch>,
    mut rx: mpsc::UnboundedReceiver<PublishJob>,
) {
    while let Some(job) = rx.recv().await {
        // ma-sdk-state r14 item 1 directive (b): the job's owned permits are
        // its whole accounting. Hold them in `held_charge` until the job is
        // fully handled (mapped + emitted, or its ticket resolved) and the
        // pre-release test gate has passed; dropping them releases the charge
        // exactly once — also when this worker is aborted with jobs queued.
        let mut held_charge: Option<BacklogCharge> = None;
        let result = match job {
            PublishJob::Frame { frame, charge } => {
                held_charge = Some(charge);
                async {
                    // r12 item 1: when the backlog overflow shutdown already
                    // delivered the terminal observation directly (the worker
                    // was parked behind a blocked write or a full events
                    // channel), every job still queued is residue from a dead
                    // session. Drop it — including outbound control requests,
                    // which must not answer a child already killed — so nothing
                    // is ever published after the terminal lifecycle.
                    if launch.exit_emitted.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Outbound::ControlRequest(env) = &*frame
                        && let ControlRequest::CanUseTool(req) = &env.request
                    {
                        return handle_can_use_tool(&inner, &launch, env, req).await;
                    }
                    // No per-frame reservation wait: ordering is guaranteed by
                    // the Reserve ticket ahead of this frame in the queue. A
                    // turn's result is enqueued only after the child received
                    // the prompt, i.e. always behind that turn's ticket, so its
                    // turn_started is already published by the time we map here.
                    let observations = {
                        let mut mapper = inner.mapper.lock().await;
                        map_outbound(&mut mapper, &frame)?
                    };
                    // r8 item 3: signal once a RESULT frame has actually been
                    // mapped (after the mapper lock), so a deferred-settle test
                    // can wait for A's result to be consumed before releasing
                    // B's failure — ordering without a sleep.
                    #[cfg(feature = "test-stub")]
                    if matches!(&*frame, Outbound::Result(_)) {
                        test_barrier::note_result_mapped(&inner.barrier);
                    }
                    emit_all(&launch, observations).await
                }
                .await
            }
            PublishJob::Reserve {
                reservation,
                charge,
            } => {
                held_charge = Some(charge);
                async {
                    // test-stub only: the worker has reached the turn's ticket
                    // and is about to park on the not-yet-committed write.
                    #[cfg(feature = "test-stub")]
                    test_barrier::note_reserve_parked(&inner.barrier);
                    // Park the whole publication pipeline at this position until
                    // the write resolves. Frames already queued ahead have
                    // drained; every frame behind waits here, so the start is
                    // emitted before any of this turn's frames.
                    if reservation.wait_for_commit().await && reservation.claim_start() {
                        // r12 item 1: the overflow terminal may have been
                        // delivered while the worker parked at this ticket
                        // (the blocked write only errors once the child is
                        // killed). Keep the book coherent but emit nothing:
                        // no start may follow the terminal lifecycle.
                        let start = {
                            let mut mapper = inner.mapper.lock().await;
                            mapper.commit_turn_reservation(reservation.seq);
                            if launch.exit_emitted.load(Ordering::Acquire) {
                                None
                            } else {
                                Some(
                                    mapper
                                        .turn_started_observation(&reservation.client_message_id)?,
                                )
                            }
                        };
                        #[cfg(feature = "test-stub")]
                        let emitted = start.is_some();
                        if let Some(start) = start {
                            emit_all(&launch, vec![start]).await?;
                        }
                        // test-stub only: hold the worker AT the ticket after
                        // publishing the start, so a test can open the next
                        // turn's book before this turn's buffered result maps.
                        // A no-op unless the worker gate is deliberately shut.
                        // The overflow-suppressed path skips it: the launch is
                        // ending and no test gate still owns this position.
                        #[cfg(feature = "test-stub")]
                        if emitted {
                            test_barrier::hold_worker_after_start(&inner.barrier).await;
                        }
                        Ok(())
                    } else {
                        // The ticket was cancelled. Roll the book back HERE, at
                        // the ticket's FIFO position — before the next job
                        // (a later turn's start, a queued frame, StdoutEof) —
                        // and emit the released deferred settle THERE. Doing
                        // this on the worker (rather than in a detached Drop
                        // task that can run after a later turn's result is
                        // mapped, or by appending the correction to the queue
                        // tail where C's start/StdoutEof could overtake it)
                        // guarantees a cancelled reservation is gone before a
                        // later turn pops it as a phantom and its correction
                        // stays at its ticket's position
                        // (ma-sdk-state r9 item 2; r10 item 2).
                        //
                        // test-stub: a test may hold the worker HERE (ticket
                        // known-cancelled, rollback not yet run) so another
                        // reservation can provably queue behind it before the
                        // ordered rollback fires.
                        #[cfg(feature = "test-stub")]
                        test_barrier::hold_cancelled_ticket(&inner.barrier).await;
                        let rolled_back =
                            rollback_reservation_at_ticket(&inner, &launch, reservation.seq).await;
                        // r12 item 2: acknowledge the rollback COMPLETED at the
                        // ticket position before any later job is handled.
                        #[cfg(feature = "test-stub")]
                        test_barrier::note_cancelled_rollback_done(&inner.barrier);
                        rolled_back
                    }
                }
                .await
            }
            PublishJob::StdoutEof => emit_exit(&inner, &launch, "exited").await,
            #[cfg(feature = "test-stub")]
            PublishJob::Fence(signal) => {
                // Everything enqueued before this fence has now been published.
                let _ = signal.send(());
                Ok(())
            }
        };
        if let Err(error) = result {
            warn!("ordered publication failed: {error}");
        }
        // Test gate (open by default): the job is fully handled but its charge
        // is still HELD. A regression shuts this before close so the publisher
        // is provably mid-release across the relaunch; the charge that then
        // drops must come off the DEAD launch's budget alone.
        #[cfg(feature = "test-stub")]
        if held_charge.is_some() {
            test_barrier::hold_before_charge_release(&inner.barrier).await;
        }
        drop(held_charge);
    }
}

/// Whether a user frame carries a tool_result whose content (or mirrored
/// sidecar) still contains data-bearing image items.
fn frame_needs_prefold(frame: &Outbound) -> bool {
    let Outbound::User(msg) = frame else {
        return false;
    };
    let blocks = match &msg.message.content {
        UserContent::Blocks(blocks) => blocks.as_slice(),
        UserContent::Text(_) => return false,
    };
    if blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .any(|block| value_has_unfolded_image(block.get("content")))
    {
        return true;
    }
    value_has_unfolded_image(msg.tool_use_result.as_ref())
}

/// Depth-first presence check for an image item with inline data that was not
/// already folded (no `remudaMedia` flag).
fn value_has_unfolded_image(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        Value::Object(map) => {
            // Case-insensitive type match and the same byte-bearing spellings
            // the fold/scrub accept (`data`, `source.data`, `file.base64`),
            // so prefold detection is a superset of what would be staged
            // inline; an `Image`/`file.base64` item can't skip the blocking
            // prefold onto a tokio worker.
            let is_image = map
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.eq_ignore_ascii_case("image"));
            let already = map
                .get("remudaMedia")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let has_bytes = map.get("data").and_then(Value::as_str).is_some()
                || map
                    .get("source")
                    .and_then(Value::as_object)
                    .and_then(|source| source.get("data"))
                    .and_then(Value::as_str)
                    .is_some()
                || map
                    .get("file")
                    .and_then(Value::as_object)
                    .and_then(|file| file.get("base64"))
                    .and_then(Value::as_str)
                    .is_some();
            if is_image && !already && has_bytes {
                return true;
            }
            map.values()
                .any(|child| value_has_unfolded_image(Some(child)))
        }
        Value::Array(items) => items
            .iter()
            .any(|child| value_has_unfolded_image(Some(child))),
        _ => false,
    }
}

/// Fold a frame's tool-result images off the async runtime, replacing native
/// content with folded markers and scrubbing the `toolUseResult` mirror.
async fn prefold_user_frame(inner: &Inner, frame: Outbound) -> DriverResult<Outbound> {
    if !frame_needs_prefold(&frame) {
        return Ok(frame);
    }
    let stager = {
        let mapper = inner.mapper.lock().await;
        mapper.media_stager.clone()
    };
    let Some(stager) = stager else {
        // No object route: the synchronous mapper produces the honest text
        // fallbacks; there is nothing to park on.
        return Ok(frame);
    };
    tokio::task::spawn_blocking(move || prefold_blocking(frame, stager.as_ref()))
        .await
        .map_err(|error| {
            DriverError::CarrierUnavailable(format!("tool-media staging task panicked: {error}"))
        })?
}

fn prefold_blocking(
    mut frame: Outbound,
    stager: &dyn remuda_protocol::ToolMediaStager,
) -> DriverResult<Outbound> {
    let Outbound::User(msg) = &mut frame else {
        return Ok(frame);
    };
    let mut outcomes = Vec::new();
    if let UserContent::Blocks(blocks) = &mut msg.message.content {
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(content) = block.get("content") else {
                continue;
            };
            let folded = remuda_protocol::fold_tool_result(Some(content), Some(stager));
            outcomes.extend(folded.images.iter().cloned());
            block["content"] = serde_json::json!({
                remuda_protocol::folded_marker(): folded.blocks,
            });
        }
    }
    if let Some(sidecar) = msg.tool_use_result.as_mut() {
        remuda_protocol::sanitize_tool_result_sidecar(sidecar, &outcomes);
    }
    Ok(frame)
}

async fn handle_can_use_tool(
    inner: &Inner,
    launch: &Launch,
    env: &ControlRequestEnvelope,
    req: &CanUseToolRequest,
) -> DriverResult<()> {
    let policy = *inner.policy.lock().await;
    let interaction_id = InteractionId::new();
    let request_id = env.request_id.clone();
    let observations = {
        let mut mapper = inner.mapper.lock().await;
        let interaction = build_interaction(&mut mapper, env, req, interaction_id.clone())?;
        vec![mapper.observation(
            Completeness::Structured,
            NativeRequestKey::Rpc {
                value_type: NativeRequestValueType::String,
                value: request_id,
            },
            ObservationPayload::InteractionRequested(Box::new(
                remuda_protocol::InteractionRequestedPayload { interaction },
            )),
        )?]
    };
    emit_all(launch, observations).await?;

    match policy {
        PermissionPolicy::AutoAllow => {
            let payload = ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input: req.input.clone(),
                updated_permissions: None,
            });
            send_control(inner, &env.request_id, payload).await?;
        }
        PermissionPolicy::AutoDeny => {
            let payload = ControlSuccessPayload::Permission(PermissionResult::Deny {
                message: "dontAsk policy denied this tool call".into(),
                interrupt: Some(false),
            });
            send_control(inner, &env.request_id, payload).await?;
        }
        PermissionPolicy::Host => {
            let mut live = inner.live.lock().await;
            if let Some(live) = live.as_mut() {
                live.pending.insert(
                    interaction_id.clone(),
                    PendingTool {
                        request_id: env.request_id.clone(),
                        input: req.input.clone(),
                    },
                );
                live.pending_by_native
                    .insert(env.request_id.clone(), interaction_id);
            }
        }
    }
    Ok(())
}

async fn send_control(
    inner: &Inner,
    request_id: &str,
    payload: ControlSuccessPayload,
) -> DriverResult<()> {
    // ma-sdk-state r4 item 5(c): permission responses ride the SAME
    // ProcessWriter FIFO queue as prompts, not the asynchronous inbound
    // bridge (pub_in_tx -> inbound_forward task -> writer_tx). The bridge's
    // extra enqueue hop let a prompt enqueued afterwards overtake an earlier
    // response; the direct writer queue gives strict prompt/control order.
    // Clone the writer under the lock, then await the enqueue lock-free so a
    // blocked pipe never holds `live` (close/cancel stay reachable).
    let writer = {
        let live = inner.live.lock().await;
        let Some(live) = live.as_ref() else {
            return Err(DriverError::ControlUnavailable);
        };
        live.process.writer()
    };
    writer
        .respond_control(request_id, payload)
        .await
        .map_err(map_wire)
}

async fn emit_all(launch: &Launch, observations: Vec<Observation>) -> DriverResult<()> {
    for observation in observations {
        // ma-sdk-state r14 item 1 directive (d): reserve channel capacity
        // OUTSIDE the emission mutex — this is the await that can park on a
        // full consumer and must never hold the seal lock — then take the
        // mutex and recheck the terminal seal before filling the reserved
        // slot. The terminal is sealed and sent under the SAME mutex through
        // its own reserved slot, so an ordinary observation can never land
        // after it, even with the worker and the overflow shutdown sending on
        // parallel OS threads.
        let tx = {
            let emission = launch.emission.lock().await;
            if emission.terminal_sent {
                return Ok(());
            }
            match emission.tx.clone() {
                Some(tx) => tx,
                None => return Ok(()),
            }
        };
        #[cfg(feature = "test-stub")]
        if tx.capacity() == 0 {
            // The NEXT await parks for consumer capacity; record the worker at
            // the saturation point so regressions acknowledge it instead of
            // polling channel capacity on a timer. The barrier slot rides the
            // launch (a shared Arc to the driver's arm slot).
            test_barrier::note_emit_waiting(&launch.barrier);
        }
        let Ok(permit) = tx.reserve_owned().await else {
            return Ok(());
        };
        let emission = launch.emission.lock().await;
        if emission.terminal_sent || emission.tx.is_none() {
            // The terminal won while the reservation parked: abandon the slot.
            drop(permit);
            drop(emission);
            return Ok(());
        }
        permit.send(observation);
    }
    Ok(())
}

/// Emit the session `exited` lifecycle, at most once per launch.
///
/// Two callers race for it on the launch scope: the reader task when stdout
/// hits EOF, and [`Driver::close`] after the ladder. Whichever arrives first
/// wins; the other is a no-op, so a consumer never sees the session exit
/// twice (§2.3).
async fn emit_exit(inner: &Inner, launch: &Launch, status: &str) -> DriverResult<()> {
    emit_terminal(inner, launch, |mapper| {
        // ma-sdk-state r6 item 4: test fault seam — fail the build AFTER the
        // permit was taken. Dropping the permit on this early return is exactly
        // what must free the terminal slot so the RunHandle reaches EOF.
        #[cfg(feature = "test-stub")]
        if inner
            .exit_build_fails
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(DriverError::CarrierUnavailable(
                "test fault: forced exit observation build failure".into(),
            ));
        }
        let session_id = mapper.session_id.clone();
        mapper.lifecycle(
            LifecycleTopic::Session,
            "session",
            Knowledge::Known { value: session_id },
            status,
            false,
        )
    })
    .await
}

/// r12 item 1: terminal lifecycle for the publication-backlog overflow
/// shutdown. Status "failed" (with `reason=publication_backlog_overflow` in
/// related ids) is the process-end classifier's exact Failed token for a
/// `session` lifecycle, so Node/Hub end the instance on it, and the reason is
/// machine-readable on the journaled event.
async fn emit_overflow_exit(
    inner: &Inner,
    launch: &Launch,
    budget: &BacklogBudget,
    items: usize,
    bytes: usize,
) -> DriverResult<()> {
    let (limit_items, limit_bytes) = budget.caps();
    emit_terminal(inner, launch, move |mapper| {
        mapper.session_backlog_overflow_observation(items, bytes, limit_items, limit_bytes)
    })
    .await
}

/// Build and deliver the single per-launch terminal observation (ma-sdk-state
/// r14 item 1, directive (d); r15 item 1 keeps every touched slot on the
/// passed [`Launch`]).
///
/// Ordering is a single emission mutex:
/// 1. the exactly-once guard (`exit_emitted`);
/// 2. the reserved terminal permit is taken BEFORE the fallible build, so a
///    build failure drops it and frees the slot (r6 item 4);
/// 3. under the emission mutex the terminal seal is set FIRST and the reserved
///    permit is filled SYNCHRONOUSLY while the mutex is held — the slot is
///    already reserved so this never waits for consumer capacity.
///
/// An ordinary sender that reserved a slot while parked on a full consumer
/// must take the same mutex and rechecks the seal before its `permit.send`, so
/// it drops that slot: nothing ordinary can follow the terminal, and every
/// observation already queued ahead of it is preserved. The rare fallback (no
/// reserved permit) seals first and then waits for one ordinary slot; the seal
/// still excludes every other ordinary sender.
async fn emit_terminal(
    inner: &Inner,
    launch: &Launch,
    build: impl FnOnce(&mut Mapper) -> DriverResult<Observation>,
) -> DriverResult<()> {
    if launch.exit_emitted.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let permit = launch.exit_permit.lock().await.take();
    // Build BEFORE sealing: a build error must leave the seal unset and drop
    // the just-taken permit, exactly as the r6 item 4 fault contract requires.
    let observation = {
        let mut mapper = inner.mapper.lock().await;
        build(&mut mapper)?
    };
    let mut emission = launch.emission.lock().await;
    if emission.terminal_sent {
        // Defensive: another terminal path won the mutex even though
        // exit_emitted should already have stopped it.
        return Ok(());
    }
    emission.terminal_sent = true;
    if let Some(permit) = permit {
        // Reserved slot: synchronous delivery, ordered behind everything
        // already queued, impossible to overtake or be overtaken by an
        // ordinary sender under the same mutex.
        permit.send(observation);
        return Ok(());
    }
    // No reserved permit: seal is already set, so every ordinary sender stops;
    // wait for one slot for the terminal itself and send it through the sealed
    // path.
    let Some(tx) = emission.tx.clone() else {
        return Ok(());
    };
    drop(emission);
    if let Ok(slot) = tx.reserve_owned().await {
        let emission = launch.emission.lock().await;
        if emission.tx.is_some() {
            slot.send(observation);
        }
        drop(emission);
    }
    Ok(())
}

fn map_outbound(mapper: &mut Mapper, frame: &Outbound) -> DriverResult<Vec<Observation>> {
    match frame {
        Outbound::KeepAlive => Ok(Vec::new()),
        Outbound::System(system) => map_system(mapper, system),
        Outbound::Assistant(msg) => stream::map_assistant(mapper, msg),
        Outbound::User(msg) => map_user(mapper, msg),
        Outbound::Result(result) => map_result(mapper, result),
        Outbound::StreamEvent(event) => stream::map_stream(mapper, event),
        Outbound::ControlRequest(_) | Outbound::ControlResponse(_) => Ok(Vec::new()),
        Outbound::ControlCancelRequest { .. } => Ok(Vec::new()),
        Outbound::RateLimitEvent(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "rate_limit_event")
        }
        Outbound::ToolProgress(_) => mapper.lifecycle_named(LifecycleTopic::Task, "tool_progress"),
        Outbound::PromptSuggestion(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "prompt_suggestion")
        }
        Outbound::Unknown(value) => mapper.opaque("unknown-type", OpaqueReason::UnknownType, value),
    }
}

fn map_system(mapper: &mut Mapper, system: &SystemMessage) -> DriverResult<Vec<Observation>> {
    match system {
        SystemMessage::Init(init) => map_init(mapper, init),
        SystemMessage::HookStarted(hook) => mapper.lifecycle_hook(
            "hook_started",
            hook.hook_id.as_deref().unwrap_or(""),
            hook.hook_event.as_deref().unwrap_or(""),
        ),
        SystemMessage::HookProgress(hook) => mapper.lifecycle_hook(
            "hook_progress",
            hook.hook_id.as_deref().unwrap_or(""),
            hook.hook_event.as_deref().unwrap_or(""),
        ),
        SystemMessage::HookResponse(hook) => mapper.lifecycle_hook(
            "hook_response",
            hook.hook_id.as_deref().unwrap_or(""),
            hook.hook_event.as_deref().unwrap_or(""),
        ),
        SystemMessage::TaskStarted(task) => map_task_started(mapper, task),
        SystemMessage::TaskProgress(task) => map_task_progress(mapper, task),
        SystemMessage::TaskUpdated(task) => map_task_updated(mapper, task),
        SystemMessage::TaskNotification(task) => map_task_notification(mapper, task),
        SystemMessage::BackgroundTasksChanged(_) => {
            mapper.lifecycle_named(LifecycleTopic::Task, "background_tasks_changed")
        }
        SystemMessage::PermissionDenied(_) => {
            mapper.lifecycle_named(LifecycleTopic::Permission, "permission_denied")
        }
        SystemMessage::ThinkingTokens(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "thinking_tokens")
        }
        SystemMessage::ApiRetry(_) => {
            mapper.lifecycle_named(LifecycleTopic::Diagnostic, "api_retry")
        }
        SystemMessage::SessionStateChanged(_) => {
            mapper.lifecycle_named(LifecycleTopic::Session, "session_state_changed")
        }
        SystemMessage::Unknown(value) => mapper.opaque("system", OpaqueReason::UnknownType, value),
        other => mapper.lifecycle_named(LifecycleTopic::Diagnostic, other.subtype_name()),
    }
}

fn map_init(mapper: &mut Mapper, init: &SystemInit) -> DriverResult<Vec<Observation>> {
    if let Some(session) = &init.session_id {
        mapper.session_id.clone_from(session);
    }
    let session_id = mapper.session_id.clone();
    let mut related = std::collections::BTreeMap::new();
    if let Some(tools) = &init.tools {
        related.insert("tools".into(), tools.join(","));
    }
    if let Some(caps) = &init.capabilities {
        related.insert("capabilities".into(), caps.join(","));
    }
    if let Some(model) = &init.model {
        related.insert("model".into(), model.clone());
    }
    Ok(vec![mapper.lifecycle_related(
        LifecycleTopic::Session,
        "session",
        Knowledge::Known { value: session_id },
        "started",
        related,
        false,
    )?])
}

/// One `user` message observation carrying `text`.
fn user_text_message(
    mapper: &mut Mapper,
    uuid: Option<&str>,
    text: String,
) -> DriverResult<Observation> {
    let key = uuid.map(ToOwned::to_owned).unwrap_or_else(|| "user".into());
    let (id, rev, op) = mapper.ids.message(&key)?;
    mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::Message(Box::new(MessagePayload {
            mutation: NodeMutation {
                node_id: id.clone(),
                revision: rev,
                operation: op,
                base_revision: None,
            },
            message_id: id,
            role: MessageRole::User,
            phase: MessagePhase::Input,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
            target_block: None,
            parent_tool_call_id: None,
            native_origin: known_or_unknown(uuid),
            // Overwritten by the transcript mapper, which has the evidence to
            // classify. stdout `user` frames are the prompt we just sent.
            origin: Some(remuda_protocol::MessageOrigin::Human),
            command_id: None,
            prompt_mode: None,
            status: ContentStatus::Complete,
        })),
    )
}

fn map_user(mapper: &mut Mapper, msg: &UserMessage) -> DriverResult<Vec<Observation>> {
    let mut out = Vec::new();
    match &msg.message.content {
        UserContent::Text(text) => {
            // An injected task-notification can arrive as a bare string too;
            // fold it before the human-shaped copy is rendered.
            out.extend(task_notification_results(mapper, text)?);
            out.push(user_text_message(
                mapper,
                msg.uuid.as_deref(),
                text.clone(),
            )?);
        }
        UserContent::Blocks(blocks) => {
            // A user array is usually tool results, but Claude also records
            // plain text there — interrupt notices most visibly. Dropping those
            // loses the reason a turn stopped.
            let mut texts = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            texts.push(text.to_owned());
                        }
                    }
                    Some("tool_result") => {
                        let tool_use_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        // The result occupies its own node (opened here), joined
                        // to the call via `tool_call_id` — same sequence as the
                        // live stream, so promoting a terminal from this
                        // transcript is indistinguishable from streaming it.
                        let call_id = mapper.ids.tool(tool_use_id)?;
                        let (result_id, revision, operation) =
                            mapper.ids.tool_result_open(tool_use_id)?;
                        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        // Image items (an MCP screenshot, say) stage their
                        // bytes through the Node's object endpoint and ride as
                        // `objectId` references; unstageable ones degrade to a
                        // text block naming media type and size. In production
                        // the frame is pre-folded on a blocking thread, so this
                        // call is a cheap marker deserialization there. D-045
                        // §6.2: never inline the content array (its base64)
                        // into the journal — the previous `to_string()` did
                        // exactly that.
                        let folded = remuda_protocol::fold_tool_result(
                            block.get("content"),
                            mapper.media_stager.as_deref(),
                        );
                        // Claude mirrors the content array into toolUseResult;
                        // scrub any mirrored image data (idempotent when the
                        // prefold pass already did it).
                        let mut sanitized_sidecar = msg.tool_use_result.clone();
                        if let Some(sidecar) = sanitized_sidecar.as_mut() {
                            remuda_protocol::sanitize_tool_result_sidecar(sidecar, &folded.images);
                        }
                        // A backgrounded (or build-deferred) subagent's launch
                        // tool_result is written *immediately* with
                        // `toolUseResult.isAsync`; the real completion arrives
                        // later as an injected `<task-notification>` user
                        // record. Mark the launch `Partial` so the task row
                        // stays running (and the web shows "running in
                        // background"); the notification replaces it for real.
                        let async_launch = remuda_protocol::tool_result_is_async_launch(
                            msg.tool_use_result.as_ref(),
                        );
                        out.push(mapper.observation(
                            Completeness::Structured,
                            NativeRequestKey::None,
                            ObservationPayload::ToolResult(Box::new(ToolResultPayload {
                                mutation: NodeMutation {
                                    node_id: result_id,
                                    revision,
                                    operation,
                                    base_revision: (revision.0 > 1).then_some(U64(revision.0 - 1)),
                                },
                                tool_call_id: call_id,
                                stage: if async_launch && !is_error {
                                    ResultStage::Partial
                                } else {
                                    ResultStage::Final
                                },
                                outcome: if is_error {
                                    ToolOutcome::Failed
                                } else {
                                    ToolOutcome::Succeeded
                                },
                                blocks: folded.blocks,
                                structured_result: match sanitized_sidecar {
                                    Some(value) => Knowledge::Known { value },
                                    None => Knowledge::NotApplicable,
                                },
                                exit_code: Knowledge::NotApplicable,
                                changes: Vec::new(),
                            })),
                        )?);
                    }
                    _ => {}
                }
            }
            // Task-notification text rides alongside tool results in the same
            // user record on some builds; fold every one before emitting the
            // joined human-readable text message.
            for text in &texts {
                out.extend(task_notification_results(mapper, text)?);
            }
            if !texts.is_empty() {
                out.push(user_text_message(
                    mapper,
                    msg.uuid.as_deref(),
                    texts.join("\n"),
                )?);
            }
        }
    }
    Ok(out)
}

/// Turn an injected `<task-notification>` text block into the final
/// ToolResult that closes the background subagent's task row.
///
/// Returns zero observations when the text is not a notification or carries no
/// `<tool-use-id>` (a background shell command with no tool row to fold onto).
/// The message itself is still rendered by the caller.
fn task_notification_results(mapper: &mut Mapper, text: &str) -> DriverResult<Vec<Observation>> {
    let Some(note) = remuda_protocol::TaskNotification::parse(text) else {
        return Ok(Vec::new());
    };
    let Some(native_tool_id) = note.tool_use_id.clone().filter(|id| !id.is_empty()) else {
        return Ok(Vec::new());
    };
    let call_id = mapper.ids.tool(&native_tool_id)?;
    let result_id = mapper.ids.tool_result_id(&native_tool_id)?;
    // Fixed revision 5 on the result node: launch Partial opens at 1, the hook
    // SubagentStop replaces at 4, and this authoritative notification (it
    // carries the real status, including killed/failed) replaces at 5 — so it
    // always wins.
    let revision = U64(5);
    let outcome = note.outcome();
    let status = note.status.clone();
    let task_id = note.task_id.clone();
    let summary = note.summary.clone();
    let body = note
        .result
        .clone()
        .or_else(|| note.summary.clone())
        .unwrap_or_default();
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::ToolResult(Box::new(ToolResultPayload {
            mutation: NodeMutation {
                node_id: result_id,
                revision,
                operation: MutationOperation::Replace,
                base_revision: Some(U64(4)),
            },
            tool_call_id: call_id,
            stage: ResultStage::Final,
            outcome,
            blocks: if body.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(Box::new(TextBlock { text: body }))]
            },
            structured_result: Knowledge::Known {
                value: serde_json::json!({
                    "taskId": task_id,
                    "status": status,
                    "summary": summary,
                }),
            },
            exit_code: Knowledge::NotApplicable,
            changes: Vec::new(),
        })),
    )?])
}

/// Max length of the turn-error text carried on a `turn/result` lifecycle
/// (D-057 OA6). The Hub stores the same bound on `lastTurnError.text`.
const TURN_ERROR_MAX_CHARS: usize = 200;

/// Truncate to at most `max` characters, never splitting a UTF-8 boundary.
fn truncate_char_boundary(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.char_indices()
        .nth(max)
        .map_or_else(|| text.to_owned(), |(index, _)| text[..index].to_owned())
}

/// Human-readable text for an error `result` frame (an API error such as 429),
/// or `None` when the frame carries nothing worth showing.
fn turn_error_text(result: &ResultMessage) -> Option<String> {
    if let Some(text) = result.result.as_ref().and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Some(text.trim().to_owned());
    }
    result
        .subtype
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

fn map_result(mapper: &mut Mapper, result: &ResultMessage) -> DriverResult<Vec<Observation>> {
    let mut related = std::collections::BTreeMap::new();
    if let Some(index) = result.result_index {
        related.insert("resultIndex".into(), index.to_string());
    }
    if let Some(turns) = result.num_turns {
        related.insert("numTurns".into(), turns.to_string());
    }
    // D-057 OA6: the Hub/Node fold needs the native queue depth to tell a
    // settled root-turn end from a Workflow's intermediate `result`
    // (`result_index` 0) or a turn with another prompt already queued.
    if let Some(queued) = result.queued_turn_count {
        related.insert("queuedTurnCount".into(), queued.to_string());
    }
    let is_turn_error = result.is_error == Some(true);
    let status = if is_turn_error { "error" } else { "turn_done" };
    // D-057 OA6: a turn-level error is retryable in place. Carry its text on
    // the turn lifecycle (severity stays info) so the projection can show an
    // additive turn-error marker without ever reading the turn failure as a
    // process/start failure.
    if is_turn_error && let Some(text) = turn_error_text(result) {
        related.insert(
            "lastError".into(),
            truncate_char_boundary(&text, TURN_ERROR_MAX_CHARS),
        );
    }
    // Workflow emits result_index 0 then 1; the first is not process completion
    // (stream-json §5: do not tear down on the first result). This stays the
    // one-shot PRINT process-completion heuristic; on the long-lived sdk child
    // it does not mean "root turn complete" (result_index grows per turn).
    let affects_completion =
        result.result_index.unwrap_or(0) > 0 && result.queued_turn_count.unwrap_or(0) == 0;

    // D-057 OA6 (ma-sdk-state r3): decide whether THIS result settles the ROOT
    // turn from evidence PER ROOT TURN, as a signal distinct from
    // `affects_completion`. The decision lives in [`TurnBook`]: a result
    // settles only when every workflow the OWNING turn opened has sent its
    // terminal task_notification (identity-tracked, not counted), nothing is
    // queued, and no newer locally-written turn is outstanding. In particular
    // `result_index` is irrelevant here: it is process-global and grows across
    // turns on the long-lived sdk child, so a workflow intermediate in a later
    // turn must not be read from it.
    let queued = result.queued_turn_count.unwrap_or(0);
    match mapper.turns.result_settles_root(queued) {
        RootSettle::Settled => {
            related.insert("settledRootTurn".into(), "true".into());
            // A real settle supersedes any stashed deferral.
            mapper.deferred_result = None;
        }
        RootSettle::Deferred => {
            // Remember this completed turn so a later rollback of the newer
            // outstanding turn can emit the settle it blocked (r6 item 5).
            mapper.deferred_result = Some(DeferredResult {
                status,
                last_error: related.get("lastError").cloned(),
            });
        }
        RootSettle::NotSettled => {}
    }
    let session_id = mapper.session_id.clone();
    let mut out = vec![mapper.lifecycle_related(
        LifecycleTopic::Turn,
        "result",
        Knowledge::Known { value: session_id },
        status,
        related,
        affects_completion,
    )?];
    for mut usage in usage_from_result(mapper, result)? {
        // c-ctxusage r6 item 2 (kept under ruling D): a LIVE print/sdk terminal
        // result read at process receipt carries the receipt time as its native
        // call time, so the Hub's TPM/lastTurnAt windows fold live print/sdk
        // turns (historical transcript usage carries the record timestamp
        // instead).
        usage.native_at = Knowledge::Known {
            value: usage.observed_at.clone(),
        };
        out.push(usage);
    }
    Ok(out)
}

/// Per-call usage from one stream-json `assistant` frame's `message.usage`.
///
/// c-usagefu (a): the authoritative per-call counters live on the assistant
/// message, not on the `result` frame (whose usage is summed over every call
/// in the turn). One Turn-scoped payload per top-level model message, keyed by
/// message id so the Hub durably dedupes the repeated content-block frames.
/// Sub-agent (nested) frames are skipped, matching transcript sidechains.
/// All-zero usage yields nothing.
fn usage_from_assistant_message(
    mapper: &mut Mapper,
    msg: &AssistantMessage,
) -> DriverResult<Option<Observation>> {
    let Some(message_id) = msg
        .message
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    if !mapper.stream.usage_messages.insert(message_id.to_owned()) {
        return Ok(None);
    };
    let Some(usage) = msg.message.get("usage") else {
        return Ok(None);
    };
    let model = msg.message.get("model").and_then(Value::as_str);
    turn_usage_observation(mapper, message_id, model, usage, 1)
}

/// Build one Turn-scoped per-call usage observation for an assistant message
/// from a native stream-json `usage` blob.
///
/// Revision 1 is the PROVISIONAL blob riding the assistant / `message_start`
/// frame: with `--include-partial-messages` its `output_tokens` is whatever
/// had been produced at that instant. Revision 2 is the FINAL blob riding
/// `message_delta`, which carries the message's full final usage (identical
/// input/cache buckets, true output count — c-usagefu r2 item 2). The Hub
/// revision-replaces the row when counters grow.
fn turn_usage_observation(
    mapper: &mut Mapper,
    message_id: &str,
    model: Option<&str>,
    usage: &Value,
    revision: u64,
) -> DriverResult<Option<Observation>> {
    // Remember the revision-1 blob so a sparse message_delta PATCH (output
    // tokens only, common behind Bedrock/Vertex/gateway profiles) overlays
    // onto it instead of zeroing the omitted input/cache buckets
    // (c-usagefu r3 item 2). First frame wins, matching the rev-1 dedupe.
    if revision == 1 && usage.is_object() && !mapper.stream.message_usage.contains_key(message_id) {
        mapper
            .stream
            .message_usage
            .insert(message_id.to_owned(), usage.clone());
    }
    let Some(event) =
        crate::usage::claude::usage_from_message_usage(Some(message_id), None, model, usage)
    else {
        return Ok(None);
    };
    // Skip an all-zero call (no fresh input, no output, no cache buckets).
    // Buckets are Option (U1): only STRUCTURAL zeros are Some(0); an omitted
    // bucket (None) is "not reported" and must NOT be treated as zero here.
    let counters = &event.tokens;
    if counters.uncached_input == Some(0)
        && counters.output == Some(0)
        && counters.cache_read == Some(0)
        && counters.cache_write() == Some(0)
    {
        return Ok(None);
    }
    let mut totals = crate::usage::UsageTotals::default();
    totals.add(&event);
    let mut payload = crate::usage::to_usage_payload(
        UsageScope::Turn,
        message_id.to_owned(),
        revision,
        None,
        &totals,
    );
    payload.usage_id = Id::derive(
        "obj",
        mapper.instance_id.as_id().as_str(),
        &format!("usage:turn:{message_id}"),
    )?;
    if let Some(model) = model
        && let Some(&window) = mapper.stream.model_windows.get(model)
    {
        payload.context_window = Some(U64(window));
    }
    Ok(Some(mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::Usage(Box::new(payload)),
    )?))
}

/// Session-level evidence from one stream-json `result` frame.
///
/// c-usagefu (a): token counters are NOT read here — `result.usage` is summed
/// over every call in the turn and double counts cache buckets once per-call
/// assistant usage is emitted (`usage_from_assistant_message`). The result is
/// still authoritative for the cumulative reported cost and carries the
/// per-model `modelUsage.contextWindow`, so this emits a token-less
/// Session-scoped snapshot the Hub revision-replaces in place. Results with no
/// cost and no modelUsage (including all-zero results) produce nothing.
fn usage_from_result(
    mapper: &mut Mapper,
    result: &ResultMessage,
) -> DriverResult<Vec<Observation>> {
    // Learn native per-model windows from modelUsage for later calls.
    if let Some(model_usage) = result.model_usage.as_ref().and_then(Value::as_object) {
        for (model, blob) in model_usage {
            if let Some(window) = blob
                .get("contextWindow")
                .and_then(Value::as_u64)
                .filter(|window| *window > 0)
            {
                mapper.stream.model_windows.insert(model.clone(), window);
            }
        }
    }
    let cost = result.total_cost_usd.map(|amount| Cost {
        amount: amount.to_string(),
        currency: "USD".into(),
    });
    let window = result
        .model_usage
        .as_ref()
        .and_then(Value::as_object)
        .filter(|models| models.len() == 1)
        .and_then(|models| {
            models
                .values()
                .next()
                .and_then(|blob| blob.get("contextWindow"))
                .and_then(Value::as_u64)
                .filter(|window| *window > 0)
        });
    // c-ctxusage x c-usagefu reconciliation — the result frame produces TWO
    // usage observations, satisfying both accepted contracts:
    //
    // 1. A TURN-scoped snapshot carrying the frame's summed turn `usage`
    //    (c-ctxusage: the live result receipt with its nativeAt feeds the
    //    per-call TPM window; scope keyed on the zero-based result index so
    //    successive frames merge revision-wise). When per-call durable
    //    Messages also cover the same turn the Hub fold suppresses this row,
    //    so print/SDK frames that already carry per-call counters never
    //    double count.
    // 2. A SESSION-scoped cost/window carrier with NO token stock
    //    (c-usagefu: `result.usage` sums every call's context and must never
    //    become cumulative session totals; the reported cumulative cost and
    //    the modelUsage contextWindow ride this observation).
    //
    // A frame with no tokens, cost or window produces nothing.
    let usage = result.usage.as_ref().filter(|u| u.is_object());
    let result_input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64);
    let result_output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64);
    let result_cache_read = usage
        .and_then(|u| {
            u.get("cache_read_input_tokens")
                .or_else(|| u.get("cached_input_tokens"))
        })
        .and_then(Value::as_u64);
    let result_cache_write = usage
        .and_then(|u| {
            u.get("cache_creation_input_tokens")
                .or_else(|| u.get("cache_write_input_tokens"))
        })
        .and_then(Value::as_u64);
    if result_input.is_none()
        && result_output.is_none()
        && result_cache_read.is_none()
        && result_cache_write.is_none()
        && cost.is_none()
        && window.is_none()
    {
        return Ok(Vec::new());
    }
    // An all-ZERO call with no cost/window is a no-op too: every reported
    // bucket is a structural zero (or omitted), so there is nothing to count
    // and nothing to bill.
    let all_token_buckets_zero_or_absent = [
        result_input,
        result_output,
        result_cache_read,
        result_cache_write,
    ]
    .iter()
    .all(|value| value.is_none() || *value == Some(0));
    if all_token_buckets_zero_or_absent && cost.is_none() && window.is_none() {
        return Ok(Vec::new());
    }
    // Turn position: raw result index (0-based -> revision 1/2/3), matching
    // how the transcript mapper stamps successive result frames.
    let turn_revision = result.result_index.map(|index| index + 1).unwrap_or(1);
    let scope_id = match (result.num_turns, result.result_index) {
        (Some(turn), Some(index)) => format!("{turn}:{index}"),
        (Some(turn), None) => turn.to_string(),
        (None, Some(index)) => index.to_string(),
        (None, None) => "0".into(),
    };
    // The session carrier revision stays monotonic across turns AND Workflow
    // sub-results (num_turns*1000 + position).
    let session_revision = result
        .num_turns
        .unwrap_or(1)
        .saturating_mul(1000)
        .saturating_add(turn_revision);
    let session_id = mapper.session_id.clone();
    let total = result_input
        .zip(result_output)
        .map(|(input, output)| input.saturating_add(output));

    let mut observations = Vec::new();

    // (1) Turn-scoped snapshot.
    //
    // When this run already emitted per-call usage (assistant /
    // message_delta), the result frame's summed usage is suppressed here —
    // including its cost, which rides the SESSION carrier below — so exactly
    // the per-call Turn rows exist (c-usagefu: the result re-sums the same
    // calls). When the run had no per-call channel, the result tokens ARE
    // the only turn evidence and ride this row (c-ctxusage TPM window); a
    // cost-only frame on such a stream revises the same Turn row.
    let emit_turn = !mapper.saw_per_call_usage;
    let result_has_tokens = result_input.is_some()
        || result_output.is_some()
        || result_cache_read.is_some()
        || result_cache_write.is_some();
    if emit_turn && (result_has_tokens || cost.is_some()) {
        let mut turn_payload = UsagePayload {
            usage_id: Id::new("obj")?,
            scope: UsageScope::Turn,
            scope_id: scope_id.clone(),
            mode: UsageMode::Snapshot,
            metric_revision: U64(turn_revision),
            source_session_id: None,
            input_tokens: opt_u64(result_input),
            input_accounting: InputAccounting::Unknown,
            output_tokens: opt_u64(result_output),
            reasoning_tokens: unknown("not-emitted"),
            cache_read_tokens: opt_u64(result_cache_read),
            cache_write_tokens: opt_u64(result_cache_write),
            total_tokens: opt_u64(total),
            cost: cost
                .as_ref()
                .map(|cost| Knowledge::Known {
                    value: cost.clone(),
                })
                .unwrap_or_else(|| unknown("not-emitted")),
            accounting: remuda_protocol::Accounting::Reported,
            native_fields_ref: None,
            context_window: window.map(U64),
        };
        turn_payload.usage_id = Id::derive(
            "obj",
            mapper.instance_id.as_id().as_str(),
            &format!("usage:result-turn:{scope_id}"),
        )?;
        observations.push(mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            ObservationPayload::Usage(Box::new(turn_payload)),
        )?);
    }

    // (2) Session-scoped cost/window carrier — token-less by design.
    let mut session_payload = UsagePayload {
        usage_id: Id::new("obj")?,
        scope: UsageScope::Session,
        scope_id: session_id,
        mode: UsageMode::Snapshot,
        metric_revision: U64(session_revision),
        source_session_id: None,
        input_tokens: unknown("not-emitted-on-result"),
        input_accounting: InputAccounting::Unknown,
        output_tokens: unknown("not-emitted-on-result"),
        reasoning_tokens: unknown("not-emitted-on-result"),
        cache_read_tokens: unknown("not-emitted-on-result"),
        cache_write_tokens: unknown("not-emitted-on-result"),
        total_tokens: unknown("not-emitted-on-result"),
        cost: cost
            .as_ref()
            .map(|cost| Knowledge::Known {
                value: cost.clone(),
            })
            .unwrap_or_else(|| unknown("not-emitted")),
        accounting: remuda_protocol::Accounting::Estimated,
        native_fields_ref: None,
        context_window: window.map(U64),
    };
    session_payload.usage_id = Id::derive(
        "obj",
        mapper.instance_id.as_id().as_str(),
        &format!("usage:result-session:{session_revision}"),
    )?;
    observations.push(mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::Usage(Box::new(session_payload)),
    )?);

    Ok(observations)
}

fn map_task_started(mapper: &mut Mapper, task: &TaskStarted) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    // D-057 OA6 (ma-sdk-state r3): track the workflow's IDENTITY against the
    // root turn that is currently emitting (the FIFO front), not a
    // process-global count. The terminal task_notification closes it.
    // local_agent/local_bash are subagent/shell tasks that do not own the root
    // result sequence.
    if task.task_type.as_deref() == Some("local_workflow") {
        mapper.turns.workflow_started(&task_id);
    }
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let tool_call_id = match &task.tool_use_id {
        Some(id) => Some(mapper.ids.tool(id)?),
        None => None,
    };
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: unknown("not-emitted"),
            native_task_id: Knowledge::Known { value: task_id },
            tool_call_id,
            state: WorkflowState::Running,
            revision: U64(1),
            title: match &task.description {
                Some(text) => Knowledge::Known {
                    value: text.clone(),
                },
                None => unknown("not-emitted"),
            },
            name: None,
            description: None,
            totals: None,
            launched_at: None,
            live: None,
            note: None,
            result_ref: None,
        })),
    )?])
}

fn map_task_progress(mapper: &mut Mapper, task: &TaskProgress) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let phase_id = mapper.ids.phase(&format!("{task_id}-progress"))?;
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowPhase(Box::new(WorkflowPhasePayload {
            workflow_id,
            phase_id,
            native_phase_id: Knowledge::Known {
                value: "progress".into(),
            },
            label: match &task.summary {
                Some(text) => Knowledge::Known {
                    value: text.clone(),
                },
                None => unknown("not-emitted"),
            },
            state: WorkflowState::Running,
            revision: U64(1),
            parent_phase_id: None,
        })),
    )?])
}

fn map_task_updated(mapper: &mut Mapper, task: &TaskUpdated) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    // Defensive companion to `map_task_notification`: real captures carry the
    // terminal patch (status=killed) one frame BEFORE the notification. Closing
    // on either terminal signal means a missing/renamed notification can never
    // strand the owning root turn in "working" forever.
    let patch_status = task
        .patch
        .as_ref()
        .and_then(|patch| patch.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if workflow_status_is_terminal(patch_status) {
        mapper.turns.workflow_terminated(&task_id);
    }
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let status = patch_status;
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: unknown("not-emitted"),
            native_task_id: Knowledge::Known { value: task_id },
            tool_call_id: None,
            state: workflow_state(status),
            revision: U64(2),
            title: unknown("not-emitted"),
            name: None,
            description: None,
            totals: None,
            launched_at: None,
            live: None,
            note: None,
            result_ref: None,
        })),
    )?])
}

fn map_task_notification(
    mapper: &mut Mapper,
    task: &TaskNotification,
) -> DriverResult<Vec<Observation>> {
    let task_id = task.task_id.clone().unwrap_or_default();
    // D-057 OA6 (ma-sdk-state r3): the terminal task_notification
    // (stopped/completed/failed) is the evidence that closes a workflow — the
    // later `result` never reliably does, because in real captures the single
    // turn result arrives AFTER the notification with result_index 0 (the
    // process-global index), and a finished workflow turn otherwise stays
    // "working" forever.
    let status = task.status.as_deref().unwrap_or("completed");
    if workflow_status_is_terminal(status) {
        mapper.turns.workflow_terminated(&task_id);
    }
    let workflow_id = mapper.ids.workflow(&task_id)?;
    let tool_call_id = match &task.tool_use_id {
        Some(id) => Some(mapper.ids.tool(id)?),
        None => None,
    };
    Ok(vec![mapper.observation(
        Completeness::Structured,
        NativeRequestKey::None,
        ObservationPayload::WorkflowRun(Box::new(WorkflowRunPayload {
            workflow_id,
            engine: WorkflowEngine::ClaudeWorkflow,
            native_run_id: unknown("not-emitted"),
            native_task_id: Knowledge::Known { value: task_id },
            tool_call_id,
            state: workflow_state(status),
            revision: U64(3),
            title: match &task.summary {
                Some(text) => Knowledge::Known {
                    value: text.clone(),
                },
                None => unknown("not-emitted"),
            },
            name: None,
            description: None,
            totals: None,
            launched_at: None,
            live: None,
            note: None,
            result_ref: None,
        })),
    )?])
}

impl Mapper {
    /// Completeness for a streamed content block: `Partial` while it is still
    /// open, `Structured` once the final block closes it.
    ///
    /// D-028a item 3 / `print-replacement.md` §2.5: on the sdk carrier a stream
    /// delta is explicitly a partial observation and the final assistant block
    /// is the authority. `claude-print` kept every content observation
    /// `Structured` before this parameter existed, and stays that way — its
    /// consumers and journal fixtures were built against that, and nothing
    /// measured on print changed here.
    fn content_completeness(&self, closed: bool) -> Completeness {
        if closed || self.driver_kind != DriverKind::ClaudeSdk {
            Completeness::Structured
        } else {
            Completeness::Partial
        }
    }

    fn observation(
        &mut self,
        completeness: Completeness,
        native_request_id: NativeRequestKey,
        body: ObservationPayload,
    ) -> DriverResult<Observation> {
        self.seq += 1;
        Ok(Observation {
            schema_version: SchemaVersion,
            event_id: EventId::new(),
            journal_id: self.journal_id.clone(),
            instance_id: self.instance_id.clone(),
            run_id: Some(self.run_id.clone()),
            host_id: self.host_id.clone(),
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            seq: U64(self.seq),
            observed_at: now()?,
            native_at: unknown("not-emitted"),
            source: ObservationSource {
                driver_kind: self.driver_kind,
                driver_version: self.pin.version.clone(),
                adapter_version: ADAPTER_VERSION.into(),
                channel: self.channel,
                delivery: SourceDelivery::Live,
                native_session_id: Knowledge::Known {
                    value: self.session_id.clone(),
                },
                native_turn_id: unknown("not-emitted"),
                native_agent_id: Knowledge::NotApplicable,
                native_item_id: unknown("not-emitted"),
                native_event_id: unknown("not-emitted"),
                native_request_id,
                source_cursor: SourceCursor::Runtime(Box::new(RuntimeCursor {
                    ledger_revision: U64(self.seq),
                })),
            },
            completeness,
            raw_ref: None,
            evidence_event_ids: vec![],
            body,
        })
    }

    fn lifecycle(
        &mut self,
        topic: LifecycleTopic,
        native_name: &str,
        native_id: Knowledge<String>,
        status: &str,
        affects_completion: bool,
    ) -> DriverResult<Observation> {
        self.lifecycle_related(
            topic,
            native_name,
            native_id,
            status,
            std::collections::BTreeMap::new(),
            affects_completion,
        )
    }

    fn lifecycle_related(
        &mut self,
        topic: LifecycleTopic,
        native_name: &str,
        native_id: Knowledge<String>,
        status: &str,
        related_ids: std::collections::BTreeMap<String, String>,
        affects_completion: bool,
    ) -> DriverResult<Observation> {
        self.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic,
                    native_name: native_name.into(),
                    native_id,
                    status: Knowledge::Known {
                        value: status.into(),
                    },
                    related_ids,
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion,
                },
            )))),
        )
    }

    /// D-057 OA6 (ma-sdk-state): mark a new root turn started right after its
    /// user frame was written to the native process. Topic/name pair with the
    /// `turn/result` lifecycle from [`map_result`]; the Hub projects activity
    /// `working` from this event and `idle` only from that settled result.
    ///
    /// Opening the turn in [`TurnBook`] HERE — at write completion, before the
    /// observation is published — is what lets a later `result` be correlated
    /// with its own root turn: the FIFO front is the turn currently emitting,
    /// and a buffered older result mapped after a newer turn_started cannot
    /// settle the newer turn.
    ///
    /// The Remuda command id is owned by the Node and is not known here; the
    /// native client message id (echoed by the harness on its `user` record) is
    /// carried so a later fold can join the two.
    /// Open the settlement book for a new root turn BEFORE its prompt write
    /// blocks (ma-sdk-state r5 item 1), emitting nothing. Returns the turn's
    /// seq so the publication ticket can clear the right reservation at commit.
    fn begin_turn_reservation(&mut self) -> u64 {
        self.turns.begin_turn()
    }

    /// The reserved turn's write committed; clear the in-flight reservation so
    /// no later turn reads a stale one. Called by the publication worker from
    /// the reservation ticket's queue position. Only clears when the
    /// committing turn is still the reserved one, so a newer begin wins.
    fn commit_turn_reservation(&mut self, seq: u64) {
        self.turns.turn_committed(seq);
    }

    /// Roll back the reservation for `seq` when its write fails. When that
    /// rollback releases a deferred root settle (r6 item 5), build the owed
    /// settled-turn observation so the caller can emit it.
    fn cancel_turn_reservation(&mut self, seq: u64) -> DriverResult<Option<Observation>> {
        if !self.turns.cancel_turn(seq) {
            return Ok(None);
        }
        let Some(deferred) = self.deferred_result.take() else {
            return Ok(None);
        };
        let mut related = std::collections::BTreeMap::new();
        if let Some(error) = deferred.last_error {
            related.insert("lastError".into(), error);
        }
        // The blocked newer turn vanished (its write failed), so the earlier
        // completed turn now settles the root. resultIndex is intentionally
        // absent: this is a settlement correction, not a new native result.
        related.insert("settledRootTurn".into(), "true".into());
        related.insert("deferredSettle".into(), "true".into());
        let session_id = self.session_id.clone();
        Ok(Some(self.lifecycle_related(
            LifecycleTopic::Turn,
            "result",
            Knowledge::Known { value: session_id },
            deferred.status,
            related,
            false,
        )?))
    }

    /// Build the turn_started observation WITHOUT touching the book — used
    /// after a write commits when the book was already pre-opened by
    /// [`Self::begin_turn_reservation`].
    fn turn_started_observation(&mut self, client_message_id: &str) -> DriverResult<Observation> {
        let mut related = std::collections::BTreeMap::new();
        if !client_message_id.is_empty() {
            related.insert("nativeClientMessageId".into(), client_message_id.into());
        }
        let session_id = self.session_id.clone();
        self.lifecycle_related(
            LifecycleTopic::Turn,
            "turn_started",
            Knowledge::Known { value: session_id },
            "working",
            related,
            false,
        )
    }

    /// Stream/replay equivalent of the live write-completed start. Only the
    /// test-stub mapper wrapper calls it; the live reader uses the
    /// begin-commit/turn_started_observation pair.
    #[cfg(any(test, feature = "test-stub"))]
    fn turn_started(&mut self, client_message_id: &str) -> DriverResult<Observation> {
        let seq = self.turns.begin_turn();
        // A start derived directly from the stream (replay/hydration) has no
        // in-flight write, so it holds no reservation.
        self.turns.turn_committed(seq);
        self.turn_started_observation(client_message_id)
    }

    /// ma-sdk-state r12 item 1: the terminal observation for a publication
    /// backlog overflow. A `session` lifecycle with the exact Failed token
    /// "failed", carrying a machine-readable reason and the occupancy/caps that
    /// tripped it, so the overflow shutdown is diagnosable from the journal.
    fn session_backlog_overflow_observation(
        &mut self,
        items: usize,
        bytes: usize,
        limit_items: usize,
        limit_bytes: usize,
    ) -> DriverResult<Observation> {
        let mut related = std::collections::BTreeMap::new();
        related.insert("reason".into(), "publication_backlog_overflow".into());
        related.insert("backlogItems".into(), items.to_string());
        related.insert("backlogBytes".into(), bytes.to_string());
        related.insert("backlogItemsLimit".into(), limit_items.to_string());
        related.insert("backlogBytesLimit".into(), limit_bytes.to_string());
        let session_id = self.session_id.clone();
        self.lifecycle_related(
            LifecycleTopic::Session,
            "session",
            Knowledge::Known { value: session_id },
            "failed",
            related,
            false,
        )
    }

    fn lifecycle_named(
        &mut self,
        topic: LifecycleTopic,
        name: &str,
    ) -> DriverResult<Vec<Observation>> {
        let session_id = self.session_id.clone();
        Ok(vec![self.lifecycle(
            topic,
            name,
            Knowledge::Known { value: session_id },
            name,
            false,
        )?])
    }

    fn lifecycle_hook(
        &mut self,
        name: &str,
        hook_id: &str,
        hook_event: &str,
    ) -> DriverResult<Vec<Observation>> {
        let mut related = std::collections::BTreeMap::new();
        if !hook_id.is_empty() {
            related.insert("hookId".into(), hook_id.into());
        }
        if !hook_event.is_empty() {
            related.insert("hookEvent".into(), hook_event.into());
        }
        Ok(vec![self.lifecycle_related(
            LifecycleTopic::Hook,
            name,
            Knowledge::Known {
                value: hook_id.to_owned(),
            },
            name,
            related,
            false,
        )?])
    }

    fn opaque(
        &mut self,
        native_type: &str,
        reason: OpaqueReason,
        value: &Value,
    ) -> DriverResult<Vec<Observation>> {
        let bytes = serde_json::to_vec(value)?;
        let digest = crate::binary::hash_bytes(&bytes)?;
        Ok(vec![self.observation(
            Completeness::Opaque,
            NativeRequestKey::None,
            ObservationPayload::Opaque(Box::new(OpaquePayload {
                native_type: native_type.into(),
                reason,
                raw_ref: RawRef {
                    object_id: Id::new("obj")?,
                    offset: U64(0),
                    length: U64(bytes.len() as u64),
                    digest,
                    media_type: "application/json".into(),
                    redaction: Redaction::None,
                },
                affects: vec![OpaqueImpact::Presentation],
                summary: None,
            })),
        )?])
    }
}

/// D-051 (6a) max inline plan text a reviewer sees. Larger plans fail closed
/// to an Approval (human-only, full text in the transcript) rather than being
/// truncated or shipped through a second channel. One host's inbox
/// aggregates every pending interaction into one list RPC, so the cap keeps a
/// burst of plans well under the 1 MiB frame limit.
const MAX_PLAN_REVIEW_BYTES: usize = 32 * 1024;

fn build_interaction(
    mapper: &mut Mapper,
    env: &ControlRequestEnvelope,
    req: &CanUseToolRequest,
    interaction_id: InteractionId,
) -> DriverResult<Interaction> {
    let now = now()?;
    let digest = crate::binary::hash_bytes(&serde_json::to_vec(&req.input)?)?;
    let tool_call_id = match &req.tool_use_id {
        Some(id) => Some(mapper.ids.tool(id)?),
        None => None,
    };
    let ask = req.tool_name == "AskUserQuestion";
    // D-051 (6a): a TOP-LEVEL ExitPlanMode with an inline plan under the cap
    // is a PlanReview routed to the parent. Every other shape (nested call,
    // missing/too-large plan, a tool_use the mapper never saw at top level)
    // stays a human-only Approval — fail closed.
    let plan_review = if !ask {
        exit_plan_review_request(mapper, req, tool_call_id.as_ref())?
    } else {
        None
    };
    let kind = if ask {
        InteractionKind::Question
    } else if plan_review.is_some() {
        InteractionKind::PlanReview
    } else {
        InteractionKind::Approval
    };
    let request = if ask {
        InteractionRequest::Question(Box::new(question_request(req)))
    } else if let Some(review) = plan_review {
        InteractionRequest::PlanReview(Box::new(review))
    } else {
        InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: req
                .display_name
                .clone()
                .unwrap_or_else(|| req.tool_name.clone()),
            description: req.description.clone().unwrap_or_default(),
            tool_call_id,
            action_ref: Id::new("obj")?,
            options: vec![
                DecisionOption {
                    id: "allow".into(),
                    label: "Allow".into(),
                    effect: DecisionEffect::AllowOnce,
                    native_value_ref: Id::new("obj")?,
                },
                DecisionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    effect: DecisionEffect::Deny,
                    native_value_ref: Id::new("obj")?,
                },
            ],
            requested_permissions_ref: None,
            input_digest: digest,
        }))
    };
    Ok(Interaction {
        meta: remuda_protocol::EntityMeta {
            id: interaction_id.clone(),
            revision: U64(1),
            created_at: now.clone(),
            updated_at: now,
        },
        instance_id: mapper.instance_id.clone(),
        run_id: Some(mapper.run_id.clone()),
        host_id: mapper.host_id.clone(),
        kind,
        request_key: InteractionRequestKey {
            native: NativeRequestKey::Rpc {
                value_type: NativeRequestValueType::String,
                value: env.request_id.clone(),
            },
            process_generation: U64(1),
            run_generation: Some(U64(1)),
            connection_epoch: Id::new("epoch")?,
        },
        request_version: U64(1),
        state: InteractionState::Pending,
        blocking: true,
        answerable: true,
        carrier: InteractionCarrier::ClaudeControl,
        request,
        deadline: unknown("none"),
        deadline_source: remuda_protocol::DeadlineSource::None,
        answer: unknown("pending"),
        delivery: remuda_protocol::DeliveryState::NotSent,
        resolution: unknown("pending"),
    })
}

/// Build the PlanReview request when the ExitPlanMode pause satisfies
/// D-051 (6a); otherwise `None` so the caller falls back to an Approval.
fn exit_plan_review_request(
    mapper: &Mapper,
    req: &CanUseToolRequest,
    mapped_tool_call_id: Option<&Id>,
) -> DriverResult<Option<PlanReviewRequest>> {
    if req.tool_name != "ExitPlanMode" {
        return Ok(None);
    }
    // The pause must correspond to a top-level tool_use the mapper saw. A
    // nested (sub-agent) call or an id the assistant stream never produced
    // fails closed.
    let Some(tool_call_id) = mapped_tool_call_id else {
        return Ok(None);
    };
    if !mapper.stream.top_level_exit_plan.contains(tool_call_id) {
        return Ok(None);
    }
    // Inline plan text, within the byte cap. The predicate is "a string no
    // larger than the cap" (D-051 (6a)): an empty string still mints — the
    // reviewer then approves/denies on the tool call itself, and the UI shows
    // the open-session hint. Absent/non-string or oversized → Approval.
    let Some(plan) = req.input.get("plan").and_then(Value::as_str) else {
        return Ok(None);
    };
    if plan.len() > MAX_PLAN_REVIEW_BYTES {
        return Ok(None);
    }
    let plan_digest = crate::binary::hash_bytes(plan.as_bytes())?;
    Ok(Some(PlanReviewRequest {
        title: req
            .display_name
            .clone()
            .unwrap_or_else(|| "Plan review".to_string()),
        // Unparsed placeholder; the inline `plan` is authoritative.
        plan_ref: Id::new("obj")?,
        plan_revision: U64(1),
        plan_digest,
        options: vec![
            DecisionOption {
                id: "approve".into(),
                label: "Approve".into(),
                effect: DecisionEffect::AllowOnce,
                native_value_ref: Id::new("obj")?,
            },
            DecisionOption {
                id: "deny".into(),
                label: "Deny".into(),
                effect: DecisionEffect::Deny,
                native_value_ref: Id::new("obj")?,
            },
        ],
        allow_feedback: true,
        plan: Some(plan.to_string()),
    }))
}

fn question_request(req: &CanUseToolRequest) -> QuestionRequest {
    let questions = req
        .input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let fields = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let title = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("question")
                .to_owned();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(opt_i, option)| QuestionOption {
                    id: option
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    label: option
                        .get("label")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| format!("option-{opt_i}")),
                    description: option
                        .get("description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                })
                .collect();
            let multi = question
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            QuestionField {
                id: format!("q{index}"),
                title: title.clone(),
                description: question
                    .get("header")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                input: if multi {
                    QuestionInput::MultiSelect
                } else {
                    QuestionInput::SingleSelect
                },
                required: true,
                options,
                allow_free_text: false,
                sensitive: false,
            }
        })
        .collect();
    QuestionRequest {
        title: "AskUserQuestion".into(),
        fields,
    }
}

fn permission_from_answer(
    input: &Value,
    answer: &InteractionAnswer,
) -> DriverResult<ControlSuccessPayload> {
    match answer {
        InteractionAnswer::Approval(approval) => {
            if approval.option_id == "allow" {
                Ok(ControlSuccessPayload::Permission(PermissionResult::Allow {
                    updated_input: input.clone(),
                    updated_permissions: None,
                }))
            } else {
                Ok(ControlSuccessPayload::Permission(PermissionResult::Deny {
                    message: "host denied".into(),
                    interrupt: Some(false),
                }))
            }
        }
        // D-051 (6c): a plan-review resolution is the ExitPlanMode permission
        // response. Approve lets the call proceed with its original input and
        // NO permission escalation — the CLI itself moves plan → default,
        // never acceptEdits/bypass. Deny feeds the reviewer's feedback
        // verbatim; only an ABSENT (null) note falls back to the default
        // message. Whitespace is forwarded as-is (the reviewer chose it;
        // validity is enforced upstream in validate_answer).
        InteractionAnswer::PlanReview(plan) => {
            if plan.option_id == "approve" {
                Ok(ControlSuccessPayload::Permission(PermissionResult::Allow {
                    updated_input: input.clone(),
                    updated_permissions: None,
                }))
            } else {
                Ok(ControlSuccessPayload::Permission(PermissionResult::Deny {
                    message: plan
                        .feedback
                        .clone()
                        .unwrap_or_else(|| "plan review denied".into()),
                    // The recorded 2.1.277 deny frame omits `interrupt` (the
                    // CLI defaults to non-interrupting); match it exactly so
                    // the VCR deep-equal of the payload passes and no
                    // undocumented field rides along.
                    interrupt: None,
                }))
            }
        }
        InteractionAnswer::Question(question) => {
            let mut updated = input.clone();
            let mut answers = serde_json::Map::new();
            for (field_id, field) in &question.answers {
                let text = field
                    .text
                    .clone()
                    .or_else(|| field.option_ids.first().cloned())
                    .unwrap_or_default();
                let key = input
                    .get("questions")
                    .and_then(Value::as_array)
                    .and_then(|questions| {
                        field_id
                            .strip_prefix('q')
                            .and_then(|n| n.parse::<usize>().ok())
                            .and_then(|i| questions.get(i))
                    })
                    .and_then(|q| q.get("question"))
                    .and_then(Value::as_str)
                    .unwrap_or(field_id.as_str())
                    .to_owned();
                answers.insert(key, Value::String(text));
            }
            if let Some(obj) = updated.as_object_mut() {
                obj.insert("answers".into(), Value::Object(answers));
            }
            Ok(ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input: updated,
                updated_permissions: None,
            }))
        }
        _ => Err(DriverError::InvalidLaunchSpec(
            "unsupported interaction answer".into(),
        )),
    }
}

/// Largest total of inlined image bytes on one turn.
///
/// The CLI re-compresses above its own internal budget, and a very large
/// payload is more likely to be refused than answered. Past this the driver
/// mentions paths instead, which the Read tool can still act on.
const MAX_INLINE_IMAGE_BYTES: usize = 3_584 * 1024;

/// Build the `user` content for one prompt.
///
/// With no attachments this is the plain string it has always been. With
/// attachments it becomes Anthropic content blocks: each image inlined as
/// base64, then the text. Non-image files (D-027b) are never inlined — the
/// CLI has no file block verified to work — so their `[File #n] … saved at …`
/// lines are prepended to the text block, which the harness's Read tool acts
/// on.
///
/// Base64 is not a preference. The design verified that a `source.type` of
/// `file` or a bare path is silently downgraded to text by the CLI, so the
/// image would be dropped without an error — worse than not sending it.
fn prompt_content(blocks: &[ContentBlock]) -> DriverResult<UserContent> {
    let attachments = crate::attachment::attachments_of(blocks);
    if attachments.is_empty() {
        return Ok(UserContent::Text(prompt_text(blocks)?));
    }
    let images: Vec<_> = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect();
    // Any non-image attachment rides a text block instead of an image block.
    let has_files = attachments.len() != images.len();
    let mut inline = Vec::with_capacity(images.len());
    let mut total = 0usize;
    for attachment in &images {
        let bytes = attachment.read()?;
        total = total.saturating_add(bytes.len());
        if total > MAX_INLINE_IMAGE_BYTES {
            // Too big to inline: fall back to the path mention so the agent
            // can still reach the image through its Read tool.
            tracing::warn!(
                object_id = %attachment.object_id,
                total,
                "attachments exceed the inline budget; sending paths instead"
            );
            return Ok(UserContent::Text(
                crate::attachment::text_with_path_mentions(blocks)?,
            ));
        }
        inline.push(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": attachment.media_type,
                "data": base64_of(&bytes),
            },
        }));
    }
    // File-reference lines precede the user text; when every attachment is an
    // image the text block is the prompt unchanged.
    let text = if has_files {
        crate::attachment::with_file_lines(&crate::attachment::text_of(blocks), &attachments)
    } else {
        crate::attachment::text_of(blocks)
    };
    if !text.is_empty() {
        inline.push(serde_json::json!({"type": "text", "text": text}));
    }
    Ok(UserContent::Blocks(inline))
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn prompt_text(blocks: &[ContentBlock]) -> DriverResult<String> {
    let mut parts = Vec::new();
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    if parts.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "prompt has no text blocks".into(),
        ));
    }
    Ok(parts.join(""))
}

fn reject_bot_bypass(spec: &InstanceSpec, origin: InputOrigin) -> DriverResult<()> {
    let gated = match &spec.permission_mode {
        PermissionMode::Claude(claude) => matches!(
            claude.mode,
            ClaudePermissionMode::BypassPermissions | ClaudePermissionMode::DontAsk
        ),
        _ => false,
    };
    if gated && matches!(origin, InputOrigin::Bot | InputOrigin::Agent) {
        return Err(DriverError::BypassNotAllowedForBot);
    }
    Ok(())
}

fn policy_from_recipe(recipe: &LaunchRecipe, spec: &InstanceSpec) -> PermissionPolicy {
    if recipe.permission.cli_mode.as_deref() == Some("bypassPermissions")
        || recipe
            .permission
            .extra_flags
            .iter()
            .any(|flag| flag.contains("skip-permissions"))
    {
        return PermissionPolicy::AutoAllow;
    }
    if recipe.permission.cli_mode.as_deref() == Some("dontAsk")
        || recipe
            .technical_debt
            .iter()
            .any(|tag| tag == crate::TECH_DEBT_M0_PERM_01)
    {
        return PermissionPolicy::AutoDeny;
    }
    match &spec.permission_mode {
        PermissionMode::Claude(claude) if claude.mode == ClaudePermissionMode::DontAsk => {
            PermissionPolicy::AutoDeny
        }
        PermissionMode::Claude(claude)
            if claude.mode == ClaudePermissionMode::BypassPermissions =>
        {
            PermissionPolicy::AutoAllow
        }
        _ => PermissionPolicy::Host,
    }
}

fn apply_bypass_flag(recipe: &mut LaunchRecipe) {
    if recipe.permission.cli_mode.as_deref() != Some("bypassPermissions") {
        return;
    }
    let flag = "--allow-dangerously-skip-permissions";
    if !recipe.argv.iter().any(|token| token == flag) {
        recipe.argv.push(flag.into());
    }
    if !recipe
        .permission
        .extra_flags
        .iter()
        .any(|token| token == flag)
    {
        recipe.permission.extra_flags.push(flag.into());
    }
}

fn refuse_prohibited_argv(argv: &[String]) -> DriverResult<()> {
    if argv.iter().any(|token| {
        crate::flags::is_banned_flag_token(token)
            || matches!(
                token.as_str(),
                "--bare" | "--safe-mode" | "--no-session-persistence" | "--continue"
            )
    }) {
        return Err(DriverError::NativeFeatureDisabled(
            "refusing prohibited flag on stream-json argv".into(),
        ));
    }
    Ok(())
}

fn map_wire(error: remuda_claude_wire::Error) -> DriverError {
    match error {
        remuda_claude_wire::Error::Io(error)
        | remuda_claude_wire::Error::Spawn { source: error, .. } => DriverError::Io(error),
        remuda_claude_wire::Error::HandshakeTimeout { .. }
        | remuda_claude_wire::Error::HandshakeFailed { .. }
        | remuda_claude_wire::Error::HandshakeEof { .. }
        | remuda_claude_wire::Error::StdinClosed
        | remuda_claude_wire::Error::StdoutClosed => DriverError::ControlUnavailable,
        remuda_claude_wire::Error::ForbiddenFlag { flag } => {
            DriverError::InvalidLaunchSpec(format!("forbidden flag {flag}"))
        }
        remuda_claude_wire::Error::MissingPipe(_)
        | remuda_claude_wire::Error::LineTooLong { .. }
        | remuda_claude_wire::Error::Json(_)
        | remuda_claude_wire::Error::InvalidPermissionMode => {
            DriverError::InvalidLaunchSpec(error.to_string())
        }
    }
}

fn now() -> DriverResult<Timestamp> {
    let t = time::OffsetDateTime::now_utc();
    let text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond(),
    );
    Timestamp::try_from(text).map_err(DriverError::Protocol)
}

/// Parse a native transcript record timestamp into the protocol clock
/// (c-ctxusage r3 item 5): millisecond UTC canonical form.
fn native_timestamp(text: &str) -> Option<Timestamp> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(text, &Rfc3339).ok()?;
    let t = t.to_offset(time::UtcOffset::UTC);
    let canonical = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond(),
    );
    Timestamp::try_from(canonical).ok()
}

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "0")).expect("digest")
}

fn fallback_obj() -> Id {
    Id::new("obj").expect("obj prefix")
}

fn fallback_host() -> HostId {
    "hst_01993ab0-0000-7000-8000-000000000001"
        .parse()
        .expect("host")
}

fn known_or_unknown(value: Option<&str>) -> Knowledge<String> {
    match value {
        Some(value) => Knowledge::Known {
            value: value.to_owned(),
        },
        None => unknown("not-emitted"),
    }
}

fn unknown<T>(reason: &str) -> Knowledge<T> {
    Knowledge::Unknown {
        reason: reason.into(),
        evidence_event_ids: Vec::<EventId>::new(),
    }
}

/// A known numeric knowledge value, or `None` (absent bucket).
fn opt_u64(value: Option<u64>) -> Knowledge<U64> {
    match value {
        Some(value) => Knowledge::Known { value: U64(value) },
        None => Knowledge::Unknown {
            reason: "not-emitted".into(),
            evidence_event_ids: Vec::new(),
        },
    }
}

fn tool_category(name: &str) -> ToolCategory {
    match name {
        "Bash" => ToolCategory::Shell,
        "Read" => ToolCategory::FileRead,
        "Write" | "Edit" => ToolCategory::FileWrite,
        "Grep" | "Glob" => ToolCategory::Search,
        "Workflow" => ToolCategory::Workflow,
        // 2.1.x renamed Task to Agent; both spellings must classify as Agent so
        // the structured view's task track collects the row.
        "Task" | "Agent" => ToolCategory::Agent,
        other if other.starts_with("mcp__") => ToolCategory::Mcp,
        _ => ToolCategory::Other,
    }
}

fn workflow_state(status: &str) -> WorkflowState {
    match status {
        "pending" => WorkflowState::Queued,
        "running" => WorkflowState::Running,
        "completed" => WorkflowState::Completed,
        "failed" => WorkflowState::Failed,
        "killed" | "stopped" | "cancelled" => WorkflowState::Cancelled,
        _ => WorkflowState::Unknown,
    }
}

/// Maps Claude transcript (`.jsonl`) records into Observations.
///
/// The transcript on disk holds the same `user` / `assistant` records the
/// stream-json stdout carries, so a promoted `shell-pty` terminal (D-025) can
/// hydrate a structured view through exactly this mapper rather than a second
/// parser.
///
/// Three D-028 §7 behaviours live here on top of that replay:
///
/// - **Regrouping.** One record holds one content block and 2–7 records share a
///   `message.id`; they are buffered by [`records::GroupKey`] and replayed as
///   one message once the run is superseded. Feeding them individually is what
///   slid the tool bookkeeping by one.
/// - **Authorship.** Every `user` record is classified into a
///   [`MessageOrigin`], so injected skill bodies and hook context stop
///   masquerading as the human's words.
/// - **Retention.** `queue-operation`, `permission-mode` and
///   `attachment.queued_command` records are journaled as lifecycle rather
///   than dropped: they are the queue ledger and the mode-drift record.
///
/// §9.1 effort read-back is resolved from the `/effort` command's
/// `<local-command-stdout>` verdict (see
/// `remuda_protocol::parse_effort_stdout`), not from the next assistant turn.
///
/// Extract the text inside `<local-command-stdout>…</local-command-stdout>`.
fn extract_local_stdout(content: &str) -> String {
    let Some(start) = content.find("<local-command-stdout>") else {
        return content.trim().to_owned();
    };
    let body = &content[start + "<local-command-stdout>".len()..];
    let end = body.find("</local-command-stdout>").unwrap_or(body.len());
    body[..end].trim().to_owned()
}

/// Test-only stdout mapper over the real stream assembler.
///
/// The live path is [`ClaudePrintDriver`]'s reader task, whose [`Mapper`] is
/// private. `tests/claude_sdk_stream.rs` replays recorded NDJSON through the
/// same `map_outbound` the driver uses, so a fixture assertion is an assertion
/// about production behaviour rather than a reimplementation of it.
#[cfg(any(test, feature = "test-stub"))]
pub struct StdoutMapper {
    mapper: Mapper,
}

#[cfg(any(test, feature = "test-stub"))]
impl StdoutMapper {
    /// Mapper stamping `driver` on channel `stdout`.
    #[must_use]
    pub fn new(driver: DriverKind, session_id: &str) -> Self {
        Self {
            mapper: Mapper {
                stream: stream::StreamState::default(),
                ids: NativeIds::default(),
                seq: 0,
                instance_id: InstanceId::new(),
                run_id: RunId::new(),
                journal_id: fallback_obj(),
                host_id: fallback_host(),
                session_id: session_id.to_owned(),
                pin: BinaryPin {
                    abs_path: String::new(),
                    version: "fixture".into(),
                    sha256: dummy_digest(),
                },
                driver_kind: driver,
                channel: SourceChannel::Stdout,
                media_stager: None,
                turns: TurnBook::default(),
                saw_per_call_usage: false,
                deferred_result: None,
            },
        }
    }

    /// Map one decoded stdout frame, exactly as the reader task does.
    pub fn map(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        map_outbound(&mut self.mapper, &Outbound::from_value(value))
    }

    /// Open the settlement book for a new root turn BEFORE its prompt write
    /// blocks (ma-sdk-state r4 item 1), without emitting anything.
    pub fn begin_turn(&mut self) {
        self.mapper.begin_turn_reservation();
    }

    /// Roll back the most recently opened turn when its prompt write fails
    /// (replay/test helper). Seq-safe: it cancels exactly the current back.
    /// Returns the deferred root-settle observation this rollback releases, if
    /// any (r6 item 5).
    pub fn cancel_turn(&mut self) -> DriverResult<Option<Observation>> {
        let seq = self.mapper.turns.outstanding.back().map(|turn| turn.seq);
        if let Some(seq) = seq {
            self.mapper.cancel_turn_reservation(seq)
        } else {
            Ok(None)
        }
    }

    /// Build the turn_started observation for an already-reserved turn.
    pub fn turn_started_observation(
        &mut self,
        client_message_id: &str,
    ) -> DriverResult<Observation> {
        self.mapper.turn_started_observation(client_message_id)
    }

    /// Record a locally-written user frame exactly as the driver send path
    /// does (write-completed → turn_started), opening a new root turn in the
    /// settlement book. Returns the same lifecycle observation the live send
    /// publishes. Replay tests use it to correlate results with turns without
    /// spawning a process (r3 item 4).
    pub fn turn_started(&mut self) -> DriverResult<Observation> {
        self.mapper.turn_started("")
    }

    /// Attach the tool-media stager so image result blocks become object
    /// references instead of text fallbacks (D-045 §6.2).
    #[must_use]
    pub fn with_media_stager(mut self, stager: Arc<dyn remuda_protocol::ToolMediaStager>) -> Self {
        self.mapper.media_stager = Some(stager);
        self
    }

    /// Native session id the mapper has adopted from `system/init`.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.mapper.session_id
    }
}

/// Whether a group drain happens mid-stream (a poll boundary) or at a hard
/// boundary (a superseding message / user record / end of stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupDrain {
    Poll,
    Supersede,
}

/// One slot in the slash → verdict FIFO (c-effortread r16 item 2).
enum EffortSlashSlot {
    /// A `/effort` switch attributed to an armed bridge generation; the slot
    /// carries the IMMUTABLE request metadata (generation, id, requested
    /// value) bound at its slash record, so a delayed verdict pairs with its
    /// own request even after a newer one armed (c-effortread r11 item 1).
    Owned(u64, crate::effort::EffortRequest),
    /// A recognised switch slash that belongs to NO armed request: an older
    /// pre-floor command (c-effortread r15 item 4) or a human-typed
    /// `/effort`. The slot keeps the slash's PLACE in verdict order, so the
    /// verdict that answers it — which can land AFTER a newer owned slash —
    /// consumes this slot and never the owned request following it.
    /// `is_switch` is false for non-switch slashes (`status`/`current`/
    /// `auto`/an invalid word): their status/other verdicts prune only a
    /// non-switch slot and can never cross an outstanding switch slot.
    Unowned { is_switch: bool },
}

/// c-effortread r17 item 1: tri-state result of popping the verdict FIFO's
/// front slot — the OWNED request this verdict answers, an UNOWNED place (an
/// older pre-floor/human slash), or an EMPTY FIFO (a verdict whose slash
/// predates this mapper, e.g. a terminal command).
enum VerdictFront {
    Owned(u64, crate::effort::EffortRequest),
    Unowned,
    Empty,
}

/// c-effortread r18 item 3 / r19 item 2 ruling B: the ONE edge the latest
/// unowned accepted verdict WOULD have published, stashed while an armed
/// switch is still outstanding. The edge is held with ITS OWN verdict
/// record's deterministic event id and raw stdout, so when the armed switch
/// ends (refusal/timeout) the deferred publish derives the SAME id the
/// journal tailer derives for that record — the live mapper and durable
/// replay converge on one event instead of the value being swallowed by edge
/// dedup. Every suppressed acceptance OVERWRITES this slot, including one
/// equal to the published baseline (a return to baseline).
struct DeferredEffortEdge {
    observed: remuda_protocol::ObservedEffort,
    event_id: EventId,
    raw: Option<String>,
}

/// Transcript-to-observations mapper for a live claude-pty session.
pub struct TranscriptMapper {
    mapper: Mapper,
    group: records::Group,
    /// Running automatic record position for unpositioned callers
    /// (`map_line`/`map_record`, c-ctxusage r15 item 8): each record advances
    /// it exactly once so same-id revisions always carry strictly increasing
    /// positions. The production tail passes real file positions via
    /// [`Self::map_line_at`] instead.
    auto_position: u64,
    /// `(promptId, text)` of human prompts already emitted, for dedup.
    seen_prompts: std::collections::HashSet<(String, String)>,
    /// Record uuids already mapped, so a re-read tail cannot double-emit.
    seen_uuids: std::collections::HashSet<String>,
    /// §9.1 effort read-back state (dedup + source attribution).
    effort: remuda_protocol::EffortTracker,
    /// Rendezvous for Remuda-initiated switches awaiting read-back.
    effort_bridge: Option<Arc<crate::effort::EffortBridge>>,
    /// c-effortread r11 item 1 / r16 item 2: one FIFO slot per RECOGNISED
    /// `/effort` switch slash, in transcript order. An owned slot carries the
    /// IMMUTABLE request metadata (generation, id, requested value) bound at
    /// its slash record; a pre-floor or human slash gets an unowned slot. Each
    /// switch-family verdict pops the front slot, so a delayed old verdict
    /// pairs with ITS OWN slash — never with a newer owned request B typed
    /// while A's verdict was outstanding.
    effort_armed: std::collections::VecDeque<EffortSlashSlot>,
    /// c-effortread r18 item 3 / r19 item 2 ruling B: the ONE latest
    /// suppressed unowned acceptance edge awaiting the armed switch's end.
    /// Every suppressed acceptance overwrites it; any authoritative
    /// effective-state publication or the end-of-switch publish clears it via
    /// [`Self::invalidate_deferred_effort`].
    deferred_effort_edge: Option<DeferredEffortEdge>,
    /// c-effortread r12 item 1: test-only one-shot seam fired immediately
    /// after a `/effort` slash takes its single pending snapshot. It lets a
    /// unit test deterministically time the request out and arm a newer one
    /// BEFORE the snapshot tuple is attributed. Never present in production
    /// builds.
    #[cfg(test)]
    effort_slash_snapshot_hook: Option<Box<dyn FnOnce() + Send>>,
    /// §9.1 effective-model read-back state.
    model: remuda_protocol::ModelTracker,
    /// Rendezvous for Remuda-initiated `/model` switches.
    model_bridge: Option<Arc<crate::model::ModelBridge>>,
    /// Generation the model tracker is armed with.
    model_generation: Option<u64>,
    /// Discovered catalog stamped onto the first model observation.
    model_catalog: Option<remuda_protocol::ModelCatalogInfo>,
    /// Whether the launch model snapshot was already emitted.
    model_launch_emitted: bool,
    /// Effective permission-mode read-back state (dedup + attribution).
    permission: remuda_protocol::LivePermissionTracker,
    /// Rendezvous for Remuda-initiated permission switches.
    permission_bridge: Option<Arc<crate::permission::PermissionBridge>>,
}

impl TranscriptMapper {
    /// Build a mapper that stamps Observations for `instance_id` on `driver`.
    pub fn new(
        driver: DriverKind,
        instance_id: InstanceId,
        run_id: RunId,
        journal_id: Id,
        host_id: HostId,
        session_id: String,
        binary_version: String,
    ) -> Self {
        // D-056: seed the effort version gate from the pinned binary; the
        // transcript's own `version` records correct it as needed.
        let mut effort = remuda_protocol::EffortTracker::new();
        effort.seed_version(&binary_version);
        Self {
            mapper: Mapper {
                stream: stream::StreamState::default(),
                ids: NativeIds::scoped(instance_id.as_id().as_str()),
                seq: 0,
                instance_id,
                run_id,
                journal_id,
                host_id,
                session_id,
                pin: BinaryPin {
                    abs_path: String::new(),
                    version: binary_version,
                    sha256: dummy_digest(),
                },
                driver_kind: driver,
                channel: SourceChannel::Transcript,
                media_stager: None,
                turns: TurnBook::default(),
                saw_per_call_usage: false,
                deferred_result: None,
            },
            group: records::Group::default(),
            auto_position: 0,
            seen_prompts: std::collections::HashSet::new(),
            seen_uuids: std::collections::HashSet::new(),
            effort,
            effort_bridge: None,
            effort_armed: std::collections::VecDeque::new(),
            deferred_effort_edge: None,
            #[cfg(test)]
            effort_slash_snapshot_hook: None,
            model: remuda_protocol::ModelTracker::new(),
            model_bridge: None,
            model_generation: None,
            model_catalog: None,
            model_launch_emitted: false,
            permission: remuda_protocol::LivePermissionTracker::new(),
            permission_bridge: None,
        }
    }

    /// Attach the tool-media stager so image result blocks become object
    /// references instead of text fallbacks (D-045 §6.2).
    #[must_use]
    pub(crate) fn with_media_stager(
        mut self,
        stager: Arc<dyn remuda_protocol::ToolMediaStager>,
    ) -> Self {
        self.mapper.media_stager = Some(stager);
        self
    }

    /// Attach the §9.1 effort bridge so this mapper drives switch read-back and
    /// emits `effort` observations. `launch` names the `--effort` selection the
    /// process started with, when there was one.
    pub(crate) fn with_effort_bridge(
        mut self,
        bridge: Arc<crate::effort::EffortBridge>,
        launch: Option<remuda_protocol::EffortSelection>,
    ) -> Self {
        if launch.is_some() {
            self.effort.mark_launch();
        }
        self.effort_bridge = Some(bridge);
        self
    }

    /// c-effortread r12 item 1: install the one-shot test seam fired right
    /// after a `/effort` slash takes its pending snapshot (cfg(test) only).
    #[cfg(test)]
    pub(crate) fn set_effort_slash_snapshot_hook(&mut self, hook: Box<dyn FnOnce() + Send>) {
        self.effort_slash_snapshot_hook = Some(hook);
    }

    /// c-effortread r8 item 5: snapshot the EffortTracker latch
    /// (name, flag, semantics) and, per c-effortread r22 ruling S, the
    /// channel that published the level, for re-seeding a replacement mapper
    /// on re-promotion. None when no level has been observed.
    pub(crate) fn effort_seed(
        &self,
    ) -> Option<(
        remuda_protocol::EffortName,
        Option<bool>,
        remuda_protocol::EffortSemantics,
        remuda_protocol::LevelSource,
    )> {
        let last = self.effort.last_observed()?;
        Some((
            last.name,
            last.ultracode,
            self.effort.semantics(),
            self.effort.level_source(),
        ))
    }

    /// c-effortread r8 item 5: seed a freshly built mapper's EffortTracker
    /// with the last published observation of the previous (same pid/session)
    /// hydrator, so a demote/re-promotion does not forget the latched
    /// level/flag/semantics. Without it a decoupled build with ultracode on
    /// emits `{high, ultracode: None}` on the next attachment-less assistant
    /// record and the chip drops from "ultracode" to "high".
    ///
    /// c-effortread r22 item 2: the seeded level keeps its ORIGINAL
    /// LevelSource, so a status-only level stays movable by a later status.
    pub(crate) fn with_effort_seed(
        mut self,
        seed: (
            remuda_protocol::EffortName,
            Option<bool>,
            remuda_protocol::EffortSemantics,
            remuda_protocol::LevelSource,
        ),
    ) -> Self {
        let (name, ultracode, semantics, level_source) = seed;
        self.effort.seed_observed(
            name,
            ultracode,
            remuda_protocol::EffortSource::Remuda,
            semantics,
            level_source,
        );
        self
    }

    /// Set whether the records about to be mapped are provenanced to THIS
    /// process (D-056 (4)).
    ///
    /// The transcript pumps call this before each batch from the tail's
    /// provenance: `true` for bytes at/after the verified resume boundary (or
    /// any live-tail read), `false` for a resume tail displaced by a
    /// shrink/replacement whose current-process provenance can no longer be
    /// established — those records hydrate conversation but must never set
    /// effort/ultracode or settle a fresh switch, so read-back stays unknown.
    /// A live (non-resume) mapper is current by default.
    pub(crate) fn set_effort_current_process(&mut self, current: bool) {
        if current {
            self.effort.mark_current_process();
        } else {
            self.effort.begin_history();
        }
    }

    /// Drive the gate from a tail batch, returning an explicit
    /// read-back-unavailable observation when a previously VERIFIED resume
    /// becomes UNVERIFIABLE mid-run (item 5).
    ///
    /// `current` is this batch's provenance. When it flips to false the
    /// tracker clears its projected effective state; if that clears a
    /// previously published `{level, ultracode}`, the returned observation
    /// carries that effective state cleared to `None` so the UI stops showing
    /// a stale "effective" (it renders `?`) and no pending switch resolves
    /// Applied. Callers emit the returned observation like any mapped edge.
    pub(crate) fn apply_effort_provenance(
        &mut self,
        current: bool,
    ) -> DriverResult<Vec<Observation>> {
        if current {
            self.set_effort_current_process(true);
            return Ok(Vec::new());
        }
        self.set_effort_current_process(false);
        // A switch armed for this process must not be settled by a record the
        // tail can no longer attribute to it. Drop every armed generation
        // WITHOUT resolving or rejecting: the bridge keeps waiting for its
        // bounded timeout and degrades, never reporting Applied (item 5).
        self.effort_armed.clear();
        let Some(cleared) = self.effort.read_back_unavailable() else {
            return Ok(Vec::new());
        };
        let requested = self
            .effort_bridge
            .as_ref()
            .and_then(|bridge| bridge.requested())
            .map(|request| remuda_protocol::EffortSelection {
                name: request.name,
                ultracode: request.ultracode,
            });
        // `cleared` only proves a projected state existed (the edge is
        // emitted once). It is NOT republished: name and ultracode withdraw
        // to None so the Hub nulls its projection and the UI renders `?`
        // instead of keeping the stale level/flag.
        let _ = cleared;
        let obs = self.mapper.observation(
            Completeness::Structured,
            remuda_protocol::NativeRequestKey::None,
            ObservationPayload::Effort(Box::new(EffortPayload {
                requested,
                request_id: None,
                effective: EffortEffective {
                    name: None,
                    ultracode: None,
                    // The provenance loss is not a level observation: source
                    // unknown, so the UI cannot attribute it to a switch.
                    source: remuda_protocol::EffortSource::Unknown,
                    observed_at: now()?,
                    readback_available: Some(false),
                },
                raw: None,
            })),
        )?;
        Ok(vec![obs])
    }

    /// Whether the effort tracker is accepting current-process records (D-056
    /// (4)). False for a resume tail that has not reached (or has lost) its
    /// current-process boundary. Test-only: production pumps drive provenance
    /// through [`Self::set_effort_current_process`].
    #[cfg(any(test, feature = "test-stub"))]
    pub(crate) fn is_current_process(&self) -> bool {
        self.effort.is_current_process()
    }

    /// Attach the §9.1 model bridge so this mapper drives `/model` read-back
    /// and emits `model` observations. `launch` is the `--model` selection the
    /// process started with; `catalog` is the driver-resolved list the picker
    /// must show, stamped onto the first model observation.
    pub(crate) fn with_model_bridge(
        mut self,
        bridge: Arc<crate::model::ModelBridge>,
        launch: Option<String>,
        catalog: Option<remuda_protocol::ModelCatalogInfo>,
    ) -> Self {
        if launch.is_some() {
            self.model.mark_launch();
            if let Some(id) = &launch {
                bridge.note_launch_request(id.clone());
            }
        }
        if let Some(catalog) = &catalog {
            bridge.set_own_catalog(crate::model_discovery::own_ids(catalog));
        }
        self.model_bridge = Some(bridge);
        self.model_catalog = catalog;
        self
    }

    /// Emit the launch-time model baseline carrying the discovered catalog, so
    /// the picker has the real list before the first prompt. Also preloads the
    /// tracker so the first assistant record with the same id stays deduped.
    pub(crate) fn take_launch_model_snapshot(
        &mut self,
        launch: Option<&str>,
    ) -> DriverResult<Vec<Observation>> {
        let Some(id) = launch.filter(|id| !id.is_empty()) else {
            return Ok(Vec::new());
        };
        if self.model_launch_emitted {
            return Ok(Vec::new());
        }
        self.model_launch_emitted = true;
        // Seed dedup: the same id on the first assistant record is not an edge.
        self.model.observe(Some(id));
        self.model.mark_launch();
        let catalog = self.model_catalog.take();
        let payload = ObservationPayload::Model(Box::new(ModelPayload {
            requested: Some(id.to_owned()),
            effective: EffectiveModel {
                id: id.to_owned(),
                source: EffortSource::Launch,
                observed_at: now()?,
            },
            raw: None,
            catalog,
            selection_path: None,
        }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// Attach the permission bridge so shift+tab switches get transcript
    /// corroboration and `permission` observations are emitted. `launch` names
    /// the mode the process started with, when known.
    pub(crate) fn with_permission_bridge(
        mut self,
        bridge: Arc<crate::permission::PermissionBridge>,
        launch: Option<ClaudePermissionMode>,
    ) -> Self {
        if let Some(mode) = launch {
            self.permission.mark_launch(mode);
            bridge.note_launch_mode(mode);
        }
        self.permission_bridge = Some(bridge);
        self
    }

    /// Map one transcript line. Blank lines and undecodable JSON yield nothing:
    /// a partially written tail is normal while the file is being appended to.
    pub fn map_line(&mut self, line: &str) -> DriverResult<Vec<Observation>> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(Vec::new());
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Ok(Vec::new());
        };
        self.map_record(value)
    }

    /// Map one transcript line with its VERIFIED byte position in the
    /// transcript file (c-effortread r15 item 4, r16 item 1). The production
    /// tail passes the position [`crate::claude_transcript::TailRead`] reports;
    /// an armed effort switch is attributed only to a slash record starting at
    /// or after the switch's pre-submit EOF (the switch's own record starts
    /// exactly AT that file length), so a backlogged older slash (still
    /// unread when the switch was armed) can never claim the newer request.
    pub fn map_line_at(
        &mut self,
        line: &str,
        position: Option<u64>,
    ) -> DriverResult<Vec<Observation>> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(Vec::new());
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Ok(Vec::new());
        };
        self.map_record_at(value, position)
    }

    /// Flush whatever assistant run is still buffered.
    ///
    /// A run is normally closed by the record that supersedes it, so the last
    /// message of a transcript would otherwise sit in the buffer forever. The
    /// tailer calls this when it reaches the end of the available input.
    ///
    /// c-ctxusage adaptation onto the effort-branch group (r23 merge): the
    /// retained group is drained IN PLACE (content + a provisional usage
    /// snapshot) at each poll rather than taken, so a turn spanning several
    /// polls keeps its identity/counters and usage revisions advance by
    /// replay-stable record position. Supersede callers use
    /// [`Self::flush_group`] which finalises the run's usage.
    pub fn flush(&mut self) -> DriverResult<Vec<Observation>> {
        self.drain_group(GroupDrain::Poll)
    }

    /// Explicit end-of-stream drain: finalise the retained run's usage even
    /// without a stop_reason (an interrupted/killed last turn keeps its
    /// counters, c-ctxusage r4 item 2).
    pub fn finish(&mut self) -> DriverResult<Vec<Observation>> {
        self.drain_group(GroupDrain::Supersede)
    }

    /// Drain retained content and (per trigger) pending usage without clearing
    /// the run's identity; the next [`records::Group::start`] resets it.
    /// Emits only conversation + the run's usage (never effort/model extras,
    /// which map_record/group-extras computed once — c-effortread r9 item 5).
    fn drain_group(&mut self, trigger: GroupDrain) -> DriverResult<Vec<Observation>> {
        let mut out = Vec::new();
        if let Some(content) = self.group.drain_content() {
            out.extend(self.emit_conversation(content.record)?);
        }
        let publishable = match trigger {
            GroupDrain::Poll => self.group.usage_publishable_at_poll(),
            GroupDrain::Supersede => self.group.usage_publishable_on_supersede(),
        };
        if publishable
            && let Some(pending) = self.group.take_usage()
            && let Some(obs) = self.usage_from_pending(&pending)?
        {
            out.push(obs);
        }
        Ok(out)
    }

    /// Emit the buffered assistant run's content and finalise its usage when a
    /// superseding record arrives (c-ctxusage group semantics on the
    /// effort-branch pump).
    fn flush_group(&mut self) -> DriverResult<Vec<Observation>> {
        // c-effortread r9 item 5: emit ONLY conversation + the run's retained
        // usage here; effort/model extras are computed exactly once elsewhere.
        self.drain_group(GroupDrain::Supersede)
    }

    /// Build the usage observation for a group's retained snapshot
    /// (c-ctxusage r14/r15): the revision is the record's replay-stable source
    /// position (+1 envelope), qualified by the native session id.
    fn usage_from_pending(
        &mut self,
        pending: &records::PendingUsage,
    ) -> DriverResult<Option<Observation>> {
        let native_at = pending.native_at.as_deref().and_then(native_timestamp);
        let event = match crate::usage::claude::usage_from_message_usage(
            Some(pending.message_id.as_str()),
            pending.request_id.as_deref(),
            pending.model.as_deref(),
            &pending.usage,
        ) {
            Some(event) => event,
            None => return Ok(None),
        };
        let mut totals = crate::usage::UsageTotals::default();
        totals.add(&event);
        let source_session =
            (!self.mapper.session_id.is_empty()).then(|| self.mapper.session_id.clone());
        let mut payload = crate::usage::to_usage_payload(
            UsageScope::Turn,
            pending.message_id.clone(),
            pending.source_position.saturating_add(1),
            source_session.as_deref(),
            &totals,
        );
        payload.usage_id = remuda_protocol::Id::derive(
            "obj",
            self.mapper.instance_id.as_id().as_str(),
            &format!("usage:turn:{}", pending.message_id),
        )?;
        let mut obs = self.mapper.observation(
            Completeness::Structured,
            remuda_protocol::NativeRequestKey::None,
            ObservationPayload::Usage(Box::new(payload)),
        )?;
        if let Some(at) = native_at {
            obs.native_at = Knowledge::Known { value: at };
        }
        Ok(Some(obs))
    }

    /// Hand one already-assembled record to the stdout mapper.
    fn emit_conversation(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message").cloned() else {
            return Ok(Vec::new());
        };
        let mut frame = serde_json::Map::new();
        frame.insert("type".into(), value["type"].clone());
        frame.insert("message".into(), message);
        for key in ["uuid", "parentToolUseId", "sessionId"] {
            if let Some(found) = value.get(key) {
                let wire = if key == "sessionId" {
                    "session_id"
                } else if key == "parentToolUseId" {
                    "parent_tool_use_id"
                } else {
                    key
                };
                frame.insert(wire.into(), found.clone());
            }
        }
        // The launch sidecar (`isAsync` / `async_launched` / `agentId`) is what
        // distinguishes an immediate background-launch tool_result from the
        // subagent's real completion; the wire frame spells it `tool_use_result`.
        if let Some(sidecar) = value.get("toolUseResult") {
            frame.insert("tool_use_result".into(), sidecar.clone());
        }
        // The link from a result back to its call. `parentToolUseId` — what this
        // mapper used to read — does not occur in a transcript at all (0 hits);
        // `sourceToolUseID` is the key Claude actually writes.
        if let Some(source) = value.get("sourceToolUseID").and_then(Value::as_str) {
            frame.insert("parent_tool_use_id".into(), Value::String(source.into()));
        }
        // c-effortread r9 item 5: emit ONLY the conversation records the
        // outbound mapper produces. Effort/model/permission side-effects are
        // computed exactly once elsewhere — in map_record for an immediate
        // record, or at group start (group_extras) for a buffered run — and are
        // never recomputed here against tracker state that a later record may
        // have advanced.
        let mut out = map_outbound(
            &mut self.mapper,
            &Outbound::from_value(Value::Object(frame)),
        )?;
        // Stamp authorship on the messages this record produced. Tool results
        // and tool calls carry their own identity and are left alone.
        if value.get("type").and_then(Value::as_str) == Some("user") {
            let origin = records::classify_user_record(&value, &value["message"]);
            for observation in &mut out {
                if let ObservationPayload::Message(payload) = &mut observation.body
                    && payload.role == MessageRole::User
                {
                    payload.origin = Some(origin);
                }
            }
        }
        Ok(out)
    }

    /// Map one decoded transcript record. Unpositioned callers get the
    /// mapper's running automatic position (c-ctxusage r15 item 8), which
    /// advances exactly once per record.
    pub fn map_record(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        let position = self.auto_position;
        self.auto_position += 1;
        self.map_record_at(value, Some(position))
    }

    /// Map one decoded transcript record with its verified transcript byte
    /// position (see [`Self::map_line_at`]).
    pub fn map_record_at(
        &mut self,
        value: Value,
        position: Option<u64>,
    ) -> DriverResult<Vec<Observation>> {
        // A sidechain record belongs to a sub-agent's own transcript view; the
        // main conversation is what the 结构 tab renders.
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return Ok(Vec::new());
        }
        if let Some(session) = value.get("sessionId").and_then(Value::as_str)
            && !session.is_empty()
        {
            self.mapper.session_id = session.to_owned();
        }
        // D-056: every transcript record names its Claude Code version; the
        // effort gate (coupled vs decoupled ultracode) follows it. Version
        // notes are not gated: a resumed mapper replays into the tracker only
        // to learn the semantics before the first current-process record.
        if let Some(version) = value.get("version").and_then(Value::as_str) {
            self.effort.note_version(version);
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let mut extra = Vec::new();
        if kind == "assistant" {
            // c-effortread r9 item 5: the read-back side-effects are computed
            // ONCE here, in native record order — even while the conversation
            // blocks buffer into the group. emit_conversation/flush_group emit
            // only the conversation records and must never recompute these
            // against tracker state a later record (e.g. a slash) advanced.
            extra = self.map_effort_assistant(&value)?;
            extra.extend(self.map_model_assistant(&value)?);
        } else if kind == "user" {
            extra = self.note_effort_user(&value, position)?;
            extra.extend(self.note_model_user(&value)?);
            extra.extend(self.note_permission_user(&value)?);
        } else if kind == "system" {
            extra = self.note_model_system(&value)?;
        } else if kind == "attachment" {
            // Top-level attachment records carry §9.1 ultra_effort_enter|exit
            // (2.1.272+) in addition to queued_command lifecycle.
            extra = self.note_effort_attachment(&value)?;
        }
        let mut mapped = match kind {
            "assistant" => self.map_assistant_record(value, position.unwrap_or(0)),
            "user" => {
                // Any user record ends the assistant run before it.
                let mut out = self.flush_group()?;
                out.extend(self.map_user_record(value)?);
                Ok(out)
            }
            // §7: the queue ledger and mode drift are not conversation content,
            // but dropping them loses the evidence for what the composer showed.
            "queue-operation" => self.map_queue_operation(&value),
            "permission-mode" | "mode" => self.map_permission_mode(&value),
            "attachment" => self.map_attachment(&value),
            _ => Ok(Vec::new()),
        }?;
        mapped.splice(0..0, extra);
        Ok(mapped)
    }

    /// §9.1: the `/effort` command writes TWO user records — the slash markup
    /// and its `<local-command-stdout>` verdict. The slash record only arms
    /// attribution (on 2.1.272 it lands even for a dismissed dialog); the
    /// verdict settles the level/toggle and resolves or rejects the switch
    /// bridge. Returns effort edge observations when the verdict changes the
    /// effective level or flag.
    fn note_effort_user(
        &mut self,
        value: &Value,
        position: Option<u64>,
    ) -> DriverResult<Vec<Observation>> {
        // D-056 (4): a resumed session replaying pre-launch records must not
        // arm or settle a current switch from a same-words slash/verdict pair.
        if !self.effort.is_current_process() {
            return Ok(Vec::new());
        }
        let Some(message) = value.get("message") else {
            return Ok(Vec::new());
        };
        let text = records::record_text(message);
        if text.contains("<command-name>/effort</command-name>") {
            let Some(args) = remuda_protocol::slash_effort_args(&text) else {
                return Ok(Vec::new());
            };
            // c-effortread r12 item 1: ONE bridge snapshot. The worker lives
            // on another task and can time this request out and arm a newer
            // one between any two lock acquisitions, so matching the slash
            // arguments and attributing the armed tuple must both come from
            // this single (generation, request) pair — otherwise B's tuple
            // gets stored for A's slash and A's delayed verdict settles B.
            // c-effortread r12 item 1: ONE bridge snapshot. The worker lives
            // on another task and can time this request out and arm a newer
            // one between any two lock acquisitions, so matching the slash
            // arguments and attributing the armed tuple must both come from
            // this single (generation, request) pair — otherwise B's tuple
            // gets stored for A's slash and A's delayed verdict settles B.
            let pending_snapshot = self
                .effort_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending_with_gen());
            // Test-only interleaving seam: fire AFTER the snapshot is taken.
            #[cfg(test)]
            if let Some(hook) = self.effort_slash_snapshot_hook.take() {
                hook();
            }
            let from_remuda = pending_snapshot.as_ref().is_some_and(|pending| {
                if pending.request.command_word() != args {
                    return false;
                }
                // c-effortread r15 item 4 / r16 items 1/3: matching command
                // content is not enough — the slash must also have SUBMISSION
                // PROVENANCE. With a verified floor L (the file length captured
                // immediately before submission) only a record starting at or
                // after L can be this switch: the command's OWN record starts
                // exactly at byte L (r16 item 1), while an older same-word
                // slash the pump had not read yet starts BELOW L and is treated
                // as a human/replaced command. When the carrier reports
                // provenance UNAVAILABLE (stat failed / no bound transcript),
                // NOTHING is attributed — the request ends through its own
                // timeout/unconfirmed path rather than by word-only matching
                // (r16 item 3). When either side lacks provenance (direct unit
                // tests via the test-only `arm`) the word match alone governs.
                pending.submit_floor.as_ref().is_none_or(|provenance| {
                    use crate::effort::SubmissionProvenance;
                    match provenance {
                        SubmissionProvenance::Floor(floor) => {
                            position.is_some_and(|record_pos| record_pos >= *floor)
                        }
                        SubmissionProvenance::Unavailable => false,
                    }
                })
            });
            // A slash read off an unverified/displaced batch cannot belong to
            // this process: arm neither the generation nor the level await, so
            // a verdict that follows provenance loss cannot settle the switch.
            let mut owned: Option<(u64, crate::effort::EffortRequest)> = None;
            if from_remuda
                && let Some(pending) = pending_snapshot
                && self.effort_armed.iter().all(|slot| {
                    !matches!(slot, EffortSlashSlot::Owned(armed_gen, _) if *armed_gen == pending.generation)
                })
            {
                let crate::effort::PendingSwitch {
                    generation,
                    request,
                    submit_floor: _,
                } = pending;
                // c-effortread r11 item 1: bind the slash generation to ITS OWN
                // immutable request (id, requested value, submission floor).
                // Two commands can be outstanding (A timed out, B armed)
                // before either verdict is read; the FIFO pairs each verdict
                // with the slash that asked for it, so A's delayed verdict
                // can never carry B's id.
                if let Some(name) = request.awaits_level(self.effort.semantics()) {
                    self.effort.arm_awaiting(name);
                }
                owned = Some((generation, request));
            }
            // c-effortread r16 item 2: note_slash also says whether the args
            // are a SWITCH (false for `status`/`current`/`auto`/invalid words).
            // Every recognised switch slash keeps a verdict-order slot: an
            // owned slot when attributed to the armed request, otherwise an
            // unowned slot (an older pre-floor command or a human /effort) so
            // its delayed verdict consumes the unowned place, never a later
            // owned request. Non-switch slashes get a non-switch slot: their
            // status/other verdicts prune only those and can never cross an
            // outstanding switch slot.
            let is_switch = self.effort.note_slash(&args, from_remuda);
            self.effort_armed.push_back(match owned.take() {
                Some((generation, request)) => EffortSlashSlot::Owned(generation, request),
                None => EffortSlashSlot::Unowned { is_switch },
            });
            return Ok(Vec::new());
        }
        if text.contains("<local-command-stdout>") {
            let stdout = extract_local_stdout(&text);
            let verdict = remuda_protocol::parse_effort_stdout(&stdout, self.effort.semantics());
            return self.settle_effort_stdout(value, stdout, verdict);
        }
        Ok(Vec::new())
    }

    /// Settle one parsed `/effort` verdict: accept/status edges carry the
    /// deterministic per-record event ids (parity with the journal tailer),
    /// and an armed bridge generation resolves or rejects with the stable
    /// reason code. Status output never touches the bridge.
    fn settle_effort_stdout(
        &mut self,
        value: &Value,
        stdout: String,
        verdict: remuda_protocol::EffortStdout,
    ) -> DriverResult<Vec<Observation>> {
        let mut observations = Vec::new();
        match verdict {
            remuda_protocol::EffortStdout::Accepted(parsed) => {
                // c-effortread r11 item 1: this verdict answers the OLDEST
                // outstanding slash (FIFO). When A's verdict is delayed past a
                // newer armed B, the pair is still (gen A, request A): the
                // bridge ignores the resolve for a stale generation and the
                // observation is stamped with A's id/requested value, never
                // B's. c-effortread r16 item 2: the front slot may be UNOWNED
                // (an older pre-floor or human slash whose verdict landed
                // here) — it consumes that place only, and a later owned
                // request keeps waiting for ITS verdict. A verdict with no
                // slot at all is a human/terminal command whose slash
                // predates this mapper.
                let front = self.pop_front_verdict_slot();
                let paired = match &front {
                    VerdictFront::Owned(generation, request) => {
                        Some((*generation, request.clone()))
                    }
                    VerdictFront::Unowned | VerdictFront::Empty => None,
                };
                // c-effortread r17 item 1 / r18 item 1 (coordinator ruling):
                // the verdict answers an UNOWNED place while ANY Remuda switch
                // is still ARMED on the bridge — regardless of whether a later
                // OWNED slot remains in this FIFO. The r17 guard fired only
                // with an owned slot still queued, so two cases fell through
                // and emitted an id-less projection that settled B via the
                // store's value fallback: B's OWN slash stored Unowned after a
                // failed floor stat (the failed-floor case), and an older
                // pre-floor accept read BEFORE B's own slash acquired its owned
                // slot. While the bridge has ANY pending switch, fold the
                // transcript level/flag only: the edge is never labelled
                // Remuda, NO settlement-eligible observation is emitted and
                // the bridge's observed selection is NOT refreshed. B settles
                // only from its own id-bearing verdict, or ends through its
                // own bounded path. (r18 item 3 latches the folded state
                // separately so its edge can be published once the bridge is
                // idle.)
                let armed_switch_pending = self
                    .effort_bridge
                    .as_ref()
                    .is_some_and(|bridge| bridge.has_pending());
                if matches!(front, VerdictFront::Unowned) && armed_switch_pending {
                    // r18 item 3 / r19 item 2 ruling B: the folded state is
                    // NOT published now, but the verdict's own edge is
                    // stashed with its record event id. The stash is ONE
                    // latest slot: EVERY suppressed accepted verdict
                    // overwrites it — including one equal to the published
                    // baseline (a return to baseline) — so it is handed back
                    // even when it changes no published state. It is
                    // published once the armed switch ends (if still current
                    // and different), or dropped by the next authoritative
                    // publication via invalidate_deferred_effort.
                    if let Some(accept) = self.effort.note_unowned_stdout(&stdout) {
                        let event_id = self.effort_event_id(
                            value,
                            accept.observed.name,
                            remuda_protocol::EFFORT_STDOUT_NATIVE,
                        );
                        self.deferred_effort_edge = Some(DeferredEffortEdge {
                            observed: accept.observed,
                            event_id,
                            raw: Some(stdout.clone()),
                        });
                    }
                    return Ok(Vec::new());
                }
                let from_remuda = paired.is_some();
                let edge = self.effort.note_stdout(&stdout, from_remuda);
                // B's OWN verdict (or an idle-bridge human verdict) is the
                // authority and publishes/supersedes anything a suppressed
                // unowned acceptance had latched (r18 item 3 / r19 item 2
                // ruling B): one shared invalidation of both slots.
                self.invalidate_deferred_effort();
                if let Some((generation, _)) = &paired
                    && let Some(bridge) = &self.effort_bridge
                {
                    // A switch to the state already in effect is accepted too:
                    // carry the post-latch observation even without an edge.
                    let settled = edge
                        .map(|(observed, _)| observed)
                        .unwrap_or(self.effort.last_observed().unwrap_or(parsed));
                    bridge.resolve(*generation, settled);
                }
                if let Some((observed, source)) = edge {
                    observations = self.effort_observation(
                        value,
                        observed,
                        source,
                        Some(stdout),
                        remuda_protocol::EFFORT_STDOUT_NATIVE,
                        paired.as_ref(),
                    )?;
                } else if paired.is_some() {
                    // c-effortread r11 item 2: an accepted verdict that changes
                    // NOTHING (the switch targeted the state already in effect)
                    // produces no tracker edge, but it IS request-specific
                    // completion evidence for the armed command that typed it.
                    // Emit the post-latch observation stamped with THAT request so
                    // the web can settle the moved axes from the id; without it a
                    // no-op switch would stay 切换中 until the pending max age.
                    let observed = self.effort.last_observed().unwrap_or(parsed);
                    observations = self.effort_observation(
                        value,
                        observed,
                        EffortSource::Remuda,
                        Some(stdout),
                        remuda_protocol::EFFORT_STDOUT_NATIVE,
                        paired.as_ref(),
                    )?;
                }
            }
            remuda_protocol::EffortStdout::Status(_) => {
                // Observation only: it never resolves/rejects an armed switch.
                // c-effortread r16 item 2: it answers the human's non-switch
                // `/effort status` slash; retire THAT non-switch slot without
                // crossing an outstanding switch slot.
                self.prune_front_non_switch_slot();
                let from_remuda = self
                    .effort_bridge
                    .as_ref()
                    .and_then(|bridge| bridge.pending())
                    .is_some();
                // c-effortread r20 ruling B′: /effort status is an
                // AUTHORITATIVE read-back. The tracker reconciled it through
                // the shared path (latch + clear its deferred slot); drop
                // THIS mapper's stashed edge in lockstep EVEN when the status
                // emits no edge — a same-level status with no ultracode suffix
                // is positive flag-OFF evidence and must not leave a
                // suppressed {max, on} for B's later refusal to publish.
                // Replayed pre-launch status reaches neither side.
                let status_edge = self.effort.note_stdout(&stdout, from_remuda);
                if self.effort.is_current_process() {
                    self.invalidate_deferred_effort();
                }
                if let Some((observed, source)) = status_edge {
                    observations = self.effort_observation(
                        value,
                        observed,
                        source,
                        Some(stdout),
                        remuda_protocol::EFFORT_STATUS_NATIVE,
                        None,
                    )?;
                }
            }
            remuda_protocol::EffortStdout::Kept
            | remuda_protocol::EffortStdout::Invalid
            | remuda_protocol::EffortStdout::WorkflowsDisabled
            | remuda_protocol::EffortStdout::UnavailableForModel
            | remuda_protocol::EffortStdout::EnvOverride => {
                // Clears the awaiting attribution; refuses the armed switch
                // (if any) with the verdict's stable reason code. The refusal
                // pairs with its own slash generation, never a newer armed
                // request's (r11 item 1). r16 item 2: an unowned front slot
                // (pre-floor/human slash) is consumed here without touching a
                // later owned request. r17 item 1 / r18 item 1: when such an
                // unowned refusal lands while ANY switch is still armed on the
                // bridge it folds NOTHING — not even when no owned FIFO slot
                // remains (the failed-floor case, or the verdict read before
                // B's slash acquired its owned slot): the armed request's
                // attribution survives and it ends through its own bounded
                // path, never from this id-less refusal.
                let front = self.pop_front_verdict_slot();
                let paired = match &front {
                    VerdictFront::Owned(generation, request) => {
                        Some((*generation, request.clone()))
                    }
                    VerdictFront::Unowned | VerdictFront::Empty => None,
                };
                let unowned_while_armed = matches!(front, VerdictFront::Unowned)
                    && self
                        .effort_bridge
                        .as_ref()
                        .is_some_and(|bridge| bridge.has_pending());
                if !unowned_while_armed {
                    self.effort.note_stdout(&stdout, paired.is_some());
                }
                if let Some((generation, _)) = paired
                    && let Some(bridge) = &self.effort_bridge
                    && let Some(reason) = verdict.reject_reason()
                {
                    bridge.reject(generation, reason);
                }
            }
            remuda_protocol::EffortStdout::Other => {
                // Unrecognised output (e.g. `/effort auto`): retire the
                // non-switch slash slot that produced it, never a switch one.
                self.prune_front_non_switch_slot();
            }
        }
        // c-effortread r18 item 3 / r19 item 2 ruling B: once NO switch is
        // armed on the bridge (an owned verdict resolved/rejected it above, B
        // ended through its bounded path and this is a later verdict, or no
        // bridge exists), publish the ONE stashed suppressed acceptance under
        // ITS OWN verdict record's event id (live-vs-journal parity). It is
        // published at most once and only when it still differs from the last
        // published state; a return-to-baseline slot emits nothing. While a
        // switch remains armed the stash stays held and the next authoritative
        // read-back invalidates it.
        let bridge_idle = self
            .effort_bridge
            .as_ref()
            .is_none_or(|bridge| !bridge.has_pending());
        if bridge_idle
            && let Some(DeferredEffortEdge {
                observed: stashed_observed,
                event_id,
                raw,
            }) = self.deferred_effort_edge.take()
            && let Some((observed, source)) = self.effort.publish_deferred(stashed_observed)
        {
            let mut emitted = self.effort_observation(
                value,
                observed,
                source,
                raw,
                remuda_protocol::EFFORT_STDOUT_NATIVE,
                None,
            )?;
            if let Some(first) = emitted.first_mut() {
                first.event_id = event_id;
            }
            observations.append(&mut emitted);
        }
        Ok(observations)
    }

    /// c-effortread r19 item 2 ruling B / r20 ruling B′: the ONE shared
    /// invalidation of the suppressed-acceptance slot. An AUTHORITATIVE
    /// effective-state publication — an owned accept, an assistant read-back,
    /// the effort clause of a `/model` verdict, an
    /// `ultra_effort_enter|exit` attachment, or a `/effort status` read-back —
    /// clears BOTH the tracker's deferred state and this mapper's stashed
    /// edge, so a suppressed acceptance can never be published after it was
    /// superseded. The end-of-switch deferred publish also drains through the
    /// tracker side here (after taking the slot for emission).
    fn invalidate_deferred_effort(&mut self) {
        self.effort.clear_deferred();
        self.deferred_effort_edge = None;
    }

    /// c-effortread r16 item 2 / r17 item 1: pop the front verdict-order slot.
    /// A switch-family verdict always consumes the OLDEST outstanding slash
    /// place (FIFO). The owned request is returned only when that place is
    /// owned; an unowned place — an older pre-floor command or a human slash —
    /// yields [`VerdictFront::Unowned`] and an empty FIFO
    /// [`VerdictFront::Empty`], so the verdict takes the human/terminal path
    /// and a later owned request keeps waiting for its own verdict.
    fn pop_front_verdict_slot(&mut self) -> VerdictFront {
        match self.effort_armed.pop_front() {
            Some(EffortSlashSlot::Owned(generation, request)) => {
                VerdictFront::Owned(generation, request)
            }
            Some(EffortSlashSlot::Unowned { .. }) => VerdictFront::Unowned,
            None => VerdictFront::Empty,
        }
    }

    /// c-effortread r16 item 2: remove the foremost outstanding NON-SWITCH
    /// slash slot (a human `status`/`current`/`auto` command answered by a
    /// status/other verdict). Switch slots and owned requests are never
    /// crossed, so such a verdict cannot steal an outstanding switch verdict.
    fn prune_front_non_switch_slot(&mut self) {
        if let Some(index) = self
            .effort_armed
            .iter()
            .position(|slot| matches!(slot, EffortSlashSlot::Unowned { is_switch: false }))
        {
            self.effort_armed.remove(index);
        }
    }

    /// A `/plan` command switches mode natively; arm slash attribution so the
    /// following `permission-mode` edge is attributed to the terminal.
    fn note_permission_user(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message") else {
            return Ok(Vec::new());
        };
        let text = records::record_text(message);
        if text.contains("<command-name>/plan</command-name>") {
            self.permission.note_slash();
        }
        Ok(Vec::new())
    }

    /// §9.1: 2.1.272 rides an `ultra_effort_enter|exit` attachment on the next
    /// prompt; on 2.1.289 a sparse enter is just a re-reminder while the toggle
    /// stays on. It corroborates the verdict's ultracode flag.
    fn note_effort_attachment(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        if let Some(label @ ("ultra_effort_enter" | "ultra_effort_exit")) =
            value.pointer("/attachment/type").and_then(Value::as_str)
        {
            let enters = label == "ultra_effort_enter";
            let edge = self.effort.note_ultra_attachment(enters);
            // r19 item 2 ruling B / r20 ruling B′: the attachment is an
            // authoritative flag publication and clears BOTH deferred copies
            // (the tracker reconciles for the current process — including a
            // sparse repeat that emits no edge — and its edge is decided
            // against the last PUBLISHED tuple, not the latched flag).
            // Replayed pre-launch attachments are ignored by both sides.
            if self.effort.is_current_process() {
                self.invalidate_deferred_effort();
            }
            if let Some((observed, source)) = edge {
                // The attachment type is the native-key label, exactly like
                // the journal tailer, so both channels derive one event id.
                return self.effort_observation(value, observed, source, None, label, None);
            }
        }
        Ok(Vec::new())
    }

    /// Build the §9.1 effort observation for one edge. `native_label` selects
    /// the deterministic event-id key (assistant message id vs
    /// `/effort`-stdout/status/attachment record uuid) so the live mapper and
    /// the journal tailer mint the same id for the same native edge.
    ///
    /// `paired` is the slash request THIS verdict answers (c-effortread r11
    /// item 1): the echo carries THAT request's id and requested selection,
    /// captured at its slash record — not the bridge's latest request. Non-
    /// verdict edges (assistant records, attachments, model verdicts) pass
    /// `None`, so they echo no request at all and settle only through the
    /// read-back fallback.
    fn effort_observation(
        &mut self,
        value: &Value,
        observed: remuda_protocol::ObservedEffort,
        source: remuda_protocol::EffortSource,
        raw: Option<String>,
        native_label: &str,
        paired: Option<&(u64, crate::effort::EffortRequest)>,
    ) -> DriverResult<Vec<Observation>> {
        let (requested, request_id) = paired
            .map(|(_, request)| {
                (
                    Some(remuda_protocol::EffortSelection {
                        name: request.name,
                        ultracode: request.ultracode,
                    }),
                    request.request_id.clone(),
                )
            })
            .unwrap_or((None, None));
        let payload = ObservationPayload::Effort(Box::new(EffortPayload {
            requested,
            request_id,
            effective: EffortEffective {
                name: Some(observed.name),
                ultracode: observed.ultracode,
                source,
                observed_at: now()?,
                readback_available: None,
            },
            raw,
        }));
        let mut obs =
            self.mapper
                .observation(Completeness::Structured, NativeRequestKey::None, payload)?;
        obs.event_id = self.effort_event_id(value, observed.name, native_label);
        // c-effortui r4 item 2: every settled effective edge (verdict or
        // assistant record) refreshes the bridge's OBSERVED selection, so the
        // next live configure classifies toggle-vs-tier-move against
        // read-back — including after a terminal-typed /effort or a resume.
        if let Some(bridge) = &self.effort_bridge {
            bridge.note_observed(observed);
        }
        Ok(vec![obs])
    }

    /// Deterministic event id for one effort edge, matching the journal
    /// tailer's key: the assistant message id for assistant edges, or
    /// `<label>:<record uuid>` for verdict/attachment edges.
    fn effort_event_id(
        &self,
        value: &Value,
        name: remuda_protocol::EffortName,
        native_label: &str,
    ) -> EventId {
        let scope = self.mapper.instance_id.as_id().as_str();
        if native_label.is_empty() {
            let native = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .or_else(|| value.get("uuid").and_then(Value::as_str))
                .unwrap_or("");
            remuda_protocol::effort_event_id(scope, native, name)
        } else {
            let native = value.get("uuid").and_then(Value::as_str).unwrap_or("");
            remuda_protocol::effort_record_event_id(scope, native_label, native, name)
        }
    }

    /// §9.1: read `effort` / `perTurnEffort` off an assistant record and emit
    /// an `effort` observation on edges. Assistant records corroborate the
    /// level (and launch-time effort) but never resolve a live switch — the
    /// command verdict does. Conversation mapping is unaffected.
    fn map_effort_assistant(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let effort = value.get("effort").and_then(Value::as_str);
        let per_turn = value.get("perTurnEffort").and_then(Value::as_str);
        // r19 item 2 ruling B: a CURRENT-process assistant record carrying a
        // parseable level is an authoritative effective-state read-back and
        // clears the suppressed-acceptance slot unconditionally — even a
        // same-value record that produces no edge. Replayed pre-launch
        // records (and a record with no level) reach neither observe_level
        // nor this invalidation.
        let level_read = self.effort.is_current_process()
            && [effort, per_turn]
                .into_iter()
                .flatten()
                .find(|raw| !raw.trim().is_empty())
                .is_some_and(|raw| {
                    remuda_protocol::EffortTracker::parse_level(raw, self.effort.semantics())
                        .is_some()
                });
        let Some((observed, source)) = self.effort.observe(effort, per_turn) else {
            if level_read {
                self.invalidate_deferred_effort();
            }
            return Ok(Vec::new());
        };
        if level_read {
            self.invalidate_deferred_effort();
        }
        let raw = effort
            .filter(|value| !value.is_empty())
            .or(per_turn.filter(|value| !value.is_empty()))
            .map(str::to_owned);
        self.effort_observation(value, observed, source, raw, "", None)
    }

    // ───────── §9.1 model read-back (same verdict pattern as effort) ─────────

    /// Settle the armed model bridge and emit an edge observation for a
    /// verdict- or assistant-observed model id.
    fn model_observation(
        &mut self,
        observed: remuda_protocol::ObservedModel,
        source: EffortSource,
        raw: Option<String>,
    ) -> DriverResult<Vec<Observation>> {
        let (requested, selection_path) = self
            .model_bridge
            .as_ref()
            .map(|bridge| {
                let path = if source == EffortSource::Remuda {
                    bridge.requested_path()
                } else {
                    None
                };
                bridge.note_effective(observed.id.clone(), source, path);
                (bridge.requested().map(|request| request.id), path)
            })
            .unwrap_or((None, None));
        let payload = ObservationPayload::Model(Box::new(ModelPayload {
            requested,
            effective: EffectiveModel {
                id: observed.id,
                source,
                observed_at: now()?,
            },
            raw,
            // The catalog rides the launch snapshot; transcript edges never
            // re-send it.
            catalog: None,
            selection_path,
        }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// Read `message.model` off an assistant record; emit an edge when the
    /// resolved id changes. Assistant records corroborate the verdict but
    /// never resolve a live switch on their own.
    fn map_model_assistant(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let model = value.pointer("/message/model").and_then(Value::as_str);
        let Some((observed, source)) = self.model.observe(model) else {
            return Ok(Vec::new());
        };
        // A post-switch assistant record can only arrive after the verdict; if
        // the verdict was somehow missed, settle the bridge here (resolved id
        // is what the next turn used).
        if let Some(generation) = self.model_generation.take()
            && let Some(bridge) = &self.model_bridge
        {
            bridge.resolve(generation, observed.clone());
        }
        self.model_observation(observed, source, model.map(str::to_owned))
    }

    /// A `/model` accepted on 2.1.272 writes TWO `user` records — slash markup
    /// and its `<local-command-stdout>` verdict. Same arm/settle split as
    /// `/effort`.
    fn note_model_user(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let Some(message) = value.get("message") else {
            return Ok(Vec::new());
        };
        let text = records::record_text(message);
        if text.contains("<command-name>/model</command-name>") {
            let Some(args) = remuda_protocol::slash_model_args(&text) else {
                return Ok(Vec::new());
            };
            let from_remuda = self
                .model_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some_and(|request| request.id == args);
            if from_remuda
                && let Some(bridge) = &self.model_bridge
                && let Some((generation, _request)) = bridge.pending_with_gen()
            {
                self.model_generation = Some(generation);
                self.model.arm_awaiting(&args);
            }
            self.model.note_slash(&args, from_remuda);
            return Ok(Vec::new());
        }
        if text.contains("<local-command-stdout>") {
            return self.settle_model_stdout(value, &extract_local_stdout(&text));
        }
        Ok(Vec::new())
    }

    /// Rejected switches (`Model '<id>' not found`) and a dismissed picker
    /// (`Kept model as <id>`) are `system` records with a top-level `content`
    /// string (`subtype: "local_command"`), not `user` records.
    fn note_model_system(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        if value.get("subtype").and_then(Value::as_str) != Some("local_command") {
            return Ok(Vec::new());
        }
        let Some(text) = value.get("content").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        if text.contains("<command-name>/model</command-name>") {
            let Some(args) = remuda_protocol::slash_model_args(text) else {
                return Ok(Vec::new());
            };
            let from_remuda = self
                .model_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some_and(|request| request.id == args);
            if from_remuda
                && let Some(bridge) = &self.model_bridge
                && let Some((generation, _request)) = bridge.pending_with_gen()
            {
                self.model_generation = Some(generation);
                self.model.arm_awaiting(&args);
            }
            self.model.note_slash(&args, from_remuda);
            return Ok(Vec::new());
        }
        if text.contains("<local-command-stdout>") {
            return self.settle_model_stdout(value, &extract_local_stdout(text));
        }
        Ok(Vec::new())
    }

    /// Shared verdict handling for `user` and `system` stdout records.
    fn settle_model_stdout(
        &mut self,
        value: &Value,
        stdout: &str,
    ) -> DriverResult<Vec<Observation>> {
        let verdict = remuda_protocol::parse_model_stdout(stdout);
        let from_remuda = self.model_generation.is_some()
            || self
                .model_bridge
                .as_ref()
                .and_then(|bridge| bridge.pending())
                .is_some();
        match verdict {
            remuda_protocol::ModelStdout::Accepted { model, effort } => {
                let mut out = Vec::new();
                // D-056 §4: a trailing ` with `<level>` effort` clause is an
                // effort observation, attributed like the model switch. It
                // never settles an effort bridge.
                if let Some(name) = effort {
                    let source = if from_remuda {
                        EffortSource::Remuda
                    } else {
                        EffortSource::Slash
                    };
                    // r19 item 2 ruling B: the /model verdict's effort clause
                    // is an authoritative effective-state publication and
                    // clears the suppressed slot — even a same-value read-back
                    // that emits no edge.
                    let edge = self.effort.observe_level(name, source);
                    self.invalidate_deferred_effort();
                    if let Some((observed, source)) = edge {
                        out.extend(self.effort_observation(
                            value,
                            observed,
                            source,
                            Some(stdout.to_owned()),
                            remuda_protocol::EFFORT_MODEL_NATIVE,
                            None,
                        )?);
                    }
                }
                let edge = self.model.note_stdout(stdout, from_remuda);
                if let Some(generation) = self.model_generation.take()
                    && let Some(bridge) = &self.model_bridge
                {
                    bridge.resolve(generation, model.clone());
                }
                if let Some((observed, source)) = edge {
                    out.extend(self.model_observation(
                        observed,
                        source,
                        Some(stdout.to_owned()),
                    )?);
                }
                return Ok(out);
            }
            remuda_protocol::ModelStdout::Kept | remuda_protocol::ModelStdout::NotFound => {
                self.model.note_stdout(stdout, from_remuda);
                if let Some(generation) = self.model_generation.take()
                    && let Some(bridge) = &self.model_bridge
                {
                    let reason = if verdict == remuda_protocol::ModelStdout::Kept {
                        "dialog-kept"
                    } else {
                        "not-found"
                    };
                    bridge.reject(generation, reason);
                }
            }
            remuda_protocol::ModelStdout::Other => {}
        }
        Ok(Vec::new())
    }

    /// Buffer an assistant record, flushing the previous run when superseded.
    ///
    /// `position` is the record's replay-stable file position (c-ctxusage r14
    /// item 2); a non-positioned caller (`map_line`) passes 0.
    fn map_assistant_record(
        &mut self,
        value: Value,
        position: u64,
    ) -> DriverResult<Vec<Observation>> {
        let message = value.get("message").cloned().unwrap_or(Value::Null);
        let Some(key) = records::GroupKey::of(&value, &message) else {
            // No `message.id` to group on — replay it on its own rather than
            // dropping a real assistant turn.
            let mut out = self.flush_group()?;
            out.extend(self.emit_conversation(value)?);
            return Ok(out);
        };
        if self.group.continues(&key) {
            self.group.push(&value, &message, position);
            return Ok(Vec::new());
        }
        let out = self.flush_group()?;
        self.group.start(key, value.clone(), position);
        self.group.push(&value, &message, position);
        Ok(out)
    }

    /// Map a user record, dropping only exact re-deliveries of one prompt.
    fn map_user_record(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
        let message = value.get("message").cloned().unwrap_or(Value::Null);
        if message.is_null() {
            return Ok(Vec::new());
        }
        // A tail that re-reads the file must not re-emit what it already sent.
        if let Some(uuid) = value.get("uuid").and_then(Value::as_str)
            && !self.seen_uuids.insert(uuid.to_owned())
        {
            return Ok(Vec::new());
        }
        // The same human prompt lands twice when it is enqueued and then
        // delivered. Both records share a `promptId`, so the pair is only a
        // duplicate when the text matches too — two different prompts queued
        // under one id are two real messages.
        let origin = records::classify_user_record(&value, &message);
        if origin == remuda_protocol::MessageOrigin::Human
            && let Some(prompt_id) = value.get("promptId").and_then(Value::as_str)
        {
            let text = records::record_text(&message);
            if !text.trim().is_empty()
                && !self
                    .seen_prompts
                    .insert((prompt_id.to_owned(), text.clone()))
            {
                return Ok(Vec::new());
            }
        }
        self.emit_conversation(value)
    }

    /// `queue-operation` → a queue lifecycle carrying enqueue / dequeue / remove.
    ///
    /// §7: Remuda's own `pty_queue` stays the authority for the queue; this is
    /// the reconciliation evidence beside it, which is why it is journaled as a
    /// lifecycle rather than turned into a message.
    fn map_queue_operation(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let operation = value
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut related = std::collections::BTreeMap::new();
        related.insert("operation".into(), operation.to_owned());
        if let Some(reason) = value.get("reason").and_then(Value::as_str) {
            related.insert("reason".into(), reason.to_owned());
        }
        if let Some(content) = value.get("content").and_then(Value::as_str) {
            related.insert("content".into(), content.to_owned());
        }
        // The journal status the composer reads: an enqueue leaves the text
        // queued, everything else releases it.
        let status = match operation {
            "enqueue" => "queued",
            "dequeue" | "remove" => "dequeued",
            other => other,
        };
        let session_id = self.mapper.session_id.clone();
        Ok(vec![self.mapper.lifecycle_related(
            LifecycleTopic::Turn,
            "queue-operation",
            Knowledge::Known { value: session_id },
            status,
            related,
            false,
        )?])
    }

    /// `permission-mode` / `mode` → the effective-mode edge observation plus
    /// the existing permission lifecycle recording the drift.
    fn map_permission_mode(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let raw = value
            .get("permissionMode")
            .or_else(|| value.get("mode"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let session_id = self.mapper.session_id.clone();
        let lifecycle = self.mapper.lifecycle_related(
            LifecycleTopic::Permission,
            "permission-mode",
            Knowledge::Known { value: session_id },
            raw,
            std::collections::BTreeMap::new(),
            false,
        )?;

        // `mode: "normal"` records the TUI render mode, not a permission mode;
        // only permission vocabulary emits an effective edge.
        let Some(mode) = crate::permission::from_native(raw) else {
            return Ok(vec![lifecycle]);
        };
        let pending = self
            .permission_bridge
            .as_ref()
            .and_then(|bridge| bridge.pending_with_gen());
        let remuda_pending = pending.map(|(_, target)| target);
        let Some((observed, source)) = self.permission.note(raw, remuda_pending) else {
            // Even a deduped record settles a live switch the status line has
            // not been read for (e.g. right after launch).
            if let Some((generation, target)) = pending
                && target == mode
            {
                self.permission_bridge
                    .as_ref()
                    .expect("pending implies a bridge")
                    .resolve(generation, mode);
            }
            return Ok(vec![lifecycle]);
        };
        if let Some((generation, target)) = pending
            && target == observed
        {
            self.permission_bridge
                .as_ref()
                .expect("pending implies a bridge")
                .resolve(generation, observed);
        }
        let mut out = self.permission_observation(observed, source, Some(raw.to_owned()))?;
        out.push(lifecycle);
        Ok(out)
    }

    /// Build the effective permission-mode observation for one edge.
    fn permission_observation(
        &mut self,
        mode: ClaudePermissionMode,
        source: remuda_protocol::PermissionSource,
        raw: Option<String>,
    ) -> DriverResult<Vec<Observation>> {
        let requested = self
            .permission_bridge
            .as_ref()
            .and_then(|bridge| bridge.armed())
            .map(|(_, target)| crate::permission::wire_word(target).to_owned());
        let payload =
            ObservationPayload::Permission(Box::new(remuda_protocol::PermissionPayload {
                requested,
                effective: remuda_protocol::PermissionEffective {
                    mode: crate::permission::wire_word(mode).to_owned(),
                    source,
                    observed_at: now()?,
                },
                raw,
            }));
        Ok(vec![self.mapper.observation(
            Completeness::Structured,
            NativeRequestKey::None,
            payload,
        )?])
    }

    /// `attachment` → only `queued_command` is journaled.
    ///
    /// §7 [V]: `attachment.queued_command` is the delivery evidence for a
    /// queued message and is **not** a `user` record, so a mapper that only
    /// tails user/assistant misses it entirely. It keeps the *enqueue*
    /// timestamp, so it is ordered by file position, never by its own clock.
    /// The other attachment subtypes (`hook_success`, `skill_listing`,
    /// `agent_listing_delta`, …) are Claude's own bookkeeping.
    fn map_attachment(&mut self, value: &Value) -> DriverResult<Vec<Observation>> {
        let attachment = value.get("attachment").unwrap_or(&Value::Null);
        if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
            return Ok(Vec::new());
        }
        let mut related = std::collections::BTreeMap::new();
        related.insert("operation".into(), "delivered".into());
        for (key, field) in [("content", "command"), ("content", "content")] {
            if let Some(text) = attachment.get(field).and_then(Value::as_str) {
                related.insert(key.into(), text.to_owned());
                break;
            }
        }
        let session_id = self.mapper.session_id.clone();
        Ok(vec![self.mapper.lifecycle_related(
            LifecycleTopic::Turn,
            "queued-command",
            Knowledge::Known { value: session_id },
            "dequeued",
            related,
            false,
        )?])
    }
}

/// Mapping / gating helpers used by `tests/claude_print_review.rs`.
#[doc(hidden)]
pub mod review {
    use super::*;

    /// A stdout mapper that keeps its state across frames.
    ///
    /// [`map_stdout_json`] builds a fresh mapper per call, so native ids never
    /// correlate — a `tool_result` in a later frame cannot find the
    /// `tool_use` that preceded it. That is fine for checking one frame's
    /// shape, but a multi-frame scenario (the D-028 §12 parity gate) needs the
    /// same mapper throughout, exactly as the live reader has.
    pub struct StdoutMapper {
        mapper: Mapper,
    }

    impl Default for StdoutMapper {
        fn default() -> Self {
            Self::new()
        }
    }

    impl StdoutMapper {
        /// A mapper stamping `claude-print` / `stdout` observations.
        #[must_use]
        pub fn new() -> Self {
            Self {
                mapper: Mapper {
                    stream: stream::StreamState::default(),
                    ids: NativeIds::default(),
                    seq: 0,
                    instance_id: InstanceId::new(),
                    run_id: RunId::new(),
                    journal_id: fallback_obj(),
                    host_id: fallback_host(),
                    session_id: "review-session".into(),
                    pin: BinaryPin {
                        abs_path: String::new(),
                        version: "review".into(),
                        sha256: dummy_digest(),
                    },
                    driver_kind: DriverKind::ClaudePrint,
                    channel: SourceChannel::Stdout,
                    media_stager: None,
                    turns: TurnBook::default(),
                    saw_per_call_usage: false,
                    deferred_result: None,
                },
            }
        }

        /// Map one stdout frame, carrying identity forward.
        pub fn map(&mut self, value: Value) -> DriverResult<Vec<Observation>> {
            map_outbound(&mut self.mapper, &Outbound::from_value(value))
        }

        /// Like [`Self::new`] but stamping a specific carrier (e.g.
        /// `DriverKind::ClaudeSdk`, which drives this same engine without
        /// `-p`). Session id is fixed for review determinism.
        #[must_use]
        pub fn with_driver(driver: DriverKind) -> Self {
            let mut mapper = Self::new();
            mapper.mapper.driver_kind = driver;
            mapper
        }
    }

    /// Map one stdout JSON object the same way the live reader does.
    pub fn map_stdout_json(value: Value) -> DriverResult<Vec<Observation>> {
        let frame = Outbound::from_value(value);
        let mut mapper = Mapper {
            stream: stream::StreamState::default(),
            ids: NativeIds::default(),
            seq: 0,
            instance_id: InstanceId::new(),
            run_id: RunId::new(),
            journal_id: fallback_obj(),
            host_id: fallback_host(),
            session_id: "review-session".into(),
            pin: BinaryPin {
                abs_path: String::new(),
                version: "review".into(),
                sha256: dummy_digest(),
            },
            driver_kind: DriverKind::ClaudePrint,
            channel: SourceChannel::Stdout,
            media_stager: None,
            turns: TurnBook::default(),
            saw_per_call_usage: false,
            deferred_result: None,
        };
        map_outbound(&mut mapper, &frame)
    }

    /// Serialize a host `control_response` for a permission answer.
    pub fn permission_control_json(
        request_id: &str,
        input: &Value,
        answer: &InteractionAnswer,
    ) -> DriverResult<Value> {
        let payload = permission_from_answer(input, answer)?;
        let inbound = Inbound::control_success(request_id, payload);
        Ok(serde_json::to_value(&inbound)?)
    }

    /// Driver-level bot/dontAsk/bypass gate.
    pub fn reject_bot(spec: &InstanceSpec, origin: InputOrigin) -> DriverResult<()> {
        reject_bot_bypass(spec, origin)
    }

    /// Final argv check for `--bare` / `--no-session-persistence`.
    pub fn refuse_argv(argv: &[String]) -> DriverResult<()> {
        refuse_prohibited_argv(argv)
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use remuda_protocol::{Knowledge, MediaBlock, ResourceBlock, TextBlock};

    /// 1x1 red PNG. Small, real, and enough to prove the bytes round-trip.
    const RED_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn image_blocks(path: &std::path::Path, text: &str) -> Vec<ContentBlock> {
        let id = remuda_protocol::Id::new("obj").expect("id");
        vec![
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: "image/png".into(),
                name: Some("shot.png".into()),
                anchor: None,
                size: None,
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: format!("file://{}", path.display()),
                media_type: Knowledge::Known {
                    value: "image/png".into(),
                },
                object_id: Some(id),
            })),
            ContentBlock::Text(Box::new(TextBlock { text: text.into() })),
        ]
    }

    /// A text-only prompt keeps the plain-string shape the CLI has always got.
    #[test]
    fn a_prompt_without_attachments_is_still_sent_as_text() {
        let blocks = vec![ContentBlock::Text(Box::new(TextBlock {
            text: "hello".into(),
        }))];
        match prompt_content(&blocks).expect("content") {
            UserContent::Text(text) => assert_eq!(text, "hello"),
            UserContent::Blocks(blocks) => panic!("expected plain text, got {blocks:?}"),
        }
    }

    /// D-027: an attachment becomes a base64 image block ahead of the text.
    /// Base64 specifically — the design verified that a `file` source or a
    /// bare path is silently downgraded to text by the CLI.
    #[test]
    fn an_attachment_becomes_a_base64_image_block_before_the_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shot.png");
        std::fs::write(&path, RED_PNG).expect("write png");

        let content =
            prompt_content(&image_blocks(&path, "what colour is the image?")).expect("content");
        let UserContent::Blocks(blocks) = content else {
            panic!("expected content blocks");
        };
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], serde_json::json!("image"));
        assert_eq!(blocks[0]["source"]["type"], serde_json::json!("base64"));
        assert_eq!(
            blocks[0]["source"]["media_type"],
            serde_json::json!("image/png")
        );
        let encoded = blocks[0]["source"]["data"].as_str().expect("data");
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("decode"),
            RED_PNG,
            "the CLI must receive the exact bytes that were staged"
        );
        assert_eq!(blocks[1]["type"], serde_json::json!("text"));
        assert_eq!(
            blocks[1]["text"],
            serde_json::json!("what colour is the image?")
        );
    }

    /// Past the inline budget the driver mentions paths instead, so the image
    /// is still reachable through the Read tool rather than simply dropped.
    #[test]
    fn oversized_attachments_fall_back_to_a_path_mention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.png");
        let mut big = RED_PNG.to_vec();
        big.resize(MAX_INLINE_IMAGE_BYTES + 1, 0);
        std::fs::write(&path, &big).expect("write png");

        match prompt_content(&image_blocks(&path, "describe it")).expect("content") {
            UserContent::Text(text) => {
                assert!(text.contains("describe it"), "{text}");
                assert!(text.contains(&path.display().to_string()), "{text}");
            }
            UserContent::Blocks(blocks) => panic!("expected a path mention, got {blocks:?}"),
        }
    }

    /// A staged file that vanished is an error, not a silently text-only turn.
    #[test]
    fn a_missing_attachment_file_fails_the_send() {
        let blocks = image_blocks(std::path::Path::new("/nonexistent/shot.png"), "hi");
        assert!(prompt_content(&blocks).is_err());
    }
}

#[cfg(test)]
mod plan_review_mint_tests {
    use super::*;
    use remuda_protocol::{
        ApprovalAnswer, Digest, InteractionAnswer as Answer, InteractionRequest, PlanReviewAnswer,
        U64,
    };
    use serde_json::json;

    const PLAN: &str = "# Plan\n\n1. Run the shell command `echo plan-approved`.\n";

    fn can_use_tool_request(tool_use_id: &str, plan: &str, tool_name: &str) -> CanUseToolRequest {
        CanUseToolRequest {
            tool_name: tool_name.into(),
            input: json!({ "plan": plan, "planFilePath": "/work/plan.md" }),
            permission_suggestions: None,
            blocked_path: None,
            blocked_paths: None,
            tool_use_id: Some(tool_use_id.into()),
            title: None,
            display_name: Some("ExitPlanMode".into()),
            description: None,
            requires_user_interaction: Some(true),
            decision_reason_type: None,
            extra: serde_json::Map::new(),
        }
    }

    fn envelope() -> ControlRequestEnvelope {
        ControlRequestEnvelope {
            request_id: "perm-exit-plan".into(),
            request: ControlRequest::Unknown(serde_json::json!({
                "subtype": "can_use_tool"
            })),
        }
    }

    /// Feed the recorded real-Claude assistant frame through the mapper so the
    /// top-level ExitPlanMode set is populated, then return the native
    /// tool_use id from that frame.
    fn feed_real_assistant_frame(mapper: &mut StdoutMapper) -> String {
        let raw =
            include_str!("../../remuda-testing/fixtures/claude/claude-exit-plan-mode-allow.jsonl");
        let frame: Value = raw
            .lines()
            .map(serde_json::from_str::<Value>)
            .filter_map(Result::ok)
            .find(|frame| {
                frame.get("type").and_then(Value::as_str) == Some("assistant")
                    && frame
                        .pointer("/message/content")
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block.get("type").and_then(Value::as_str) == Some("tool_use")
                                    && block.get("name").and_then(Value::as_str)
                                        == Some("ExitPlanMode")
                            })
                        })
            })
            .expect("recorded ExitPlanMode assistant frame");
        let native_id = frame
            .pointer("/message/content")
            .and_then(Value::as_array)
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b.get("name").and_then(Value::as_str) == Some("ExitPlanMode"))
            })
            .and_then(|b| b.get("id"))
            .and_then(Value::as_str)
            .expect("native tool id")
            .to_string();
        let obs = mapper.map(frame).expect("map assistant");
        assert!(
            obs.iter().any(|o| matches!(
                o.body,
                ObservationPayload::ToolCall(ref c)
                    if matches!(&c.tool_name, Knowledge::Known { value } if value == "ExitPlanMode")
            )),
            "the frame maps an ExitPlanMode tool_call"
        );
        native_id
    }

    #[test]
    fn a_top_level_exit_plan_mode_with_inline_plan_mints_a_plan_review() {
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        let native_id = feed_real_assistant_frame(&mut mapper);
        let req = can_use_tool_request(&native_id, PLAN, "ExitPlanMode");
        let interaction =
            build_interaction(&mut mapper.mapper, &envelope(), &req, InteractionId::new())
                .expect("build");
        assert_eq!(interaction.kind, InteractionKind::PlanReview);
        let InteractionRequest::PlanReview(review) = &interaction.request else {
            panic!("expected plan review, got {:?}", interaction.request);
        };
        assert_eq!(review.plan.as_deref(), Some(PLAN));
        assert_eq!(review.plan_revision, U64(1));
        assert!(review.allow_feedback);
        let ids: Vec<_> = review.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["approve", "deny"]);
        // Digest is sha256 of the exact plan bytes.
        let expected = crate::binary::hash_bytes(PLAN.as_bytes()).expect("digest");
        assert_eq!(review.plan_digest, expected);
    }

    #[test]
    fn a_nested_sub_agent_exit_plan_mode_falls_back_to_approval() {
        // A tool_use nested under another tool call is not in the top-level
        // set even though the tool name matches.
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        let nested_frame = json!({
            "type": "assistant",
            "message": {
                "id": "msg_nested",
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_nested",
                    "name": "ExitPlanMode",
                    "input": {"plan": PLAN}
                }]
            },
            "parent_tool_use_id": "toolu_parent"
        });
        mapper.map(nested_frame).expect("map");
        let req = can_use_tool_request("toolu_nested", PLAN, "ExitPlanMode");
        let interaction =
            build_interaction(&mut mapper.mapper, &envelope(), &req, InteractionId::new())
                .expect("build");
        assert_eq!(
            interaction.kind,
            InteractionKind::Approval,
            "nested ExitPlanMode stays human-only"
        );
    }

    #[test]
    fn a_plan_over_32kib_falls_back_to_approval() {
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        let native_id = feed_real_assistant_frame(&mut mapper);
        let big = format!("# {}\n", "z".repeat(32 * 1024));
        let req = can_use_tool_request(&native_id, &big, "ExitPlanMode");
        let interaction =
            build_interaction(&mut mapper.mapper, &envelope(), &req, InteractionId::new())
                .expect("build");
        assert_eq!(interaction.kind, InteractionKind::Approval);
    }

    #[test]
    fn an_unknown_tool_never_mints_a_plan_review() {
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        feed_real_assistant_frame(&mut mapper);
        let req = can_use_tool_request("toolu_bash", "echo hi", "Bash");
        let interaction =
            build_interaction(&mut mapper.mapper, &envelope(), &req, InteractionId::new())
                .expect("build");
        assert_eq!(interaction.kind, InteractionKind::Approval);
    }

    #[test]
    fn approve_carries_no_permission_escalation() {
        let input = json!({"plan": PLAN});
        let digest = Digest::try_from(format!("sha256:{}", "a".repeat(64))).unwrap();
        let answer = Answer::PlanReview(Box::new(PlanReviewAnswer {
            option_id: "approve".into(),
            plan_revision: U64(1),
            plan_digest: digest,
            feedback: None,
        }));
        match permission_from_answer(&input, &answer).expect("payload") {
            ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input,
                updated_permissions,
            }) => {
                assert_eq!(updated_input, input);
                assert!(updated_permissions.is_none(), "approve never escalates");
            }
            other => panic!("expected allow, got {other:?}"),
        }
    }

    #[test]
    fn deny_uses_the_reviewer_feedback_or_a_default_message() {
        let input = json!({"plan": PLAN});
        let digest = Digest::try_from(format!("sha256:{}", "a".repeat(64))).unwrap();
        let mk = |feedback: Option<String>| {
            Answer::PlanReview(Box::new(PlanReviewAnswer {
                option_id: "deny".into(),
                plan_revision: U64(1),
                plan_digest: digest.clone(),
                feedback,
            }))
        };
        let with_feedback =
            permission_from_answer(&input, &mk(Some("do less".into()))).expect("payload");
        assert!(matches!(
            with_feedback,
            ControlSuccessPayload::Permission(PermissionResult::Deny { ref message, .. })
                if message == "do less"
        ));
        let default = permission_from_answer(&input, &mk(None)).expect("payload");
        assert!(matches!(
            default,
            ControlSuccessPayload::Permission(PermissionResult::Deny { ref message, .. })
                if message == "plan review denied"
        ));
    }

    fn plan_review_options() -> Vec<DecisionOption> {
        vec![
            DecisionOption {
                id: "approve".into(),
                label: "Approve".into(),
                effect: DecisionEffect::AllowOnce,
                native_value_ref: Id::new("obj").unwrap(),
            },
            DecisionOption {
                id: "deny".into(),
                label: "Deny".into(),
                effect: DecisionEffect::Deny,
                native_value_ref: Id::new("obj").unwrap(),
            },
        ]
    }

    #[test]
    fn validate_rejects_bad_plan_answers() {
        let mk_review = |revision: u64, allow_feedback: bool| PlanReviewRequest {
            title: "Plan review".into(),
            plan_ref: Id::new("obj").unwrap(),
            plan_revision: U64(revision),
            plan_digest: crate::binary::hash_bytes(PLAN.as_bytes()).unwrap(),
            options: plan_review_options(),
            allow_feedback,
            plan: Some(PLAN.into()),
        };
        let request = InteractionRequest::PlanReview(Box::new(mk_review(1, true)));
        let digest = crate::binary::hash_bytes(PLAN.as_bytes()).unwrap();
        let mk = |option: &str, revision: u64, digest: Digest, feedback: Option<String>| {
            Answer::PlanReview(Box::new(PlanReviewAnswer {
                option_id: option.into(),
                plan_revision: U64(revision),
                plan_digest: digest,
                feedback,
            }))
        };
        // Option the request never offered.
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk(
                    "allow-once",
                    1,
                    crate::binary::hash_bytes(PLAN.as_bytes()).unwrap(),
                    None
                )
            )
            .is_err()
        );
        // Revision must echo the REQUEST's revision (2 here), not a hardcoded 1.
        let rev2 = InteractionRequest::PlanReview(Box::new(mk_review(2, true)));
        assert!(
            remuda_driver::interaction::validate_answer(
                &rev2,
                &mk("approve", 1, digest.clone(), None)
            )
            .is_err(),
            "revision 1 answer cannot satisfy a revision-2 request"
        );
        assert!(
            remuda_driver::interaction::validate_answer(
                &rev2,
                &mk("approve", 2, digest.clone(), None)
            )
            .is_ok(),
            "the matching request revision is accepted"
        );
        // Wrong digest.
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk(
                    "approve",
                    1,
                    Digest::try_from(format!("sha256:{}", "b".repeat(64))).unwrap(),
                    None
                )
            )
            .is_err()
        );
        // Approve must not carry feedback (it would be silently dropped).
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk("approve", 1, digest.clone(), Some("note".into()))
            )
            .is_err()
        );
        // Oversized feedback on deny.
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk("deny", 1, digest.clone(), Some("x".repeat(4097)))
            )
            .is_err()
        );
        // When the request disallows feedback, even deny feedback is refused.
        let no_feedback = InteractionRequest::PlanReview(Box::new(mk_review(1, false)));
        assert!(
            remuda_driver::interaction::validate_answer(
                &no_feedback,
                &mk("deny", 1, digest.clone(), Some("note".into()))
            )
            .is_err()
        );
        // But a bare deny with no feedback is still accepted there.
        assert!(
            remuda_driver::interaction::validate_answer(
                &no_feedback,
                &mk("deny", 1, digest.clone(), None)
            )
            .is_ok()
        );
        // Valid approve and deny (the offered options) with/without feedback.
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk("approve", 1, digest.clone(), None)
            )
            .is_ok()
        );
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk("deny", 1, digest, Some("do less".into()))
            )
            .is_ok()
        );
        // Whitespace-only feedback is not rejected here (length-based): it is
        // forwarded verbatim by permission_from_answer; the reviewer typed it.
        assert!(
            remuda_driver::interaction::validate_answer(
                &request,
                &mk(
                    "deny",
                    1,
                    crate::binary::hash_bytes(PLAN.as_bytes()).unwrap(),
                    Some("   ".into())
                )
            )
            .is_ok()
        );
    }

    #[test]
    fn deny_feedback_is_forwarded_verbatim_including_whitespace() {
        let input = json!({"plan": PLAN});
        let digest = Digest::try_from(format!("sha256:{}", "a".repeat(64))).unwrap();
        let mk = |feedback: Option<String>| {
            Answer::PlanReview(Box::new(PlanReviewAnswer {
                option_id: "deny".into(),
                plan_revision: U64(1),
                plan_digest: digest.clone(),
                feedback,
            }))
        };
        // Absent (null) note -> default message.
        assert!(matches!(
            permission_from_answer(&input, &mk(None)).expect("payload"),
            ControlSuccessPayload::Permission(PermissionResult::Deny { ref message, .. })
                if message == "plan review denied"
        ));
        // Present but whitespace-only -> forwarded verbatim (NOT default).
        assert!(matches!(
            permission_from_answer(&input, &mk(Some("  ".into()))).expect("payload"),
            ControlSuccessPayload::Permission(PermissionResult::Deny { ref message, .. })
                if message == "  "
        ));
        // Ordinary text verbatim.
        assert!(matches!(
            permission_from_answer(&input, &mk(Some("do less".into()))).expect("payload"),
            ControlSuccessPayload::Permission(PermissionResult::Deny { ref message, .. })
                if message == "do less"
        ));
    }

    #[test]
    fn an_empty_string_plan_still_mints_a_plan_review() {
        // The approved predicate is "plan is a string <= 32 KiB"; an empty
        // string qualifies — the reviewer gates the tool call itself and the
        // UI falls back to the open-session hint.
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "sess");
        let native_id = feed_real_assistant_frame(&mut mapper);
        let mut req = can_use_tool_request(&native_id, PLAN, "ExitPlanMode");
        req.input = json!({ "plan": "" });
        let interaction =
            build_interaction(&mut mapper.mapper, &envelope(), &req, InteractionId::new())
                .expect("build");
        assert_eq!(interaction.kind, InteractionKind::PlanReview);
        let InteractionRequest::PlanReview(review) = &interaction.request else {
            panic!("expected plan review");
        };
        assert_eq!(review.plan.as_deref(), Some(""));
        // Digest of the empty string is still well-defined.
        assert_eq!(review.plan_digest, crate::binary::hash_bytes(b"").unwrap());
    }

    #[test]
    fn a_plan_review_answer_does_not_match_an_approval_request() {
        // Cross-kind answers must fail, so a plan answer cannot settle an
        // approval or vice versa.
        let approval = InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: "Bash".into(),
            description: String::new(),
            tool_call_id: None,
            action_ref: Id::new("obj").unwrap(),
            options: Vec::new(),
            requested_permissions_ref: None,
            input_digest: Digest::try_from(format!("sha256:{}", "a".repeat(64))).unwrap(),
        }));
        let plan = Answer::PlanReview(Box::new(PlanReviewAnswer {
            option_id: "approve".into(),
            plan_revision: U64(1),
            plan_digest: crate::binary::hash_bytes(PLAN.as_bytes()).unwrap(),
            feedback: None,
        }));
        assert!(
            remuda_driver::interaction::validate_answer(&approval, &plan).is_err(),
            "a plan answer cannot satisfy an approval"
        );
        let approval_answer = Answer::Approval(Box::new(ApprovalAnswer {
            option_id: "allow".into(),
            input_digest: Digest::try_from(format!("sha256:{}", "a".repeat(64))).unwrap(),
        }));
        let review = InteractionRequest::PlanReview(Box::new(PlanReviewRequest {
            title: "Plan review".into(),
            plan_ref: Id::new("obj").unwrap(),
            plan_revision: U64(1),
            plan_digest: crate::binary::hash_bytes(PLAN.as_bytes()).unwrap(),
            options: Vec::new(),
            allow_feedback: true,
            plan: Some(PLAN.into()),
        }));
        assert!(remuda_driver::interaction::validate_answer(&review, &approval_answer).is_err());
    }
}

/// c-cardsettle r6 item 1: the PRODUCTION mapper's startup frames must never
/// classify as a process end. The real captured `system/init` frame maps to
/// `topic=session, nativeName=session, status=started, severity=info` — the
/// same name the later `emit_exit("exited")` uses — so the classifier must
/// distinguish them by the exact status rather than the name + a severity
/// fallback (which ended every live session at startup).
#[cfg(test)]
mod process_end_startup_tests {
    use super::*;
    use remuda_protocol::process_end::{
        ProcessEndKind, entity_process_end, process_end, process_end_event,
        process_end_observation, process_end_value,
    };
    use serde_json::json;

    #[test]
    fn the_real_init_frame_does_not_end_the_session() {
        let raw = include_str!("../../remuda-testing/fixtures/claude/claude-p-init.json");
        let init: Value = serde_json::from_str(raw).expect("real captured system/init");
        let mut mapper = StdoutMapper::new(DriverKind::ClaudePrint, "before-session-id");
        let observations = mapper.map(init).expect("real init maps");

        // The mapper adopted the init's session id and emitted exactly one
        // session lifecycle carrying status "started".
        let init_native = observations
            .iter()
            .find_map(|observation| match &observation.body {
                ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                    LifecyclePayload::Native(native)
                        if native.topic == LifecycleTopic::Session
                            && native.native_name == "session" =>
                    {
                        Some(native)
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("map_init emits the session lifecycle");
        assert!(
            matches!(&init_native.status, Knowledge::Known { value } if value == "started"),
            "the real init status is {init_native:?}"
        );
        assert_eq!(init_native.severity, Severity::Info);

        // Every classifier entry point refuses to end on it.
        assert_eq!(process_end(init_native), None);
        assert_eq!(process_end_value(&json!(init_native)), None);
        let init_observation = observations
            .iter()
            .find(|observation| {
                matches!(
                    &observation.body,
                    ObservationPayload::Lifecycle(payload)
                        if matches!(payload.as_ref(), LifecyclePayload::Native(native)
                            if native.topic == LifecycleTopic::Session)
                )
            })
            .expect("observation");
        assert_eq!(process_end_observation(init_observation), None);

        // The PTY ready frames (claude_pty.rs / generic_pty.rs emit
        // name=session with the herdr agent-status string) are live frames too.
        for status in ["idle", "working", "blocked", "done", "unknown"] {
            let ready = NativeLifecycle {
                topic: LifecycleTopic::Session,
                native_name: "session".to_owned(),
                native_id: Knowledge::Known {
                    value: "pane-1".into(),
                },
                status: Knowledge::Known {
                    value: status.into(),
                },
                related_ids: std::collections::BTreeMap::new(),
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            };
            assert_eq!(process_end(&ready), None, "{status} ready is not an end");
        }
        // Entity ready/unknown are equally not process end.
        assert_eq!(entity_process_end(Some("instance"), Some("ready")), None);
        assert_eq!(entity_process_end(Some("instance"), None), None);

        // Sanity: the REAL later ends still classify through the same path —
        // print emit_exit("exited") and a non-zero shell native_exit.
        let print_exit = NativeLifecycle {
            topic: LifecycleTopic::Session,
            native_name: "session".to_owned(),
            native_id: Knowledge::Known {
                value: "sess".into(),
            },
            status: Knowledge::Known {
                value: "exited".into(),
            },
            related_ids: std::collections::BTreeMap::new(),
            data_ref: None,
            severity: Severity::Info,
            affects_completion: false,
        };
        assert_eq!(
            process_end(&print_exit).map(|end| end.kind),
            Some(ProcessEndKind::Exited)
        );
        let shell_failed = json!({
            "kind": "lifecycle",
            "observedAt": "2026-10-07T10:00:00.000Z",
            "payload": {
                "type": "native",
                "topic": "session",
                "nativeName": "native_exit",
                "severity": "error",
                "status": {"state": "known", "value": "failed"},
                "relatedIds": {"exitCode": "1"}
            }
        });
        assert_eq!(
            process_end_event(&shell_failed).map(|end| end.kind),
            Some(ProcessEndKind::Failed)
        );
    }
}

#[cfg(test)]
mod turn_reservation_wakeup_tests {
    use super::*;
    use std::time::Duration;

    /// The ONE named bound for every await in the ticket-wakeup tests
    /// (ma-sdk-state r16 item 3): no inline `tokio::time::timeout` at the
    /// wait/join sites.
    async fn bounded_ticket_wait<F: std::future::Future>(future: F, what: &str) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .unwrap_or_else(|_| panic!("bounded test wait timed out: {what}"))
    }

    /// ma-sdk-state r5 item 3: cancelling BEFORE a waiter registers its
    /// `committed.notified()` must not lose the wakeup. `wait_for_commit`
    /// observes the resolved flag and returns false promptly (pre-fix it parked
    /// on the Notify forever, wedging the publication worker until close).
    #[tokio::test]
    async fn cancel_before_wait_resolves_without_a_start_and_without_hanging() {
        let reservation = TurnReservation::shared(0, "msg-cancel-before-wait".to_string());
        // Cancel first; only afterwards does the (worker) side await.
        reservation.cancel();
        let committed =
            bounded_ticket_wait(reservation.wait_for_commit(), "a cancel racing ahead").await;
        assert!(!committed, "a cancel resolves the ticket as not-committed");
    }

    /// A normal commit resolves a waiting ticket as committed. The commit may
    /// land before the waiter registers its `notified()`: `wait_for_commit`
    /// reads the resolved flag first and resolves without a sleep-driven race
    /// (the parked-notify ordering is exercised by every scheduling test).
    #[tokio::test]
    async fn commit_resolves_a_waiting_ticket_as_committed() {
        let reservation = TurnReservation::shared(0, "msg-commit".to_string());
        let ticket = reservation.clone();
        let waiter = tokio::spawn(async move { ticket.wait_for_commit().await });
        reservation.commit();
        let committed = bounded_ticket_wait(waiter, "the waiter task")
            .await
            .expect("waiter resolves");
        assert!(committed, "a commit resolves the ticket as committed");
        assert!(
            reservation.claim_start(),
            "a committed ticket may claim the start"
        );
    }
}

#[cfg(test)]
mod exit_permit_on_build_error_tests {
    use tokio::sync::mpsc;

    /// ma-sdk-state r5 item 5: `emit_exit` takes the reserved terminal
    /// `OwnedPermit` out of `Inner.exit_permit` BEFORE the fallible observation
    /// build. If that build returns `Err`, the permit is a local that drops
    /// while unwinding, releasing the bounded-channel slot and letting the
    /// RunHandle reach EOF after close. Leaving the permit parked on that error
    /// keeps the slot occupied forever. The observation build only fails on
    /// timestamp generation, so this pins the channel-ownership invariant the
    /// fix depends on.
    #[tokio::test]
    async fn the_terminal_permit_is_released_when_the_exit_build_errors() {
        let (tx, mut rx) = mpsc::channel::<u8>(1);

        // The single reserved slot, exactly as Inner.exit_permit parks it.
        let mut parked: Option<mpsc::OwnedPermit<u8>> = Some(
            tx.clone()
                .try_reserve_owned()
                .expect("the terminal slot is reservable"),
        );
        assert!(
            tx.try_reserve().is_err(),
            "the parked terminal permit occupies the only slot"
        );

        // emit_exit's fallible build: on Err the local permit is dropped (the
        // r5 fix), rather than left parked in Inner.exit_permit.
        let build: Result<(), ()> = Err(());
        if build.is_err() {
            drop(parked.take());
        }
        assert!(parked.is_none());

        // The slot is free again and the receiver still reaches clean EOF once
        // the sender goes away (the RunHandle does not wedge after close).
        assert!(
            tx.try_reserve().is_ok(),
            "the terminal slot must be released on a build error"
        );
        drop(tx);
        assert!(rx.recv().await.is_none(), "the stream reaches EOF");
    }
}

#[cfg(test)]
mod turn_book_reserved_seq_tests {
    use super::*;

    /// ma-sdk-state r5 item 1 follow-up: the worker clears `reserved` when it
    /// processes a ticket, but a NEWER turn may already have begun by then
    /// (send N+1's begin races ahead of the worker draining N's ticket).
    /// Committing the OLDER seq must not clobber the newer in-flight
    /// reservation, or a later failed write could not roll its own book back.
    #[test]
    fn committing_an_older_ticket_does_not_clear_a_newer_reservation() {
        let mut book = TurnBook::default();
        let a = book.begin_turn();
        let b = book.begin_turn();
        assert_eq!(book.reserved, Some(b));

        // N's ticket is processed only after N+1 began: conditional clear.
        book.turn_committed(a);
        assert_eq!(
            book.reserved,
            Some(b),
            "the older commit must not clobber turn B's reservation"
        );

        // A failed write for B must still be able to roll B back (back turn is
        // the reserved one).
        book.cancel_turn(b);
        // B removed; A still outstanding, reserved cleared.
        assert_eq!(book.reserved, None);
        assert!(book.outstanding.iter().any(|turn| turn.seq == a));
        assert!(!book.outstanding.iter().any(|turn| turn.seq == b));

        // Committing the matching (current) seq clears normally.
        let c = book.begin_turn();
        assert_eq!(book.reserved, Some(c));
        book.turn_committed(c);
        assert_eq!(book.reserved, None);
    }

    /// ma-sdk-state r7 item 1: a failed write must roll back the matching turn
    /// WHEREVER it sits, not only the FIFO back — otherwise a reservation whose
    /// cleanup lost the write_seq race leaves a phantom turn ahead of a later
    /// one and shifts attribution.
    #[test]
    fn cancel_removes_a_matching_entry_even_when_it_is_not_the_back() {
        let mut book = TurnBook::default();
        let a = book.begin_turn();
        let b = book.begin_turn();
        let c = book.begin_turn();
        assert!(!book.cancel_turn(b), "removing the middle settles nothing");
        let seqs: Vec<u64> = book.outstanding.iter().map(|turn| turn.seq).collect();
        assert_eq!(seqs, vec![a, c], "B gone, A and C intact");
        // reserved still names C (B was not the reserved back here); clearing it
        // is keyed to seq.
        assert_eq!(book.reserved, Some(c));
        // Removing C clears the C reservation; A still outstanding so no settle.
        assert!(!book.cancel_turn(c));
        assert_eq!(book.reserved, None);
        assert_eq!(
            book.outstanding.iter().map(|t| t.seq).collect::<Vec<_>>(),
            vec![a]
        );
    }

    /// ma-sdk-state r7 item 4: when the LAST local turn's result reports a
    /// queued native follow-up, the deferred settle must be CLEARED — a later
    /// failed write can never synthesise settledRootTurn while the native queue
    /// is in flight.
    #[test]
    fn a_queued_last_local_result_clears_deferred_so_a_later_failure_cannot_settle() {
        let mut book = TurnBook::default();
        book.begin_turn(); // A
        book.begin_turn(); // B (last local)
        assert_eq!(book.result_settles_root(0), RootSettle::Deferred);
        assert!(book.deferred_settle, "A is deferred behind B");

        // B completes but a native follow-up turn is queued: B pops, NotSettled,
        // and the book empties. The deferral must not survive.
        assert_eq!(book.result_settles_root(1), RootSettle::NotSettled);
        assert!(book.outstanding.is_empty());
        assert!(
            !book.deferred_settle,
            "queued native turn owns the settle, not a later cancel"
        );

        // A later write failure (no matching entry left) releases nothing.
        assert!(!book.cancel_turn(1));
    }

    /// ma-sdk-state r6 item 5: turn A completes while newer turn B is
    /// outstanding (its result is deferred), then B's write fails and is rolled
    /// back. The rollback must release A's deferred settle so the root idles
    /// instead of staying working.
    #[test]
    fn rolling_back_the_newer_turn_releases_the_older_deferred_settle() {
        let mut book = TurnBook::default();
        book.begin_turn(); // A
        book.begin_turn(); // B

        // A's result pops A but cannot settle behind B.
        assert_eq!(book.result_settles_root(0), RootSettle::Deferred);
        assert!(book.deferred_settle);
        assert_eq!(book.outstanding.len(), 1, "only B remains outstanding");

        // B's failed write rolls B back and releases the owed settle.
        let released = {
            let back = book.outstanding.back().expect("B is the back").seq;
            book.cancel_turn(back)
        };
        assert!(released, "rolling B back releases A's deferred settle");
        assert!(book.outstanding.is_empty());
        assert!(!book.deferred_settle);

        // A genuinely-queued/native result is NOT deferred, so cancelling a
        // blocker later releases nothing.
        book.begin_turn(); // C
        book.begin_turn(); // D
        assert_eq!(book.result_settles_root(1), RootSettle::NotSettled);
        assert!(!book.deferred_settle, "a queued result is not deferred");
        let back = book.outstanding.back().expect("D is the back").seq;
        assert!(!book.cancel_turn(back), "no deferred settle to release");
    }

    /// ma-sdk-state r11 item 1: A completes while B is outstanding (deferred);
    /// a NEWER turn C is then cancelled while B (older than C) is still active.
    /// C's cancellation must NOT release the deferred settle — B still owns the
    /// root's working state. Releasing it here emitted settledRootTurn in the
    /// middle of B's turn (Node/Hub read the session as idle mid-turn).
    #[test]
    fn cancelling_a_newer_c_keeps_the_deferral_while_older_b_is_active() {
        let mut book = TurnBook::default();
        book.begin_turn(); // A (committed)
        book.begin_turn(); // B (committed, still active)

        // A's result pops A; B keeps the root working: deferred.
        assert_eq!(book.result_settles_root(0), RootSettle::Deferred);
        assert!(book.deferred_settle);

        // C reserves behind B and is cancelled. B is OLDER than C and still
        // outstanding, so the deferral survives C's removal.
        let c = book.begin_turn();
        assert!(!book.cancel_turn(c), "C is newer than the active blocker B");
        assert!(
            book.deferred_settle,
            "the owed settle stays deferred while B is active"
        );
        assert_eq!(
            book.outstanding
                .iter()
                .map(|turn| turn.seq)
                .collect::<Vec<_>>(),
            vec![1],
            "C removed, B intact"
        );

        // B's own result then settles the root the ordinary way (the stashed
        // deferral is superseded by that real settle).
        assert_eq!(book.result_settles_root(0), RootSettle::Settled);
        assert!(!book.deferred_settle);
        assert!(book.outstanding.is_empty());

        // The mirror ordering: with A2 deferred behind B2, cancelling B2
        // (the active blocker) releases the deferral even while a newer D2 is
        // already booked behind it — D2's start is queued behind B2's ticket
        // and therefore behind the correction.
        book.begin_turn(); // A2
        let b2 = book.begin_turn(); // B2 (the blocker)
        assert_eq!(book.result_settles_root(0), RootSettle::Deferred);
        let d2 = book.begin_turn(); // D2 (newer reservation behind B2)
        assert!(
            book.cancel_turn(b2),
            "cancelling the blocker B2 releases A2"
        );
        assert!(!book.deferred_settle);
        assert_eq!(
            book.outstanding
                .iter()
                .map(|turn| turn.seq)
                .collect::<Vec<_>>(),
            vec![d2],
            "only D2 remains; the correction precedes its start"
        );
    }
}

/// ma-sdk-state r14 item 1: the per-launch owned-permit backlog accounting
/// under controlled admission/release, cap breach, concurrent drain, and
/// cross-launch isolation.
#[cfg(test)]
mod backlog_accounting_tests {
    use super::*;
    use std::sync::Arc;

    /// [`PublishJob`] does not implement `Debug`; assert admission without it.
    fn admit(queue: &PublishQueue, what: &str) {
        match queue.admit_frame(Box::new(Outbound::KeepAlive)) {
            Ok(()) => {}
            Err(_) => panic!("{what} must be admitted"),
        }
    }

    fn budget(cap_items: usize, cap_bytes: usize) -> Arc<BacklogBudget> {
        BacklogBudget::new(cap_items, cap_bytes)
    }

    /// The ONE named bound for joining the admission/drain stress threads
    /// (ma-sdk-state r16 item 3): no inline `tokio::time::timeout` at the
    /// join sites.
    async fn bounded_accounting_join<F: std::future::Future>(future: F, what: &str) -> F::Output {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .unwrap_or_else(|_| panic!("bounded test wait timed out: {what}"))
    }

    /// Directive (b): every exit path releases by Drop — a queued job drained,
    /// a job discarded while queued (worker aborted, receiver dropped), and a
    /// charge the caller never queued all return their permits exactly once, so
    /// occupancy settles at exactly zero and the next admission succeeds
    /// instead of tripping a false overflow.
    #[test]
    fn dropped_jobs_release_their_charge_on_every_exit_path() {
        const CAP: usize = 2;
        let b = budget(CAP, 1024 * 1024 * 1024);
        let (queue, mut rx) = PublishQueue::new(Arc::clone(&b));
        let cost = frame_backlog_cost(&Outbound::KeepAlive);

        admit(&queue, "A");
        admit(&queue, "B");
        assert_eq!(b.occupied(), (2, 2 * cost), "A and B charged");

        // The worker drains A (takes the job and drops it after handling).
        let job_a = rx.try_recv().expect("A visible");
        assert!(matches!(job_a, PublishJob::Frame { .. }));
        drop(job_a);
        assert_eq!(b.occupied(), (1, cost), "A released by Drop");

        // Real occupancy is one: C is admitted — no false overflow.
        admit(&queue, "C");
        assert_eq!(b.occupied(), (2, 2 * cost), "B + C charged");

        // close aborts the worker with B and C still queued: dropping the
        // receiver drops the buffered jobs, and their permits return.
        drop(rx);
        assert_eq!(b.occupied(), (0, 0), "queued residue releases on teardown");

        // The next admission succeeds on the same (drained) budget.
        let charge = b
            .acquire(cost)
            .expect("a fresh admission succeeds once occupancy returned to zero");
        assert_eq!(b.occupied(), (1, cost));
        drop(charge);
        assert_eq!(b.occupied(), (0, 0));
        assert!(!b.overflowed.load(Ordering::Acquire));
        assert!(b.snapshot().items_high <= CAP);
        assert!(b.snapshot().bytes_high <= 2 * cost);
    }

    /// Controlled concurrent admission and drain on a multi-thread runtime:
    /// admitting threads and a dropping consumer run truly in parallel, and
    /// once every job is drained occupancy is exactly zero with no false
    /// overflow and high-water marks inside the caps (directive (b): permit
    /// add/drop is the only accounting movement, so it cannot race itself).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_admissions_and_drains_settle_at_exactly_zero() {
        const CAP: usize = 16;
        const ADMITTERS: usize = 8;
        const PER_ADMITTER: usize = 200;
        const TOTAL: usize = ADMITTERS * PER_ADMITTER;

        let b = budget(CAP, 1024 * 1024 * 1024);
        let (queue, rx) = PublishQueue::new(Arc::clone(&b));

        let drainer = tokio::task::spawn_blocking(move || {
            let mut rx = rx;
            let mut seen = 0usize;
            while seen < TOTAL {
                match rx.blocking_recv() {
                    Some(job) => {
                        assert!(matches!(job, PublishJob::Frame { .. }));
                        seen += 1;
                        drop(job);
                    }
                    None => panic!("the queue closed with only {seen}/{TOTAL} jobs drained"),
                }
            }
            seen
        });

        let mut admitters = Vec::new();
        for _ in 0..ADMITTERS {
            let q = queue.clone();
            admitters.push(tokio::task::spawn_blocking(move || {
                let mut accepted = 0usize;
                while accepted < PER_ADMITTER {
                    match q.admit_frame(Box::new(Outbound::KeepAlive)) {
                        Ok(()) => accepted += 1,
                        // Caps momentarily full while the drainer is behind:
                        // yield and retry — this is admission contention, not a
                        // shutdown (no overflow latch fires in this test).
                        Err((BacklogAdmitError::Overflow { .. }, _)) => {
                            std::thread::yield_now();
                        }
                        Err((kind, _)) => panic!("unexpected admission refusal: {kind:?}"),
                    }
                }
                accepted
            }));
        }
        for handle in admitters {
            let n = bounded_accounting_join(handle, "an admitter thread")
                .await
                .expect("admitter thread");
            assert_eq!(n, PER_ADMITTER);
        }
        drop(queue);
        let seen = bounded_accounting_join(drainer, "the drainer thread")
            .await
            .expect("drainer thread");
        assert_eq!(seen, TOTAL);
        assert_eq!(
            b.occupied(),
            (0, 0),
            "concurrent drain leaves exactly zero occupancy"
        );
        assert!(!b.overflowed.load(Ordering::Acquire));
        let snapshot = b.snapshot();
        assert!(
            snapshot.items_high <= CAP,
            "items high-water stays inside the cap"
        );
        // One more admission succeeds on the drained budget.
        let charge = b
            .acquire(frame_backlog_cost(&Outbound::KeepAlive))
            .expect("the drained budget admits again");
        drop(charge);
        assert_eq!(b.occupied(), (0, 0));
    }

    /// Directive (a): a launch-A charge released AFTER launch B's budget exists
    /// can touch only A's semaphores. The budgets are independent objects — the
    /// relaunch regression asserts the same property through the driver.
    #[test]
    fn a_launch_a_charge_cannot_touch_launch_b_budget() {
        let budget_a = budget(4, 1024 * 1024 * 1024);
        let (queue_a, mut rx_a) = PublishQueue::new(Arc::clone(&budget_a));
        admit(&queue_a, "A");
        let job_a = rx_a.try_recv().expect("A visible");
        // Simulate the publisher still holding A's charge while the driver
        // relaunches: B is constructed FRESH with A's job still in flight.
        let budget_b = budget(4, 1024 * 1024 * 1024);
        let (queue_b, _rx_b) = PublishQueue::new(Arc::clone(&budget_b));
        assert_ne!(
            budget_a.id(),
            budget_b.id(),
            "each launch gets a fresh budget"
        );
        assert_eq!(budget_b.occupied(), (0, 0), "B starts unencumbered");
        let b_cost = frame_backlog_cost(&Outbound::KeepAlive);
        admit(&queue_b, "B");
        assert_eq!(budget_b.occupied(), (1, b_cost), "B admits its own job");

        // A's publisher now releases its in-flight charge (an aborted task's
        // Drop). It lands on A's dead semaphore only.
        drop(job_a);
        assert_eq!(budget_a.occupied(), (0, 0), "A returned to zero");
        assert_eq!(
            budget_b.occupied(),
            (1, b_cost),
            "launch B's budget is untouched by launch A's release"
        );
    }

    /// A genuine item-cap breach is refused, names the overflow, and leaves no
    /// charge behind: the refused frame is returned to the caller with no
    /// permits attached.
    #[test]
    fn a_frame_past_the_item_cap_is_refused_without_holding_a_charge() {
        let b = budget(1, 1024 * 1024 * 1024);
        let (queue, _rx) = PublishQueue::new(Arc::clone(&b));
        admit(&queue, "the one frame");
        let Err((kind, returned)) = queue.admit_frame(Box::new(Outbound::KeepAlive)) else {
            panic!("the second frame must be refused");
        };
        assert!(
            matches!(kind, BacklogAdmitError::Overflow { .. }),
            "refusal names the overflow, not closure: {kind:?}"
        );
        // The refused frame comes back uncharged and dropping it changes
        // nothing.
        drop(returned);
        assert_eq!(b.occupied().0, 1, "the refusal reserved nothing");
        assert!(
            !b.overflowed.load(Ordering::Acquire),
            "the latch is the spawner's job"
        );
    }

    /// A byte-cap breach is refused the same way and carries the frame's exact
    /// byte cost in the diagnostic.
    #[test]
    fn a_frame_past_the_byte_cap_is_refused_with_its_cost() {
        let cost = frame_backlog_cost(&Outbound::KeepAlive);
        let b = budget(16, cost); // one frame fits, a second does not
        let (queue, _rx) = PublishQueue::new(Arc::clone(&b));
        admit(&queue, "the one frame");
        let Err((kind, _returned)) = queue.admit_frame(Box::new(Outbound::KeepAlive)) else {
            panic!("the second frame must exceed the byte cap");
        };
        match kind {
            BacklogAdmitError::Overflow { frame_bytes, .. } => assert_eq!(frame_bytes, cost),
            other => panic!("expected overflow, got {other:?}"),
        }
        assert_eq!(b.occupied(), (1, cost));
    }
}

/// ma-sdk-state r14 item 3 (directive (d)): on a multi-threaded runtime an
/// ordinary emission that reserved consumer capacity while parked on a full
/// channel must never land AFTER the terminal observation, however the
/// terminal send races the slot freeing. Observations already queued ahead of
/// the terminal are preserved.
#[cfg(test)]
mod emission_ordering_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// The ONE named bound for every await in the terminal-ordering stress
    /// test (ma-sdk-state r15 item 5): no inline `tokio::time::timeout` at the
    /// scheduling/join sites.
    async fn bounded_race_wait<F: std::future::Future>(future: F, what: &str) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .unwrap_or_else(|_| panic!("bounded test wait timed out: {what}"))
    }

    fn is_terminal(obs: &Observation) -> bool {
        match &obs.body {
            ObservationPayload::Lifecycle(payload) => match payload.as_ref() {
                LifecyclePayload::Native(native) => {
                    native.native_name == "session"
                        && matches!(
                            &native.status,
                            Knowledge::Known { value } if value == "exited"
                        )
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Trigger half of the FIRE_AFTER acknowledgement (ma-sdk-state r15 item 5).
    ///
    /// A retained [`oneshot`], not a `Notify`: the drainer thread can reach
    /// FIRE_AFTER and signal BEFORE the firing thread registers its wait, and
    /// `Notify::notify_waiters` stores nothing — that single notification is
    /// lost and the test hangs forever. The oneshot retains the signal and the
    /// later receive completes immediately.
    #[derive(Clone)]
    struct FireTrigger {
        tx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    }

    impl FireTrigger {
        fn fire(&self) {
            if let Some(tx) = self.tx.lock().expect("fire gate lock").take() {
                let _ = tx.send(());
            }
        }
    }

    /// Create the FIRE_AFTER acknowledgement: a one-shot trigger the drainer
    /// fires once and a receiver the firing thread awaits under its named
    /// bound.
    fn fire_gate() -> (FireTrigger, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            FireTrigger {
                tx: std::sync::Arc::new(std::sync::Mutex::new(Some(tx))),
            },
            rx,
        )
    }

    /// One stress iteration: tiny channel, one terminal slot pre-reserved, many
    /// ordinary emitters, a fast drainer freeing slots continuously, and the
    /// terminal fired once ordinary traffic is provably flowing. Returns the
    /// number of observations received AFTER the terminal (must be zero).
    async fn one_terminal_race_iteration(iter: usize) -> usize {
        const CAP: usize = 4;
        const EMITTERS: usize = 8;
        const PER_EMITTER: usize = 2_000;
        // Fire the terminal once this many ordinary observations drained.
        const FIRE_AFTER: usize = 24;

        let inner = build_inner(None, DriverKind::ClaudePrint);
        let (tx, mut rx) = mpsc::channel::<Observation>(CAP);
        let exit_permit = tx
            .clone()
            .try_reserve_owned()
            .expect("the terminal slot reserves");
        // The emission state is launch-scoped (r15 item 1); the in-crate
        // publication tests drive the launch directly with no child.
        let budget = BacklogBudget::new(16_384, 128 * 1024 * 1024);
        let (publish_queue, _publish_rx) = PublishQueue::new(budget);
        let launch = Launch::new(
            publish_queue,
            Some(tx),
            Some(exit_permit),
            None,
            #[cfg(feature = "test-stub")]
            Arc::clone(&inner.barrier),
        );

        let ordinary = {
            let mut mapper = inner.mapper.lock().await;
            mapper
                .lifecycle(
                    LifecycleTopic::Session,
                    "x-ordinary",
                    Knowledge::Known { value: "s".into() },
                    "ok",
                    false,
                )
                .expect("ordinary observation")
        };

        // Retained FIRE_AFTER acknowledgement (see [`fire_gate`]).
        let (fire, fire_rx) = fire_gate();
        let ordinary_seen = std::sync::Arc::new(AtomicUsize::new(0));

        // Fast drainer: frees slots as fast as it can, which is exactly what
        // lets a parked ordinary send compete with the terminal send.
        let drainer_seen = std::sync::Arc::clone(&ordinary_seen);
        let drainer = tokio::spawn(async move {
            let mut terminal_index: Option<usize> = None;
            let mut after = 0usize;
            let mut total = 0usize;
            let mut preserved_before = 0usize;
            while let Some(obs) = rx.recv().await {
                total += 1;
                if is_terminal(&obs) {
                    assert!(terminal_index.is_none(), "iteration {iter}: terminal twice");
                    terminal_index = Some(total);
                    continue;
                }
                match terminal_index {
                    None => preserved_before += 1,
                    Some(_) => after += 1,
                }
                if drainer_seen.fetch_add(1, Ordering::AcqRel) + 1 == FIRE_AFTER {
                    fire.fire();
                }
            }
            (preserved_before, after)
        });

        // Emitters hammer the ordinary path on parallel OS threads.
        let mut emitters = Vec::new();
        for _ in 0..EMITTERS {
            let launch = Arc::clone(&launch);
            let obs = ordinary.clone();
            emitters.push(tokio::spawn(async move {
                for _ in 0..PER_EMITTER {
                    emit_all(&launch, vec![obs.clone()])
                        .await
                        .expect("emit_all");
                }
            }));
        }

        // Wait for the RETAINED FIRE_AFTER acknowledgement (the drainer may
        // have fired it on another worker thread before this point), then fire
        // the terminal on yet another thread.
        bounded_race_wait(
            async { fire_rx.await.expect("the FIRE_AFTER acknowledgement") },
            "the retained FIRE_AFTER drain acknowledgement",
        )
        .await;
        let terminal_inner = Arc::clone(&inner);
        let terminal_launch = Arc::clone(&launch);
        let terminal =
            tokio::spawn(
                async move { emit_exit(&terminal_inner, &terminal_launch, "exited").await },
            );
        bounded_race_wait(terminal, "iteration {iter}: terminal send")
            .await
            .expect("terminal task")
            .expect("terminal emitted");

        for handle in emitters {
            bounded_race_wait(handle, "iteration {iter}: emitter bound")
                .await
                .expect("emitter task");
        }
        // Seal and drop the sender so the drainer reaches channel EOF.
        launch.emission.lock().await.tx = None;
        drop(launch);
        let (preserved, after) = bounded_race_wait(drainer, "iteration {iter}: drainer bound")
            .await
            .expect("drainer task");
        assert!(
            preserved >= FIRE_AFTER,
            "iteration {iter}: observations queued before the terminal are preserved"
        );
        after
    }

    /// ma-sdk-state r15 item 5: the drainer can win the thread schedule and
    /// reach FIRE_AFTER BEFORE the firing thread awaits the acknowledgement —
    /// on a multi-thread runtime this actually happens. Signalling before the
    /// wait must not be lost: with the old `Notify::notify_waiters` it stored
    /// no permit and the (named-bounded) wait hung; the retained oneshot
    /// completes immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_fire_acknowledgement_is_retained_when_signalled_before_the_wait() {
        let (fire, fire_rx) = fire_gate();
        // Deterministically reproduce the winning schedule: signal first.
        fire.fire();
        bounded_race_wait(
            async {
                fire_rx
                    .await
                    .expect("the retained pre-wait acknowledgement")
            },
            "a FIRE_AFTER acknowledgement signalled before the wait",
        )
        .await;
    }

    /// Run the race repeatedly: a single reordered delivery in ANY iteration
    /// fails the test (directive (d) must hold under real thread parallelism).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn nothing_ordinary_follows_the_terminal_under_multithreaded_contention() {
        for iter in 0..20 {
            let after = one_terminal_race_iteration(iter).await;
            assert_eq!(
                after, 0,
                "iteration {iter}: {after} ordinary observations landed AFTER the terminal"
            );
        }
    }
}

#[cfg(test)]
mod partial_usage_poll_boundary_tests {
    use super::*;
    use serde_json::json;

    fn mapper() -> TranscriptMapper {
        TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "partial-usage-session".into(),
            "2.1.289".into(),
        )
    }

    /// One same-message assistant frame: the first record reports every
    /// counter, the later repeated record (Claude repeats stop_reason on each
    /// content record) reports only output.
    fn full_record() -> String {
        json!({
            "type": "assistant",
            "uuid": "msg-partial-uuid-1",
            "requestId": "req-partial",
            "sessionId": "partial-usage-session",
            "version": "2.1.289",
            "timestamp": "2026-10-14T10:00:00.000Z",
            "message": {
                "id": "msg-partial",
                "role": "assistant",
                "type": "message",
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 3,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 1,
                        "ephemeral_1h_input_tokens": 2
                    }
                }
            }
        })
        .to_string()
    }

    fn output_only_record() -> String {
        json!({
            "type": "assistant",
            "uuid": "msg-partial-uuid-2",
            "requestId": "req-partial",
            "sessionId": "partial-usage-session",
            "version": "2.1.289",
            "timestamp": "2026-10-14T10:00:01.000Z",
            "message": {
                "id": "msg-partial",
                "role": "assistant",
                "type": "message",
                "stop_reason": "end_turn",
                "usage": { "output_tokens": 20 }
            }
        })
        .to_string()
    }

    fn last_usage(observations: Vec<Observation>) -> Box<UsagePayload> {
        observations
            .into_iter()
            .filter_map(|observation| match observation.body {
                ObservationPayload::Usage(usage) => Some(usage),
                _ => None,
            })
            .next_back()
            .expect("the group published at least one usage observation")
    }

    /// c-ctxusage r26 item 3: records that coalesce within ONE poll (one
    /// unpublished flush) merge per field before publication — a later
    /// output-only frame must not erase the earlier input/cache counters.
    #[test]
    fn coalesced_records_in_one_poll_merge_per_field() {
        let payload = {
            let mut mapper = mapper();
            mapper.map_line(&full_record()).expect("map full");
            mapper.map_line(&output_only_record()).expect("map partial");
            last_usage(mapper.flush().expect("flush"))
        };
        assert_eq!(payload.input_tokens, Knowledge::Known { value: U64(100) });
        assert_eq!(payload.output_tokens, Knowledge::Known { value: U64(20) });
        assert_eq!(
            payload.cache_read_tokens,
            Knowledge::Known { value: U64(0) },
            "earlier cache read retained"
        );
        assert_eq!(
            payload.cache_write_tokens,
            Knowledge::Known { value: U64(3) },
            "earlier TTL-split cache write retained (1+2)"
        );
        assert_eq!(payload.total_tokens, Knowledge::Known { value: U64(123) });
    }

    /// c-ctxusage r26 item 3 / U1 across a poll boundary: once a flush has
    /// published, the group's accumulated usage is consumed. A later record
    /// in another poll is published with exactly the buckets IT reported —
    /// omitted buckets stay Unknown on the wire (never retained from the
    /// earlier frame, never zero). The Hub's `merged_over` combines it with
    /// the prior stored frame (end-to-end identity is pinned in
    /// remuda-node `usage_turn_revision`).
    #[test]
    fn a_partial_record_in_a_later_poll_stays_partial_on_the_wire() {
        let mut mapper = mapper();
        mapper.map_line(&full_record()).expect("map full");
        let first = last_usage(mapper.flush().expect("first poll flush"));
        assert_eq!(first.input_tokens, Knowledge::Known { value: U64(100) });

        mapper.map_line(&output_only_record()).expect("map partial");
        let second = last_usage(mapper.flush().expect("second poll flush"));
        assert_eq!(
            second.output_tokens,
            Knowledge::Known { value: U64(20) },
            "the later frame's output reports"
        );
        for omitted in [
            &second.input_tokens,
            &second.cache_read_tokens,
            &second.cache_write_tokens,
            &second.total_tokens,
        ] {
            assert!(
                matches!(omitted, Knowledge::Unknown { .. }),
                "an omitted bucket is Unknown across a poll boundary, got {omitted:?}"
            );
        }
    }
}
