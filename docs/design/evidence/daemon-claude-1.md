# launchd Claude startup and macOS access boundaries

Verified locally on 2026-09-13 with Claude Code **2.1.270**, Herdr **0.9.0**,
and baseline `9dd7ec7b59cd43ea325c2bbe2404210ffe31ff2c`. The deployed failure
reported in `intranet-enroll-1.md` was reproduced with separate launchd jobs.
The installed `com.remuda.node` service and its data were not changed.

## Isolation and method

The owned `remuda dev` Hub and Node used **59680 / 59687**. Two temporary
LaunchAgents used labels `com.remuda.node.x-dclaude-59680` and
`com.remuda.node.x-dclaude-control-59680`, private data directories under
`/tmp/remuda-driver/x-dclaude`, and separate Herdr sockets/sessions. The current
installer supports a data directory but fixes the service label, so the
explicitly permitted temporary-plist method was used instead of replacing a
unit. Both Nodes exchanged fresh D-018 enroll tokens for persisted host tokens
and connected to the owned Hub over outbound WebSocket transport. This local
loopback test used `ws://`; it does not claim another TLS deployment check.

Each Hub-created Claude request selected `claude-pty`, `model: haiku`,
`delegation: none`, `permissionMode: default`, and `maxBudgetUsd: "0.3"` with:

```text
Reply with exactly the single word PONG and nothing else. Do not use any tools.
```

Claude's interactive budget flag is not treated as a verified spending cap.
Requests were bound to the intended host ID. Raw journals, process samples,
`ps eww` captures, debug traces and tokens remain private; credentials, personal
paths, and hostnames are omitted here. No privacy grant was automated.

## Before: native startup waits on protected user configuration

| Trial | Instance | Observed result |
| --- | --- | --- |
| Background launchd | `ins_01a099ee-c25e-7487-bd60-6afd1ec3659f` | Create accepted 08:42:36 UTC, input queued at seq 3 / 08:44:22, create settled at seq 5; journal stayed at seq 7, no UI or hook, stopped at 08:53:44. |
| Interactive launchd control, same binary | `ins_01a099ec-8dd9-74c1-baac-23cd1855e0a4` | Accepted 08:40:12, input queued at seq 5 / 08:40:18; journal stayed at seq 7, no UI or hook. |
| In-process dev control | `ins_01a099ee-58a0-70cd-a89f-434bd10863a9` | Accepted 08:42:09; SessionStart seq 7 / 08:42:17; queued input delivered seq 9 / 08:42:19; native screen displayed `⏺ PONG`. |

All times are UTC. The Background Node was PID **26300**, PPID **1**, PGID
**26300**, no controlling terminal. Its Claude child was PID **58795**, PPID
**57710**, PGID **58795**, TTY `ttys046`. The Interactive daemon was PID **43627**,
PPID **1**, with Claude PID **44665**, PPID **44518**, TTY `ttys038`.

Both daemon Claude PTYs reported **24 rows × 80 columns** via `TIOCGWINSZ`.
`lsof -a -p <claude-pid> -d 0,1,2` mapped stdin, stdout and stderr to that same
PTY. Therefore stderr was captured in the combined pane stream; there was no
separate native stderr file, and it contained no error beyond the echoed launch
command. `herdr pane read ... --source recent-unwrapped --lines 40` showed only:

```text
workspace claude --permission-mode default --setting-sources user,project,local
  --model haiku --session-id <id> --max-budget-usd 0.3
  --settings <owned-node>/instances/<id>/launch/settings.json
```

Neither `session-meta.json.raw` nor `session-meta.json` existed. Commands targeted
the exact private `HERDR_SOCKET_PATH`; the dev control used
`herdr --session x-dclaude-dev-59680`. No focused/default user pane was targeted.
The daemon Claude environments contained normal `HOME`, `SHELL=/bin/zsh`,
`LANG=C.UTF-8`, `TERM=xterm-256color`, `COLORTERM=truecolor`, a valid user temporary
directory, and Node-scoped `XDG_CONFIG_HOME`. `CLAUDE_CONFIG_DIR`, `TERMINFO`, and
`LC_ALL` were absent. Its PATH contained the Claude installation directory and
system executable directories. In-process dev had locale/terminal metadata
differences but no additional proxy or provider credential variable.

An owned diagnostic pane launched the same native binary with `--debug-file`;
this was a separate diagnostic command, not a change to the Hub launch allowlist.
It completed setup in **102 ms**, then stopped at:

