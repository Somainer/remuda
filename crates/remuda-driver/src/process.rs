//! Process identity helpers used when a supervisor records a spawn.

use remuda_protocol::{Id, ProcessIdentity};
use std::fs;
use std::path::Path;

/// Snapshot the current process as a [`ProcessIdentity`].
pub fn current_process_identity(supervisor_id: Id) -> ProcessIdentity {
    let pid = std::process::id();
    ProcessIdentity {
        pid,
        birth_id: birth_id(pid),
        supervisor_id,
    }
}

fn birth_id(pid: u32) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Some(starttime) = linux_starttime(pid) {
            return format!("pid:{pid}:starttime:{starttime}");
        }
    }
    match std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(file_identity)
    {
        Some(identity) => format!("pid:{pid}:{identity}"),
        None => format!("pid:{pid}"),
    }
}

#[cfg(target_os = "linux")]
fn linux_starttime(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .map(ToOwned::to_owned)
}

fn file_identity(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("dev{}:ino{}", meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        Some(format!("len{}", meta.len()))
    }
}
