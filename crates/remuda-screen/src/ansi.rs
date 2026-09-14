//! Text-side adapters: turn raw PTY bytes into something the matchers can read
//! without a terminal emulator.
//!
//! These exist because the emulator is off by default this phase (D-028 §13
//! P0). Every helper here reproduces, byte for byte, what the matchers used to
//! do inline in `remuda-driver`, so a screen classified one way before the
//! extraction is classified the same way after it.

/// Last ~8 KiB of the screen, so a long scrollback cannot mask current state.
///
/// The bound is in bytes and is snapped up to a char boundary, matching the
/// pre-extraction behaviour exactly.
#[must_use]
pub fn screen_tail(screen: &str) -> String {
    const TAIL: usize = 8192;
    if screen.len() <= TAIL {
        return screen.to_owned();
    }
    let start = screen
        .char_indices()
        .rev()
        .map(|(index, _)| index)
        .find(|index| *index <= screen.len() - TAIL)
        .unwrap_or(0);
    screen[start..].to_owned()
}

/// Last `limit` characters of the screen.
///
/// Character-counted rather than byte-counted; the agent-banner fallback has
/// always used this bound and a byte bound would classify differently on a
/// screen of wide characters.
#[must_use]
pub fn char_tail(screen: &str, limit: usize) -> String {
    screen
        .chars()
        .rev()
        .take(limit)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Remove CSI / OSC / charset escapes so text matching sees rendered content.
///
/// This is a reader for heuristics, not a terminal emulator: cursor motion is
/// dropped rather than replayed, which is enough to recognize the composer, a
/// running turn, or a dialog. When [`crate::Emulator`] is running, its grid is
/// the better input and this function is not on the path.
#[must_use]
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if ch != '\u{7}' {
                out.push(ch);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters/intermediates, then one final byte.
            Some('[') => {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST.
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            // Charset selection and other two-byte sequences.
            Some('(' | ')' | '#' | '=' | '>') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_keeps_text_and_drops_control_sequences() {
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m text"), "red text");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(strip_ansi("\u{1b}(Bplain"), "plain");
    }

    #[test]
    fn the_tail_helpers_are_bounded_and_utf8_safe() {
        let wide = "界".repeat(6000);
        assert!(screen_tail(&wide).len() <= 8192 + 3);
        assert!(screen_tail(&wide).chars().all(|ch| ch == '界'));
        assert_eq!(char_tail(&wide, 10).chars().count(), 10);
        assert_eq!(char_tail("short", 4096), "short");
    }
}
