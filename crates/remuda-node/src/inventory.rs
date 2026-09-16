//! Host inventory for `node.hello` / heartbeat (Hub-canonical JSON).
//!
//! Hub (`remuda-hub::inventory`) stores `cli[]` as `{kind,version,path,auth}`
//! with `auth` in `{gateway-native,logged_in,logged_out,unknown}`, `herdr` as
//! `{version,socket,path}`, and `resources` as `{cpuPct,memPct}`. Extra fields
//! (`sha256`, `installed`, `nativeGateway`, `cpuCount`, `os`/`kernel`/`libc`)
//! are advertised for Node use; Hub ignores unknown keys. Auth never carries
//! secret values.

use remuda_protocol::{
    AdapterTransport, Capability, CapabilitySet, CapabilitySnapshot, CapabilityState, Digest,
    DriverDescriptor, DriverKind, Id, Knowledge, U64,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Native CLIs probed on PATH.
pub const CLI_KINDS: [&str; 5] = ["claude", "codex", "grok", "agy", "gemini"];

/// Default cache lifetime for PATH/--version/sha256/auth probes.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

/// Login heuristic result. Serialized as Hub `auth`.
///
/// Claude may also report `gateway-native` when `~/.claude/settings.json`
/// configures an API gateway (env keys or `apiKeyHelper` present). Values of
/// those keys are never serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CliAuth {
    /// `~/.claude/settings.json` has a native API gateway.
    #[serde(rename = "gateway-native")]
    GatewayNative,
    /// A non-secret local marker indicates a logged-in CLI.
    LoggedIn,
    /// Binary is present and the login marker is absent.
    LoggedOut,
    /// Binary missing, or the heuristic could not be applied.
    Unknown,
}

/// One native CLI in Hub `cli[]` shape, plus `sha256`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliEntry {
    /// Native product name (`claude`, `codex`, …).
    pub kind: String,
    /// First non-empty `--version` line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Canonical absolute executable path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Login heuristic; never a secret value.
    pub auth: CliAuth,
    /// Binary was resolved on PATH.
    pub installed: bool,
    /// Claude-only: `settings.json` configures a native API gateway (boolean).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_gateway: Option<bool>,
    /// `sha256:` + 64 hex digits of the executable, when hashed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Herdr binary and socket in Hub `{version,socket,path}` shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HerdrReport {
    /// First non-empty `herdr --version` line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Configured or default API socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// Canonical absolute `herdr` path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// Load snapshot. Hub persists `cpuPct` / `memPct`; counts are extra.
///
/// `cpuPct` is the **1-minute** load average relative to logical CPU count:
/// Linux reads the first whitespace-separated field of `/proc/loadavg`
/// (`1m 5m 15m`); macOS reads the first numeric token of `sysctl -n
/// vm.loadavg` (same order, printed inside braces). The 5/15-minute indices
/// are deliberately not used: admission must see the load that is gone now,
/// not the load that is still decaying from an hour ago.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceReport {
    /// Logical CPU count.
    pub cpu_count: u32,
    /// Total physical memory in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    /// 1-minute load average relative to CPU count, 0–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<u8>,
    /// Raw 1-minute load average (not normalized by CPU count).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_avg1: Option<f64>,
    /// Used / total memory, 0–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_pct: Option<u8>,
    /// Free scratch disk in GiB (`df` of the temp dir); read by
    /// placement §3.4 and `remuda hostcap`. Best-effort like the other fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_gb: Option<f64>,
}

/// Fresh CPU/memory sample, bypassing the 30 s inventory probe cache.
///
/// The full inventory probe shells out for CLI versions and hashes binaries;
/// placement heartbeats need only a `/proc/loadavg` + `/proc/meminfo` read
/// (microseconds), so the periodic resource report uses this directly rather
/// than [`collect`], whose cached snapshot would freeze the Hub's admission
/// view between `node.hello` frames — the stale-CPU refusal this sampler
/// exists to prevent.
#[must_use]
pub fn sample_resources() -> ResourceReport {
    detect_resources()
}

/// Labels, concurrency, and Herdr socket supplied by Node config.
#[derive(Debug, Clone)]
pub struct CollectRequest {
    /// Operator placement labels (`region=sg`).
    pub labels: BTreeMap<String, String>,
    /// Advertised maximum concurrent Instances.
    pub max_instances: usize,
    /// Explicit Herdr API socket; default `~/.config/herdr/herdr.sock` is used only if it exists.
    pub herdr_socket: Option<PathBuf>,
}

impl Default for CollectRequest {
    fn default() -> Self {
        Self {
            labels: BTreeMap::new(),
            max_instances: 8,
            herdr_socket: None,
        }
    }
}

