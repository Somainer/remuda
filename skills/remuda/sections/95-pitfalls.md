## Pitfalls

Learned from the dogfood-1..3 acceptance runs
(`docs/design/dogfood-{1,2,3}-report.md`):

- **`^DONE` alone misses TUI output.** dogfood-2 wrote both DONE files and
  still timed out: the pane said `• DONE`. Fixed by the bullet-stripping
  matcher — but only with `(?m)`, so `^` anchors per line.
- **Agents bury the completion line in prose.** In dogfood-3 grok's first
  wait timed out; one `send` of "Print DONE as its own line now" made the
  retry match immediately. Budget one nudge per agent rather than a longer
  timeout. Codex matched on the first wait.
- **Confirm work, don't infer it from a timeout.** dogfood-2's workers had
  finished; only the regex failed. On timeout, `read --source screen` and
  check the worktree before concluding anything failed.
- **Creation is not readiness.** The brief is queued until the pane is live
  (dogfood-1 lost it to `control unavailable` before that was fixed). Hub
  lifecycle can sit at `requested` while the Node is running, so treat
  `requested` and snapshot `activity=idle` as unproven — prefer `wait
  --until line:` or `blocked` over trusting a status field.
- **First launch can block on a dialog.** Folder-trust and
  bypass-permissions prompts stop the agent before it reads the brief.
  `read --source screen`, then answer with `keys` (`down`, `enter` — note
  the default button is often the refusing one).
- **`stop` doesn't reclaim the herdr workspace.** Panes and tabs accumulate
  across runs; expect leftovers and `driver shutdown did not settle` on Node
  exit.
- **Give each agent its own `CARGO_TARGET_DIR`.** Parallel agents sharing
  one target dir block on the same lock.
- **Screen is bounded.** Last ~80 lines per snapshot. Ask agents to print
  conclusions, not to scroll; use the journal for history.
- **A green `DONE` is not a green gate.** Workers run crate-scoped checks;
  `remuda merge --gate` runs the workspace gate on the *merged* tree. Land
  through it rather than trusting the worker's own test run.
- **A refusal tells you what to do next — read it.** A Hub error body can
  carry a `reason` (`grant-required`, `out-of-scope`, `not-owner`,
  `operator-only`, `agent-route`, `approval-required`, `stale-caller`,
  `approval-invalid`, `delivery-unknown`) and a `next` step — an `action`
  plus a `hint` — which the CLI prints under the message. Do not retry a
  refusal blindly: a missing grant or scope does not change between attempts.
  Do what `next.action` says — `ask-owner` report the blocked action and ask
  the owner, `await-approval` wait for the pending approval then retry with
  its `approvalId`, `read-then-decide` read the command state before
  re-sending, `fix-request` correct the request, `stop` end the turn.
