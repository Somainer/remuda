//! Codex app-server JSON-RPC client over stdin/stdout NDJSON.
//!
//! D-013 freeze: spawn, `initialize`/`initialized`, `thread/start`,
//! `turn/start`, typed notifications, and `turn/interrupt`. Native frames omit
//! `"jsonrpc":"2.0"`. `thread/resume`, `model/list`, daemon, and `unix://`/`ws://`
//! are out of scope.

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
pub use error::{RpcError, WireError};
pub use notification::{
    AgentMessageDeltaNotification, DeprecationNoticeNotification, ErrorNotification,
    ItemCompletedNotification, ItemStartedNotification, McpServerStartupStatusUpdatedNotification,
    ServerNotification, ThreadStartedNotification, ThreadStatusChangedNotification,
    ThreadTokenUsageUpdatedNotification, TurnCompletedNotification, TurnStartedNotification,
    TypedServerNotification, WarningNotification,
};
pub use peer::JsonRpcPeer;
pub use process::{CodexAppServer, REASONING_EFFORTS, SpawnSpec};
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
    InitializeCapabilities, InitializeParams, InitializeResponse, MessagePhase, Personality,
    ReasoningSummary, SandboxMode, SandboxPolicy, Thread, ThreadEnvironment, ThreadItem,
    ThreadStartParams, ThreadStartResponse, ThreadStatus, ThreadTokenUsage, TokenUsageBreakdown,
    Turn, TurnInterruptParams, TurnInterruptResponse, TurnItemsView, TurnStartParams,
    TurnStartResponse, TurnStatus, TypedThreadItem, UserInput,
};
