//! Declarative scenario format for `fake-harness`.
//!
//! A scenario is a JSON or YAML document (chosen by file extension, defaulting
//! to JSON) describing the turns the fake harness plays. Every field below is
//! optional except [`Scenario::turns`]; the fake's own defaults reproduce the
//! evidence sessions in `docs/design/evidence/` (`RUN_TOOL` → one approval-gated
//! command → `SPIKE_COMPLETE`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// Top-level scenario document.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Scenario {
    /// Scenario format version; currently `1`.
    #[serde(default)]
    pub version: u32,
    /// Optional free-form name, recorded in the debug event log.
    #[serde(default)]
    pub name: Option<String>,
    /// Turn scripts. Prompts are matched with [`TurnSpec::match_against`];
    /// unmatched prompts take the next unconsumed catch-all turn, and a prompt
    /// matching nothing at all gets the built-in echo turn.
    #[serde(default)]
    pub turns: Vec<TurnSpec>,
    /// Exit automatically after this many submitted user turns (steer/queue
    /// deliveries count). When unset the binary stays up until `/exit`,
    /// `/quit`, EOF, or a kill signal.
    #[serde(default)]
    pub quit_after_turns: Option<u32>,
}

/// One scripted turn: user prompt → optional thinking → streamed text → zero or
/// more tool calls → end of turn.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct TurnSpec {
    /// Prompt that selects this turn. `match` is exact; `match_prefix` matches
    /// a prefix. Neither is set → catch-all, consumed once in declaration order.
    #[serde(default)]
    pub r#match: Option<String>,
    /// Prefix-match variant of [`Self::match`].
    #[serde(default)]
    pub match_prefix: Option<String>,
    /// Thinking text, streamed in [`Self::think_chunks`] chunks.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Number of thinking chunks (default 1 when `thinking` is present).
    #[serde(default)]
    pub think_chunks: Option<usize>,
    /// Assistant reply text, streamed in [`Self::chunks`] chunks.
    #[serde(default)]
    pub text: Option<String>,
    /// Number of text chunks (default 1).
    #[serde(default)]
    pub chunks: Option<usize>,
    /// Delay between streamed chunks, in milliseconds. The clock advances by
    /// the same amount; default 8 ms.
    #[serde(default)]
    pub chunk_delay_ms: Option<u64>,
    /// Tool calls played in order, each separated by an approval boundary.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Final stop reason recorded for the turn (`end_turn` by default;
    /// `cancelled` is normally produced by Esc / Ctrl+C, not by the script).
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Extra usage numbers; defaults are small deterministic counters.
    #[serde(default)]
    pub usage: Option<UsageSpec>,
    /// Scripted spinner status line(s), painted verbatim in the working
    /// region instead of the dialect's stock line. Frames advance on the
    /// fake's one-second working repaint and then hold; they reproduce real
    /// 2.1.272 status lines (`· Razzmatazzing… (49m 38s · ↓ 66.0k tokens ·
    /// thinking some more with xhigh effort)`) for screen-tier tests.
    #[serde(default)]
    pub spinner: Option<SpinnerSpec>,
}

/// Scripted spinner status line sequence.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SpinnerSpec {
    /// Exact spinner rows, one per elapsed-second repaint; the last frame is
    /// held for the rest of the turn. At least one is required when present.
    #[serde(default)]
    pub frames: Vec<String>,
}

impl TurnSpec {
    /// True when `prompt` selects this turn spec.
    #[must_use]
    pub fn matches(&self, prompt: &str) -> bool {
        if let Some(exact) = &self.r#match {
            return exact == prompt;
        }
        if let Some(prefix) = &self.match_prefix {
            return prompt.starts_with(prefix);
        }
        false
    }

    /// Whether this turn may be consumed once by any unmatched prompt.
    #[must_use]
    pub fn is_catch_all(&self) -> bool {
        self.r#match.is_none() && self.match_prefix.is_none()
    }

    /// Thinking chunk count, defaulting to one chunk.
    #[must_use]
    pub fn think_chunk_count(&self) -> usize {
        self.think_chunks.unwrap_or(1).max(1)
    }

    /// Text chunk count, defaulting to one chunk.
    #[must_use]
    pub fn text_chunk_count(&self) -> usize {
        self.chunks.unwrap_or(1).max(1)
    }

    /// Inter-chunk delay in milliseconds.
    #[must_use]
    pub fn delay_ms(&self) -> u64 {
        self.chunk_delay_ms.unwrap_or(8)
    }
}

