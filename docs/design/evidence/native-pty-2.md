# native-pty-2 — the lifecycle five on a Remuda-owned PTY, live run

Evidence for [D-028](../decisions.md) §5 (new session, send, stop, delete,
exit detection, resume), §6 (steer / queue / interrupt, honestly reported),
§8 (Node restart, plan A), and the **P2 acceptance criterion**: *the two launch
paths produce the same journal, differing only in `launchedBy`*.

- **When:** 2026-09-14, 05:06–06:15 UTC.
- **Where:** `remuda dev` from this worktree, Hub `127.0.0.1:62280`, Node
  `127.0.0.1:62287`, data dir and workspace under `/tmp/remuda-p2`.
- **Agent:** `claude` 2.1.221, pinned
  `sha256:60db8e88d42c24b5199c92cfd56ec88370c510c3789c6f364af748354f087ada`.
- **Flags:** `REMUDA_PTY_CARRIER=native`, `REMUDA_PTY_HOOKS=1`,
  `REMUDA_PTY_EMULATOR=1`. Zero herdr.

Nothing here is a fixture. Every quoted line is from that run.

## 1 — New Session starts the agent, not a shell around it

`POST /v1/instances` with `kind: claude`, `driver: shell-pty`. What the Node
then owns, from `ps -eo ppid=,pid=,args=`:

```
2069536 2074516 …/@anthropic-ai/claude-code/bin/claude.exe --setting-sources user,project,local
```

The Node's direct child **is** `claude` — not a login shell that ran it. Its
pid equals its pgid, because `portable-pty` calls `setsid()`, which is what
makes §5.3's group ladder reach the whole tree later.

The entity:

```
kind: claude   driver: shell-pty   lifecycle: ready   launchedBy: remuda
```

argv comes from the materializer's per-kind recipe (§5.1 steps 2–3), not from
`spec.args`: `--setting-sources user,project,local` is the preset's, and
`--settings <overlay>` is added when the hook overlay exists. `claude.exe` is
the real filename inside the npm package — Remuda execs the *pinned* binary,
where a human's `claude` goes through the bin shim. §2 below is about what
that difference cost.

## 2 — Both paths promote, and their journals match

§1.0 rule 2 makes D-025 promotion the only detection path, including for the
launch Remuda controls. Two instances, same workspace, same binary:

- **A** — `kind: claude`, Remuda runs the command.
- **B** — `kind: terminal`, then `instance.send` of the literal text `claude`,
  which is what a human typing it produces.

```
  [1] A/B mode = native   native
  [2] A/B mode = native   promoted
  [3] A/B mode = promoted promoted
```

Both reach `mode: promoted`. The agent's own event stream, from each
instance's journal:

```
launched: [agent_detected, agent_promoted, transcript_unbound, agent_status]
typed   : [agent_detected, agent_promoted, transcript_unbound, agent_status]
EQUAL
```

The full journals differ by one command envelope — `accepted` / `settled` for
the `instance.send` that carried the human's keystroke. That is the operator's
action, not the agent's; there is no `send` on the launched path because
nobody typed anything. Every event the *agent* produced is identical, in the
same order, on the same channel.

### 2.1 — Three defects this found, all of which failed silently

This comparison is the reason P2's acceptance criterion is a diff and not a
checklist. Each of these produced a working-looking system:

1. **`ps -g <pgid>` is not "this process group."** POSIX and the BSD `ps` read
   it that way; procps-ng — every mainstream Linux — reads `-g` as *session*
   and returns nothing for a pgid that is not also a session id. A PTY's
   foreground group never is. So D-025 detection found **zero rows on Linux**
   and promotion never fired at all, for either path. Measured: `ps -o
   pid=,args= -g 1930867` printed nothing while `ps -eo pgid=,pid=,args= |
   awk '$1==1930867'` printed the running `claude`.

2. **The agent table only knew `claude`.** A human's `claude` resolves to the
   npm bin shim, which `exec`s and appears as `claude`; Remuda's pinned
   binary appears as `claude.exe`. Only the hand-typed path promoted — which
   is exactly the "capability that exists only when Remuda starts it" that
   §1.0 rule 5 calls a P-level defect. It was invisible until both paths were
   run side by side.

