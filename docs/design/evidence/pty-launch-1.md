# PTY pane availability before native agent launch

Verified 2026-09-13 on macOS with Claude Code **2.1.270**. Pristine baseline:
`5818f7ee27ed9f10d5a61b9a220f7294832dc771`. The baseline failed twice among three
back-to-back cold launches; all six patched launches reached native `PONG`.

## Ordering and availability audit

Inspected `git log -p 5818f7e^..5818f7e -- crates/remuda-driver crates/remuda-node`,
`git show f612cd9 -- crates/remuda-driver crates/remuda-node`, and the Claude diff
from `96ec888`. References below use baseline source line numbers.

| Driver | Pre-start ordering | D-022 effect |
| --- | --- | --- |
| Claude PTY | Materialize and inject hook; ensure Herdr; create workspace; split; record ownership; immediately `start_agent` (`claude_pty.rs:235-319`). | Removes stale `session-meta.json` before pane creation (`:238-244`). Adds no `pane.run`, send-keys, or other pre-start pane command. |
| Generic PTY | Create workspace; split; record; `wait_shell_prompt`; `start_agent` (`generic_pty.rs:408-449`). | Ordering unchanged. Existing heuristic waits up to 8 s for `➜`, `$`, `%`, or `>`, then proceeds even after expiry (`:973-994`). |
| Shell PTY | Allocate `portable-pty`, then `spawn_command` (`shell_pty.rs:106-123`). | File unchanged; no Herdr pane or `agent.start` exists in this carrier. |

Paths in the table are under `crates/remuda-driver/src/`. Trust handling is created
after Claude starts (`claude_pty.rs:388-406`); Node's D-022 input queue also follows
driver start (`crates/remuda-node/src/runtime.rs:699-715`). D-022 did not reorder
the failing operation. Generic's preexisting heuristic delay explains a timing
asymmetry consistent with the report, without proving the cause of any one race.

Herdr source at `1a7c691559bb6ea8ad366bce68f87f8c3f6db098` checks for an existing
agent and an available shell (`src/app/agents.rs:187-195`). On macOS/Linux,
`src/platform/mod.rs:301-311` requires shell PID = foreground process group,
a recognized shell process at that PID, and no other foreground process.
`pane.process_info` exposes these facts (`src/app/api/panes.rs:517-545`), including
`foreground_process_group_id`, which Remuda's typed response previously omitted.
There is no `at_prompt` field in `pane.get`; agent status and prompt glyphs do not
prove shell availability. The process predicate is Herdr's launch authority.

The new shared `pty_launch::start_agent` polls those facts every **200 ms** for
**20 s**, failing closed on missing metadata. Both Herdr-backed drivers use it
through the existing blocked-start wrapper; Generic's glyph heuristic is removed.
Only **`agent_pane_busy`** retries: Herdr rejects it before managed-agent creation
and byte submission (`src/app/agents.rs:217-218`). Timeout, disconnect, blocked,
and not-ready results do not replay; existing exact-pane blocked reconciliation
is retained. A dispatched start retains its native RPC timeout. Readiness probes
are capped at two seconds and the remaining deadline; final diagnostics add at
most two seconds and include the last **32 lines / 4096 UTF-8 bytes** of screen.
Shell PTY keeps its direct local carrier because it never calls Herdr.

## Real before/after journals

Both runs used owned Hub **59580**, Node **59587**, Herdr session `x-launch-59580`,
and workspace `/tmp/remuda-driver/x-launch/workspace`. Each started with a fresh
Node process and stopped test Herdr server. The request body was:

```json
{"kind":"claude","driver":"claude-pty","model":"haiku","permissionMode":"bypass","maxBudgetUsd":"0.3","cwd":"/tmp/remuda-driver/x-launch/workspace","prompt":"Reply with exactly the single word PONG and nothing else. Do not use any tools.","title":"<trial>"}
```

Three immediate POSTs returned HTTP 200 / accepted before launch settled. Three
more were submitted during `cargo build --workspace --all-targets --locked`.
All timestamps below are the Node event's `observedAt`, UTC on 2026-09-13.

```text
before-cold-0: ins_01a09871-4406-7138-91e2-4cb8256965c5, pane w3:p2
seq 1  01:45:54.997  command accepted
seq 2  01:46:01.287  command settled; outcome=rejected; execution=not-dispatched
  driver operation failed: invalid launch spec: herdr agent.start: agent_pane_busy:
  agent target pane w3:p2 is not an available shell
seq 3  01:46:01.293  instance failed; same reason

before-cold-2: ins_01a09871-4531-7117-8f62-20436ec89230, pane w1:p2
seq 1  01:45:55.296  command accepted
seq 2  01:46:01.172  command settled; outcome=rejected; execution=not-dispatched
  herdr agent.start: agent_pane_busy: agent target pane w1:p2 is not an available shell
seq 3  01:46:01.177  instance failed; same reason
```

