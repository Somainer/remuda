//! Screen dialects for the three harnesses.
//!
//! `render` produces the exact byte repaint the binary writes into the PTY:
//! alt-screen / bracketed-paste DECSET, OSC 0 title and OSC 9;4 progress,
//! then a full home-and-redraw of clipped logical lines plus the cursor
//! position. `render_grid` is the deterministic plaintext view golden
//! snapshots are compared against.
//!
//! Strings come from the captured evidence:
//! - codex dialogs / working line: `docs/design/evidence/codex-signals-1.md`;
//! - grok footer / `[stop]` chip / permission box: `grok-signals-1.md`;
//! - claude glyph / `esc to interrupt` / trust dialog: `promote.rs` and
//!   `pty_interaction.rs` detectors. The claude evidence session captured no
//!   approval frame, so its three choice lines are modeled on 2.1.x and kept
//!   here in one table for easy correction (documented in
//!   `testing-fake-harness.md`).

/// Harness screen dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialect {
    /// Claude Code 2.1.x TUI.
    Claude,
    /// Codex CLI 0.154 TUI.
    Codex,
    /// Grok 1.0.30 TUI.
    Grok,
}

impl Dialect {
    /// Parse `--kind`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "grok" => Ok(Self::Grok),
            other => Err(format!("unknown kind {other:?} (want claude|codex|grok)")),
        }
    }

    /// Client version string shown in the UI.
    #[must_use]
    pub fn version(self) -> &'static str {
        match self {
            Self::Claude => "2.1.270 (Claude Code)",
            Self::Codex => "0.154.0",
            Self::Grok => "1.0.30",
        }
    }
}

/// Screen-dialect version, selected with `--dialect-version`.
///
/// `Legacy` is the dialect the golden snapshots were captured against and must
/// stay byte-stable. `Modern` reproduces claude 2.1.270's measured TUI: the
/// `esc to interrupt` footer phrase is gone and turn edges ride `OSC 0` /
/// `OSC 9;4` with an **empty** percent field (`9;4;3;`, not `9;4;3;0`). Only
/// the claude dialect has a modern variant today.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DialectVersion {
    /// Pre-2.1.270 phrases; OSC only at entry.
    #[default]
    Legacy,
    /// 2.1.270: no `esc to interrupt`, live OSC edges with empty percent.
    Modern,
}

impl DialectVersion {
    /// Parse `--dialect-version`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "legacy" => Ok(Self::Legacy),
            "modern" => Ok(Self::Modern),
            other => Err(format!(
                "unknown dialect version {other:?} (want legacy|modern)"
            )),
        }
    }

    /// Whether this is the 2.1.270 dialect.
    #[must_use]
    pub fn is_modern(self) -> bool {
        self == Self::Modern
    }
}

/// What the screen is currently doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenMode {
    /// First-start trust-directory dialog.
    Trust,
    /// Composer ready.
    Idle,
    /// A turn is running.
    Working,
    /// Native approval dialog owns the keyboard.
    Approval,
}

/// Working-phase detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkingPhase {
    /// Spinner before the first token.
    Thinking,
    /// A tool process is running.
    Running,
    /// Final text is streaming.
    Responding,
}

/// Approval dialog content.
#[derive(Clone, Debug)]
pub struct ApprovalView {
    /// Tool name in the header line.
    pub tool: String,
    /// Command/description shown verbatim.
    pub command: String,
    /// Optional justification/reason line.
    pub reason: Option<String>,
    /// Selected menu index (0-based).
    pub selected: usize,
    /// Grok's wider numbered choice set.
    pub choices: Vec<String>,
}

