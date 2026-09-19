//! Per-instance hook socket placement (AF_UNIX `sun_path` limit).
//!
//! The hook socket conventionally lives at `<instance dir>/hook.sock`, but an
//! instance id is 40 bytes and a data directory under a long home path pushes
//! that address past the platform's `sockaddr_un.sun_path` limit (107 bytes on
//! Linux, 103 on macOS). Both bind *and connect* reject such a path with
//! `path must be shorter than SUN_LEN` — the limit applies before symlink
//! resolution.
//!
//! This module owns the product rule:
//!
//! - `<instance dir>/hook.sock` when it is at most
//!   [`SAFE_SOCKET_PATH_BYTES`](remuda_signal::runtime_dir::SAFE_SOCKET_PATH_BYTES);
//! - otherwise a short `<uuid>.sock` in the secure per-user runtime directory,
//!   with `hook.sock` under the instance dir left as a *discovery* symlink.
//!
//! The real short path is what the overlay, shadow hooks, relay and every
//! other wire client use; no client may connect through the long symlink.

use crate::error::{DriverError, DriverResult};
use remuda_signal::runtime_dir::SocketPlacement;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// File name used inside the per-user runtime directory for one instance.
///
/// The instance id is `ins_<uuid>`; the socket carries the bare canonical
/// uuid so the runtime directory stays as short as it can. Inputs that do not
/// have that shape cannot rely on a globally unique last component, so they
/// get a digest of the full instance directory: two different directories can
/// never collide on one runtime socket, however oddly they are named.
fn runtime_socket_name(instance_dir: &Path) -> String {
    let id = instance_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let bare = id.strip_prefix("ins_").unwrap_or(id);
    if Uuid::parse_str(bare).is_ok() {
        format!("{bare}.sock")
    } else {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(instance_dir.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        let hex: String = digest[..10]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("inst-{hex}.sock")
    }
}

/// Choose where this instance's hook socket binds and what discovery link it
/// leaves. Does not bind; the caller binds
/// [`SocketPlacement::bind_path`] and installs the link afterwards.
pub(crate) fn place_instance_socket(instance_dir: &Path) -> DriverResult<SocketPlacement> {
    let preferred = instance_dir.join("hook.sock");
    let name = runtime_socket_name(instance_dir);
    remuda_signal::runtime_dir::place_socket(&preferred, &name).map_err(|error| {
        DriverError::SettingsIsolationUnavailable(format!(
            "hook socket placement is impossible for {}: {error}",
            preferred.display()
        ))
    })
}

/// File name the token broker socket would carry in the runtime directory for
/// a given preferred path.
///
/// The broker is not keyed to an instance id at its bind site, so the name is
/// a stable digest of the preferred path: the same data directory resolves to
/// the same short socket across restarts, and distinct directories can never
/// collide.
pub(crate) fn broker_runtime_name(preferred: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(preferred.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("broker-{hex}.sock")
}

/// Choose placement for the token broker socket, given the conventional
/// (possibly too long) path a caller supplied. Callers bind the returned
/// [`SocketPlacement::bind_path`], install the link, and bake the *real* path
/// into the `apiKeyHelper` script — never the long link.
pub fn place_token_broker_socket(preferred: &Path) -> DriverResult<SocketPlacement> {
    let name = broker_runtime_name(preferred);
    remuda_signal::runtime_dir::place_socket(preferred, &name).map_err(|error| {
        DriverError::CredentialUnavailable(format!(
            "token broker socket placement is impossible for {}: {error}",
            preferred.display()
        ))
    })
}

/// Whether instance sockets under `data_dir` will be redirected, i.e.
/// `<data_dir>/instances/<40-char id>/hook.sock` exceeds the safe length.
///
/// Used for the one-time Node-start warning; the id spelling mirrors
/// `ins_<canonical-uuid>` (40 chars).
#[must_use]
pub fn instance_sockets_would_redirect(data_dir: &Path) -> bool {
    const REPRESENTATIVE_ID: &str = "ins_01990000-0000-7000-8000-000000000000";
    let representative = data_dir
        .join("instances")
        .join(REPRESENTATIVE_ID)
        .join("hook.sock");
    !remuda_signal::runtime_dir::path_fits_sun_path(&representative)
}

/// All per-user runtime directory candidates in preference order (infallible:
/// the list is environment-derived and never performs IO).
///
/// Test fixtures walk these to pick the first under which their full socket
/// address fits, the same way [`place_token_broker_socket`] chooses.
#[must_use]
pub fn runtime_socket_candidates() -> Vec<PathBuf> {
    remuda_signal::runtime_dir::runtime_dir_candidates(remuda_signal::runtime_dir::current_uid())
}

/// The platform's usable `sockaddr_un.sun_path` length, in bytes.
#[must_use]
pub fn runtime_socket_limit() -> usize {
    remuda_signal::runtime_dir::SUN_PATH_LIMIT
}

/// Re-export for callers that need the concrete path pair.
pub(crate) type Placement = SocketPlacement;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp root at a fixed short path.
    ///
    /// Ambient `TMPDIR` can itself be long enough to trigger redirection, so a
    /// test asserting an *in-place* (non-redirected) socket must not use
    /// `tempfile`; the layout must be short under every CI TMPDIR.
    struct ShortTemp(PathBuf);

    impl ShortTemp {
        fn new(tag: &str) -> Self {
            let dir = PathBuf::from(format!("/tmp/remuda-sptest-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ShortTemp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_short_instance_dir_keeps_hook_sock_in_place() {
        // Fixed short root so ambient TMPDIR length cannot redirect.
        let root = ShortTemp::new("short");
        let instance_dir = root
            .path()
            .join("instances/ins_01990000-0000-7000-8000-000000000001");
        std::fs::create_dir_all(&instance_dir).unwrap();
        assert!(instance_dir.join("hook.sock").as_os_str().len() <= 100);
        let placement = place_instance_socket(&instance_dir).unwrap();
        assert!(!placement.redirected());
        assert_eq!(placement.bind_path(), instance_dir.join("hook.sock"));
        assert_eq!(placement.link_path(), None);
    }

    #[test]
    fn a_long_instance_dir_is_redirected_with_a_uuid_socket_name() {
        let root = tempfile::tempdir().unwrap();
        let instance_dir = root
            .path()
            .join("x".repeat(70))
            .join("instances/ins_01990000-0000-7000-8000-000000000009");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let placement = place_instance_socket(&instance_dir).unwrap();
        assert!(placement.redirected());
        assert_eq!(
            placement.bind_path().file_name().unwrap().to_str().unwrap(),
            "01990000-0000-7000-8000-000000000009.sock"
        );
        assert_eq!(
            placement.link_path().unwrap(),
            instance_dir.join("hook.sock").as_path()
        );
        assert!(
            placement.bind_path().as_os_str().len() <= remuda_signal::runtime_dir::SUN_PATH_LIMIT
        );
    }

    #[test]
    fn two_instances_never_share_a_runtime_socket() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("x".repeat(70)).join("instances");
        let first =
            place_instance_socket(&base.join("ins_01990000-0000-7000-8000-000000000001")).unwrap();
        let second =
            place_instance_socket(&base.join("ins_01990000-0000-7000-8000-000000000002")).unwrap();
        assert!(first.redirected() && second.redirected());
        assert_ne!(first.bind_path(), second.bind_path());
    }

    #[test]
    fn non_canonical_instance_dirs_get_a_full_path_digest_name() {
        let root = tempfile::tempdir().unwrap();
        let odd = root
            .path()
            .join("x".repeat(70))
            .join("instances/not-an-id/with space");
        let other = root
            .path()
            .join("x".repeat(70))
            .join("instances/not-an-id/other space");
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let odd_placement = place_instance_socket(&odd).unwrap();
        let other_placement = place_instance_socket(&other).unwrap();
        let name = odd_placement
            .bind_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with("inst-") && name.ends_with(".sock"),
            "{name}"
        );
        assert!(!name.contains('/') && !name.contains(' '), "{name}");
        assert_ne!(
            odd_placement.bind_path(),
            other_placement.bind_path(),
            "distinct oddly-named dirs must not share a runtime socket"
        );
        // Stable for the same directory.
        assert_eq!(
            place_instance_socket(&odd).unwrap().bind_path(),
            odd_placement.bind_path()
        );
    }

    #[test]
    fn broker_names_are_stable_and_distinct() {
        let a = Path::new("/long/a/instances/ins_x/broker.sock");
        let b = Path::new("/long/b/instances/ins_x/broker.sock");
        assert_eq!(broker_runtime_name(a), broker_runtime_name(a));
        assert_ne!(broker_runtime_name(a), broker_runtime_name(b));
        assert!(broker_runtime_name(a).starts_with("broker-"));
        assert!(broker_runtime_name(a).ends_with(".sock"));
    }

    #[test]
    fn redirect_predicate_matches_the_real_layout() {
        let short = Path::new("/var/lib/remuda");
        assert!(!instance_sockets_would_redirect(short));
        let long = PathBuf::from(format!("/home/u/{}", "x".repeat(90)));
        assert!(instance_sockets_would_redirect(&long));
    }
}
