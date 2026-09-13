//! Bounded workspace filesystem probes for service processes, including macOS TCC.

use crate::NodeError;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Remediation for a daemon that lacks macOS protected-folder access.
pub fn workspace_access_guidance(executable: &Path) -> String {
    format!(
        "grant Full Disk Access to {} in System Settings → Privacy & Security, or use a directory outside ~/Documents, ~/Desktop, and ~/Downloads",
        executable.display()
    )
}

/// Warn about a configured protected path without reading it or requiring permission.
/// The caller decides whether the current platform is macOS.
pub fn macos_workspace_guidance(
    path: &Path,
    home: Option<&Path>,
    executable: &Path,
) -> Option<String> {
    let home = home?;
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    ["Documents", "Desktop", "Downloads"]
        .iter()
        .any(|folder| normalized.starts_with(home.join(folder)))
        .then(|| format!(
            "macOS protects this workspace; launchd access is separate from terminal access; {}",
            workspace_access_guidance(executable)
        ))
}

/// Check directory traversal and enumeration in a killable subprocess.
/// A stalled TCC check must never occupy the Node RPC worker indefinitely.
pub fn workspace_access_check(path: &Path) -> Result<(), NodeError> {
    #[cfg(unix)]
    {
        let path = absolute(path)?;
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "CDPATH=; cd \"$1\" || exit; exec /bin/ls -f . >/dev/null",
                "remuda-workspace-probe",
            ])
            .arg(&path);
        let output = bounded_workspace_command(&mut command, &path, PROBE_TIMEOUT)?;
        check_output(&path, &output)
    }
    #[cfg(not(unix))]
    {
        std::fs::read_dir(path).map(|_| ()).map_err(NodeError::from)
    }
}

/// Create the configured workspace before registration, with the same deadline.
pub fn prepare_workspace(path: &Path) -> Result<(), NodeError> {
    #[cfg(unix)]
    {
        let path = absolute(path)?;
        let mut command = Command::new("/bin/mkdir");
        command.arg("-p").arg(&path);
        let output = bounded_workspace_command(&mut command, &path, PROBE_TIMEOUT)?;
        check_output(&path, &output)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    workspace_access_check(path)
}

fn absolute(path: &Path) -> Result<PathBuf, NodeError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn check_output(path: &Path, output: &Output) -> Result<(), NodeError> {
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    Err(access_error(path, detail.trim(), false))
}

pub(crate) fn check_workspace_output(path: &Path, output: &Output) -> Result<(), NodeError> {
    let detail = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && permission_denied(&detail) {
        return Err(access_error(path, detail.trim(), false));
    }
    Ok(())
}

fn permission_denied(detail: &str) -> bool {
    detail.contains("Operation not permitted") || detail.contains("Permission denied")
}

fn access_error(path: &Path, detail: &str, timed_out: bool) -> NodeError {
    let mut message = format!("workspace {} is inaccessible: {detail}", path.display());
    if cfg!(target_os = "macos") && (timed_out || permission_denied(detail)) {
        let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("remuda"));
        message.push_str(if timed_out {
            "; macOS may have denied access; "
        } else {
            "; macOS denied access; "
        });
        message.push_str(&workspace_access_guidance(&binary));
    }
    NodeError::InvalidRequest(message)
}

