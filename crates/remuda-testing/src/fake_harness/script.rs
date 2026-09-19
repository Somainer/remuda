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
    /// Fire a non-blocking `Notification` hook carrying Claude Code's idle
    /// prompt (`notification_type: "idle_prompt"`) immediately *after* the
    /// turn's `Stop`. Reproduces the ordering where the idle advisory arrives
    /// once the turn has already ended; it must never raise a phase or a wait.
    #[serde(default)]
    pub idle_notification: bool,
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
    /// Selected option labels for a `ask_user_question` call, one per scripted
    /// question. The completed grok frame echoes them in
    /// `rawOutput.UserAnswered`.
    #[serde(default)]
    pub answer: Option<Vec<String>>,
    /// Scripted file change for a write-style tool (`write` / `search_replace`):
    /// the progress and completed grok frames carry a `{type:"diff"}` content
    /// block. Absent for every other tool.
    #[serde(default)]
    pub diff: Option<DiffSpec>,
}

/// A scripted `{type:"diff"}` content block (grok write tools).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct DiffSpec {
    /// Changed path. Defaults to `input.file_path`, the field the real client
    /// puts in `locations[].path`.
    #[serde(default)]
    pub path: Option<String>,
    /// Replaced text (empty for a create).
    #[serde(default)]
    pub old_text: Option<String>,
    /// Replacement text. Defaults to `input.content`.
    #[serde(default)]
    pub new_text: Option<String>,
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

    /// Stable grok tool name for a scripted call.
    ///
    /// The grok dialect writes `_meta["x.ai/tool"]{name,kind}` from *this*
    /// value rather than the scripted `name`, because a grok scenario names the
    /// tool the way the model calls it (`run_terminal_command`, `read_file`, …)
    /// — the real client's namespace, not the Claude `Bash` the other dialects
    /// normalize to. A scenario that still spells a Claude name (the shipped
    /// `ok`/`slow` fixtures) gets the grok equivalent, so the same scenario
    /// stays runnable under all three dialects.
    #[must_use]
    pub fn grok_name(&self) -> String {
        grok_tool_name(&self.name).to_owned()
    }

    /// Frame `kind` for the scripted tool, from the same name table task
    /// `c-grok-toolid` translates by (design §3.1): measured kinds are
    /// `execute` / `write` / `edit` / `ask_user` / `other`.
    ///
    /// This is the **`_meta["x.ai/tool"].kind`** value, not the frame's
    /// top-level `kind` — the two differ (see [`Self::grok_acp_kind`]).
    #[must_use]
    pub fn grok_kind(&self) -> &'static str {
        grok_tool_kind(&self.grok_name())
    }

    /// The top-level `kind` on a progress frame: the ACP [`ToolKind`], which is
    /// a *different* vocabulary from `x.ai/tool.kind`.
    ///
    /// Measured values: `execute` for a shell call, `edit` for a write, `other`
    /// for `ask_user_question` (1.0.30 fixture frame 37; 1.0.34 ACP capture).
    /// Everything else falls back to `other`, the ACP default for an
    /// unclassified tool — deliberately *not* guessed into a more specific
    /// kind, because the 1.0.30 fixture never exercised those tools.
    ///
    /// [`ToolKind`]: https://agentclientprotocol.com
    #[must_use]
    pub fn grok_acp_kind(&self) -> &'static str {
        match self.grok_name().as_str() {
            "run_terminal_command" => "execute",
            "write" | "search_replace" => "edit",
            _ => "other",
        }
    }

    /// `x.ai/tool.label` — the short human label the real client ships.
    #[must_use]
    pub fn grok_label(&self) -> &'static str {
        grok_tool_label(&self.grok_name())
    }

    /// `x.ai/tool.namespace`. The captures split two ways: the build's own
    /// tools report `grok_build`, the file tools report `opencode`
    /// (`fixtures/grok/grok-acp-session.jsonl`).
    #[must_use]
    pub fn grok_namespace(&self) -> &'static str {
        match self.grok_name().as_str() {
            "write" | "search_replace" | "read_file" | "list_dir" => "opencode",
            _ => "grok_build",
        }
    }

    /// Whether the real client marks the tool read-only.
    #[must_use]
    pub fn grok_read_only(&self) -> bool {
        matches!(
            self.grok_name().as_str(),
            "read_file" | "list_dir" | "grep" | "web_search" | "web_fetch" | "ask_user_question"
        )
    }

    /// Tool `variant` the progress frame normalizes `rawInput` into. Captured
    /// values are `Bash` (shell) and `AskUserQuestion`; `Write` and
    /// `SearchReplace` follow `rawOutput.type` in the captured write frames
    /// ([U] for the search/replace spelling — the 1.0.30 fixture has no write).
    #[must_use]
    pub fn grok_variant(&self) -> &'static str {
        grok_tool_variant(&self.grok_name())
    }

    /// Human display title for the progress frame, mirroring the captured
    /// `Execute \`printf …\`` / `Write \`/tmp/…\`` / `Ask: …` shapes.
    #[must_use]
    pub fn grok_title(&self) -> String {
        let input = self.input_object();
        let field = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
        match self.grok_name().as_str() {
            "run_terminal_command" => format!("Execute `{}`", field("command")),
            "read_file" | "write" | "search_replace" => {
                format!("{} `{}`", capitalise(self.grok_label()), field("file_path"))
            }
            "list_dir" => format!("List `{}`", field("target_directory")),
            "ask_user_question" => format!("Ask: {}", field("question")),
            _ => self.grok_label().to_owned(),
        }
    }
}

