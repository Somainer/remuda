# `--mcp-config` for Remuda

Pass this file to Claude Code so the coordinator can call Remuda tools
(`remuda_instance_*`, `remuda_worktree_create`, `remuda_fleet_send`,
`remuda_fleet_keys`) without a
Hub root secret. The MCP process is `remuda mcp`. Do **not** put a Hub URL or
device token in the committed JSON: `remuda mcp` resolves them at runtime.

Config: [`remuda-mcp.json`](./remuda-mcp.json).

```sh
claude --mcp-config docs/design/remuda-mcp.json --strict-mcp-config
```

## Runtime resolution

`remuda mcp` (and `instance` / `fleet`) resolve Hub URL and credentials in this
order. Empty values and placeholders that start with `<` are ignored.

**URL**

1. `--hub`
2. `REMUDA_HUB`
3. `$REMUDA_DATA_DIR/listen` or `$REMUDA_DATA_DIR/dev-hub/listen` (written by Hub
   after bind; `remuda dev` uses the `dev-hub` subdirectory)
4. `./data/dev-hub/listen` then `./data/listen`
5. `http://127.0.0.1:18080` when a `bootstrap-token` or `access-code` file exists
   in those directories (`remuda dev`)
6. `http://127.0.0.1:8080` otherwise (`remuda hub` default)

**Device token**

1. `--token` / `REMUDA_TOKEN`
2. else mint from bootstrap / access code:

   1. `--bootstrap-token` / `REMUDA_BOOTSTRAP_TOKEN`
   2. `bootstrap-token` or `access-code` in the data dirs above

`remuda dev` persists `listen` and `bootstrap-token` under
`$REMUDA_DATA_DIR/dev-hub` (default `./data/dev-hub`). After `remuda dev` is
running from the repo root, the committed mcp-config works with no edits.

Optional env (not committed): `REMUDA_DATA_DIR`, `REMUDA_HUB`, `REMUDA_TOKEN`.

Headless print:

```sh
claude -p "$PROMPT" \
  --model haiku \
  --max-budget-usd 0.3 \
  --output-format stream-json \
  --mcp-config docs/design/remuda-mcp.json \
  --strict-mcp-config \
  --permission-mode dontAsk \
  --allowedTools mcp__remuda__remuda_worktree_create,mcp__remuda__remuda_instance_create,mcp__remuda__remuda_instance_wait,mcp__remuda__remuda_instance_read,mcp__remuda__remuda_instance_send,mcp__remuda__remuda_instance_keys,mcp__remuda__remuda_instance_list,mcp__remuda__remuda_instance_stop,mcp__remuda__remuda_instance_rm,mcp__remuda__remuda_fleet_send,mcp__remuda__remuda_fleet_keys
```

Replace `command` in the JSON with an absolute `remuda` binary when `PATH` is
empty (Claude `--print` often is). Do not put the bootstrap token in the prompt.
Coordinator workflow: `skills/remuda/SKILL.md`. Merging agent branches with
gates: [`coordinator-guide.md`](./coordinator-guide.md).
