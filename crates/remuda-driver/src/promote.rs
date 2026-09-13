//! Terminal → agent promotion: recognize an agent CLI running inside a
//! `shell-pty` login shell (D-025).
//!
//! Detection is the PTY's own foreground process group (`tcgetpgrp` on the
//! master fd), then the process name / argv of the pids in that group. That is
//! the kernel's answer to "what is the user typing at", so it follows job
//! control without heuristics. A screen signature over the ring buffer is the
//! fallback when the fd query is unavailable, never the primary path.

use remuda_protocol::AgentKind;
use std::path::Path;

/// One row of the known-agent table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSignature {
    /// Agent kind this row promotes to.
    pub kind: AgentKind,
    /// Executable basenames that identify the CLI.
    pub names: &'static [&'static str],
    /// Whether this kind hydrates structured messages from a native transcript.
    pub hydrates_transcript: bool,
}

/// Known agent CLIs, in match order. Only `claude` hydrates a transcript in the
/// MVP; the others switch `kind` so the UI stops calling them a plain terminal.
pub const AGENT_TABLE: &[AgentSignature] = &[
    AgentSignature {
        kind: AgentKind::Claude,
        names: &["claude", "claude-code"],
        hydrates_transcript: true,
    },
    AgentSignature {
        kind: AgentKind::Codex,
        names: &["codex"],
        hydrates_transcript: false,
    },
    AgentSignature {
        kind: AgentKind::Grok,
        names: &["grok"],
        hydrates_transcript: false,
    },
    AgentSignature {
        kind: AgentKind::Agy,
        names: &["agy"],
        hydrates_transcript: false,
    },
];

/// Shells and wrappers that are never an agent, whatever else is on the line.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "login", "env",
];

/// One row of the PTY foreground process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    /// Process id.
    pub pid: i32,
    /// Full argv as one line, as `ps -o args=` renders it.
    pub args: String,
}

/// What the poller concluded about the current foreground process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Promoted agent kind.
    pub kind: AgentKind,
    /// Pid of the matched process.
    pub pid: i32,
    /// Native session id from `--session-id` / `--resume`, when argv carried one.
    pub session_id: Option<String>,
    /// This kind hydrates structured messages from a transcript.
    pub hydrates_transcript: bool,
}

/// Reads the foreground process group of a live PTY.
///
/// Implemented over the real process table in production and over a fixture in
/// tests, so detection logic is exercised without spawning anything.
pub trait ProcessTable: Send + Sync {
    /// Rows of process group `pgid`. An empty vec means "nothing there".
    fn process_group(&self, pgid: i32) -> Vec<ProcessRow>;
}

/// `ps`-backed process table.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessTable;

impl ProcessTable for SystemProcessTable {
    fn process_group(&self, pgid: i32) -> Vec<ProcessRow> {
        if pgid <= 0 {
            return Vec::new();
        }
        let output = std::process::Command::new("ps")
            .args(["-o", "pid=,args=", "-g", &pgid.to_string()])
            .output();
        match output {
            Ok(out) if out.status.success() => parse_ps_rows(&String::from_utf8_lossy(&out.stdout)),
            _ => Vec::new(),
        }
    }
}

/// Parse `ps -o pid=,args=` output into rows.
#[must_use]
pub fn parse_ps_rows(stdout: &str) -> Vec<ProcessRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace)?;
            let pid = pid.parse().ok()?;
            let args = rest.trim_start();
            (!args.is_empty()).then(|| ProcessRow {
                pid,
                args: args.to_owned(),
            })
        })
        .collect()
}