Baseline recipes materialized around `01:46:00.5924`; Herdr startup attempts were
logged at `01:46:00.594146`. Both failures followed within 0.70 s; these are not
direct pane-creation timestamps. Cold trial 1 (`ins_01a09871-449e-7186-b4ec-94be44927fc6`)
reached ready at seq 3 / `01:46:01.094`, settled completed at seq 6 / `01:46:01.109`,
and delivered queued input at seq 9 / `01:46:05.222`; its native PONG was recorded
at `01:46:07.937`. All three baseline build-load trials succeeded (ready at
`01:46:18.626`, `01:46:19.015`, `01:46:19.125`; native PONG at `01:46:31.573`,
`01:46:31.409`, `01:46:30.783`). That build lasted 27.17 s: load did not always
trigger the race.

The patched Node listened at `01:51:53.933563`. All six requests below settled
completed with error null, delivered the queued input, and reached native PONG.
The second batch ran during an 18.55 s build. No failed-instance event occurred.

| Trial / instance ID | Accepted | Ready | Create settled | Input delivered | Native PONG |
| --- | --- | --- | --- | --- | --- |
| cold-0 / `ins_01a09876-f3b2-7073-8ad5-c487437aa085` | seq 1, 01:52:07.650 | seq 2, 01:52:13.351 | seq 6, 01:52:13.398 | seq 9, 01:52:17.907 | 01:52:19.935 |
| cold-1 / `ins_01a09876-f449-713b-8c77-4080d9d8a66b` | seq 1, 01:52:07.800 | seq 2, 01:52:13.742 | seq 5, 01:52:13.758 | seq 9, 01:52:18.082 | 01:52:20.374 |
| cold-2 / `ins_01a09876-f4dc-7008-b35e-302a9231a96d` | seq 1, 01:52:07.947 | seq 3, 01:52:13.377 | seq 6, 01:52:13.404 | seq 9, 01:52:17.621 | 01:52:19.543 |
| load-0 / `ins_01a09877-3c79-76df-863a-34afa6f1979b` | seq 1, 01:52:26.282 | seq 3, 01:52:34.131 | seq 6, 01:52:34.172 | seq 9, 01:52:38.881 | 01:52:40.854 |
| load-1 / `ins_01a09877-3d35-71a6-b224-74a8c51e718c` | seq 1, 01:52:26.485 | seq 3, 01:52:34.942 | seq 6, 01:52:34.983 | seq 9, 01:52:39.094 | 01:52:41.322 |
| load-2 / `ins_01a09877-3e2d-7196-bd7f-bcbd0fe0210a` | seq 1, 01:52:26.785 | seq 2, 01:52:35.346 | seq 6, 01:52:35.383 | seq 9, 01:52:39.294 | 01:52:41.424 |

PONG text was verified independently in native transcripts located through owned
`session-meta.json` files and Herdr screens (`⏺ PONG`), not inferred from create
settlement. This is a finite live sample; fake-client tests prove wait/retry
behavior. The PTY budget argument is not treated as a verified spending cap.
Raw logs, journals, saved binaries and screens remain in `/tmp/remuda-driver/x-launch`;
machine-specific metadata and credentials are not committed. Binary SHA-256:

- Pristine `remuda-before`: `3f4b667c8db28cdd23de6231a30a0bb3222a180bff4117f62851bca5e966070c`.
- Patched `remuda-after`: `bc8c4622ea6370ea406db7969574a8a390dd0c03fbf03e9494aeb165232714c0` (final production logic before formatting, unused-import cleanup, and test additions).

## Reproduction, checks and cleanup

Use the assigned target with incremental compilation disabled. Preserve the
pristine executable, pair a human device via `/v1/login` using the generated
private bootstrap file, submit the batches above, and capture each
`GET /v1/instances/<id>/journal` plus Herdr screen. Repeat with the patched binary
after closing baseline instances and stopping the owned Node/Herdr server.

```sh
export CARGO_TARGET_DIR="$(git rev-parse --path-format=absolute --git-common-dir)/../target-x-launch"
export CARGO_INCREMENTAL=0
REMUDA_DATA_DIR=/tmp/remuda-driver/x-launch/data \
REMUDA_HERDR_SESSION=x-launch-59580 REMUDA_COOKIE_SECURE=0 \
"$CARGO_TARGET_DIR/debug/remuda" dev \
  --hub-listen 127.0.0.1:59580 --listen 127.0.0.1:59587 \
  --workspace /tmp/remuda-driver/x-launch/workspace
```

Passed: `cargo build -p remuda --locked`; `cargo fmt --all`;
`cargo clippy --workspace --all-targets --locked -- -D warnings`;
`cargo test --workspace --locked` (**705 passed, 0 failed, 13 existing ignored**);
`./scripts/ci/secret-scan.sh`; `git diff --check`.
Seven focused helper tests additionally passed via
`cargo test -p remuda-driver --lib pty_launch --locked`: busy-to-ready, always
busy/no start with bounded screen error, readiness/start busy race, nonbusy-error
non-replay, unsafe/missing process metadata, screen line bounds, and hanging
readiness/diagnostic RPC bounds. Existing Claude/Generic integrations passed.

All twelve live instances were closed via `instance.close` and observed `exited`.
Both owned listeners stopped; Herdr agents and workspaces were empty before
stopping its isolated server. No copied Claude credentials were created.
