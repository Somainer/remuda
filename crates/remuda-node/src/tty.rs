//! TTY binary framing and per-instance PTY bridges.

use crate::NodeError;
use remuda_driver::{HerdrTty, LocalPty, ProgressBar, PtySnapshot, SnapshotSource, TTY_SNAPSHOT_MAX, TtyBridge};
use remuda_protocol::{BinaryChannel, Id, InstanceId, StreamUuid, U64, encode_binary_frame};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

/// Number of bytes in the protocol v1 binary frame header.
pub const TTY_FRAME_HEADER_BYTES: usize = 32;
/// Binary channel value for TTY output.
pub const TTY_CHANNEL_OUTPUT: u8 = BinaryChannel::TtyOutput as u8;
/// Binary channel value for TTY input.
pub const TTY_CHANNEL_INPUT: u8 = BinaryChannel::TtyInput as u8;
/// Default PTY size before the first resize.
pub const TTY_DEFAULT_COLS: u16 = 80;
/// Default PTY size before the first resize.
pub const TTY_DEFAULT_ROWS: u16 = 24;
/// Default `maxTtyInputBytes`.
pub const TTY_MAX_INPUT_BYTES: usize = 4096;

/// Encode one protocol v1 TTY output frame.
pub fn encode_tty_frame(stream_id: &Id, offset: u64, payload: &[u8]) -> Result<Vec<u8>, NodeError> {
    encode_channel_frame(BinaryChannel::TtyOutput, stream_id, offset, payload)
}

/// Encode one protocol v1 TTY input frame.
pub fn encode_tty_input(stream_id: &Id, payload: &[u8]) -> Result<Vec<u8>, NodeError> {
    encode_channel_frame(BinaryChannel::TtyInput, stream_id, 0, payload)
}

fn encode_channel_frame(
    channel: BinaryChannel,
    stream_id: &Id,
    offset: u64,
    payload: &[u8],
) -> Result<Vec<u8>, NodeError> {
    let stream_uuid = StreamUuid::from_prefixed_id(stream_id.as_str())
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
    encode_binary_frame(channel, stream_uuid, offset, payload)
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))
}

/// Live output (and stream-open) events for Hub/stdio pumps.
#[derive(Debug, Clone)]
pub enum TtyEvent {
    /// A new stream is live; Hub should bind stream UUID → instance.
    Open {
        /// Instance that owns the stream.
        instance_id: InstanceId,
        /// Stream identity (`tty_…`).
        stream_id: Id,
    },
    /// Raw output bytes to wrap in a `tty.frame` binary envelope.
    Bytes {
        /// Instance that owns the stream.
        instance_id: InstanceId,
        /// Stream identity (`tty_…`).
        stream_id: Id,
        /// Byte offset of this payload.
        offset: u64,
        /// ANSI / PTY bytes.
        payload: Vec<u8>,
    },
    /// Actual mode observed by the local terminal emulator.
    Mode {
        /// Instance that owns the stream.
        instance_id: InstanceId,
        /// Stream identity.
        stream_id: Id,
        /// Whether DEC alternate screen is currently active.
        alt_screen: bool,
    },
    /// `OSC 9;4` progress observed by the local terminal emulator
    /// (native-config, 2026-09-16). Emitted on edges only.
    Progress {
        /// Instance that owns the stream.
        instance_id: InstanceId,
        /// Stream identity.
        stream_id: Id,
        /// Parsed progress state.
        progress: ProgressBar,
    },
}

/// Wire spelling of a [`ProgressBar`] for the additive `progress` field.
#[must_use]
pub(crate) fn progress_json(progress: ProgressBar) -> Value {
    let mut value = json!({ "state": progress.state_str() });
    if let Some(percent) = progress.percent() {
        value["percent"] = json!(percent);
    }
    value
}

/// Result of [`TtyRegistry::attach`].
#[derive(Debug, Clone)]
pub struct TtyAttach {
    /// Stream identity.
    pub stream_id: Id,
    /// Stream epoch.
    pub stream_epoch: Id,
    /// Snapshot bytes to replay before live frames.
    pub snapshot: Vec<u8>,
    /// Offset of the first snapshot byte.
    pub available_from: u64,
    /// Offset after the snapshot (live starts here).
    pub next_offset: u64,
    /// Current columns.
    pub cols: u16,
    /// Current rows.
    pub rows: u16,
    /// `?1049` was active when the snapshot was taken, so the client is looking
    /// at a full-screen TUI and must not hijack the wheel for local scrollback
    /// (D-028 §4.6). None on raw-ring fallback: that path cannot know the mode.
    pub alt_screen: Option<bool>,
    /// Parsed `OSC 9;4` progress at snapshot time for the header bar. None on
    /// carriers without an emulator and before the harness reports progress.
    pub progress: Option<ProgressBar>,
}

