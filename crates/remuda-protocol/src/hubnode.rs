//! Operational Hub↔Node JSON-RPC 2.0 frames for `GET /v1/node` and stdio.
//!
//! Distinct from [`crate::MethodCall`] (`runtime.hello` catalog in `protocol.md`
//! §7). This module is the M1 wire both Hub and Node speak: `node.hello` with
//! host inventory, persisted `hostId`, and an enrollment token; heartbeat;
//! instance methods; batched `journal.append`; and `tty.frame`.
//!
//! Authentication:
//! - WebSocket: HTTP `Authorization: Bearer <token>` on the upgrade.
//! - Stdio: first JSON-RPC frame is [`METHOD_NODE_AUTH`]; it is not forwarded
//!   onto the Hub WebSocket.

use crate::{
    BINARY_HEADER_LEN, BinaryChannel, BinaryFrameError, BinaryHeader, JsonRpcVersion,
    PROTOCOL_VERSION, ProtocolVersion, U64, decode_binary_frame,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// JSON-RPC method for the first stdio frame (Bearer equivalent).
pub const METHOD_NODE_AUTH: &str = "node.auth";
/// Node inventory + enrollment hello.
pub const METHOD_NODE_HELLO: &str = "node.hello";
/// Node liveness + inventory refresh.
pub const METHOD_NODE_HEARTBEAT: &str = "node.heartbeat";
/// Alias accepted by Hub for [`METHOD_NODE_HELLO`].
pub const METHOD_RUNTIME_HELLO: &str = "runtime.hello";
/// Alias accepted by Hub for [`METHOD_NODE_HEARTBEAT`].
pub const METHOD_RUNTIME_HEARTBEAT: &str = "runtime.heartbeat";
/// Read registered workspace membership on the Node.
pub const METHOD_WORKSPACE_LIST: &str = "workspace.list";
/// Prepare or commit persistent workspace registration.
pub const METHOD_WORKSPACE_REGISTER: &str = "workspace.register";
/// Prepare or commit persistent workspace removal.
pub const METHOD_WORKSPACE_UNREGISTER: &str = "workspace.unregister";
/// Provision a worker's worktree and per-worker target directory (M1 batch 5a).
pub const METHOD_WORKER_PROVISION: &str = "worker.provision";
/// Reclaim a worker's tab, worktree and target directory (M1 batch 5a).
pub const METHOD_WORKER_REMOVE: &str = "worker.remove";
/// Create an Instance on the Node.
pub const METHOD_INSTANCE_CREATE: &str = "instance.create";
/// Continue an exited Instance's native session on a new Instance (D-026).
pub const METHOD_INSTANCE_RESUME: &str = "instance.resume";
/// Submit a prompt to an Instance.
pub const METHOD_INSTANCE_SEND: &str = "instance.send";
/// Switch model / effort on a live Instance.
pub const METHOD_INSTANCE_CONFIGURE: &str = "instance.configure";
/// Cancel the current run.
pub const METHOD_INSTANCE_CANCEL: &str = "instance.cancel";
/// Answer a pending interaction (`instance.respond`).
pub const METHOD_INSTANCE_RESPOND: &str = "instance.respond";
/// Alias accepted by Hub for [`METHOD_INSTANCE_RESPOND`].
pub const METHOD_INTERACTION_RESPOND: &str = "interaction.respond";
/// Append one or more journal events.
pub const METHOD_JOURNAL_APPEND: &str = "journal.append";
/// TTY JSON control frame; binary envelopes use [`TTY_BINARY_HEADER_LEN`].
pub const METHOD_TTY_FRAME: &str = "tty.frame";
/// Observed renderer screen state for an authenticated TTY stream.
pub const METHOD_TTY_MODE: &str = "tty.mode";
/// Write logical keys to an instance PTY (Hub `POST .../commands` `tty.write`).
pub const METHOD_TTY_WRITE: &str = "tty.write";
/// Alias accepted for [`METHOD_TTY_WRITE`].
pub const METHOD_INSTANCE_KEYS: &str = "instance.keys";
/// Resize a live PTY (follow JSON `tty.resize` / Hub→Node).
pub const METHOD_TTY_RESIZE: &str = "tty.resize";
/// Attach (or refresh) a TTY stream; Hub→Node. Snapshot bytes may be in the result.
pub const METHOD_TTY_ATTACH: &str = "tty.attach";
/// Read the current screen as text; Hub→Node. Read-only and side-effect free:
/// unlike [`METHOD_TTY_ATTACH`] it opens no stream and moves no offset, so it
/// is safe to call against a session a human is watching.
pub const METHOD_TTY_SCREEN: &str = "tty.screen";
/// HTTP Authorization scheme for `GET /v1/node`.
pub const WS_AUTHORIZATION_SCHEME: &str = "Bearer";
/// `params.scheme` on [`METHOD_NODE_AUTH`].
pub const AUTH_SCHEME_BEARER: &str = "bearer";
/// Binary `tty.frame` header length; same as [`BINARY_HEADER_LEN`].
pub const TTY_BINARY_HEADER_LEN: usize = BINARY_HEADER_LEN;

/// JSON-RPC 2.0 request or notification with an optional protocol `version`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HubNodeRequest {
    /// JSON-RPC version; always `"2.0"`.
    pub jsonrpc: JsonRpcVersion,
    /// Request id. Absent on notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Hub↔Node protocol version (`protocol.md` §7.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProtocolVersion>,
    /// Method name (`node.hello`, `instance.create`, …).
    pub method: String,
    /// Method params object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response with an optional protocol `version`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HubNodeResponse {
    /// JSON-RPC version; always `"2.0"`.
    pub jsonrpc: JsonRpcVersion,
    /// Echoed request id.
    pub id: Value,
    /// Hub↔Node protocol version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProtocolVersion>,
    /// Success payload. Mutually exclusive with `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error object. Mutually exclusive with `result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

/// JSON-RPC 2.0 error object (code + message).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JsonRpcErrorObject {
    /// Native JSON-RPC code.
    pub code: i64,
    /// Human-readable message; not a Remuda discriminant.
    pub message: String,
    /// Optional structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Known Hub↔Node methods on `/v1/node` and stdio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubNodeMethod {
    /// [`METHOD_NODE_AUTH`].
    NodeAuth,
    /// [`METHOD_NODE_HELLO`].
    NodeHello,
    /// [`METHOD_RUNTIME_HELLO`].
    RuntimeHello,
    /// [`METHOD_NODE_HEARTBEAT`].
    NodeHeartbeat,
    /// [`METHOD_RUNTIME_HEARTBEAT`].
    RuntimeHeartbeat,
    /// [`METHOD_WORKSPACE_LIST`].
    WorkspaceList,
    /// [`METHOD_WORKSPACE_REGISTER`].
    WorkspaceRegister,
    /// [`METHOD_WORKSPACE_UNREGISTER`].
    WorkspaceUnregister,
    /// [`METHOD_WORKER_PROVISION`].
    WorkerProvision,
    /// [`METHOD_WORKER_REMOVE`].
    WorkerRemove,
    /// [`METHOD_INSTANCE_CREATE`].
    InstanceCreate,
    /// [`METHOD_INSTANCE_RESUME`].
    InstanceResume,
    /// [`METHOD_INSTANCE_SEND`].
    InstanceSend,
    /// [`METHOD_INSTANCE_CONFIGURE`].
    InstanceConfigure,
    /// [`METHOD_INSTANCE_CANCEL`].
    InstanceCancel,
    /// [`METHOD_INSTANCE_RESPOND`].
    InstanceRespond,
    /// [`METHOD_INTERACTION_RESPOND`].
    InteractionRespond,
    /// [`METHOD_JOURNAL_APPEND`].
    JournalAppend,
    /// [`METHOD_TTY_FRAME`].
    TtyFrame,
    /// [`METHOD_TTY_MODE`].
    TtyMode,
    /// [`METHOD_TTY_WRITE`].
    TtyWrite,
    /// [`METHOD_INSTANCE_KEYS`].
    InstanceKeys,
    /// [`METHOD_TTY_RESIZE`].
    TtyResize,
    /// [`METHOD_TTY_ATTACH`].
    TtyAttach,
    /// [`METHOD_TTY_SCREEN`].
    TtyScreen,
}

