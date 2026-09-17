# Dispatch first-run dialogs: unattended first tool call, and watch never calls a dialog "working"

A dispatched worker must reach its first tool call without a human answering
Claude's first-run dialogs, and `remuda watch` must never print `working` while
one of those dialogs is on screen.

Observed by the owner (2026-09-18 00:15, demo on main `4ae9cda0`): a worker
dispatched with `remuda dispatch --driver shell-pty --model
ark/seed-evolving[1m]` (native profile, delegation none) to the ssh-stdio Node
launched, but its fresh scoped config dir produced **two** Claude first-run
dialogs that parked it until the coordinator answered them by hand:

1. the workspace **trust** dialog ("Quick safety check: Is this a project you
   created or one you trust?", `No, exit` / `Yes, I trust this folder`) for the
   provisioned worktree `<home>/remuda/remuda-wt/<name>`;
2. the auto-mode **outside-reads** question ("Allow reads outside the working
   directories?", `Yes, keep allowing` / `No, block …` / `No, ask again`).

Meanwhile `remuda watch` reported the worker `working` with no detail while a
dialog was on screen.

Verified against the installed Claude Code **2.1.274** binary. All live work
ran under isolated scratch (`/tmp/remuda-mq-o1`, its own `remuda dev` Hub+Node,
a throwaway scoped `CLAUDE_CONFIG_DIR`); the ssh-stdio Node and the real
`remuda-sg` herdr session were never touched. Home paths are redacted to
`/tmp/<home>`, and the model gateway token is only referenced by key name.

## Root cause

`native.rs` gated the pre-trust decision on
`auto_trust_registered_workspaces && cwd_is_registered(cwd, registered_root)`.
A worktree provisioned for a dispatch sits **beside** the registered workspace
root — `<repo>/../remuda-wt/<name>` — so `cwd_is_registered` returned false and
nothing was seeded. The trust dialog then reached the screen, and there was no
seeded answer to the outside-reads question either.

The screen classifier (`remuda-protocol::classify_screen`) had no rule for a
first-run dialog, so a parked worker fell through to `working`.

## The fix

Three independent launch-intent flags, each with its own rule, seeded into the
Node-owned scoped config dir **before** the process starts
(`crates/remuda-node/src/native.rs`, `seed_shell_agent_prelaunch`):

- **trust** — pre-accept the folder-trust dialog for exactly `cwd`, but only
  when `trust_containment` names a recorded containment: the registered
  workspace root, **or** a worktree the Node itself provisioned, verified
  against the `<git-common-dir>/remuda-worktrees.json` catalog
  (`worktree::provisioned_worktree_named`) — explicit provenance, never a
  path-shape guess. The Node journals one line naming which containment
  allowed it.
- **outside reads** — when the project posture is bypass (the dispatch spec's
  `permissionMode: bypassPermissions`), seed the persisted answer equivalent to
  "Yes, keep allowing" (global key `hasSeenAutoModeOutsideReadPrompt`, verified
  against the shipped bundle: the dialog's `allow` branch sets exactly that).
  Under any other posture the dialog is left alone.
- **onboarding** — the existing D-029 wizard seed, unchanged.

An inherited operator config dir is never edited: `seed_shell_agent_prelaunch`
no-ops entirely when `inherit_default_config` is set.

If a dialog reaches the screen anyway, `classify_screen` now returns
`Blocked { reason, line }` with the dialog title, for both carriers and both
screen sources (emulated grid and raw VT ring tail), below report detection and
above idle/stall, and **not** echo-deduped — a modal still on screen keeps
reporting blocked (`crates/remuda-screen/src/dialog.rs` `first_run_dialog`,
folded in through `crates/remuda-hub/src/worker_watch.rs`).

## Live run on this host

Two `remuda dev` stacks on isolated scratch, both native carrier
(`REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`), the real
Claude 2.1.274 binary via a gateway provider profile (`ark/seed-evolving[1m]`),
a real git repo registered as the workspace. The brief:

```
1. Read the file <home>/outside-read-target.txt with your file read tool
   (it lives OUTSIDE your working directory).
2. Run git rev-parse --short HEAD in your working directory.
3. Reply on one line: DONE <sha> or BLOCKED <reason>.
```

### BEFORE (baseline binary from `origin/main`)

`remuda dispatch … --driver shell-pty` (roster driver `shell-pty`) into the
provisioned worktree. The worker parked on the trust dialog:

```
$ remuda instance read (tty.screen)      # lifecycle: ready
 Accessing workspace:
 /tmp/<home>/remuda-wt/c-live-pre
 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). …
 Claude Code'll be able to read, edit, and execute files here.
 ❯ No, exit
   Yes, I trust this folder
 Enter to confirm · Esc to cancel
```

and `remuda watch` called it `working` with no detail — the reported defect:

```
$ remuda watch
NAME        STATUS   DRIVER     SHA/REASON  DETAIL
c-live-pre  working  shell-pty  -           -
```

Answering the trust dialog by hand (arrow-down + Enter through `tty.write`) and
submitting the brief, a second real `claude --permission-mode auto` reading a
file outside its cwd then showed the outside-reads dialog:

```
 Read outside the working directories
  Read(/tmp/<home>/outside-read-target.txt)
 Auto mode and the sandbox read outside the working directories without asking. …
 To undo, remove permissions.blockReadsOutsideWorkingDirectories from your user settings; …
 Allow reads outside the working directories?
 ❯ 1. Yes, keep allowing reads outside the working directories
   2. No, block reads outside the working directories from now on
   3. No, ask again next time
```

Both dialogs are the fixtures pinned in
`crates/remuda-testing/tests/fixtures/claude-{trust,outside-reads}-dialog.txt`.

### AFTER (fixed binary from this branch)

Same dispatch, same worktree layout, gateway delegation (a scoped native home,
the ssh-stdio-Node shape). The Node journaled the provenance line before
launch:

```
INFO build_driver{driver=ShellPty}: folder trust pre-accepted before shell-pty
launch (dispatch-onboarding-1) cwd=/tmp/<home>/remuda-wt/c-live-fix
containment="provisioned-worktree-record"
```

The worker mounted the composer directly — no trust dialog, no wizard — read
two files (including the one outside its cwd, so no outside-reads dialog blocked
it), ran a shell command, and reported DONE, all unattended:

```
$ remuda watch            # first poll, ~5s after dispatch
NAME        STATUS  DRIVER     SHA/REASON  DETAIL
c-live-fix  done    shell-pty  f9f05e0     DONE f9f05e0
```

`grep -c tty.write` over the whole Node log for this run: **0** — nobody sent a
key. The seeded scoped `.claude.json` carried exactly the two intended flags:

```
projects["/tmp/<home>/remuda-wt/c-live-fix"].hasTrustDialogAccepted = true
hasSeenAutoModeOutsideReadPrompt                                     = true
hasCompletedOnboarding                                              = true
```

(Claude then merged its own `projects["/tmp/<home>/repo"]` entry as it ran,
confirming the seed and the CLI's own writes coexist.)

### AFTER, dialog forced: watch reports `blocked`

To prove the classifier, a fixed stack started with
`REMUDA_AUTO_TRUST_REGISTERED_WORKSPACES=false` (pre-trust suppressed) so the
trust dialog reached the screen. The **fixed** `remuda watch` classified it:

```
$ remuda watch
NAME      STATUS   DRIVER     SHA/REASON                                  DETAIL
c-forced  blocked  shell-pty  Is this a project you created or one yo…    Is this a project you created or one you trust?
```

```json
{
 "name": "c-forced",
 "state":  { "state": "blocked", "reason": "Is this a project you created or one you trust?" },
 "watch":  { "status": "blocked", "reason": "Is this a project you created or one you trust?",
             "detail": "Is this a project you created or one you trust?" }
}
```

`blocked` with the dialog title as the reason — never `working`.

## Tests

- `crates/remuda-driver/src/claude_onboarding.rs` — `allow_reads_outside_workspaces`
  writes the global flag once, preserves other keys, upgrades an ask-again
  spelling, refuses a relative dir.
- `crates/remuda-node/src/worktree.rs` — `provisioned_worktree_named` matches
  only the exact recorded path; a subdirectory, a sibling, a missing dir, and
  an unrelated repo all fail closed.
- `crates/remuda-node/src/native.rs` — a provisioned-worktree cwd gets the trust
  flag and (under bypass) the outside-reads answer; an unregistered cwd gets no
  trust flag; an inherited config dir is untouched; the outside-reads answer is
  seeded for bypass only.
- `crates/remuda-screen/src/dialog.rs` / `signature.rs` — both dialog layouts
  detected from their fixtures, survive a soft wrap, and one marker quoted in
  model output is never a dialog; the outside-reads screen reads as `Blocked`.
- `crates/remuda-protocol/src/worker.rs` — a screen dialog blocks with its title
  even while the carrier says busy, stays blocked across echo baselines, and is
  ignored without a live screen.
- `crates/remuda-hub/tests/watch.rs` — both dialog layouts (emulated grid and
  raw VT ring tail) classify `blocked` with the title over `POST
  /v1/workers/observe`, and a modal still on screen does not collapse to
  working.
- `crates/remuda/src/cmd/watch.rs` — `remuda watch` renders `blocked` + the
  dialog title, never the lifecycle state.
- `crates/remuda-node/tests/dispatch_onboarding.rs` — end-to-end (fake harness
  with a config-driven trust gate, native carrier): a dispatched worker reaches
  its first tool call with no keys sent; the scoped config carries the exact-cwd
  trust flag and the bypass outside-reads answer.

## Gates

`cargo test` for `remuda-driver`, `remuda-node`, `remuda-screen`,
`remuda-protocol`, `remuda-hub`, `remuda`; `cargo clippy --workspace
--all-targets -- -D warnings` clean; `cargo fmt --all`.
