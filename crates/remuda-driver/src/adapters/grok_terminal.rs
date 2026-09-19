//! Live stdout from grok `terminal/<toolCallId>.log` files
//! (grok-structural-translation.md §3.2 / §6.1 PR6; D-043 owns the frames).
//!
//! While a grok `run_terminal_command` call is Running, the TUI streams the
//! command's stdout to `<sessionDir>/terminal/<toolCallId>.log` — visible an
//! order of magnitude before the completed frame. [`TerminalTails`] is folded
//! into the grok live projection once per 250 ms poll:
//!
//! * a tail starts when a shell call goes `Running` and reads on the existing
//!   poll rhythm (no extra thread);
//! * appended bytes are published as one [`ResultStage::Partial`] tool result
//!   per poll carrying only the new bytes — byte-offset dedupe, never a
//!   re-emission of the scrollback;
//! * a log that is missing (including one that appears late) is silent: no
//!   error, no observation;
//! * the tail stops at the terminal frame. Remaining bytes flushed in that
//!   same poll are emitted immediately *before* the Final result, which stays
//!   authoritative: the web projection replaces the result outright, so the
//!   partial bytes are never concatenated into the final blocks;
//! * when the log shrinks or is replaced (detected as size below the read
//!   offset), the tail restarts at byte zero and republishes the new content
//!   as a `Replace` snapshot, so a rotated log never shows its head twice.
//!   The rotation signal latches across an intervening empty poll (the file
//!   truncated to zero before the new head is written) or an incomplete UTF-8
//!   prefix, and is cleared only when the Replace is actually emitted. An
//!   equal-or-larger in-place replacement is indistinguishable from growth,
//!   the same limitation [`crate::grok_session::SessionTail`] documents.
//!
//! Path trust: the completed frame's `rawOutput.output_file` is never opened —
//! the 1.0.30 fixture records an absolute path from the capture machine
//! (`/home/dev/…`). Only the relocatable conventional path
//! `<sessionDir>/terminal/<toolCallId>.log` is read, and only when the native
//! id is a single safe path component: an id containing a separator, `..` or
//! an absolute prefix cannot escape the session directory.
//!
//! Revision sequencing: the frame translator numbers Proposed 1 / Running 2 /
//! Final 3 without knowing a tail exists, and D-043 also fires the Running
//! mutation on *every* statusless progress frame (its own counter advances
//! each time). Strictly increasing revisions below the Final are required
//! (`assemble.ts` `newerMutation`), so every post-Open observation on a
//! tracked node — the Running Replace mutations and the Final Close — is
//! re-sequenced through one per-node counter. With no log bytes the values
//! stay byte-identical to D-043 (Running 2, Final 3 base 2); each Partial is
//! an `Append` at the next revision with the exact previous base, and the
//! Final `Close` lands one revision above the last one.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use remuda_protocol::{
    ContentBlock, Id, Knowledge, MutationOperation, NodeMutation, ObservationPayload, ResultStage,
    TextBlock, ToolCallPayload, ToolCallState, ToolOutcome, ToolResultPayload, U64,
};

use super::{AdapterObservation, GrokAdapter, not_emitted};

/// Largest log read published in one Partial observation per call.
const MAX_BYTES_PER_POLL: u64 = 64 * 1024;
/// Largest total log payload published for one call. The Final result remains
/// authoritative for everything past the cap; a runaway log must not blow the
/// observation channel.
const MAX_BYTES_PER_CALL: u64 = 1024 * 1024;
/// Per-call log directory name inside the session directory.
const TERMINAL_DIR: &str = "terminal";
/// The only grok tool whose TUI writes a `terminal/<id>.log` (design doc
/// §3.1/§3.2); other tools never get a tail even if their call reaches
/// Running.
const RUN_TERMINAL_COMMAND: &str = "run_terminal_command";

/// Per-call terminal log tails, folded into the grok live poll.
#[derive(Default)]
pub struct TerminalTails {
    /// Calls seen this adapter lifetime, keyed by native tool call id.
    calls: HashMap<String, CallTail>,
    /// First-seen order, so end-of-batch polling is deterministic.
    order: Vec<String>,
}

