# Promoted Claude hooks, activity and interrupt

Follow-up to [P1](native-pty-1.md) for the reported bug:
「结构化视图无法同步 working 的状态，不能做打断」.

## Scope and reproduction

The run uses a task-owned `remuda dev`, Hub `127.0.0.1:62780`, Node
`127.0.0.1:62787`, a scratch workspace and data directory, with
`REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`. No coordinator demo or OS GUI is
used. The installed native harness is Claude Code **2.1.270**.

Create `kind=terminal`, `driver=shell-pty` through `POST /v1/instances`, then
send the following through `POST /v1/instances/{id}/commands`, operation
`tty.write`, with UTF-8 bytes encoded in `payload.dataBase64`:

```sh
claude --settings ~/.claude/settings.relay.json --model 'claude-opus-5[1m]'
```

The shell command ends in CR. After Claude's composer appears, send the short
prompt body and CR in **separate** writes. Observe `GET /v1/instances/{id}`,
`GET /v1/instances/{id}/journal`, and the task-owned Node's TTY snapshot.

The causal baseline restores the generated shim from main `274c0d0` into just
the baseline instance's launch directory. This isolates the launch behavior
on the same development setup; it is not a claim that the entire development
binary is an unmodified main build.

On 2026-09-14 at 07:26–07:28 UTC, instance
`ins_01a09ecf-2da7-77b9-9cdd-946cf6513b0b` promoted to Claude, launched by the
user. The short prompt was accepted by the native composer and the screen
showed `Actioning… (34s)`, while the Hub remained `activity=idle` and
`signalTier=null`. **Zero Remuda hook-channel observations** arrived. This
reproduces the missing turn hooks; it does not reproduce the coordinator's
single SessionStart exactly. Native Esc restored the pending prompt, after
which `/exit` returned to the shell and the instance was closed.

## Root cause and changes

The original shim explicitly passed through whenever it found `--settings`
or `--settings=...`. It did not merge that file with the generated overlay.
The instance's unused `launch/settings.json` already registered the hook set;
its existence did not mean Claude loaded it. The supplied relay settings file
contained environment/model configuration and **no hook registrations**.

The shim now execs `remuda hook launch` for explicit settings. The helper reads
the file or inline JSON, preserves user configuration and hook matchers,
appends Remuda registrations, pins the existing terminal keys, writes a
private, unique overlay beneath the instance's launch directory, and execs the
real Claude with the original remaining arguments. The original file is not
modified. Explicit setting sources and model flags remain intact.

The merged file has mode `0600`, its parent is private, and it registers:

```text
SessionStart UserPromptSubmit Stop StopFailure SessionEnd SubagentStop
Notification PreToolUse PostToolUse PostToolBatch MessageDisplay
PermissionRequest Elicitation
```

The Node binds SessionStart to the current promoted PID/session before
persisting `nativeRef.signalTier=hook`. Promotion includes the PID; an early
SessionStart is retained until that promotion arrives. Driver-local envelope
IDs are rebound by the existing per-instance receiver/store path, so comparing
them directly with Hub instance IDs would incorrectly discard valid hooks.
The causal Node test deliberately uses different IDs to protect this boundary.
The bounded pending cache also preserves a replacement process's early
SessionStart across the previous process's demotion and replays its latest
matching turn boundary when promotion catches up. A prompt submitted before
that poll therefore still becomes working. A new SessionStart from the same
verified foreground PID can also replace its session binding; subsequent
events from the old session remain inert. Legacy `claude-pty` keeps its
existing screen status path; the new hook-tier claim belongs to the bound
promoted shell process.
The promotion poller also hands the socket's matching SessionStart transcript
to the existing transcript binder. Previously its public ingest entry had no
production caller, so a session without Claude's separate PID file could be
hooked yet still display the manual transcript picker. The fake-harness e2e
exercises this socket binding without supplying a resume ID or synthetic PID
file.

UserPromptSubmit sets working; Stop and StopFailure set idle. SubagentStop and
lower-priority screen observations cannot override a bound hook turn. The Hub
mirrors the Node's authoritative Instance lifecycle updates, including signal
tier. The Node also stamps its validated activity onto the causal native
observation as `relatedIds.remudaActivity`. Hub and web can apply that state
without a second per-turn Instance event and its separate journal ACK;
unvalidated hook status and SubagentStop remain inert. Session binding still
publishes the full Instance with identity and signal tier. Other activity such
as waiting-interaction retains full Instance propagation. The causal native
event durably carries each validated working/idle transition exactly once.
The web store applies these validated updates directly from follow, rather
than waiting for its two-second HTTP poll. Journal sequence comparisons also
prevent an older in-flight poll from undoing the new activity.

Promoted Claude cancel sends Esc. Shell cancel retains Ctrl-C. A key write is
not recorded as completed interruption: a matching Stop/StopFailure or fresh
native interruption screen confirms it and produces lifecycle `interrupted`.
Idle cancellation is a no-op. This overlaps the broader key-ownership work on
`origin/wt/r-p2-core/agent-pty-lifecycle`, which was not merged at initial
implementation; this branch only maps Esc for promoted Claude.

