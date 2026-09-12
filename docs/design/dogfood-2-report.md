# D0 dogfood-2 report

Follow-on to [`dogfood-1-report.md`](./dogfood-1-report.md). Same demo:
[`scripts/demo/dogfood-1.sh`](../../scripts/demo/dogfood-1.sh) (haiku `-p`,
`--mcp-config` runtime copy, Hub `http://127.0.0.1:18080`). Evidence:
[`evidence/dogfood-2.jsonl`](./evidence/dogfood-2.jsonl) (redacted).

## Gaps closed

1. **Create-time prompt / early `instance send`.** Node now ACKs the create
   command, then the instance worker waits on `Driver::wait_control` until
   generic-pty has a live pane (watch channel kept open even with no
   subscriber). `GenericPtyDriver::send` also waits, then delivers through
   herdr `agent.prompt` (retries `agent_not_ready`). Fake-herdr test:
   `fake_herdr_send_before_start_waits_for_control`.
2. **Screen journal.** generic-pty always pumps `agent.read` into a bounded
   Lifecycle snapshot (`nativeName=screen`, last 80 lines,
   `completeness=screen-derived`, `channel=pty`). `instance read --source
   screen` selects those events; `wait --until line:` matches them (and
   `line-matcher`). Fake-herdr test:
   `fake_herdr_journals_bounded_screen_snapshot`.

## What ran

Haiku used Remuda MCP tools (plus ToolSearch). It also called Bash/Agent
against local tool-result files; that is coordinator noise, not a Remuda
control-plane path.

1. `remuda_worktree_create` → `df1`
2. Parallel `remuda_instance_create` kind=codex/grok `driver=pty` with briefs
3. `remuda_instance_wait` `until=line:(?m)^DONE` timeoutMs=180000
4. `remuda_instance_read` `source=screen` (not journal-fallback)
5. `remuda_instance_stop` scope=instance

Session: haiku cost ~$0.14, ~444s, 13 turns, exit 0.
Coordinator JSON: `codexDone=false`, `grokDone=false` (wait regex).

**Worker files were written** in the dogfood worktree:

- `dogfood-codex.txt` → `codex-ok`
- `dogfood-grok.txt` → `grok-ok`

generic-pty logs: `agent.start dispatched` for both kinds (no
`control unavailable`, no `NATIVE_PROTOCOL_ERROR` on send).

## Herdr checklist

| Herdr action | Remuda equivalent | Dogfood-2 |
|---|---|---|
| tab + `agent start --kind K -- <yolo>` | `instance create --driver pty` + yolo presets | **Works.** Codex/Grok start in the worktree cwd. Create-time brief is delivered after control is ready. |
| `agent prompt` | `instance send` | **Works.** Queued until the pane is live; delivered via `agent.prompt`. |
| `agent wait --until idle\|blocked` | `instance wait --until idle\|done\|blocked\|line:` | **Partial.** Wait is wired. `line:(?m)^DONE` timed out: TUI pane text showed `• DONE`, not a line that starts with `DONE`. |
| `agent read --lines N` | `instance read --source screen\|journal` | **Works.** `source=screen` returned pane snapshots (no journal-fallback). Completeness `screen-derived`. |
| `agent send-keys` | `instance keys` | **Not exercised.** `tty.write` still Hub-side. |
| task brief + `DONE` | wait line + files | **Files yes; wait regex no.** Brief reached both workers. They wrote the DONE files. Screen wait did not treat `• DONE` as `^DONE`. |

## Remaining gaps

3. **`tty.write` / keys** still not dispatched Hub→Node WSS (`_ => ok`).
4. Hub instance lifecycle can stay `requested` while the Node is ready.
5. Committed `docs/design/remuda-mcp.json` still defaults `REMUDA_HUB` to
   `:8080`; the demo injects `:18080` + a device token at runtime.
6. `wait --until line:(?m)^DONE` does not match TUI bullets (`• DONE`).
   Use `line:DONE` or strip list markers if wait must succeed on TUI text.

## Verdict

D0-5 **end-to-end DONE files are real**: both pty workers received the brief
and wrote `dogfood-codex.txt` / `dogfood-grok.txt`. Screen observations are
in the journal. Coordinator wait still timed out on a too-strict `^DONE`
regex against TUI formatting.