/// Claude tool name → grok native name. Only the names the shipped scenarios
/// use need an entry; anything else passes through unchanged so a grok-only
/// scenario keeps its own spelling.
fn grok_tool_name(name: &str) -> &str {
    match name {
        "Bash" => "run_terminal_command",
        "Read" => "read_file",
        "Write" => "write",
        "Edit" => "search_replace",
        "Glob" => "list_dir",
        "Grep" => "grep",
        "Task" | "Agent" => "spawn_subagent",
        other => other,
    }
}

/// Name → `kind`, the frame field the category fallback reads. Mirrors the
/// `kind` column of the design doc §3.1 table.
fn grok_tool_kind(name: &str) -> &'static str {
    match name {
        "run_terminal_command" => "execute",
        "write" | "search_replace" => "write",
        "ask_user_question" => "ask_user",
        _ => "other",
    }
}

/// Name → `label`, as the real client stamps it.
fn grok_tool_label(name: &str) -> &'static str {
    match name {
        "run_terminal_command" => "Run Command",
        "read_file" => "Read File",
        "write" => "Write",
        "search_replace" => "Search Replace",
        "list_dir" => "List Directory",
        "grep" => "Search",
        "ask_user_question" => "Ask User",
        "spawn_subagent" => "Spawn Subagent",
        "workflow" => "Workflow",
        _ => "Tool",
    }
}

/// Name → `rawInput.variant` on the progress frame.
fn grok_tool_variant(name: &str) -> &'static str {
    match name {
        "run_terminal_command" => "Bash",
        "read_file" => "Read",
        "write" => "Write",
        "search_replace" => "SearchReplace",
        "list_dir" => "ListDir",
        "ask_user_question" => "AskUserQuestion",
        _ => "Tool",
    }
}

fn capitalise(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
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

    fn tool(name: &str, input: Value) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            input,
            ..ToolSpec::default()
        }
    }

    #[test]
    fn grok_identity_follows_the_name_table_not_the_scripted_dialect_name() {
        // A claude-named scenario still yields grok's stable frame identity.
        let bash = tool("Bash", serde_json::json!({ "command": "printf hi" }));
        assert_eq!(bash.grok_name(), "run_terminal_command");
        assert_eq!(bash.grok_kind(), "execute");
        assert_eq!(bash.grok_label(), "Run Command");
        assert_eq!(bash.grok_variant(), "Bash");
        assert!(!bash.grok_read_only());
        assert_eq!(
            bash.grok_title(),
            "Execute `printf hi`",
            "title is the display sentence, not the name"
        );
    }

    #[test]
    fn grok_names_pass_through_and_carry_their_own_kind() {
        let ask = tool(
            "ask_user_question",
            serde_json::json!({ "question": "Pick one." }),
        );
        assert_eq!(ask.grok_name(), "ask_user_question");
        assert_eq!(ask.grok_kind(), "ask_user");
        assert_eq!(ask.grok_variant(), "AskUserQuestion");
        assert!(ask.grok_read_only());
        assert_eq!(ask.grok_title(), "Ask: Pick one.");

        let write = tool(
            "write",
            serde_json::json!({ "file_path": "/tmp/out.txt", "content": "OK" }),
        );
        assert_eq!(write.grok_kind(), "write");
        assert_eq!(write.grok_title(), "Write `/tmp/out.txt`");
    }
}
