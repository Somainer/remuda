# D0-5 dogfood-1 report

Live coordinator: `claude -p --model haiku --max-budget-usd 0.5` with
`--mcp-config docs/design/remuda-mcp.json` (runtime copy injects Hub URL +
device token; committed file still launches `remuda mcp`). Evidence:
[`evidence/dogfood-1.jsonl`](./evidence/dogfood-1.jsonl) (redacted).

Script: [`scripts/demo/dogfood-1.sh`](../../scripts/demo/dogfood-1.sh).
Hub: `http://127.0.0.1:18080` via `remuda dev`.

## What ran

Haiku used only Remuda MCP tools (after ToolSearch):

1. `remuda_worktree_create` → `wt/df1/work` at `remuda-wt/df1`
2. `remuda_instance_create` kind=codex driver=pty and kind=grok driver=pty, both `worktree=df1`, with task briefs
3. `remuda_instance_wait` `until=line:(?m)^DONE` timeoutMs=180000 (parallel)
4. `remuda_instance_read` source=screen (fell back to journal)
5. `remuda_instance_stop` scope=instance

Last captured session: haiku cost ~$0.11, ~415s, 11 turns, exit 0.
Both waits **timed out**. Neither worker wrote `dogfood-codex.txt` /
`dogfood-grok.txt`. Coordinator JSON: `codexDone=false`, `grokDone=false`.

## Herdr checklist

| Herdr action | Remuda equivalent | Dogfood-1 |
|---|---|---|
| `herdr tab create --cwd X --label L` + `agent start --kind K -- <yolo>` | `instance create --kind K --driver pty --cwd/--worktree/--name --prompt-file` + yolo presets | **Partial.** MCP create with `driver=pty` lands `generic-pty`. Codex yolo `--dangerously-bypass-approvals-and-sandbox` and grok `--always-approve` launch. Herdr panes start in the worktree cwd. Initial brief does **not** reach the TUI (`control unavailable` on `instance.send`). |
| `agent prompt` | `instance send` | **Gap.** Node journals `NATIVE_PROTOCOL_ERROR` / `driver operation failed: control unavailable` for the create-time prompt. Codex/Grok TUIs stay at an empty composer. |
| `agent wait --until idle\|blocked` | `instance wait --until idle\|done\|blocked\|line:<regex>` | **Wired.** Wait polls Hub journal. `line:(?m)^DONE` no longer matches the brief text (create payload / `prompt_echo`). This run correctly timed out after 180s. |
| `agent read --lines N` | `instance read --lines N --source screen\|journal` | **Partial.** MCP read works; screen is empty so it falls back to journal. Journal has lifecycle + the send error, not TTY frames. |
| `agent send-keys` | `instance keys` | **Not exercised.** `tty.write` is still a Hub-side command; WSS dispatch does not map it onto `GenericPtyDriver::send_keys`. |
| `agent list` | `instance list` | **Not called** by haiku; Hub list shows name/kind/cwd/host. |
| fleet pause/resume | `fleet send --all` | **Not in this acceptance path.** |
| task brief + `DONE ` | `wait --until line:DONE` | **Partial.** Brief is accepted on create. Workers never print `DONE` because they never see the brief. Line-matcher default is `^DONE`. |
| `git worktree add ../remuda-wt/<name> -b wt/…` | `remuda worktree create` / MCP `remuda_worktree_create` | **Worked.** Reuse-on-repeat, catalog in git-common-dir. |
| coordinator merge gate | `remuda merge --gate` | **Out of scope (post-D0).** |
| herdr skill | `skills/remuda/SKILL.md` | Present on main (D0-4). This session used `--strict-mcp-config` + allowed tool list instead of the skill. |

## Glue fixed in this worktree

- `remuda worktree` cwd is honored on the Node (`CreateInstanceRequest.cwd` → `DriverLaunch.workspace_root`). Agents cwd into the git worktree, not the Node default workspace.
- `pty` deserializes as `generic-pty` on the WSS create path.
- `instance.close` is dispatched on the Node WSS runtime (stop/rm).
- Generic-pty no longer pins the Claude binary for grok/codex.
- Grok yolo no longer passes `--name` (grok 1.0.30 rejects it and herdr `agent.start` times out).
- Herdr agent names are unique per instance (`rmd-<kind>-<id>`). Two parallel pty creates used to collide on the host-id suffix.
- After `pane.split`, wait until a shell prompt is visible before `agent.start`.
- Dismiss Codex “trust this directory” with Enter when it appears.
- `instance wait --until line:` ignores `prompt_echo` and create-command payloads so the brief cannot satisfy `DONE`.
- Herdr client lock is not held across `agent.prompt` / `agent.wait` RPCs.

## Remaining gaps

1. **Initial prompt never reaches the PTY.** `generic-pty` `agent.start` succeeds (both kinds idle in herdr). Hub then records send as `control unavailable`. Likely race: Hub HTTP create returns (and/or WSS create RPC times out ~8s) while Node `driver.start()` is still in `agent.start`; the create-time `Send` hits a driver whose live pane is not yet installed, or a second factory instance. Symptom in remuda log: `node rpc timeout` on the grok create command.
2. **No TTY/screen journal.** `read --source screen` is journal-fallback. Screen wait therefore cannot see `DONE` until observations include tty or the line-matcher event.
3. **`tty.write` / keys** not dispatched on the Hub→Node WSS path (`HubNodeMethod` has no close-relative for keys; `_ => ok`).
4. **Hub instance lifecycle** stays `requested` even after the Node started the driver; snapshot `activity=idle` is not a herdr idle proof.
5. **Committed `docs/design/remuda-mcp.json`** still defaults `REMUDA_HUB` to `:8080`. The demo copies it and injects `:18080` plus a device token so the token is not in git.

## Verdict

D0-5 **MCP coordinator path is real**: a haiku `-p` session created a worktree, created two pty instances, waited, read, and stopped, with redacted stream-json on disk. D0-5 **end-to-end DONE files are not real yet**: generic-pty starts Codex and Grok in the worktree, but the task brief is not delivered, so workers never print `DONE`.