/// An explicitly sequenced phase of a workspace mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMutationPhase {
    /// Validate and persist command intent without changing membership.
    Prepare,
    /// Revalidate and persist membership before acknowledging settlement.
    Commit,
}

/// `workspace.register` / `workspace.unregister` input (D-023).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMutationParams {
    /// Stable idempotency identity reused for both phases.
    pub command_id: String,
    /// Absolute directory on the Node filesystem.
    pub path: String,
    /// Required: commit without a durable prepare is rejected.
    pub phase: WorkspaceMutationPhase,
}

/// Registered root projected into a host's inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredWorkspace {
    /// Stable identity persisted by the Node.
    pub workspace_id: String,
    /// Owning Node identity.
    pub host_id: String,
    /// Canonical absolute root.
    pub root: String,
}

/// Authoritative result of `workspace.list` and workspace mutation phases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRegistryResult {
    /// Monotonic membership revision for inventory race reconciliation.
    pub workspace_revision: u64,
    /// Complete membership snapshot, including an empty registry.
    pub workspaces: Vec<RegisteredWorkspace>,
    /// Target workspace identity; absent on reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Echo of the mutation identity; absent on reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// `prepared` or `settled`; absent on reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// `node.auth` params (stdio first frame).
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeAuthParams {
    /// Bootstrap or persisted host token.
    pub token: String,
    /// Auth scheme; `bearer` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
}