/// One tracked call: node revision counter plus its optional live log tail.
struct CallTail {
    /// Journal node shared by the call and its results.
    node: Id,
    /// Native turn id to stamp the partial envelopes with.
    turn_id: Option<String>,
    /// Last revision emitted on the node; 0 before the Open is seen.
    revision: u64,
    /// Bytes published for this call during the current fold pass, so the
    /// inline Running drain and the end-of-batch idle drain share one cap.
    poll_bytes: u64,
    /// A Proposed `Open` for this node passed through the fold.
    opened: bool,
    /// Present between the Running mutation and the terminal frame.
    log: Option<LogTail>,
}

/// Bounded incremental byte reader for one `terminal/<callId>.log`, modelled
/// on [`crate::grok_session::SessionTail`] but returning raw appended bytes
/// (terminal output is not line-buffered — a progress line has no newline
/// until the command exits).
struct LogTail {
    /// Conventional log path under the bound session directory.
    path: PathBuf,
    /// Bytes already consumed.
    offset: u64,
    /// Incomplete trailing UTF-8 sequence held over to the next read.
    pending: Vec<u8>,
    /// Bytes consumed and accounted against the per-call budget.
    consumed: u64,
    /// Per-call budget reached; the slot is kept so the file is never reopened
    /// from byte zero.
    capped: bool,
    /// Rotation detected at or before the last read, not yet published as a
    /// Replace. Latches across empty reads and incomplete UTF-8 prefixes so
    /// the signal cannot be lost between a shrink and the next visible text.
    rotated: bool,
}

impl TerminalTails {
    /// Create an empty tail set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one projected poll: start tails at Running, publish appended log
    /// bytes as Partial results before the batch continues, flush and stop at
    /// the Final result. Best-effort by construction — a missing or unreadable
    /// log never surfaces an error, so a log problem cannot kill the poll
    /// carrier; the terminal frame stays authoritative.
    pub fn fold(&mut self, adapter: &GrokAdapter, out: &mut Vec<AdapterObservation>) {
        let session_dir = adapter
            .binding()
            .and_then(|binding| binding.directory.clone());
        for call in self.calls.values_mut() {
            call.poll_bytes = 0;
        }
        let mut emitted = Vec::with_capacity(out.len() + 4);
        for mut observation in std::mem::take(out) {
            match &observation.payload {
                ObservationPayload::ToolCall(call) => {
                    let item_id = observation.item_id.clone();
                    let frame_revision = call.mutation.revision.0;
                    let is_open = call.mutation.operation == MutationOperation::Open;
                    let node = call.tool_call_id.clone();
                    let tool_name = known_tool_name(call);
                    let running = call.state == ToolCallState::Running;
                    let sequenced = self.note_call(
                        item_id.clone(),
                        node,
                        observation.turn_id.clone(),
                        frame_revision,
                        is_open,
                    );
                    // Re-number a post-Open Running Replace through the shared
                    // counter: D-043 fires one per statusless progress frame
                    // with its own arithmetic, which would otherwise collide
                    // with a Partial already on the node and freeze the card.
                    if !is_open
                        && let Some(revision) = sequenced
                        && let ObservationPayload::ToolCall(call) = &mut observation.payload
                    {
                        call.mutation.revision = U64(revision);
                        call.mutation.base_revision = Some(U64(revision - 1));
                    }
                    if running
                        && let Some(call_id) = item_id
                        && let Some(dir) = &session_dir
                    {
                        self.start(&call_id, dir, tool_name.as_deref());
                        emitted.push(observation);
                        self.drain(&call_id, &mut emitted);
                    } else {
                        emitted.push(observation);
                    }
                }
                ObservationPayload::ToolResult(result) if result.stage == ResultStage::Final => {
                    if let Some(call_id) = observation.item_id.clone() {
                        // Bytes the TUI flushed before the completed frame
                        // belong immediately before the Final, never after.
                        self.drain(&call_id, &mut emitted);
                        let previous = self
                            .calls
                            .get(&call_id)
                            .filter(|tail| tail.opened)
                            .map(|tail| tail.revision);
                        self.remove(&call_id);
                        if let Some(previous) = previous
                            && let ObservationPayload::ToolResult(result) = &mut observation.payload
                        {
                            result.mutation = NodeMutation {
                                node_id: result.tool_call_id.clone(),
                                revision: U64(previous + 1),
                                operation: MutationOperation::Close,
                                base_revision: Some(U64(previous)),
                            };
                        }
                    }
                    emitted.push(observation);
                }
                _ => emitted.push(observation),
            }
        }
        // Calls still running with no frame this poll get their idle poll.
        for call_id in self.order.clone() {
            self.drain(&call_id, &mut emitted);
        }
        *out = emitted;
    }

