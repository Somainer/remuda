---
name: remuda
description: "Control Remuda instances, worktrees, and fleets from a Claude Code coordinator. Use when dispatching coding agents through remuda CLI or MCP (create/worktree/send/wait/read/keys/fleet), not for ordinary local shell work."
---

# Remuda

Remuda hosts native coding agents (claude, codex, grok, agy, …) on Hub-connected Nodes. This skill is the coordinator surface: create an isolated git worktree, start an instance, send a task brief, wait, read `DONE <sha>`, then stop.

Prefer the `remuda` CLI when this session can run shell. Prefer MCP tools (`remuda_instance_*`, `remuda_worktree_create`, `remuda_fleet_send`) when the session is attached with `--mcp-config docs/design/remuda-mcp.json`.

The installed binary is the authority for flags. Start with:

```bash
remuda instance
remuda worktree
remuda fleet
```

Do not run bare `remuda` for discovery; it expects a subcommand.

## Worktree, then instance

Default topology is one git worktree per agent and one instance in that worktree. Do not share the coordinator checkout.

```bash
remuda worktree create reviewer --base main --path ../remuda-wt/reviewer
# git worktree add -b wt/reviewer/work ../remuda-wt/reviewer main
# records path in <git-common-dir>/remuda-worktrees.json

remuda instance create \
  --name reviewer \
  --kind codex \
  --driver generic-pty \
  --worktree reviewer \
  --prompt-file /tmp/remuda-brief-reviewer.md
```

`--worktree <name>` calls `git worktree add -b wt/<name>/…` when the name is new and stores the absolute path as the instance workspace `cwd`. `pty` is an alias for `generic-pty` (tty-attach / live-attach; see driver capabilities). Print drivers (`claude-print`) do not attach a PTY: `keys` and `read --source screen` still hit Hub, but screen data only exists on a tty-attach driver.

Create responses include `instanceId` (`ins_…`). Later commands accept that id or `--name`.

## Dispatch, wait, read

Write a task brief file (not a one-line prompt) when the work is more than a sentence. The brief must tell the worker:

- work only in its worktree
- `export CARGO_TARGET_DIR=$PWD/target` when building this repo (shared checkout `target/`; see `docs/research/tasks/_impl-rules.md`)
- commit on `wt/<name>/…`
- finish with `git rebase main`, then `cargo check --workspace --all-targets --locked` and relevant tests
- reply with a single line `DONE <sha>`
- never push and never touch `main`

```bash
remuda instance send reviewer --file /tmp/remuda-brief-reviewer.md
remuda instance wait reviewer --until idle --timeout 120000
remuda instance wait reviewer --until 'line:DONE ' --timeout 120000
remuda instance read reviewer --lines 120 --source journal
```

`--until` values: `idle` (ready for input; not `requested`), `done` (run terminal / closed), `blocked` (approval or question), `line:<regex>` (journal text). `--timeout` is milliseconds (default 30000, max 300000). `read --source screen` uses tty/raw_tty observations and falls back to the journal when none exist.

Inspect before answering a blocked agent:

```bash
remuda instance list
remuda instance read reviewer --lines 80 --source screen
remuda instance keys reviewer esc
```

`keys` validates every name (`enter`, `esc`, `ctrl+c`, arrows, or a single character) before Hub `tty.write`.

## Fleet and teardown

```bash
remuda fleet send --all "PAUSE git commits ~5 minutes for coordinator history rewrite"
remuda fleet send --labels region=sg --file /tmp/resume.md
remuda instance stop reviewer --scope run
remuda instance rm reviewer
```

`--all` and `--labels` are mutually exclusive. Do not `rm` instances you did not create unless the user asked.

## MCP equivalents

| CLI | MCP tool |
| --- | --- |
| `worktree create` | `remuda_worktree_create` |
| `instance create` | `remuda_instance_create` |
| `instance list` | `remuda_instance_list` |
| `instance send` / `--file` | `remuda_instance_send` (`text` or `file`) |
| `instance wait --until` | `remuda_instance_wait` (`until`, `timeoutMs`) |
| `instance read --lines --source` | `remuda_instance_read` |
| `instance keys` | `remuda_instance_keys` |
| `instance stop` / `rm` | `remuda_instance_stop` / `remuda_instance_rm` |
| `fleet send --all\|--labels` | `remuda_fleet_send` |

Claude Code names tools `mcp__remuda__<tool>`. Config: `docs/design/remuda-mcp.json`
(committed file has no Hub URL or token). `remuda mcp` resolves, in order:
`--hub` / `REMUDA_HUB` / `$REMUDA_DATA_DIR/dev-hub/listen` / `./data/dev-hub/listen`
/ `http://127.0.0.1:18080` when a `bootstrap-token` or `access-code` file exists
(else `:8080`). Token: `REMUDA_TOKEN`, else `REMUDA_BOOTSTRAP_TOKEN` or the
dev access-code / `bootstrap-token` file. After `remuda dev`, the committed
config works without editing.

```sh
claude --mcp-config docs/design/remuda-mcp.json --strict-mcp-config
```
