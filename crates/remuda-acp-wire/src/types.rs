//! Spawn, session, and classified ACP wire types.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Error;

/// Default grok model for this crate's spawn helper.
pub const DEFAULT_MODEL: &str = "grok-4.6";

/// Client identity sent on `initialize`.
pub const CLIENT_NAME: &str = "runtime";

/// How a captured probe line was transported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// `grok agent … stdio`.
    Stdio,
    /// `grok agent serve` WebSocket.
    Ws,
    /// Unknown or missing `transport` field.
    Other,
}

impl TransportKind {
    /// Parse a capture `transport` string.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("stdio") => Self::Stdio,
            Some("ws") => Self::Ws,
            _ => Self::Other,
        }
    }
}

/// Direction of a captured RPC relative to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Client → agent.
    C2a,
    /// Agent → client.
    A2c,
    /// Missing or unknown `dir`.
    Other,
}

impl Direction {
    /// Parse a capture `dir` string.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("c2a") => Self::C2a,
            Some("a2c") => Self::A2c,
            _ => Self::Other,
        }
    }
}

/// Envelope used by `docs/research/cli-help/grok-acp-*.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureMeta {
    /// Optional timestamp from the probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<f64>,
    /// Client→agent or agent→client.
    pub dir: Direction,
    /// stdio vs WebSocket.
    pub transport: TransportKind,
}

/// Known `sessionUpdate` tags from stable ACP v1, plus unknown grok tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionUpdateKind {
    /// User prompt echo / load replay.
    UserMessageChunk,
    /// Assistant text delta.
    AgentMessageChunk,
    /// Reasoning delta.
    AgentThoughtChunk,
    /// New tool invocation.
    ToolCall,
    /// Tool progress or result.
    ToolCallUpdate,
    /// Execution plan (not a workflow).
    Plan,
    /// Slash / skill catalog.
    AvailableCommandsUpdate,
    /// Session mode change.
    CurrentModeUpdate,
    /// Config option snapshot.
    ConfigOptionUpdate,
    /// Title / metadata.
    SessionInfoUpdate,
    /// Usage snapshot.
    UsageUpdate,
    /// Tag the SDK `SessionUpdate` enum does not know.
    Unknown(String),
}

impl SessionUpdateKind {
    /// Classify a `sessionUpdate` tag string.
    #[must_use]
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "user_message_chunk" => Self::UserMessageChunk,
            "agent_message_chunk" => Self::AgentMessageChunk,
            "agent_thought_chunk" => Self::AgentThoughtChunk,
            "tool_call" => Self::ToolCall,
            "tool_call_update" => Self::ToolCallUpdate,
            "plan" => Self::Plan,
            "available_commands_update" => Self::AvailableCommandsUpdate,
            "current_mode_update" => Self::CurrentModeUpdate,
            "config_option_update" => Self::ConfigOptionUpdate,
            "session_info_update" => Self::SessionInfoUpdate,
            "usage_update" => Self::UsageUpdate,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Wire tag, including unknown values.
    #[must_use]
    pub fn tag(&self) -> &str {
        match self {
            Self::UserMessageChunk => "user_message_chunk",
            Self::AgentMessageChunk => "agent_message_chunk",
            Self::AgentThoughtChunk => "agent_thought_chunk",
            Self::ToolCall => "tool_call",
            Self::ToolCallUpdate => "tool_call_update",
            Self::Plan => "plan",
            Self::AvailableCommandsUpdate => "available_commands_update",
            Self::CurrentModeUpdate => "current_mode_update",
            Self::ConfigOptionUpdate => "config_option_update",
            Self::SessionInfoUpdate => "session_info_update",
            Self::UsageUpdate => "usage_update",
            Self::Unknown(tag) => tag,
        }
    }
}

/// One classified ACP JSON-RPC object (raw JSON, not the SDK connection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireEvent {
    /// JSON-RPC request (`id` + `method`).
    Request {
        /// Request id (string or number, as JSON).
        id: Value,
        /// Method name.
        method: String,
        /// Params object or array.
        #[serde(default)]
        params: Value,
    },
    /// JSON-RPC success response.
    Response {
        /// Matching request id.
        id: Value,
        /// Result payload.
        result: Value,
    },
    /// JSON-RPC error response.
    Error {
        /// Matching request id.
        id: Value,
        /// Numeric code.
        code: i64,
        /// Message.
        message: String,
        /// Optional data.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// `session/update` notification, typed when the tag is stable ACP.
    SessionUpdate {
        /// Session id string when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// `update.sessionUpdate` tag.
        update_kind: SessionUpdateKind,
        /// Full `update` object.
        update: Value,
        /// Optional `_meta`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    /// `_x.ai/*` (or rewritten `x.ai/*`) extension method.
    Ext {
        /// Wire method, including the leading underscore.
        method: String,
        /// Params.
        #[serde(default)]
        params: Value,
        /// Request id when this was a request rather than a notification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
    },
    /// Notification or request whose method is not session/update and not `_x.ai/*`.
    Unknown {
        /// Method name.
        method: String,
        /// Params.
        #[serde(default)]
        params: Value,
        /// Request id when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
    },
}

/// How to launch `grok agent … stdio`.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Executable (`GROK_BINARY` or `grok`).
    pub binary: PathBuf,
    /// `--model` value.
    pub model: String,
    /// Insert `--always-approve` (no `session/request_permission`).
    pub always_approve: bool,
    /// Insert `--no-leader` (default: true).
    pub no_leader: bool,
    /// Child working directory.
    pub cwd: PathBuf,
    /// Optional `GROK_HOME`.
    pub grok_home: Option<PathBuf>,
    /// Extra environment (after the crate's required vars).
    pub extra_env: BTreeMap<String, String>,
}

