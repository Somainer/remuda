//! Listen, data-dir, and cookie policy.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use uuid::Uuid;

/// Cookie carrying the device session token.
pub const DEVICE_COOKIE: &str = "remuda_device";
/// Default deadline for the Node to durably accept a command.
pub const DEFAULT_COMMAND_ACCEPT_TIMEOUT_MS: u64 = 5_000;
/// Minimum create settlement deadline; native cold starts must fit inside it.
pub const MIN_CREATE_SETTLE_TIMEOUT_MS: u64 = 120_000;
/// Default ack deadline for a forwarded non-create command before it fails.
pub const DEFAULT_COMMAND_SETTLE_TIMEOUT_MS: u64 = 10_000;
/// Default lifetime of the bootstrap device access code (D-018).
pub const DEFAULT_BOOTSTRAP_TTL_HOURS: u64 = 24;
/// Default lifetime of a minted node enroll token (D-018).
pub const DEFAULT_ENROLL_TOKEN_TTL_MINUTES: u64 = 60;
/// Default per-file attachment staging ceiling (D-027b): 25 MiB.
pub const DEFAULT_ATTACHMENT_MAX_BYTES: usize = 25 * 1024 * 1024;

/// How Hub binds and authenticates.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubConfig {
    /// Hub-supervised SSH executable and optional upload artifact.
    #[serde(default)]
    pub ssh_hosts: crate::ssh_hosts::SshHostOptions,
    /// SQLite, bootstrap-token file, and blob cache root.
    pub data_dir: PathBuf,
    /// HTTP/WS bind address.
    pub listen: SocketAddr,
    /// Device pairing access code. Empty means generate. Never enrolls a Node (D-018).
    pub bootstrap_token: String,
    /// Bootstrap access-code lifetime in hours; `0` disables expiry.
    #[serde(default = "default_bootstrap_ttl_hours")]
    pub bootstrap_ttl_hours: u64,
    /// Lifetime of a minted node enroll token, in minutes.
    #[serde(default = "default_enroll_token_ttl_minutes")]
    pub enroll_token_ttl_minutes: u64,
    /// Set the `Secure` flag on the device cookie (production HTTPS).
    pub cookie_secure: bool,
    /// Canonical externally visible HTTP(S) origin, without a path.
    #[serde(default)]
    pub public_origin: Option<String>,
    /// Exact immediate TCP peers permitted to supply forwarding headers.
    #[serde(default)]
    pub trusted_proxies: Vec<IpAddr>,
    /// Extra allowed `Origin` values. Empty means same-origin with `Host`.
    pub allowed_origins: Vec<String>,
    /// Optional on-disk `web/dist` override used before the embedded assets.
    pub web_root: Option<PathBuf>,
    /// How long `waiting-interaction` must last before a blocked push (ms).
    #[serde(default = "default_push_block_ms")]
    pub push_block_ms: u64,
    /// Per-follow-socket outbound queue. Overflow emits `{type:gap}` and a resync snapshot.
    #[serde(default = "default_follow_buffer_events")]
    pub follow_buffer_events: usize,
    /// Deadline for the Node's durable command-accept response (ms).
    #[serde(default = "default_command_accept_timeout_ms")]
    pub command_accept_timeout_ms: u64,
    /// Deadline for a later create settlement observation (ms, minimum 120 seconds).
    #[serde(default = "default_create_settle_timeout_ms")]
    pub create_settle_timeout_ms: u64,
    /// Bounded ack deadline for a forwarded non-create command (send/steer/etc).
    /// If neither an accept nor an error reply arrives, the row moves to
    /// `failed` with a reason instead of sitting at `queued` forever (default
    /// ten seconds). Config key `commandSettleTimeoutMs`.
    #[serde(default = "default_command_settle_timeout_ms")]
    pub command_settle_timeout_ms: u64,
    /// Offline grace before stale instances exit with reason host-lost (default ten minutes).
    #[serde(default = "default_host_lost_grace_ms")]
    pub host_lost_grace_ms: u64,
    /// How long a `requested` instance may wait for a Node receipt before it
    /// expires to `failed` and stops holding a placement slot (default five minutes).
    #[serde(default = "default_requested_grace_ms")]
    pub requested_grace_ms: u64,
    /// How long a passing-but-unlanded gate verify keeps its pinned merge refs
    /// on the lane host before retention drops them (default seven days).
    #[serde(default = "default_gate_ref_retention_ms")]
    pub gate_ref_retention_ms: u64,
    /// Grace after a gate cancel is requested before the scheduler finishes a
    /// `canceling` job `canceled` on its own, even if the lane never answered
    /// `gate.cancel` (default 30 seconds). A late run reply after this is
    /// ignored rather than applied.
    #[serde(default = "default_gate_cancel_grace_ms")]
    pub gate_cancel_grace_ms: u64,
    /// Per-IP authentication attempt burst budget (login/pair/passkey).
    #[serde(default = "default_auth_ip_burst")]
    pub auth_ip_burst: f64,
    /// Per-IP authentication token refill (tokens/second).
    #[serde(default = "default_auth_ip_refill_per_sec")]
    pub auth_ip_refill_per_sec: f64,
    /// Global authentication attempt burst budget.
    #[serde(default = "default_auth_global_burst")]
    pub auth_global_burst: f64,
    /// Global authentication token refill (tokens/second).
    #[serde(default = "default_auth_global_refill_per_sec")]
    pub auth_global_refill_per_sec: f64,
    /// Per-file attachment staging ceiling in bytes (D-027b). The browser and
    /// Node quote the same value; default 25 MiB covers PDFs and small archives
    /// while bounding the SQLite blob store. Config key `attachmentMaxBytes`.
    #[serde(default = "default_attachment_max_bytes")]
    pub attachment_max_bytes: usize,
    /// Placement only trusts a CPU/mem sample younger than this (default 60 s).
    /// A persisted sample at or beyond this age triggers a bounded
    /// `host.resources` refresh before the host may be refused as saturated.
    #[serde(default = "default_resource_sample_max_age_ms")]
    pub resource_sample_max_age_ms: u64,
    /// Bounded wait for an on-demand `host.resources` refresh before placement
    /// decides on the data it has (default one second).
    #[serde(default = "default_resource_refresh_timeout_ms")]
    pub resource_refresh_timeout_ms: u64,
}

