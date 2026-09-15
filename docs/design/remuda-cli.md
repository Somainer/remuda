# Coordinator CLI: doctor, agents, worktrees, and merge

This section is kept here while the Remuda skill is being revised separately.
The commands below are also discoverable through `remuda <command> --help`.

## Host preflight

```sh
remuda doctor --local --hub "$REMUDA_HUB"
remuda doctor --host "$HOST_ID" --json
remuda --config remuda.toml --data-dir ./data doctor --local --json
```

`doctor` defaults to this machine. `--host` selects a registered host through its
active, authenticated Hub connection; it does not substitute the coordinator's
local binaries for a disconnected host's inventory. `--host` and `--local` are
mutually exclusive. Remote checks require a Hub and Node with `host.doctor`
support. Both the outbound WebSocket and SSH stdio carriers support the RPC.

The report reuses the Node inventory collector for Claude, Codex, Grok, Agy,
Gemini, and Herdr versions and paths. Agent login states are the same local
credential-marker heuristics used by Node inventory (`gateway-native`,
`logged_in`, `logged_out`, or `unknown`). They do not validate credentials with
providers or start models. Claude `gateway-native` means
`~/.claude/settings.json` has gateway env keys or `apiKeyHelper` (booleans
only; values are never reported). `doctor` also emits `gateway.claude`.
Herdr has no provider login. Cargo and pnpm are also checked on PATH.

Other checks cover the configured data directory, `enrollment.json`,
`node/host-id`, available disk space, configured local listener ports, and
access to the authenticated Hub registry. Identity files are read without being
created or changed; credential contents are never included in the report.
Directory checks inspect filesystem metadata rather than performing a write.
For an active remote Node, the report lists listeners owned by that runtime;
outbound-only Nodes do not need an inbound port. Local preflight recognizes an
already authenticated Hub listener, and Node listeners reported through the
matching persisted host identity. Other bind failures are port conflicts.
The report includes registered hosts with their current Hub link state.

Severity and exit behavior:

| Status | Examples | Exit effect |
| --- | --- | --- |
| `ok` | Binary/version present, valid persisted identity, reachable Hub | None |
| `warning` | Optional CLI, Herdr, cargo or pnpm absent; login unknown/logged out; data directory or identity not initialized; disk measurement unavailable | None |
| `blocker` | No agent CLI installed; malformed/unreadable identity; enrollment permissions not owner-only; invalid/read-only data directory; disk below 1 GiB; occupied required port; inaccessible Hub or selected host | Exit 1 |

Human output prints each finding and registered host link. `--json` returns
`exitCode`, `mode`, `hostId`, `inventory`, `checks` (name/status/message/details),
and `registeredHosts`. Hub URL resolution uses explicit flags and environment,
then the data directory's `listen` file, then configured Node Hub URL or Hub
listener. Device/bootstrap credentials follow the existing CLI resolution;
a configured Hub bootstrap secret is also supported. Use a device token or
access-code/bootstrap file; do not place credentials in URLs.

## Live instances across hosts

```sh
remuda instance ls
remuda instance ls --watch
remuda agents --watch --host "$HOST_ID"
remuda agents --watch --json
```

`instance ls` and `instance list` are equivalent. `agents` is a top-level alias
for the same listing behavior. The table includes name, host, kind, lifecycle,
activity, connectivity, worktree/cwd, and the last available output line.
`--host` filters both snapshots and displayed follow updates.

Watching opens `/v1/follow` without an instance filter and refreshes at most four
times per second. Registry metadata is reconciled every two seconds so new
instances, removals, lifecycle changes and host disconnects appear even without
an output event. Initial output comes from recent journal tails. Reconnects and
follow gaps reload those tails and authoritative metadata; older buffered
follow events cannot replace a newer tail. Failed tail reads are retried on the
next refresh and have `lastLineAvailable: false`. Output lines are bounded and
terminal escape/control sequences are removed.

Disconnected/stale snapshots are explicitly labeled while the client retries
its connection. Ctrl-C or SIGTERM exits cleanly. `--json` emits one JSON object
for a single listing, or compact newline-delimited snapshots when watching,
with `items`, `stream`, and `stale`. Instance rows retain IDs, host link state,
and separate lifecycle/activity/connectivity fields for automation.

