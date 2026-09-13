//! Request params and success payloads for the typed Herdr subset.
//!
//! Field names follow `docs/research/cli-help/herdr-api-schema.json` (protocol 22).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Agent lifecycle as reported by Herdr (screen detection + hooks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Ready for input.
    Idle,
    /// Turn in progress.
    Working,
    /// Waiting on a prompt / permission dialog.
    Blocked,
    /// Idle and unseen (`done` is derived, not a detector state).
    Done,
    /// Detector did not produce a state.
    Unknown,
}

/// `pane.read` / `agent.read` buffer selection. Socket uses underscores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    /// Currently visible viewport.
    Visible,
    /// Recent scrollback.
    Recent,
    /// Recent scrollback without wrapping.
    RecentUnwrapped,
    /// Text the detector uses.
    Detection,
}

/// Text vs ANSI for `pane.read` / `agent.read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFormat {
    /// Strip SGR (Herdr default).
    #[default]
    Text,
    /// Keep ANSI.
    Ansi,
}

/// Split direction for `pane.split`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    /// Split to the right.
    Right,
    /// Split below.
    Down,
}

/// `agent_session.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    /// Native session id (Claude UUID, …).
    Id,
    /// Filesystem path (pi / omp).
    Path,
}

/// Native session identity Herdr received from a hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    /// Integration source (`herdr:claude`, …).
    pub source: String,
    /// Agent kind string.
    pub agent: String,
    /// Id vs path.
    pub kind: AgentSessionRefKind,
    /// Session UUID or path.
    pub value: String,
}

/// Empty params object. Herdr requires `params` even when unused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyParams {}

/// `workspace.create` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    /// Initial cwd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra env for the root pane.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Focus the new workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
    /// Sidebar label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Copy cwd policy from this workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_workspace_id: Option<String>,
}

/// `tab.create` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCreateParams {
    /// Parent workspace; omitted = current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Initial cwd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra env.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Focus the new tab.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
    /// Tab label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `tab.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabListParams {
    /// Filter to this workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// `tab.close` / `tab.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabTarget {
    /// Tab id (`w1:t1`).
    pub tab_id: String,
}

/// `pane.split` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSplitParams {
    /// Split direction.
    pub direction: SplitDirection,
    /// Workspace of the target pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Pane to split; omitted = focused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    /// Split ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f32>,
    /// New pane cwd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra env for the new pane.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Focus the new pane.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focus: bool,
}

/// `pane.close` / `pane.get` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTarget {
    /// Pane id (`w1:p1`).
    pub pane_id: String,
}

/// `pane.read` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadParams {
    /// Pane id.
    pub pane_id: String,
    /// Buffer to read.
    pub source: ReadSource,
    /// Optional line cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// Text vs ANSI.
    #[serde(default, skip_serializing_if = "is_default_read_format")]
    pub format: ReadFormat,
    /// Strip SGR when `format=text`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub strip_ansi: bool,
}

/// `pane.send_text` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSendTextParams {
    /// Pane id.
    pub pane_id: String,
    /// Literal bytes as UTF-8 text (no trailing Enter unless included).
    pub text: String,
}

/// `pane.send_keys` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSendKeysParams {
    /// Pane id.
    pub pane_id: String,
    /// Key names (`enter`, `down`, `ctrl+c`, …).
    pub keys: Vec<String>,
}

/// `pane.process_info` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProcessInfoParams {
    /// Pane id; omitted = focused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// `agent.start` params. Trailing CLI args go in `args` (Herdr's `--` list).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStartParams {
    /// Display name unique in the session.
    pub name: String,
    /// Kind (`claude`, `codex`, `grok`, …).
    pub kind: String,
    /// Pane that will host the process.
    pub pane_id: String,
    /// Extra argv after the binary (not persisted by Herdr — Remuda must store these).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Startup timeout in ms (Herdr: >3000 and ≤300000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `agent.prompt` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPromptParams {
    /// Agent name or pane id.
    pub target: String,
    /// Prompt text.
    pub text: String,
    /// Optional wait-after-submit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<AgentWaitSpec>,
}

/// Shared wait options for `agent.prompt.wait` and `agent.wait`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWaitSpec {
    /// Statuses that complete the wait.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<AgentStatus>,
    /// Timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `agent.wait` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWaitParams {
    /// Agent name or pane id.
    pub target: String,
    /// Statuses that complete the wait.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<AgentStatus>,
    /// Timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `agent.get` / `agent.read` / `agent.send_keys` target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTarget {
    /// Agent name or pane id.
    pub target: String,
}