/// Process-local files and PATH used while probing (injectable in tests).
#[derive(Debug, Clone)]
pub struct ProbeEnv {
    /// `PATH` used to resolve CLIs and `herdr`.
    pub path: OsString,
    /// Home directory for auth-marker paths (`~/.claude.json`, …).
    pub home: PathBuf,
    /// Optional `HOSTNAME` override.
    pub hostname: Option<String>,
    /// `HERDR_SOCKET_PATH` when the process set one.
    pub herdr_socket_env: Option<PathBuf>,
    /// `XDG_CONFIG_HOME` for the default Herdr socket.
    pub xdg_config_home: Option<PathBuf>,
}

impl ProbeEnv {
    /// Read PATH / HOME / HOSTNAME / Herdr socket from the current process.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
            hostname: std::env::var("HOSTNAME")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty()),
            herdr_socket_env: std::env::var_os("HERDR_SOCKET_PATH")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty()),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty()),
        }
    }
}

/// Cached host probe with a TTL.
pub struct Collector {
    env: ProbeEnv,
    ttl: Duration,
    cache: Mutex<Option<CachedProbe>>,
}

struct CachedProbe {
    at: Instant,
    parts: ProbeParts,
}

#[derive(Debug, Clone)]
struct ProbeParts {
    hostname: String,
    cli: Vec<CliEntry>,
    herdr_path: Option<PathBuf>,
    herdr_version: Option<String>,
    os: String,
    kernel: Option<String>,
    libc: Option<String>,
    resources: ResourceReport,
}

/// Full inventory attached to `node.hello` params (flat or nested under `host`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    /// Best-effort hostname.
    pub hostname: String,
    /// Operator placement labels from config.
    pub labels: BTreeMap<String, String>,
    /// Configured concurrency ceiling.
    pub max_instances: usize,
    /// Native CLIs in Hub `cli[]` shape.
    pub cli: Vec<CliEntry>,
    /// Herdr binary and socket.
    pub herdr: HerdrReport,
    /// CPU / memory snapshot.
    pub resources: ResourceReport,
    /// `std::env::consts::OS`.
    pub os: String,
    /// `uname -r`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// glibc / musl / libSystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
    /// D-028 §5.1 driver launch inventory, echoed by every hello under
    /// `capabilities.driverInventory`. Skipped when empty so a Node that
    /// cannot describe itself is read as "not reported", never as a refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub driver_inventory: Vec<DriverDescriptor>,
}

impl HostSnapshot {
    /// Nested `host` object Hub `from_node_params` accepts.
    #[must_use]
    pub fn to_hub_host(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Hub `cli[]` JSON (`kind` / `version` / `path` / `auth`).
    #[must_use]
    pub fn cli_hub_json(&self) -> Value {
        serde_json::to_value(&self.cli).unwrap_or_else(|_| Value::Array(Vec::new()))
    }
}

/// Probe using process PATH/HOME, honoring the process-wide TTL cache.
#[must_use]
pub fn collect(config: &CollectRequest) -> HostSnapshot {
    global().snapshot(config)
}

/// Probe using process PATH/HOME, bypassing the TTL cache.
#[must_use]
pub fn collect_fresh(config: &CollectRequest) -> HostSnapshot {
    global().snapshot_fresh(config)
}

fn global() -> &'static Collector {
    static GLOBAL: OnceLock<Collector> = OnceLock::new();
    GLOBAL.get_or_init(|| Collector::new(ProbeEnv::from_process(), DEFAULT_TTL))
}

impl Collector {
    /// Build a collector with an explicit PATH/HOME view (tests use a fake PATH).
    #[must_use]
    pub fn new(env: ProbeEnv, ttl: Duration) -> Self {
        Self {
            env,
            ttl,
            cache: Mutex::new(None),
        }
    }

    /// Cached snapshot merged with the current config labels / socket.
    #[must_use]
    pub fn snapshot(&self, config: &CollectRequest) -> HostSnapshot {
        let parts = self.parts(false);
        assemble(&parts, config, &self.env)
    }

    /// Snapshot that always re-probes.
    #[must_use]
    pub fn snapshot_fresh(&self, config: &CollectRequest) -> HostSnapshot {
        let parts = self.parts(true);
        assemble(&parts, config, &self.env)
    }

    fn parts(&self, force: bool) -> ProbeParts {
        let mut guard = match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !force
            && let Some(cached) = guard.as_ref()
            && cached.at.elapsed() < self.ttl
        {
            return cached.parts.clone();
        }
        let parts = probe(&self.env);
        *guard = Some(CachedProbe {
            at: Instant::now(),
            parts: parts.clone(),
        });
        parts
    }
}

