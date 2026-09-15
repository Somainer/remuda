//! Shared TTY attach surface for herdr-carried and local PTY drivers.

use crate::error::{DriverError, DriverResult};
use async_trait::async_trait;
use remuda_herdr::{Client, TerminalObserver};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Bytes retained for attach-time snapshot replay; D-016.
pub const TTY_SNAPSHOT_MAX: usize = 256 * 1024;

/// How Node streams and writes a live PTY.
#[derive(Clone)]
pub enum TtyBridge {
    /// Herdr pane; Node opens `terminal session control`.
    Herdr {
        /// Socket client (cloneable).
        client: Client,
        /// Native pane id (`w1:p2`).
        pane_id: String,
    },
    /// In-process portable-pty (`shell-pty`).
    Local(Arc<dyn LocalPty>),
}

/// Control session over `herdr terminal session control`.
pub struct HerdrTty {
    observer: TerminalObserver,
}

impl HerdrTty {
    /// Open a writable ANSI stream for `pane_id`.
    pub async fn open(client: &Client, pane_id: &str, cols: u16, rows: u16) -> DriverResult<Self> {
        let observer = TerminalObserver::open_control(client, pane_id, cols.max(1), rows.max(1))
            .await
            .map_err(map_term)?;
        Ok(Self { observer })
    }

    /// Next ANSI payload (`full` is true for a screen reset / snapshot frame,
    /// `alt_screen` is the byte-stream scanner's post-frame DEC mode reading).
    pub async fn next_output(&mut self) -> Option<DriverResult<(Vec<u8>, bool, bool)>> {
        match self.observer.next_frame().await? {
            Ok(frame) => Some(Ok((frame.bytes.to_vec(), frame.full, frame.alt_screen))),
            Err(err) => Some(Err(map_term(err))),
        }
    }

    /// Inject raw bytes (keyboard and mouse).
    pub async fn write_bytes(&mut self, bytes: &[u8]) -> DriverResult<()> {
        self.observer.send_bytes(bytes).await.map_err(map_term)
    }

    /// Resize the PTY (control mode).
    pub async fn resize(&mut self, cols: u16, rows: u16) -> DriverResult<()> {
        self.observer
            .resize(cols.max(1), rows.max(1))
            .await
            .map_err(map_term)
    }
}

fn map_term(err: remuda_herdr::Error) -> DriverError {
    DriverError::Io(std::io::Error::other(err.to_string()))
}

pub use remuda_screen::SnapshotSource;

/// An attach snapshot plus the provenance Node reports and logs.
///
/// D-028 §4.6 replaces the ring slice with a synthesized repaint when the
/// emulator is on, and requires falling back to the ring "on any emulator
/// error — honest fallback, log it". Carrying the source alongside the bytes is
/// what makes that honesty checkable rather than aspirational.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySnapshot {
    /// Bytes to replay before live frames. D-016 wire format unchanged.
    pub bytes: Vec<u8>,
    /// Which path produced them.
    pub source: SnapshotSource,
    /// `?1049` was active when the snapshot was taken. Always false on the
    /// raw-ring path, which has no way to know.
    pub alt_screen: bool,
}

impl PtySnapshot {
    /// Snapshot taken straight from the byte ring.
    #[must_use]
    pub fn raw_ring(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            source: SnapshotSource::RawRing,
            alt_screen: false,
        }
    }
}

/// Local PTY byte pump used by [`TtyBridge::Local`].
#[async_trait]
pub trait LocalPty: Send + Sync {
    /// Subscribe to live output chunks.
    fn subscribe(&self) -> broadcast::Receiver<Vec<u8>>;
    /// Bounded snapshot (last [`TTY_SNAPSHOT_MAX`] bytes).
    fn snapshot(&self) -> Vec<u8>;
    /// Attach snapshot with its provenance (D-028 §4.6).
    ///
    /// Defaults to labelling [`LocalPty::snapshot`] as a ring slice, so a
    /// carrier without an emulator needs no change and cannot accidentally
    /// claim to have produced a repaint.
    fn screen_snapshot(&self) -> PtySnapshot {
        PtySnapshot::raw_ring(self.snapshot())
    }
    /// Current emulator mode without synthesizing a repaint for every chunk.
    /// None means this carrier has no trustworthy mode observation.
    fn alt_screen(&self) -> Option<bool> {
        None
    }
    /// Write raw bytes (keyboard and mouse sequences).
    async fn write_bytes(&self, bytes: &[u8]) -> DriverResult<()>;
    /// Resize the PTY.
    async fn resize(&self, cols: u16, rows: u16) -> DriverResult<()>;
}

/// Map logical `tty.write` keys onto raw PTY bytes.
#[must_use]
pub fn logical_keys_to_bytes(keys: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for key in keys {
        let lower = key.trim().to_ascii_lowercase();
        if lower.is_empty() || lower == "raw" {
            continue;
        }
        let bytes: &[u8] = match lower.as_str() {
            "enter" | "return" | "cr" => b"\r",
            "lf" | "newline" => b"\n",
            "esc" | "escape" => b"\x1b",
            "ctrl+c" | "c-c" | "etx" => b"\x03",
            "ctrl+d" | "eot" => b"\x04",
            "ctrl+z" | "susp" => b"\x1a",
            "ctrl+l" | "ff" => b"\x0c",
            "ctrl+u" => b"\x15",
            "tab" => b"\t",
            "backspace" | "bs" => b"\x7f",
            "up" => b"\x1b[A",
            "down" => b"\x1b[B",
            "right" => b"\x1b[C",
            "left" => b"\x1b[D",
            "home" => b"\x1b[H",
            "end" => b"\x1b[F",
            "space" => b" ",
            other if other.len() == 1 => {
                out.push(other.as_bytes()[0]);
                continue;
            }
            other => {
                out.extend_from_slice(other.as_bytes());
                continue;
            }
        };
        out.extend_from_slice(bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::logical_keys_to_bytes;

    #[test]
    fn maps_enter_esc_and_ctrl_c() {
        assert_eq!(
            logical_keys_to_bytes(&["enter".into(), "esc".into(), "ctrl+c".into()]),
            b"\r\x1b\x03"
        );
    }
}
