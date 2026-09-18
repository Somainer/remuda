//! Read-only diagnostics shared by the local CLI and the authenticated host RPC.

use crate::{CollectRequest, Collector, ProbeEnv};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Node-owned probe inputs. RPC callers cannot choose filesystem paths.
#[derive(Debug, Clone, Default)]
pub struct DoctorContext {
    /// Enrollment root; absent for an unconfigured in-memory runtime.
    pub data_dir: Option<PathBuf>,
    /// Listeners this runtime has actually bound, rather than candidate ports.
    pub listeners: Vec<SocketAddr>,
}

/// One diagnostic with explicit severity and non-secret evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Stable check name.
    pub name: String,
    /// `ok`, `warning`, or `blocker`.
    pub status: String,
    /// Human-readable finding.
    pub message: String,
    /// Non-secret measurements or paths.
    pub details: Value,
}

/// A point-in-time host preflight, not a guarantee of provider authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    /// Nonzero when any check reports a blocker.
    pub exit_code: i32,
    /// Node collector output, including heuristic login states.
    pub inventory: Value,
    /// Ordered diagnostics.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Start an empty report, useful when the remote host cannot be reached.
    pub fn empty() -> Self {
        Self {
            exit_code: 0,
            inventory: Value::Null,
            checks: Vec::new(),
        }
    }

    /// Append a check and retain the most severe exit status.
    pub fn check(&mut self, name: &str, status: &str, message: &str, details: Value) {
        if status == "blocker" {
            self.exit_code = 1;
        }
        self.checks.push(DoctorCheck {
            name: name.into(),
            status: status.into(),
            message: message.into(),
            details,
        });
    }
}

/// Check the registered workspace using the same bounded probe as Instance creation.
pub fn doctor_workspace(report: &mut DoctorReport, workspace: &Path, _env: &ProbeEnv) {
    match crate::workspace_access_check(workspace) {
        Ok(()) => report.check(
            "workspace.access",
            "ok",
            "Node can read the registered workspace",
            json!({"path":workspace}),
        ),
        Err(error) => report.check(
            "workspace.access",
            "blocker",
            &error.to_string(),
            json!({"path":workspace}),
        ),
    }
    #[cfg(target_os = "macos")]
    if let Ok(executable) = std::env::current_exe()
        && let Some(message) =
            crate::macos_workspace_guidance(workspace, Some(&_env.home), &executable)
    {
        report.check(
            "workspace.macos-access",
            "warning",
            &message,
            json!({"path":workspace,"executable":executable}),
        );
    }
}

/// Check access before inventory touches workspace-adjacent configuration files.
/// An access blocker returns a partial report instead of entering unbounded reads.
pub fn doctor_with_workspace(
    context: &DoctorContext,
    workspace: &Path,
    env: ProbeEnv,
) -> DoctorReport {
    doctor_with_workspaces(context, &[workspace.to_path_buf()], env)
}

/// Probe each current registered root, then perform config preflight and inventory once.
/// An empty registry still receives the same guarded config and inventory checks.
pub(crate) fn doctor_with_workspaces(
    context: &DoctorContext,
    workspaces: &[PathBuf],
    env: ProbeEnv,
) -> DoctorReport {
    let mut preflight = DoctorReport::empty();
    for workspace in workspaces {
        doctor_workspace(&mut preflight, workspace, &env);
    }
    if preflight.exit_code != 0 {
        return preflight;
    }
    #[cfg(target_os = "macos")]
    {
        let default_config = env.home.join(".claude");
        for marker in [
            default_config.join("settings.json"),
            env.home.join(".claude.json"),
        ] {
            if let Err(message) = claude_inventory_access(&marker) {
                preflight.check(
                    "config.claude.inventory",
                    "blocker",
                    &message,
                    json!({"path":marker}),
                );
                return preflight;
            }
        }
        doctor_claude_config_access(
            &mut preflight,
            &env.home,
            std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        );
        if preflight.exit_code != 0 {
            return preflight;
        }
    }
    let mut report = doctor_snapshot(context, env);
    report.checks.splice(0..0, preflight.checks);
    report
}

#[cfg(target_os = "macos")]
fn claude_inventory_access(path: &Path) -> Result<(), String> {
    // Read one byte solely to trigger access checks; values never enter output.
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"
probe_error=$(/bin/ls -ld "$1" 2>&1)
if [ $? -ne 0 ]; then
    case "$probe_error" in *": No such file or directory") exit 0 ;; esac
    printf '%s\n' "$probe_error" >&2; exit 1
