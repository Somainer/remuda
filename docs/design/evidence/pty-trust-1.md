# PTY startup prompt and workspace trust evidence

Verified on 2026-09-13 with real Claude Code **2.1.269**, Herdr **0.9.0**,
and this branch's `remuda dev`, based on `21ac15ae70da40ee62831e80b8c42c839efd43fb`.
Hub listened on `127.0.0.1:59480`; Node listened on `127.0.0.1:59487`.
All Remuda data and the throwaway Claude config were under the worktree's
ignored `.tmp/pty-trust-4/`. The tested Remuda executable SHA-256 was
`01bbfd5a3a6a42b9cf4e676b7aa9014710519d832426310317dbcc25356e0d4b`.

## Real run

The registered workspace was a fresh detached Git worktree at the base commit.
The authenticated throwaway `CLAUDE_CONFIG_DIR` had **zero project trust
entries** before launch. Credentials were supplied locally, never printed or
included in this evidence. The request selected `model: "haiku"` and forwarded
`--max-budget-usd 0.3`; Claude's interactive help describes the budget flag as
print-mode-only, so it is not treated here as a verified PTY spending limit.
No tools were requested by the prompt.

```json
{
  "kind": "claude",
  "driver": "claude-pty",
  "model": "haiku",
  "permissionMode": "bypass",
  "maxBudgetUsd": "0.3",
  "cwd": "<worktree>/.tmp/pty-trust-4/workspace",
  "claudeConfigDir": "<worktree>/.tmp/pty-trust-4/native-home",
  "prompt": "Reply with exactly the single word PONG and nothing else."
}
```

`POST /v1/instances` returned HTTP **200**, command state **accepted**, for
`ins_01a09860-56b3-76f4-8571-f08f3bc2df9d` and command
`cmd_01a09860-56b3-76f4-8571-f091cd0d2257`.
The durable create settlement had no error: its typed outcome was `completed`,
which represents successful resource creation. It never changed to rejected.

The actual blocked screen contained this menu (surrounding path and prose omitted):

```text
Quick safety check: Is this a project you created or one you trust?

❯ No, exit
  Yes, I trust this folder

Enter to confirm · Esc to cancel
```

The adapter derived **`down`, `enter`** from the selected `No, exit` row and
wrote those logical keys through Herdr `agent.send_keys` to the owned pane
`w2:p2` in session `x-trust-59480`. Native clearing, persisted project trust,
and the subsequent response verified that this sequence accepted the real
dialog. No human TTY answer or manual prompt replay was sent.

## Journal excerpts

The following are selected events from the Hub's persisted copy of the Node
journal. Times are UTC on 2026-09-13; sequence numbers are unchanged.

| Seq | Time | Observation |
| --- | --- | --- |
| 3 | 01:27:31.569 | Instance `ready`, connected |
| 5 | 01:27:31.580 | User message `open`, revision 1, status **queued** |
| 6 | 01:27:31.589 | `instance.create` settled successfully, error null |
| 7 | 01:27:35.390 | Herdr `agent_status=blocked` |
| 8 | 01:27:35.493 | **interaction.requested**, blocking, question, carrier `native-tty` |
| 9 | 01:27:35.603 | Interaction `answer-committed`, delivery `written` |
| 10 | 01:27:35.603 | Native diagnostic **trust-dialog auto-accepted (registered workspace)** |
| 11 | 01:27:35.713 | Herdr `agent_status=idle` |
| 12 | 01:27:35.713 | Interaction `resolved`, no longer blocking |
| 13 | 01:27:37.843 | Current Claude **SessionStart recorded**, native transcript located |
| 14 | 01:27:38.299 | Same user message `replace`, revision 2, status **complete** |
| 15 | 01:27:38.471 | Herdr `agent_status=working` |
| 16 | 01:27:41.476 | Herdr `agent_status=idle` |

The queued and completed message ID was
`obj_01a09860-6dfc-743c-a2c2-b85bdcd2f26f`; the interaction ID was
`int_01a09860-7d45-7709-b4c8-542c8a7759d5`.

The native Claude JSONL journal identified by seq 13 contained the following
text records. These are **native transcript excerpts**, not fabricated Remuda
message events; claude-pty currently links that transcript separately from
its normalized lifecycle/TTY journal. Non-text blocks and private paths are
omitted.

