//! Server→client JSON-RPC requests (approvals, elicitation, …).
//!
//! Replies **must** be `{id, result}` with no `method` field. v2 decisions use
//! `accept` / `decline`; legacy `execCommandApproval` uses `approved` / `denied`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rpc::{JsonRpcRequest, RequestId};

/// Inbound server request. Unknown methods become [`ServerRequest::Unknown`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerRequest {
    /// Known method.
    Typed(TypedServerRequest),
    /// Catch-all including `id` and `method`.
    Unknown(Value),
}

/// Tagged server request (`method` + `id` + `params`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum TypedServerRequest {
    /// v2 command approval.
    #[serde(
        rename = "item/commandExecution/requestApproval",
        rename_all = "camelCase"
    )]
    CommandExecutionApproval {
        /// JSON-RPC id to echo on the reply.
        id: RequestId,
        /// Approval params.
        params: CommandExecutionRequestApprovalParams,
    },
    /// v2 file-change approval.
    #[serde(rename = "item/fileChange/requestApproval", rename_all = "camelCase")]
    FileChangeApproval {
        /// JSON-RPC id.
        id: RequestId,
        /// Approval params.
        params: FileChangeRequestApprovalParams,
    },
    /// Permission-subset approval.
    #[serde(rename = "item/permissions/requestApproval", rename_all = "camelCase")]
    PermissionsApproval {
        /// JSON-RPC id.
        id: RequestId,
        /// Native params.
        params: Value,
    },
    /// Tool user-input questions.
    #[serde(rename = "item/tool/requestUserInput", rename_all = "camelCase")]
    ToolUserInput {
        /// JSON-RPC id.
        id: RequestId,
        /// Native params.
        params: Value,
    },
    /// MCP elicitation.
    #[serde(rename = "mcpServer/elicitation/request", rename_all = "camelCase")]
    McpElicitation {
        /// JSON-RPC id.
        id: RequestId,
        /// Native params.
        params: Value,
    },
    /// Legacy exec approval (`approved` / `denied`, not `accept`).
    #[serde(rename = "execCommandApproval", rename_all = "camelCase")]
    ExecCommandApproval {
        /// JSON-RPC id.
        id: RequestId,
        /// Native params.
        params: Value,
    },
    /// Legacy patch approval.
    #[serde(rename = "applyPatchApproval", rename_all = "camelCase")]
    ApplyPatchApproval {
        /// JSON-RPC id.
        id: RequestId,
        /// Native params.
        params: Value,
    },
    /// Dynamic client tool call. This is not an approval.
    #[serde(rename = "item/tool/call", rename_all = "camelCase")]
    DynamicToolCall {
        /// JSON-RPC id.
        id: RequestId,
        /// Native params.
        params: Value,
    },
}

/// v2 command-execution approval params. Extra fields are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalParams {
    /// Item id.
    pub item_id: String,
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Distinct callback id when several approvals share one item.
    #[serde(default)]
    pub approval_id: Option<String>,
    /// `command` or `writeStdin`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Command string.
    #[serde(default)]
    pub command: Option<String>,
    /// Cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Start timestamp in milliseconds.
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    /// Reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// Allowed decisions when the server enumerates them.
    #[serde(default)]
    pub available_decisions: Option<Vec<Value>>,
}

/// v2 file-change approval params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalParams {
    /// Item id.
    pub item_id: String,
    /// Thread id.
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
    /// Optional extra write root. Do not treat as an automatic grant.
    #[serde(default)]
    pub grant_root: Option<String>,
    /// Reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// Start timestamp in milliseconds.
    #[serde(default)]
    pub started_at_ms: Option<i64>,
}

/// v2 command approval decision. Do not mix with legacy `approved`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandExecutionApprovalDecision {
    /// Named decision.
    Named(CommandExecutionApprovalNamed),
    /// `{acceptWithExecpolicyAmendment:{execpolicy_amendment:[…]}}`.
    ExecPolicy(Value),
    /// `{applyNetworkPolicyAmendment:{…}}`.
    NetworkPolicy(Value),
}

/// Named v2 command decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionApprovalNamed {
    /// Accept this command.
    Accept,
    /// Accept and cache for the session.
    AcceptForSession,
    /// Deny; the turn continues.
    Decline,
    /// Deny and interrupt the turn.
    Cancel,
}