```text
[STARTUP] setup() completed in 102ms
[STARTUP] Loading commands and agents...
```

Process sampling showed the native event loop waiting, rather than consuming
startup CPU. The default Claude home was outside protected folders, but one
user skill symlink reached `$HOME/Documents/<skill-source>`. From the same
launchd-owned Herdr, `/bin/ls` of that directory and `/bin/cat` of `SKILL.md`
both failed to return before explicit **5.005 s / 5.009 s** deadlines. This was
a stalled filesystem operation, not merely an immediate error message.

`log show --predicate 'subsystem == "com.apple.TCC"' --last 15m` correlated the
native children with the responsible daemon executable:

```text
08:40:18.323 service=kTCCServiceSystemPolicyAllFiles preflight=yes
               responsible_pid=43627 accessing_pid=44665
08:40:18.329 authValue=0 authReason=5
08:44:39.006 service=kTCCServiceSystemPolicyAllFiles preflight=yes
               responsible_pid=26300 accessing_pid=58795
08:44:39.013 authValue=0 authReason=5
```

Those preflight denials alone do not identify a blocked file. The bounded file
probes and following single-link comparison establish the access dependency:

| Owned config experiment on the Interactive daemon | Result |
| --- | --- |
| Copy existing global metadata/settings, retain plugins and other skill links, omit the one protected skill link | `ins_01a099f5-0f66-734d-9b01-94b4f8a95dbb`: SessionStart at 08:49:39 (seq 7), queued input delivered 08:49:42 (seq 9), full UI. This copy used a different keychain namespace and reported “Not logged in”; it proves startup, not model execution. |
| Restore only that link in the same owned config | `ins_01a099f7-c095-7329-a8ec-bc78025ff610`: accepted 08:52:25, input queued 08:52:33, journal stayed at seq 7 and no hook/UI until stop at 08:54:52. |

The diagnostic setup did not edit the normal user config or skill links; Claude
performed its ordinary session metadata writes. The experiment also
rules out pane size, hook serialization, fresh onboarding, and scheduling as
the sole cause of this startup stall. It does not prove that every future blank
Claude screen has the same cause.

## Scheduling and implementation

Background policy was a second measured startup penalty: at 139 seconds elapsed,
the daemon had received **15.45 CPU seconds**, priority **4**, and was still
hashing the 207,500,480-byte Claude executable. The Interactive control completed
inventory with about **18.81 CPU seconds** by 45 seconds elapsed, priority **31**.
Changing this policy alone left the native UI blocked, as recorded above.
The generated launchd unit now uses Interactive policy for user-requested WSS
terminal work; no environment, proxy or credential inheritance was broadened.

Filesystem checks now execute in a bounded subprocess from `/`, rather than
performing a potentially blocked pre-exec cwd change. Timeout kills the process
group and does not wait indefinitely for a descendant-held output pipe. Workspace
registration/creation and read-only Git probes share the access diagnostics.
Mutating Git operations retain their previous wait behavior after access checks. On macOS,
Claude settings access, immediate skill entries’ `SKILL.md`, and command/agent
Markdown discovery are checked before binary pinning or Herdr carrier creation.
Unrelated skill fixtures/examples are excluded from the probe. Permission errors and deadlines explain Full Disk Access and the
explicit accessible-config alternative. User skills remain enabled; D-022's
SessionStart gate and user-dialog waiting remain intact.


## After: bounded errors and a native response under launchd

The patched binary ran as PID **81724**, PPID **1**, without a controlling
terminal, under the owned Interactive LaunchAgent. The final production code
was rebased on `63c63bb` before building; the later Clippy correction changed
only two test permission literals.

| Check through the owned Hub API | Result |
| --- | --- |
| Create with the original default config and protected skill link | HTTP 400 in **3.238 s**; explicit Claude configuration access timeout and Full Disk Access guidance; no native carrier created. |
| Create after replacing only the owned registered workspace path with a symlink to a fresh owned Documents fixture | HTTP 400 in **3.149 s**, workspace access timeout and guidance naming the running daemon binary. The fixture and symlink were removed in `finally`, restoring the original workspace. |
| Host doctor during that protected-workspace trial | HTTP 200 in **3.099 s**, `exitCode: 1`, `workspace.access: blocker`, `inventory: null`, actionable remediation. |
| Host doctor with accessible workspace and original protected config | HTTP 200 in **3.229 s**, `workspace.access: ok`, `claude.config-access: blocker`, `exitCode: 1`. |
| Create with the explicit accessible isolated config | Instance `ins_01a09a0f-2ccb-75f5-966a-4bdf0db7093a` reached SessionStart, delivered its queued input, and returned native `PONG`. |