fi
probe_error=$(/usr/bin/head -c 1 "$1" 2>&1 >/dev/null)
if [ $? -ne 0 ]; then
    case "$probe_error" in *": No such file or directory") exit 0 ;; esac
    printf '%s\n' "$probe_error" >&2; exit 1
fi
"#,
            "remuda-claude-inventory-probe",
        ])
        .arg(path);
    let result = crate::workspace_access::bounded_workspace_command(
        &mut command,
        path,
        Duration::from_secs(3),
    );
    let detail = match result {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => String::from_utf8_lossy(&output.stderr).into_owned(),
        Err(error) => error.to_string(),
    };
    let executable = std::env::current_exe().unwrap_or_else(|_| "remuda".into());
    Err(format!(
        "Claude inventory configuration could not be read: {}; {}",
        detail.trim(),
        crate::workspace_access_guidance(&executable)
    ))
}

/// Run filesystem, disk, executable, and identity probes using the Node collector.
/// Auth states only describe existing markers; no login command or model is run.
pub fn doctor_snapshot(context: &DoctorContext, env: ProbeEnv) -> DoctorReport {
    let snapshot =
        Collector::new(env.clone(), Duration::ZERO).snapshot_fresh(&CollectRequest::default());
    let mut report = DoctorReport::empty();
    report.inventory = snapshot.to_hub_host();
    let mut installed = 0;
    for cli in &snapshot.cli {
        // `computer-use` is not a PATH binary: it is one stat inside the Codex
        // app bundle, so it gets its own read-only check below rather than the
        // PATH wording, and it must not count toward "an agent CLI is
        // installed" — it cannot launch an agent.
        if cli.kind == crate::COMPUTER_USE_KIND {
            continue;
        }
        if cli.path.is_some() {
            installed += 1;
        }
        let (status, message) = if cli.path.is_none() {
            ("warning", "binary missing from PATH")
        } else if cli.version.is_none() {
            ("warning", "binary found; version probe failed")
        } else {
            ("ok", "binary and version found")
        };
        report.check(
            &format!("binary.{}", cli.kind),
            status,
            message,
            json!({"path":cli.path,"version":cli.version}),
        );
        report.check(
            &format!("login.{}", cli.kind),
            if matches!(
                cli.auth,
                crate::CliAuth::LoggedIn | crate::CliAuth::GatewayNative
            ) {
                "ok"
            } else {
                "warning"
            },
            "login-marker heuristic; credentials were not validated with the provider",
            json!({"state":cli.auth,"installed":cli.installed,"evidence":"local-marker"}),
        );
        if cli.kind == "claude" {
            let configured = cli.native_gateway.unwrap_or(false);
            report.check(
                "gateway.claude",
                if configured { "ok" } else { "warning" },
                if configured {
                    "native API gateway configured in settings.json (values not reported)"
                } else {
                    "no native API gateway in ~/.claude/settings.json"
                },
                json!({"configured":configured,"installed":cli.installed}),
            );
        }
    }
    if installed == 0 {
        report.check(
            "agents",
            "blocker",
            "no native agent CLI is installed",
            Value::Null,
        );
    }
    // Read-only presence check for the vendor Computer Use client
    // (`docs/design/codex-cua.md` §3.4). It never runs the vendor binary and
    // it never probes authentication — absence is a warning, not a blocker,
    // because it only gates an explicitly requested per-launch capability.
    let computer_use_path = crate::computer_use_client_path(&env);
    let computer_use = snapshot_computer_use(&snapshot);
    let computer_use_installed = computer_use.is_some_and(|entry| entry.installed);
    report.check(
        "computer-use",
        if computer_use_installed { "ok" } else { "warning" },
        if computer_use_installed {
            "Codex Computer Use client present; authentication is not probed"
        } else {
            "Codex Computer Use client not found; `--capability computer-use` will refuse on this host"
        },
        json!({
            "installed": computer_use_installed,
            "version": computer_use.and_then(|entry| entry.version.clone()),
            "path": computer_use_path,
            "auth": "unknown",
        }),
    );
    report.check(
        "binary.herdr",
        if snapshot.herdr.path.is_some() {
            "ok"
        } else {
            "warning"
        },
        "herdr is required for PTY drivers; print drivers can run without it",
        json!(snapshot.herdr),
    );
    report.check(
        "login.herdr",
        "ok",
        "herdr has no provider login",
        json!({"state":"not-applicable"}),
    );
    for name in ["cargo", "pnpm"] {
        let path = crate::inventory::find_executable(name, &env.path);
        let version = path.as_deref().and_then(crate::inventory::binary_version);
        report.check(
            &format!("binary.{name}"),
            if path.is_some() && version.is_some() {
                "ok"
            } else {
                "warning"
            },
            "development/build tool (not required for every Node driver)",
            json!({"path":path,"version":version}),
        );
    }
    if let Some(data_dir) = &context.data_dir {
        match std::fs::metadata(data_dir) {
            Ok(meta) if !meta.is_dir() || meta.permissions().readonly() => report.check(
                "data-dir",
                "blocker",
                "data directory is not a writable directory",
                json!({"path":data_dir}),
            ),
            Ok(_) => report.check(
                "data-dir",
                "ok",
                "data directory exists and is not marked read-only",
                json!({"path":data_dir}),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.check(
                "data-dir",
                "warning",
                "data directory has not been initialized",
                json!({"path":data_dir}),
            ),
            Err(_) => report.check(
                "data-dir",
                "blocker",
                "data directory cannot be inspected",
                json!({"path":data_dir}),
            ),
        }
        identity_check(&mut report, data_dir);
        let existing = data_dir
            .ancestors()
            .find(|path| path.exists())
            .unwrap_or(Path::new("."));
        let available = disk_free(existing, &env);
        report.check(
            "disk",
            match available {
                Some(bytes) if bytes < 1024 * 1024 * 1024 => "blocker",
                Some(_) => "ok",
                None => "warning",
            },
            "available disk space; less than 1 GiB blocks new work",
            json!({"path":existing,"availableBytes":available,"minimumBytes":1024u64*1024*1024}),
        );
    } else {
        report.check(
            "data-dir",
            "warning",
            "runtime has no persisted data directory",
            Value::Null,
        );
    }
    report.check(
        "listeners",
        "ok",
        if context.listeners.is_empty() {
            "no inbound TCP listener required by this runtime"
        } else {
            "listeners bound by this runtime"
        },
        json!(context.listeners),
    );
    report
}

/// The `computer-use` row from an inventory, if this Node reported one.
///
/// `None` is "this Node version does not report the row", which is distinct
/// from a reported row with `installed: false`.
fn snapshot_computer_use(snapshot: &crate::HostSnapshot) -> Option<&crate::CliEntry> {
    snapshot
        .cli
        .iter()
        .find(|entry| entry.kind == crate::COMPUTER_USE_KIND)
}

#[cfg(target_os = "macos")]
fn doctor_claude_config_access(
    report: &mut DoctorReport,
    home: &Path,
    inherited_override: Option<PathBuf>,
) {
    let scope = if inherited_override.is_some() {
        "inherited PTY CLAUDE_CONFIG_DIR"
    } else {
        "default Claude configuration"
    };
    let path = inherited_override.unwrap_or_else(|| home.join(".claude"));
    if !path.is_absolute() {
        report.check(
            "claude.config-access",
            "warning",
            "relative CLAUDE_CONFIG_DIR depends on the instance working directory; configuration access is checked before launch",
            json!({"path":path,"scope":scope}),
        );
        return;
    }
    match crate::native_config_access::check_claude_config_access(&path) {
        Ok(()) => report.check(
            "claude.config-access",
            "ok",
            "Claude startup extension directories are readable; credentials were not checked",
            json!({"path":path,"scope":scope}),
        ),
        Err(error) => report.check(
            "claude.config-access",
            "blocker",
            &error.to_string(),
            json!({"path":path,"scope":scope}),
        ),
    }
}

fn identity_check(report: &mut DoctorReport, data_dir: &Path) {
    let node_path = data_dir.join("node/host-id");
    match std::fs::read_to_string(&node_path) {
        Ok(raw) => {
            let host = raw.trim().parse::<remuda_protocol::HostId>().ok();
            report.check(
                "identity.node",
                if host.is_some() { "ok" } else { "blocker" },
                "persisted Node host-id; file was not changed",
                json!({"path":node_path,"hostId":host}),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.check(
            "identity.node",
            "warning",
            "Node host-id has not been initialized",
            json!({"path":node_path}),
        ),
        Err(_) => report.check(
            "identity.node",
            "blocker",
            "Node host-id cannot be read",
            json!({"path":node_path}),
        ),
    }
    let path = data_dir.join("enrollment.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let parsed = serde_json::from_slice::<Value>(&bytes).ok();
            let host = parsed
                .as_ref()
                .and_then(|v| v.get("hostId").or_else(|| v.get("host_id")))
                .and_then(Value::as_str);
            let valid = host.is_some_and(|host| host.parse::<remuda_protocol::HostId>().is_ok());
            report.check(
                "identity",
                if valid { "ok" } else { "blocker" },
                if valid {
                    "persisted host identity found"
                } else {
                    "invalid enrollment identity; file was not changed"
                },
                json!({"path":path,"hostId":host.filter(|_| valid)}),
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if path
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
                {
                    report.check(
                        "identity.permissions",
                        "blocker",
                        "enrollment file must be owner-only (chmod 600)",
                        json!({"path":path}),
                    );
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.check(
            "identity",
            "warning",
            "enrollment file is absent; doctor does not create identities",
            json!({"path":path}),
        ),
        Err(_) => report.check(
            "identity",
            "blocker",
            "enrollment file cannot be read",
            json!({"path":path}),
        ),
    }
}

fn disk_free(path: &Path, env: &ProbeEnv) -> Option<u64> {
    let executable = crate::inventory::find_executable("df", &env.path)?;
    let output = std::process::Command::new(executable)
        .args(["-P", "-k"])
        .arg(path)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df(&String::from_utf8(output.stdout).ok()?)
}

fn parse_df(output: &str) -> Option<u64> {
    output
        .lines()
        .skip(1)
        .find(|line| !line.trim().is_empty())?
        .split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

/// Check a prospective listener without taking ownership of any existing process.
pub fn doctor_port(report: &mut DoctorReport, name: &str, address: SocketAddr) {
    let result = TcpListener::bind(address);
    report.check(name, if result.is_ok() { "ok" } else { "blocker" },
        if result.is_ok() { "port is available for a new listener" } else { "port conflict or bind permission denied" },
        json!({"address":address,"errorKind":result.as_ref().err().map(|error| format!("{:?}", error.kind()))}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_doctor_uses_current_registry_and_checks_all_roots() {
        let fixture = tempfile::tempdir().unwrap();
        let initial = fixture.path().join("initial");
        let missing = fixture.path().join("missing");
        let accessible = fixture.path().join("accessible");
        for path in [&initial, &missing, &accessible] {
            std::fs::create_dir(path).unwrap();
        }
        let node = crate::DevNode::new(
            &crate::DevServerConfig::loopback(0)
                .with_workspace_root(initial.clone())
                .with_workspaces(vec![missing.clone(), accessible.clone()])
                .with_workspace_roots(vec![fixture.path().to_owned()]),
        )
        .unwrap();
        let roots = node.workspaces().unwrap();
        for phase in ["prepare", "commit"] {
            node.workspace_rpc(
                "workspace.unregister",
                json!({"commandId":"remove-initial", "path":roots[0].root_path, "phase":phase}),
            )
            .unwrap();
        }
        std::fs::remove_dir(initial).unwrap();
        std::fs::remove_dir(missing).unwrap();
        let report = node.doctor().await.unwrap();
        let checks = report["checks"].as_array().unwrap();
        let access: Vec<_> = checks
            .iter()
            .filter(|check| check["name"] == "workspace.access")
            .collect();
        assert_eq!(access.len(), 2);
        assert_eq!(access[0]["details"]["path"], roots[1].root_path);
        assert_eq!(access[0]["status"], "blocker");
        assert_eq!(access[1]["details"]["path"], roots[2].root_path);
        assert_eq!(access[1]["status"], "ok");
        assert!(report["inventory"].is_null());
        node.shutdown().await.unwrap();
    }

    #[test]
    fn empty_registry_and_multiple_roots_collect_one_inventory() {
        let fixture = tempfile::tempdir().unwrap();
        let env = ProbeEnv {
            path: fixture.path().join("no-binaries").into_os_string(),
            home: fixture.path().join("home"),
            hostname: Some("fixture".into()),
            herdr_socket_env: None,
            xdg_config_home: None,
            codex_home: None,
        };
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        for path in [&env.home, &first, &second] {
            std::fs::create_dir(path).unwrap();
        }
        for roots in [Vec::new(), vec![first, second]] {
            let report = doctor_with_workspaces(&DoctorContext::default(), &roots, env.clone());
            assert!(!report.inventory.is_null());
            assert_eq!(
                report
                    .checks
                    .iter()
                    .filter(|check| check.name == "binary.cargo")
                    .count(),
                1,
            );
            assert_eq!(
                report
                    .checks
                    .iter()
                    .filter(|check| check.name == "workspace.access")
                    .count(),
                roots.len(),
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn empty_registry_retains_claude_config_preflight() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join(".claude"), "not a directory").unwrap();
        let mut env = ProbeEnv::from_process();
        env.home = fixture.path().to_owned();
        let report = doctor_with_workspaces(&DoctorContext::default(), &[], env);
        assert_eq!(report.exit_code, 1);
        assert!(report.inventory.is_null());
        assert!(report.checks.iter().any(|check| {
            check.name.starts_with("config.claude.") && check.status == "blocker"
        }));
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.name != "workspace.access")
        );
    }

    #[test]
    fn missing_workspace_short_circuits_inventory() {
        let fixture = tempfile::tempdir().unwrap();
        let report = doctor_with_workspace(
            &DoctorContext::default(),
            &fixture.path().join("missing"),
            ProbeEnv::from_process(),
        );
        assert_eq!(report.exit_code, 1);
        assert!(report.inventory.is_null());
        assert_eq!(report.checks[0].name, "workspace.access");
        assert!(
            report
                .checks
                .iter()
                .all(|check| !check.name.starts_with("binary."))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inaccessible_claude_configuration_short_circuits_inventory() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join(".claude"), "not a directory").unwrap();
        let mut env = ProbeEnv::from_process();
        env.home = fixture.path().to_owned();
        let report = doctor_with_workspace(&DoctorContext::default(), fixture.path(), env);
        assert_eq!(report.exit_code, 1);
        assert!(report.inventory.is_null());
        assert!(report.checks.iter().any(|check| {
            check.name.starts_with("config.claude.")
                && check.status == "blocker"
                && check.message.contains("configuration")
        }));
    }

    #[test]
    fn workspace_diagnostics_report_failed_access_as_a_blocker() {
        let fixture = tempfile::tempdir().unwrap();
        let mut report = DoctorReport::empty();
        doctor_workspace(
            &mut report,
            &fixture.path().join("missing"),
            &ProbeEnv::from_process(),
        );
        assert_eq!(report.exit_code, 1);
        assert!(
            report
                .checks
                .iter()
                .any(|check| { check.name == "workspace.access" && check.status == "blocker" })
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn protected_workspace_diagnostics_include_daemon_permission_guidance() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("Documents/workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut env = ProbeEnv::from_process();
        env.home = fixture.path().to_owned();
        let mut report = DoctorReport::empty();
        doctor_workspace(&mut report, &workspace, &env);
        assert_eq!(report.exit_code, 0);
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "workspace.macos-access")
            .unwrap();
        assert_eq!(check.status, "warning");
        assert!(check.message.contains("Full Disk Access"));
        assert!(check.message.contains("Privacy & Security"));
    }

    #[test]
    fn disk_units_and_conflicting_port_are_explicit() {
        assert_eq!(
            parse_df(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/test 900 100 800 12% /test path\n"
            ),
            Some(819200)
        );
        assert_eq!(parse_df("invalid"), None);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut report = DoctorReport::empty();
        doctor_port(&mut report, "test-port", listener.local_addr().unwrap());
        assert_eq!(report.exit_code, 1);
    }

    #[cfg(unix)]
    #[test]
    fn missing_tools_low_disk_and_invalid_identity_are_explicit_blockers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let data = dir.path().join("data");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir_all(data.join("node")).unwrap();
        let df = bin.join("df");
        std::fs::write(&df, "#!/bin/sh\nprintf 'Filesystem Blocks Used Available Capacity Mounted\\nfixture 9000 0 800 0%% /\\n'\n").unwrap();
        std::fs::set_permissions(df, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            data.join("enrollment.json"),
            "invalid-identity-value-must-not-leak",
        )
        .unwrap();
        std::fs::write(
            data.join("node/host-id"),
            "invalid-host-value-must-not-leak",
        )
        .unwrap();
        let report = doctor_snapshot(
            &DoctorContext {
                data_dir: Some(data.clone()),
                listeners: Vec::new(),
            },
            ProbeEnv {
                path: bin.into_os_string(),
                home: dir.path().join("home"),
                hostname: Some("fixture".into()),
                herdr_socket_env: None,
                xdg_config_home: None,
                codex_home: None,
            },
        );
        assert_eq!(report.exit_code, 1);
        for name in ["agents", "disk", "identity", "identity.node"] {
            assert!(
                report
                    .checks
                    .iter()
                    .any(|check| check.name == name && check.status == "blocker"),
                "{name}: {report:?}"
            );
        }
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "binary.cargo" && check.status == "warning")
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("must-not-leak")
        );
        assert_eq!(
            std::fs::read_to_string(data.join("node/host-id")).unwrap(),
            "invalid-host-value-must-not-leak"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fake_binaries_and_login_markers_are_reported_without_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let home = dir.path().join("home");
        let data = dir.path().join("data");
        for path in [&bin, &home, &data] {
            std::fs::create_dir(path).unwrap();
        }
        for name in [
            "claude", "codex", "grok", "agy", "herdr", "cargo", "pnpm", "df",
        ] {
            let content = if name == "df" {
                "#!/bin/sh\nprintf 'Filesystem Blocks Used Available Capacity Mounted\\nfixture 9000000 0 8000000 0%% /\\n'\n".to_owned()
            } else {
                format!("#!/bin/sh\nprintf '{name} fixture-1.0\\n'\n")
            };
            let path = bin.join(name);
            std::fs::write(&path, content).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::create_dir(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/auth.json"),
            "dummy-credential-never-returned",
        )
        .unwrap();
        let env = ProbeEnv {
            path: bin.into_os_string(),
            home,
            hostname: Some("fixture".into()),
            herdr_socket_env: None,
            xdg_config_home: None,
            codex_home: None,
        };
        let report = doctor_snapshot(
            &DoctorContext {
                data_dir: Some(data.clone()),
                listeners: Vec::new(),
            },
            env,
        );
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.inventory["cli"][1]["auth"], "logged_in");
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "binary.pnpm" && c.status == "ok")
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("dummy-credential-never-returned")
        );
        assert!(!data.join("enrollment.json").exists());
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "gateway.claude" && c.status == "warning")
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_native_gateway_is_reported_without_settings_values() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let home = dir.path().join("home");
        let data = dir.path().join("data");
        for path in [&bin, &home, &data] {
            std::fs::create_dir(path).unwrap();
        }
        let claude = bin.join("claude");
        std::fs::write(&claude, "#!/bin/sh\nprintf 'claude fixture-1.0\\n'\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(home.join(".claude")).unwrap();
        let secret = "sk-fake-doctor-gateway-zzzz";
        std::fs::write(
            home.join(".claude/settings.json"),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://gateway.example.invalid/v1",
                    "ANTHROPIC_AUTH_TOKEN": secret
                }
            })
            .to_string(),
        )
        .unwrap();
        let report = doctor_snapshot(
            &DoctorContext {
                data_dir: Some(data),
                listeners: Vec::new(),
            },
            ProbeEnv {
                path: bin.into_os_string(),
                home,
                hostname: Some("fixture".into()),
                herdr_socket_env: None,
                xdg_config_home: None,
                codex_home: None,
            },
        );
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains(secret), "token leaked: {encoded}");
        assert!(
            !encoded.contains("gateway.example.invalid"),
            "base url leaked: {encoded}"
        );
        assert_eq!(report.inventory["cli"][0]["auth"], "gateway-native");
        let gateway = report
            .checks
            .iter()
            .find(|c| c.name == "gateway.claude")
            .expect("gateway.claude");
        assert_eq!(gateway.status, "ok");
        assert_eq!(gateway.details["configured"], true);
        assert_eq!(gateway.details["installed"], true);
    }

    /// A bin dir with the CLIs every doctor fixture needs, and a bare home.
    fn doctor_fixture(dir: &Path) -> (PathBuf, PathBuf) {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        let home = dir.join("home");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&home).unwrap();
        for name in ["claude", "herdr", "cargo", "pnpm", "df"] {
            let path = bin.join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\nprintf '{name} fixture-1.0\\n'\n"),
            )
            .unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (bin, home)
    }

    /// The doctor check is read-only, non-blocking, and names its path.
    #[test]
    fn computer_use_check_reports_absence_with_the_path_it_looked_for() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, home) = doctor_fixture(dir.path());
        let env = ProbeEnv {
            path: bin.into_os_string(),
            home,
            hostname: Some("fixture".into()),
            herdr_socket_env: None,
            xdg_config_home: None,
            codex_home: None,
        };
        let report = doctor_snapshot(
            &DoctorContext {
                data_dir: None,
                listeners: Vec::new(),
            },
            env,
        );
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "computer-use")
            .expect("computer-use check");
        // Absent is a warning, never a blocker: it only gates an explicitly
        // requested per-launch capability.
        assert_eq!(check.status, "warning");
        assert_eq!(check.details["installed"], false);
        assert_eq!(check.details["auth"], "unknown");
        assert!(
            check.details["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("SkyComputerUseClient")),
            "the check must name the path it looked for: {check:?}"
        );
        assert!(
            check.message.contains("not found"),
            "absence is reported plainly: {check:?}"
        );
    }

    /// A present bundle flips the same check to ok and reports the version.
    #[test]
    fn computer_use_check_reports_presence_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, home) = doctor_fixture(dir.path());
        let probe = ProbeEnv {
            path: bin.into_os_string(),
            home: home.clone(),
            hostname: Some("fixture".into()),
            herdr_socket_env: None,
            xdg_config_home: None,
            codex_home: None,
        };
        // Lay down the vendor client and its plist exactly where the probe looks.
        let client_path = crate::computer_use_client_path(&probe);
        std::fs::create_dir_all(client_path.parent().unwrap()).unwrap();
        std::fs::write(&client_path, "#!/bin/sh\nexit 1\n").unwrap();
        let plist_path = crate::computer_use_plist_path(&probe);
        std::fs::create_dir_all(plist_path.parent().unwrap()).unwrap();
        std::fs::write(
            &plist_path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>3.1.4</string>
</dict></plist>
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&client_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let report = doctor_snapshot(
            &DoctorContext {
                data_dir: None,
                listeners: Vec::new(),
            },
            probe,
        );
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "computer-use")
            .expect("computer-use check");
        #[cfg(unix)]
        assert_eq!(check.status, "ok", "{check:?}");
        #[cfg(unix)]
        assert_eq!(check.details["installed"], true);
        #[cfg(unix)]
        assert_eq!(check.details["version"], "3.1.4");
        assert_eq!(check.details["auth"], "unknown");
    }
}
#[cfg(target_os = "macos")]
#[test]
fn claude_config_diagnostics_distinguish_default_override_and_relative_scopes() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join(".claude"), "invalid directory").unwrap();
    let mut default = DoctorReport::empty();
    doctor_claude_config_access(&mut default, fixture.path(), None);
    assert_eq!(default.exit_code, 1);
    assert_eq!(default.checks[0].name, "claude.config-access");
    assert_eq!(default.checks[0].status, "blocker");
    assert!(default.checks[0].message.contains("Full Disk Access"));

    let isolated = fixture.path().join("isolated");
    std::fs::create_dir_all(isolated.join("skills/example")).unwrap();
    std::fs::write(
        isolated.join("skills/example/SKILL.md"),
        "private-markdown-never-returned",
    )
    .unwrap();
    let mut overridden = DoctorReport::empty();
    doctor_claude_config_access(&mut overridden, fixture.path(), Some(isolated));
    assert_eq!(overridden.exit_code, 0);
    assert_eq!(overridden.checks[0].status, "ok");
    assert_eq!(
        overridden.checks[0].details["scope"],
        "inherited PTY CLAUDE_CONFIG_DIR"
    );
    assert!(
        !serde_json::to_string(&overridden)
            .unwrap()
            .contains("private-markdown-never-returned")
    );

    let mut relative = DoctorReport::empty();
    doctor_claude_config_access(&mut relative, fixture.path(), Some("relative".into()));
    assert_eq!(relative.exit_code, 0);
    assert_eq!(relative.checks[0].status, "warning");
    assert!(
        relative.checks[0]
            .message
            .contains("instance working directory")
    );
}