impl TtyAttach {
    /// JSON-RPC result body (includes `snapshotBase64` when non-empty).
    ///
    /// D-016 wire format is unchanged; `altScreen` and `progress` are the
    /// additive fields (D-028 §4.6, native-config), so an older client simply
    /// ignores them.
    pub fn into_json(self) -> Result<Value, NodeError> {
        let snapshot_b64 = if self.snapshot.is_empty() {
            None
        } else {
            Some(data_encoding_base64(&self.snapshot))
        };
        Ok(json!({
            "streamId": self.stream_id,
            "streamEpoch": self.stream_epoch,
            "representation": "pty-bytes",
            "nextOffset": self.next_offset.to_string(),
            "availableFrom": self.available_from.to_string(),
            "screenSnapshotRef": null,
            "snapshotAtOffset": {"state": "known", "value": self.available_from.to_string()},
            "writerLease": null,
            "snapshotBase64": snapshot_b64,
            "cols": self.cols,
            "rows": self.rows,
            "altScreen": self.alt_screen,
            "progress": self.progress.map(progress_json),
        }))
    }
}

fn data_encoding_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied();
        let b2 = bytes.get(i + 2).copied();
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        if b1.is_none() {
            out.push('=');
            out.push('=');
        } else {
            out.push(
                TABLE[(((b1.unwrap_or(0) & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char,
            );
            if b2.is_none() {
                out.push('=');
            } else {
                out.push(TABLE[(b2.unwrap_or(0) & 0x3f) as usize] as char);
            }
        }
        i += 3;
    }
    out
}

enum Backend {
    /// `ctl` carries writes/resizes into the control task; `alt_screen` is
    /// the byte-stream scanner's current DEC alt-screen reading.
    Herdr {
        ctl: mpsc::Sender<Ctl>,
        alt_screen: Arc<AtomicBool>,
    },
    Local(Arc<dyn LocalPty>),
}

enum Ctl {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

struct TtySession {
    stream_id: Id,
    stream_epoch: Id,
    offset: AtomicU64,
    ring: Mutex<VecDeque<u8>>,
    backend: Backend,
    cols: AtomicU16,
    rows: AtomicU16,
}

/// Per-Node TTY bridges (one per pty-backed instance).
#[derive(Clone)]
pub struct TtyRegistry {
    sessions: Arc<RwLock<BTreeMap<InstanceId, Arc<TtySession>>>>,
    events: broadcast::Sender<TtyEvent>,
}

impl Default for TtyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TtyRegistry {
    /// Bounded live fan-out for Hub/stdio pumps.
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            sessions: Arc::new(RwLock::new(BTreeMap::new())),
            events,
        }
    }

    /// Subscribe to stream-open and output events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TtyEvent> {
        self.events.subscribe()
    }

    /// Start a bridge for `instance_id`. Replaces any previous session.
    pub async fn start(
        &self,
        instance_id: InstanceId,
        bridge: TtyBridge,
        cols: u16,
        rows: u16,
    ) -> Result<Id, NodeError> {
        self.stop(&instance_id).await;
        let stream_id = Id::new("tty")?;
        let stream_epoch = Id::new("epoch")?;
        let cols = cols.max(1);
        let rows = rows.max(1);
        // Announce ownership before spawning a pump that can immediately emit
        // bytes or a mode edge; Hub rejects unbound stream notifications.
        let _ = self.events.send(TtyEvent::Open {
            instance_id: instance_id.clone(),
            stream_id: stream_id.clone(),
        });
        let session = match bridge {
            TtyBridge::Herdr { client, pane_id } => {
                let (ctl_tx, ctl_rx) = mpsc::channel(64);
                // A fresh pane starts on its primary screen; the pump's
                // scanner promotes this the instant the stream says otherwise.
                let alt_screen = Arc::new(AtomicBool::new(false));
                let session = Arc::new(TtySession {
                    stream_id: stream_id.clone(),
                    stream_epoch,
                    offset: AtomicU64::new(0),
                    ring: Mutex::new(VecDeque::new()),
                    backend: Backend::Herdr {
                        ctl: ctl_tx,
                        alt_screen: Arc::clone(&alt_screen),
                    },
                    cols: AtomicU16::new(cols),
                    rows: AtomicU16::new(rows),
                });
                spawn_herdr_pump(
                    Arc::clone(&session),
                    instance_id.clone(),
                    self.events.clone(),
                    TtyBridge::Herdr { client, pane_id },
                    cols,
                    rows,
                    ctl_rx,
                    alt_screen,
                );
                session
            }
            TtyBridge::Local(local) => {
                let rx = local.subscribe();
                let snapshot = local.snapshot();
                let start_len = snapshot.len() as u64;
                let session = Arc::new(TtySession {
                    stream_id: stream_id.clone(),
                    stream_epoch,
                    offset: AtomicU64::new(start_len),
                    ring: Mutex::new(VecDeque::from(snapshot)),
                    backend: Backend::Local(Arc::clone(&local)),
                    cols: AtomicU16::new(cols),
                    rows: AtomicU16::new(rows),
                });
                spawn_local_pump(
                    Arc::clone(&session),
                    instance_id.clone(),
                    self.events.clone(),
                    rx,
                );
                session
            }
        };
        self.sessions
            .write()
            .await
            .insert(instance_id.clone(), Arc::clone(&session));
        Ok(stream_id)
    }

    /// Snapshot + stream metadata for a follower attach.
    pub async fn attach(&self, instance_id: &InstanceId) -> Result<TtyAttach, NodeError> {
        let session = self
            .sessions
            .read()
            .await
            .get(instance_id)
            .cloned()
            .ok_or_else(|| NodeError::InvalidRequest("instance has no TTY bridge".into()))?;
        let snapshot: PtySnapshot = match &session.backend {
            Backend::Local(local) => {
                let snap = local.screen_snapshot();
                if snap.bytes.is_empty() {
                    PtySnapshot::raw_ring(session.ring.lock().await.iter().copied().collect())
                } else {
                    snap
                }
            }
            Backend::Herdr { .. } => {
                PtySnapshot::raw_ring(session.ring.lock().await.iter().copied().collect())
            }
        };
        // Herdr carriers have no emulator-backed repaint, but the relay tracks
        // DEC alt-screen deterministically from the byte stream, so its
        // reading is trustworthy even when the snapshot is a raw ring slice.
        let herdr_alt_screen = match &session.backend {
            Backend::Herdr { alt_screen, .. } => Some(alt_screen.load(Ordering::SeqCst)),
            Backend::Local(_) => None,
        };
        // OSC 9;4 is parsed by the local carrier's emulator. The herdr relay
        // sees rendered damage frames (no OSC), so it contributes nothing.
        let local_progress = match &session.backend {
            Backend::Local(local) => local.progress(),
            Backend::Herdr { .. } => None,
        };
        let next_offset = session.offset.load(Ordering::SeqCst);
        // A repaint is synthesized, not a slice of the stream, so it occupies
        // no offset range of its own. Anchoring it at `next_offset` keeps the
        // client's offset accounting continuous: the snapshot paints the screen
        // and the next live byte is the first one it has not seen.
        let available_from = match snapshot.source {
            SnapshotSource::Repaint => next_offset,
            SnapshotSource::RawRing => next_offset.saturating_sub(snapshot.bytes.len() as u64),
        };
        tracing::debug!(
            instance_id = %instance_id.as_id(),
            source = snapshot.source.label(),
            alt_screen = snapshot.alt_screen,
            bytes = snapshot.bytes.len(),
            "tty attach snapshot"
        );
        Ok(TtyAttach {
            stream_id: session.stream_id.clone(),
            stream_epoch: session.stream_epoch.clone(),
            snapshot: snapshot.bytes,
            available_from,
            next_offset,
            cols: session.cols.load(Ordering::SeqCst),
            rows: session.rows.load(Ordering::SeqCst),
            alt_screen: herdr_alt_screen.or_else(|| {
                (snapshot.source == SnapshotSource::Repaint).then_some(snapshot.alt_screen)
            }),
            progress: local_progress,
        })
    }

    /// Stream id for an instance, if a bridge is live.
    pub async fn stream_id(&self, instance_id: &InstanceId) -> Option<Id> {
        self.sessions
            .read()
            .await
            .get(instance_id)
            .map(|session| session.stream_id.clone())
    }

    /// Write raw PTY bytes. Enforces [`TTY_MAX_INPUT_BYTES`].
    pub async fn write_bytes(
        &self,
        instance_id: &InstanceId,
        bytes: &[u8],
    ) -> Result<(), NodeError> {
        if bytes.len() > TTY_MAX_INPUT_BYTES {
            return Err(NodeError::InvalidRequest(format!(
                "tty input exceeds {TTY_MAX_INPUT_BYTES} bytes"
            )));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let session = self
            .sessions
            .read()
            .await
            .get(instance_id)
            .cloned()
            .ok_or_else(|| NodeError::InvalidRequest("instance has no TTY bridge".into()))?;
        match &session.backend {
            Backend::Herdr { ctl, .. } => {
                ctl.send(Ctl::Write(bytes.to_vec()))
                    .await
                    .map_err(|_| NodeError::DriverUnavailable)?;
            }
            Backend::Local(local) => {
                local
                    .write_bytes(bytes)
                    .await
                    .map_err(|err| NodeError::Driver(err.to_string()))?;
            }
        }
        Ok(())
    }

    /// Resize the live PTY.
    pub async fn resize(
        &self,
        instance_id: &InstanceId,
        cols: u16,
        rows: u16,
    ) -> Result<(u16, u16), NodeError> {
        if cols == 0 || rows == 0 {
            return Err(NodeError::InvalidRequest(
                "tty.resize cols and rows must be positive".into(),
            ));
        }
        let session = self
            .sessions
            .read()
            .await
            .get(instance_id)
            .cloned()
            .ok_or_else(|| NodeError::InvalidRequest("instance has no TTY bridge".into()))?;
        match &session.backend {
            Backend::Herdr { ctl, .. } => {
                ctl.send(Ctl::Resize { cols, rows })
                    .await
                    .map_err(|_| NodeError::DriverUnavailable)?;
            }
            Backend::Local(local) => {
                local
                    .resize(cols, rows)
                    .await
                    .map_err(|err| NodeError::Driver(err.to_string()))?;
            }
        }
        session.cols.store(cols, Ordering::SeqCst);
        session.rows.store(rows, Ordering::SeqCst);
        Ok((cols, rows))
    }

    /// Drop the bridge without closing the native process.
    pub async fn stop(&self, instance_id: &InstanceId) {
        self.sessions.write().await.remove(instance_id);
    }
}

fn spawn_herdr_pump(
    session: Arc<TtySession>,
    instance_id: InstanceId,
    events: broadcast::Sender<TtyEvent>,
    bridge: TtyBridge,
    cols: u16,
    rows: u16,
    mut ctl_rx: mpsc::Receiver<Ctl>,
    alt_screen_slot: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let TtyBridge::Herdr { client, pane_id } = bridge else {
            return;
        };
        let mut observer = match HerdrTty::open(&client, &pane_id, cols, rows).await {
            Ok(observer) => observer,
            Err(err) => {
                tracing::warn!(%err, pane_id, "herdr terminal session control failed");
                return;
            }
        };
        // Byte-stream mode observation (D-028 §4.6 baseline): no herdr API
        // needed — the relay scans the same bytes the web renders for
        // DECSET/DECRST 1049/1047/47 and RIS. A flip is published before the
        // bytes that caused it, so the badge/wheel policy update ahead of paint.
        let mut last_mode = false;
        loop {
            tokio::select! {
                frame = observer.next_output() => {
                    let Some(frame) = frame else { break; };
                    let Ok((bytes, _full, alt_screen)) = frame else { break; };
                    if bytes.is_empty() {
                        continue;
                    }
                    if alt_screen != last_mode {
                        last_mode = alt_screen;
                        alt_screen_slot.store(alt_screen, Ordering::SeqCst);
                        let _ = events.send(TtyEvent::Mode {
                            instance_id: instance_id.clone(),
                            stream_id: session.stream_id.clone(),
                            alt_screen,
                        });
                    }
                    push_output(&session, &instance_id, &events, &bytes).await;
                }
                ctl = ctl_rx.recv() => {
                    match ctl {
                        Some(Ctl::Write(bytes)) => {
                            if observer.write_bytes(&bytes).await.is_err() {
                                break;
                            }
                        }
                        Some(Ctl::Resize { cols, rows }) => {
                            if observer.resize(cols, rows).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });
}

fn spawn_local_pump(
    session: Arc<TtySession>,
    instance_id: InstanceId,
    events: broadcast::Sender<TtyEvent>,
    mut rx: broadcast::Receiver<Vec<u8>>,
) {
    tokio::spawn(async move {
        let mut last_mode = None;
        let mut last_progress = None;
        loop {
            match rx.recv().await {
                Ok(bytes) if !bytes.is_empty() => {
                    if let Backend::Local(local) = &session.backend {
                        if let Some(alt_screen) = local.alt_screen()
                            && last_mode != Some(alt_screen)
                        {
                            last_mode = Some(alt_screen);
                            let _ = events.send(TtyEvent::Mode {
                                instance_id: instance_id.clone(),
                                stream_id: session.stream_id.clone(),
                                alt_screen,
                            });
                        }
                        // OSC 9;4 edges, the channel c-ttymode used for
                        // alt-screen: published before the bytes that carry
                        // the sequence so the header updates ahead of paint.
                        let progress = local.progress();
                        if progress != last_progress {
                            last_progress = progress;
                            if let Some(progress) = progress {
                                let _ = events.send(TtyEvent::Progress {
                                    instance_id: instance_id.clone(),
                                    stream_id: session.stream_id.clone(),
                                    progress,
                                });
                            }
                        }
                    }
                    push_output(&session, &instance_id, &events, &bytes).await;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

async fn push_output(
    session: &TtySession,
    instance_id: &InstanceId,
    events: &broadcast::Sender<TtyEvent>,
    bytes: &[u8],
) {
    {
        let mut ring = session.ring.lock().await;
        ring.extend(bytes.iter().copied());
        while ring.len() > TTY_SNAPSHOT_MAX {
            ring.pop_front();
        }
    }
    let offset = session
        .offset
        .fetch_add(bytes.len() as u64, Ordering::SeqCst);
    let _ = events.send(TtyEvent::Bytes {
        instance_id: instance_id.clone(),
        stream_id: session.stream_id.clone(),
        offset,
        payload: bytes.to_vec(),
    });
}

/// Decode a binary input frame into `(stream uuid, payload)`.
pub fn decode_tty_input(
    frame: &[u8],
    max_payload: u32,
) -> Result<(StreamUuid, Vec<u8>), NodeError> {
    let (header, payload) = remuda_protocol::decode_binary_frame(frame, max_payload)
        .map_err(|err| NodeError::InvalidRequest(err.to_string()))?;
    if header.channel != BinaryChannel::TtyInput {
        return Err(NodeError::InvalidRequest(
            "binary tty input requires channel 3".into(),
        ));
    }
    Ok((header.stream_uuid, payload.to_vec()))
}

impl TtyAttach {
    /// Offset as protocol [`U64`].
    #[must_use]
    pub fn next_offset_u64(&self) -> U64 {
        U64(self.next_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_protocol_v1_header() {
        let stream = Id::new("tty").expect("registered ID prefix");
        let frame = encode_tty_frame(&stream, 7, b"abc").expect("valid frame");
        assert_eq!(frame.len(), 35);
        assert_eq!(&frame[..4], &[1, TTY_CHANNEL_OUTPUT, 0, 0]);
        assert_eq!(&frame[20..28], &7_u64.to_be_bytes());
        assert_eq!(&frame[28..32], &3_u32.to_be_bytes());
        assert_eq!(&frame[32..], b"abc");
    }

    #[test]
    fn input_frame_uses_channel_three() {
        let stream = Id::new("tty").expect("registered ID prefix");
        let frame = encode_tty_input(&stream, b"\x1b[<0;1;1M").expect("valid frame");
        assert_eq!(frame[1], TTY_CHANNEL_INPUT);
        let (uuid, payload) = decode_tty_input(&frame, 4096).expect("decode");
        assert_eq!(payload, b"\x1b[<0;1;1M");
        assert_eq!(uuid, StreamUuid::from_prefixed_id(stream.as_str()).unwrap());
    }
}