    /// Track a ToolCall mutation and advance the per-node revision counter.
    /// Post-Open mutations get the next sequential revision regardless of the
    /// frame translator's numbering, so a second progress update cannot collide
    /// with a Partial already emitted. Returns the node's resulting revision
    /// for calls this fold tracks.
    fn note_call(
        &mut self,
        call_id: Option<String>,
        node: Id,
        turn_id: Option<String>,
        frame_revision: u64,
        is_open: bool,
    ) -> Option<u64> {
        let call_id = call_id?;
        if !self.calls.contains_key(&call_id) {
            self.order.push(call_id.clone());
            self.calls.insert(
                call_id.clone(),
                CallTail {
                    node: node.clone(),
                    turn_id: turn_id.clone(),
                    revision: 0,
                    poll_bytes: 0,
                    opened: false,
                    log: None,
                },
            );
        }
        let call = self.calls.get_mut(&call_id).expect("inserted above");
        call.node = node;
        if turn_id.is_some() {
            call.turn_id = turn_id;
        }
        if is_open {
            call.revision = frame_revision;
            call.opened = true;
        } else if call.opened {
            call.revision += 1;
        } else {
            // Defensive: a Running Replace whose Proposed Open this fold never
            // saw (the plain adapter suppresses this). Trust the frame's
            // revision so the Final still closes above it.
            call.revision = frame_revision;
            call.opened = true;
        }
        Some(call.revision)
    }

    /// Open the conventional log tail for a Running shell call. Idempotent and
    /// restrictive: only `run_terminal_command` with a single-component native
    /// id is tailable; anything else is silently not followed.
    fn start(&mut self, call_id: &str, session_dir: &Path, tool_name: Option<&str>) {
        if tool_name != Some(RUN_TERMINAL_COMMAND) || !safe_log_name(call_id) {
            return;
        }
        if let Some(call) = self.calls.get_mut(call_id)
            && call.log.is_none()
        {
            call.log = Some(LogTail::new(
                session_dir
                    .join(TERMINAL_DIR)
                    .join(format!("{call_id}.log")),
            ));
        }
    }

