# Evidence: M1 batch 5b — `remuda watch/worker/report` (co-watch)

Date: 2026-09-16. Branch: `wt/co-watch/watch-worker-report`.
Design: [coordinator-hierarchy.md] §1.1 goal 6, §2.2, §2.4, §4.6, §5.3.
Behaviour spec productised: `coord-scripts/coord-watch-all.sh` (screen watcher)
and `coord-scripts/remote-respawn.sh` (same-worktree respawn). Predecessor:
[coordinator-5a-dispatch.md](coordinator-5a-dispatch.md).

[coordinator-hierarchy.md]: ../coordinator-hierarchy.md

This batch completes the **watch** half of the human coordinator's
dispatch→watch→gate→land→retire loop using only `remuda` verbs. The CLI never
opens an ssh session; every intervention is Hub → Node → `instance.send` /
`tty.write`.

## What shipped

### Verbs

| Verb | Surface | Hub route |
|---|---|---|
| `remuda watch` | `[--project p] [--once\|--follow] [--json] [--interval-secs N] [--stall-mins N]` | `POST /v1/workers/observe` |
| `remuda worker nudge` | `<name> [--text …]` | `POST /v1/workers/{id}/nudge` |
| `remuda worker answer` | `<name> <enter\|esc\|1..9\|text>` | `POST /v1/workers/{id}/answer` |
| `remuda worker switch-model` | `<name> <model-id>` | `POST /v1/workers/{id}/switch-model` |
| `remuda worker resume` | `<name> [--handback …]` | `POST /v1/workers/{id}/resume` |
| `remuda worker replace` | `<name>` | `POST /v1/workers/{id}/replace` |
| `remuda worker stop` | `<name>` | `POST /v1/workers/{id}/stop` |
| `remuda report` | `[--project p] [--for-owner] [--json]` | roster + mergequeue reports |

Plus an additive `remuda dispatch --driver <claude-pty|shell-pty|claude-print>`
override; default carrier selection (5a) is unchanged.

### Classification

The roster-driven watcher reads each active worker's visible screen through
the Node `tty.screen` RPC (the c-native3 instance screen-read route) and
classifies it with pure, model-free rules in `remuda-protocol`
(`classify_screen`):

- **working** – running normally.
- **done `<sha>`** – a freshly-read `DONE <7-40 hex>` line newer than the
  roster's known tip.
- **blocked `<reason>`** – a fresh `BLOCKED <reason>` line.
- **idle-api-error** – the agent is idle *and* the screen shows
  `API Error` / `Connection closed` / `Retrying` (a 429/retry loop that needs a
  nudge). A retry blip during a busy turn is **not** this class.
