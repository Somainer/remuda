//! Codex app-server JSON-RPC client over stdin/stdout NDJSON.
//!
//! Native frames omit `"jsonrpc":"2.0"`. The default spawn is
//! `codex app-server --listen stdio://` with an absolute binary path.
//! `unix://` is **WebSocket over UDS**, not JSONL; this crate rejects it.
//!
//! `codex-app-server-protocol` is not a dependency: that crate pulls `rmcp`,
//! `zstd`, and several `codex-*` workspace packages. Types here are the subset
//! Remuda uses, taken from the 0.154.0 schema and stdio probe.

#![allow(clippy::large_enum_variant)]

mod codec;
mod error;
mod notification;
mod peer;
mod process;
mod rpc;
mod server_request;
mod types;

pub use codec::{decode_line, is_skippable_line};
pub use error::WireError;
pub use notification::{
    AgentMessageDeltaNotification, DeprecationNoticeNotification, ErrorNotification,
    ItemCompletedNotification, ItemStartedNotification, McpServerStartupStatusUpdatedNotification,
    ServerNotification, ThreadStartedNotification, ThreadStatusChangedNotification,
    ThreadTokenUsageUpdatedNotification, TurnCompletedNotification, TurnStartedNotification,
    TypedServerNotification, WarningNotification,
};
pub use peer::JsonRpcPeer;
pub use process::{CodexAppServer, Listen, SpawnSpec};
pub use rpc::{
    DEFAULT_MAX_LINE_BYTES, Inbound, JsonRpcError, JsonRpcErrorBody, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, RequestId, WireFrame, encode_line,
};
pub use server_request::{
    CommandExecutionApprovalDecision, CommandExecutionApprovalNamed,
    CommandExecutionRequestApprovalParams, CommandExecutionRequestApprovalResponse,
    FileChangeApprovalDecision, FileChangeRequestApprovalParams, FileChangeRequestApprovalResponse,
    McpElicitationResponse, ServerReply, ServerRequest, TypedServerRequest,
};
pub use types::{
    ActivePermissionProfile, ApprovalsReviewer, AskForApproval, AskForApprovalMode, ClientInfo,
    InitializeCapabilities, InitializeParams, InitializeResponse, MessagePhase, Model,
    ModelListParams, ModelListResponse, Personality, ReasoningEffortOption, ReasoningSummary,
    SandboxMode, SandboxPolicy, Thread, ThreadEnvironment, ThreadItem, ThreadListCwdFilter,
    ThreadListParams, ThreadListResponse, ThreadReadParams, ThreadReadResponse, ThreadResumeParams,
    ThreadResumeResponse, ThreadSourceKind, ThreadStartParams, ThreadStartResponse, ThreadStatus,
    ThreadTokenUsage, TokenUsageBreakdown, Turn, TurnInterruptParams, TurnInterruptResponse,
    TurnItemsView, TurnStartParams, TurnStartResponse, TurnStatus, TurnSteerParams,
    TurnSteerResponse, TypedThreadItem, UserInput,
};