/// One scripted tool call.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ToolSpec {
    /// Tool name as the model calls it (`Bash`, `exec_command`,
    /// `run_terminal_command`, …). Codex normalizes this to `Bash` in its
    /// PermissionRequest stdin, like the real 0.154 client.
    pub name: String,
    /// Tool input. Strings are accepted for convenience and wrapped as
    /// `{"command": "<string>"}`; objects are passed through verbatim.
    #[serde(default)]
    pub input: Value,
    /// How approval is obtained:
    /// - `"auto"` (default): run without a dialog;
    /// - `"ask"`: show the native approval dialog and wait for keys;
    /// - `"hook"`: block on the PermissionRequest hook's decision and fall
    ///   back to the dialog when no hook answers;
    /// - `"hook_required"`: same, but a missing hook is treated as a deny.
    #[serde(default)]
    pub approval: Option<String>,
    /// Simulated tool runtime in milliseconds; Esc / Ctrl+C are honored while
    /// it elapses (the tool itself is never executed).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Tool result text/content. Defaults to empty success output.
    #[serde(default)]
    pub result: Option<Value>,
    /// Process exit code for shell-like tools (default 0).
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Mark the result an error (default false).
    #[serde(default)]
    pub is_error: Option<bool>,
    /// Script this call as a backgrounded Agent launch: the tool result and
    /// PostToolUse response return immediately with
    /// `{isAsync:true,status:"async_launched",agentId}`, and a
    /// `<task-notification>` for the same tool call is enqueued and delivered
    /// after the turn ends (with a `SubagentStop` hook), carrying
    /// `async_status` / `async_result`. Reproduces the c-tasktrack lifecycle.
    #[serde(default)]
    pub async_agent: Option<AsyncAgent>,
}

/// A backgrounded Agent launch and its later completion.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AsyncAgent {
    /// Harness agentId surfaced in the launch result. Defaults to a fixed id.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Completion `<status>`: `completed` (default), `killed`, `failed`.
    #[serde(default)]
    pub status: Option<String>,
    /// Completion `<result>` body.
    #[serde(default)]
    pub result: Option<String>,
    /// Completion `<summary>` line.
    #[serde(default)]
    pub summary: Option<String>,
}

impl ToolSpec {
    /// Input object seen by hooks and written into artifacts.
    #[must_use]
    pub fn input_object(&self) -> Value {
        match &self.input {
            Value::String(command) => serde_json::json!({ "command": command }),
            Value::Null => serde_json::json!({}),
            other => other.clone(),
        }
    }

    /// Approval mode for this call.
    #[must_use]
    pub fn approval_mode(&self) -> ApprovalMode {
        match self.approval.as_deref() {
            Some("ask") => ApprovalMode::Ask,
            Some("hook") => ApprovalMode::HookThenAsk,
            Some("hook_required") | Some("hook-required") => ApprovalMode::HookRequired,
            _ => ApprovalMode::Auto,
        }
    }

    /// Simulated runtime.
    #[must_use]
    pub fn duration(&self) -> u64 {
        self.duration_ms.unwrap_or(12)
    }

    /// Result payload written into artifacts.
    #[must_use]
    pub fn result_value(&self) -> Value {
        self.result
            .clone()
            .unwrap_or_else(|| Value::String(String::new()))
    }

    /// Exit code for shell-like tools.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.exit_code.unwrap_or(0)
    }

    /// Whether the tool result is an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.is_error.unwrap_or(false)
    }
}

/// How a scripted tool call obtains approval.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApprovalMode {
    /// No approval needed.
    #[default]
    Auto,
    /// Native screen dialog, answered by keys.
    Ask,
    /// Blocking hook decision, falling back to the dialog.
    HookThenAsk,
    /// Blocking hook decision required; silence denies.
    HookRequired,
}

/// Token usage numbers attached to one turn.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct UsageSpec {
    /// Prompt tokens.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Completion tokens.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Reasoning tokens.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    /// Cached read tokens.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

