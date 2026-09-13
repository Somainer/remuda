//! Dialog parsers: read a menu, a y/n question, or the Claude trust prompt off
//! the screen and say which keys answer it (D-028 §4.2, §4.4 tier D).
//!
//! Extracted verbatim from `remuda-driver`'s `pty_interaction.rs`. What comes
//! back here is plain data — choices, key sequences, and whether the excerpt
//! was truncated — and the caller turns it into a protocol `Interaction`. That
//! split is what lets this crate stay free of herdr *and* of interaction
//! plumbing, while the D-022 invariants (set `attempted` before writing, never
//! replay an Enter, `answerable &= !screen_truncated`) stay where the writes
//! happen, which is the only place they can be enforced.

use crate::grid::ScreenGrid;

/// Rows of screen kept in a dialog excerpt.
pub const SCREEN_LINES: usize = 32;
/// Bytes of screen kept in a dialog excerpt.
pub const SCREEN_BYTES: usize = 4096;

/// One answerable choice read off the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenChoice {
    /// Stable id — the menu number, or `y`/`n`/`enter`.
    pub id: String,
    /// Label as rendered.
    pub label: String,
    /// Logical keys that select it.
    pub keys: Vec<String>,
}

/// What a screen-derived prompt turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRequest {
    /// Bounded, trimmed excerpt shown to the human.
    pub excerpt: String,
    /// The excerpt dropped content; a reply would be answering blind, so the
    /// caller must clear `answerable` (D-022).
    pub truncated: bool,
    /// Two choice ids collided, so no key mapping can be trusted.
    pub ambiguous: bool,
    /// Choices, empty for a free-text prompt.
    pub choices: Vec<ScreenChoice>,
    /// The prompt reads as a permission/approval rather than a question.
    pub approval: bool,
}

impl ScreenRequest {
    /// The prompt takes typed text rather than a choice.
    #[must_use]
    pub fn free_text(&self) -> bool {
        self.choices.is_empty()
    }
}

/// Words that make a menu an approval rather than a plain question.
const APPROVAL_WORDS: &[&str] = &[
    "approve",
    "approval",
    "permission",
    "allow",
    "trust",
    "proceed",
    "run this command",
];

/// Inline yes/no markers that make a prompt a two-key approval.
const YES_NO_MARKERS: &[&str] = &["[y/n]", "(y/n)", "[yes/no]", "(yes/no)"];

/// Bound a screen to the excerpt a human is shown, reporting whether the bound
/// dropped anything.
#[must_use]
pub fn excerpt(screen: &str) -> (String, bool) {
    let lines: Vec<_> = screen.lines().collect();
    let tail = lines[lines.len().saturating_sub(SCREEN_LINES)..].join("\n");
    let mut start = tail.len().saturating_sub(SCREEN_BYTES);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    let truncated = start != 0 || lines.len() > SCREEN_LINES;
    (tail[start..].trim().to_owned(), truncated)
}