fn assemble(parts: &ProbeParts, config: &CollectRequest, env: &ProbeEnv) -> HostSnapshot {
    HostSnapshot {
        hostname: parts.hostname.clone(),
        labels: config.labels.clone(),
        max_instances: config.max_instances,
        cli: parts.cli.clone(),
        herdr: HerdrReport {
            version: parts.herdr_version.clone(),
            socket: herdr_socket(config, env),
            path: parts.herdr_path.clone(),
        },
        resources: parts.resources.clone(),
        os: parts.os.clone(),
        kernel: parts.kernel.clone(),
        libc: parts.libc.clone(),
        driver_inventory: driver_inventory(),
    }
}

/// What this Node can actually launch, for the Hub host view (D-028 §5.1).
///
/// The demo this exists to prevent: New Session offered `claude` on
/// `shell-pty`, the Node silently fell back to a login shell because
/// `REMUDA_PTY_CARRIER` was unset, and the first prompt was typed into zsh —
/// which ran it as a command. Nothing anywhere reported that native launch was
/// off, so the UI could not have known. `launchable` is that report, and the
/// web keys its `shell-pty` default on it.
///
/// Only `shell-pty` is described: it is the one driver whose ability to launch
/// an agent depends on a runtime flag rather than on a binary being present.
/// An empty inventory means "not reported", and no caller reads it as
/// "cannot" — [`super::transport::hubnode::hello_capabilities`] sends no
/// capabilities at all in that case, never `{}`.
#[must_use]
pub fn driver_inventory() -> Vec<DriverDescriptor> {
    let native = remuda_driver::shell_pty::native_carrier_enabled();
    let Ok(empty_digest) = Digest::try_from(format!("sha256:{:x}", Sha256::digest([]))) else {
        return Vec::new();
    };
    vec![DriverDescriptor {
        kind: DriverKind::ShellPty,
        adapter_version: remuda_driver::ADAPTER_VERSION.to_owned(),
        // The binary is per-launch here — a login shell or whichever agent the
        // recipe pins — so there is nothing host-wide to name or hash. Left
        // empty rather than filled with a plausible-looking `$SHELL`, which
        // would be wrong for every agent launch. The digest is the sha256 of
        // no bytes, which is what "nothing was hashed" spells in a field the
        // wire type requires to be a well-formed sha256.
        binary_path: String::new(),
        binary_version: String::new(),
        binary_digest: empty_digest,
        launchable: native,
        reason_code: if native {
            "carrier-native".to_owned()
        } else {
            // Names the flag's absence, not a defect: `shell-pty` still
            // launches a login shell, and D-025 promotion still works inside
            // it. What is unavailable is Remuda running the agent command.
            "carrier-not-enabled".to_owned()
        },
        capabilities: driver_capability_snapshot(DriverKind::ShellPty),
    }]
}

/// Capability snapshot the Node reports for a locally registered driver.
///
/// Used both for the host's `driverInventory` descriptor and for the in-process
/// fake-driver instance records, so the two never drift apart.
#[must_use]
pub fn driver_capability_snapshot(driver: DriverKind) -> CapabilitySnapshot {
    let unknown_capability = Capability {
        state: CapabilityState::Unknown,
        provision: remuda_protocol::CapabilityProvision::Unknown,
        scope: Vec::new(),
        reason_code: "not-verified".to_owned(),
        prerequisites: Vec::new(),
        evidence: Vec::new(),
    };
    let unsupported = Capability {
        state: CapabilityState::Unsupported,
        provision: remuda_protocol::CapabilityProvision::Unknown,
        scope: Vec::new(),
        reason_code: "fake-driver".to_owned(),
        prerequisites: Vec::new(),
        evidence: Vec::new(),
    };
    let supported = Capability {
        state: CapabilityState::Supported,
        provision: remuda_protocol::CapabilityProvision::Native,
        scope: vec!["local-fixture".to_owned()],
        reason_code: "fake-driver".to_owned(),
        prerequisites: Vec::new(),
        evidence: Vec::new(),
    };
    CapabilitySnapshot {
        adapter_transport: AdapterTransport::NativeRustWire,
        id: Id::new("obj").expect("constant valid id"),
        driver_kind: driver,
        adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        binary_version: "fake".to_owned(),
        binary_digest: Digest::try_from(format!("sha256:{:064x}", 0)).expect("constant digest"),
        native_protocol_version: Knowledge::NotApplicable,
        settings_revision: U64(1),
        provider_profile_revision: U64(1),
        capabilities: CapabilitySet {
            resume: unsupported.clone(),
            steer: unsupported.clone(),
            // D-028 §6: unmeasured for the fake driver as for every real
            // one; `unknown` keeps the fixture honest rather than teaching
            // tests that a fake can queue or interrupt.
            queue: unknown_capability.clone(),
            interrupt: unknown_capability.clone(),
            model_switch: unsupported.clone(),
            fork: unsupported.clone(),
            structured_workflow: unsupported.clone(),
            artifact: unsupported.clone(),
            tty_attach: if matches!(
                driver,
                DriverKind::GenericPty | DriverKind::ClaudePty | DriverKind::ShellPty
            ) {
                supported.clone()
            } else {
                unsupported.clone()
            },
            hooks: unsupported,
            interactive_approval: unknown_capability.clone(),
            question: unknown_capability.clone(),
            plan_review: unknown_capability.clone(),
            elicitation: unknown_capability.clone(),
            live_attach: unknown_capability,
            completion_native_turn: supported,
            completion_task: Capability {
                state: CapabilityState::Unsupported,
                provision: remuda_protocol::CapabilityProvision::Unknown,
                scope: Vec::new(),
                reason_code: "fake-native-turn-only".to_owned(),
                prerequisites: Vec::new(),
                evidence: Vec::new(),
            },
        },
    }
}

