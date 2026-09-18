# `claude-sdk` M1: a live Claude session that survives two turns

**Date:** 2026-09-19.
**Status:** measured. One live session on a throwaway local dev server, redacted below.
**Design:** [print-replacement.md](../print-replacement.md) §2.1 (process model), §2.2 (resume and session identity), §2.5 (observation table), §2.6 (second carrier). Decision row [D-037](../decisions.md).
**Scope:** M1 = §3 batch 1 + batch 2 + the registration half of batch 3.
**Commits:** the live run below was made on branch `wt/c-sdkdriver/b-sdkdriver-md` at
`56e55988` (the M1 series as first reviewed). The handback round that followed —
the bounded close ladder, the web driver label, the test fixes and this
paragraph — landed on the same branch; its final commit is `HANDBACK_SHA`. The
argv, pids, session id and journal quoted here are from the `56e55988` tree; the
close ladder changed `Driver::close` after that run, and its behaviour is covered
by `tests/claude_sdk_process.rs` rather than by a second live session.

`LIVE`: this session spent real Haiku budget on two one-word turns (reported cost
below, two `result` frames). Everything else in the change is covered by
`fake-claude` and recorded fixtures, with no network in CI.

Redaction: the host is a shared devbox, so absolute home paths, the operator
username, the hostname, the dev access code and the enrollment identity are not
reproduced. Instance / session / host ids are from a data directory that was
deleted after the run. No tokens appear in this file or in any committed fixture.

---

## 1. What was being tested

`claude-print` always launches with `-p` ("Print response and exit"), so the
child is gone after one response — which is why print cannot carry a worker
([D-035](../decisions.md)). The claim M1 makes is that dropping that one flag,
and nothing else, yields a carrier whose child stays alive and takes further
turns on an open stdin. That is only believable against the real CLI: a fake can
be made to do anything.

## 2. Setup

A dedicated dev server, its own data dir and its own ports — never the demo:

```
remuda dev --data-dir <scratch>/data --port 18771 --hub-listen 127.0.0.1:18772 \
           --workspace <scratch>/ws --workspace-root <scratch>
```

The Node resolved the operator's real installed CLI from `PATH`:

```
INFO using Claude binary from PATH path=<home>/.nvm/versions/node/v22.22.2/bin/claude
```

The workspace was a throwaway git repo containing one README. The whole
`<scratch>` tree was removed after the run.

## 3. The instance launched on `claude-sdk`

`remuda instance create --kind claude --driver claude-sdk` was accepted and
forwarded verbatim — no silent substitution:

```json
{ "operation": "instance.create", "state": "accepted", "resolution": "clear",
  "forwarded": true,
  "payload": { "spec": { "kind": "claude", "driver": "claude-sdk",
                         "delegation": "none", "providerSource": "host-inventory" } } }
```

The Node materialized the launch under the new carrier:

```
INFO driver.start: materialized launch recipe driver=ClaudeSdk \
     binary=<home>/.nvm/.../@anthropic-ai/claude-code/bin/claude.exe \
     binary_override=false binary_digest=sha256:15e2d051…58fa07 debt=[]
```

`debt=[]` matters: this carrier does not carry print's M0 `dontAsk` auto-deny
tag, because it refuses that mode outright (§2.8).

## 4. The argv, from the live process table

The whole point of the change is one absent flag. Read from `/proc` while the
child was serving turns:

```
claude.exe --input-format stream-json --output-format stream-json --verbose \
           --include-partial-messages --include-hook-events \
           --forward-subagent-text --replay-user-messages \
           --permission-mode default --permission-prompts host \
           --permission-prompt-tool stdio --model haiku --session-id <uuid>
```

No `-p`. No `--print`. No `--bare`, `--safe-mode`, `--no-session-persistence` or
`--continue`. Non-interactive mode comes from piped stdout, exactly as §1.3 read
out of the extension's SDK argv builder. `--permission-prompt-tool stdio` is
present, so the host approval channel is the one print already used.

Note the model: a bare `claude-haiku-4-5-20251001` was refused by this host's
gateway routing (`API Error: 400 Bare Claude model name cannot be routed. Use a
provider-prefixed name or relay alias.`), and so was an `anthropic/`-prefixed
spelling. The relay alias `haiku` routed. That is a **provider-routing**
observation on this host, not an sdk finding — and it is not evidence about
gateway-on-sdk either way. Per §4.2 that stays **unknown** until the dedicated
probe runs; this session did not run it.

## 5. Two turns, one process

Two `instance.send` commands to the same instance, ~40s apart. The pid serving
the child was read before the first and after the second:

```
PID_BEFORE=1880856
send HTTP 200            # "Reply with exactly one word: TURNONE"
send HTTP 200            # "Reply with exactly one word: TURNTWO"
PID_AFTER=1880856        # SAME PROCESS served both turns
```

A `-p` child would have exited during turn one. Both turns also report the same
native session id, so this is one conversation and not two.

## 6. The journal (51 events, `driverKind` `claude-sdk` throughout)

Hook-topic lifecycles elided; `runtime` rows are the Node's own bookkeeping, and
`stdout` rows are what this driver mapped. Session id redacted to `<session>`.