/// Parse whatever prompt is on screen.
///
/// Two shapes are recognised: an inline `[y/n]`, and a numbered menu. A menu
/// with a cursor (`❯`) is answered by arrow keys relative to the current
/// selection rather than by typing its number, because a TUI that draws a
/// cursor is usually not listening for digits.
#[must_use]
pub fn screen_request(grid: &ScreenGrid) -> ScreenRequest {
    let (excerpt, truncated) = excerpt(&grid.text());
    let lower = excerpt.to_lowercase();
    let mut choices: Vec<ScreenChoice> = Vec::new();
    let mut approval;
    if YES_NO_MARKERS.iter().any(|marker| lower.contains(marker)) {
        approval = true;
        choices = vec![
            ScreenChoice {
                id: "y".into(),
                label: "Yes (y)".into(),
                keys: vec!["y".into(), "enter".into()],
            },
            ScreenChoice {
                id: "n".into(),
                label: "No (n)".into(),
                keys: vec!["n".into(), "enter".into()],
            },
        ];
    } else {
        let mut selected = None;
        for line in excerpt.lines() {
            let line = line.trim();
            let marked = line.starts_with(['❯', '›', '>']);
            let line = line.trim_start_matches(['❯', '›', '>']).trim_start();
            let digits = line.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || digits > 2 {
                continue;
            }
            let rest = &line[digits..];
            if !rest.starts_with(['.', ')', ':']) {
                continue;
            }
            let label = rest[1..].trim();
            if label.is_empty() {
                continue;
            }
            if marked {
                selected = Some(choices.len());
            }
            let id = line[..digits].to_owned();
            let mut keys: Vec<_> = id.chars().map(|c| c.to_string()).collect();
            keys.push("enter".into());
            choices.push(ScreenChoice {
                id,
                label: label.to_owned(),
                keys,
            });
        }
        if choices.len() < 2 || choices.len() > 12 {
            choices.clear();
        }
        if let Some(selected) = selected {
            for (index, choice) in choices.iter_mut().enumerate() {
                choice.keys = cursor_keys(index, selected);
            }
        }
        if choices.is_empty()
            && (lower.contains("enter to continue") || lower.contains("press enter to continue"))
        {
            choices.push(ScreenChoice {
                id: "enter".into(),
                label: "Enter to continue".into(),
                keys: vec!["enter".into()],
            });
        }
        approval = !choices.is_empty() && APPROVAL_WORDS.iter().any(|word| lower.contains(word));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(choices.len());
    let mut ambiguous = false;
    for choice in &choices {
        ambiguous |= seen.contains(&choice.id.as_str());
        seen.push(&choice.id);
    }
    if ambiguous {
        choices.clear();
        approval = false;
    }
    ScreenRequest {
        excerpt,
        truncated,
        ambiguous,
        choices,
        approval,
    }
}

/// Arrow keys that move a menu cursor from `from` to `to`, then Enter.
fn cursor_keys(to: usize, from: usize) -> Vec<String> {
    let mut keys = vec![
        if to < from {
            "up".to_owned()
        } else {
            "down".to_owned()
        };
        to.abs_diff(from)
    ];
    keys.push("enter".into());
    keys
}

/// Keys that accept Claude's folder-trust dialog, if that exact dialog with
/// both known choices and exactly one cursor is on screen.
///
/// Narrow on purpose: this is the one dialog D-022 lets a carrier answer
/// automatically, and only inside an already-registered workspace. Anything
/// that does not match exactly returns `None` and the human is asked.
#[must_use]
pub fn trust_dialog_keys(grid: &ScreenGrid) -> Option<Vec<String>> {
    let normalized = grid.flat();
    if !normalized.contains("Quick safety check:")
        || !normalized.contains("Is this a project you created or one you trust?")
    {
        return None;
    }
    let mut choices = Vec::new();
    let mut selected = None;
    for line in grid.lines.iter() {
        let line = line.trim();
        let marked = line.starts_with(['❯', '›', '>']);
        let label = line.trim_start_matches(['❯', '›', '>']).trim();
        // Numbered versions of the same Claude menu use the same cursor keys.
        let label = label
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')'])
            .trim();
        if matches!(label, "No, exit" | "Yes, I trust this folder") {
            if marked && selected.replace(choices.len()).is_some() {
                return None;
            }
            choices.push(label);
        } else if marked {
            return None;
        }
    }
    if choices.len() != 2 || choices[0] == choices[1] {
        return None;
    }
    let selected = selected?;
    let yes = choices
        .iter()
        .position(|label| *label == "Yes, I trust this folder")?;
    Some(cursor_keys(yes, selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(screen: &str) -> ScreenGrid {
        ScreenGrid::from_raw(screen)
    }

    #[test]
    fn a_yes_no_prompt_is_an_approval_with_two_keys() {
        let request = screen_request(&raw("Continue? [y/n]"));
        assert!(request.approval);
        assert_eq!(
            request
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            vec!["y", "n"]
        );
        assert_eq!(request.choices[0].keys, vec!["y", "enter"]);
    }

    #[test]
    fn a_cursor_menu_is_answered_by_arrows_from_the_current_selection() {
        let request = screen_request(&raw("Select environment:\n❯ 1. Development\n2. Staging"));
        assert!(!request.approval, "a plain selection is not an approval");
        assert_eq!(request.choices[0].keys, vec!["enter"]);
        assert_eq!(request.choices[1].keys, vec!["down", "enter"]);
    }

    #[test]
    fn duplicate_ids_make_the_prompt_unanswerable_rather_than_a_guess() {
        let request = screen_request(&raw("Choose:\n1. First\n1. Duplicate"));
        assert!(request.ambiguous);
        assert!(request.choices.is_empty());
        assert!(!request.approval);
    }

    #[test]
    fn a_free_text_prompt_has_no_choices() {
        let request = screen_request(&raw("Type a project name:"));
        assert!(request.free_text());
        assert!(!request.truncated);
    }

    #[test]
    fn a_long_screen_is_excerpted_and_flagged_truncated() {
        let request = screen_request(&raw(&"界".repeat(3000)));
        assert!(request.truncated, "a blind reply must be refused (D-022)");
        assert!(request.excerpt.len() <= SCREEN_BYTES);
    }

    #[test]
    fn an_approval_word_promotes_a_menu_to_an_approval() {
        let request = screen_request(&raw("Allow this command?\n❯ 1. Yes\n2. No"));
        assert!(request.approval);
    }

    #[test]
    fn exact_trust_dialog_requires_unambiguous_selected_menu() {
        let title = "Quick safety check: Is this a project you created or one you trust?";
        assert_eq!(
            trust_dialog_keys(&raw(&format!(
                "{title}\n❯ No, exit\nYes, I trust this folder"
            ))),
            Some(vec!["down".into(), "enter".into()])
        );
        assert_eq!(
            trust_dialog_keys(&raw(&format!(
                "{title}\n❯ Yes, I trust this folder\nNo, exit"
            ))),
            Some(vec!["enter".into()])
        );
        for screen in [
            "Do you trust this command?\n❯ No, exit\nYes, I trust this folder".to_owned(),
            format!("{title}\nNo, exit\nYes, I trust this folder"),
            format!("{title}\n❯ No, exit\n❯ Yes, I trust this folder"),
            format!("{title}\n❯ No, exit\nYes, I trust this folder\nNo, exit"),
        ] {
            assert!(trust_dialog_keys(&raw(&screen)).is_none(), "{screen}");
        }
    }
}