/// Everything the renderer needs; the engine owns the canonical instance.
#[derive(Clone, Debug)]
pub struct View {
    /// Which harness.
    pub dialect: Dialect,
    /// Current mode.
    pub mode: ScreenMode,
    /// Finished transcript lines (most recent at the bottom), newest last.
    pub transcript: Vec<String>,
    /// In-flight assistant text line, replaced as chunks stream in.
    pub streaming_line: Option<String>,
    /// Current composer text.
    pub draft: String,
    /// Queued follow-up prompts.
    pub queued: Vec<String>,
    /// codex steer lines admitted into the running turn.
    pub steering: Vec<String>,
    /// Working phase, when mode is Working.
    pub phase: Option<WorkingPhase>,
    /// Elapsed seconds for the working spinner.
    pub elapsed_secs: u64,
    /// Background terminal count (codex screen).
    pub background: u32,
    /// Approval dialog, when mode is Approval.
    pub approval: Option<ApprovalView>,
    /// Transient notice (grok's "Press Ctrl+c to cancel the turn").
    pub notice: Option<String>,
    /// Tool description for title/header ("Running: …").
    pub running_tool: Option<String>,
    /// Model label.
    pub model: String,
    /// Working-directory basename for title/banner.
    pub dir_name: String,
    /// Whether the binary entered the alt screen (`--no-alt-screen` clears it).
    pub alt_screen: bool,
    /// Legacy or 2.1.270 modern screen dialect (claude only).
    pub dialect_version: DialectVersion,
}

impl View {
    /// Fresh idle view.
    #[must_use]
    pub fn new(dialect: Dialect, model: String, dir_name: String) -> Self {
        Self {
            dialect,
            mode: ScreenMode::Idle,
            transcript: Vec::new(),
            streaming_line: None,
            draft: String::new(),
            queued: Vec::new(),
            steering: Vec::new(),
            phase: None,
            elapsed_secs: 0,
            background: 0,
            approval: None,
            notice: None,
            running_tool: None,
            model,
            dir_name,
            alt_screen: true,
            dialect_version: DialectVersion::Legacy,
        }
    }

    /// OSC 0 title payload.
    #[must_use]
    pub fn title(&self) -> String {
        match self.dialect {
            Dialect::Claude if self.dialect_version.is_modern() => {
                // 2.1.270 (claude-channels §3.1): glyph-prefixed title. `✳` is
                // idle *or* a dialog; `◐`/`◑` alternate ~1 Hz while busy.
                match self.mode {
                    ScreenMode::Working => {
                        let glyph = if self.elapsed_secs.is_multiple_of(2) {
                            '\u{25d0}'
                        } else {
                            '\u{25d1}'
                        };
                        format!("{glyph} {}", self.dir_name)
                    }
                    // Approval and plain idle share the spark; progress (not
                    // the title) disambiguates them.
                    _ => format!("\u{2733} {}", self.dir_name),
                }
            }
            Dialect::Claude => match self.mode {
                ScreenMode::Approval => format!("⚠ Action Required - {}", self.dir_name),
                ScreenMode::Working => format!("{} - claude", self.dir_name),
                _ => format!("{} - claude", self.dir_name),
            },
            Dialect::Codex => "codex".to_owned(),
            Dialect::Grok => {
                let suffix = "grok";
                match self.mode {
                    ScreenMode::Approval => {
                        let tool = truncate(&self.running_tool.clone().unwrap_or_default(), 24);
                        format!("⚠ Action Required - ⠦ - {tool} - {suffix}")
                    }
                    ScreenMode::Working => match self.phase {
                        Some(WorkingPhase::Running) => {
                            let tool = self.running_tool.clone().unwrap_or_else(|| "tool".into());
                            format!("⠸ - Running: {tool} - {suffix}")
                        }
                        _ => format!("⠦ - Thinking - {suffix}"),
                    },
                    _ => suffix.to_owned(),
                }
            }
        }
    }

    /// Raw `OSC 9;4` payload after the `9;4;` prefix.
    ///
    /// Modern claude emits the state token with an **empty** percent
    /// (`3;` / `0;`), exactly as measured in the 2.1.270 probes; the legacy
    /// dialect keeps `state;value` so its existing golden bytes are unchanged.
    #[must_use]
    pub fn osc_progress_payload(&self) -> String {
        let (state, value) = self.progress();
        if self.dialect == Dialect::Claude && self.dialect_version.is_modern() {
            format!("{state};")
        } else {
            format!("{state};{value}")
        }
    }

    /// OSC 9;4 parameters `(state, value)`.
    #[must_use]
    pub fn progress(&self) -> (u8, i64) {
        match (&self.dialect, self.mode) {
            (Dialect::Claude, ScreenMode::Working) => (3, 0),
            (Dialect::Claude, ScreenMode::Approval) => (3, 0),
            (_, ScreenMode::Working | ScreenMode::Approval) => (1, -1),
            _ => (0, 0),
        }
    }