/// `agent.read` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReadParams {
    /// Agent name or pane id.
    pub target: String,
    /// Buffer to read.
    pub source: ReadSource,
    /// Optional line cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// Text vs ANSI.
    #[serde(default, skip_serializing_if = "is_default_read_format")]
    pub format: ReadFormat,
    /// Strip SGR when `format=text`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub strip_ansi: bool,
}

/// `agent.send_keys` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSendKeysParams {
    /// Agent name or pane id.
    pub target: String,
    /// Key names.
    pub keys: Vec<String>,
}

/// `ping` success (`type=pong`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Herdr version (`0.9.0`).
    pub version: String,
    /// Internal protocol number (`22`).
    pub protocol: u32,
    /// Advertised server capabilities.
    #[serde(default)]
    pub capabilities: Option<ServerCapabilities>,
}

/// Subset of `ping` capabilities we care about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Live TUI handoff.
    #[serde(default)]
    pub live_handoff: bool,
    /// Detached daemon mode.
    #[serde(default)]
    pub detached_server_daemon: bool,
    /// Endpoint generation.
    #[serde(default)]
    pub endpoint_protocol_generation: Option<u32>,
    /// Client-shell surface interest.
    #[serde(default)]
    pub surface_interest: bool,
    /// Health probes.
    #[serde(default)]
    pub health_check: bool,
}

/// Workspace row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Workspace id (`w1`).
    pub workspace_id: String,
    /// Sidebar number.
    #[serde(default)]
    pub number: usize,
    /// Label.
    #[serde(default)]
    pub label: String,
    /// Focused.
    #[serde(default)]
    pub focused: bool,
    /// Pane count.
    #[serde(default)]
    pub pane_count: usize,
    /// Tab count.
    #[serde(default)]
    pub tab_count: usize,
    /// Active tab id.
    #[serde(default)]
    pub active_tab_id: String,
    /// Aggregated agent status.
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
}

/// Tab row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    /// Tab id.
    pub tab_id: String,
    /// Parent workspace.
    pub workspace_id: String,
    /// Order.
    #[serde(default)]
    pub number: usize,
    /// Label.
    #[serde(default)]
    pub label: String,
    /// Focused.
    #[serde(default)]
    pub focused: bool,
    /// Pane count.
    #[serde(default)]
    pub pane_count: usize,
    /// Aggregated agent status.
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
}

/// Pane row (subset used by the typed client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Pane id.
    pub pane_id: String,
    /// Terminal id.
    #[serde(default)]
    pub terminal_id: String,
    /// Workspace id.
    pub workspace_id: String,
    /// Tab id.
    pub tab_id: String,
    /// Focused.
    #[serde(default)]
    pub focused: bool,
    /// Cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Label.
    #[serde(default)]
    pub label: Option<String>,
    /// Detected agent kind.
    #[serde(default)]
    pub agent: Option<String>,
    /// Agent status.
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
    /// OSC title.
    #[serde(default)]
    pub terminal_title: Option<String>,
    /// Title without spinner prefix.
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    /// Content revision.
    #[serde(default)]
    pub revision: u64,
    /// Viewport geometry if present.
    #[serde(default)]
    pub scroll: Option<PaneScrollInfo>,
}

/// Scroll / viewport snapshot on a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneScrollInfo {
    /// Offset from bottom.
    #[serde(default)]
    pub offset_from_bottom: u64,
    /// Max offset.
    #[serde(default)]
    pub max_offset_from_bottom: u64,
    /// Visible rows (headless still reports a real value).
    #[serde(default)]
    pub viewport_rows: u64,
}

/// Agent row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Terminal id.
    #[serde(default)]
    pub terminal_id: String,
    /// Assigned name.
    #[serde(default)]
    pub name: Option<String>,
    /// Kind (`claude`, …).
    #[serde(default)]
    pub agent: Option<String>,
    /// Status.
    #[serde(default = "unknown_status")]
    pub agent_status: AgentStatus,
    /// Workspace id.
    #[serde(default)]
    pub workspace_id: String,
    /// Tab id.
    #[serde(default)]
    pub tab_id: String,
    /// Pane id.
    pub pane_id: String,
    /// Focused.
    #[serde(default)]
    pub focused: bool,
    /// Native session (Claude UUID, …).
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    /// Ready for `agent.prompt`.
    #[serde(default)]
    pub interactive_ready: bool,
    /// Still in `agent.start`.
    #[serde(default)]
    pub launch_pending: bool,
    /// Cwd.
    #[serde(default)]
    pub cwd: Option<String>,
    /// OSC title.
    #[serde(default)]
    pub terminal_title: Option<String>,
    /// Title without spinner.
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    /// Detector sequence.
    #[serde(default)]
    pub state_change_seq: u64,
    /// Content revision.
    #[serde(default)]
    pub revision: u64,
}