```json
{"timestamp":"2026-09-13T01:27:38.307Z","type":"user","text":"Reply with exactly the single word PONG and nothing else."}
{"timestamp":"2026-09-13T01:27:40.553Z","type":"assistant","text":"PONG"}
```

The real terminal also displayed `PONG`. After completion, Hub detail reported
`lifecycle=running`, `activity=idle`, `connectivity=connected`, with no
`lastError`. Hub `running` is its presentation of Node lifecycle `ready`.
There was no failed-instance event or rejected create settlement.

## Startup race covered by the implementation

An earlier real probe accepted the trust dialog but observed a transient
Herdr idle state before Claude's SessionStart hook. Herdr acknowledged a
prompt during that interval, while Claude's eventual editor was empty.
That acknowledged input was not replayed. Claude prompt delivery now requires
both the **current carrier's SessionStart** and Herdr idle/interactive-ready.
The final run above verifies the full trust-to-PONG sequence with that gate.
The hook watcher remains active while a user leaves a dialog pending, and
stale metadata is removed before a new carrier starts.

## Reproduction and checks

With an authenticated throwaway config whose project trust map is empty and
a mode-600 access-code file prepared under `.tmp`, the owned dev process was:

```sh
export CARGO_TARGET_DIR="$(git rev-parse --path-format=absolute --git-common-dir)/../target-x-trust"
export CARGO_INCREMENTAL=0
REMUDA_DATA_DIR="$PWD/.tmp/pty-trust-4/data" \
REMUDA_HERDR_SESSION=x-trust-59480 \
REMUDA_COOKIE_SECURE=0 \
"$CARGO_TARGET_DIR/debug/remuda" dev \
  --hub-listen 127.0.0.1:59480 --listen 127.0.0.1:59487 \
  --access-code-file "$PWD/.tmp/pty-trust-4/access-code" \
  --workspace "$PWD/.tmp/pty-trust-4/workspace"
```

Node config `[node] auto_trust_registered_workspaces = false` or
`REMUDA_AUTO_TRUST_REGISTERED_WORKSPACES=false` leaves the native trust
interaction for the user. Automatic trust requires canonical cwd containment
inside the registered root; a neighboring unregistered worktree and symlink
escape do not qualify. Generic/shell PTY prompts share the Node queue, while
the automatic trust policy applies only to claude-pty.

Automated coverage includes bounded FIFO delivery for all three PTY drivers,
create settlement while queued, both readiness and send-time control races,
pending interaction replies, cancellation, uncertain-send non-replay,
close failure and retry, and commands accepted during close. Driver fixtures
cover exact menu navigation, disabled automatic trust, canonical containment,
and delayed SessionStart. A Hub WSS regression verifies successful create
settlement arriving before its RPC receipt while prompt and interaction stay
visible.

Checks on the implementation used the target directory above and incremental
compilation disabled:

```sh
cargo fmt --all
cargo build --locked -p remuda
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
(cd web && pnpm test)
./scripts/ci/secret-scan.sh
git diff --check
```

Workspace tests passed: **698 passed, 0 failed, 13 existing opt-in/helper tests ignored**.
Workspace all-targets clippy passed with warnings denied. Web tests passed:
**134 tests in 41 files**; the only web change is the generated `queued` wire
status. Formatting, generated-file parity, secret scan, and diff whitespace
checks passed.

The live instance was explicitly closed and reached `exited`. Both owned dev
listeners stopped, Herdr reported no agents or workspaces, and the empty test
server was stopped. All four temporary Git worktrees and the copied Claude
credentials were removed. The queue is owned by the running instance worker;
uncertain sends and pending input from an ended worker are not automatically
replayed on restart.

Final validation includes the scoped-MCP security changes from `origin/main`
at `21ac15ae70da40ee62831e80b8c42c839efd43fb`. Queued input now preserves
each command's human/bot/agent origin, with a regression covering mixed
origins and the default agent origin. The real canary above ran after that
integration, using the same executable source as the successful final checks.

A separate preliminary attempt in this final environment was rejected by
Herdr `agent.start` with `agent_pane_busy` before Claude launched. That genuine
launch failure remained rejected, as required. The successful canary used a
new instance with the still-untrusted config; no possibly delivered prompt
was replayed.