    fn working_line(&self) -> String {
        let elapsed = self.elapsed_secs;
        match self.dialect {
            Dialect::Claude if self.dialect_version.is_modern() => {
                // The defining D-2 trait: no `esc to interrupt` anywhere. The
                // spinner verb is decoration and the elapsed counter is the
                // only machine-readable part (design §2.4: never transported).
                match self.phase {
                    Some(WorkingPhase::Running) => {
                        format!("\u{273b} Grooving… ({elapsed}s)")
                    }
                    Some(WorkingPhase::Responding) => "\u{273b} Responding…".to_owned(),
                    _ => "\u{273b} Thinking…".to_owned(),
                }
            }
            Dialect::Claude => match self.phase {
                Some(WorkingPhase::Running) => {
                    let tool = self.running_tool.clone().unwrap_or_else(|| "Bash".into());
                    format!("⏳ {tool}… (esc to interrupt)")
                }
                Some(WorkingPhase::Responding) => "✻ Responding… (esc to interrupt)".to_owned(),
                _ => "✻ Thinking… (esc to interrupt)".to_owned(),
            },
            Dialect::Codex => {
                let mut line = format!("Working ({elapsed}s • esc to interrupt)");
                if self.background > 0 {
                    line.push_str(&format!(
                        " · {} background terminal{} running",
                        self.background,
                        if self.background == 1 { "" } else { "s" }
                    ));
                }
                line
            }
            Dialect::Grok => {
                let spinner = if matches!(self.phase, Some(WorkingPhase::Running)) {
                    "⠸"
                } else {
                    "⠋"
                };
                let label = match self.phase {
                    Some(WorkingPhase::Running) => "Running…",
                    Some(WorkingPhase::Responding) => "Responding…",
                    _ => "Thinking…",
                };
                // The `[stop]` chip is the grok working anchor (D-028 §10),
                // deliberately used instead of the braille spinner alone.
                format!(
                    "{spinner} {label} {elapsed}.0s{pad}[stop]",
                    pad = " ".repeat(20usize.saturating_sub(label.len() + 4))
                )
            }
        }
    }