/// Match one process group against [`AGENT_TABLE`].
///
/// The first row that names a known agent wins. Shells are skipped so the
/// login `$SHELL` itself never promotes.
#[must_use]
pub fn detect(rows: &[ProcessRow]) -> Option<Detected> {
    for row in rows {
        let argv = split_argv(&row.args);
        let Some(program) = argv.first() else {
            continue;
        };
        let base = basename(program);
        if SHELLS.contains(&base.as_str()) {
            continue;
        }
        for entry in AGENT_TABLE {
            if entry.names.contains(&base.as_str()) {
                return Some(Detected {
                    kind: entry.kind,
                    pid: row.pid,
                    session_id: session_id_from_argv(&argv),
                    hydrates_transcript: entry.hydrates_transcript,
                });
            }
        }
    }
    None
}

/// Last-resort detection over the terminal screen when the foreground process
/// group is unavailable. Deliberately narrow: only the Claude TUI's own banner.
#[must_use]
pub fn detect_from_screen(screen: &str) -> Option<AgentKind> {
    let tail: String = screen
        .chars()
        .rev()
        .take(4096)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let lower = strip_ansi(&tail).to_ascii_lowercase();
    (lower.contains("welcome to claude code") || lower.contains("claude code v"))
        .then_some(AgentKind::Claude)
}

/// Screen-derived status of a promoted agent TUI.
///
/// Same class of evidence `claude-pty` takes from herdr's `agent_status`, read
/// here off the PTY ring instead. It is a heuristic over rendered text, so it
/// is reported as screen-derived and never treated as proof of task success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenStatus {
    /// An input box is on screen and accepting a prompt.
    Idle,
    /// A turn is running; the TUI offers `esc to interrupt`.
    Working,
    /// A native dialog owns the keyboard; a prompt would answer it by accident.
    Blocked,
}

