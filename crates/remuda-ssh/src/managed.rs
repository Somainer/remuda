//! Preflight for Hub-supervised daemon bridges. Remote writes use a private
//! /tmp data directory and an optional user service unit.

use crate::{Error, SshClient, bootstrap, sh_single_quote};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Operator permission to install a missing Node binary.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BinaryPolicy {
    /// Require a compatible remuda already on the remote PATH.
    #[default]
    RequireInstalled,
    /// Upload a compatible local artifact into /tmp when remuda is absent.
    UploadIfMissing,
}

/// Checked launch command, rooted below /tmp/remuda-ssh-<host-id>.
pub struct ManagedNode {
    /// Command passed to the existing SSH stdio carrier.
    pub argv: Vec<String>,
}

impl ManagedNode {
    /// Probe the daemon independently of a failed bridge, without remote writes.
    pub async fn daemon_reachable(client: &SshClient, host_id: &str) -> Result<bool, Error> {
        validate_host_id(host_id)?;
        let dir = sh_single_quote(&format!("/tmp/remuda-ssh-{host_id}"))?;
        let script = format!(
            "set -eu; if [ -x {dir}/remuda ]; then binary={dir}/remuda; else binary=$(command -v remuda); fi; \
             exec env REMUDA_DATA_DIR={dir} REMUDA_CONFIG={dir}/remuda.toml \"$binary\" node status --data-dir {dir}"
        );
        let result = client
            .exec(&["sh", "-c", &script], None, Duration::from_secs(15))
            .await?;
        Ok(result.status == Some(0))
    }
}

fn validate_host_id(host_id: &str) -> Result<(), Error> {
    if !host_id.starts_with("hst_")
        || !host_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::Enroll("invalid managed host identity".into()));
    }
    Ok(())
}

/// Accept SSH aliases or user@host; never flags, URLs or shell expressions.
pub fn validate_target(target: &str) -> Result<(), Error> {
    let parts: Vec<_> = target.split('@').collect();
    if parts.len() > 2 || target.len() > 255 {
        return Err(Error::Enroll(
            "SSH target must be an alias or user@host".into(),
        ));
    }
    for part in parts {
        crate::target::validate_alias(part)?;
    }
    Ok(())
}

/// Conservative compatibility: identical release version.
#[must_use]
pub fn version_compatible(output: &str, expected: &str) -> bool {
    output.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("remuda") && words.next() == Some(expected)
    })
}

/// Check platform/version, optionally upload, then prepare a private durable Node identity.
pub async fn prepare_managed_node(
    client: &SshClient,
    host_id: &str,
    label: &str,
    labels: &[String],
    policy: BinaryPolicy,
    upload_binary: Option<&Path>,
) -> Result<ManagedNode, Error> {
    validate_target(&client.alias)?;
    validate_host_id(host_id)?;
    let timeout = Duration::from_secs(30);
    let dir = format!("/tmp/remuda-ssh-{host_id}");
    let qdir = sh_single_quote(&dir)?;
    // mkdir must not follow a pre-existing symlink or use another user's directory.
    client.exec(&["sh", "-c", &format!(
        "set -eu; umask 077; test ! -L {qdir}; if [ ! -d {qdir} ]; then mkdir -m 700 {qdir}; fi; test -O {qdir}; chmod 700 {qdir}"
    )], None, timeout).await?.ok()?;
    let report = client
        .exec(
            &["sh", "-c", "uname -s; uname -m; command -v remuda || true"],
            None,
            timeout,
        )
        .await?
        .ok()?;
    let mut lines = report.stdout.lines();
    let os = lines.next().unwrap_or("");
    let arch = lines.next().unwrap_or("");
    let installed = lines.next().filter(|line| !line.is_empty());
    let binary = if let Some(path) = installed {
        path.to_owned()
    } else {
        if policy == BinaryPolicy::RequireInstalled {
            return Err(Error::Enroll("remuda is missing on remote PATH; choose upload_if_missing to install a private copy".into()));
        }
        let local = match upload_binary {
            Some(path) => path.to_path_buf(),
            None if same_platform(os, arch) => std::env::current_exe()?,
            None if os == "Linux" && arch == "x86_64" => musl_artifact().ok_or_else(|| Error::Enroll(
                "Linux musl artifact missing; build deploy/m1 artifact and set REMUDA_SSH_UPLOAD_BINARY on the Hub".into()
            ))?,
            None => return Err(Error::Enroll(format!("no compatible upload artifact for {os}/{arch}; set REMUDA_SSH_UPLOAD_BINARY"))),
        };
        let dest = format!("{dir}/remuda");
        bootstrap(client, &local, &dest).await?;
        dest
    };
    let version = client
        .exec(&[binary.as_str(), "version"], None, timeout)
        .await?
        .ok()?;
    if !version_compatible(&version.stdout, env!("CARGO_PKG_VERSION")) {
        return Err(Error::Enroll(format!(
            "remote remuda version is incompatible; expected {} (installed binaries are never overwritten)",
            env!("CARGO_PKG_VERSION")
        )));
    }
    let enrollment = serde_json::to_string(&serde_json::json!({"hostId": host_id}))?;
    let config = format!("data_dir = {qdir}\n[node]\nworkspace = '{dir}/workspace'\n");
    let setup = format!(
        "set -eu; umask 077; mkdir -p {qdir}/node {qdir}/workspace {qdir}/herdr {qdir}/codex; \
         if [ -f {qdir}/node/host-id ]; then test \"$(cat {qdir}/node/host-id)\" = {id}; \
         else printf '%s\\n' {id} > {qdir}/node/host-id; fi; \
         if [ ! -f {qdir}/enrollment.json ]; then printf '%s\\n' {enrollment} > {qdir}/enrollment.json; fi; \
         cat > {qdir}/remuda.toml",
        id = sh_single_quote(host_id)?,
        enrollment = sh_single_quote(&enrollment)?
    );
    client
        .exec(&["sh", "-c", &setup], Some(config.as_bytes()), timeout)
        .await?
        .ok()?;
    let mut command = vec![
        binary,
        "node".into(),
        "--data-dir".into(),
        dir.clone(),
        "--display-label".into(),
        label.into(),
    ];
    for label in labels {
        let (key, value) = label
            .split_once('=')
            .or_else(|| label.split_once(':'))
            .unwrap_or((label, "true"));
        command.extend(["--label".into(), format!("{key}={value}")]);
    }
    let command = command
        .iter()
        .map(|arg| sh_single_quote(arg))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let launch = format!(
        "cd {qdir}/workspace && exec env CODEX_HOME={qdir}/codex REMUDA_DATA_DIR={qdir} REMUDA_CONFIG={qdir}/remuda.toml {command}"
    );
    let status = client
        .exec(&["sh", "-c", &format!("{launch} status")], None, timeout)
        .await?;
    if status.status != Some(0) {
        let installed = client
            .exec(&["sh", "-c", &format!("{launch} install")], None, timeout)
            .await?;
        if installed.status != Some(0) {
            // Headless SSH accounts may have no user service manager. Keep the
            // daemon detached, and report that this start is not an enabled unit.
            tracing::warn!(host_id, stderr = %installed.stderr, "user service unavailable; starting detached daemon");
            client
                .exec(
                    &["sh", "-c", &format!("{launch} run --daemon")],
                    None,
                    timeout,
                )
                .await?
                .ok()?;
        }
        client
            .exec(&["sh", "-c", &format!("{launch} status")], None, timeout)
            .await?
            .ok()?;
    }
    Ok(ManagedNode {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("{launch} bridge --no-start"),
        ],
    })
}

