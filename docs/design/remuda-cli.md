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
credential-marker heuristics used by Node inventory (`logged_in`, `logged_out`,
or `unknown`). They do not validate credentials with providers or start models.
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
worktree under the selected repository's `data/tmp`. A caller checkout whose
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
Clippy, workspace tests (one automatic retry), and optional web install/build/
tests when forced or when `web/` changes. It uses its own target directory
(default `<repo>/target-gate`) and `CARGO_INCREMENTAL=0`.

Successful merge commits use `merge: <branch> into main` by default.
`--message` overrides the message while retaining an appended gate summary.
The summary records every gate step's status, duration in milliseconds, attempt
count, and `retried` flag, including skipped web checks. The command records this
body after the checks, verifies the tree is unchanged, then uses the final commit
for the main compare-and-swap and push. An already-merged branch does not amend
an existing main commit. Step commands, machine paths and credential values are
not copied into the commit message.

Merge JSON still includes per-step statuses/timings and whether main was updated
or pushed. Exit codes remain 0 success, 1 gate/preflight/push failure, 2 merge
conflict, and 3 lost main compare-and-swap. Temporary worktrees are removed on
success and failure. A push failure can occur after a successful local CAS;
inspect `mainUpdated` before retrying. `--no-push` skips only the push.

## MCP tools

| Tool | Arguments | Result |
| --- | --- | --- |
| `remuda_doctor` | Optional `host` or `local`, optional local `dataDir` | Same structured diagnostic report; blockers set `isError: true` |
| `remuda_worktree_rm` | Required `name` (name or explicit path), optional `repo`, boolean `force` | Removal result including retained branch; same protections as CLI |
| `remuda_merge` | Required `branch`, `gate: true` or `dryRun: true`; optional `web`, `noPush`, `repo`, `targetDir`, `message` | Same merge report and exit semantics |

Example tool arguments:

```json
{"name":"remuda_doctor","arguments":{"host":"hst_example"}}
{"name":"remuda_worktree_rm","arguments":{"name":"worker","repo":".","force":false}}
{"name":"remuda_merge","arguments":{"branch":"wt/worker/task","gate":true,"message":"merge: finish host onboarding"}}
```