impl ScreenStatus {
    /// Wire label matching the `agent_status` values Node already folds into
    /// [`remuda_protocol::Activity`].
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// Classify the tail of a promoted Claude TUI screen.
///
/// Blocked is checked first: mistaking a dialog for an idle prompt is the one
/// error that silently answers a question the human never saw (D-022).
///
/// The input is raw PTY bytes — unlike the herdr-carried drivers, nothing has
/// stripped ANSI for us — so escapes are removed before any matching.
#[must_use]
pub fn screen_status(screen: &str) -> Option<ScreenStatus> {
    let tail = strip_ansi(&screen_tail(screen));
    let flat = tail.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.contains("Do you want to")
        || flat.contains("Is this a project you created or one you trust?")
        || flat.contains("Yes, I trust this folder")
    {
        return Some(ScreenStatus::Blocked);
    }
    if flat.contains("esc to interrupt") {
        return Some(ScreenStatus::Working);
    }
    // The composer box is the TUI's "ready for a prompt" signal. A full-screen
    // TUI repaints with cursor motion rather than newlines, so the prompt glyph
    // is not reliably at the start of a line in the raw byte stream — look for
    // the glyph itself, not for a line that begins with it.
    tail.contains('\u{276f}').then_some(ScreenStatus::Idle)
}

/// Remove CSI / OSC / charset escapes so text matching sees rendered content.
///
/// This is a reader for heuristics, not a terminal emulator: cursor motion is
/// dropped rather than replayed, which is enough to recognize the composer, a
/// running turn, or a dialog.
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

/// Last ~8 KiB of the screen, so a long scrollback cannot mask current state.
fn screen_tail(screen: &str) -> String {
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

/// `--session-id <uuid>` / `--resume <uuid>` from an agent's argv.
fn session_id_from_argv(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter().skip(1);
    while let Some(arg) = iter.next() {
        let value = match arg.split_once('=') {
            Some((flag, inline)) if is_session_flag(flag) => Some(inline.to_owned()),
            _ if is_session_flag(arg) => iter.next().cloned(),
            _ => None,
        };
        if let Some(value) = value.filter(|value| looks_like_uuid(value)) {
            return Some(value);
        }
    }
    None
}

fn is_session_flag(flag: &str) -> bool {
    matches!(flag, "--session-id" | "--resume" | "-r")
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value
            .chars()
            .enumerate()
            .all(|(index, ch)| match (index, ch) {
                (8 | 13 | 18 | 23, ch) => ch == '-',
                (_, ch) => ch.is_ascii_hexdigit(),
            })
}

/// Split a `ps` argv line on unescaped whitespace. `ps` does not quote, so this
/// is a best-effort split that is good enough to read flags off.
fn split_argv(line: &str) -> Vec<String> {
    line.split_whitespace().map(ToOwned::to_owned).collect()
}

fn basename(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_owned())
}

/// Foreground process group of a live PTY.
///
/// `portable-pty` exposes `tcgetpgrp` for us, so this needs no `unsafe` and no
/// direct libc dependency. `None` means the query is unavailable (not a
/// terminal yet, or a platform without job control) — the caller falls back to
/// [`detect_from_screen`].
#[must_use]
pub fn foreground_pgid(master: &dyn portable_pty::MasterPty) -> Option<i32> {
    #[cfg(unix)]
    {
        master.process_group_leader()
    }
    #[cfg(not(unix))]
    {
        let _ = master;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-backed process table for detection tests.
    struct FakeTable(Vec<ProcessRow>);

    impl ProcessTable for FakeTable {
        fn process_group(&self, _pgid: i32) -> Vec<ProcessRow> {
            self.0.clone()
        }
    }

    fn rows(lines: &[(i32, &str)]) -> Vec<ProcessRow> {
        lines
            .iter()
            .map(|(pid, args)| ProcessRow {
                pid: *pid,
                args: (*args).to_owned(),
            })
            .collect()
    }

    #[test]
    fn login_shell_alone_does_not_promote() {
        let table = FakeTable(rows(&[(100, "/bin/zsh -l")]));
        assert_eq!(detect(&table.process_group(100)), None);
    }

    #[test]
    fn claude_in_the_foreground_group_promotes_with_its_session_id() {
        let table = FakeTable(rows(&[
            (100, "/bin/zsh -l"),
            (
                101,
                "/Users/x/.local/bin/claude --model opus --session-id 04b95a78-e876-4212-aa9c-a6482f30f583",
            ),
        ]));
        let found = detect(&table.process_group(101)).expect("claude detected");
        assert_eq!(found.kind, AgentKind::Claude);
        assert_eq!(found.pid, 101);
        assert_eq!(
            found.session_id.as_deref(),
            Some("04b95a78-e876-4212-aa9c-a6482f30f583")
        );
        assert!(found.hydrates_transcript);
    }

    #[test]
    fn resume_and_inline_flag_forms_both_yield_the_session() {
        for args in [
            "claude --resume 04b95a78-e876-4212-aa9c-a6482f30f583",
            "claude --session-id=04b95a78-e876-4212-aa9c-a6482f30f583",
        ] {
            let found = detect(&rows(&[(7, args)])).expect("detected");
            assert_eq!(
                found.session_id.as_deref(),
                Some("04b95a78-e876-4212-aa9c-a6482f30f583"),
                "{args}"
            );
        }
    }

    #[test]
    fn a_non_uuid_session_argument_is_not_adopted() {
        let found = detect(&rows(&[(7, "claude --resume latest")])).expect("detected");
        assert_eq!(found.session_id, None);
    }

    #[test]
    fn secondary_clis_switch_kind_without_transcript_hydration() {
        for (args, kind) in [
            ("codex --full-auto", AgentKind::Codex),
            ("grok agent", AgentKind::Grok),
            ("agy -p hi", AgentKind::Agy),
        ] {
            let found = detect(&rows(&[(9, args)])).expect("detected");
            assert_eq!(found.kind, kind, "{args}");
            assert!(!found.hydrates_transcript, "{args}");
        }
    }

    #[test]
    fn an_ordinary_foreground_command_is_not_an_agent() {
        assert_eq!(detect(&rows(&[(5, "vim docs/design/decisions.md")])), None);
        // A path that merely contains the word is not the binary.
        assert_eq!(detect(&rows(&[(5, "cat /tmp/claude-notes.txt")])), None);
    }

    #[test]
    fn ps_output_parses_into_pid_and_args() {
        let parsed = parse_ps_rows("  100 /bin/zsh -l\n  101 claude --model opus\n\n");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].pid, 101);
        assert_eq!(parsed[1].args, "claude --model opus");
    }

