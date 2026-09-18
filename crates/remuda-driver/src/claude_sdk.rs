//! `claude-sdk` driver: the structured Claude carrier that survives many turns.
//!
//! This is the same transport `claude-print` uses — stream-json NDJSON over
//! stdio, the same `initialize` handshake, the same mapper — minus one launch
//! flag. `-p` means "Print response and exit", so a print child is gone after
//! one response and can never be a worker carrier ([D-035], `print-replacement.md`
//! §2.1). Dropping it is what makes each [`Driver::send`] another `user` line on
//! a stdin that stays open, exactly as the VS Code extension's Agent SDK
//! `query()` path does (§1.3, §1.6). Non-interactive mode comes from piped
//! stdout, not from `-p`.
//!
//! What this carrier deliberately does **not** have (§1.11, §2.6):
//!
//! * a Terminal view — stdio is not a PTY, and `shell-pty` stays the default
//!   dual-view carrier ([D-028]);
//! * a signal-tier ladder — no hooks, file tail, OSC or screen, so
//!   [`remuda_protocol::SignalTier::None`] is the honest report;
//! * IDE lock files, the `ide` MCP client, or diff accept/reject (§1.10).
//!
//! M1 scope is `print-replacement.md` §3 batches 1 and 2 plus the registration
//! half of batch 3. The suggestions whitelist and launch-bypass refuse (§2.4),
//! `onUserDialog`, subagent drill-in, the sidecar and the gateway probe (§4.2)
//! are M2; gateway-on-sdk stays **unknown** until that probe runs.
//!
//! [D-026]: ../../../docs/design/decisions.md
//! [D-028]: ../../../docs/design/decisions.md
//! [D-035]: ../../../docs/design/decisions.md

use crate::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::DriverResult;
use async_trait::async_trait;
use remuda_protocol::{
    DriverInput, DriverKind, InstanceSpec, InteractionAnswer, InteractionId, NativeRef,
};

/// Construction options for [`ClaudeSdkDriver`].
///
/// The same options print takes: profile, launch dir, native home, binary pin,
/// origin, broker, extra env, agent MCP context, setting sources, handshake
/// timeout and the host-validated `--settings` overlay.
pub type ClaudeSdkOptions = ClaudePrintOptions;

/// Native Claude stream-json driver with stdin held open across turns.
///
/// Wraps the shared print engine with [`DriverKind::ClaudeSdk`] rather than
/// forking it, so the mapper, `can_use_tool` handling, `prompt_content` base64
/// image blocks ([D-027]) and usage-from-`result` are one implementation with
/// one set of fixtures (`print-replacement.md` §2.5, §3 batch 2).
///
/// [D-027]: ../../../docs/design/decisions.md
pub struct ClaudeSdkDriver {
    inner: ClaudePrintDriver,
}

impl ClaudeSdkDriver {
    /// Build a driver from explicit options.
    pub fn new(options: ClaudeSdkOptions) -> Self {
        Self {
            inner: ClaudePrintDriver::with_carrier(options, DriverKind::ClaudeSdk),
        }
    }

    /// SIGKILL the child. Stdout EOF then emits a session-exited lifecycle.
    pub async fn kill(&self) -> DriverResult<DriverAck> {
        self.inner.kill().await
    }
}

#[async_trait]
impl Driver for ClaudeSdkDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        self.inner.capabilities().await
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        self.inner.start(spec).await
    }

    /// No live attach: this carrier has no PTY and no second view on the child
    /// (§2.6). Resume-into-terminal stays [D-026] — a **new** `shell-pty`
    /// instance with `--resume`, never a second view on a dead stdio child.
    ///
    /// [D-026]: ../../../docs/design/decisions.md
    async fn attach(&self, native_ref: NativeRef) -> DriverResult<DriverAck> {
        self.inner.attach(native_ref).await
    }

    /// One more `user` NDJSON line on the live stdin — no relaunch, no resume.
    ///
    /// In-session turns never resume (§2.2): resume is only for a *new* process
    /// continuing the same native JSONL. `Steer` stays `CapabilityUnknown`
    /// because a second `user` frame mid-turn may be native queue or steer and
    /// we have not measured which on this transport (§2.3, D-028a item 2).
    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        self.inner.send(input).await
    }

    /// Native `control_request` / `interrupt`, not `\x03` (§2.3).
    ///
    /// The capability cell stays `unknown` regardless: `fake-claude` acks this
    /// frame because its script says to, which proves Remuda writes the request,
    /// not that the CLI aborts a turn (`capabilities.rs`, D-037).
    async fn cancel(&self) -> DriverResult<DriverAck> {
        self.inner.cancel().await
    }

    async fn respond_interaction(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        self.inner.respond_interaction(id, answer).await
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        self.inner.close().await
    }

    /// Resume is a **new** process with `--resume <id>` ([D-026]); the exited
    /// child is never resurrected and `--continue` is never passed.
    ///
    /// [D-026]: ../../../docs/design/decisions.md
    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        self.inner.resume(native_ref).await
    }

    /// Launch a fresh process continuing `session_id`, whose authority is the
    /// native `system/init.session_id` the mapper copied off stdout (§2.2).
    /// A missing or empty id is [`crate::DriverError::NativeSessionNotFound`],
    /// never a silent empty conversation.
    async fn start_resumed(
        &self,
        spec: InstanceSpec,
        session_id: String,
    ) -> DriverResult<RunHandle> {
        self.inner.start_resumed(spec, session_id).await
    }
}

// `send_keys`, `write_tty`, `resize_tty`, `screen_read`, `tty_bridge` and
// `alt_screen` are intentionally left to the trait's unsupported defaults
// (`driver.rs`): there is no PTY on this carrier, so `CapabilityUnsupported` and
// `None` are the truthful answers rather than an empty grid (§2.6).