impl std::fmt::Debug for NodeAuthParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeAuthParams")
            .field("token", &"<redacted>")
            .field("scheme", &self.scheme)
            .finish()
    }
}

/// `node.hello` params: persisted host id, inventory, enrollment token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeHelloParams {
    /// Persisted host identity (`hst_…`). Also accepted nested under `host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Registry display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Node binary version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    /// Process epoch (`epoch_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_epoch: Option<String>,
    /// Enrollment / host token when not using HTTP Bearer (stdio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_token: Option<String>,
    /// Carrier (`ssh-stdio`, `outbound-wss`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Protocol version inside params (frame also has `version`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProtocolVersion>,
    /// Framing declaration (`ndjson` / `websocket-message`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Value>,
    /// Nested host inventory (hostname, labels, cli, herdr, resources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<NodeHostInventory>,
    /// Feature flags / capabilities blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
    /// Top-level CLI inventory (Hub also reads nested `host.cli`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<Value>,
}

/// Host inventory nested under `params.host` (or flattened into params).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeHostInventory {
    /// Host identity when not at the params root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Authoritative registered workspace snapshot, including an empty registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<RegisteredWorkspace>>,
    /// Monotonic Node membership revision for stale inventory rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<u64>,
    /// Best-effort hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Placement labels (map or `["region=sg"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Value>,
    /// Concurrent instance ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_instances: Option<u32>,
    /// CLI inventory array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<Value>,
    /// Herdr presence object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herdr: Option<Value>,
    /// Load snapshot `{cpuPct,memPct}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    /// `std::env::consts::OS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Kernel release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// libc identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
    /// Display label when nested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `node.heartbeat` params (inventory refresh + optional watermarks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeHeartbeatParams {
    /// Connection id from hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Lease id from hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    /// Node binary version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    /// Carrier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Nested or flat inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<NodeHostInventory>,
    /// CLI inventory refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<Value>,
    /// Capabilities blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
    /// Instance journal watermarks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instance_watermarks: Vec<JournalSeqWatermark>,
}

/// `instance.create` params as Hub actually forwards them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCreateParams {
    /// Instance identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Command identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// Create spec (kind, driver, prompt, hostId, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    /// Initial prompt payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_input: Option<Value>,
    /// Flattened kind when Hub omits `spec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Flattened driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Flattened prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Target host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Target workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// Kind of a staged attachment (D-027b, 2026-09-15).
///
/// Images keep the native image delivery a harness supports (a base64 image
/// block for `claude-print`, a readable path for the PTY family); every other
/// kind is delivered as a path reference the harness can open with its own
/// file tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    /// PNG, JPEG, GIF or WebP, sniffed from the bytes by the Hub.
    Image,
    /// Any other file. Delivered as a path reference, never inlined.
    File,
}

impl Default for AttachmentKind {
    /// Frames written before D-027b carried images only.
    fn default() -> Self {
        Self::Image
    }
}

/// One staged attachment referenced by an `instance.send` command (D-027).
///
/// Metadata only. The bytes live behind `GET /v1/objects/{objectId}` on the
/// Hub and are pulled by the Node before dispatch, because a command frame is
/// capped at 1 MiB and the prompt itself at 64 KiB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRef {
    /// Hub object identity (`obj_…`).
    pub object_id: String,
    /// Image vs. arbitrary file (D-027b). Defaults to image for frames written
    /// before the field existed.
    #[serde(default)]
    pub kind: AttachmentKind,
    /// MIME type as determined by the Hub: magic-byte sniff for images, the
    /// sanitised declared `Content-Type` for everything else.
    pub media_type: String,
    /// Sanitised original filename (no path separators, no control characters,
    /// length-capped). The Node lands the pull under this name; it is never
    /// used unsanitised as a path component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Stored byte length, for local budget checks before the pull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Lowercase hex SHA-256 of the bytes, so the Node can verify the pull
    /// landed byte-identical (D-027b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// 1-based anchor number, matching the `[Image #n]` token in the prompt
    /// text and the composer chip (2026-09-15). The array is ordered by token
    /// appearance; older clients omit this and the receiver falls back to the
    /// array position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
}

/// `instance.send` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSendParams {
    /// Target Instance.
    pub instance_id: String,
    /// Command identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// Run identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Structured input (`{text}` or prompt wrapper).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// Flattened prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Staged attachment metadata (D-027). Additive: an older Node that does
    /// not know this field simply degrades the send to text-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
}

impl AttachmentKind {
    /// Classify a Hub-accepted media type. Images keep native delivery;
    /// everything else is delivered as a path reference (D-027b).
    #[must_use]
    pub fn from_media_type(media_type: &str) -> Self {
        match media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" => {
                AttachmentKind::Image
            }
            _ => AttachmentKind::File,
        }
    }

    /// Wire form used in SQLite rows and journal metadata.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::File => "file",
        }
    }
}

