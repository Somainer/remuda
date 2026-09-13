# pty-toolcalls-1 — claude-pty tool calls in the 结构 view, live run

Evidence for the user report 「在 claude tty 模式下，toolcall 同步有问题」 —
a `claude-pty` session's tool calls never reached the structured view.

- **When:** 2026-09-13, 17:47–19:05 UTC.
- **Where:** `remuda dev` from this worktree, Hub `127.0.0.1:61680`, Node
  `127.0.0.1:61687`, data dir under a scratch directory (not committed).
- **Agent:** `claude` 2.1.270 in a Herdr pane, driver `claude-pty`, launched
  through the Hub REST surface (`POST /v1/instances`, then
  `POST /v1/instances/:id/commands` with `instance.send`).
- **Prompt:** `use Bash to run ls, then Read the file README.md, then summarize
  the repo in one line` — chosen so the turn contains two real tools plus
  commentary text, which is the shape that was broken.
- **What is shown:** the same prompt run twice against the same Node, once with
  the transcript pump absent (before) and once with it present (after).

Nothing here is a fixture. The mapper-level counterpart is
`crates/remuda-driver/tests/pty_toolcalls.rs` over
`tests/fixtures/claude-transcript-tools.jsonl`.

## What was wrong

`claude-pty` launches the native TUI, which does **not** speak stream-json. Its
`tool_use` / `tool_result` blocks exist in exactly one place: the transcript at
`~/.claude/projects/<encoded cwd>/<session>.jsonl`. The driver's SessionStart
hook already recorded that path — `nativeTranscriptPath` was populated on the
instance, and the Node mirrored it onto the spec — but **nothing ever opened the
file**. Only `shell-pty`'s promotion supervisor (D-025) tailed a transcript;
the `claude-pty` carrier had no equivalent.

So the 结构 tab could only ever show what the Node itself journaled: the prompt
it queued. Every tool the agent ran, and every reply it wrote, was invisible
there while the 终端 tab showed the whole turn.

## Before — 15 events, no tool call, no transcript channel

Instance `tooltty-before`, journal via `GET /v1/instances/:id/journal`:

```
  1 lifecycle  ch=runtime  command
  2 lifecycle  ch=runtime  instance
  3 lifecycle  ch=runtime  instance
  4 lifecycle  ch=runtime  command
  5 lifecycle  ch=herdr    session       unknown
  6 lifecycle  ch=herdr    agent_status  unknown
  7 lifecycle  ch=runtime  instance
  8 lifecycle  ch=hook     SessionStart  recorded
  9 lifecycle  ch=herdr    agent_status  idle
 10 lifecycle  ch=runtime  command
 11 message    ch=runtime  user          queued
 12 message    ch=runtime  user          complete
 13 lifecycle  ch=runtime  command
 14 lifecycle  ch=herdr    agent_status  working
 15 lifecycle  ch=herdr    agent_status  idle
```

Channel census: `runtime` 9, `herdr` 5, `hook` 1. **`transcript` 0.**
Kind census: `lifecycle` 13, `message` 2, **`tool_call` 0, `tool_result` 0.**

Meanwhile the transcript on disk for that very session held 5 tool blocks
(`Bash` + its result, `Read` + its result, plus the commentary and final text).
seq 8 proves the driver knew the file's path the whole time.

The 结构 tab for this instance:
[pty-toolcalls-1-before.png](./pty-toolcalls-1-before.png) — it never gets past
`加载 snapshot…`, because there is no message, thought or tool node to render.
seq 14/15 (`working` → `idle`) are the only sign a turn happened at all.

## After — the same prompt, 26 events, the conversation

Instance `tooltty-evidence`, same Node, same prompt:

```
 11 message     ch=runtime    user      op=open    st=queued   'use Bash to run ls, then Read the file READM'
 12 message     ch=runtime    user      op=replace st=complete 'use Bash to run ls, then Read the file READM'
 13 lifecycle   ch=runtime    command
 14 lifecycle   ch=herdr      agent_status working
 15 message     ch=transcript user      op=open    st=complete 'use Bash to run ls, then Read the file READM'
 16 message     ch=transcript assistant op=open    st=complete "I'll run `ls` and read the README."
 17 message     ch=transcript assistant op=close   st=complete "I'll run `ls` and read the README."
 18 tool_call   ch=transcript Bash  id=…762c1e6fa534 op=open  rev=1 state=proposed input=known
 19 tool_call   ch=transcript Bash  id=…762c1e6fa534 op=close rev=2 state=proposed input=known
 20 tool_result ch=transcript call=…762c1e6fa534     op=open  rev=1 stage=final outcome=succeeded
 21 tool_call   ch=transcript Read  id=…0a11a362b671 op=open  rev=1 state=proposed input=known
 22 tool_call   ch=transcript Read  id=…0a11a362b671 op=close rev=2 state=proposed input=known
 23 tool_result ch=transcript call=…0a11a362b671     op=open  rev=1 stage=final outcome=succeeded
 24 message     ch=transcript assistant op=open    st=complete 'Remuda is a pre-alpha Rust workspace (plus a'
 25 message     ch=transcript assistant op=close   st=complete 'Remuda is a pre-alpha Rust workspace (plus a'
 26 lifecycle   ch=herdr      agent_status idle
```

Channel census: `transcript` 11, `runtime` 9, `herdr` 5, `hook` 1.
Kind census: `lifecycle` 13, `message` 7, `tool_call` 4, `tool_result` 2.

Reading it:

- seq 11/12 vs 15 — the same prompt twice, on purpose and unchanged from
  [terminal-promote-1](./terminal-promote-1.md): `ch=runtime` is the Node's
  record that it queued and dispatched the text, `ch=transcript` is Claude's
  record that it received it. Two different facts.
- seq 18–20 and 21–23 — each tool opens, closes, and gets a result whose
  `toolCallId` is the **same node** as its call, so the web renders one card
  with its output rather than a call stuck without a result.
- `input=known` on close — the card can show `$ ls` and the file path. A
  streaming-input placeholder would render a tool with no arguments.
- seq 16/17 — commentary text and the tool it introduces both survive, which is
  the case (`text` + `tool_use` split across two assistant records sharing one
  `message.id`) that the fixture test pins.

The 结构 tab for this instance:
[pty-toolcalls-1-after.png](./pty-toolcalls-1-after.png) — the prompt, the
commentary, an expanded `Bash` card with its stdout, a `Read` card with the
file, and the final summary.

## The second defect the live run exposed

Surveying real transcripts for `user` content-array shapes found a case the
mapper dropped entirely:

| shape | records |
|---|---|
| `('tool_result',)` | 9638 |
| `('text',)` | 69 |
| `('image', 'text')` | 17 |
| `('text', 'tool_result')` | 1 |

`map_user` only emitted observations for `tool_result` blocks, so the 69+17
records whose array holds `text` — Claude writes interrupt notices that way
(`[Request interrupted by user for tool use]`) — mapped to nothing. That
affects `claude-print` and promoted terminals too, not just `claude-pty`. The
fixture carries one such record and `pty_toolcalls.rs` asserts it survives.

## Tests

- `cargo test -p remuda-driver --test pty_toolcalls` (4) — fixture with
  interleaved `tool_use` / `tool_result`: block order, call↔result linkage,
  known input on close, and the user text block.
- `cargo test --workspace --locked`, `cargo clippy --workspace --all-targets
  --locked -- -D warnings`, `cargo fmt --all`, `pnpm test` (283),
  `./scripts/ci/secret-scan.sh`.
- Screenshot capture is opt-in and writes outside the repo by default:
  `web/tests/e2e/pty-toolcalls.spec.ts` and `pty-toolcalls-before.spec.ts` skip
  unless `PTY_TOOLCALLS_INSTANCE` names a live instance, and they are excluded
  from the default Playwright run. They were driven here with
  `--config=playwright.evidence.config.ts` against the live Hub.

## Not covered here

- The before/after screenshots are from two different instances, because the
  before-state required a build without the pump. Both ran on the same Node,
  same prompt, same model.
- `state: proposed` on a hydrated tool call is the transcript's own evidence
  level: the record proves Claude emitted the call, not that the tool ran. The
  `tool_result` is what proves execution, which is why the card reads its
  outcome from there.
- Sub-agent (`isSidechain`) records are still skipped, unchanged.
- Model routing for this run used a local gateway settings overlay passed by
  path (`settingsOverlayPath`); its contents are never logged or copied here.
