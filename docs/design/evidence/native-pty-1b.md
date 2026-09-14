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

## Gate follow-up: native fixture portability (2026-09-14)

The coordinator reported a second Hub gate failure on dedicated ports after
merging `30e0755` with main `3da158d`; the failing spec names were unavailable.
Fetching and rebasing locally left that same base. On this Mac, the unchanged
branch passed the complete suite with both Chrome and CI's bundled Chromium:
17 passed, one existing conditional skip, about two minutes per run. The
remote failure itself has therefore **not** been reproduced here. Review did
not establish a new `store.ts`, provider-discovery, or Passkey regression.

Three independent problems were reproduced or covered by focused regressions:

1. **The native spec depended on warm executable artifacts.** The standalone
   Hub command built only `hub_e2e`, while the new spec accessed `debug/remuda`
   and `debug/fake-harness`. Parking just those two files in this worker's
   Cargo target made the original spec fail with `ENOENT` before enrollment.
   Its `beforeAll` now builds the default executables unconditionally, retaining
   explicit binary overrides and the inherited Cargo target/build settings.
   This also refreshes stale binaries and works when an external Hub is reused.
   Build setup has its own bounded deadline; turn activity still has 1.5 s.
2. **Linux selected a session instead of the foreground process group.**
   Numeric `ps -g` selects a session ID in Linux procps, as documented in the
   [procps manual](https://man7.org/linux/man-pages/man1/ps.1.html).
   A disposable Debian container created a job with PID/PGID 190 and SID 1.
   The old query returned zero rows; querying explicit PID/PGID columns and
   filtering PGID returned the job. Linux now uses that explicit filter; BSD
   keeps its existing selector. A parser regression excludes other groups,
   and a real Unix process-group test checks an owned child whose PGID differs
   from its inherited session. The child is killed and reaped on every path.
3. **Linux's hook shell changed the reported parent PID.** In the same
   disposable container, `/bin/sh -c` retained an interpreter: the relay-shaped
   process had PPID 197 while the harness-shaped caller was PID 1. With an
   explicit `exec`, PPID was 1. Every generated relay command now starts with
   `exec`. The CLI regression materializes a real overlay, runs its command
   through `/bin/sh`, receives it over the authenticated hook socket, and
   asserts the envelope's PPID is the harness process. A trailing shell builtin
   prevents an optional shell optimization from hiding this bug on macOS.

The Linux container probes establish command and parent-PID semantics; they
are not a Linux run of the complete Remuda gate. All containers used for these
probes were disposable. Local Hub runs used worker-owned ports 58580/58589 and
upstream 58581. All local run logs, both successful baseline artifacts, and
the missing-binary reproduction artifacts were retained outside the repository.

The corrected complete Hub suite started with **both native executables
absent** and passed on CI's bundled Chromium with retries disabled: 17 passed,
one existing conditional skip, 3.4 minutes including the automatic build.
The native spec took 21.7 s and retained exactly two native Esc interruptions,
two journal `interrupted` events, two completed turns in the same process, and
`signalTier=hook`. Its three working assertions passed 34–83 ms after the CR
request returned; idle assertions passed in 1–4 ms after the native/journal
completion checks. These are fake-harness measurements, not model latency.

Follow-up validation on main `3da158d`:

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | PASS |
| `cargo clippy -p remuda-driver -p remuda --all-targets --locked -- -D warnings` | PASS |
| `cargo test -p remuda-driver -p remuda --locked` | PASS: 36 targets, 555 passed, 0 failed, 10 existing ignored tests |
| `pnpm --dir web test` | PASS: 69 files, 420 tests |
| `pnpm --dir web run build` | PASS |
| `pnpm --dir web run lint` | PASS: four existing warnings in unchanged components |
| `CI=1 pnpm --dir web run test:e2e:hub --retries=0 --reporter=list` | PASS: 17 passed, one existing conditional skip; missing binaries rebuilt by setup |
| `./scripts/ci/secret-scan.sh` | PASS |

The owned Hub, web, upstream and native Node processes were stopped. Temporary
binary backups and generated Spaces screenshots were removed; test artifacts
remain outside the committed tree for diagnosing any subsequent gate failure.

Main advanced to `26371ad` during delivery. Both commits rebased cleanly, then
fmt, the same two-crate clippy/test commands (555 passed), web tests (420 passed),
and both CI scans passed again. The complete Hub suite also passed again on
that base with CI Chromium and no retries: 17 passed, one existing conditional
skip, 3.8 minutes including builds. The native spec took 22.6 s and again
recorded exactly two Esc interruptions, two journal interruptions, two completed
turns in one process, and hook signal tier. Final artifacts were archived and
the worker's listeners and generated screenshots were cleaned up.

## Integration with P2 native carriers and launch options (2026-09-14)

The coordinator's next gate stopped on merge conflicts after main advanced to
`230532e`. The two worker commits were rebased with P2's intent from
`native-pty-2.md` and the launch-option commits reviewed before resolution.

- P2 retains the native agent recipe, process-group stop ladder and exit waiter,
  observed launch origin, driver inventory, per-harness key table, and first
  prompt readiness. Hook working/idle and readiness use the current agent PID;
  a stale SessionStart cannot unlock another foreground process. Cancel uses
  P2's key table, with the existing hook/native-screen interruption confirmation.
- Native Claude now merges the configured settings into the HookSession overlay,
  then rematerializes its recipe so argv and the settings digest agree. The
  original recipe validates before hook artifacts are created. A regression
  preserves provider/custom environment and user hooks, checks every registered
  event and the audited digest, and verifies rejection before hook setup.
- The shell shim retains launchopts' pinned executable resolver and the Rust
  explicit-settings merger. PID markers use their materialized directory in
  both resolver modes. The CLI regression tests file and inline settings with
  both PATH and a pin; a failing PATH decoy proves the pinned executable wins.
  Direct native agent launches intentionally skip the PATH shim, so they do not
  receive the hand-typed shim-bypass diagnostic.
- P2's portable `ps -eo pgid=,pid=,args=` implementation supersedes this branch's
  earlier platform-specific implementation. Both its detection coverage and the
  worker's actual process-group regression survive, as does relay `exec` for
  parent-PID fidelity. The journal parity test uses main's isolated `--out`
  generator API. Node pump shutdown, capability refresh and pending SessionStart
  binding remain in one observation path.

The rebased full Hub suite used CI Chromium, no retries, Hub/web ports
58580/58589 and upstream 58581: **17 passed, one existing conditional skip,
3.9 minutes**. The promoted native spec took 22.7 s, with two native Esc
interruptions, two journal interruptions, two completed turns and hook tier.
Its three working assertions passed in 77–79 ms after their CR requests;
idle assertions passed in 1–8 ms after native/journal completion checks.
Passkey, effort slider, native PTY selection, provider discovery and Spaces
all passed in the same full run.

### Fresh real Claude run after the shim resolution

A disposable `remuda dev` ran on Hub 62780 / Node 62787 with
`REMUDA_PTY_CARRIER=native`, `REMUDA_PTY_EMULATOR=1` and `REMUDA_PTY_HOOKS=1`.
A Hub-created `kind=terminal`, `driver=shell-pty` instance received the original
hand-typed command:

```sh
claude --settings ~/.claude/settings.relay.json --model 'claude-opus-5[1m]'
```

Claude Code **2.1.270** ran as PID **3782**, with session
`6d4bf208-eac9-4a29-836e-0c0fb95444bb`. The instance was
`ins_01a0a00c-cffe-737f-8621-5f3dde534a3e`. Hub showed `mode=promoted`,
`launchedBy=user`, and `signalTier=hook`. Its private per-invocation settings
file was mode 0600, preserved every non-hook source setting, and contained all
13 registered events. The shim PID marker and SessionStart PPID both matched
3782. No source settings or credentials were changed or copied into this doc.

| Journal seq | Evidence | Result |
| --- | --- | --- |
| 5 / 8 | Promotion / SessionStart with PPID 3782 | Native session and hook tier bound |
| 10 / 13 / 16 | UserPromptSubmit / MessageDisplay / Stop | `HOOK_REBASE_OK` completed; working → idle |
| 17 / 19 | UserPromptSubmit / PreToolUse | `sleep 60` turn working |
| 24 / 25 | PostToolUse / PostToolBatch | Real tool hooks arrive through the merged settings |
| 29 | Fresh PTY `interrupted` | `instance.cancel` sends Esc; native Interrupted composer and Hub idle; PID 3782 still alive |
| 31 / 33 / 34 | UserPromptSubmit / MessageDisplay / Stop | Same process replies `HOOK_AFTER_ESC` and completes |
| 37 / 41 | Background-completion UserPromptSubmit / second `interrupted` | New native turn encountered upstream API retries; another Esc returns it to idle |
| 43 | SubagentStop | No working annotation or new working transition |

The three user prompts were observed working through Hub polling at 499, 964,
and 897 ms from the CR request start. Normal Stop events took 7–8 ms from Node
observation to Hub persistence. The two Esc-to-interrupted polling measurements
were **1,315 ms and 1,531 ms**; these include the command request and polling.
The first interruption reached Hub 14 ms after its native PTY observation.
Real Claude did not emit Stop for Esc, so the fresh native Interrupted screen
remains necessary evidence; neither command settlement nor old screen history
was counted as turn completion. The later background completion was its own
UserPromptSubmit, distinct from SubagentStop.

The hook/interrupt checks passed. Cleanup exposed a separate close limitation:
P2's stop ladder logged `Operation not permitted (os error 1)`, while the
existing close path still settled `explicit-close` as exited. The login shell
and its separate foreground Claude process were still present at that point.
The close/stop error handling is unchanged from main in this resolution. The
worker explicitly terminated its owned PIDs and stopped its dev process;
all three PIDs and all five owned listener ports were then verified absent.
This is recorded as a stop-reporting follow-up, not a successful native close.

Validation on the resolved `230532e` source base:

| Check | Result |
| --- | --- |
| `cargo fmt --all` | PASS |
| `cargo clippy -p remuda-driver -p remuda-node -p remuda-signal -p remuda --all-targets --locked -- -D warnings` | PASS |
| `cargo test -p remuda-driver -p remuda-node -p remuda-signal -p remuda --locked` | PASS: 56 targets, 860 passed, 0 failed, 10 existing ignored |
| `pnpm --dir web test --maxWorkers=2` | PASS: 70 files, 432 tests |
| `pnpm --dir web run gen:api` | PASS: no generated diff |
| `pnpm --dir web run build` | PASS |
| Full `pnpm --dir web run test:e2e:hub` with CI Chromium and no retries | PASS: 17 passed, one existing conditional skip |
| `./scripts/ci/secret-scan.sh` and `./scripts/ci/no-tunnel-scan.sh` | PASS |

Full-suite artifacts and real-run logs were archived outside the repository.
Generated Spaces screenshots were removed. The two rebased worker commits retain the hook changes and explain the
P2/launchopts integration.


### Final main update: macOS parent-watch portability

The final fetch advanced main again to `7ae99e6`, adding fake-helper parent
watching and server cleanup. The two worker commits rebased cleanly, but the
requested clippy command then failed with `E0432`: nix 0.28 does not expose
`unistd::pipe2` on macOS. This was new main code, not a hook merge conflict.

A third, narrowly scoped commit supplies a notification pipe with the same
flags: Linux keeps atomic `pipe2`; other Unix platforms use `pipe` and set
`FD_CLOEXEC` and `O_NONBLOCK` on both owned descriptors before starting the
watcher. The regression checks both ends' flags, empty-pipe `EAGAIN`, and that
the notification byte wakes the async waiter. It does not start a watcher that
could terminate the test runner. The parent-death and stdin-hangup behavior is
preserved. The requested Rust checks and full Hub suite are repeated on this
final source base, including the existing helper lifecycle tests.

Final validation completed after rebasing onto `e97c200` (the new web
filters/overlay changes). A tree comparison confirmed that Rust sources,
`Cargo.toml` and `Cargo.lock` were identical to the five-crate run on
`7ae99e6` plus the pipe fix. Web tests/build and the full Hub suite used the
new `e97c200` web source.

| Final check | Result |
| --- | --- |
| `cargo fmt --all --check` | PASS |
| `cargo clippy -p remuda-driver -p remuda-node -p remuda-signal -p remuda -p remuda-testing --all-targets --locked -- -D warnings` | PASS |
| `cargo test -p remuda-driver -p remuda-node -p remuda-signal -p remuda -p remuda-testing --locked` | PASS: 68 targets, 924 passed, 0 failed, 10 existing ignored |
| Notification pipe / fake server drop and orphan cleanup regressions | PASS |
| `pnpm --dir web test --maxWorkers=2` | PASS: 73 files, 487 tests |
| Web build and API generation | PASS; no generated API diff |
| Full Hub e2e, same dedicated ports, CI Chromium, no retries | PASS: 17 passed, one existing conditional skip, 3.5 minutes |

The final promoted spec took 19.1 s: two native Esc interruptions, two journal
interruptions, two completed turns in one process, and hook signal tier. Its
working assertions took 77–78 ms; idle assertions took 1–2 ms after completion
checks. Final artifacts were archived, generated Spaces screenshots removed,
and all five worker listener ports verified unused. Four older orphan fake
servers from this worker's own Cargo targets were also terminated; other
workers' processes were left alone.

Final secret scan, no-tunnel scan and whitespace checks also passed.

### Rebase onto inventory extraction and streaming (`eefcbba`)

The next merge gate found a runtime conflict after the inventory extraction,
P3 streaming and UX-C1 status changes reached main. This rebase keeps
`driver_inventory()` and `driver_capability_snapshot()` exclusively in
`inventory.rs`; runtime still calls those shared builders. Host snapshots and
the Node hello/heartbeat retain `driverInventory`. The stdio test's synthetic
host now supplies the new inventory field explicitly.

The observation pump keeps both P3 message assembly and the promoted-hook
binding/activity fold. A joint SignalBus regression verifies early
SessionStart/prompt binding, hook tier, two MessageDisplay chunks with ordered
`open`/`append` mutations, streaming/completed status, and the existing turn
ownership behavior. Message synthesis uses the same bound PID/session
check as activity: foreign hooks remain raw evidence and cannot add foreground
text or close its message. The regression sends foreign final chunks with the
same message id before the valid chunks to prove they cannot poison assembly.
Unbound early display hooks remain raw evidence for transcript hydration;
non-shell-pty streaming keeps its existing behavior.

The P3 fake-Hub streaming helper had also introduced a second blind socket read.
Removing that read preserves the fixture's sole RPC reader, so an append ACK
cannot cause a concurrent Hub command to be discarded. The streaming chunks,
revisions and test cleanup remain unchanged. The full suite includes the new
streaming and UX status cases alongside the promoted-Claude spec.

Validation on this source base:

| Check | Result |
| --- | --- |
| `cargo fmt --all` | PASS |
| `cargo clippy -p remuda-driver -p remuda-node -p remuda-signal -p remuda --all-targets --locked -- -D warnings` | PASS |
| `cargo test -p remuda-driver -p remuda-node -p remuda-signal -p remuda --locked` | PASS: 59 targets, 901 passed, 0 failed, 11 marked ignored; the inventory child fixture is re-executed by its passing parent test |
| `pnpm --dir web test --maxWorkers=2` | PASS: 78 files, 601 tests |
| Web build and API generation | PASS; no generated API diff |
| Web lint | Exit 0; five existing warnings in untouched web files |
| Full Hub e2e on Hub `58580`, web `58589`, upstream `58581`, CI Chromium, no retries | PASS: 26 passed, one existing conditional skip, 4.3 minutes |

The promoted-Claude case passed in 20.8 s. Its captured instance is
`kind=claude`, `mode=promoted`, `launchedBy=user`, `signalTier=hook`, and still
running/idle after two completed turns and two interruptions. The native log
records two `interrupt` events with `by=esc`, matching two journal
`interrupted` lifecycle events. Working assertions completed in 77–82 ms;
idle assertions completed in 1–2 ms after their completion checks. The new
P3 streaming case and all eight UX status cases passed in the same full run.

Final formatting, secret scan, no-tunnel scan and whitespace checks passed.
The run's artifacts were archived and its six untracked Spaces screenshots
removed. All five worker ports were unused afterward, with no remaining native
helpers from this worker's target directory.
