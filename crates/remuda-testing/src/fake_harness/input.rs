//! PTY input chunk semantics.
//!
//! The harness reads raw bytes (raw mode when a TTY is attached). The *write
//! boundary* matters and is exactly what the evidence sessions established:
//!
//! - a standalone `\r` read is the Enter key → submit;
//! - body text and `\r` arriving in the **same read** are a paste-burst; the
//!   CR is swallowed, so `write_all(b"hi\r")` can never submit
//!   ([claude-queue-steer-1](../../../../docs/design/evidence/claude-queue-steer-1.md));
//! - a bracketed paste (`ESC[200~ … ESC[201~]`) inserts text verbatim, and its
//!   embedded CRs become line breaks rather than submits;
//! - a lone `\x1b` read is Esc; `\x1b[A` / `\x1b[B` are arrow keys.
//!
//! [`Parser::push`] takes one read's worth of bytes and returns zero or more
//! decoded inputs. State carries across reads for split escape sequences.

/// Decoded keypress / input event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Input {
    /// Enter key as its own read (`\r` or `\n` alone).
    Submit,
    /// Lone Esc.
    Escape,
    /// Tab (`\x09`).
    Tab,
    /// Ctrl+C (`\x03`).
    CtrlC,
    /// Ctrl+Q (`\x11`) — grok's quit chord.
    CtrlQ,
    /// Ctrl+X (`\x18`) — claude's `chat:queueSubmit` prefix.
    CtrlX,
    /// Backspace (`0x7f` / `\x08`).
    Backspace,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// A digit row key (`1`..`9`).
    Digit(u8),
    /// A burst of printable bytes; a CR riding in the same read was swallowed.
    Text(String),
    /// Bracketed-paste content; embedded CRs are kept as `\n`.
    Paste(String),
}

/// Stateful chunk parser.
#[derive(Debug, Default)]
pub struct Parser {
    /// Inside a bracketed paste waiting for `ESC[201~`.
    in_paste: bool,
    /// Paste bytes accumulated across reads.
    paste_buf: Vec<u8>,
}

impl Parser {
    /// New parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one read's bytes; returns decoded inputs in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        if self.in_paste {
            let (paste, rest, still_in) = split_paste_end(chunk);
            self.paste_buf.extend_from_slice(paste);
            if still_in {
                return out;
            }
            self.in_paste = false;
            let completed = std::mem::take(&mut self.paste_buf);
            out.push(Input::Paste(clean_paste(&completed)));
            if !rest.is_empty() {
                out.extend(self.parse_plain(rest));
            }
            return out;
        }
        out.extend(self.parse_plain(chunk));
        out
    }

    fn parse_plain(&mut self, chunk: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        // Paste-burst rule: if this read carries any body text, every CR in the
        // same read is inserted/swallowed rather than treated as Enter.
        let burst = chunk.iter().any(|b| b.is_ascii_graphic() || *b == b' ');
        let mut text = String::new();
        let mut i = 0;
        while i < chunk.len() {
            let byte = chunk[i];
            match byte {
                b'\r' | b'\n' => {
                    flush_text(&mut text, &mut out);
                    if !burst {
                        out.push(Input::Submit);
                    }
                }
                0x1b => {
                    flush_text(&mut text, &mut out);
                    // Paste start: buffer content (possibly across reads)
                    // until ESC[201~, emit one Paste, then keep parsing.
                    if chunk[i..].starts_with(b"\x1b[200~") {
                        self.in_paste = true;
                        let (paste, rest, still_in) = split_paste_end(&chunk[i + 6..]);
                        self.paste_buf.extend_from_slice(paste);
                        if still_in {
                            return out;
                        }
                        self.in_paste = false;
                        let completed = std::mem::take(&mut self.paste_buf);
                        out.push(Input::Paste(clean_paste(&completed)));
                        if !rest.is_empty() {
                            out.extend(self.parse_plain(rest));
                        }
                        return out;
                    }
                    match self.decode_escape(&chunk[i..]) {
                        Some((Some(input), consumed)) => {
                            out.push(input);
                            i += consumed;
                            continue;
                        }
                        Some((None, consumed)) => {
                            i += consumed;
                            continue;
                        }
                        None => {
                            // Truncated CSI at end of read: drop the partial
                            // sequence rather than misreading its ESC as Esc.
                            return out;
                        }
                    }
                }
                b'\t' => {
                    flush_text(&mut text, &mut out);
                    out.push(Input::Tab);
                }
                0x03 => {
                    flush_text(&mut text, &mut out);
                    out.push(Input::CtrlC);
                }
                0x11 => {
                    flush_text(&mut text, &mut out);
                    out.push(Input::CtrlQ);
                }
                0x18 => {
                    flush_text(&mut text, &mut out);
                    out.push(Input::CtrlX);
                }
                0x7f | 0x08 => {
                    flush_text(&mut text, &mut out);
                    out.push(Input::Backspace);
                }
                b'1'..=b'9' => {
                    flush_text(&mut text, &mut out);
                    out.push(Input::Digit(byte - b'0'));
                }
                b if b.is_ascii_graphic() || b == b' ' => text.push(b as char),
                _ => {}
            }
            i += 1;
        }
        flush_text(&mut text, &mut out);
        out
    }

    /// Decode an escape sequence beginning with ESC.
    fn decode_escape(&mut self, chunk: &[u8]) -> Option<(Option<Input>, usize)> {
        if chunk.len() >= 2 && chunk[1] == b'[' {
            let mut j = 2;
            while j < chunk.len() {
                let b = chunk[j];
                if b.is_ascii_alphabetic() || b == b'~' {
                    let seq = &chunk[..=j];
                    let input = match seq {
                        b"\x1b[A" => Some(Input::Up),
                        b"\x1b[B" => Some(Input::Down),
                        _ => None,
                    };
                    return Some((input, j + 1));
                }
                j += 1;
            }
            return None;
        }
        if chunk.len() == 1 {
            return Some((Some(Input::Escape), 1));
        }
        // ESC + other byte is an Alt chord the fake does not bind.
        Some((None, 2))
    }
}

