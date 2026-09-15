//! Deterministic alternate-screen tracker over the raw pane byte stream.
//!
//! The Node/web badge needs to know whether the pane is showing a full-screen
//! TUI (`?1049`) without a terminal emulator — herdr's carrier relays raw
//! bytes, and its protocol does not (yet) report screen mode. This is the
//! byte-stream baseline D-028 asks for: parse just enough ECMA-48 to notice
//! DECSET/DECRST for the alternate-screen modes and RIS, and nothing else.
//!
//! Recognized transitions:
//! - `ESC [ ? 1049 h` / `l` — save-cursor alt buffer (what modern TUIs use)
//! - `ESC [ ? 1047 h` / `l` — alt buffer switch with clear on leave
//! - `ESC [ ? 47 h`  / `l` — the legacy alt-buffer switch
//! - `ESC c` (RIS) — a full reset always returns to the primary screen
//!
//! Mixed-mode sequences (`?1049;25h`) are handled parameter by parameter.
//! This models a single "alt screen active" bit: a SET of any of the three
//! modes enters, a RESET leaves. xterm effectively maps 1049 onto mode 47 and
//! real applications use exactly one of the three, so the bit model agrees
//! with every observed producer; a program that toggles 1049 and 47 against
//! each other does not exist in practice.
//!
//! The scanner is split-chunk safe: bytes of an unfinished sequence are
//! retained from its ESC byte and re-scanned with the next chunk. It
//! deliberately does not decode UTF-8 or any terminal semantics beyond the
//! modes above.

/// Mode numbers that switch the visible buffer (DEC private modes).
const ALT_MODES: &[u16] = &[47, 1047, 1049];

/// A half-recognized sequence longer than this is treated as garbage and the
/// scanner resynchronizes at the next ESC. Real CSI/OSC sequences are far
/// shorter; this only bounds memory on a corrupt stream.
const MAX_PENDING: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Ordinary bytes.
    Ground,
    /// Just consumed ESC; deciding what follows.
    Escape,
    /// ESC plus one 0x20..=0x2F intermediate (e.g. `ESC (`); one final byte ends it.
    EscapeIntermediate,
    /// Inside `ESC [ ...`, collecting the private marker, params, intermediates.
    Csi,
    /// Inside an OSC string (`ESC ]`), ends at BEL or ST.
    Osc,
    /// ESC seen inside an OSC/DCS/APC string — a backslash is the ST terminator.
    StringEscape,
    /// Inside DCS/APC/PM/SOS (`ESC P|_|^|X`), ends at ST.
    StringUntilSt,
}

/// Streaming alternate-screen observer. One per pane relay.
#[derive(Debug, Clone)]
pub struct AltScreenScanner {
    alt: bool,
    state: State,
    /// CSI parameter bytes collected so far (`?1049;25`).
    csi_params: Vec<u8>,
    /// Suffix of the last chunk belonging to an unfinished sequence.
    pending: Vec<u8>,
    /// Index in `pending` where the current ESC sequence starts.
    seq_start: usize,
}

impl Default for AltScreenScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl AltScreenScanner {
    /// A pane starts on its primary screen.
    #[must_use]
    pub fn new() -> Self {
        Self {
            alt: false,
            state: State::Ground,
            csi_params: Vec::new(),
            pending: Vec::new(),
            seq_start: 0,
        }
    }