```
seq | channel | completeness | kind      | detail
 19 | stdout  | structured   | lifecycle | topic=session name=session status=started nativeId=<session>
 21 | stdout  | structured   | message   | role=user      status=complete   'Reply with exactly one word: TURNONE'
 22 | stdout  | partial      | message   | role=assistant status=streaming  'T'
 23 | stdout  | partial      | message   | role=assistant status=streaming  'UR'
 24 | stdout  | partial      | message   | role=assistant status=streaming  'NON'
 25 | stdout  | partial      | message   | role=assistant status=streaming  'E'
 26 | stdout  | structured   | message   | role=assistant status=complete   'TURNONE'
 27 | stdout  | structured   | message   | role=assistant status=complete   'TURNONE'
 31 | stdout  | structured   | lifecycle | topic=turn name=result status=turn_done affects=False
 32 | stdout  | structured   | usage     | in=2 out=7 total=9 cost=0.19333125 USD
 34 | runtime | structured   | message   | role=user      status=complete   'Reply with exactly one word: TURNTWO'   <- second send
 41 | stdout  | structured   | lifecycle | topic=session name=session status=started nativeId=<session>
 43 | stdout  | structured   | message   | role=user      status=complete   'Reply with exactly one word: TURNTWO'
 44 | stdout  | partial      | message   | role=assistant status=streaming  'TURNTWO'
 45 | stdout  | structured   | message   | role=assistant status=complete   'TURNTWO'
 46 | stdout  | structured   | message   | role=assistant status=complete   'TURNTWO'
 50 | stdout  | structured   | lifecycle | topic=turn name=result status=turn_done affects=True
 51 | stdout  | structured   | usage     | in=2 out=8 total=10 cost=0.20871 USD
```

Reading this against the four things the task asked to show:

1. **journal `driverKind`** — every one of the 51 events carries
   `source.driverKind = "claude-sdk"`, and the driver's own rows carry
   `source.channel = "stdout"`.
2. **an assembled message** — seq 22-27: four `Partial` deltas (`T`, `UR`,
   `NON`, `E`) fold onto one node and the final block closes it `Structured`
   with the whole text. This is [D-028a](../decisions.md) item 3 and §2.5
   holding on live bytes: deltas are partial observations, the final assistant
   block is the authority. Print reports `Structured` on the same frames and was
   left that way.
3. **the bound native session id** — seq 19 is the `session` / `started`
   lifecycle mapped from `system/init`, carrying the CLI's own session uuid as
   `nativeId`. The Node lifts any `topic=Session` / `name=session` lifecycle into
   `nativeRef` with no driver-specific code (`runtime.rs`), which is what makes
   `--resume` possible later ([D-026](../decisions.md), §2.2).
4. **the second turn on the same process** — seq 34 onward is the second send,
   answered by the same pid under the same `nativeId`.

Two `result` frames arrived, one per turn — and the `affects_completion` values
above (turn 1 `false`, turn 2 `true`) are **not** what §2.5 asks for. That table
says `affects_completion` only when the result is terminal. What actually decides
it is `map_result`'s inherited print heuristic, `result_index > 0 &&
queued_turn_count == 0`, which was written for a Workflow emitting `result_index`
0 then 1 inside **one** print turn: the first is not process completion, the
second is.

On a child that survives, `result_index` instead grows **per turn**, so the rule
reads "every turn after the first is terminal" — which is why turn 1 reports
`false` and turn 2 reports `true` here. Both are wrong on this carrier: turn 1
did complete, and turn 2 was not the end of the session (the child was still
alive and took `close` afterwards). Nothing downstream in M1 keys off this field
on the sdk carrier, so it is recorded rather than patched; fixing it means
deciding what "terminal" means for a long-lived child, which is an M2 item in
[D-037](../decisions.md) and not a same-round change.

## 7. What this session did *not* show

Honest gaps, so the ADR does not over-claim:

- **No Terminal view, by construction.** `GET …/journal` returned
  `driverHint: "no live screen for this session; showing journal events (a PTY
  carrier serves GET /v1/instances/<id>/screen)"`. Stdio is not a PTY; that is
  §2.6, and the UI must not offer `tty.attach` here. `signalTier` is absent —
  `SignalTier::None` — because this carrier has no hook/file/OSC/screen ladder.
- **Interrupt was not measured.** `instance.cancel` routes to the native
  `control_request` / `interrupt` (§2.3), but this session never cancelled a
  turn, and `fake-claude` acking that frame only proves Remuda writes it. The
  capability cell therefore stays `unknown` for queue and interrupt on every
  driver, sdk included — see the note in `capabilities.rs`.
- **Steer was not measured.** Whether a second `user` frame mid-turn steers or
  queues natively is exactly the open question in D-028a item 2. Both sends here
  were sequential, after the previous turn finished.
- **Gateway-on-sdk was not probed.** §4.2 items 1-4 (settings overlay honoured
  without `-p`, `ping` frames on a slow gateway, `can_use_tool` without `-p`,
  session id after a gateway turn) remain M2. This run used the host's native
  login, `delegation: none`.
- **`affects_completion` is on the print heuristic, not the §2.5 rule.** See §6:
  it is `result_index`-derived, which means something different once the child
  outlives a turn. M2 owns it.
- **No approval or AskUserQuestion round-trip live.** Those are covered against
  `fake-claude` (`approval` / `askuser` scripts) via the shared
  `handle_can_use_tool`, which M1 reuses unchanged.

## 8. Reproducing

```
cargo test -p remuda-claude-wire    # both argv templates; sdk has no -p
cargo test -p remuda-testing        # turn barrier; two turns on one fake child
cargo test -p remuda-driver         # assembler units + process tests vs fake-claude
cargo test -p remuda-node           # (claude, claude-sdk) accepted; sdk never a default
```

No network and no billed model in any of those. The live session above is
evidence, not a gate — per §3's fixture strategy.
