# Journal test fixtures

| File | Source | Notes |
| --- | --- | --- |
| `claude-askuser-snippet.jsonl` | `docs/research/cli-help/claude-askuser.jsonl` | Short `control_request` / `user` / `thinking_tokens` lines; `_dir` stripped. |
| `claude-transcript-ok.jsonl` | `~/.claude/projects/-private-tmp-hh-probe-ctl/11111111-1111-4111-8111-111111111111.jsonl` | Desensitized excerpt (ai-title, queue-operation, user, assistant OK). Synthetic Bash tool_use/tool_result and `compact_boundary` appended for pairing tests. |
| `workflow-journal.jsonl` | `~/.claude/projects/-private-tmp-hh-probe-int/efa36071-18af-490c-acb5-92700a06d969/subagents/workflows/wf_d20c2ed9-2cd/journal.jsonl` | `launched` / `started` / `result`. |
| `agent-a199316cfcd1e1850.meta.json` | same workflow directory | Model id only; no secrets. |
| `interaction.json` | `crates/remuda-protocol/tests/fixtures/interaction.json` | Wire-shaped Interaction for first-writer-wins tests. |
| `workflow/runs-221/wf_c3422384-cb1/**` | `~/.claude/projects/-tmp-remuda-r-ux-w-spike-proj/…/subagents/workflows/wf_c3422384-cb1` | Real claude 2.1.221 4-agent two-phase run: journal.jsonl + four agent-*.jsonl/meta. No prompts beyond the public spike script. |
| `workflow/runs-221/wf_93a3b4ed-afb/**` | same project, run-level-failure spike | Real 2.1.221 one-agent run ending in a run-level `failed`. |
| `workflow/scripts/spike-wf-1.js` | same project, `workflows/scripts/` | The spike script copy (name/description/phases + 4 `agent(literal,{label,phase})`). |
| `workflow/scripts/spike-wf-runfail-wf_93a3b4ed-afb.js` | same project | Script copy for the run-failure spike. |