## Local Git worktrees

```sh
remuda worktree ls --repo . --json
remuda worktree rm worker --repo .
remuda worktree rm ../remuda-wt/worker --repo . --force
remuda worktree prune --repo . --dry-run
remuda worktree prune --repo .
```

`ls` (`list`) combines Git's linked-worktree registry with names recorded by
`remuda worktree create`. It includes paths, branches, HEADs, primary/locked/
missing/prunable flags. A name or explicit path must identify exactly one
registered tree for `rm`.

Removal preserves the branch. The primary worktree, current worktree, and any
worktree on `main` are protected. Locked worktrees and active merges/rebases
are refused even with `--force`. Without `--force`, Git also refuses staged,
unstaged, and untracked user changes. Force explicitly permits deletion of
those files. The catalog is changed only after successful removal.

`prune` removes stale Git registrations and reconciles missing catalog entries;
it does not remove existing worktree directories or branches. Locked missing
trees remain registered. `--dry-run` previews the catalog entries that would be
removed. Catalog mutations are serialized and files replaced atomically.
These commands operate on the CLI/MCP server's local repository.

## Coordinator merge queue and history

```sh
remuda merge --list --repo .
remuda merge --pending --json
remuda merge wt/worker/task --dry-run
remuda merge wt/worker/task --gate --no-push
remuda merge wt/worker/task --gate --message 'merge: finish host onboarding'
```

`--list` (`--pending`) lists only local `wt/*` branches with commits ahead of
local `main`. Each row reports its ahead count, clean/conflict merge status,
conflicting files, and worktree readiness. JSON includes the pinned main/source
commits and any staged-change or rebase/merge blocker. This is a local queue
inspection: it does not fetch, move refs, or alter existing worktrees.

For `--gate`, fetch and preflight run before creating a detached temporary
worktree under an OS-temp `remuda-mq-<pid>-<n>` scratch root (never under
the checkout; removal is containment-checked against that exact root). A caller checkout whose
HEAD does not contain local main is rejected with a “checkout is behind main”
message. A local main behind or diverged from fetched `origin/main` is also
rejected; the coordinator must update it before retrying.

The executable **and step plan** for a real gate come from
`scripts/ci/gate.sh` in the **merged temporary worktree**. A branch that changes
the script is tested with that version. The caller's file contents cannot
replace it. A missing script in the selected tree is an explicit failure.
Dry-run does not construct a merge or run gate checks; its provisional step
plan comes from the current checkout, so branch changes can alter the real plan.

The gate keeps the shared CI order: secret scan, formatting, workspace check,
Clippy, affected crate tests (one automatic retry), and optional web install/build/
tests when forced or when `web/` changes. It uses its own target directory
(default `<repo>/target-gate`) and `CARGO_INCREMENTAL=0`. `--affected` is the
default and includes reverse dependencies; `--full` tests the whole workspace.
The Cargo test step reports selected crate names. See [affected test selection](merge-affected.md).

Successful merge commits use `merge: <branch> into main` by default.
`--message` overrides the message while retaining an appended gate summary.
The summary records every gate step's status, duration in milliseconds, attempt
count, and `retried` flag, including skipped web checks. The command records this
body after the checks, verifies the tree is unchanged, then uses the final commit
for the main compare-and-swap and push. An already-merged branch does not amend
an existing main commit. Step commands, machine paths and credential values are
not copied into the commit message.

Merge JSON still includes per-step statuses/timings and whether main was updated
or pushed. Exit codes are 0 success, 1 gate/preflight/push failure, 2 merge
conflict (and `base_moved` for `--land`/`--queue`), and 3 lost main
compare-and-swap for the legacy verify-and-advance flow. Temporary worktrees are removed on
success and failure. A push failure can occur after a successful local CAS;
inspect `mainUpdated` before retrying. `--no-push` skips only the push.

## Optimistic verification: `--onto`, `--land`, and `--queue`

