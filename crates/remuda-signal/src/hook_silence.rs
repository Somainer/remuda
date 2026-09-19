//! Why a promoted instance's hook channel has gone quiet.
//!
//! The live strip greys its elapsed reading once the hook tier falls silent,
//! but "silent" by itself never names the cause. These are the Node-side
//! checks that actually can, in dependency order:
//!
//! 1. [`HookSilenceReason::RelayMissing`] — the pinned relay binary a hook
//!    execs is absent or no longer executable (a rebuild deleted the path the
//!    live pin degraded to).
//! 2. [`HookSilenceReason::SocketRefused`] — the relay exists but a connect to
//!    the instance hook socket fails (nothing is accepting hook deliveries).
//! 3. [`HookSilenceReason::LinkStalled`] — relay and socket are up, but the
//!    journal pump has not advanced within its bound (events can arrive and go
//!    nowhere — the 5 s non-blocking give-up in the relay then loses a Stop).
//!
//! The badge names exactly the first check that failed — never a guess — and
//! with every check healthy there is no reason, so absent a result the badge
//! stays exactly as it reads today.

use std::path::Path;
use std::time::{Duration, Instant};

/// Stable wire spelling of one failed check; mirrored by the web badge copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSilenceReason {
    /// The pinned relay path is absent or not executable.
    RelayMissing,
    /// A connect to the instance hook socket was refused/timed out.
    SocketRefused,
    /// The journal pump has not advanced within its bound.
    LinkStalled,
}

impl HookSilenceReason {
    /// Snake-case id carried on the diagnostic (`relay-missing`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RelayMissing => "relay-missing",
            Self::SocketRefused => "socket-refused",
            Self::LinkStalled => "link-stalled",
        }
    }
}

/// Probe outcomes, split out so the decision itself is pure and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookSilenceProbes {
    /// The pinned relay binary exists, is a regular file, and is executable.
    pub relay_executable: bool,
    /// Something accepted a connection on the instance hook socket.
    pub socket_listening: bool,
    /// The journal pump advanced within the freshness bound.
    pub link_advanced: bool,
}

/// Pick the first failed check in dependency order. An earlier failure makes
/// the later hops unknowable, so reporting the first is honest and specific.
#[must_use]
pub fn classify(probes: &HookSilenceProbes) -> Option<HookSilenceReason> {
    if !probes.relay_executable {
        return Some(HookSilenceReason::RelayMissing);
    }
    if !probes.socket_listening {
        return Some(HookSilenceReason::SocketRefused);
    }
    if !probes.link_advanced {
        return Some(HookSilenceReason::LinkStalled);
    }
    None
}

