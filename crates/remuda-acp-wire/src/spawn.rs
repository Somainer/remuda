//! Spawn `grok agent … stdio` with isolated env.

use std::process::Stdio;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tracing::{debug, warn};

use crate::error::Error;
use crate::types::SpawnSpec;

/// Running grok ACP stdio process with pipes taken for the client.
pub struct GrokChild {
    child: Child,
}

impl GrokChild {
    /// Spawn according to [`SpawnSpec`]. Stdin/stdout/stderr are piped.
    pub fn spawn(spec: &SpawnSpec) -> Result<(Self, ChildStdin, ChildStdout, ChildStderr), Error> {
        let args = spec.args();
        debug!(
            binary = %spec.binary.display(),
            cwd = %spec.cwd.display(),
            ?args,
            "spawning grok acp stdio"
        );
        let mut command = Command::new(&spec.binary);
        command
            .args(&args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("GROK_DISABLE_AUTOUPDATER", "1");
        if let Some(home) = &spec.grok_home {
            command.env("GROK_HOME", home);
        }
        for (key, value) in &spec.extra_env {
            command.env(key, value);
        }
        #[cfg(unix)]
        {
            command.process_group(0);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::BinaryNotFound {
                    path: spec.binary.clone(),
                });
            }
            Err(err) => return Err(err.into()),
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Spawn("child stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Spawn("child stdout missing".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Spawn("child stderr missing".into()))?;
        Ok((Self { child }, stdin, stdout, stderr))
    }

    /// Best-effort SIGKILL (and process group on Unix).
    pub async fn kill(&mut self) {
        if let Err(err) = self.child.kill().await {
            warn!(?err, "killing grok acp child");
        }
    }
}

/// Drain agent stderr so the pipe cannot block, logging each line.
pub async fn drain_stderr(stderr: ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => debug!(target: "remuda_acp_wire::stderr", "{line}"),
            Ok(None) => break,
            Err(err) => {
                debug!(?err, "grok stderr closed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn stdio_argv_matches_protocol() {
        let spec = SpawnSpec::stdio(PathBuf::from("/tmp/remuda-acp-wire"));
        assert_eq!(
            spec.args(),
            [
                "agent",
                "--always-approve",
                "--model",
                "grok-4.6",
                "--no-leader",
                "stdio"
            ]
        );
    }

    #[test]
    fn can_omit_always_approve() {
        let mut spec = SpawnSpec::stdio(PathBuf::from("/tmp"));
        spec.always_approve = false;
        spec.no_leader = false;
        spec.model = "grok-4.5".into();
        assert_eq!(spec.args(), ["agent", "--model", "grok-4.5", "stdio"]);
    }
}