fn probe(env: &ProbeEnv) -> ProbeParts {
    let cli: Vec<CliEntry> = CLI_KINDS.iter().map(|kind| probe_cli(kind, env)).collect();
    let herdr_path = find_executable("herdr", &env.path);
    let herdr_version = herdr_path.as_deref().and_then(binary_version);
    if let Some(path) = herdr_path.as_ref() {
        tracing::debug!(path = %path.display(), version = herdr_version.as_deref(), "herdr inventory");
    }
    ProbeParts {
        hostname: hostname(env),
        cli,
        herdr_path,
        herdr_version,
        os: std::env::consts::OS.to_owned(),
        kernel: run_system("uname", &["-r"]),
        libc: detect_libc(),
        resources: detect_resources(),
    }
}

fn probe_cli(kind: &str, env: &ProbeEnv) -> CliEntry {
    let path = find_executable(kind, &env.path);
    let installed = path.is_some();
    let version = path.as_deref().and_then(binary_version);
    let sha256 = path.as_deref().and_then(hash_file);
    let native_gateway = (kind == "claude").then(|| claude_native_gateway_configured(&env.home));
    let auth = cli_auth(kind, &env.home, installed, native_gateway.unwrap_or(false));
    tracing::debug!(
        kind,
        path = path.as_deref().map(|p| p.display().to_string()),
        version = version.as_deref(),
        installed,
        native_gateway,
        ?auth,
        "cli inventory"
    );
    CliEntry {
        kind: kind.to_owned(),
        version,
        path,
        auth,
        installed,
        native_gateway,
        sha256,
    }
}

fn cli_auth(kind: &str, home: &Path, installed: bool, native_gateway: bool) -> CliAuth {
    if !installed {
        return CliAuth::Unknown;
    }
    match kind {
        "claude" if native_gateway => CliAuth::GatewayNative,
        "claude" => claude_oauth_auth(home),
        "codex" => marker_auth(&home.join(".codex").join("auth.json")),
        "grok" => marker_auth(&home.join(".grok").join("auth.json")),
        "agy" => agy_auth(home),
        _ => CliAuth::Unknown,
    }
}

/// Whether `~/.claude/settings.json` configures a native API gateway.
///
/// True when `env.ANTHROPIC_BASE_URL` and `env.ANTHROPIC_AUTH_TOKEN` are both
/// present, or when `apiKeyHelper` is a non-empty string. Values are never
/// returned or logged.
#[must_use]
pub fn claude_native_gateway_configured(home: &Path) -> bool {
    settings_json_has_native_gateway(&home.join(".claude").join("settings.json"))
}

fn settings_json_has_native_gateway(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    if let Some(helper) = value.get("apiKeyHelper") {
        match helper {
            Value::String(text) if !text.trim().is_empty() => return true,
            Value::Array(items)
                if items
                    .iter()
                    .any(|item| item.as_str().is_some_and(|text| !text.trim().is_empty())) =>
            {
                return true;
            }
            _ => {}
        }
    }
    let Some(env) = value.get("env").and_then(Value::as_object) else {
        return false;
    };
    env_key_present(env, "ANTHROPIC_BASE_URL") && env_key_present(env, "ANTHROPIC_AUTH_TOKEN")
}

fn env_key_present(env: &serde_json::Map<String, Value>, key: &str) -> bool {
    match env.get(key) {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Number(_)) => true,
        Some(Value::Bool(true)) => true,
        _ => false,
    }
}