    /// Whether the alternate screen is active after everything fed so far.
    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// Feed one relay chunk. Returns `Some(new_mode)` only when the mode
    /// changed while processing this chunk, so a pump can emit one mode event
    /// at the flip instead of diffing on every frame.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<bool> {
        let before = self.alt;
        if bytes.is_empty() {
            return None;
        }
        // `pending` holds the unfinished sequence from the previous chunk;
        // this chunk extends it. Only the new bytes are walked (the state
        // machine already consumed the carried prefix); at the end anything
        // short of Ground is kept as the new pending suffix.
        self.seq_start = 0;
        let carried = self.pending.len();
        self.pending.extend_from_slice(bytes);
        let mut i = carried;
        while i < self.pending.len() {
            let byte = self.pending[i];
            match self.state {
                State::Ground => {
                    if byte == 0x1b {
                        self.seq_start = i;
                        self.state = State::Escape;
                    }
                    i += 1;
                }
                State::Escape => match byte {
                    b'[' => {
                        self.state = State::Csi;
                        self.csi_params.clear();
                        i += 1;
                    }
                    b']' => {
                        self.state = State::Osc;
                        i += 1;
                    }
                    b'P' | b'_' | b'^' | b'X' => {
                        self.state = State::StringUntilSt;
                        i += 1;
                    }
                    // RIS: hard reset, primary screen.
                    b'c' => {
                        self.alt = false;
                        self.state = State::Ground;
                        i += 1;
                    }
                    0x20..=0x2f => {
                        self.state = State::EscapeIntermediate;
                        i += 1;
                    }
                    0x1b => {
                        // A new ESC restarts the two-byte look.
                        self.seq_start = i;
                        i += 1;
                    }
                    // Any other byte ends the (non-)sequence.
                    _ => {
                        self.state = State::Ground;
                        i += 1;
                    }
                },
                State::EscapeIntermediate => {
                    self.state = State::Ground;
                    i += 1;
                }
                State::Csi => match byte {
                    // Parameter bytes 0x30..=0x3F ('?' is 0x3F).
                    0x30..=0x3f => {
                        if self.csi_params.len() < MAX_PENDING {
                            self.csi_params.push(byte);
                        }
                        i += 1;
                    }
                    // Intermediate bytes are legal but irrelevant here.
                    0x20..=0x2f => i += 1,
                    // Control bytes may be interspersed; ignore them.
                    0x00..=0x1f | 0x7f => i += 1,
                    // Final byte 0x40..=0x7E.
                    _ => {
                        self.apply_csi(byte);
                        self.state = State::Ground;
                        i += 1;
                    }
                },
                State::Osc => match byte {
                    0x07 => {
                        self.state = State::Ground;
                        i += 1;
                    }
                    0x1b => {
                        self.state = State::StringEscape;
                        i += 1;
                    }
                    _ => i += 1,
                },
                State::StringUntilSt => match byte {
                    0x1b => {
                        self.state = State::StringEscape;
                        i += 1;
                    }
                    _ => i += 1,
                },
                State::StringEscape => {
                    if byte == b'\\' {
                        self.state = State::Ground;
                        i += 1;
                    } else if byte == 0x1b {
                        // Another ESC restarts the two-byte look.
                        self.seq_start = i;
                        i += 1;
                    } else {
                        // Not an ST: that ESC began a new sequence; reprocess
                        // the current byte as that sequence's second byte.
                        self.seq_start = i.saturating_sub(1);
                        self.state = State::Escape;
                    }
                }
            }
            if self.state != State::Ground && i - self.seq_start > MAX_PENDING {
                // Corrupt / unterminated sequence: resynchronize at ground.
                // Do not advance, so an embedded ESC is still observed.
                self.state = State::Ground;
                self.csi_params.clear();
            }
        }
        if self.state == State::Ground {
            self.pending.clear();
        } else {
            // Retain the unfinished suffix from its opening ESC onward.
            self.pending.drain(..self.seq_start);
        }
        (self.alt != before).then_some(self.alt)
    }

    /// Interpret a completed CSI if it is a private DECSET/DECRST for an
    /// alternate-screen mode.
    fn apply_csi(&mut self, final_byte: u8) {
        let set = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        // Private sequences start the parameter field with '?'.
        let Some(rest) = self.csi_params.strip_prefix(b"?") else {
            return;
        };
        for raw in rest.split(|byte| *byte == b';' || *byte == b':') {
            let text = String::from_utf8_lossy(raw);
            if let Ok(mode) = text.trim().parse::<u16>()
                && ALT_MODES.contains(&mode)
            {
                self.alt = set;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_1049_enter_and_leave() {
        let mut scanner = AltScreenScanner::new();
        assert!(!scanner.alt_screen());
        assert_eq!(scanner.feed(b"\x1b[?1049h"), Some(true));
        assert!(scanner.alt_screen());
        assert_eq!(scanner.feed(b"repaint bytes"), None);
        assert_eq!(scanner.feed(b"\x1b[?1049l"), Some(false));
        assert!(!scanner.alt_screen());
    }

    #[test]
    fn tracks_legacy_47_and_1047() {
        let mut a = AltScreenScanner::new();
        assert_eq!(a.feed(b"\x1b[?47h"), Some(true));
        assert_eq!(a.feed(b"\x1b[?47l"), Some(false));

        let mut b = AltScreenScanner::new();
        assert_eq!(b.feed(b"\x1b[?1047h"), Some(true));
        assert_eq!(b.feed(b"\x1b[?1047l"), Some(false));
    }

    #[test]
    fn non_private_47_is_not_alt_screen() {
        // Without the DEC '?' prefix, 47 is an unrelated SGR parameter.
        let mut scanner = AltScreenScanner::new();
        assert_eq!(scanner.feed(b"\x1b[47h"), None);
        assert!(!scanner.alt_screen());
    }

    #[test]
    fn batched_private_modes_are_split_parameter_by_parameter() {
        let mut scanner = AltScreenScanner::new();
        // Cursor-hide rides the same DECSET; alt must still flip.
        assert_eq!(scanner.feed(b"\x1b[?1049;25h"), Some(true));
        assert_eq!(
            scanner.feed(b"\x1b[?25l"),
            None,
            "25 alone is cursor visibility"
        );
        assert!(scanner.alt_screen());
        assert_eq!(scanner.feed(b"\x1b[?1049;25l"), Some(false));
    }

    #[test]
    fn ris_returns_to_the_primary_screen() {
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"\x1b[?1049h");
        assert_eq!(scanner.feed(b"\x1bc"), Some(false));
        assert!(!scanner.alt_screen());
    }

    #[test]
    fn a_sequence_split_between_chunks_is_resumed() {
        let full: &[u8] = b"\x1b[?1049h";
        for cut in 0..full.len() {
            let mut scanner = AltScreenScanner::new();
            let (head, tail) = full.split_at(cut);
            scanner.feed(head);
            assert!(
                !scanner.alt_screen(),
                "cut at {cut}: head alone must not enter"
            );
            assert_eq!(scanner.feed(tail), Some(true), "cut at {cut}");
            assert!(scanner.alt_screen());
            // Subsequent ordinary bytes must not re-trigger.
            assert_eq!(scanner.feed(b"painted"), None);
        }
    }

    #[test]
    fn a_split_leave_sequence_flips_back() {
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"\x1b[?1049h");
        scanner.feed(b"full screen paint");
        assert_eq!(scanner.feed(b"\x1b[?104"), None);
        assert_eq!(scanner.feed(b"9l"), Some(false));
        assert!(!scanner.alt_screen());
    }

    #[test]
    fn split_sequence_amid_following_output() {
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"prefix\x1b[?10");
        assert_eq!(scanner.feed(b"49h"), Some(true));
        assert!(scanner.alt_screen());
        // The byte right after the completed sequence in a later chunk is
        // processed as ground data, not replayed as the sequence tail.
        assert_eq!(scanner.feed(b"frame"), None);
        assert!(scanner.alt_screen());
    }

    #[test]
    fn repeated_enter_is_not_a_flip() {
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"\x1b[?1049h");
        assert_eq!(scanner.feed(b"\x1b[?1049h"), None);
        assert!(scanner.alt_screen());
    }

    #[test]
    fn ordinary_output_between_modes_keeps_state() {
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"$ ");
        scanner.feed(b"\x1b[?1049h\x1b[2J\x1b[H");
        assert!(scanner.alt_screen());
        // Screen erase / cursor-home must not touch the tracked bit.
        scanner.feed(b"TUI frame");
        assert!(scanner.alt_screen());
    }

    #[test]
    fn osc_and_other_strings_cannot_fake_the_mode() {
        let mut scanner = AltScreenScanner::new();
        // Text that looks like a mode switch inside an OSC title is ignored.
        scanner.feed(b"\x1b]0;?1049h\x07");
        assert!(!scanner.alt_screen());
        // ST-terminated OSC as well.
        scanner.feed(b"\x1b]2;?1047h\x1b\\");
        assert!(!scanner.alt_screen());
        // The scanner still works after the string closes.
        assert_eq!(scanner.feed(b"\x1b[?1049h"), Some(true));
    }

    #[test]
    fn split_osc_then_real_enter_is_still_seen() {
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"\x1b]0;title");
        scanner.feed(b"continued\x07");
        assert_eq!(scanner.feed(b"\x1b[?1049h"), Some(true));
    }

    #[test]
    fn unterminated_garbage_resynchronizes() {
        let mut scanner = AltScreenScanner::new();
        let mut junk = vec![b'\x1b', b'['];
        junk.extend(std::iter::repeat_n(b'1', MAX_PENDING + 10));
        scanner.feed(&junk);
        assert!(!scanner.alt_screen());
        // A well-formed sequence after the garbage is recognized.
        assert_eq!(scanner.feed(b"\x1b[?47h"), Some(true));
    }

    #[test]
    fn byte_by_byte_feed_matches_whole_frame_feed() {
        let stream: &[u8] = b"boot\r\n\x1b[?1049h\x1b[2Jpaint\x1b[?1049l$ ";
        let mut whole = AltScreenScanner::new();
        whole.feed(stream);
        assert!(!whole.alt_screen());

        let mut split = AltScreenScanner::new();
        let mut flips = Vec::new();
        for byte in stream {
            if let Some(mode) = split.feed(std::slice::from_ref(byte)) {
                flips.push(mode);
            }
        }
        assert_eq!(split.alt_screen(), whole.alt_screen());
        assert_eq!(flips, vec![true, false]);
    }

    #[test]
    fn real_world_enter_chunk_then_random_split_leave() {
        // Mirrors a relay: enter arrives intact inside one big paint frame,
        // the leave arrives byte-split after more output.
        let mut scanner = AltScreenScanner::new();
        scanner.feed(b"\x1b[?2004h\x1b[?1049h\x1b[2J\x1b[Hfullscreen TUI");
        assert!(scanner.alt_screen());
        for (i, byte) in b"\x1b[?1049l".iter().enumerate() {
            let flipped = scanner.feed(std::slice::from_ref(byte));
            if i < 7 {
                assert_eq!(flipped, None, "byte {i}");
                assert!(scanner.alt_screen());
            } else {
                assert_eq!(flipped, Some(false));
            }
        }
        assert!(!scanner.alt_screen());
        // Ready for the next turn.
        assert_eq!(scanner.feed(b"$ "), None);
        assert_eq!(scanner.feed(b"\x1b[?1049h"), Some(true));
    }
}