impl std::str::FromStr for AttachmentKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "image" => Ok(AttachmentKind::Image),
            "file" => Ok(AttachmentKind::File),
            other => Err(format!("unknown attachment kind {other}")),
        }
    }
}

/// Longest original filename carried through staging (D-027b).
///
/// One byte under the usual 256 NAME_MAX so a sanitised name never lands on a
/// filesystem that rejects the boundary itself.
pub const MAX_ATTACHMENT_NAME_LEN: usize = 255;

/// Sanitise a caller-supplied attachment filename (D-027b).
///
/// The name is metadata, not a trusted path: it must contain no path
/// separators (`/`, `\`), no control characters (including NUL), and is capped
/// at [`MAX_ATTACHMENT_NAME_LEN`] bytes. Leading/trailing whitespace and dots
/// are trimmed so a name cannot hide as a dotfile or trailing-dot junk.
/// Returns `None` when nothing usable remains — the caller then synthesises a
/// name rather than rejecting the upload.
#[must_use]
pub fn sanitize_attachment_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || c == '.');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(['/', '\\'])
        || trimmed.chars().any(char::is_control)
        || trimmed.len() > MAX_ATTACHMENT_NAME_LEN
    {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Conventional extension (without the dot) for a media type, used when no
/// original filename is available. Unknown types fall back to `bin`.
#[must_use]
pub fn extension_for_media_type(media_type: &str) -> &'static str {
    match media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/x-bzip2" => "bz2",
        "application/x-7z-compressed" => "7z",
        "application/x-xz" => "xz",
        "application/rtf" => "rtf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "bin",
    }
}

/// `instance.cancel` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCancelParams {
    /// Target Instance.
    pub instance_id: String,
    /// Command identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// Run identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// `instance.respond` / `interaction.respond` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRespondParams {
    /// Target Instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Pending interaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<String>,
    /// Command identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// Opaque answer payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
}

/// `tty.write` / `instance.keys` params (Hub command payload).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyWriteParams {
    /// Target Instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Command identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// Logical key names (`enter`, `esc`, `ctrl+c`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Optional PTY bytes (CLI also sends names in `keys`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// Caller (`cli`, `mcp`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl TtyWriteParams {
    /// Non-empty key names, or a single `raw` token when only `dataBase64` is set.
    #[must_use]
    pub fn key_names(&self) -> Vec<String> {
        let names: Vec<String> = self
            .keys
            .iter()
            .map(|key| key.trim())
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect();
        if !names.is_empty() {
            return names;
        }
        if self
            .data_base64
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            vec!["raw".to_owned()]
        } else {
            Vec::new()
        }
    }
}

/// `tty.resize` params (follow JSON and Hub→Node).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyResizeParams {
    /// Target Instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Stream identity when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// Columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    /// Rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    /// Optional revision; older values are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resize_revision: Option<Value>,
}

/// `tty.attach` params (Hub→Node / follow `tty=1`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyAttachParams {
    /// Target Instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// `read` or `write`. Write requires command permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Requested columns for the snapshot paint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    /// Requested rows for the snapshot paint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
}

/// Batched `journal.append` with an optional sequence watermark.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalAppendParams {
    /// Instance whose journal is appended.
    pub instance_id: String,
    /// Single-event form (pre-batch Hub).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
    /// Batched events. Preferred over `event` when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    /// Optional sequence hint (string or number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<Value>,
    /// Inclusive durable-seq watermark for this batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<JournalSeqWatermark>,
}

/// Sequence watermark carried on `journal.append` and heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JournalSeqWatermark {
    /// Journal identity when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_id: Option<String>,
    /// Instance identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Inclusive durable sequence (string or number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_seq: Option<Value>,
    /// Resume cursor (string or number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<Value>,
    /// Last acked seq as [`U64`] when the Node can emit a canonical string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<U64>,
}

/// JSON `tty.frame` control params. Binary frames use [`TtyBinaryEnvelopeSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyFrameParams {
    /// Instance that owns the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Stream identity (`tty_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    /// Channel byte (`1` = tty output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    /// Stream byte offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<Value>,
    /// Optional base64 payload when not using the binary envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

