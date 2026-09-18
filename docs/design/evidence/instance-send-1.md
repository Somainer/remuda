# instance.send lands on shell-pty native and its ledger row resolves

Date: 2026-09-18 (UTC). Scope: live reproduction that an `instance.send` built
by the CLI reaches a Claude agent on the native shell-pty carrier, and that the
Hub command ledger resolves instead of sitting at `queued`. No demo data dir or
ports were touched; this ran against a throwaway `remuda dev` server with its
own data dir and high loopback ports. Paths below are redacted (`<HOME>`,
`<SCRATCH>`).

## Conclusion

The ssh-stdio demo defect — `remuda instance send` returned `state: queued,
forwarded: true` and no turn ever started — is fixed. On this run, over the
**native shell-pty carrier with the emulator on**:

- an `instance.send` whose payload is a prompt wrapper with text blocks
  (`input.blocks: [{type:"text", text}]`, mode nested under `input`) is now
  folded to a prompt by the Node and **reaches the driver**; the CLI reports
  `state: accepted` / `resolvedState: accepted`, and the agent starts a turn;
- a second send issued while a turn is running is **held and delivered at the
  turn boundary** — the composer showed the second prompt and the agent
  answered it (the word `LATER` appeared after the first turn's work);
- every send **settles** in the ledger, and the new
  `GET /v1/instances/{id}/commands` lists each with operation, state,
  resolution and timestamps.

The composer submitted correctly on the **glyph rung alone**: on this host the
pinned hook relay could not load (an environment `libssl.so.3` gap, unrelated to
this change), so **no `UserPromptSubmit` / `Stop` hook fired for any prompt** and
the driver fell back to the emulator screen signature throughout. Consequently
the mid-turn hold and the boundary delivery were observed only through the
emulator transcript (the rendered grid), not through hook evidence. The Enter
still submitted every prompt, so no ghost-suggestion clearing key was needed in
the keys module (brief Fix 2); the hold-and-deliver mechanics are additionally
covered deterministically, with hooks on, by the driver test
`shell_pty_fake_send::a_send_lands_when_idle_and_is_held_until_a_running_turn_ends`,
which asserts on the fake harness's own submit/enqueue event log.

## Setup

- `remuda 0.1.0` built from this branch, rustc 1.94.1, target
  `x86_64-unknown-linux-gnu`. (An earlier draft cited a commit sha that was
  rebased away; the run predates the final history, so no sha is pinned here.)
- Claude binary: `2.1.274 (Claude Code)`, resolved from PATH (the real CLI, not
  a Remuda shim).
- Scratch dev server, its own data dir and loopback ports, never the demo:

  ```sh
  REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1 \
    remuda dev --data-dir <SCRATCH>/data --port 47131 \
      --hub-listen 127.0.0.1:47130 --workspace-root <SCRATCH>/ws
  ```

- Instance created with `driver: shell-pty`, `kind: claude`, cwd `<SCRATCH>/ws`.
  The Node logged `terminal emulator on for this PTY (REMUDA_PTY_EMULATOR)` and
  `pinned native binary … version=2.1.274 (Claude Code)`.

## Idle send (CLI)

`remuda instance send <ins> --text "Reply with the single word PONG …"` returned:

```json
{
  "command": {
    "operation": "instance.send",
    "state": "accepted",
    "resolution": "clear",
    "forwarded": true,
    "payload": { "input": { "type": "prompt", "mode": "new-turn",
      "blocks": [{ "type": "text", "text": "Reply with the single word PONG …" }],
      "origin": "human" } }
  },
  "replayed": false,
  "resolvedState": "accepted"
}
```

Before the fix this same block-shaped payload made the stdio Node reply
`invalid request: instance.send requires input.text`, and no turn started. Here
the journal moved `agent_status: idle → working → idle` and grew new message
events, so the prompt reached the driver and the agent answered.

## Mid-turn send is held to the boundary

A first send started a long turn; a second send followed ~2 s later, mid-turn.
Both were accepted (`resolvedState: accepted`, `forwarded: true`). The emulator
grid at the end showed the second prompt had been delivered and answered:

```
❯ After you finish, also say the word LATER.
  ⎿  UserPromptSubmit hook error
  ⎿  Failed with non-blocking status code:
     <SCRATCH>/data/dev-hub/node/hook-bin/<digest>/remuda: error while loading
     shared libraries: libssl…
● LATER
● Ran 2 stop hooks
  ⎿  Stop hook error: … libssl.so.3: cannot open shared object file …
✻ Worked for 4s · done
```

The `UserPromptSubmit` / `Stop` hook errors are the relay's missing `libssl.so.3`
on this host — the hook rung was unavailable, and the send still submitted via
the emulator glyph rung. `LATER` printing confirms the mid-turn send was
absorbed at the turn boundary rather than typed into the running turn.

## Ledger resolves; the new route lists it

`GET /v1/instances/{id}/commands?limit=6` returned each command newest-first,
every send `settled` / `clear` (accepted then settled from the mirrored
journal), never stuck at `queued`:

```
instance.send  state=settled  resolution=clear  reason=None  created=…02:30:55Z
instance.send  state=settled  resolution=clear  reason=None  created=…02:30:53Z
instance.send  state=settled  resolution=clear  reason=None  created=…02:29:47Z
instance.create state=settled resolution=clear  reason=None  created=…02:27:13Z
```

The failed-resolution paths are covered deterministically by two Hub tests,
since this live host accepts every send: `a_send_the_node_rejects_resolves_failed_and_is_listed_by_the_new_route`
(an error reply → `failed`) and `a_send_the_node_never_acks_fails_on_the_deadline`
(the Node receives the call but never replies → `reconciling` → the bounded ack
deadline fails the row). The store test `a_journaled_accept_recovers_a_deadline_failed_send`
proves the §2.5 recovery direction: a journaled accept that arrives after a
deadline failure flips the row back to `accepted` and clears the reason.

## Known follow-up for the web owner (not fixed here)

`web/src/lib/api.ts` coerces command state and resolution through allowlists
that predate this change: an unknown `state` becomes `"accepted"` and an unknown
`resolution` becomes `"clear"`. A `failed` send therefore renders in the web UI
as accepted with a clear resolution, hiding the failure and its reason. The Hub
now emits `failed` / `failed` with a `reason`, and the generated client
(`web/src/lib/api.generated.ts`, regenerated here) carries them, but the
hand-written coercion in `api.ts` needs the web owner to extend those allowlists.
This worker's ownership excludes `web/` beyond the generated client, so it is
flagged rather than fixed.
