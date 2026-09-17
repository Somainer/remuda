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

## Reaching remote instances

The same tools drive an instance regardless of which enrolled host runs it;
the Hub routes each call Hub → Node → instance over the Node's outbound
link, so there is no per-host transport to configure.

| Tool | Reaches a remote instance as |
| --- | --- |
| `remuda_instance_send` | A new prompt on the named `instanceId` (any host). |
| `remuda_instance_list` / `remuda_instance_read` / `remuda_instance_wait` | Inventory, journal/screen and state for instances in scope. |
| `remuda_instance_respond` / `remuda_instance_keys` / `remuda_instance_stop` / `remuda_instance_rm` | Interactions, logical keys and lifecycle on the named instance. |
| `remuda_fleet_send` / `remuda_fleet_keys` | Label/host-filtered broadcasts (never `all` from Agent origin). |

**Project-scope messaging rule.** An Agent-origin `remuda_instance_send`
needs no per-message human approval when the target instance is inside the
caller's *explicitly narrowed* delegation scope — at least one of the
caller's `projectIds`, `hostIds` or `workspaceIds` is set, and each set
dimension admits the target on its matching attribute. An instance with
every dimension empty (universe scope) keeps the ownership-only rule, so an
ordinary agent a human launched without a delegation box still needs the
one-shot Interaction for an unowned sibling. Host identity is deliberately
irrelevant once the box admits the target: an agent on host A can message an
in-scope instance on host B with no prompt. The send is journaled on both
sides (an `outbound` message on the sender naming target instance/host, an
`inbound` message on the receiver naming the sender). Everything else keeps
the one-shot human Interaction: out-of-scope targets, `remuda_instance_keys`
/ raw `tty.write`, shell-driver targets, and an explicit
`x-remuda-require-approval` header. `fleet --all` stays banned from Agent
origin, and an Agent can never answer an Interaction itself.

Host **file** tools are intentionally absent from MCP: `host files ls/get/
search` are operator-only routes and an Agent token always receives 403.

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