3. **`cancel` and `send` read the poller, not the session.** For a
   Remuda-launched agent the kind is known at spawn; reading only
   `promoted_kind` left a ~1 s window in which `instance.cancel` took the
   shell branch and sent `\x03` to a Claude TUI.

## 3 — Capabilities are the session's, reported honestly

`capabilities` on the promoted instance:

```
  steer      supported  native     measured
  queue      supported  emulated   remuda-holds-the-queue
  interrupt  supported  native     measured
```

Before P2 the same session reported the static `DriverKind` row —
`steer: unsupported / unknown / fake-driver`, `queue` and `interrupt`
`unknown` — because nothing ever asked the live driver. The Node now calls
`Driver::capabilities()` after start **and again after promotion**, which is
the event that changes the answer: a PTY carrying a login shell becomes a PTY
carrying `claude`.

`queue: emulated` is the §6 honesty rule doing its job. Claude's native Enter
queue has no after-turn-only guarantee (both Enter and `Ctrl+X`+Enter were
absorbed at tool boundaries in claude-queue-steer-1), so Remuda holds the
queue itself and says so rather than dressing its own ledger up as the
harness's. `agy` stays `unknown` on all three.

## 4 — Stop: interrupting a turn and stopping a process are different

`instance.cancel` on a session with a running turn sends the harness's own
key and the process stays up. `instance.close` runs the §5.3 ladder:

```
INFO pty process group stopped rung="sigint" pgid=2098846
```

```
  claude gone
  group: []
```

`rung="sigint"` means the first rung was enough — the ladder escalates to
`SIGHUP` (with the master fd closed) and then `SIGKILL` only if the group
survives, and rechecks after each. The group is verified empty before the
instance is reported `exited`; a group that outlives `SIGKILL` is journaled as
`stop-incomplete` instead, because §5.3 step 4 forbids reporting a clean stop
over a process tree that is still there.

What this replaces reported success while leaving processes running:
`state.child.try_lock()` then skip-on-failure sent **no signal at all** in the
common case, because the reader thread holds that lock; and `child.kill()` is
a `SIGKILL` to the login shell alone, which orphans the `claude` inside it. A
node test spawns a grandchild and asserts it is gone, since that is the exact
shape the old code left running.

## 5 — `Esc` on an idle composer quits, so cancel does not send it

The first cancel attempt **killed the session**:

```
native_exit | {"exitCode": "1", "promotedKind": "claude", "reason": "native-exit-code-1"}
agent_demoted | {"kind": "terminal", "previousKind": "claude"}
```

claude-queue-steer-1's evidence that `Esc` interrupts is entirely from a
*running turn* — during a retrying request, and during a `Bash` tool call.
Sending it to an idle composer was extrapolation beyond that evidence, and the
failure mode is losing the session the caller meant to keep.

`cancel` now returns `not-dispatched` when the screen says idle: there is
nothing to interrupt, and having asked is not an error. An **unknown** screen
still sends, because failing to interrupt a turn that might be running is the
worse of the two risks. Re-run after the fix: the process stayed alive and the
instance stayed `ready`.

This is also the one place where P2 knowingly stops short. What the ladder
wants is the harness's own *positive* evidence that a turn ended (claude's
`Stop`, codex's `TurnAborted`, grok's `turn_ended{outcome}`); the screen
signature is a weaker stand-in, and the hook path that would supply the real
thing is P5.

## 6 — Exit detection, and where `launchedBy` is still wrong

§5.5 wanted both witnesses, and the accidental `Esc` proved them out end to
end: `child.wait()` produced the status, the journal carried
`native_exit{exitCode: 1, reason: native-exit-code-1}`, the instance settled
`failed`, and the promoted kind demoted back to `terminal` — a crashed agent
no longer reads `ready` forever. Exit code 0 settles `exited`; a non-zero
code, a signal, or a bare EOF settle `failed`.

**`launchedBy` was wrong here, and is now fixed.** The run recorded both
instances reading back `launchedBy: remuda`: the Hub derived that field from
`mode == "promoted"`, which meant "a human typed it" before P2 — but §1.0
rule 2 makes *every* agent promote, so the inference no longer held.