/// The subprocess must receive its workspace as an argument, not `current_dir`:
/// a denied pre-exec chdir can otherwise stall `spawn` itself before the deadline.
pub(crate) fn bounded_workspace_command(
    command: &mut Command,
    workspace: &Path,
    timeout: Duration,
) -> Result<Output, NodeError> {
    use std::io::Read;
    command
        .current_dir("/")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| access_error(workspace, &error.to_string(), false))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (send, receive) = std::sync::mpsc::channel();
    let read = |pipe: Option<Box<dyn Read + Send>>, is_stderr, send: std::sync::mpsc::Sender<_>| {
        std::thread::spawn(move || {
            let result = (|| {
                let mut data = Vec::new();
                if let Some(mut pipe) = pipe {
                    pipe.read_to_end(&mut data)?;
                }
                Ok::<_, std::io::Error>(data)
            })();
            let _ = send.send((is_stderr, result));
        })
    };
    read(
        stdout.map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        false,
        send.clone(),
    );
    read(
        stderr.map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        true,
        send,
    );
    let mut stdout = None;
    let mut stderr = None;
    let deadline = Instant::now() + timeout;
    loop {
        while let Ok((is_stderr, result)) = receive.try_recv() {
            if is_stderr {
                stderr = Some(result);
            } else {
                stdout = Some(result);
            }
        }
        if let Some(status) = child.try_wait()?
            && stdout.is_some()
            && stderr.is_some()
        {
            return Ok(Output {
                status,
                stdout: stdout.take().unwrap_or_else(|| Ok(Vec::new()))?,
                stderr: stderr.take().unwrap_or_else(|| Ok(Vec::new()))?,
            });
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            if let Ok(pid) = i32::try_from(child.id()) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill();
            // Reap off the request thread: even an uninterruptible filesystem
            // syscall must not turn the error response into another wait.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Err(access_error(workspace, "access probe timed out", true));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_paths_use_components_and_normalize_parent_segments() {
        let home = Path::new("/home/dev");
        let binary = Path::new("/opt/remuda/bin/remuda");
        for path in [
            "Documents/repo",
            "Desktop/repo",
            "Downloads/repo",
            "src/../Documents/repo",
        ] {
            let message = macos_workspace_guidance(&home.join(path), Some(home), binary).unwrap();
            assert!(message.contains("/opt/remuda/bin/remuda"));
            assert!(message.contains("Full Disk Access"));
        }
        for path in ["Documents-other/repo", "Documents/../src", "src/repo"] {
            assert!(macos_workspace_guidance(&home.join(path), Some(home), binary).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn probe_rejects_files_and_missing_paths_and_accepts_unusual_directory_names() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("repo ' with $literal spaces");
        prepare_workspace(&directory).unwrap();
        workspace_access_check(&directory).unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(workspace_access_check(&file).is_err());
        assert!(workspace_access_check(&root.path().join("missing")).is_err());
    }

    #[tokio::test]
    async fn inaccessible_workspace_is_rejected_before_registration_or_instance_acceptance() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(workspace.clone())
            .with_workspace_roots(vec![parent.path().to_path_buf()]);
        assert!(crate::DevNode::new(&config).is_err());
        std::fs::create_dir(&workspace).unwrap();
        let node = crate::DevNode::new(&config).unwrap();
        std::fs::remove_dir(&workspace).unwrap();
        let request = serde_json::from_value(serde_json::json!({"prompt":"fixture"})).unwrap();
        let error = node.create_instance(request).await.unwrap_err();
        assert!(error.to_string().contains("inaccessible"));
        assert!(node.list_instances().unwrap().items.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn stalled_probe_is_killed_and_returns_within_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let start = Instant::now();
        let error =
            bounded_workspace_command(&mut command, Path::new("/tmp"), Duration::from_millis(80))
                .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("timed out"));
        if cfg!(target_os = "macos") {
            assert!(error.to_string().contains("Full Disk Access"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn exited_parent_with_inherited_pipes_is_still_bounded() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let start = Instant::now();
        let error =
            bounded_workspace_command(&mut command, Path::new("/tmp"), Duration::from_millis(80))
                .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("timed out"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn eperm_includes_actionable_daemon_guidance() {
        let error = access_error(
            Path::new("/tmp/workspace"),
            "Operation not permitted",
            false,
        )
        .to_string();
        assert!(error.contains("macOS denied access"));
        assert!(error.contains("System Settings → Privacy & Security"));
        assert!(error.contains("outside ~/Documents"));
    }
}