/// v2 file-change decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeApprovalDecision {
    /// Accept.
    Accept,
    /// Accept for the session.
    AcceptForSession,
    /// Deny; the turn continues.
    Decline,
    /// Deny and interrupt.
    Cancel,
}

/// `item/commandExecution/requestApproval` result body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalResponse {
    /// Decision.
    pub decision: CommandExecutionApprovalDecision,
}

/// `item/fileChange/requestApproval` result body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalResponse {
    /// Decision.
    pub decision: FileChangeApprovalDecision,
}

/// MCP elicitation result body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpElicitationResponse {
    /// `accept` / `decline` / `cancel`.
    pub action: String,
    /// Optional form content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

impl ServerRequest {
    /// Classify a `{id, method}` frame. Never returns a decode error.
    pub fn from_request(request: JsonRpcRequest, raw: Value) -> Self {
        let tagged = serde_json::json!({
            "id": request.id,
            "method": request.method,
            "params": request.params.unwrap_or(Value::Null),
        });
        match serde_json::from_value::<TypedServerRequest>(tagged) {
            Ok(typed) => Self::Typed(typed),
            Err(_) => Self::Unknown(raw),
        }
    }

    /// JSON-RPC id the client must echo on `{id, result}`.
    pub fn id(&self) -> Option<RequestId> {
        match self {
            Self::Typed(typed) => Some(typed.id()),
            Self::Unknown(value) => value
                .get("id")
                .cloned()
                .and_then(|id| serde_json::from_value(id).ok()),
        }
    }

    /// Wire method name.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Typed(typed) => Some(typed.method_name()),
            Self::Unknown(value) => value.get("method").and_then(Value::as_str),
        }
    }
}

impl TypedServerRequest {
    fn id(&self) -> RequestId {
        match self {
            Self::CommandExecutionApproval { id, .. }
            | Self::FileChangeApproval { id, .. }
            | Self::PermissionsApproval { id, .. }
            | Self::ToolUserInput { id, .. }
            | Self::McpElicitation { id, .. }
            | Self::ExecCommandApproval { id, .. }
            | Self::ApplyPatchApproval { id, .. }
            | Self::DynamicToolCall { id, .. } => id.clone(),
        }
    }

    fn method_name(&self) -> &'static str {
        match self {
            Self::CommandExecutionApproval { .. } => "item/commandExecution/requestApproval",
            Self::FileChangeApproval { .. } => "item/fileChange/requestApproval",
            Self::PermissionsApproval { .. } => "item/permissions/requestApproval",
            Self::ToolUserInput { .. } => "item/tool/requestUserInput",
            Self::McpElicitation { .. } => "mcpServer/elicitation/request",
            Self::ExecCommandApproval { .. } => "execCommandApproval",
            Self::ApplyPatchApproval { .. } => "applyPatchApproval",
            Self::DynamicToolCall { .. } => "item/tool/call",
        }
    }
}

/// Success reply body wrapper used when the caller already has JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServerReply {
    /// Echoed request id. Never include `method`.
    pub id: RequestId,
    /// Result object, for example `{"decision":"accept"}`.
    pub result: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_has_id_and_result_only() {
        let reply = ServerReply {
            id: RequestId::Integer(9),
            result: serde_json::json!({"decision":"accept"}),
        };
        let value = serde_json::to_value(&reply).expect("serialize");
        assert!(value.get("method").is_none());
        assert!(value.get("jsonrpc").is_none());
        assert_eq!(value["id"], 9);
        assert_eq!(value["result"]["decision"], "accept");
    }

    #[test]
    fn v2_command_approval_round_trip() {
        let raw = serde_json::json!({
            "id": 42,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "itemId": "item-1",
                "threadId": "thread-1",
                "turnId": "turn-1",
                "kind": "command",
                "command": "ls",
                "startedAtMs": 1
            }
        });
        let request = JsonRpcRequest {
            id: RequestId::Integer(42),
            method: "item/commandExecution/requestApproval".into(),
            params: raw.get("params").cloned(),
        };
        match ServerRequest::from_request(request, raw) {
            ServerRequest::Typed(TypedServerRequest::CommandExecutionApproval { id, params }) => {
                assert_eq!(id, RequestId::Integer(42));
                assert_eq!(params.item_id, "item-1");
                assert_eq!(params.kind.as_deref(), Some("command"));
            }
            other => panic!("expected typed approval, got {other:?}"),
        }
    }
}
