//! System `ssh` argv: keepalive, BatchMode, optional ControlMaster.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::Error;

/// Default `ServerAliveInterval` (seconds). Matches herdr's keepalive floor.
pub const SERVER_ALIVE_INTERVAL: u32 = 15;
/// Default `ServerAliveCountMax`.
pub const SERVER_ALIVE_COUNT_MAX: u32 = 3;
/// `ControlPersist` seconds when multiplexing is on.
pub const CONTROL_PERSIST_SECS: u32 = 60;

/// How to invoke the OpenSSH client.
#[derive(Debug, Clone)]
pub struct SshOptions {
    /// `ssh` executable. Default `ssh` on `PATH`.
    pub ssh_binary: PathBuf,
    /// When true, add ControlMaster=auto / ControlPath / ControlPersist=60.
    pub control_master: bool,
    /// Directory holding `cm-%C` sockets. Created with mode `0700`.
    pub runtime_dir: PathBuf,
    /// `ConnectTimeout` seconds. `None` omits the option.
    pub connect_timeout_secs: Option<u32>,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            ssh_binary: PathBuf::from("ssh"),
            control_master: false,
            runtime_dir: default_runtime_dir(),
            connect_timeout_secs: Some(10),
        }
    }
}

impl SshOptions {
    /// Keepalive + BatchMode, no ControlMaster.
    #[must_use]
    pub fn keepalive() -> Self {
        Self::default()
    }

    /// Keepalive + ControlMaster=auto at [`default_runtime_dir`].
    #[must_use]
    pub fn with_control_master() -> Self {
        Self {
            control_master: true,
            ..Self::default()
        }
    }

    /// `-o key=value` pairs always passed (and optional ControlMaster).
    #[must_use]
    pub fn o_args(&self) -> Vec<(String, String)> {
        let mut out = vec![
            ("BatchMode".into(), "yes".into()),
            (
                "ServerAliveInterval".into(),
                SERVER_ALIVE_INTERVAL.to_string(),
            ),
            (
                "ServerAliveCountMax".into(),
                SERVER_ALIVE_COUNT_MAX.to_string(),
            ),
            ("NumberOfPasswordPrompts".into(), "0".into()),
        ];
        if let Some(secs) = self.connect_timeout_secs {
            out.push(("ConnectTimeout".into(), secs.to_string()));
            out.push(("ConnectionAttempts".into(), "1".into()));
        }
        if self.control_master {
            out.push(("ControlMaster".into(), "auto".into()));
            out.push((
                "ControlPath".into(),
                self.runtime_dir
                    .join("cm-%C")
                    .to_string_lossy()
                    .into_owned(),
            ));
            out.push(("ControlPersist".into(), CONTROL_PERSIST_SECS.to_string()));
        }
        out
    }

    /// Apply [`Self::o_args`] plus `-T` to a tokio `ssh` command.
    pub fn apply(&self, cmd: &mut Command) -> Result<(), Error> {
        if self.control_master {
            prepare_runtime_dir(&self.runtime_dir)?;
        }
        cmd.arg("-T");
        for (key, value) in self.o_args() {
            cmd.arg("-o").arg(format!("{key}={value}"));
        }
        Ok(())
    }

    /// Same flags for `std::process::Command`.
    pub fn apply_std(&self, cmd: &mut std::process::Command) -> Result<(), Error> {
        if self.control_master {
            prepare_runtime_dir(&self.runtime_dir)?;
        }
        cmd.arg("-T");
        for (key, value) in self.o_args() {
            cmd.arg("-o").arg(format!("{key}={value}"));
        }
        Ok(())
    }
}

/// Client bound to one SSH alias.
#[derive(Debug, Clone)]
pub struct SshClient {
    /// Alias from `~/.ssh/config`.
    pub alias: String,
    /// Invocation flags.
    pub options: SshOptions,
}

/// Captured remote stdout/stderr and status.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Exit code.
    pub status: Option<i32>,
    /// UTF-8 lossy stdout.
    pub stdout: String,
    /// UTF-8 lossy stderr (truncated).
    pub stderr: String,
}

impl ExecOutput {
    /// Treat non-zero as [`Error::Remote`].
    pub fn ok(self) -> Result<Self, Error> {
        if self.status == Some(0) {
            Ok(self)
        } else {
            Err(Error::remote(self.status, self.stderr))
        }
    }
}

impl SshClient {
    /// Bind `alias` with `options`.
    #[must_use]
    pub fn new(alias: impl Into<String>, options: SshOptions) -> Self {
        Self {
            alias: alias.into(),
            options,
        }
    }

    /// Build `ssh [flags] <alias> -- <quoted remote argv>`.
    ///
    /// OpenSSH joins extra arguments with spaces and hands them to the remote
    /// login shell, so each token must be POSIX-quoted.
    pub fn command(&self, remote_argv: &[impl AsRef<OsStr>]) -> Result<Command, Error> {
        crate::managed::validate_target(&self.alias)?;
        let mut cmd = Command::new(&self.options.ssh_binary);
        self.options.apply(&mut cmd)?;
        cmd.arg(&self.alias).arg("--");
        cmd.arg(join_remote_argv(remote_argv)?);
        cmd.kill_on_drop(true);
        Ok(cmd)
    }

    /// Run a remote argv, optional stdin bytes, with a timeout.
    pub async fn exec(
        &self,
        remote_argv: &[impl AsRef<OsStr>],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput, Error> {
        let mut cmd = self.command(remote_argv)?;
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = spawn_ssh(&self.options.ssh_binary, cmd)?;
        if let Some(bytes) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            pipe.write_all(bytes).await?;
            pipe.shutdown().await?;
        }
        wait_output(child, timeout).await
    }