/// `~/.claude.json`: only the presence of the `oauthAccount` object key.
fn claude_oauth_auth(home: &Path) -> CliAuth {
    let path = home.join(".claude.json");
    match std::fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CliAuth::LoggedOut,
        Err(_) => CliAuth::Unknown,
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => {
                if map.contains_key("oauthAccount") {
                    CliAuth::LoggedIn
                } else {
                    CliAuth::LoggedOut
                }
            }
            Ok(_) => CliAuth::LoggedOut,
            Err(_) => CliAuth::Unknown,
        },
    }
}

fn marker_auth(path: &Path) -> CliAuth {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => CliAuth::LoggedIn,
        Ok(_) => CliAuth::LoggedOut,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CliAuth::LoggedOut,
        Err(_) => CliAuth::Unknown,
    }
}

fn agy_auth(home: &Path) -> CliAuth {
    let candidates = [
        home.join(".agy").join("settings.json"),
        home.join(".config").join("agy").join("settings.json"),
    ];
    if candidates
        .iter()
        .any(|path| marker_auth(path) == CliAuth::LoggedIn)
    {
        CliAuth::LoggedIn
    } else {
        CliAuth::LoggedOut
    }
}

fn herdr_socket(config: &CollectRequest, env: &ProbeEnv) -> Option<PathBuf> {
    if let Some(path) = config.herdr_socket.clone() {
        return Some(path);
    }
    if let Some(path) = env.herdr_socket_env.clone() {
        return Some(path);
    }
    let default = default_herdr_socket(env);
    default.exists().then_some(default)
}

fn default_herdr_socket(env: &ProbeEnv) -> PathBuf {
    let config_dir = env
        .xdg_config_home
        .clone()
        .unwrap_or_else(|| env.home.join(".config"));
    config_dir.join("herdr").join("herdr.sock")
}

fn hostname(env: &ProbeEnv) -> String {
    if let Some(name) = env.hostname.clone() {
        return name;
    }
    run_system("uname", &["-n"]).unwrap_or_else(|| "unknown".to_owned())
}

pub(crate) fn find_executable(name: &str, path_var: &OsString) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
        .and_then(canonicalize_or_abs)
}

fn canonicalize_or_abs(path: PathBuf) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return Some(canonical);
    }
    if path.is_absolute() {
        return Some(path);
    }
    let cwd = std::env::current_dir().ok()?;
    Some(cwd.join(path))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn binary_version(path: &Path) -> Option<String> {
    let output = spawn_version(path)?;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8(output.stderr).ok()?
    } else {
        String::from_utf8(output.stdout).ok()?
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(256).collect())
}

fn spawn_version(path: &Path) -> Option<std::process::Output> {
    let mut delay = Duration::from_millis(5);
    for _ in 0..5 {
        match Command::new(path).arg("--version").output() {
            Ok(output) => return Some(output),
            Err(err) if is_retryable_spawn(&err) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %err,
                    "retry --version after ETXTBSY/spawn failure"
                );
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
            }
            Err(_) => return None,
        }
    }
    None
}

fn is_retryable_spawn(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ExecutableFileBusy | ErrorKind::Interrupted | ErrorKind::WouldBlock
    ) || err.raw_os_error() == Some(26)
}

fn hash_file(path: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path).ok()?;
    let mut buf = [0_u8; 8192];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(format!("sha256:{:x}", hasher.finalize()))
}

fn detect_libc() -> Option<String> {
    if cfg!(target_os = "macos") {
        return Some("libSystem".to_owned());
    }
    if let Some(text) = run_system("getconf", &["GNU_LIBC_VERSION"]) {
        return Some(text);
    }
    run_system("ldd", &["--version"]).and_then(|text| {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    })
}

fn detect_resources() -> ResourceReport {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1);
    let (mem_bytes, mem_available) = memory_bytes();
    let mem_pct = match (mem_bytes, mem_available) {
        (Some(total), Some(avail)) if total > 0 => Some(mem_to_pct(total, avail)),
        _ => None,
    };
    let load_avg1 = loadavg();
    let cpu_pct = load_avg1.map(|load| load_to_pct(load, cpu_count));
    let disk_free_gb = disk_free_gib();
    ResourceReport {
        cpu_count,
        mem_bytes,
        cpu_pct,
        load_avg1,
        mem_pct,
        disk_free_gb,
    }
}

/// 1-minute load average as a percentage of logical CPU count, clamped 0–100.
pub(crate) fn load_to_pct(load_one_minute: f64, cpu_count: u32) -> u8 {
    percent(load_one_minute, f64::from(cpu_count.max(1)))
}

/// Used memory as a percentage of total: `(total - available) / total`.
pub(crate) fn mem_to_pct(total_bytes: u64, available_bytes: u64) -> u8 {
    let used = total_bytes.saturating_sub(available_bytes);
    percent(used as f64, total_bytes as f64)
}