fn same_platform(os: &str, arch: &str) -> bool {
    let os = match os {
        "Darwin" => "macos",
        "Linux" => "linux",
        other => other,
    };
    let arch = match arch {
        "arm64" => "aarch64",
        other => other,
    };
    os == std::env::consts::OS && arch == std::env::consts::ARCH
}

fn musl_artifact() -> Option<PathBuf> {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    [
        PathBuf::from("deploy/out/remuda-linux-musl"),
        target.join("x86_64-unknown-linux-musl/release/remuda"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_starts_detached_daemon_when_user_manager_is_absent() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let binary = fixture.path().join("remuda");
        let ssh = fixture.path().join("ssh");
        std::fs::write(&binary, "#!/bin/sh\ncase \"$*\" in *version*) echo 'remuda 0.1.0'; exit 0;; esac\nfor arg do command=$arg; done\nprintf '%s\\n' \"$command\" >> \"$REMUDA_DATA_DIR/calls\"\ncase \"$command\" in status) test -f \"$REMUDA_DATA_DIR/alive\";; install) echo 'no user service manager' >&2; exit 1;; --daemon) touch \"$REMUDA_DATA_DIR/alive\";; *) exit 2;; esac\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let report = format!(
            "printf 'Linux\\nx86_64\\n%s\\n' {}",
            sh_single_quote(binary.to_str().unwrap()).unwrap()
        );
        std::fs::write(&ssh, format!("#!/bin/sh\nfor arg do command=$arg; done\ncase \"$command\" in *'uname -s'*) {report};; *) exec /bin/sh -c \"$command\";; esac\n")).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let client = SshClient::new(
            "fixture-node",
            crate::SshOptions {
                ssh_binary: ssh,
                ..Default::default()
            },
        );
        let id = format!(
            "hst_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let remote = PathBuf::from(format!("/tmp/remuda-ssh-{id}"));
        let prepared = prepare_managed_node(
            &client,
            &id,
            "worker's label",
            &[],
            BinaryPolicy::RequireInstalled,
            None,
        )
        .await
        .unwrap();
        assert!(prepared.argv[2].ends_with("bridge --no-start"));
        assert!(!prepared.argv[2].contains("--stdio"));
        assert_eq!(
            std::fs::read_to_string(remote.join("calls")).unwrap(),
            "status\ninstall\n--daemon\nstatus\n"
        );
        prepare_managed_node(
            &client,
            &id,
            "worker's label",
            &[],
            BinaryPolicy::RequireInstalled,
            None,
        )
        .await
        .unwrap();
        let calls = std::fs::read_to_string(remote.join("calls")).unwrap();
        assert_eq!(
            calls.matches("install").count(),
            1,
            "live daemon is reused on reconnect"
        );
        std::fs::remove_dir_all(remote).unwrap();
    }
    #[test]
    fn targets_and_versions_fail_closed() {
        for target in ["sg-node", "dev@sg.example", "192.0.2.1"] {
            assert!(validate_target(target).is_ok());
        }
        for target in [
            "-oProxyCommand=id",
            "a;id",
            "a b",
            "a\nb",
            "a@@b",
            "a@-b",
            "",
        ] {
            assert!(validate_target(target).is_err());
        }
        assert!(version_compatible("remuda 0.1.0\ntarget=linux\n", "0.1.0"));
        assert!(!version_compatible("remuda 0.2.0", "0.1.0"));
        assert!(!version_compatible("other 0.1.0", "0.1.0"));
    }
}