    /// Stream a local file to the remote command's stdin (`ssh … cat > dest`).
    pub async fn exec_stdin_file(
        &self,
        remote_argv: &[impl AsRef<OsStr>],
        file: &Path,
        timeout: Duration,
    ) -> Result<ExecOutput, Error> {
        let mut cmd = self.command(remote_argv)?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn_ssh(&self.options.ssh_binary, cmd)?;
        let mut pipe = child.stdin.take().ok_or_else(|| {
            Error::Io(std::io::Error::other("ssh stdin pipe missing after spawn"))
        })?;
        let mut reader = tokio::fs::File::open(file).await?;
        tokio::io::copy(&mut reader, &mut pipe).await?;
        pipe.shutdown().await?;
        drop(pipe);
        wait_output(child, timeout).await
    }

    /// Close a ControlMaster multiplex socket (`ssh -O exit`).
    pub async fn control_exit(&self) -> Result<(), Error> {
        if !self.options.control_master {
            return Ok(());
        }
        let mut cmd = Command::new(&self.options.ssh_binary);
        self.options.apply(&mut cmd)?;
        cmd.arg("-O")
            .arg("exit")
            .arg(&self.alias)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = spawn_ssh(&self.options.ssh_binary, cmd)?;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        Ok(())
    }
}

/// Short directory for `ControlPath=…/cm-%C`.
///
/// macOS unix-socket paths cap at 104 bytes; `$TMPDIR` under `/var/folders/…`
/// is already too long once `%C` expands, so this prefers `/tmp/rs-<user>`.
#[must_use]
pub fn default_runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("REMUDA_SSH_RUNTIME") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let candidate = PathBuf::from(dir).join("rs");
        if candidate.to_string_lossy().len() <= 40 {
            return candidate;
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "u".into());
    let short: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let short = if short.is_empty() {
        "u".to_string()
    } else {
        short
    };
    PathBuf::from("/tmp").join(format!("rs-{short}"))
}

pub(crate) fn prepare_runtime_dir(dir: &Path) -> Result<(), Error> {
    let display = dir.to_string_lossy();
    if display.contains(char::is_whitespace) {
        return Err(Error::InvalidPath(format!(
            "runtime dir must not contain whitespace: {display}"
        )));
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn spawn_ssh(binary: &Path, mut cmd: Command) -> Result<tokio::process::Child, Error> {
    cmd.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            Error::SshNotFound(binary.to_path_buf())
        } else {
            Error::from(err)
        }
    })
}

async fn wait_output(child: tokio::process::Child, timeout: Duration) -> Result<ExecOutput, Error> {
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| Error::Timeout(timeout))??;
    Ok(ExecOutput {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: crate::error::trim_stderr(String::from_utf8_lossy(&output.stderr).into_owned()),
    })
}

/// Single-quote a string for a POSIX remote shell. Newlines are allowed.
pub fn sh_single_quote(value: &str) -> Result<String, Error> {
    if value.contains('\0') {
        return Err(Error::InvalidPath(value.into()));
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn join_remote_argv(remote_argv: &[impl AsRef<OsStr>]) -> Result<String, Error> {
    let mut joined = String::new();
    for (i, arg) in remote_argv.iter().enumerate() {
        let text = arg
            .as_ref()
            .to_str()
            .ok_or_else(|| Error::InvalidPath("remote argv is not valid UTF-8".into()))?;
        if i > 0 {
            joined.push(' ');
        }
        joined.push_str(&sh_single_quote(text)?);
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_flags_are_fixed() {
        let opts = SshOptions::keepalive();
        let pairs = opts.o_args();
        assert!(pairs.contains(&("BatchMode".into(), "yes".into())));
        assert!(pairs.contains(&("ServerAliveInterval".into(), "15".into())));
        assert!(pairs.contains(&("ServerAliveCountMax".into(), "3".into())));
        assert!(!pairs.iter().any(|(k, _)| k == "ControlMaster"));
    }

    #[test]
    fn control_master_path_uses_percent_c() {
        let mut opts = SshOptions::with_control_master();
        opts.runtime_dir = PathBuf::from("/tmp/rs-testcm");
        let pairs = opts.o_args();
        assert!(pairs.contains(&("ControlMaster".into(), "auto".into())));
        assert!(pairs.contains(&("ControlPersist".into(), "60".into())));
        let path = pairs
            .iter()
            .find(|(k, _)| k == "ControlPath")
            .map(|(_, v)| v.as_str())
            .expect("ControlPath");
        assert_eq!(path, "/tmp/rs-testcm/cm-%C");
        assert!(
            path.replace("%C", "0123456789abcdef0123456789abcdef01234567")
                .len()
                < 104,
            "expanded ControlPath must fit macOS sockaddr_un"
        );
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_single_quote("a'b").unwrap(), "'a'\\''b'");
    }

    #[test]
    fn join_remote_argv_quotes_sh_c_script() {
        let joined = super::join_remote_argv(&["sh", "-c", "if true; then echo hi; fi"]).unwrap();
        assert_eq!(joined, "'sh' '-c' 'if true; then echo hi; fi'");
    }

    #[test]
    fn default_runtime_dir_fits_unix_socket_cap() {
        let dir = default_runtime_dir();
        let expanded = dir
            .join("cm-0123456789abcdef0123456789abcdef01234567")
            .to_string_lossy()
            .into_owned();
        assert!(
            expanded.len() < 104,
            "ControlPath {expanded} is too long for macOS"
        );
    }
}