/// Observed renderer state for a bound stream; Node → Hub only.
///
/// Both observations are optional and independent: a mode edge carries
/// `altScreen` only, an `OSC 9;4` edge carries `progress` only, and the
/// attach notice may carry either or both.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyModeParams {
    /// Instance whose renderer produced the observation.
    pub instance_id: String,
    /// Authenticated, previously registered stream identity (`tty_…`).
    pub stream_id: String,
    /// Actual renderer screen state, never inferred from launch preference.
    /// Omitted on a progress-only edge; an explicit null is rejected — the
    /// "clear to unknown" reading exists only on the Hub's own attach notice.
    #[serde(
        default,
        deserialize_with = "missing_ok_null_rejected_bool",
        skip_serializing_if = "Option::is_none"
    )]
    pub alt_screen: Option<bool>,
    /// Parsed `OSC 9;4` progress for the terminal header bar, when the Node's
    /// emulator observed one this update (native-config, 2026-09-16).
    /// Additive: absent on older Nodes and on carriers without an emulator;
    /// clients must then leave the bar as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TtyProgress>,
}

/// ConEmu `OSC 9;4` progress state for the terminal header bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TtyProgressState {
    /// State 0: finished — the bar is hidden.
    Done,
    /// State 1: determinate percent.
    Percent,
    /// State 2: error.
    Error,
    /// State 3: indeterminate activity.
    Indeterminate,
    /// State 4: paused / warning.
    Paused,
}

/// Parsed `OSC 9;4` progress carried by a `tty.mode` notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyProgress {
    /// `done` / `percent` / `error` / `indeterminate` / `paused`.
    pub state: TtyProgressState,
    /// 0..=100 for determinate progress; senders frequently omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

/// Missing becomes `None`; an explicit `null` stays an error.
fn missing_ok_null_rejected_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(bool::deserialize(deserializer)?))
}

/// Fixed binary envelope for WS/stdio `tty.frame` (not JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TtyBinaryEnvelopeSpec {
    /// Byte 0; always `1` for v1.
    pub version: u8,
    /// Header size in bytes ([`TTY_BINARY_HEADER_LEN`]).
    pub header_len: u32,
    /// Channel byte for terminal output.
    pub tty_output_channel: u8,
    /// Channel byte for object chunks.
    pub object_chunk_channel: u8,
    /// Channel byte for terminal input (raw PTY bytes).
    pub tty_input_channel: u8,
    /// Byte layout of the 32-byte header.
    pub layout: String,
}

impl Default for TtyBinaryEnvelopeSpec {
    fn default() -> Self {
        Self::v1()
    }
}

impl TtyBinaryEnvelopeSpec {
    /// v1 envelope matching [`decode_binary_frame`].
    #[must_use]
    pub fn v1() -> Self {
        Self {
            version: 1,
            header_len: TTY_BINARY_HEADER_LEN as u32,
            tty_output_channel: BinaryChannel::TtyOutput as u8,
            object_chunk_channel: BinaryChannel::ObjectChunk as u8,
            tty_input_channel: BinaryChannel::TtyInput as u8,
            layout: "byte0=version(1) byte1=channel(1=output,2=object,3=input) bytes2-3=reserved(0) bytes4-19=streamUuidv7 bytes20-27=offsetBE u64 bytes28-31=payloadLenBE u32 payload".into(),
        }
    }
}

