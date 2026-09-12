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