/// Free scratch disk in GiB via `df -Pk <dir>` (the same source the
/// coordinator watcher polls); `None` on platforms/runs where `df` is absent.
/// Scratch lives under the OS temp dir (`/tmp` on Linux), which is what the
/// project `diskBudgetGb` budgets (design §3.2).
fn disk_free_gib() -> Option<f64> {
    let target = std::env::temp_dir();
    let output = Command::new("df").arg("-Pk").arg(&target).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // `df -Pk`: second data line, 4th column = available 1K-blocks.
    let line = text.lines().nth(1)?;
    let avail_kib: f64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kib / (1024.0 * 1024.0))
}

fn percent(num: f64, den: f64) -> u8 {
    if den <= 0.0 {
        return 0;
    }
    ((num / den) * 100.0).clamp(0.0, 100.0).round() as u8
}

fn memory_bytes() -> (Option<u64>, Option<u64>) {
    if let Some(linux) = linux_meminfo() {
        return linux;
    }
    let total = run_system("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse().ok());
    (total, None)
}

fn linux_meminfo() -> Option<(Option<u64>, Option<u64>)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let kib: u64 = parts.next()?.parse().ok()?;
        let bytes = kib.saturating_mul(1024);
        match key {
            "MemTotal:" => total = Some(bytes),
            "MemAvailable:" => available = Some(bytes),
            _ => {}
        }
    }
    Some((total, available))
}

/// 1-minute load average (index 0 of the OS triple).
///
/// Linux `/proc/loadavg` and macOS `sysctl vm.loadavg` both print
/// `1m 5m 15m` in that order, so the first numeric token is the one-minute
/// value on both platforms.
fn loadavg() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        return text.split_whitespace().next()?.parse().ok();
    }
    let raw = run_system("sysctl", &["-n", "vm.loadavg"])?;
    raw.split_whitespace()
        .find_map(|tok| tok.trim_matches(|c| c == '{' || c == '}').parse().ok())
}

fn run_system(name: &str, args: &[&str]) -> Option<String> {
    let bin = system_binary(name)?;
    let output = Command::new(bin).args(args).output().ok()?;
    if !output.status.success() && output.stdout.is_empty() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8(output.stderr).ok()?
    } else {
        String::from_utf8(output.stdout).ok()?
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(256).collect())
}