It is now stored rather than inferred, on both sides, from the same
discriminator: what the instance was **before** its first promotion. A
session created as `terminal` that becomes `claude` is a human typing into a
shell; one created as `claude` that becomes `claude` is the launch Remuda
ran. Written once, so a demote/repromote cycle cannot rewrite a session's
origin, and rows predating the column keep the old derivation, which was
sound for them. §2's comparison is still stated over the journal, because
that is where the unification claim actually lives.

## 6.1 — What the host now reports about itself

The demo that prompted this: New Session offered `claude` on `shell-pty`, the
Node silently fell back to a login shell because `REMUDA_PTY_CARRIER` was
unset, and the first prompt was typed into zsh — which ran it as a command.
Nothing anywhere reported that native launch was off, so the UI could not
have known.

The Node now describes itself. `Host.driver_inventory` carries a
`DriverDescriptor` for `shell-pty` whose `launchable` is
`native_carrier_enabled()`, with `reason_code: carrier-not-enabled` when the
flag is off, and the hello frame sends it under `capabilities` — which the
Hub stores verbatim and the web already reads as
`capabilities.driverInventory[].launchable`. `capabilities` was previously
hardcoded `None`, which is why the field never arrived.

Only `shell-pty` is described: it is the one driver whose ability to launch
an agent depends on a runtime flag rather than on a binary existing. A Node
that reports no inventory is read as "not reported", never as "cannot".

Relatedly, `wait_control` no longer gates on `promoted_kind`. That was the
other half of the same demo — a Remuda-launched agent has no promotion yet at
create time, so the create-time prompt bypassed the ready ladder entirely.
It now runs §5.2's ladder (hook receipt > emulator modes + quiescence > glyph)
from the first byte, and an agent that is booting, working or blocked holds
the prompt in the D-022 queue instead of typing into whatever is on screen.

## 7 — Delete and purge

`DELETE /v1/instances/{id}?force=1` on the Hub already routes through
`instance.close` — so it reuses the §5.3 ladder rather than carrying a second
kill — and then asks the Node to `instance.purge`. Purge removes
`<data_dir>/instances/<id>` and nothing else; `~/.claude` was intact after
every run here. The Node's own `instance.purge` refuses a live instance with a
409 rather than yanking the directory out from under a running process.

## 8 — Node restart (§8, plan A)

Not exercised against the live Node — killing it would end the session that
produced everything above — but covered by two node tests that drive the real
`portable-pty`. A Node restart leaves rows that still say `ready` for PTYs
that died with the process; `reconcile_native_pty` settles them `exited` with
`lastError: node-epoch-changed` and journals a diagnostic carrying
`resumable: true`, which is what the web renders as 「Node 重启，会话已结束」
plus a Resume affordance. A second reconcile adds nothing, so a Node that
restarts repeatedly does not accumulate one notice per restart.

It runs **before** the herdr reconcile. Waiting out a predecessor's session
server can consume the whole retry budget, and native-PTY rows must not sit at
`ready` behind an unrelated carrier's wait.

## 9 — Not covered here

- **`--effort`, `CODEX_HOME` / `GROK_HOME`, yolo argv** are wired through the
  materializer and unit tested, but this run used neither a non-default effort
  nor codex/grok. `grok` is not installed on this host; codex 0.147 is, but
  its adapter is P6.
- **Resume** is implemented on both sides §5.6 named — the driver's
  `CapabilityUnsupported` is gone and the Hub now returns `shell-pty` for a
  `shell-pty` parent — and the argv shaping is unit tested, including that
  codex's resume is a subcommand rather than a flag. It is **not** exercised
  live: that needs a session with real transcript history, which this
  scratch workspace has none of.
- **steer and queue as user actions.** The capability is reported truthfully
  and the key table is measured, but the composer wiring that sends them is
  P4, so nothing here pressed those buttons.
- **The parity gate as `remuda journal diff`.** §2's comparison is over the
  event signature stream, not through the diff tool, because that tool's
  input is a journal dump of a *scripted 3-turn scenario* and the live session
  here ran no turns. Wiring the live pair through it is P7's gate.
- **grok and agy** were not run at all. Their rows in the key table come from
  grok-signals-1; agy's are `unknown`, which is what the capability reports.