/// Default per-file attachment ceiling (D-027b): 25 MiB.
pub fn default_attachment_max_bytes() -> usize {
    DEFAULT_ATTACHMENT_MAX_BYTES
}

/// Placement trusts resource samples for 60 s; the Node heartbeat refreshes
/// them every 15 s, so a healthy link always has a fresh sample on hand.
pub fn default_resource_sample_max_age_ms() -> u64 {
    60_000
}

/// A `host.resources` on-demand refresh must return within ~1 s; refusal
/// latency stays bounded when a Node is wedged.
pub fn default_resource_refresh_timeout_ms() -> u64 {
    1_000
}

fn default_auth_ip_burst() -> f64 {
    10.0
}

fn default_auth_ip_refill_per_sec() -> f64 {
    0.1
}

fn default_auth_global_burst() -> f64 {
    64.0
}

fn default_auth_global_refill_per_sec() -> f64 {
    8.0
}

fn default_host_lost_grace_ms() -> u64 {
    600_000
}

fn default_requested_grace_ms() -> u64 {
    crate::store::REQUESTED_SLOT_WINDOW_MS
}

/// Seven days: long enough that an operator can land a passing verify by hand
/// after a weekend, short enough that abandoned merges do not accumulate.
fn default_gate_ref_retention_ms() -> u64 {
    7 * 24 * 60 * 60 * 1000
}

/// Thirty seconds: long enough for a well-behaved lane to kill its step group
/// and report `canceled`, short enough that a parked or dead lane cannot hold
/// the outcome hostage.
fn default_gate_cancel_grace_ms() -> u64 {
    30 * 1000
}

fn default_push_block_ms() -> u64 {
    30_000
}

fn default_bootstrap_ttl_hours() -> u64 {
    DEFAULT_BOOTSTRAP_TTL_HOURS
}

fn default_enroll_token_ttl_minutes() -> u64 {
    DEFAULT_ENROLL_TOKEN_TTL_MINUTES
}

fn default_follow_buffer_events() -> usize {
    256
}

fn default_command_accept_timeout_ms() -> u64 {
    DEFAULT_COMMAND_ACCEPT_TIMEOUT_MS
}

fn default_create_settle_timeout_ms() -> u64 {
    MIN_CREATE_SETTLE_TIMEOUT_MS
}