impl HubNodeMethod {
    /// Wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NodeAuth => METHOD_NODE_AUTH,
            Self::NodeHello => METHOD_NODE_HELLO,
            Self::RuntimeHello => METHOD_RUNTIME_HELLO,
            Self::NodeHeartbeat => METHOD_NODE_HEARTBEAT,
            Self::RuntimeHeartbeat => METHOD_RUNTIME_HEARTBEAT,
            Self::WorkspaceList => METHOD_WORKSPACE_LIST,
            Self::WorkspaceRegister => METHOD_WORKSPACE_REGISTER,
            Self::WorkspaceUnregister => METHOD_WORKSPACE_UNREGISTER,
            Self::WorkerProvision => METHOD_WORKER_PROVISION,
            Self::WorkerRemove => METHOD_WORKER_REMOVE,
            Self::InstanceCreate => METHOD_INSTANCE_CREATE,
            Self::InstanceResume => METHOD_INSTANCE_RESUME,
            Self::InstanceSend => METHOD_INSTANCE_SEND,
            Self::InstanceConfigure => METHOD_INSTANCE_CONFIGURE,
            Self::InstanceCancel => METHOD_INSTANCE_CANCEL,
            Self::InstanceRespond => METHOD_INSTANCE_RESPOND,
            Self::InteractionRespond => METHOD_INTERACTION_RESPOND,
            Self::JournalAppend => METHOD_JOURNAL_APPEND,
            Self::TtyFrame => METHOD_TTY_FRAME,
            Self::TtyMode => METHOD_TTY_MODE,
            Self::TtyWrite => METHOD_TTY_WRITE,
            Self::InstanceKeys => METHOD_INSTANCE_KEYS,
            Self::TtyResize => METHOD_TTY_RESIZE,
            Self::TtyAttach => METHOD_TTY_ATTACH,
            Self::TtyScreen => METHOD_TTY_SCREEN,
        }
    }

    /// Parse a method string. Unknown methods return `None`.
    #[must_use]
    pub fn parse(method: &str) -> Option<Self> {
        Some(match method {
            METHOD_NODE_AUTH => Self::NodeAuth,
            METHOD_NODE_HELLO => Self::NodeHello,
            METHOD_RUNTIME_HELLO => Self::RuntimeHello,
            METHOD_NODE_HEARTBEAT => Self::NodeHeartbeat,
            METHOD_RUNTIME_HEARTBEAT => Self::RuntimeHeartbeat,
            METHOD_WORKSPACE_LIST => Self::WorkspaceList,
            METHOD_WORKSPACE_REGISTER => Self::WorkspaceRegister,
            METHOD_WORKSPACE_UNREGISTER => Self::WorkspaceUnregister,
            METHOD_WORKER_PROVISION => Self::WorkerProvision,
            METHOD_WORKER_REMOVE => Self::WorkerRemove,
            METHOD_INSTANCE_CREATE => Self::InstanceCreate,
            METHOD_INSTANCE_RESUME => Self::InstanceResume,
            METHOD_INSTANCE_SEND => Self::InstanceSend,
            METHOD_INSTANCE_CONFIGURE => Self::InstanceConfigure,
            METHOD_INSTANCE_CANCEL => Self::InstanceCancel,
            METHOD_INSTANCE_RESPOND => Self::InstanceRespond,
            METHOD_INTERACTION_RESPOND => Self::InteractionRespond,
            METHOD_JOURNAL_APPEND => Self::JournalAppend,
            METHOD_TTY_FRAME => Self::TtyFrame,
            METHOD_TTY_MODE => Self::TtyMode,
            METHOD_TTY_WRITE => Self::TtyWrite,
            METHOD_INSTANCE_KEYS => Self::InstanceKeys,
            METHOD_TTY_RESIZE => Self::TtyResize,
            METHOD_TTY_ATTACH => Self::TtyAttach,
            METHOD_TTY_SCREEN => Self::TtyScreen,
            _ => return None,
        })
    }

    /// Hello (including `runtime.hello` alias).
    #[must_use]
    pub fn is_hello(self) -> bool {
        matches!(self, Self::NodeHello | Self::RuntimeHello)
    }

    /// Stdio auth frame.
    #[must_use]
    pub fn is_auth(self) -> bool {
        matches!(self, Self::NodeAuth)
    }

    /// Instance control methods dispatched by the Node runtime.
    #[must_use]
    pub fn is_instance(self) -> bool {
        matches!(
            self,
            Self::InstanceCreate
                | Self::InstanceSend
                | Self::InstanceConfigure
                | Self::InstanceCancel
                | Self::InstanceRespond
                | Self::InteractionRespond
                | Self::TtyWrite
                | Self::InstanceKeys
                | Self::TtyResize
                | Self::TtyAttach
                | Self::TtyScreen
        )
    }
}

impl HubNodeRequest {
    /// Build a request with [`PROTOCOL_VERSION`] on the frame.
    #[must_use]
    pub fn call(id: impl Into<Value>, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2,
            id: Some(id.into()),
            version: Some(PROTOCOL_VERSION),
            method: method.to_owned(),
            params: Some(params),
        }
    }

    /// Build a notification (no id).
    #[must_use]
    pub fn notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2,
            id: None,
            version: Some(PROTOCOL_VERSION),
            method: method.to_owned(),
            params: Some(params),
        }
    }

    /// Decode from a JSON value without requiring unknown fields to fail.
    pub fn from_value(value: &Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value.clone())
    }

    /// Known method, if this frame uses one of the Hub↔Node names.
    #[must_use]
    pub fn method_kind(&self) -> Option<HubNodeMethod> {
        HubNodeMethod::parse(&self.method)
    }
}