## Aliases and absolute paths

The generated ZDOTDIR sources the user's zsh startup files and reasserts the
shim directory at the front of PATH. It preserves aliases and functions;
PATH precedence does **not** override shell alias expansion. An alias whose
expansion invokes `claude` by name still reaches the shim. An alias expanding
to an absolute executable path, or an explicit `~/.local/bin/claude`, bypasses
it. No user rc file is rewritten and no alias is removed.

Before exec, an enabled shim creates a private per-PID marker in
`launch/shim-pids/`. A detected Claude with a readable marker directory but no
matching marker or SessionStart gets one diagnostic per promotion epoch:
「claude 未经 shim 启动，hook 不可用」. Screen detection continues. An absence of
events alone is not used as bypass proof.

## Validation record

### Real native run

The final native instance was `ins_01a09ed7-6650-7695-87df-df6f9800fcf2`,
07:35–07:41 UTC on 2026-09-14. Its native session was
`2eb8be63-7d5b-4bd9-a62b-9162b53b823d`, foreground PID `64369`.
The task-owned Node's instance API returned `nativeRef.signalTier="hook"`;
the Hub's flattened instance API returned `signalTier="hook"`.

| Journal seq | Channel | Native event | Observed result |
| --- | --- | --- | --- |
| 5 | runtime | agent_promoted | kind=claude, mode=promoted, launchedBy=user, PID=64369 |
| 9 | hook | SessionStart | native session bound, tier=hook |
| 11 | hook | UserPromptSubmit | Hub working |
| 16 | hook | MessageDisplay | `HOOKGAP_OK` |
| 17 | hook | Stop | Hub idle |
| 21 | hook | UserPromptSubmit | next turn working |
| 24 | hook | PreToolUse | Bash `sleep 30` |
| 31 | pty | interrupted | fresh native interruption screen after instance.cancel; idle |
| 35 | hook | UserPromptSubmit | same process accepts next turn |
| 38 | hook | MessageDisplay | `HOOKGAP_AFTER` |
| 41 | hook | Stop | idle |
| 43 | hook | SubagentStop | diagnostic only; no new working turn |

The first prompt was `Reply with only HOOKGAP_OK.`. Polling the Hub recorded
working at **1.132 s** from the start of the CR command request, then idle at
**7.565 s**. The hook timestamps were 07:35:38.613 for UserPromptSubmit and
07:35:45.336 for Stop. These measurements include HTTP command dispatch and
polling; they are not claimed as network-free bus latency.

The next prompt requested Bash `sleep 30`. After its PreToolUse observation,
`instance.cancel` produced journal `interrupted` within **1.042 s** and the
native screen displayed `Interrupted · What should Claude do instead?`.
**No Stop/StopFailure hook arrived for this cancellation**: the confirmation
was explicitly `channel=pty`, `completeness=screen-derived`, PID=64369,
`requestedBy=instance.cancel`. The subsequent `HOOKGAP_AFTER` response proves
the same Claude session survived. A later native idle Notification was also
observed and retains the existing notification/interaction mapping.

The exact-path run `ins_01a09ed9-1cf8-7532-8530-d7414332bb39` used
`~/.local/bin/claude` with the same settings/model flags. The alias run
`ins_01a09eda-6c82-7467-9ef8-784eece166f4` first ran
`alias claude="$HOME/.local/bin/claude"`, then the bare command with those
flags. Both promoted, produced **zero hook observations**, kept screen-derived
idle, and recorded the required Chinese bypass diagnostic once (seq 6).
Each native agent was exited with `/exit` before closing its shell instance.

The transport is working: the final native events above invoked the real
relay through the private instance socket. The missing baseline hooks were
not socket/credential/timeout failures. During implementation a new overly
strict ID guard did drop a SessionStart and logged that rejection; it was
removed in favor of the existing receiver ownership plus PID/session and
process-generation checks before this final run.

Screen confirmation deliberately requires a fresh interruption marker in PTY
output, a changed rendered screen, and the current turn's marker before the
empty native composer. The grid and marker count are sampled together. A bounded
stream recognizer handles ANSI sequences and split reads; its counter survives
scrolling, so a new marker can replace the previous marker in a full viewport.
A marker arriving before the composer remains pending until that composer is
rendered. Historical markers above a newer submitted prompt, even with a
changing spinner, and unchanged old repaints are not confirmation.
Nonempty drafts are not taken as composer confirmation. If Claude cancels an
early request by merely
restoring its prompt with no hook or fresh marker, Esc is sent but no completed
interruption is claimed. Identical historical and new repaints remain
ambiguous without stronger native evidence.

### Automated checks