fn flush_text(text: &mut String, out: &mut Vec<Input>) {
    if !text.is_empty() {
        out.push(Input::Text(std::mem::take(text)));
    }
}

/// Split a chunk read while inside a paste at `ESC[201~`.
fn split_paste_end(chunk: &[u8]) -> (&[u8], &[u8], bool) {
    if let Some(index) = find_subslice(chunk, b"\x1b[201~") {
        let paste = &chunk[..index];
        let rest = &chunk[index + 6..];
        (paste, rest, false)
    } else {
        (chunk, &[], true)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn clean_paste(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_cr_submits() {
        let mut parser = Parser::new();
        assert_eq!(parser.push(b"abc"), vec![Input::Text("abc".into())]);
        assert_eq!(parser.push(b"\r"), vec![Input::Submit]);
    }

    #[test]
    fn body_and_cr_in_one_read_does_not_submit() {
        let mut parser = Parser::new();
        assert_eq!(parser.push(b"hello\r"), vec![Input::Text("hello".into())]);
        // A separate Enter still submits the drafted text.
        assert_eq!(parser.push(b"\r"), vec![Input::Submit]);
    }

    #[test]
    fn bracketed_paste_with_cr_inserts_but_does_not_submit() {
        let mut parser = Parser::new();
        let events = parser.push(b"\x1b[200~line one\rline two\x1b[201~");
        assert_eq!(events, vec![Input::Paste("line one\nline two".into())]);
    }

    #[test]
    fn split_paste_across_reads() {
        let mut parser = Parser::new();
        assert!(parser.push(b"\x1b[200~abc").is_empty());
        assert_eq!(
            parser.push(b"def\x1b[201~"),
            vec![Input::Paste("abcdef".into())]
        );
        assert_eq!(parser.push(b"\r"), vec![Input::Submit]);
    }

    #[test]
    fn lone_esc_and_arrows() {
        let mut parser = Parser::new();
        assert_eq!(parser.push(b"\x1b"), vec![Input::Escape]);
        assert_eq!(parser.push(b"\x1b[A"), vec![Input::Up]);
        assert_eq!(parser.push(b"\x1b[B"), vec![Input::Down]);
    }

    #[test]
    fn control_keys() {
        let mut parser = Parser::new();
        assert_eq!(parser.push(b"\x09"), vec![Input::Tab]);
        assert_eq!(parser.push(b"\x03"), vec![Input::CtrlC]);
        assert_eq!(parser.push(b"\x11"), vec![Input::CtrlQ]);
        assert_eq!(parser.push(b"\x7f"), vec![Input::Backspace]);
        assert_eq!(parser.push(b"2"), vec![Input::Digit(2)]);
    }
}