impl HubNodeResponse {
    /// Successful result with [`PROTOCOL_VERSION`].
    #[must_use]
    pub fn ok(id: impl Into<Value>, result: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2,
            id: id.into(),
            version: Some(PROTOCOL_VERSION),
            result: Some(result),
            error: None,
        }
    }

    /// JSON-RPC error response.
    #[must_use]
    pub fn err(id: impl Into<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2,
            id: id.into(),
            version: Some(PROTOCOL_VERSION),
            result: None,
            error: Some(JsonRpcErrorObject {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

impl NodeHelloParams {
    /// Host id from the params root or nested `host.hostId`.
    #[must_use]
    pub fn persisted_host_id(&self) -> Option<&str> {
        self.host_id
            .as_deref()
            .or_else(|| self.host.as_ref().and_then(|host| host.host_id.as_deref()))
    }
}

impl JournalAppendParams {
    /// Events to append: `events` when non-empty, else the single `event`.
    #[must_use]
    pub fn events_to_append(&self) -> Vec<Value> {
        if !self.events.is_empty() {
            return self.events.clone();
        }
        self.event.clone().into_iter().collect()
    }

    /// Sequence hint as i64.
    #[must_use]
    pub fn seq_i64(&self) -> Option<i64> {
        self.seq.as_ref().and_then(value_as_i64)
    }
}

impl JournalSeqWatermark {
    /// Durable sequence as i64.
    #[must_use]
    pub fn durable_i64(&self) -> Option<i64> {
        self.durable_seq
            .as_ref()
            .and_then(value_as_i64)
            .or_else(|| self.seq.map(|seq| seq.0 as i64))
    }
}

impl InstanceSendParams {
    /// Prompt text from `input.text`, `input` string, or `prompt`.
    #[must_use]
    pub fn prompt_text(&self) -> Option<&str> {
        self.input
            .as_ref()
            .and_then(|input| {
                input
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| input.as_str())
            })
            .or(self.prompt.as_deref())
    }

    /// Staged attachments from the flattened field or an `input.attachments`
    /// wrapper, whichever the caller used.
    #[must_use]
    pub fn attachments(&self) -> Vec<AttachmentRef> {
        if !self.attachments.is_empty() {
            return self.attachments.clone();
        }
        self.input
            .as_ref()
            .and_then(|input| input.get("attachments"))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }
}

/// Decode a v1 binary tty/object envelope.
pub fn decode_tty_binary_frame(
    frame: &[u8],
    max_payload: u32,
) -> Result<(BinaryHeader, &[u8]), BinaryFrameError> {
    decode_binary_frame(frame, max_payload)
}

/// Extract the token from `Authorization: Bearer …`.
#[must_use]
pub fn bearer_from_authorization(header: &str) -> Option<&str> {
    let rest = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?;
    let rest = rest.trim();
    if rest.is_empty() { None } else { Some(rest) }
}

/// JSON-RPC request object with protocol `version`.
#[must_use]
pub fn rpc_request(id: impl Into<Value>, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.into(),
        "method": method,
        "version": PROTOCOL_VERSION,
        "params": params,
    })
}

/// JSON-RPC success object with protocol `version`.
#[must_use]
pub fn rpc_ok(id: impl Into<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.into(),
        "version": PROTOCOL_VERSION,
        "result": result,
    })
}

/// JSON-RPC error object with protocol `version`.
#[must_use]
pub fn rpc_error(id: impl Into<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.into(),
        "version": PROTOCOL_VERSION,
        "error": { "code": code, "message": message },
    })
}