After rebasing onto `3da158d`, the dedicated
`promoted-claude-hub-live.spec.ts` passed against a disposable production Node
and `fake-harness --kind claude` (24.5 s final focused test runtime). It types explicit user
settings through the shim, checks hook tier and user promotion, asserts web
working/idle within 1.5 s windows, clicks the actual 打断 button, checks native
`interrupt by=esc` plus journal `interrupted`, and completes another turn on
the surviving session. The test repeats cancellation in
that same session and requires exactly two native Esc events and two journal
interruptions. The 1.5 s values are assertion windows after submission or
observed native completion, not measured hook-to-browser latency.
The fixture supplies a real transcript path in
SessionStart, as native Claude does; it does not fabricate a Stop hook on Esc.

Focused driver/CLI tests check all registrations, file and inline settings,
private overlay permissions, unchanged user files/arguments, and unchanged PID
through the merger's exec. Node SignalBus→observation-pump tests cover different
driver/Hub IDs, current PID/session binding, working/idle, SubagentStop, foreign
events and screen precedence. Hub and web tests cover authoritative activity
mirroring and the stale-poll race.

Existing test-environment failures surfaced during the required full gates and
were made deterministic without changing production behavior or OS settings:
driver construction uses an explicit temporary Claude config; the stdio
dispatch test uses its existing injected inventory seam; WSS authorization and
daemon replay fixtures supply temporary Claude configuration to native creates;
the temporary workspace-root test uses a unique directory and compares
canonical paths, accounting for macOS `/var` → `/private/var` resolution; and
the parity generator test now writes to an isolated directory before comparing
bytes, instead of briefly truncating the fixtures that sibling tests read.
The latter race caused intermittent empty/invalid journal-diff results.
The Claude transcript fixture also waits for its native approval event before
sending `1`; sending it during the preceding thinking phase was ignored and
left the fixture waiting for approval until its exit deadline.
No tests or deadlines were removed.

Full Hub runs exposed a fake-node websocket reader that assumed the next frame
was a journal ACK and could discard a concurrent RPC. Its main loop is now the
sole reader after enrollment. Provider discovery honors the assigned upstream
port, and its save test waits for the successful POST before asserting the form
closed. Reload uses the same 20 s deadline as initial history while retaining
the exact one-message assertion; the unchanged reload test also passed alone.

The repeated-cancel run caught a separate delivery delay: the Node recorded
UserPromptSubmit at 08:43:30.761 and its working Instance event at 30.767, but
the Hub projected that Instance event at 33.379. The per-event ACK path made
activity wait for another serialized append. This motivated the validated
activity annotation above; the 1.5 s feature assertion was retained.

A further run isolated periodic delay before Hub projection: seq 33
`interrupted` was observed by the Node at 09:11:31.554, persisted by the Hub at
32.869, and received on browser follow at 32.871. Another turn's submit waited
0.989 s before Hub persistence, then 10 ms to browser receipt. These are
separate clock samples on the same machine. The full verifier in device-token
authentication ran on the sole Hub SQLite writer, competing with journal
appends while the web's regular authenticated polls ran. Verification now
executes outside that writer; a subsequent guarded lookup rechecks the same
credential and returns current device scope. Revoked or replaced credentials
remain rejected, without caching or reducing Argon2 parameters. A gated
verifier regression proves unrelated writer jobs can proceed while verification
is paused and covers credential mutation during that pause.

After the Hub writer change, the 09:17 UTC focused run passed both
interrupts and the surviving process's next completed turn. Its eight
UserPromptSubmit/Stop/interrupted events took **5–27 ms** from Node observation
to Hub persistence and **7–51 ms** to browser WebSocket receipt. The three
working assertions passed in **77–83 ms** after their CR requests returned;
idle assertions passed in 2–42 ms after the test observed native completion or
the interrupted journal entry. These are fake-harness measurements through the
production transport; the separate native Claude measurements remain above.
After the final screen-parser fixes and all Rust gates, the 09:57 UTC native
e2e rerun also passed (24.5 s). Its eight turn/interrupt events took 7–18 ms to
Hub persistence and 8–27 ms to browser receipt, with exactly two native Esc
interruptions, two journal interruptions and two completed turns in one process.

Final gates on rebased main `3da158d`:

| Check | Result |
| --- | --- |
| `cargo fmt --all` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo test --workspace --locked --no-fail-fast` | PASS: 144 test targets, 1,348 passed, 0 failed, 13 existing ignored tests |
| `pnpm test` | PASS: 69 files, 420 tests |
| `pnpm run build` | PASS |
| `pnpm run lint` | PASS: four existing warnings in unchanged components |
| `pnpm run test:e2e:hub` | PASS: 17 tests, 1 existing conditional skip, 2.0 min; native two-interrupt test 17.8 s |
| `./scripts/ci/secret-scan.sh` | PASS |

The skipped test requires the suite's external real Node mode. This suite's
new native-hook test independently starts a disposable production Node.
The own dev and e2e listeners were stopped; the five real-run instances and
their native agents exited, and generated Spaces screenshots were removed.
