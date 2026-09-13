# terminal-promote-1 — Terminal → agent promotion, live run

Evidence for [D-025](../decisions.md) and
[remote-terminal.md § Terminal → agent promotion](../remote-terminal.md#terminal--agent-promotion).

- **When:** 2026-09-13, 12:03–12:10 UTC.
- **Where:** `remuda dev` from this worktree, Hub `127.0.0.1:60380`, Node
  `127.0.0.1:60387`, data dir and workspace under a scratch directory.
- **Agent:** `claude` 2.1.270, started by typing into the terminal instance:
  `claude --settings <home>/.claude/settings.relay.json --model claude-opus-5`.
- **What is shown:** a plain `terminal` instance, the promotion when `claude`
  takes the PTY foreground, a prompt delivered from the composer, the reply
  hydrated from the native transcript as structured messages, and demotion on
  `/exit`.

Commands are the Hub REST surface (`POST /v1/instances`,
`POST /v1/instances/:id/commands`, `GET /v1/instances/:id`,
`GET /v1/instances/:id/journal`). Nothing in the run is a fixture.

## 1 — Terminal instance, before any agent

`POST /v1/instances` with `kind: terminal`, `driver: shell-pty`:

```json
{
  "kind": "terminal",
  "driver": "shell-pty",
  "mode": null,
  "promotedAt": null,
  "activity": "unknown",
  "lifecycle": "running"
}
```

`mode` is absent: nothing has promoted this instance.

## 2–3 — `claude` takes the foreground → promotion

`tty.write` of `claude …\r` at **12:03:32Z**. The instance detail at
**12:03:35Z**:

```json
{
  "kind": "claude",
  "driver": "shell-pty",
  "mode": "promoted",
  "promotedAt": "2026-09-13T12:03:33.180Z",
  "activity": "idle",
  "lifecycle": "running"
}
```

`promotedAt` is ~1.1 s after the write, inside the ~2 s budget. `kind` moved to
`claude`; `driver` is still `shell-pty`, which is the point of the design — no
second driver, no relaunch, the same PTY and the same TTY bridge.

`activity: idle` comes from the screen heuristics (`agent_status`), so the
composer knows the TUI is ready rather than still booting.

## 4–5 — composer prompt and hydrated transcript

`instance.send` with `Reply with exactly the single word: promoted`. The prompt
is typed into the PTY as the composer body, then Enter as a **separate** write
(see *What the live run corrected* below). Claude answered `promoted`.

## 6–7 — `/exit` → demotion

`/exit` then Enter at **12:09:41Z**; the instance detail at **12:09:45Z**:

```json
{
  "kind": "terminal",
  "driver": "shell-pty",
  "mode": "native",
  "promotedAt": null,
  "activity": "idle",
  "lifecycle": "running"
}
```

`lifecycle` is still `running`: demotion is not a close. The terminal tab keeps
working, and the shell is back.

## Full journal

Every promotion fact is journaled, so Hub, web and MCP see the same thing:

```
  4 native  agent_detected   agent detected: claude     kind=claude
  5 native  agent_promoted   claude                     kind=claude mode=promoted promotedAt=2026-09-13T12:03:33.180Z
  6 native  agent_status     idle
  8 message user      ch=runtime    open    ''
  9 message user      ch=runtime    replace ''
 12 message user      ch=runtime    open    ''
 13 message user      ch=runtime    replace ''
 16 message user      ch=runtime    open    'Reply with exactly the single word: promoted'
 17 message user      ch=runtime    replace 'Reply with exactly the single word: promoted'
 19 message user      ch=transcript open    'Reply with exactly the single word: promoted'
 20 message assistant ch=transcript open    'promoted'
 21 message assistant ch=transcript close   'promoted'
 22 native  agent_demoted    terminal                   kind=terminal mode=native previousKind=claude
 23 native  agent_detected   agent detected: none
```

Reading it:

- seq 4/5 — the detection diagnostic and the promotion. Only `agent_promoted`
  moves the entity; `agent_detected` is there so a human reading the journal can
  see *why*.
- seq 19–21 — **`ch=transcript`**: these came from the native
  `~/.claude/projects/<encoded cwd>/<session>.jsonl`, mapped by the same mapper
  `claude-print` uses, with the usual `open` → `close` mutations. This is what
  makes the 结构 tab a real conversation and not a screen scrape.
- seq 16/17 vs 19 — the same prompt appears twice on purpose: `ch=runtime` is
  the Node's own record that a prompt was queued and dispatched; `ch=transcript`
  is Claude's record that it actually received it. Two different facts.
- seq 8/9 and 12/13 are empty because those two sends were made with a
  malformed request body during the run (top-level `prompt` instead of
  `payload.prompt`) — an operator error while driving REST by hand, not a
  product defect. seq 16 onward is the correct shape.
- seq 22/23 — demotion, with `previousKind` recorded.

## What the live run corrected

Four things only a real TUI would have shown; each is now covered by a unit test:

1. **cwd canonicalization.** Claude names its project directory after the path
   *it* resolved. `/tmp/x` becomes `/private/tmp/x` on macOS, so the transcript
   lookup was reading an empty directory. `project_dir` now canonicalizes.
2. **ANSI in the raw ring.** Unlike the herdr-carried drivers, `shell-pty` holds
   raw PTY bytes with nothing stripping escapes, so the screen heuristics matched
   nothing. They now strip ANSI before matching.
3. **The composer is repainted, not printed.** A full-screen TUI positions the
   cursor and paints the prompt mid-line, so "a line that starts with `❯`" never
   matched. The check is now for the glyph itself.
4. **Enter must be its own write.** `text\r` in a single write is accepted as
   bytes and silently never submitted; the TUI reads its composer body and its
   submit key separately. Prompt delivery is now two writes with a 500 ms gap
   (120 ms was measured as too short, 500 ms as reliable).

## Not covered here

- `codex` / `grok` / `agy` are detection + `kind` switch only; no transcript
  hydration, and no live run in this evidence.
- The screen heuristics are screen-derived evidence (`completeness:
  screen-derived`), not proof of task success — the same standing caveat as
  `claude-pty`'s `agent_status`.
- The web rendering of this state is covered by unit tests
  (`web/src/pages/SessionPage.promote.test.tsx`), not by a screenshot in this run.