`--onto <ref>` verifies a branch merged onto an explicit base — `main` or a
commit sha — without touching `main`. The merge is constructed in the
temporary worktree, the gate runs on that exact merged tree, and the JSON
report records `base`, `head` (pinned branch commit), `tree` (the verified
tree oid) and `merged`. With `--no-push` (the queue's mode) `--onto` never
advances local main; add `--land` to advance main in the same invocation.
The merge commit's sha is stable across the gate (the gate summary is
attached as a `refs/notes/remuda-gate` note instead of amending), so a queue
lane can speculate onto it.

Verification reports persist under the repository's common git directory:

```
<git-common-dir>/remuda/merge-reports/<branch-slug>/<base>.json
```

`wt/a/b` slugs to `wt-a-b`. A `<base>.preparing.json` sidecar appears the
moment the merge commit is constructed (pinned at
`refs/remuda/merge/<slug>/<base>`), before the gate finishes, which is what a
speculating lane waits on.

`remuda merge <branch> --land --onto <ref>` fast-forwards main to a merge
that was verified for **exactly** this `(branch, base)` pair. It never
re-runs the gate: it loads the persisted report, checks the branch head and
the merge's two parents still match, and advances main only when
`main == base`. If main moved meanwhile it exits **2** with status
`base_moved`, `currentMain` in the JSON (and printed to stderr), so the
caller re-verifies onto the new main. Reports produced by `--gate` without
`--onto` keep the historical verify-and-advance flow (the commit is amended
with the gate summary body); `--land` requires `--onto`.

`--queue <branch>...` verifies N branches as an optimistic queue. Lane 1
verifies branch 1 onto main; lane 2 speculatively verifies branch 2 onto
branch 1's not-yet-landed merge commit. When branch 1 lands, branch 2 lands
directly if its base *is* the new main; otherwise (branch 1 failed, or a
third party moved main) it is re-verified onto the real main. Each lane gets
its own target directory (`<target-dir>-lane<N>`, lane 1 unchanged) and a
derived Hub e2e port pair: `--e2e-port-base` (default 58980) and
`+10*(lane-1)` / that `+9`, so lanes are 58980/58989, 58990/58999, …
sharing one Playwright browser endpoint. The web e2e step is serialised
across lanes with an advisory `flock(1)` on `--e2e-lock`
(default `<git-common-dir>/remuda/e2e.lock`); all Cargo steps run fully
parallel. Concurrency defaults to 2 lanes (`--lanes N`).

Queue JSON is the normal merge report plus a `queue` summary: queue order,
for every branch every verification (lane, base, merge, tree, status,
`speculative`, `reused`), the landed sha, and a human `why`. Exit codes:
**0** every branch landed, **1** at least one gate failure (landed branches
stay landed), **2** an unresolved base move (main kept moving past the
re-verification budget; `currentMain` names it).

```sh
remuda merge wt/worker/task --gate --onto main --no-push --json   # verify only
remuda merge wt/worker/task --land --onto <base-sha> --no-push --json
remuda merge --queue wt/a wt/b --gate --no-push --json --lanes 2
```

## MCP tools

| Tool | Arguments | Result |
| --- | --- | --- |
| `remuda_doctor` | Optional `host` or `local`, optional local `dataDir` | Same structured diagnostic report; blockers set `isError: true` |
| `remuda_worktree_rm` | Required `name` (name or explicit path), optional `repo`, boolean `force` | Removal result including retained branch; same protections as CLI |
| `remuda_merge` | Required `branch` (or `queue`), `gate: true` or `dryRun: true`; optional `onto`, `land`, `queue`, `lanes`, `web`, `noPush`, `repo`, `targetDir`, `message`, `affected`, `full` | Same merge report and exit semantics (`base_moved` is exit 2) |

Example tool arguments:

```json
{"name":"remuda_doctor","arguments":{"host":"hst_example"}}
{"name":"remuda_worktree_rm","arguments":{"name":"worker","repo":".","force":false}}
{"name":"remuda_merge","arguments":{"branch":"wt/worker/task","gate":true,"message":"merge: finish host onboarding"}}
```