/// Parse a JSON number or decimal string as i64.
#[must_use]
pub fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trip_lifts_nested_host_id() {
        let params = NodeHelloParams {
            host_id: None,
            label: Some("devbox-sg".into()),
            node_version: Some("0.1.0".into()),
            node_epoch: None,
            enrollment_token: Some("tok".into()),
            transport: Some("ssh-stdio".into()),
            version: Some(PROTOCOL_VERSION),
            protocol: None,
            host: Some(NodeHostInventory {
                host_id: Some("hst_01993ab0-0000-7000-8000-000000000004".into()),
                workspaces: None,
                workspace_revision: None,
                hostname: Some("devbox".into()),
                labels: Some(json!({"region":"sg"})),
                max_instances: Some(8),
                cli: None,
                herdr: None,
                resources: None,
                os: None,
                kernel: None,
                libc: None,
                label: None,
            }),
            capabilities: None,
            cli: None,
        };
        assert_eq!(
            params.persisted_host_id(),
            Some("hst_01993ab0-0000-7000-8000-000000000004")
        );
        let req = HubNodeRequest::call("hello-1", METHOD_NODE_HELLO, json!(params));
        let value = serde_json::to_value(&req).expect("ser");
        assert_eq!(value["version"]["major"], 1);
        assert_eq!(value["jsonrpc"], "2.0");
        let decoded = HubNodeRequest::from_value(&value).expect("de");
        assert_eq!(decoded.method_kind(), Some(HubNodeMethod::NodeHello));
    }

    /// D-027: the field is additive, so a legacy frame with no `attachments`
    /// decodes cleanly and reports none.
    #[test]
    fn send_params_without_attachments_decode_as_text_only() {
        let params: InstanceSendParams =
            serde_json::from_value(json!({"instanceId":"ins_1", "prompt":"hi"})).expect("de");
        assert!(params.attachments().is_empty());
        assert_eq!(params.prompt_text(), Some("hi"));
        // An empty list must not appear on the wire either.
        let value = serde_json::to_value(&params).expect("ser");
        assert!(value.get("attachments").is_none());
    }

    /// Attachment metadata survives a round trip and is also accepted inside
    /// the `input` wrapper, matching how `prompt_text` reads both shapes.
    #[test]
    fn send_params_carry_attachment_metadata() {
        let flat: InstanceSendParams = serde_json::from_value(json!({
            "instanceId": "ins_1",
            "prompt": "what colour is this?",
            "attachments": [{
                "objectId": "obj_1",
                "mediaType": "image/png",
                "name": "shot.png",
                "size": 1234,
            }],
        }))
        .expect("de");
        let refs = flat.attachments();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].object_id, "obj_1");
        assert_eq!(refs[0].media_type, "image/png");
        assert_eq!(refs[0].size, Some(1234));
        assert_eq!(
            serde_json::to_value(&flat).expect("ser")["attachments"][0]["objectId"],
            "obj_1"
        );

        let wrapped: InstanceSendParams = serde_json::from_value(json!({
            "instanceId": "ins_1",
            "input": {"text":"hi", "attachments":[{"objectId":"obj_2", "mediaType":"image/jpeg"}]},
        }))
        .expect("de");
        assert_eq!(wrapped.attachments()[0].object_id, "obj_2");
        assert_eq!(wrapped.attachments()[0].name, None);
    }

    #[test]
    fn attachment_kind_defaults_to_image_and_classifies_files() {
        assert_eq!(AttachmentKind::default(), AttachmentKind::Image);
        assert_eq!(
            AttachmentKind::from_media_type("image/png"),
            AttachmentKind::Image
        );
        assert_eq!(
            AttachmentKind::from_media_type("image/jpeg; charset=binary"),
            AttachmentKind::Image
        );
        assert_eq!(
            AttachmentKind::from_media_type("application/pdf"),
            AttachmentKind::File
        );
        assert_eq!(AttachmentKind::from_media_type(""), AttachmentKind::File);
        let legacy: AttachmentRef =
            serde_json::from_value(json!({"objectId":"o","mediaType":"image/png"}))
                .expect("legacy frame");
        assert_eq!(legacy.kind, AttachmentKind::Image);
    }

    #[test]
    fn filename_sanitisation_rejects_paths_controls_and_oversize() {
        assert_eq!(
            sanitize_attachment_name("report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            sanitize_attachment_name("  季度报告 Q3.pdf  ").as_deref(),
            Some("季度报告 Q3.pdf")
        );
        assert_eq!(sanitize_attachment_name("../etc/passwd"), None);
        assert_eq!(sanitize_attachment_name("a\\b.txt"), None);
        assert_eq!(sanitize_attachment_name("nul\0.txt"), None);
        assert_eq!(sanitize_attachment_name("line\nbreak.txt"), None);
        assert_eq!(sanitize_attachment_name("   "), None);
        assert_eq!(sanitize_attachment_name("..."), None);
        assert_eq!(sanitize_attachment_name(&"a".repeat(256)), None);
        let max_name = "a".repeat(255);
        assert_eq!(
            sanitize_attachment_name(&max_name).as_deref(),
            Some(max_name.as_str())
        );
    }

    #[test]
    fn extensions_map_for_known_and_unknown_types() {
        assert_eq!(extension_for_media_type("application/pdf"), "pdf");
        assert_eq!(extension_for_media_type("text/plain; charset=utf-8"), "txt");
        assert_eq!(extension_for_media_type("application/zip"), "zip");
        assert_eq!(extension_for_media_type("image/png"), "png");
        assert_eq!(extension_for_media_type("application/x-weird-blob"), "bin");
        assert_eq!(extension_for_media_type(""), "bin");
    }

    #[test]
    fn journal_prefers_events_batch() {
        let params = JournalAppendParams {
            instance_id: "ins_1".into(),
            event: Some(json!({"kind":"old"})),
            events: vec![json!({"kind":"a"}), json!({"kind":"b"})],
            seq: Some(json!("3")),
            watermark: Some(JournalSeqWatermark {
                journal_id: None,
                instance_id: Some("ins_1".into()),
                durable_seq: Some(json!("3")),
                after_seq: None,
                seq: None,
            }),
        };
        assert_eq!(params.events_to_append().len(), 2);
        assert_eq!(params.seq_i64(), Some(3));
        assert_eq!(
            params.watermark.as_ref().and_then(|w| w.durable_i64()),
            Some(3)
        );
    }

    #[test]
    fn tty_spec_matches_binary_header() {
        let spec = TtyBinaryEnvelopeSpec::v1();
        assert_eq!(spec.header_len as usize, BINARY_HEADER_LEN);
        assert_eq!(spec.tty_output_channel, BinaryChannel::TtyOutput as u8);
        assert_eq!(spec.tty_input_channel, BinaryChannel::TtyInput as u8);
    }

    #[test]
    fn bearer_strips_scheme() {
        assert_eq!(bearer_from_authorization("Bearer abc"), Some("abc"));
        assert_eq!(bearer_from_authorization("Bearer "), None);
    }

    #[test]
    fn node_auth_params_debug_redacts_token() {
        let params = NodeAuthParams {
            token: "host-secret".into(),
            scheme: Some("bearer".into()),
        };
        let rendered = format!("{params:?}");
        assert!(!rendered.contains("host-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