impl SpawnSpec {
    /// Stdio spawn for this crate: always-approve, no-leader, grok-4.6.
    #[must_use]
    pub fn stdio(cwd: impl Into<PathBuf>) -> Self {
        Self {
            binary: grok_binary(),
            model: DEFAULT_MODEL.to_string(),
            always_approve: true,
            no_leader: true,
            cwd: cwd.into(),
            grok_home: None,
            extra_env: BTreeMap::new(),
        }
    }

    /// Argv after the binary (`agent --always-approve --model … --no-leader stdio`).
    #[must_use]
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["agent".to_string()];
        if self.always_approve {
            args.push("--always-approve".into());
        }
        args.push("--model".into());
        args.push(self.model.clone());
        if self.no_leader {
            args.push("--no-leader".into());
        }
        args.push("stdio".into());
        args
    }
}

/// Connect to `grok agent serve` at `/ws`.
#[derive(Debug, Clone)]
pub struct ServeSpec {
    /// `ws://127.0.0.1:PORT/ws` — `server-key` query is stripped if present.
    pub url: String,
    /// Bearer secret. Never placed in the URL.
    pub secret: String,
}

impl ServeSpec {
    /// Loopback serve URL plus secret.
    #[must_use]
    pub fn new(url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            secret: secret.into(),
        }
    }
}

/// Parameters for `session/new`.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Absolute cwd.
    pub cwd: PathBuf,
    /// MCP servers JSON (empty list is not “disable user MCP”).
    pub mcp_servers: Vec<Value>,
    /// When true, `_meta.yoloMode = true` (always-approve sessions).
    pub yolo_mode: bool,
}

impl SessionSpec {
    /// New session at `cwd`. `yolo_mode` matches `--always-approve` spawn.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            mcp_servers: Vec::new(),
            yolo_mode: true,
        }
    }

    /// `_meta` object, or `None` when empty.
    #[must_use]
    pub fn meta_map(&self) -> Option<Map<String, Value>> {
        if !self.yolo_mode {
            return None;
        }
        let mut meta = Map::new();
        meta.insert("yoloMode".into(), Value::Bool(true));
        Some(meta)
    }
}

/// Completed `session/prompt` plus streamed updates.
#[derive(Debug, Clone)]
pub struct PromptTurn {
    /// `stopReason` from the prompt result.
    pub stop_reason: agent_client_protocol::schema::v1::StopReason,
    /// `session/update` events observed before the result.
    pub updates: Vec<WireEvent>,
}

impl PromptTurn {
    /// Concatenate `agent_message_chunk` text blocks.
    #[must_use]
    pub fn assistant_text(&self) -> String {
        let mut out = String::new();
        for event in &self.updates {
            if let WireEvent::SessionUpdate {
                update_kind: SessionUpdateKind::AgentMessageChunk,
                update,
                ..
            } = event
                && let Some(text) = update.pointer("/content/text").and_then(Value::as_str)
            {
                out.push_str(text);
            }
        }
        out
    }

    /// True when any `tool_call` update was observed.
    #[must_use]
    pub fn has_tool_call(&self) -> bool {
        self.updates.iter().any(|event| {
            matches!(
                event,
                WireEvent::SessionUpdate {
                    update_kind: SessionUpdateKind::ToolCall,
                    ..
                }
            )
        })
    }
}

/// Inbound connection-level notification that is not `session/update`.
#[derive(Debug, Clone, PartialEq)]
pub enum InboundEvent {
    /// `_x.ai/*` extension.
    Ext {
        /// Method including the leading underscore.
        method: String,
        /// Params.
        params: Value,
    },
    /// Anything else (including grok-unknown methods).
    Unknown {
        /// Method.
        method: String,
        /// Params.
        params: Value,
    },
}

/// `GROK_BINARY` or `grok`.
#[must_use]
pub fn grok_binary() -> PathBuf {
    std::env::var_os("GROK_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("grok"))
}

/// Rewrite `x.ai/…` to `_x.ai/…`. Other methods must already start with `_`.
pub fn ensure_ext_method(method: &str) -> Result<String, Error> {
    if method.starts_with('_') {
        Ok(method.to_string())
    } else if method.starts_with("x.ai/") {
        Ok(format!("_{method}"))
    } else {
        Err(Error::ExtMethod(method.to_string()))
    }
}

/// `initialize` params with **empty** `clientCapabilities` (no fs/terminal).
#[must_use]
pub fn initialize_params(name: &str, version: &str) -> Value {
    serde_json::json!({
        "protocolVersion": 1,
        "clientCapabilities": {},
        "clientInfo": {
            "name": name,
            "version": version,
        }
    })
}

/// Crate version used as `clientInfo.version`.
#[must_use]
pub fn adapter_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Typed SDK initialize request: capabilities stay at defaults (fs/terminal off).
#[must_use]
pub fn initialize_request() -> agent_client_protocol::schema::v1::InitializeRequest {
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{Implementation, InitializeRequest};

    InitializeRequest::new(ProtocolVersion::V1)
        .client_info(Implementation::new(CLIENT_NAME, adapter_version()))
}

/// True when initialize JSON advertises fs or terminal support.
#[must_use]
pub fn client_capabilities_declare_fs_or_terminal(params: &Value) -> bool {
    let caps = params.get("clientCapabilities").unwrap_or(&Value::Null);
    let fs = caps.get("fs").unwrap_or(&Value::Null);
    let read = fs.get("readTextFile").and_then(Value::as_bool) == Some(true);
    let write = fs.get("writeTextFile").and_then(Value::as_bool) == Some(true);
    let terminal = caps.get("terminal").and_then(Value::as_bool) == Some(true);
    read || write || terminal
}
