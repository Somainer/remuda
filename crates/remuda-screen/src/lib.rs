//! Screen signatures, dialog parsers, and the PTY terminal emulator (D-028
//! §4.1, §4.2, §4.6, §10).
//!
//! This crate is the signature library §4.2 calls for: **zero herdr
//! dependency**, input is a rendered grid rather than an ANSI-stripped byte
//! tail, and nothing here knows about drivers, interactions, or transports.
//! `remuda-driver` keeps thin adapters over it so existing callers are
//! unchanged.
//!
//! Three layers, in dependency order:
//!
//! - [`ansi`] — the degraded text path, kept byte-identical to the pre-D-028
//!   matchers so the emulator-off default behaves exactly as before.
//! - [`grid`] — [`ScreenGrid`], the rendered screen plus modes and OSC state.
//! - [`emulator`] — [`Emulator`], vt100 over the PTY byte stream, and the
//!   synthesized attach repaint.
//!
//! On top of those, [`signature`] classifies an agent TUI's screen and
//! [`dialog`] reads answerable prompts off it.
//!
//! # Honest fallback
//!
//! §4.6 requires the byte ring to remain the fallback "on any emulator error".
//! [`snapshot`] is where that decision is made and named, so a caller cannot
//! silently ship a degraded snapshot as if it were a repaint.

pub mod ansi;
pub mod dialog;
pub mod emulator;
pub mod grid;
pub mod osc;
pub mod signature;

pub use ansi::{char_tail, screen_tail, strip_ansi};
pub use dialog::{
    SCREEN_BYTES, SCREEN_LINES, ScreenChoice, ScreenRequest, excerpt, screen_request,
    trust_dialog_keys,
};
pub use emulator::{DEFAULT_SCROLLBACK_LINES, Emulator, MAX_COLS, MAX_ROWS};
pub use grid::{ModeSet, OscState, ScreenGrid};
pub use osc::{OscStatus, ProgressBar, progress_active, progress_state};
pub use signature::{HookHealth, ScreenLatch, ScreenStatus, detect_from_screen, screen_status};

/// Where an attach snapshot's bytes came from.
///
/// Reported so the caller can log a degradation instead of quietly serving a
/// worse snapshot (§4.6: "fall back to the raw ring on any emulator error —
/// honest fallback, log it").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSource {
    /// Synthesized repaint: reset + current grid + current mode set.
    Repaint,
    /// A slice of the raw byte ring.
    RawRing,
}

impl SnapshotSource {
    /// Short label for logs and diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Repaint => "repaint",
            Self::RawRing => "raw-ring",
        }
    }
}

/// An attach snapshot and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Bytes to replay before live frames. D-016 wire format unchanged.
    pub bytes: Vec<u8>,
    /// Which path produced them.
    pub source: SnapshotSource,
    /// `?1049` was active when the snapshot was taken, so the client should
    /// stop treating the viewport as scrollable (§4.6). Always false on the
    /// raw-ring path, which cannot know.
    pub alt_screen: bool,
    /// Parsed `OSC 9;4` state at snapshot time, for the terminal header's
    /// progress bar. `None` when the harness never reported progress or the
    /// snapshot is a raw-ring slice (native-config, 2026-09-16).
    pub progress: Option<ProgressBar>,
}

/// Build an attach snapshot, preferring the emulator's repaint.
///
/// `emulator` is `None` when the feature flag is off or the emulator was never
/// created; `fallback` is the raw ring. A repaint that comes back empty is
/// treated as a failure rather than as an empty screen — an emulator that has
/// seen bytes always paints something, so an empty repaint means the emulator
/// is not in a state worth trusting, and the ring is the honest answer.
#[must_use]
pub fn snapshot(emulator: Option<&Emulator>, fallback: Vec<u8>) -> Snapshot {
    let Some(emulator) = emulator else {
        return Snapshot {
            bytes: fallback,
            source: SnapshotSource::RawRing,
            alt_screen: false,
            progress: None,
        };
    };
    let repaint = emulator.repaint();
    if repaint.is_empty() {
        tracing::warn!(
            "pty emulator produced an empty repaint; falling back to the raw ring snapshot"
        );
        return Snapshot {
            bytes: fallback,
            source: SnapshotSource::RawRing,
            alt_screen: false,
            progress: None,
        };
    }
    Snapshot {
        bytes: repaint,
        source: SnapshotSource::Repaint,
        alt_screen: emulator.alt_screen(),
        progress: emulator.progress(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_an_emulator_the_snapshot_is_the_ring_and_says_so() {
        let snap = snapshot(None, b"raw bytes".to_vec());
        assert_eq!(snap.source, SnapshotSource::RawRing);
        assert_eq!(snap.bytes, b"raw bytes");
        assert!(
            !snap.alt_screen,
            "the ring path cannot know the mode; it must not claim to"
        );
        assert_eq!(snap.source.label(), "raw-ring");
    }

    #[test]
    fn with_an_emulator_the_snapshot_is_a_repaint_carrying_the_alt_flag() {
        let mut emulator = Emulator::new(20, 4);
        emulator.feed(b"\x1b[?1049h\x1b[2J\x1b[Hfullscreen");
        let snap = snapshot(Some(&emulator), b"raw bytes".to_vec());
        assert_eq!(snap.source, SnapshotSource::Repaint);
        assert!(snap.alt_screen);
        assert_ne!(snap.bytes, b"raw bytes");
        let mut replayed = Emulator::new(20, 4);
        replayed.feed(&snap.bytes);
        assert_eq!(replayed.grid().lines[0], "fullscreen");
    }

    #[test]
    fn a_fresh_emulator_still_repaints_a_blank_screen_rather_than_falling_back() {
        // Reset plus an empty grid is a valid snapshot: it is what the client
        // should show. Only a genuinely empty byte string is a failure.
        let emulator = Emulator::new(20, 4);
        let snap = snapshot(Some(&emulator), b"stale ring".to_vec());
        assert_eq!(snap.source, SnapshotSource::Repaint);
        assert!(!snap.bytes.is_empty());
    }
}