- **stalled** – a single turn busy with an unchanged screen past the project
  `stallThresholdMins` (default 30, matching "a single API turn running for
  >30 min with no process activity").
- **gone** – host offline, terminal lifecycle, or no live carrier.

Echo suppression (the exact `coord-watch-all.sh` practice): brief-contract
prose and placeholders (`<sha>`/`<reason>`, "reply on one line", …) never match,
and a DONE/BLOCKED equal to the roster's already-known tip is treated as an
echo (a resumed/rescrolled session re-shows its old report). Only a **new**
DONE/BLOCKED moves the durable lifecycle `state`; idle/stalled/gone stay
point-in-time on the new `watch` roster field — consistent with I1 (DONE is a
claim, not a gate).

During the live run below the headless carrier returned a **raw VT ring tail**
(cursor-positioned, width-padded repaint rows with no newline boundaries)
rather than an emulated row grid. The classifier therefore has two modes:
emulated grids keep the start-of-line anchor; raw mode locates the strict
7–40-hex `DONE` token anywhere in the concatenated tail (the grep semantics),
with the same echo/placeholder guards. This does not touch `shell_pty.rs` (a
file this batch does not own).

### Persistence (additive on the 5a roster)

`WorkerRoster` gains `watch` (`WorkerWatch`: status, detail, echo baselines
`lastDoneSha`/`lastBlocked`, `lastScreenDigest`, `lastActivityAt`),
`lastNudgeAt`, `resumedFrom`, `replaceCount`. Schema + OpenAPI + generated web
client/types were regenerated.

### Intervention semantics

- **nudge** delivers the "continue where you left off" prompt as a
  `nudge.md` file attachment (never inline), and is throttled by the project's
  `nudgeThrottleMins`.
- **answer** encodes `enter`/`esc`/digits/free-text to PTY bytes in the
  protocol (`encode_answer_key`) and writes them via `tty.write`; free text is
  submitted with a trailing CR.
- **switch-model** runs esc ×2 → `/model <id>` → Enter, but the confirming
  Enter is sent **only** when the bottom of the screen actually shows the
  switch/confirm dialog; otherwise the model is left unchanged and the command
  reports `confirmed:false` for the operator to answer manually. No blind Enter.
- **resume** uses the D-026 native `instance.resume` (terminal) when a native
  session id is known; otherwise it relaunches a fresh agent in the same
  surviving worktree (the harness-equivalent of `claude --continue`), repoints
  `instanceId`, records `resumedFrom`, and re-delivers the last brief as a
  `handback.md` file with a state-loss preamble.
- **replace** retires (force) then re-dispatches the same brief/name on a
  **fresh branch slug** (retire intentionally keeps the old branch) and bumps
  `replaceCount`.
- **stop** closes the instance but does not reclaim the worktree/target dir.

## Tests

- `remuda-protocol/src/worker.rs` — 13 classifier/encoder unit tests: new DONE,
  known-tip echo, brief placeholder, BLOCKED + dedupe, idle-after-API-error
  (idle vs busy), connection-closed, stalled window, gone (offline/closed),
  raw-ring DONE without a line boundary, answer-key encoding.
- `crates/remuda-hub/tests/watch.rs` — 11 integration tests against a
  scripted-screen fake Node WS: every roster classification transition (incl.
  stalled via an aged activity stamp through a test-only seam) plus nudge file
  transport + throttle, answer `tty.write`, switch-model confirm gate (both
  directions), resume relaunch + brief re-send, replace retire/re-dispatch,
  and stop-without-reclaim.
- `crates/remuda/tests/watch_cli.rs` — 5 CLI tests against the in-process Hub
  and scripted-screen fake Node: `watch --once/--follow`, every classification
  surface, the worker verbs, and `report --for-owner` change-driven emission.

Gates run green: `cargo test -p remuda-protocol/-p remuda-node/-p remuda-hub/-p
remuda`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all`, `just gen-types --check`, and `pnpm gen:api` + `pnpm
typecheck`. The OpenAPI route-coverage test covers all seven new routes.

## Live run on this host (dispatch a fake-harness worker, watch it to done, retire it)

Isolated scratch `TMPDIR=/tmp/remuda-mq-5b`; a real git repo registered as the
Node workspace; isolated XDG/CLAUDE config; `REMUDA_CLAUDE_BIN` pointed at a
wrapper around the deterministic `fake-harness` binary (never the real claude);
an isolated `REMUDA_HERDR_SESSION=remuda-5b-isolated`; the real outbound link
was not used (loopback `remuda dev`, Hub `127.0.0.1:58582`). Scratch was
removed afterwards.

The scenario worker runs a long tool call, then prints `DONE 9f3807e59491`.

```text
$ remuda dispatch --project prj_… --brief brief.md --name c-final5b \
      --harness claude --driver shell-pty
{ "worker": { "name": "c-final5b", "branch": "wt/c-final5b/brief-md",
  "worktreePath": "/tmp/remuda-mq-5b/remuda-wt/c-final5b",
  "portBlock": "58600-58609", "state": { "state": "working" } } }

$ remuda watch                       # mid-task
NAME       STATUS   SHA/REASON  DETAIL
c-final5b  working  -           -

$ remuda worker nudge c-final5b     # delivered as an nudge.md file attachment
… "attachments": [{ "name": "nudge.md", … }] …   lastNudgeAt set

$ remuda worker nudge c-final5b     # immediate repeat is throttled
hub HTTP 409: nudge throttled for another 898s (project policy)

$ remuda watch --follow --interval-secs 1
NAME       STATUS   SHA/REASON      DETAIL
c-final5b  working  -               -
c-final5b  done    9f3807e59491     DONE 9f3807e59491
watch: every worker is done or retired       # exit 0

$ remuda report --for-owner
report: nothing new for the owner            # a clean DONE needs no owner action

$ remuda retire c-final5b
{ "worker": { "name": "c-final5b",
  "state": { "state": "retired" },
  "watch": { "status": "done", "sha": "9f3807e59491",
             "detail": "DONE 9f3807e59491", "lastDoneSha": "9f3807e59491", … } },
  "node": { "worktreeRemoved": true, "targetRemoved": true, "reclaimedBytes": "0" } }
```

Post-run checks: the managed worktree
`/tmp/remuda-mq-5b/remuda-wt/c-final5b` and target dir
`/tmp/remuda-mq-5b/remuda-target/c-final5b` were both removed and
`git worktree list` no longer registered them; no `remuda dev` / fake-harness
process was left running. No ssh, tunnel tooling, `deploy/`, or real herdr
session was touched.

## Notes / follow-ups (not blockers)

- The D-028 native carrier does not wire the per-instance hook overlay when a
  binary override (`REMUDA_CLAUDE_BIN`) is used, so in this isolated run a
  second interactive steer was not needed: the worker finishes in one turn and
  `watch --follow` catches the DONE. Multi-turn native-carrier hook delivery is
  exercised elsewhere (D-028 acceptance); `worker resume/replace` re-brief
  through file `instance.send` regardless.
- Gate/land merge reports are read read-only from the mergequeue on-disk
  location (the gate files this batch does not own); when `r-mergequeue` lands a
  Hub-side report, `remuda report` can key off it without changing the verb.