/// `workspace.create` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreated {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// New workspace.
    pub workspace: WorkspaceInfo,
    /// Root tab.
    pub tab: TabInfo,
    /// Root pane (has real geometry even headless).
    pub root_pane: PaneInfo,
}

/// `workspace.list` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceList {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Workspaces.
    pub workspaces: Vec<WorkspaceInfo>,
}

/// `tab.create` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCreated {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// New tab.
    pub tab: TabInfo,
    /// Root pane of the tab.
    pub root_pane: PaneInfo,
}

/// `tab.list` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabList {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Tabs.
    pub tabs: Vec<TabInfo>,
}

/// Generic ok result (`pane.close`, `tab.close`, `pane.send_text`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResult {
    /// Result discriminant (`ok`).
    #[serde(rename = "type")]
    pub kind: String,
}

/// `pane.split` / `pane.info` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfoResult {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Pane.
    pub pane: PaneInfo,
}

/// `pane.read` / `agent.read` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadResult {
    /// Result discriminant (`pane_read`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Nested read payload (Herdr wraps it as `read` on some variants).
    #[serde(default)]
    pub read: Option<PaneReadBody>,
    /// Flattened pane id when Herdr inlines the body.
    #[serde(default)]
    pub pane_id: Option<String>,
    /// Flattened text.
    #[serde(default)]
    pub text: Option<String>,
    /// Flattened truncated flag.
    #[serde(default)]
    pub truncated: Option<bool>,
}

/// Inner `pane.read` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadBody {
    /// Pane id.
    pub pane_id: String,
    /// Workspace id.
    #[serde(default)]
    pub workspace_id: String,
    /// Tab id.
    #[serde(default)]
    pub tab_id: String,
    /// Source.
    pub source: ReadSource,
    /// Format.
    #[serde(default)]
    pub format: ReadFormat,
    /// Screen text.
    pub text: String,
    /// Revision.
    #[serde(default)]
    pub revision: u64,
    /// Truncated.
    #[serde(default)]
    pub truncated: bool,
}

impl PaneReadResult {
    /// Screen text, whether nested or inlined.
    #[must_use]
    pub fn text(&self) -> &str {
        self.read
            .as_ref()
            .map(|body| body.text.as_str())
            .or(self.text.as_deref())
            .unwrap_or("")
    }
}

/// `pane.process_info` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProcessInfoResult {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Nested process info.
    #[serde(default)]
    pub process_info: Option<PaneProcessInfo>,
}

/// Process-tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProcessInfo {
    /// Pane id.
    pub pane_id: String,
    /// Shell pid.
    #[serde(default)]
    pub shell_pid: Option<u32>,
    /// Foreground process group; a ready shell owns its own group.
    #[serde(default)]
    pub foreground_process_group_id: Option<u32>,
    /// Foreground processes.
    #[serde(default)]
    pub foreground_processes: Vec<PaneProcessInfoProcess>,
}

/// One process in `pane.process_info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProcessInfoProcess {
    /// Pid.
    pub pid: u32,
    /// Process name.
    pub name: String,
    /// argv0.
    #[serde(default)]
    pub argv0: Option<String>,
    /// Full argv.
    #[serde(default)]
    pub argv: Option<Vec<String>>,
}

/// `agent.start` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStarted {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Agent.
    pub agent: AgentInfo,
    /// Argv Herdr actually executed (does **not** persist across restore).
    #[serde(default)]
    pub argv: Vec<String>,
}

/// `agent.get` / `agent.wait` / `agent.prompt` success that carries an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfoResult {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Agent.
    pub agent: AgentInfo,
}

/// `agent.list` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentList {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Agents.
    pub agents: Vec<AgentInfo>,
}

/// `session.snapshot` success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshotResult {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
    /// Snapshot body.
    pub snapshot: SessionSnapshot,
}

/// One-shot bootstrap of the whole session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Herdr version.
    pub version: String,
    /// Protocol number.
    pub protocol: u32,
    /// Focused workspace.
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    /// Focused tab.
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    /// Focused pane.
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    /// Workspaces.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    /// Tabs.
    #[serde(default)]
    pub tabs: Vec<TabInfo>,
    /// Panes.
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    /// Agents.
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
}

/// `events.subscribe` ack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionStarted {
    /// Result discriminant.
    #[serde(rename = "type")]
    pub kind: String,
}

/// Where a named Herdr session keeps its API socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPaths {
    /// `herdr.sock` (JSON-RPC).
    pub api: PathBuf,
    /// `herdr-client.sock` (private binary protocol; terminal observe uses this via CLI).
    pub client: PathBuf,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_default_read_format(format: &ReadFormat) -> bool {
    matches!(format, ReadFormat::Text)
}

fn unknown_status() -> AgentStatus {
    AgentStatus::Unknown
}
