//! Node instance management and outbound Hub connection.

mod adapter_registry;
mod api_relay;
mod attachments;
mod carrier;
mod carrier_objects;
mod carrier_recovery;
pub(crate) mod computer_use;
mod config;
#[cfg(unix)]
mod daemon;
mod diagnostics;
mod driver;
mod enroll;
mod entity;
mod error;
mod files;
mod gate;
mod hook_shim;
mod identity;
mod interactions;
mod inventory;
pub(crate) mod journal_flush;
mod model;
mod native;
#[cfg(any(target_os = "macos", all(test, unix)))]
mod native_config_access;
mod origin;
pub mod prompt_correlation;
/// Carrier reclamation, adoption without replay, and restart reconciliation.
pub mod reclaim;
mod runtime;
mod runtime_link;
mod server;
mod service;
pub mod signal;
pub mod signal_messages;
mod stdio;
mod store;
pub mod subagent;
mod transport;
mod tty;
mod worker;
/// Hook-driven Workflow card producer. Public for the integration test and
/// potential reuse; it has no Node state beyond the per-session fold itself.
pub mod workflow_producer;
mod workspace;
mod workspace_access;
mod workspace_scm;
mod worktree;

pub use attachments::{
    HubObjectSource, MaterializedAttachment, ObjectSource, attachments_dir,
    cleanup as cleanup_attachments, materialize as materialize_attachments,
    sweep_orphans as sweep_attachment_orphans,
};
pub use carrier::{
    AuthState, CarrierFuture, CarrierKind, CliInventory, HerdrInventory, HostInventory,
    HostInventoryConfig, HubCarrier, NodeHello, NodeHelloParams, NodeHelloProtocol,
    OutboundWssCarrier, StdioCarrier,
};
pub use config::{DEFAULT_DEV_PORT, DevServerConfig};

/// Test-only access to the `computer-use` host preflight (D-045). Production
/// callers run it from the native driver factory; tests call this directly.
pub fn computer_use_preflight_for_test(
    kind: &remuda_protocol::AgentKind,
) -> Result<(), remuda_driver::DriverError> {
    computer_use::host_preflight(kind)
}

/// Pure preflight gate (D-045) with injectable host facts, for tests that
/// cannot mutate process env (the workspace forbids `unsafe`, hence
/// `set_var`).
pub fn computer_use_evaluate(
    kind: &remuda_protocol::AgentKind,
    os: &str,
    row: Option<&inventory::CliEntry>,
) -> Result<(), remuda_driver::DriverError> {
    computer_use::evaluate(kind, os, row)
}

/// Default CUA client install path (`$CODEX_HOME/computer-use/Codex Computer
/// Use.app`) used in refusal messages; exposed for tests (D-045 round 5).
pub fn computer_use_default_client_path() -> Option<std::path::PathBuf> {
    computer_use::default_client_path()
}
#[cfg(unix)]
pub use daemon::{
    DaemonControl, DaemonListener, DaemonWssLease, bind_daemon, connect_daemon_bridge,
    daemon_is_running, daemon_socket_path, run_daemon_runtime_controlled,
    run_daemon_runtime_listener, run_daemon_runtime_opts,
};
pub use diagnostics::{
    DoctorCheck, DoctorContext, DoctorReport, doctor_port, doctor_snapshot, doctor_with_workspace,
    doctor_workspace,
};
pub use driver::{
    Driver, DriverEmission, DriverError, DriverFactory, DriverFuture, DriverLaunch, DriverRegistry,
    DriverRequest, DriverStartFuture, FakeDriver,
};
pub use enroll::{
    Enrollment, apply_hello_result, default_data_dir, load_or_create as load_or_create_enrollment,
    save as save_enrollment,
};
pub use error::NodeError;
pub use identity::load_or_create_host_id;
pub use interactions::{InteractionRuntime, PendingInteraction};
pub use inventory::{
    CLI_KINDS, COMPUTER_USE_KIND, CliAuth, CliEntry, CollectRequest, Collector, DEFAULT_TTL,
    HerdrReport, HostSnapshot, ProbeEnv, ResourceReport, claude_native_gateway_configured, collect,
    collect_fresh, computer_use_bundle_path, computer_use_client_path, computer_use_plist_path,
    driver_capability_snapshot,
};
pub use model::{
    CommandAction, CreateInstanceRequest, CreateInstanceResponse, InstanceCommandRequest,
};
pub use native::{NativeDriverConfig, ToolMediaStagerSlot, native_driver_registry};
pub use runtime::DevNode;
pub use runtime_link::attach_runtime;
pub use server::{DevServer, dev_router, dispatch_hub_rpc};
pub use service::{LocalDrivers, RunningNode, ServeConfig, compose, serve};
pub use signal::{
    HOOKS_ENABLE_ENV, HookSessionEvidence, binds_instance, hook_activity, hooks_enabled,
};
// Hook-channel silence diagnostics live in remuda-signal next to the socket
// they probe; re-exported here as the Node-owned surface the instance badge
// reads.
pub use remuda_signal::hook_silence::{
    HookSilenceProbes, HookSilenceReason, classify as classify_hook_silence,
    diagnose as diagnose_hook_silence, link_fresh as hook_link_fresh,
    relay_executable as hook_relay_executable, socket_listening as hook_socket_listening,
};
pub use signal_messages::{MessageAssembler, MessageDelta, message_delta};
pub use stdio::{
    StdioOptions, run_stdio, run_stdio_opts, run_stdio_runtime_opts, run_stdio_with_node,
};
pub use store::{LocalStore, MemoryStore};
pub use transport::{
    Backoff, HubRequest, JournalSender, NodeTransport, TransportMetrics, TransportMetricsSnapshot,
    WssCarrier, WssConfig, WssLink,
};
pub use tty::{
    TTY_CHANNEL_INPUT, TTY_CHANNEL_OUTPUT, TTY_DEFAULT_COLS, TTY_DEFAULT_ROWS,
    TTY_FRAME_HEADER_BYTES, TTY_MAX_INPUT_BYTES, TtyAttach, TtyEvent, TtyRegistry,
    decode_tty_input, encode_tty_frame, encode_tty_input,
};
pub use workspace_access::{
    macos_workspace_guidance, prepare_workspace, workspace_access_check, workspace_access_guidance,
};
