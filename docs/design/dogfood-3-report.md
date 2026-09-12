# D0 dogfood-3 report

Follow-on to [`dogfood-2-report.md`](./dogfood-2-report.md). Same demo:
[`scripts/demo/dogfood-1.sh`](../../scripts/demo/dogfood-1.sh) (haiku `-p`,
`--mcp-config` runtime copy). Hub listen
`REMUDA_DOGFOOD_HUB_LISTEN=127.0.0.1:28080` /
`REMUDA_DOGFOOD_NODE_LISTEN=127.0.0.1:28787` so the coordinator demo keeps
`:18080`. Evidence: [`evidence/dogfood-3.jsonl`](./evidence/dogfood-3.jsonl)
(redacted).

## Gap closed

6. **`wait --until line:(?m)^DONE` vs TUI bullets.** Shared matcher
   (`until_met` / `normalize_wait_line` in `crates/remuda/src/cmd/instance.rs`,
   used by CLI `instance wait` and MCP `remuda_instance_wait`) strips leading
   whitespace and one list marker (`•`, `●`, `◆`, `▸`, `▪`, `-`, `*`, `>`)
   before applying the regex. The raw journal line is unchanged;
   `matchedLine` on the wait JSON is the raw line. Wrapped task-brief lines
   (`for example: DONE`, `further input`) are not matches. Wait JSON no longer
   dumps the full event list (MCP overflowed at ~87k chars).

## What ran

Haiku used Remuda MCP tools (plus ToolSearch).

1. `remuda_worktree_create` → `df3`
2. Parallel `remuda_instance_create` kind=codex/grok `driver=pty` with briefs
3. `remuda_instance_wait` `until=line:(?m)^DONE` timeoutMs=180000
4. Grok first wait timed out; `remuda_instance_send` "Print DONE as its own
   line now." then wait timeoutMs=60000
5. `remuda_instance_read` `source=screen`
6. `remuda_instance_stop` scope=instance

Session: haiku cost ~$0.10, ~258s, 15 turns, exit 0.
Coordinator JSON: `codexDone=true`, `grokDone=true`.

Wait results:

- Codex: `reason=condition-met`, `matchedLine="• DONE"`
- Grok (retry): `reason=condition-met`, raw indented `DONE` line

Worker files in the dogfood worktree:

- `dogfood-codex.txt` → `codex-ok`
- `dogfood-grok.txt` → `grok-ok`

## Herdr checklist

| Herdr action | Remuda equivalent | Dogfood-3 |
|---|---|---|
| tab + `agent start --kind K -- <yolo>` | `instance create --driver pty` + yolo presets | **Works.** |
| `agent prompt` | `instance send` | **Works.** Create-time brief after control ready; grok retry send delivered. |
| `agent wait --until idle\|blocked` | `instance wait --until idle\|done\|blocked\|line:` | **Works.** `line:(?m)^DONE` matches `• DONE` and indented `DONE`. |
| `agent read --lines N` | `instance read --source screen\|journal` | **Works.** `source=screen` pane snapshots. |
| `agent send-keys` | `instance keys` | **Not exercised.** `tty.write` still Hub-side. |
| task brief + `DONE` | wait line + files | **Works.** Files and wait both succeeded. |

## Remaining gaps

3. **`tty.write` / keys** still not dispatched Hub→Node WSS (`_ => ok`).
4. Hub instance lifecycle can stay `requested` while the Node is ready.
5. Committed `docs/design/remuda-mcp.json` still defaults `REMUDA_HUB` to
   `:8080`; the demo injects `HUB_LISTEN` + a device token at runtime.

## Verdict

D0-5 **coordinator wait succeeded**: both pty workers wrote the DONE files
and `remuda_instance_wait` returned `condition-met` (`codexDone=true`,
`grokDone=true`). Grok needed one explicit follow-up send before its pane
showed a `DONE` line.
