//! D-060 (cb-memory-inject): the lineage memory a chapter starts with.
//!
//! The Hub renders the index plus about-me at chapter start and stamps it on
//! the forwarded create/resume params as `memoryContext {rev, text}`. The
//! launch materializer is its only writer (main-agent.md §12.3), with one of
//! two mechanisms per launch path:
//!
//! - **Flag.** The paths that build the Claude argv themselves (`claude_argv`,
//!   and `materialize_shell_pty_agent` for kind `claude`) push
//!   `--append-system-prompt <text>` once, after the caller-derived arguments,
//!   and record the pair on the recipe as their own emission. The
//!   finished-argv guards skip exactly that one pair and refuse every other
//!   occurrence; `BANNED` and the `spec.args` checks are unchanged.
//! - **Data block.** Every other agent launch records the framed text on the
//!   recipe, and the driver puts it ahead of the first input it delivers.
//!
//! Kinds `terminal` and `generic` get neither: no agent reads their first
//! input, so the text would be typed into a shell or an arbitrary program.

use crate::recipe::LaunchRecipe;
pub use remuda_protocol::hubnode::MemoryContext;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// The one flag the materializer emits for `memoryContext`.
pub const APPEND_SYSTEM_PROMPT: &str = "--append-system-prompt";

/// How a launch carries the chapter's memory, recorded on the recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mechanism", rename_all = "kebab-case")]
pub enum RecipeMemory {
    /// The materializer's own `--append-system-prompt <text>` pair in argv.
    Flag {
        /// Lineage memory revision the text was rendered at.
        rev: u64,
        /// The flag's value, exactly as emitted.
        text: String,
    },
    /// Framed data the driver delivers ahead of the first input.
    DataBlock {
        /// Lineage memory revision the text was rendered at.
        rev: u64,
        /// The block, exactly as it precedes the first input.
        text: String,
    },
}

impl RecipeMemory {
    /// The emitted `--append-system-prompt` value, for a flag recipe.
    #[must_use]
    pub fn flag_text(&self) -> Option<&str> {
        match self {
            Self::Flag { text, .. } => Some(text),
            Self::DataBlock { .. } => None,
        }
    }

    /// The first-input block, for a data-block recipe.
    #[must_use]
    pub fn data_block(&self) -> Option<&str> {
        match self {
            Self::DataBlock { text, .. } => Some(text),
            Self::Flag { .. } => None,
        }
    }
}

impl LaunchRecipe {
    /// The `--append-system-prompt` value this recipe's materializer emitted
    /// itself: the one pair the finished-argv guards let through.
    #[must_use]
    pub fn own_system_prompt(&self) -> Option<&str> {
        self.memory.as_ref().and_then(RecipeMemory::flag_text)
    }

    /// The data block the driver delivers ahead of its first input.
    #[must_use]
    pub fn memory_data_block(&self) -> Option<&str> {
        self.memory.as_ref().and_then(RecipeMemory::data_block)
    }
}

/// The first token a finished-argv guard refuses, skipping exactly one
/// `--append-system-prompt <own>` pair: the materializer's own emission.
///
/// Every other `--append-system-prompt` (a second pair, a different value, the
/// `=` spelling) reaches `refused` like any other token, so only the recorded
/// pair is exempt.
pub(crate) fn first_refused<'a>(
    argv: &'a [String],
    own: Option<&str>,
    refused: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    let mut exempt = own;
    let mut index = 0;
    while let Some(token) = argv.get(index) {
        if token == APPEND_SYSTEM_PROMPT
            && let Some(text) = exempt
            && argv.get(index + 1).is_some_and(|value| value == text)
        {
            exempt = None;
            index += 2;
            continue;
        }
        if refused(token) {
            return Some(token);
        }
        index += 1;
    }
    None
}

/// The first delivered input with the data block ahead of it.
#[must_use]
pub fn with_data_block(block: &str, input: &str) -> String {
    if input.is_empty() {
        block.to_owned()
    } else {
        format!("{block}\n\n{input}")
    }
}

/// A driver's not-yet-delivered data block: armed from the recipe at launch,
/// put ahead of the first input that is actually written, then gone. A send
/// that fails before writing keeps it for the retry.
#[derive(Debug, Default)]
pub(crate) struct PendingDataBlock(Mutex<Option<String>>);

impl PendingDataBlock {
    /// Arm from a freshly materialized recipe (clears any previous launch's).
    pub(crate) fn arm(&self, recipe: &LaunchRecipe) {
        *self.slot() = recipe.memory_data_block().map(str::to_owned);
    }

    /// `input` as it must be written: with the block ahead of it while one is
    /// pending. Returns whether the block was included.
    pub(crate) fn compose(&self, input: String) -> (String, bool) {
        match self.slot().as_deref() {
            Some(block) => (with_data_block(block, &input), true),
            None => (input, false),
        }
    }

    /// The block reached the agent; later inputs go out as they are.
    pub(crate) fn delivered(&self) {
        *self.slot() = None;
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    fn banned(token: &str) -> bool {
        crate::flags::is_banned_flag_token(token)
    }

    #[test]
    fn only_the_recorded_pair_is_exempt() {
        let own = "<remuda-memory>index</remuda-memory>";
        let finished = argv(&["--model", "m", APPEND_SYSTEM_PROMPT, own]);
        assert_eq!(first_refused(&finished, Some(own), banned), None);
        // Without the recorded emission the same pair is refused.
        assert_eq!(
            first_refused(&finished, None, banned),
            Some(APPEND_SYSTEM_PROMPT)
        );
        // A second occurrence, even with the same value, is refused.
        let twice = argv(&[APPEND_SYSTEM_PROMPT, own, APPEND_SYSTEM_PROMPT, own]);
        assert_eq!(
            first_refused(&twice, Some(own), banned),
            Some(APPEND_SYSTEM_PROMPT)
        );
        // A different value is not the materializer's pair.
        let other = argv(&[APPEND_SYSTEM_PROMPT, "caller text"]);
        assert_eq!(
            first_refused(&other, Some(own), banned),
            Some(APPEND_SYSTEM_PROMPT)
        );
        // `--system-prompt` is never emitted and never exempt.
        let replace = argv(&[APPEND_SYSTEM_PROMPT, own, "--system-prompt", "x"]);
        assert_eq!(
            first_refused(&replace, Some(own), banned),
            Some("--system-prompt")
        );
        let joined = format!("{APPEND_SYSTEM_PROMPT}={own}");
        assert_eq!(
            first_refused(&argv(&[joined.as_str()]), Some(own), banned),
            Some(joined.as_str())
        );
    }

    #[test]
    fn the_block_precedes_only_the_first_written_input() {
        let pending = PendingDataBlock::default();
        let mut recipe_memory = Some(RecipeMemory::DataBlock {
            rev: 3,
            text: "BLOCK".into(),
        });
        let block = recipe_memory.take().unwrap();
        *pending.slot() = block.data_block().map(str::to_owned);
        // A failed write keeps it for the retry.
        assert_eq!(pending.compose("hi".into()), ("BLOCK\n\nhi".into(), true));
        assert_eq!(pending.compose("hi".into()), ("BLOCK\n\nhi".into(), true));
        pending.delivered();
        assert_eq!(pending.compose("again".into()), ("again".into(), false));
        assert_eq!(with_data_block("B", ""), "B");
    }
}