    #[test]
    fn an_idle_composer_accepts_a_prompt() {
        let screen = "\n\u{2500}\u{2500}\u{2500}\n\u{276f} Try \"how do I log an error?\"\n\u{2500}\u{2500}\u{2500}\n";
        assert_eq!(screen_status(screen), Some(ScreenStatus::Idle));
        assert_eq!(ScreenStatus::Idle.label(), "idle");
    }

    #[test]
    fn a_running_turn_reads_as_working_not_idle() {
        let screen = "\u{276f} hello\n\u{2726} Thinking… (esc to interrupt)\n";
        assert_eq!(
            screen_status(screen),
            Some(ScreenStatus::Working),
            "a turn in flight must not look ready for the next prompt"
        );
    }

    #[test]
    fn a_native_dialog_reads_as_blocked_even_with_a_composer_on_screen() {
        // A prompt typed here would silently answer the dialog (D-022).
        let screen = "\u{276f} earlier\nQuick safety check:\nIs this a project you created or one you trust?\n\u{276f} Yes, I trust this folder\n";
        assert_eq!(screen_status(screen), Some(ScreenStatus::Blocked));
    }

    #[test]
    fn a_booting_or_plain_shell_screen_has_no_status_yet() {
        assert_eq!(screen_status("$ claude\n"), None);
        assert_eq!(screen_status(""), None);
    }

    #[test]
    fn a_repainted_composer_without_a_leading_newline_still_reads_idle() {
        // Recorded shape: the TUI positions the cursor and paints the prompt
        // mid-line, so the glyph never starts a line.
        let screen = "\u{1b}[13;1H\u{2500}\u{2500}\u{1b}[14;3H\u{276f} Try \"fix lint errors\"\u{1b}[15;1H\u{2500}\u{2500}";
        assert_eq!(screen_status(screen), Some(ScreenStatus::Idle));
    }

    #[test]
    fn only_the_tail_of_a_long_screen_decides_status() {
        // An old dialog scrolled far above must not keep the session blocked.
        let stale = format!(
            "Is this a project you created or one you trust?\n{}\n\u{276f} ready\n",
            "filler line\n".repeat(2000)
        );
        assert_eq!(screen_status(&stale), Some(ScreenStatus::Idle));
    }

    #[test]
    fn ansi_escapes_do_not_hide_the_composer() {
        // Shape recorded from a real promoted TUI: the composer line is
        // preceded by cursor-positioning and colour escapes.
        let screen = "\u{1b}[2J\u{1b}[H\u{1b}[1;36mClaude Code\u{1b}[0m v2.1.270\r\n\u{1b}[38;5;240m\u{2500}\u{2500}\u{2500}\u{1b}[0m\r\n\u{1b}[?25h\u{276f} Try \"how do I log an error?\"\r\n";
        assert_eq!(
            screen_status(screen),
            Some(ScreenStatus::Idle),
            "raw PTY bytes carry ANSI; matching must run on stripped text"
        );
    }

    #[test]
    fn strip_ansi_keeps_text_and_drops_control_sequences() {
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m text"), "red text");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(strip_ansi("\u{1b}(Bplain"), "plain");
    }

    #[test]
    fn screen_fallback_only_fires_on_the_claude_banner() {
        assert_eq!(
            detect_from_screen("\n ✻ Welcome to Claude Code!\n"),
            Some(AgentKind::Claude)
        );
        assert_eq!(detect_from_screen("$ ls -la\ntotal 12\n"), None);
    }
}