fn system_binary(name: &str) -> Option<PathBuf> {
    const DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
    DIRS.iter()
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn write_stub(dir: &Path, name: &str, version: &str) -> PathBuf {
        place_on_path(
            dir,
            name,
            remuda_testing::install_executable(dir, name, format!("#!/bin/sh\necho '{version}'\n")),
        )
    }

    fn write_counter_stub(dir: &Path, name: &str) -> PathBuf {
        place_on_path(
            dir,
            name,
            remuda_testing::install_executable(
                dir,
                name,
                "#!/bin/sh\n\
                 hits=\"$0.hits\"\n\
                 n=0\n\
                 if [ -f \"$hits\" ]; then n=$(cat \"$hits\"); fi\n\
                 n=$((n+1))\n\
                 echo \"$n\" > \"$hits\"\n\
                 echo \"claude 0.0.$n\"\n",
            ),
        )
    }

    /// `install_executable` uses a unique inode; PATH lookup needs `dir/name`.
    fn place_on_path(dir: &Path, name: &str, unique: PathBuf) -> PathBuf {
        let dest = dir.join(name);
        std::fs::rename(&unique, &dest).unwrap_or_else(|err| {
            panic!(
                "place stub {} on PATH as {}: {err}",
                unique.display(),
                dest.display()
            )
        });
        dest
    }

    fn env_for(dir: &Path, home: &Path) -> ProbeEnv {
        ProbeEnv {
            path: dir.as_os_str().to_owned(),
            home: home.to_path_buf(),
            hostname: Some("test-host".to_owned()),
            herdr_socket_env: None,
            xdg_config_home: Some(home.join(".config")),
        }
    }

    fn config_with(labels: &[(&str, &str)], socket: Option<PathBuf>) -> CollectRequest {
        let mut map = BTreeMap::new();
        for (k, v) in labels {
            map.insert((*k).to_owned(), (*v).to_owned());
        }
        CollectRequest {
            labels: map,
            max_instances: 4,
            herdr_socket: socket,
        }
    }

    #[test]
    fn fake_path_resolves_absolute_version_and_sha256() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let claude = write_stub(bin.path(), "claude", "claude 9.9.9");
        write_stub(bin.path(), "gemini", "gemini 1.2.3");
        let expected_sha = hash_file(&claude).expect("sha");
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[("region", "sg")], None));
        assert_eq!(snap.hostname, "test-host");
        assert_eq!(snap.labels.get("region").map(String::as_str), Some("sg"));
        assert_eq!(snap.max_instances, 4);
        assert_eq!(snap.cli.len(), 5);
        let claude_ent = snap.cli.iter().find(|c| c.kind == "claude").unwrap();
        assert_eq!(claude_ent.version.as_deref(), Some("claude 9.9.9"));
        assert_eq!(claude_ent.sha256.as_deref(), Some(expected_sha.as_str()));
        let path = claude_ent.path.as_ref().expect("path");
        assert!(path.is_absolute(), "{path:?}");
        assert_eq!(claude_ent.auth, CliAuth::LoggedOut);
        assert!(claude_ent.installed);
        assert_eq!(claude_ent.native_gateway, Some(false));
        let gemini = snap.cli.iter().find(|c| c.kind == "gemini").unwrap();
        assert_eq!(gemini.version.as_deref(), Some("gemini 1.2.3"));
        assert_eq!(gemini.auth, CliAuth::Unknown);
        let missing = snap.cli.iter().find(|c| c.kind == "codex").unwrap();
        assert!(missing.path.is_none());
        assert_eq!(missing.auth, CliAuth::Unknown);
        assert_eq!(snap.os, std::env::consts::OS);
        assert!(snap.kernel.is_some() || cfg!(not(unix)));
        assert!(snap.resources.cpu_count >= 1);
        let hub = snap.to_hub_host();
        assert_eq!(hub["cli"][0]["kind"], "claude");
        assert!(hub["cli"][0].get("auth").is_some());
        assert!(hub.get("resources").is_some());
    }

    #[test]
    fn auth_heuristics_do_not_emit_secrets() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_stub(bin.path(), "claude", "claude 1");
        write_stub(bin.path(), "codex", "codex 1");
        write_stub(bin.path(), "grok", "grok 1");
        write_stub(bin.path(), "agy", "agy 1");
        let secret = "sk-fake-DO-NOT-LEAK-123456";
        std::fs::write(
            home.path().join(".claude.json"),
            json!({ "oauthAccount": { "token": secret }, "theme": "dark" }).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex").join("auth.json"), secret).unwrap();
        std::fs::create_dir_all(home.path().join(".grok")).unwrap();
        std::fs::write(home.path().join(".grok").join("auth.json"), secret).unwrap();
        std::fs::create_dir_all(home.path().join(".agy")).unwrap();
        std::fs::write(home.path().join(".agy").join("settings.json"), "{}").unwrap();

        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[], None));
        let encoded = serde_json::to_string(&snap).unwrap();
        assert!(!encoded.contains(secret), "secret leaked: {encoded}");
        assert_eq!(auth_of(&snap, "claude"), CliAuth::LoggedIn);
        assert_eq!(auth_of(&snap, "codex"), CliAuth::LoggedIn);
        assert_eq!(auth_of(&snap, "grok"), CliAuth::LoggedIn);
        assert_eq!(auth_of(&snap, "agy"), CliAuth::LoggedIn);
        assert_eq!(hub_auth(&snap, "claude"), "logged_in");
    }

    #[test]
    fn claude_without_oauth_key_is_logged_out() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_stub(bin.path(), "claude", "claude 1");
        std::fs::write(home.path().join(".claude.json"), r#"{"theme":"dark"}"#).unwrap();
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[], None));
        assert_eq!(auth_of(&snap, "claude"), CliAuth::LoggedOut);
    }

    #[test]
    fn claude_settings_env_gateway_is_gateway_native_without_leaking_values() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_stub(bin.path(), "claude", "claude 1");
        let secret = "sk-fake-gateway-DO-NOT-LEAK-zzzz";
        let base = "https://gateway.example.invalid/v1";
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude").join("settings.json"),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": base,
                    "ANTHROPIC_AUTH_TOKEN": secret
                },
                "model": "passthrough/auto"
            })
            .to_string(),
        )
        .unwrap();
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[], None));
        let encoded = serde_json::to_string(&snap).unwrap();
        assert!(!encoded.contains(secret), "token leaked: {encoded}");
        assert!(!encoded.contains(base), "base url leaked: {encoded}");
        let claude = snap.cli.iter().find(|c| c.kind == "claude").unwrap();
        assert_eq!(claude.auth, CliAuth::GatewayNative);
        assert_eq!(claude.native_gateway, Some(true));
        assert!(claude.installed);
        assert_eq!(hub_auth(&snap, "claude"), "gateway-native");
        assert!(claude_native_gateway_configured(home.path()));
    }

    #[test]
    fn claude_settings_api_key_helper_is_gateway_native() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_stub(bin.path(), "claude", "claude 1");
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude").join("settings.json"),
            json!({ "apiKeyHelper": "/usr/local/bin/gateway-helper" }).to_string(),
        )
        .unwrap();
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[], None));
        assert_eq!(auth_of(&snap, "claude"), CliAuth::GatewayNative);
        assert_eq!(
            snap.cli
                .iter()
                .find(|c| c.kind == "claude")
                .unwrap()
                .native_gateway,
            Some(true)
        );
    }

    #[test]
    fn claude_settings_without_gateway_keys_is_not_native() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_stub(bin.path(), "claude", "claude 1");
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::write(
            home.path().join(".claude").join("settings.json"),
            json!({ "env": { "CLAUDE_CODE_SIMPLE": "1" }, "theme": "dark" }).to_string(),
        )
        .unwrap();
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[], None));
        assert_eq!(auth_of(&snap, "claude"), CliAuth::LoggedOut);
        assert_eq!(
            snap.cli
                .iter()
                .find(|c| c.kind == "claude")
                .unwrap()
                .native_gateway,
            Some(false)
        );
    }

    #[test]
    fn herdr_uses_config_socket_and_fake_binary() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_stub(bin.path(), "herdr", "herdr 0.9.0");
        let socket = PathBuf::from("/tmp/remuda-test-herdr.sock");
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_secs(30));
        let snap = collector.snapshot(&config_with(&[], Some(socket.clone())));
        assert_eq!(snap.herdr.version.as_deref(), Some("herdr 0.9.0"));
        assert_eq!(snap.herdr.socket.as_deref(), Some(socket.as_path()));
        assert!(snap.herdr.path.as_ref().is_some_and(|p| p.is_absolute()));
        let hub = snap.to_hub_host();
        assert_eq!(hub["herdr"]["socket"], socket.to_string_lossy().as_ref());
        assert!(hub["herdr"].get("path").is_some());
    }

    #[test]
    fn ttl_cache_skips_version_until_expiry() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_counter_stub(bin.path(), "claude");
        let collector = Collector::new(env_for(bin.path(), home.path()), Duration::from_millis(80));
        let cfg = config_with(&[], None);
        let first = collector.snapshot(&cfg);
        let second = collector.snapshot(&cfg);
        assert_eq!(first.cli[0].version, second.cli[0].version);
        assert_eq!(first.cli[0].version.as_deref(), Some("claude 0.0.1"));
        std::thread::sleep(Duration::from_millis(120));
        let third = collector.snapshot(&cfg);
        assert_eq!(third.cli[0].version.as_deref(), Some("claude 0.0.2"));
        let fresh = collector.snapshot_fresh(&cfg);
        assert_eq!(fresh.cli[0].version.as_deref(), Some("claude 0.0.3"));
    }

    #[test]
    fn sampler_reports_one_minute_load_as_cpu_percent_and_real_counts() {
        // 6 runnable on 14 cores is the post-build reading from the Mac demo
        // ticket: load is already gone, but a frozen 100% sample kept refusing.
        assert_eq!(load_to_pct(0.0, 14), 0);
        assert_eq!(load_to_pct(6.0, 14), 43);
        assert_eq!(load_to_pct(14.0, 14), 100);
        // Oversubscription clamps to 100 rather than overflowing u8.
        assert_eq!(load_to_pct(28.0, 14), 100);
        // A zero/absent cpu count must not divide by zero.
        assert_eq!(load_to_pct(6.0, 0), 100);
        assert_eq!(mem_to_pct(0, 0), 0);
        assert_eq!(mem_to_pct(100, 25), 75);
        assert_eq!(mem_to_pct(100, 100), 0);

        // The live sampler reads this host: count is always populated, and on
        // the Unix CI machines both pressure figures resolve (MemAvailable on
        // Linux; memPct stays None on macOS where only hw.memsize exists).
        let sample = sample_resources();
        assert!(sample.cpu_count >= 1);
        assert!(sample.cpu_pct.is_some(), "1-minute loadavg must resolve");
        assert!((0..=100).contains(&sample.cpu_pct.unwrap()));
        if cfg!(target_os = "linux") {
            assert!(sample.mem_pct.is_some(), "MemAvailable must resolve");
            assert!(sample.mem_bytes.is_some());
        }
        let encoded = serde_json::to_value(&sample).expect("serialize");
        assert!(encoded.get("cpuPct").is_some());
        assert!(encoded.get("cpuCount").is_some());
    }

    fn auth_of(snap: &HostSnapshot, kind: &str) -> CliAuth {
        snap.cli.iter().find(|c| c.kind == kind).unwrap().auth
    }

    fn hub_auth(snap: &HostSnapshot, kind: &str) -> String {
        snap.cli_hub_json()
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["kind"] == kind)
            .unwrap()["auth"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}