impl Scenario {
    /// Load a scenario. `.yaml` / `.yml` parse as YAML, anything else as JSON.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        Self::parse(
            &text,
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("json"),
        )
    }

    /// Parse scenario text in the named format (`json`, `yaml`, `yml`).
    pub fn parse(text: &str, format: &str) -> Result<Self, String> {
        let mut scenario: Scenario = if matches!(format, "yaml" | "yml") {
            serde_yaml::from_str(text).map_err(|err| format!("scenario yaml: {err}"))?
        } else {
            serde_json::from_str(text).map_err(|err| format!("scenario json: {err}"))?
        };
        if scenario.version != 0 && scenario.version != 1 {
            return Err(format!("unsupported scenario version {}", scenario.version));
        }
        scenario.version = 1;
        for (idx, turn) in scenario.turns.iter().enumerate() {
            if turn.tools.iter().any(|tool| tool.name.is_empty()) {
                return Err(format!("turn {idx}: tool name is empty"));
            }
            if turn
                .spinner
                .as_ref()
                .is_some_and(|spinner| spinner.frames.is_empty())
            {
                return Err(format!("turn {idx}: spinner requires at least one frame"));
            }
        }
        Ok(scenario)
    }

    /// Select the turn for a submitted prompt: an explicit `match` wins, then
    /// the next unconsumed catch-all. `consumed` tracks catch-all indices.
    #[must_use]
    pub fn select_turn(&self, prompt: &str, consumed: &BTreeMap<usize, ()>) -> Option<usize> {
        let mut first_catch_all = None;
        for (idx, turn) in self.turns.iter().enumerate() {
            if turn.matches(prompt) {
                return Some(idx);
            }
            if first_catch_all.is_none() && turn.is_catch_all() && !consumed.contains_key(&idx) {
                first_catch_all = Some(idx);
            }
        }
        first_catch_all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_scenario_with_defaults() {
        let scenario = Scenario::parse(
            r#"{
                "turns": [
                  {"match": "RUN_TOOL", "text": "SPIKE_COMPLETE", "chunks": 2,
                   "tools": [{"name": "Bash", "input": "echo hi", "approval": "ask"}]}
                ]
            }"#,
            "json",
        )
        .expect("parse");
        assert_eq!(scenario.turns.len(), 1);
        let turn = &scenario.turns[0];
        assert_eq!(turn.text.as_deref(), Some("SPIKE_COMPLETE"));
        assert_eq!(turn.text_chunk_count(), 2);
        assert_eq!(turn.delay_ms(), 8);
        let tool = &turn.tools[0];
        assert_eq!(tool.name, "Bash");
        assert_eq!(tool.input_object()["command"], "echo hi");
        assert_eq!(tool.approval_mode(), ApprovalMode::Ask);
    }

    #[test]
    fn yaml_scenario_parses() {
        let scenario = Scenario::parse(
            "turns:\n  - match_prefix: SLOW\n    thinking: hmm\n    text: done\n",
            "yaml",
        )
        .expect("yaml");
        assert_eq!(scenario.turns.len(), 1);
        assert_eq!(scenario.turns[0].thinking.as_deref(), Some("hmm"));
        assert!(scenario.turns[0].matches("SLOW_QUEUE"));
    }

    #[test]
    fn selection_prefers_explicit_match_then_catch_all() {
        let scenario = Scenario::parse(
            r#"{"turns":[
                {"match":"A","text":"a"},
                {"text":"default"},
                {"match":"A","text":"a2"}
            ]}"#,
            "json",
        )
        .unwrap();
        assert_eq!(scenario.select_turn("A", &BTreeMap::new()), Some(0));
        let mut consumed = BTreeMap::new();
        consumed.insert(1, ());
        assert_eq!(scenario.select_turn("other", &consumed), None);
        assert_eq!(scenario.select_turn("other", &BTreeMap::new()), Some(1));
    }

    #[test]
    fn spinner_frames_are_optional_and_must_be_nonempty_when_present() {
        let ok = Scenario::parse(
            r#"{"turns":[{"text":"done","spinner":{"frames":[
                "· Forging… (3s · thinking with xhigh effort)",
                "· Forging… (4s · ↓ 25 tokens · thinking with xhigh effort)"]}}]}"#,
            "json",
        )
        .expect("parse");
        assert_eq!(ok.turns[0].spinner.as_ref().unwrap().frames.len(), 2);
        let err = Scenario::parse(
            r#"{"turns":[{"text":"x","spinner":{"frames":[]}}]}"#,
            "json",
        )
        .unwrap_err();
        assert!(err.contains("spinner"), "{err}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Scenario::parse(r#"{"turns":[],"bogus":1}"#, "json").unwrap_err();
        assert!(err.contains("bogus"), "{err}");
    }
}
