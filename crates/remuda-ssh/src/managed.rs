//! Preflight for Hub-supervised hosts. All remote writes stay in a private /tmp directory.

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
    if !host_id.starts_with("hst_")
        || !host_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::Enroll("invalid managed host identity".into()));
    }
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
        "--stdio".into(),
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
    Ok(ManagedNode {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "cd {qdir}/workspace && exec env CODEX_HOME={qdir}/codex REMUDA_DATA_DIR={qdir} REMUDA_CONFIG={qdir}/remuda.toml {command}"
            ),
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