    fn composer_prompt(&self) -> &'static str {
        match self.dialect {
            Dialect::Claude => "❯ ",
            Dialect::Codex => "› ",
            Dialect::Grok => "❯ ",
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    chars.truncate(max.saturating_sub(1));
    chars.into_iter().collect::<String>() + "…"
}

fn clip(line: &str, cols: usize) -> String {
    line.chars().take(cols).collect()
}

/// Append the finished transcript plus any in-flight streaming line.
fn extend_conversation(lines: &mut Vec<String>, view: &View) {
    lines.extend(view.transcript.iter().cloned());
    if let Some(line) = &view.streaming_line {
        lines.push(line.clone());
    }
}

/// Plain logical lines in draw order, before viewport clipping.
fn logical_lines(view: &View) -> Vec<String> {
    match view.dialect {
        Dialect::Claude => claude_lines(view),
        Dialect::Codex => codex_lines(view),
        Dialect::Grok => grok_lines(view),
    }
}

fn claude_lines(view: &View) -> Vec<String> {
    let mut lines = Vec::new();
    match view.mode {
        ScreenMode::Trust => {
            lines.push("Welcome to Claude Code!".into());
            lines.push(String::new());
            lines.push("Quick safety check:".into());
            lines.push("Is this a project you created or one you trust?".into());
            lines.push(String::new());
            if view.approval.as_ref().is_some_and(|a| a.selected == 0) {
                lines.push("❯ No, exit".into());
                lines.push("  Yes, I trust this folder".into());
            } else {
                lines.push("  No, exit".into());
                lines.push("❯ Yes, I trust this folder".into());
            }
            return lines;
        }
        ScreenMode::Approval => {
            extend_conversation(&mut lines, view);
            if let Some(approval) = &view.approval {
                lines.push(String::new());
                lines.push("Do you want to run this command?".into());
                lines.push(format!("⏺ {}({})", approval.tool, approval.command));
                if let Some(reason) = &approval.reason {
                    lines.push(reason.clone());
                }
                lines.push(String::new());
                for (idx, choice) in approval.choices.iter().enumerate() {
                    let marker = if idx == approval.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    lines.push(format!("{marker}{choice}"));
                }
            }
            return lines;
        }
        _ => {}
    }
    extend_conversation(&mut lines, view);
    if matches!(view.mode, ScreenMode::Working) {
        lines.push(String::new());
        lines.push(view.working_line());
        for queued in &view.queued {
            lines.push(format!("  ↳ {queued}"));
        }
        if !view.queued.is_empty() {
            lines.push("Press up to edit queued messages".into());
        }
        lines.push(String::new());
        lines.push(format!("{}{}", view.composer_prompt(), view.draft));
        if view.draft.is_empty() {
            // nothing
        }
    } else {
        lines.push(String::new());
        lines.push(format!("{}{}", view.composer_prompt(), view.draft));
        lines.push(String::new());
        lines.push("  /help for shortcuts".into());
    }
    lines
}

fn codex_lines(view: &View) -> Vec<String> {
    let mut lines = Vec::new();
    match view.mode {
        ScreenMode::Trust => {
            lines.push("OpenAI Codex".into());
            lines.push(String::new());
            lines.push("Do you trust the files in this folder?".into());
            lines.push(view.dir_name.to_string());
            lines.push(String::new());
            lines.push("❯ 1. Yes, proceed".into());
            lines.push("  2. No, exit".into());
            return lines;
        }
        ScreenMode::Approval => {
            extend_conversation(&mut lines, view);
            if let Some(approval) = &view.approval {
                lines.push(String::new());
                lines.push("Would you like to run the following command?".into());
                if let Some(reason) = &approval.reason {
                    lines.push(format!("Reason: {reason}"));
                }
                lines.push(format!("$ {}", approval.command));
                for (idx, choice) in approval.choices.iter().enumerate() {
                    let marker = if idx == approval.selected {
                        "› "
                    } else {
                        "  "
                    };
                    lines.push(format!("{marker}{choice}"));
                }
                lines.push("Press enter to confirm or esc to cancel".into());
            }
            return lines;
        }
        _ => {}
    }
    // Banner is part of the scrollback, emitted once on first draw.
    extend_conversation(&mut lines, view);
    if matches!(view.mode, ScreenMode::Working) {
        lines.push(String::new());
        lines.push(view.working_line());
        for steer in &view.steering {
            lines.push(String::new());
            lines.push(
                "Messages to be submitted after next tool call (press esc to interrupt and send immediately)"
                    .into(),
            );
            lines.push(format!("  ↳ {steer}"));
        }
        if !view.queued.is_empty() {
            lines.push("Queued follow-up inputs".into());
            for queued in &view.queued {
                lines.push(format!("  ↳ {queued}"));
            }
            lines.push("    ⌥ + ↑ edit last queued message".into());
        }
        if view.notice.as_deref() == Some("interrupted") {
            lines.push(String::new());
            lines.push("Conversation interrupted - tell the model what to do differently.".into());
            if view.background > 0 {
                lines.push(format!(
                    "{} background terminal running · /ps to view · /stop to close",
                    view.background
                ));
            }
        }
        lines.push(String::new());
        lines.push(format!("› {}", view.draft));
        if view.draft.is_empty() && view.steering.is_empty() && view.queued.is_empty() {
            // no hint while composing a steer
        } else if !view.draft.is_empty() {
            lines.push("tab to queue message".into());
        }
    } else {
        lines.push(String::new());
        lines.push(format!("› {}", view.draft));
        lines.push(String::new());
        lines.push(format!("model: {}", view.model));
    }
    lines
}

fn grok_lines(view: &View) -> Vec<String> {
    let mut lines = Vec::new();
    match view.mode {
        ScreenMode::Approval => {
            extend_conversation(&mut lines, view);
            if let Some(approval) = &view.approval {
                lines.push(String::new());
                lines.push("┃".into());
                if let Some(reason) = &approval.reason {
                    lines.push(format!("┃  {reason}"));
                }
                lines.push(format!("┃  {}", approval.command));
                lines.push("┃  ← → narrow scope".into());
                lines.push("┃".into());
                for (idx, choice) in approval.choices.iter().enumerate() {
                    let radio = if idx == approval.selected {
                        "●"
                    } else {
                        "○"
                    };
                    lines.push(format!("┃  {} ({radio}) {choice}", idx + 1));
                }
                lines.push("┃".into());
                lines.push(
                    "┃  1/4:select  │  Tab:next option  │  ←/→:scope  │  Ctrl+o:always-approve  │  Ctrl+c:cancel"
                        .into(),
                );
            }
            return lines;
        }
        ScreenMode::Trust => {
            lines.push("grok".into());
            lines.push(String::new());
            lines.push("❯ 1. Yes, proceed".into());
            lines.push("  2. No, exit".into());
            return lines;
        }
        _ => {}
    }
    extend_conversation(&mut lines, view);
    if let Some(notice) = &view.notice {
        lines.push(notice.clone());
    }
    if matches!(view.mode, ScreenMode::Working) {
        lines.push(String::new());
        lines.push(view.working_line());
        for (idx, queued) in view.queued.iter().enumerate() {
            lines.push(format!("#{} {queued}", idx + 1));
        }
        // Composer box, width clamped to narrow terminals.
        let width = 78usize;
        let inner = width.saturating_sub(4).max(8);
        lines.push("╭".to_owned() + &"─".repeat(inner + 2) + "╮");
        let draft = truncate(&view.draft, inner);
        lines.push(format!("│ ❯ {draft:<inner$} │"));
        let label = format!(" {} ─╯", view.model);
        let bar = "─".repeat((inner + 2).saturating_sub(label.chars().count()));
        lines.push(format!("╰{bar}{label}"));
        lines.push(grok_footer(view));
    } else {
        lines.push(String::new());
        lines.push(format!("❯ {}", view.draft));
        lines.push(String::new());
        lines.push(grok_footer(view));
    }
    lines
}

fn grok_footer(view: &View) -> String {
    let mut parts = vec!["Shift+Tab:mode".to_owned(), "Ctrl+.:shortcuts".to_owned()];
    match view.mode {
        ScreenMode::Working => {
            if !view.queued.is_empty() {
                parts.insert(0, "Enter:send now".to_owned());
                parts.insert(1, "Ctrl+;:queue".to_owned());
            } else if !view.draft.is_empty() {
                parts.insert(0, "Enter:queue".to_owned());
                parts.insert(1, "Ctrl+Enter:send now".to_owned());
            }
            parts.insert(
                if view.draft.is_empty() { 0 } else { 2 },
                "Ctrl+c:cancel".into(),
            );
        }
        ScreenMode::Idle => {
            parts.insert(0, "Enter:submit".to_owned());
        }
        ScreenMode::Approval | ScreenMode::Trust => {
            parts.insert(0, "Ctrl+c:cancel".to_owned());
        }
    }
    parts.join("  │  ")
}

/// Render the plaintext grid (`rows` lines, each exactly `cols` columns of
/// cell text). The logical block is anchored at the **bottom** like a real
/// TUI (blank rows pad the top); overflow scrolls off the top and long lines
/// clip at `cols`.
#[must_use]
pub fn render_grid(view: &View, cols: u16, rows: u16) -> Vec<String> {
    let cols = cols.max(1) as usize;
    let rows = rows.max(1) as usize;
    let logical = logical_lines(view);
    let visible = logical.len().min(rows);
    let top_pad = rows - visible;
    let start = logical.len() - visible;
    let mut grid = Vec::with_capacity(rows);
    for _ in 0..top_pad {
        grid.push(" ".repeat(cols));
    }
    for line in &logical[start..] {
        let mut line = clip(line, cols);
        while line.chars().count() < cols {
            line.push(' ');
        }
        grid.push(line);
    }
    grid
}

/// Cursor (row, col), 1-based, for the current view.
#[must_use]
pub fn cursor_pos(view: &View, cols: u16, rows: u16) -> (u16, u16) {
    let logical = logical_lines(view);
    let top_pad = (rows as usize).saturating_sub(logical.len().min(rows as usize));
    let row = (top_pad + logical.len()).max(1) as u16;
    let prompt_cols = view.composer_prompt().chars().count() as u16;
    let col = (prompt_cols + view.draft.chars().count() as u16 + 1).min(cols);
    (row, col)
}

/// One-time terminal entry: alt-screen (unless the view disabled it),
/// bracketed-paste enable, OSC title and progress.
#[must_use]
pub fn enter(view: &View) -> String {
    let mut out = String::new();
    if view.alt_screen {
        out.push_str("\x1b[?1049h");
    }
    out.push_str("\x1b[?2004h");
    out.push_str(&format!("\x1b]0;{}\x07", view.title()));
    out.push_str(&format!("\x1b]9;4;{}\x07", view.osc_progress_payload()));
    out
}

/// The OSC 0 / OSC 9;4 bytes needed to move from `(prev_title, prev_progress)`
/// to the view's current regions.
///
/// The modern claude dialect calls this on every repaint: the real TUI emits
/// these edges at turn start (+16–31 ms), at permission dialogs (−9 ms) and at
/// turn end (`9;4;0` right after `Stop`). The legacy dialect emits OSC only at
/// [`enter`], so its callers must not use this.
#[must_use]
pub fn osc_transitions(
    view: &View,
    prev_title: &str,
    prev_progress: &str,
) -> (String, Option<String>, Option<String>) {
    let title = view.title();
    let progress = view.osc_progress_payload();
    let mut out = String::new();
    let mut next_title = None;
    let mut next_progress = None;
    if title != prev_title {
        out.push_str(&format!("\x1b]0;{title}\x07"));
        next_title = Some(title);
    }
    if progress != prev_progress {
        out.push_str(&format!("\x1b]9;4;{progress}\x07"));
        next_progress = Some(progress);
    }
    (out, next_title, next_progress)
}

/// Full byte repaint for a PTY: home-clear, every grid row positioned
/// absolutely, cursor. Pair with [`enter`] once at startup.
#[must_use]
pub fn repaint(view: &View, cols: u16, rows: u16) -> String {
    let grid = render_grid(view, cols, rows);
    let (crow, ccol) = cursor_pos(view, cols, rows);
    let mut out = String::new();
    out.push_str("\x1b[1;1H\x1b[2J");
    for (idx, line) in grid.iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H", idx + 1));
        out.push_str(line);
    }
    out.push_str("\x1b[?25h");
    out.push_str(&format!("\x1b[{crow};{ccol}H"));
    out
}