/// True when `path` names a present, executable regular file.
#[must_use]
pub fn relay_executable(path: Option<&Path>) -> bool {
    let Some(path) = path else { return false };
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && has_exec_bit(&metadata),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn has_exec_bit(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn has_exec_bit(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// A single bounded connect to the hook socket. Acceptance proves a listener;
/// a refused/absent path or a timeout is `false`. The probe drops the
/// connection immediately — the server logs the empty delivery and is
/// unaffected (its accept loop is per-connection).
pub async fn socket_listening(path: &Path) -> bool {
    matches!(
        tokio::time::timeout(SOCKET_PROBE_TIMEOUT, tokio::net::UnixStream::connect(path)).await,
        Ok(Ok(_stream))
    )
}

const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// True when the pump last advanced within `bound`; never-advanced is stalled.
#[must_use]
pub fn link_fresh(last_advance: Option<Instant>, bound: Duration) -> bool {
    last_advance.is_some_and(|at| at.elapsed() <= bound)
}

/// Run every check for one instance. `relay` is the pinned relay path (when
/// the Node could resolve one), `socket` the instance hook socket, and
/// `last_advance` the most recent hook record from the foreground pid.
pub async fn diagnose(
    relay: Option<&Path>,
    socket: &Path,
    last_advance: Option<Instant>,
    link_bound: Duration,
) -> Option<HookSilenceReason> {
    let probes = HookSilenceProbes {
        relay_executable: relay_executable(relay),
        socket_listening: socket_listening(socket).await,
        link_advanced: link_fresh(last_advance, link_bound),
    };
    classify(&probes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_nonexecutable_relay_is_relay_missing() {
        let dir = std::env::temp_dir().join(format!("remuda-hook-silence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let relay = dir.join("relay");

        // Absent.
        assert!(!relay_executable(Some(&relay)));
        assert_eq!(
            classify(&HookSilenceProbes {
                relay_executable: false,
                socket_listening: false,
                link_advanced: false,
            }),
            Some(HookSilenceReason::RelayMissing)
        );

        // Present but not executable.
        std::fs::write(&relay, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(!relay_executable(Some(&relay)));

        // Executable: the check passes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(relay_executable(Some(&relay)));
        assert!(!relay_executable(None));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unbound_socket_is_socket_refused_once_the_relay_is_present() {
        // Fixed short root: std::env::temp_dir honours a long TMPDIR and the
        // pid-suffixed directory then exceeds sun_path.
        let root = crate::runtime_dir::testutil::ShortDir::new();
        let dir = root.path();
        let socket = dir.join("hooks.sock");

        // Nothing ever bound it.
        assert!(!socket_listening(&socket).await);
        let probes = HookSilenceProbes {
            relay_executable: true,
            socket_listening: false,
            link_advanced: true,
        };
        assert_eq!(classify(&probes), Some(HookSilenceReason::SocketRefused));

        // Bind it: a connect now succeeds.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        assert!(socket_listening(&socket).await);
        let probes = HookSilenceProbes {
            relay_executable: true,
            socket_listening: true,
            link_advanced: true,
        };
        assert_eq!(classify(&probes), None);
        drop(listener);
    }

    #[test]
    fn a_pump_that_has_not_advanced_is_link_stalled_and_an_absent_relay_wins() {
        let bound = Duration::from_millis(200);
        assert!(link_fresh(Some(Instant::now()), bound));
        let stale = Instant::now().checked_sub(bound * 5).unwrap();
        assert!(!link_fresh(Some(stale), bound));
        assert!(!link_fresh(None, bound));

        // relay + socket healthy, pump stale → link-stalled.
        assert_eq!(
            classify(&HookSilenceProbes {
                relay_executable: true,
                socket_listening: true,
                link_advanced: false,
            }),
            Some(HookSilenceReason::LinkStalled)
        );
        // The relay check precedes the link check.
        assert_eq!(
            classify(&HookSilenceProbes {
                relay_executable: false,
                socket_listening: true,
                link_advanced: false,
            }),
            Some(HookSilenceReason::RelayMissing)
        );
    }

    #[tokio::test]
    async fn a_long_instance_dir_probes_the_real_socket_not_the_symlink() {
        // The silence probe is called with the live HookSession's socket
        // path. Under a long data dir that is the short runtime path: the
        // under-instance symlink cannot be connected (sun_path), so probing
        // it would report a false socket-refused.
        let root = tempfile::tempdir().unwrap();
        let instance_dir = root
            .path()
            .join("x".repeat(80))
            .join("instances/ins_01990000-0000-7000-8000-000000000002");
        std::fs::create_dir_all(&instance_dir).unwrap();
        let preferred = instance_dir.join("hook.sock");
        let placement = crate::runtime_dir::place_socket(
            &preferred,
            "01990000-0000-7000-8000-000000000002.sock",
        )
        .unwrap();
        assert!(placement.redirected());
        let listener = tokio::net::UnixListener::bind(placement.bind_path()).unwrap();
        placement.install_link().unwrap();

        assert!(
            socket_listening(placement.bind_path()).await,
            "the real short socket must answer the probe"
        );
        assert!(
            !socket_listening(&preferred).await,
            "the long symlink path is un-connectable and must read as not listening"
        );
        drop(listener);
        placement.remove_link();
        let _ = std::fs::remove_file(placement.bind_path());
    }

    #[test]
    fn the_reason_spelling_is_stable() {
        assert_eq!(HookSilenceReason::RelayMissing.as_str(), "relay-missing");
        assert_eq!(HookSilenceReason::SocketRefused.as_str(), "socket-refused");
        assert_eq!(HookSilenceReason::LinkStalled.as_str(), "link-stalled");
    }
}