fn default_command_settle_timeout_ms() -> u64 {
    DEFAULT_COMMAND_SETTLE_TIMEOUT_MS
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            ssh_hosts: crate::ssh_hosts::SshHostOptions::default(),
            data_dir: PathBuf::from("./data"),
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            bootstrap_token: String::new(),
            bootstrap_ttl_hours: default_bootstrap_ttl_hours(),
            enroll_token_ttl_minutes: default_enroll_token_ttl_minutes(),
            cookie_secure: true,
            public_origin: None,
            trusted_proxies: Vec::new(),
            allowed_origins: Vec::new(),
            web_root: None,
            push_block_ms: default_push_block_ms(),
            follow_buffer_events: default_follow_buffer_events(),
            command_accept_timeout_ms: default_command_accept_timeout_ms(),
            create_settle_timeout_ms: default_create_settle_timeout_ms(),
            command_settle_timeout_ms: default_command_settle_timeout_ms(),
            host_lost_grace_ms: default_host_lost_grace_ms(),
            requested_grace_ms: default_requested_grace_ms(),
            gate_ref_retention_ms: default_gate_ref_retention_ms(),
            gate_cancel_grace_ms: default_gate_cancel_grace_ms(),
            auth_ip_burst: default_auth_ip_burst(),
            auth_ip_refill_per_sec: default_auth_ip_refill_per_sec(),
            auth_global_burst: default_auth_global_burst(),
            auth_global_refill_per_sec: default_auth_global_refill_per_sec(),
            attachment_max_bytes: default_attachment_max_bytes(),
            resource_sample_max_age_ms: default_resource_sample_max_age_ms(),
            resource_refresh_timeout_ms: default_resource_refresh_timeout_ms(),
        }
    }
}

impl HubConfig {
    /// Test helper: insecure cookie, generated bootstrap, caller-supplied data dir.
    pub fn for_test(data_dir: PathBuf) -> Self {
        Self {
            ssh_hosts: crate::ssh_hosts::SshHostOptions::default(),
            data_dir,
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            bootstrap_token: format!("boot-{}", Uuid::new_v4().simple()),
            bootstrap_ttl_hours: default_bootstrap_ttl_hours(),
            enroll_token_ttl_minutes: default_enroll_token_ttl_minutes(),
            cookie_secure: false,
            public_origin: None,
            trusted_proxies: Vec::new(),
            allowed_origins: Vec::new(),
            web_root: None,
            push_block_ms: 80,
            follow_buffer_events: default_follow_buffer_events(),
            command_accept_timeout_ms: default_command_accept_timeout_ms(),
            create_settle_timeout_ms: default_create_settle_timeout_ms(),
            command_settle_timeout_ms: default_command_settle_timeout_ms(),
            host_lost_grace_ms: default_host_lost_grace_ms(),
            requested_grace_ms: default_requested_grace_ms(),
            gate_ref_retention_ms: default_gate_ref_retention_ms(),
            gate_cancel_grace_ms: default_gate_cancel_grace_ms(),
            // Keep production rate-limit defaults here so the 429 integration
            // test exercises real budgets; the Playwright harness relaxes them
            // explicitly because it mounts the login page dozens of times from
            // one loopback IP (each mount starts a passkey ceremony).
            auth_ip_burst: default_auth_ip_burst(),
            auth_ip_refill_per_sec: default_auth_ip_refill_per_sec(),
            auth_global_burst: default_auth_global_burst(),
            auth_global_refill_per_sec: default_auth_global_refill_per_sec(),
            attachment_max_bytes: default_attachment_max_bytes(),
            resource_sample_max_age_ms: default_resource_sample_max_age_ms(),
            resource_refresh_timeout_ms: default_resource_refresh_timeout_ms(),
        }
    }

    /// Effective create settlement deadline, clamped to the protocol safety floor.
    pub(crate) fn create_settle_timeout_ms(&self) -> u64 {
        self.create_settle_timeout_ms
            .max(MIN_CREATE_SETTLE_TIMEOUT_MS)
    }

    /// Effective ack deadline for a forwarded non-create command.
    ///
    /// Never shorter than the RPC accept timeout plus a one-second margin: the
    /// deadline must outlast the in-flight `call`, or a slow-but-valid accept
    /// would be failed before `mark_accepted` runs. (`mark_accepted` now
    /// recovers a `failed` row as a backstop, but the clamp keeps the common
    /// path from ever racing.)
    pub(crate) fn command_settle_timeout_ms(&self) -> u64 {
        self.command_settle_timeout_ms
            .max(self.command_accept_timeout_ms.saturating_add(1_000))
    }
}

/// RFC3339 UTC with millisecond precision (`protocol.md` §1.1).
pub fn now_rfc3339() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

/// 256-bit hex token (device or Node).
pub fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Constant-time compare for bootstrap / other plaintext secrets.
#[must_use]
pub fn secret_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for i in 0..len {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

/// Protocol branded ID as a string.
pub fn new_id(prefix: &str) -> Result<String, remuda_protocol::WireValueError> {
    remuda_protocol::Id::new(prefix).map(|id| id.to_string())
}

/// Lowercase hex SHA-256, used to deduplicate staged attachments (D-027).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
