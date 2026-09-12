//! Control-plane coordination with driver capabilities via [`FakeDriver`].
//!
//! `generic-pty` (x-grokdrv) advertises `tty-attach` / `live-attach`. FakeDriver
//! is the in-process Claude-print stub used by tests: keys/screen still go
//! through Hub `tty.write` / journal, but the matrix says print cannot attach.

use remuda_driver::{Driver, FakeDriver, MatrixMark, capability_matrix};
use remuda_protocol::{CapabilityName, CapabilityState, DriverKind};

#[tokio::test]
async fn fake_driver_snapshot_matches_print_matrix() {
    let driver = FakeDriver::new();
    let snap = driver.capabilities().await.expect("capabilities");
    assert_eq!(snap.driver_kind, DriverKind::ClaudePrint);
    assert_eq!(
        snap.capabilities.tty_attach.state,
        CapabilityState::Unsupported
    );
    assert_eq!(
        capability_matrix(DriverKind::ClaudePrint, CapabilityName::TtyAttach),
        MatrixMark::NotProvided
    );
}

#[test]
fn generic_pty_advertises_tty_attach_for_keys_and_screen() {
    assert_eq!(
        capability_matrix(DriverKind::GenericPty, CapabilityName::TtyAttach),
        MatrixMark::SupportedStar
    );
    assert_eq!(
        capability_matrix(DriverKind::GenericPty, CapabilityName::LiveAttach),
        MatrixMark::SupportedStar
    );
    assert_eq!(
        capability_matrix(DriverKind::ClaudePty, CapabilityName::TtyAttach),
        MatrixMark::SupportedStar
    );
}