The outbound daemon dispatcher previously fell through to `{"ok": true}` for
`host.doctor`. It now returns the actual doctor report. The Hosts panel renders
its blockers and rejects malformed acknowledgments. Access preflight runs
before inventory reads, so diagnostics do not repeat the filesystem stall they
are meant to explain.

The successful canary used the same isolated copy as the single-link experiment,
with that protected link omitted. The owned test LaunchAgent also set
`CLAUDE_SECURESTORAGE_CONFIG_DIR` to an empty string, allowing the installed native
CLI to use its existing default keychain namespace with the separate config
root. No credential value was extracted, copied, or injected. This was an
explicit test configuration, not a new Remuda environment default or automatic
removal of user configuration.

Final native process evidence:

```text
process       PID    PPID   PGID   TTY
Node          81724  1      81724  none
pane shell    88637  88592  88637  ttys049
Claude        88872  88637  88872  ttys049
TIOCGWINSZ: rows=24 columns=80
stdin/stdout/stderr: same slave PTY
```

`ps eww` confirmed the previous sane HOME/SHELL/LANG/TERM/COLORTERM/TMPDIR and
Node-scoped XDG environment, plus the two deliberate config-selection variables
above. Both `launch/session-meta.json.raw` and `launch/session-meta.json` existed.
The latter named the current native session and transcript under the owned
configuration root.

| Durable journal/native evidence | UTC time |
| --- | --- |
| Create accepted, seq 1 | 09:18:02.093 |
| User input queued, seq 4 | 09:18:11.221 |
| Native SessionStart recorded, seq 7 | 09:18:13.639 |
| Same user input became complete/delivered, seq 9 | 09:18:15.453 |
| Native transcript assistant text: `PONG` | 09:18:17.567 |
| Herdr returned to idle, seq 11 | 09:18:18.605 |

The pane capture showed:

```text
Claude Code v2.1.270
Haiku 4.5 · Claude Max
❯ Reply with exactly the single word PONG and nothing else. Do not use any tools.
⏺ PONG
```

Some runtime-ledger metadata still carries the existing `fake` fixture labels;
those labels were not used as proof of native execution. The separate Herdr
process, hook files, native transcript and screen establish the response. This
check does not claim a structured assistant reply in the Hub PTY journal.

## Cleanup and limits

The successful instance was closed through the Hub API and then observed as
`lifecycle: exited`. All earlier canaries were also observed exited. Both owned
Herdr servers reported empty workspace lists before being stopped. Both
LaunchAgent labels were absent from `launchctl print` after bootout; their
plists were deleted. The final daemon, shell, native Claude and Herdr PIDs were
absent. The dev Hub/Node were stopped, the Documents fixture was removed, and the
isolated copied configuration and task enrollment files were deleted. The
installed `com.remuda.node` service was not reconfigured or restarted.

Remuda cannot grant macOS privacy access. With the original denied skill target,
the fixed behavior is an actionable bounded failure. Successful native startup
requires readable configuration/workspace data, demonstrated here using the
explicit accessible configuration alternative. The probes cover the observed
workspace and user-extension discovery paths; they are not a general promise
that arbitrary future plugin, MCP, keychain or network operations cannot wait.


## Validation

All required gates passed on the rebased source with the assigned private Cargo
target and `CARGO_INCREMENTAL=0`:

- `cargo fmt --all`.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- `cargo test --workspace --locked`: **744 passed, 0 failed, 13 ignored** across
  107 test binaries/doc-test groups. Ignored tests were not claimed as executed.
- `./scripts/ci/secret-scan.sh` and `git diff --check`.
- Web `pnpm test`: **42 files, 143 tests passed**; `pnpm build` and `pnpm lint`
  passed with the existing bundle-size and unrelated lint warnings.

The regression suite covers EPERM, deadline termination, descendant-held pipes,
registration/create rejection, protected-folder path boundaries, missing versus
denied config ancestors, preserving safe/broken skill links, excluding unrelated
nested fixtures and cycles, settings access without returning its contents, and
rejection before native carrier creation. CLI integration verifies the installer
warning for Documents, Desktop and Downloads before any service mutation. A real
Hub HTTP → outbound WSS → Node integration verifies an actual doctor blocker;
Hosts tests cover display, retry, offline state and malformed reports.