/// Exit sequence written on shutdown: leave alt-screen and reset paste mode.
#[must_use]
pub fn teardown(view: &View) -> String {
    let mut out = String::new();
    out.push_str("\x1b[?2004l");
    if view.alt_screen {
        out.push_str("\x1b[?1049l");
    }
    out
}

/// Default approval dialog choices per dialect (exact evidence strings).
#[must_use]
pub fn default_choices(dialect: Dialect) -> Vec<String> {
    match dialect {
        Dialect::Claude => vec![
            "1. Yes, proceed once".into(),
            "2. Yes, and don't ask again for this command".into(),
            "3. No, and tell Claude what to do differently (esc)".into(),
        ],
        Dialect::Codex => vec![
            "1. Yes, proceed (y)".into(),
            "3. No, and tell Codex what to do differently (esc)".into(),
        ],
        Dialect::Grok => vec![
            "Yes, and don't ask again for anything (always-approve mode)".into(),
            "Yes, proceed".into(),
            "No, reject (type to add feedback)".into(),
            "Never allow: printf".into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working(dialect: Dialect) -> View {
        let mut view = View::new(dialect, "spike".into(), "work".into());
        view.mode = ScreenMode::Working;
        view.phase = Some(WorkingPhase::Thinking);
        view.elapsed_secs = 2;
        view.running_tool = Some("Bash".into());
        view
    }

    #[test]
    fn claude_working_line_has_esc_hint_and_glyph() {
        let text = render_grid(&working(Dialect::Claude), 80, 24).join("\n");
        assert!(text.contains("esc to interrupt"));
        assert!(text.contains('❯'));
    }

    #[test]
    fn codex_working_line_and_glyph() {
        let text = render_grid(&working(Dialect::Codex), 80, 24).join("\n");
        assert!(text.contains("Working (2s • esc to interrupt)"));
        assert!(text.contains('›'));
    }

    #[test]
    fn grok_working_has_stop_chip_in_title_and_queue_footer() {
        let mut view = working(Dialect::Grok);
        view.draft = "FOLLOW".into();
        let text = render_grid(&view, 105, 30).join("\n");
        assert!(text.contains("Enter:queue"));
        assert!(text.contains("Ctrl+Enter:send now"));
        assert!(view.title().contains("Thinking"));
    }

    #[test]
    fn alt_screen_paste_and_osc_are_emitted() {
        let view = working(Dialect::Grok);
        let entry = enter(&view);
        assert!(entry.contains("\x1b[?1049h"));
        assert!(entry.contains("\x1b[?2004h"));
        assert!(entry.contains("\x1b]0;"));
        let repaint = repaint(&view, 80, 24);
        assert!(repaint.contains("\x1b[1;1H"));
        // The OSC 9;4 payload rides the one-time entry, not every repaint.
        assert!(entry.contains("\x1b]9;4;1;-1\x07"));
        assert!(teardown(&view).contains("\x1b[?1049l"));
    }

    #[test]
    fn narrow_grid_clips_and_pads() {
        let grid = render_grid(&working(Dialect::Claude), 40, 20);
        assert_eq!(grid.len(), 20);
        for line in &grid {
            assert_eq!(line.chars().count(), 40);
        }
    }

    #[test]
    fn trust_dialog_strings_match_detectors() {
        let mut view = View::new(Dialect::Claude, "m".into(), "work".into());
        view.mode = ScreenMode::Trust;
        let text = render_grid(&view, 80, 24).join("\n");
        assert!(text.contains("Yes, I trust this folder"));
        assert!(text.contains("Is this a project you created or one you trust?"));
    }

    #[test]
    fn modern_claude_working_screen_has_no_esc_to_interrupt() {
        // D-2: the phrase this defect is named for must genuinely be absent.
        let mut view = View::new(Dialect::Claude, "m".into(), "work".into());
        view.dialect_version = DialectVersion::Modern;
        view.mode = ScreenMode::Working;
        view.phase = Some(WorkingPhase::Running);
        let text = render_grid(&view, 80, 24).join("\n");
        assert!(!text.contains("esc to interrupt"), "modern text:\n{text}");
        assert!(!text.contains("interrupt"));
        // And the legacy dialect keeps it, so the goldens stay meaningful.
        view.dialect_version = DialectVersion::Legacy;
        let legacy = render_grid(&view, 80, 24).join("\n");
        assert!(legacy.contains("esc to interrupt"));
    }

    #[test]
    fn modern_claude_emits_osc_edges_with_an_empty_percent() {
        let mut view = View::new(Dialect::Claude, "m".into(), "probe".into());
        view.dialect_version = DialectVersion::Modern;
        assert_eq!(view.osc_progress_payload(), "0;");
        let idle_title = view.title();
        assert!(idle_title.starts_with('\u{2733}'), "{idle_title:?}");

        view.mode = ScreenMode::Working;
        assert_eq!(view.osc_progress_payload(), "3;");
        let busy_title = view.title();
        assert!(
            busy_title.starts_with(['\u{25d0}', '\u{25d1}']),
            "{busy_title:?}"
        );
        // Legacy keeps the full state;value shape.
        view.dialect_version = DialectVersion::Legacy;
        assert_eq!(view.osc_progress_payload(), "3;0");
    }

    #[test]
    fn modern_osc_transitions_fire_only_on_the_edge() {
        let mut view = View::new(Dialect::Claude, "m".into(), "probe".into());
        view.dialect_version = DialectVersion::Modern;
        let prev_title = view.title();
        let prev_progress = view.osc_progress_payload();
        let (quiet, t, p) = osc_transitions(&view, &prev_title, &prev_progress);
        assert!(quiet.is_empty());
        assert!(t.is_none() && p.is_none());

        view.mode = ScreenMode::Working;
        let (edges, t, p) = osc_transitions(&view, &prev_title, &prev_progress);
        assert!(edges.contains("\x1b]9;4;3;\x07"), "{edges:?}");
        assert!(
            edges.starts_with("\x1b]0;"),
            "title edge rides the turn start"
        );
        assert_eq!(t, Some(view.title()));
        assert_eq!(p, Some("3;".to_owned()));
    }
}
