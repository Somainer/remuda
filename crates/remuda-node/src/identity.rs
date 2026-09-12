//! Stable on-disk identity for a Node host.

use crate::NodeError;
use remuda_protocol::HostId;
use std::io::Write;
use std::path::Path;

/// Load `<node_dir>/host-id`, creating it with owner-only permissions once.
///
/// `create_new` makes concurrent starters converge on the winning identity.
pub fn load_or_create_host_id(node_dir: &Path) -> Result<HostId, NodeError> {
    std::fs::create_dir_all(node_dir)?;
    let path = node_dir.join("host-id");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            let id = HostId::new();
            writeln!(file, "{}", id.as_id())?;
            file.sync_all()?;
            Ok(id)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let text = std::fs::read_to_string(&path)?;
            text.trim().parse().map_err(NodeError::from)
        }
        Err(error) => Err(NodeError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = load_or_create_host_id(dir.path()).expect("first id");
        let second = load_or_create_host_id(dir.path()).expect("second id");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("host-id"))
                .expect("host-id")
                .trim(),
            first.as_id().as_str()
        );
    }

    #[test]
    fn invalid_persisted_identity_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("host-id"), "not-a-host-id\n").expect("fixture");
        assert!(load_or_create_host_id(dir.path()).is_err());
    }
}
