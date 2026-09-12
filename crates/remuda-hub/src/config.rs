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
/// Default lifetime of the bootstrap device access code (D-018).
pub const DEFAULT_BOOTSTRAP_TTL_HOURS: u64 = 24;
/// Default lifetime of a minted node enroll token (D-018).
pub const DEFAULT_ENROLL_TOKEN_TTL_MINUTES: u64 = 60;

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
    /// Offline grace before stale instances exit with reason host-lost (default ten minutes).
    #[serde(default = "default_host_lost_grace_ms")]
    pub host_lost_grace_ms: u64,
}

fn default_host_lost_grace_ms() -> u64 {
    600_000
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
            host_lost_grace_ms: default_host_lost_grace_ms(),
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
            host_lost_grace_ms: default_host_lost_grace_ms(),
        }
    }

    /// Effective create settlement deadline, clamped to the protocol safety floor.
    pub(crate) fn create_settle_timeout_ms(&self) -> u64 {
        self.create_settle_timeout_ms
            .max(MIN_CREATE_SETTLE_TIMEOUT_MS)
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
