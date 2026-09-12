# `--mcp-config` for Remuda

Pass this file to Claude Code so the coordinator can call Remuda tools
(`remuda_instance_*`, `remuda_worktree_create`, `remuda_fleet_send`) without a
Hub root secret. The MCP process is `remuda mcp`; it logs in with
`REMUDA_TOKEN` or mints a device token from `REMUDA_BOOTSTRAP_TOKEN`.

Config: [`remuda-mcp.json`](./remuda-mcp.json).

```sh
claude --mcp-config docs/design/remuda-mcp.json --strict-mcp-config
```

Headless print:

```sh
claude -p "$PROMPT" \
  --model haiku \
  --max-budget-usd 0.3 \
  --output-format stream-json \
  --mcp-config docs/design/remuda-mcp.json \
  --strict-mcp-config \
  --permission-mode dontAsk \
  --allowedTools mcp__remuda__remuda_worktree_create,mcp__remuda__remuda_instance_create,mcp__remuda__remuda_instance_wait,mcp__remuda__remuda_instance_read,mcp__remuda__remuda_instance_send,mcp__remuda__remuda_instance_keys,mcp__remuda__remuda_instance_list,mcp__remuda__remuda_instance_stop,mcp__remuda__remuda_instance_rm,mcp__remuda__remuda_fleet_send
```

Replace `command` in the JSON with an absolute `remuda` binary when `PATH` is
empty (Claude `--print` often is). Do not put the bootstrap token in the prompt.
Coordinator workflow: `skills/remuda/SKILL.md`.
