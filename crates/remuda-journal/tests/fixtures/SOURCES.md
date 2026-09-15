# Journal test fixtures

| File | Source | Notes |
| --- | --- | --- |
| `claude-askuser-snippet.jsonl` | `docs/research/cli-help/claude-askuser.jsonl` | Short `control_request` / `user` / `thinking_tokens` lines; `_dir` stripped. |
| `claude-transcript-ok.jsonl` | `~/.claude/projects/-private-tmp-hh-probe-ctl/11111111-1111-4111-8111-111111111111.jsonl` | Desensitized excerpt (ai-title, queue-operation, user, assistant OK). Synthetic Bash tool_use/tool_result and `compact_boundary` appended for pairing tests. |
| `workflow-journal.jsonl` | `~/.claude/projects/-private-tmp-hh-probe-int/efa36071-18af-490c-acb5-92700a06d969/subagents/workflows/wf_d20c2ed9-2cd/journal.jsonl` | `launched` / `started` / `result`. |
| `agent-a199316cfcd1e1850.meta.json` | same workflow directory | Model id only; no secrets. |
| `interaction.json` | `crates/remuda-protocol/tests/fixtures/interaction.json` | Wire-shaped Interaction for first-writer-wins tests. |
- `claude-transcript-tasktrack.jsonl` — identical sanitized c-tasktrack fixture to `crates/remuda-driver/tests/fixtures/claude-transcript-tasktrack.jsonl` (see its SOURCES entry): three Agent lifecycles (sync foreground, backgrounded launch + task-notification completion, killed). Used by `tests/tasktrack_fold.rs` for the file-transcript mapper.