    /// Publish appended bytes for one call as a single Partial result. Silent
    /// when the file is absent, unchanged, unreadable, or budget-capped. A
    /// rotated log is published as a Replace snapshot; steady-state growth is
    /// an Append delta.
    fn drain(&mut self, call_id: &str, out: &mut Vec<AdapterObservation>) {
        let Some(call) = self.calls.get_mut(call_id) else {
            return;
        };
        let Some(log) = call.log.as_mut() else {
            return;
        };
        if log.capped {
            return;
        }
        let poll_remaining = MAX_BYTES_PER_POLL.saturating_sub(call.poll_bytes);
        let Some(mut bytes) = log.read_appended(poll_remaining) else {
            return;
        };
        // Per-call budget: discard bytes past the cap rather than publishing
        // them; the Final result carries the authoritative text. A truncated
        // rotation snapshot stays a Replace (it still replaces, not appends).
        let remaining = MAX_BYTES_PER_CALL.saturating_sub(log.consumed) as usize;
        if remaining == 0 {
            log.capped = true;
            return;
        }
        if bytes.len() > remaining {
            bytes.truncate(remaining);
            log.capped = true;
        }
        let text = log.decode(&bytes);
        log.consumed += bytes.len() as u64;
        call.poll_bytes += bytes.len() as u64;
        if text.is_empty() {
            // An incomplete UTF-8 prefix after rotation leaves the latched
            // flag set: the completed first text of the new file must still
            // Replace.
            return;
        }
        call.revision += 1;
        let revision = call.revision;
        // Consume the latched rotation signal only when the Replace is
        // actually published, so an empty poll between truncation and the new
        // head cannot turn it into an Append.
        let operation = if log.rotated {
            log.rotated = false;
            MutationOperation::Replace
        } else {
            MutationOperation::Append
        };
        let payload = ObservationPayload::ToolResult(Box::new(ToolResultPayload {
            mutation: NodeMutation {
                node_id: call.node.clone(),
                revision: U64(revision),
                operation,
                base_revision: Some(U64(revision - 1)),
            },
            tool_call_id: call.node.clone(),
            stage: ResultStage::Partial,
            // The command is still running; outcome/exit code arrive with the
            // terminal frame.
            outcome: ToolOutcome::Unknown,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock { text }))],
            structured_result: not_emitted(),
            exit_code: Knowledge::Unknown {
                reason: "not-emitted".into(),
                evidence_event_ids: Vec::new(),
            },
            changes: Vec::new(),
        }));
        let mut observed = AdapterObservation::partial(payload);
        observed.item_id = Some(call_id.to_owned());
        observed.turn_id = call.turn_id.clone();
        out.push(observed);
    }

    /// Drop a call's tail at its terminal frame; a later log flush must emit
    /// nothing.
    fn remove(&mut self, call_id: &str) {
        self.calls.remove(call_id);
        self.order.retain(|id| id != call_id);
    }
}

/// Read the D-043 stable tool name (`_meta["x.ai/tool"].name`), never the
/// human title, from a call payload's `Known` knowledge.
fn known_tool_name(call: &ToolCallPayload) -> Option<String> {
    match &call.tool_name {
        Knowledge::Known { value } => Some(value.clone()),
        _ => None,
    }
}

/// True only when a native tool call id is safe to interpolate as one
/// filename component under the session's terminal directory: exactly one
/// [`Component::Normal`] whose spelling equals the id. A `..` traversal, a
/// separator, an absolute prefix or a NUL byte all refuse the tail.
fn safe_log_name(id: &str) -> bool {
    if id.is_empty() || id.as_bytes().contains(&0) {
        return false;
    }
    let mut components = Path::new(id).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(part)), None) if part == OsStr::new(id)
    )
}

impl LogTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            pending: Vec::new(),
            consumed: 0,
            capped: false,
            rotated: false,
        }
    }

    /// Read at most `limit` newly appended bytes, latching [`Self::rotated`]
    /// when the file shrank below the read offset (rotation): the offset and
    /// the held partial UTF-8 reset and subsequent reads restart at byte zero.
    /// The latch is owned by the caller, which clears it once it has published
    /// the Replace, so an empty read on the truncated file cannot lose the
    /// signal. `None` covers every absent / unreadable / unchanged case — the
    /// log channel is best-effort.
    fn read_appended(&mut self, limit: u64) -> Option<Vec<u8>> {
        let mut file = File::open(&self.path).ok()?;
        let len = file.metadata().ok()?.len();
        if len < self.offset {
            self.rotated = true;
            self.offset = 0;
            self.pending.clear();
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut buf = Vec::new();
        let read = file.take(limit).read_to_end(&mut buf).ok()?;
        self.offset += read as u64;
        if read == 0 {
            return None;
        }
        Some(buf)
    }

    /// Decode one bounded read as UTF-8, holding an incomplete trailing
    /// sequence for the next poll; genuinely invalid bytes degrade to the
    /// replacement char instead of stalling the tail.
    fn decode(&mut self, bytes: &[u8]) -> String {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        match std::str::from_utf8(&buffer) {
            Ok(_) => String::from_utf8(buffer).expect("validated above"),
            Err(error) => {
                let valid = error.valid_up_to();
                match error.error_len() {
                    Some(_) => String::from_utf8_lossy(&buffer).into_owned(),
                    None => {
                        let text = String::from_utf8_lossy(&buffer[..valid]).into_owned();
                        self.pending = buffer[valid..].to_vec();
                        text
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
