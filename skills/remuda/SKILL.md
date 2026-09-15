---
name: remuda
description: "Dispatch and drive coding agents (claude, codex, grok, agy, gemini) on Remuda Hub/Node hosts from a coordinator session. Use when the task is to run work in isolated git worktrees on other agents — create worktree, create pty instances, deliver task briefs, wait for DONE, read screens, stop — via the remuda CLI or the remuda_* MCP tools. Not for ordinary local shell work."
---

# Remuda

Remuda runs native coding-agent CLIs on Hub-connected Nodes. One worktree per
agent, one PTY instance per worktree, a task brief in, a `DONE <sha>` line out.

Two interchangeable surfaces. Use whichever this session has:

- **CLI** `remuda instance|worktree|fleet|merge …` when you can run shell.
- **MCP** `mcp__remuda__remuda_*` when launched with
  `--mcp-config docs/design/remuda-mcp.json --strict-mcp-config`.

The installed binary is the authority for flags — `remuda instance --help`,
`remuda merge --help`. Bare `remuda` has no default subcommand; don't run it
for discovery.

## Coordinator loop

The whole job, per agent. Steps 3–6 repeat until the agent reports.

```bash
# 1. Isolate. One worktree per agent, never the coordinator checkout.
remuda worktree create reviewer --base main
# → {"name":"reviewer","path":"…/remuda-wt/reviewer","branch":"wt/reviewer/work"}

# 2. Write the brief to a file, then start the agent with it.
remuda instance create --name reviewer --kind codex --driver pty \
  --worktree reviewer --prompt-file /tmp/brief-reviewer.md
# → {"instanceId":"ins_…", …}   later commands take ins_… or --name

# 3. Wait for the agreed completion line.
remuda instance wait reviewer --until 'line:(?m)^DONE' --timeout 300000

# 4. On timeout: look before you steer.
remuda instance read reviewer --lines 120 --source screen

# 5. Steer — a nudge, an answer, or a key.
remuda instance send reviewer --text "Print DONE <sha> as its own line now."
remuda instance keys reviewer enter

# 6. Collect the sha from matchedLine, then tear down.
remuda instance stop reviewer --scope instance

# 7. Verify and land the branch (see "Verify and merge" below).
remuda merge wt/reviewer/work --gate --json
```

`wait` returns JSON with `reason` (`condition-met` | `timeout`),
`matchedLine` (the **raw** journal line), `lifecycle`, `activity`, `asOfSeq`.
Exit 0 only means the Hub answered — always read `reason`.

Fan out by running steps 1–2 for each agent, then waiting on each. Waits are
independent; nothing is shared but the Hub.

## Task briefs

Write a file, not a one-line prompt, for anything longer than a sentence.
`--prompt-file` (create) and `--file` (send) read it locally. A brief that
omits these produces an agent that pushes, edits `main`, or never terminates:

- work **only** inside its own worktree; the path is its cwd
- per-agent build dir: `export CARGO_TARGET_DIR=/tmp/<proj>-target-<name>`
  (a shared `target/` serializes every agent behind one lock)
- commit on `wt/<name>/…` with explicit pathspecs, never `git add -A`
- run the gates for the crates touched (fmt, test, clippy `-D warnings`) and
  the repo secret scan
- **never push**, never touch `main` — the coordinator fetches the branch
- finish with a single line: `DONE <sha>`, or `BLOCKED <reason>`

Tell the agent the literal completion line. The `DONE <sha>` convention is
what makes `wait --until 'line:…'` work, and the sha is what you merge.

## wait conditions

| `--until` | Met when |
| --- | --- |
| `line:<regex>` | A journal/screen line matches (see matcher below) |
| `idle` | Ready for input — **not** Hub lifecycle `requested` |
| `blocked` | Approval prompt, question, or interaction event |
| `done` | Run terminal, or lifecycle `closed`/`failed`/`terminated` |

`--timeout` is **milliseconds**: default 30000, hard max 300000 (5 min). A
longer job needs a wait loop, not a bigger number.

**The matcher is bullet-tolerant.** TUIs render output as `• DONE`, so before
applying your regex the matcher strips leading whitespace and *one* list
marker (`•`, `●`, `◆`, `▸`, `▪`, `-`, `*`, `>`). `line:(?m)^DONE` therefore
matches `• DONE` and indented `DONE`. `matchedLine` reports the raw line.
It also ignores the brief's own echo (lines containing `for example`,
`further input`, `tui bullet`) so your instructions can't satisfy the wait.

## read

```bash
remuda instance read reviewer --lines 120 --source screen   # pane text
remuda instance read reviewer --lines 200 --source journal  # lifecycle events
```

`screen` selects tty observations (generic-pty journals a bounded 80-line
snapshot, `completeness=screen-derived`). With no tty observations the
response comes back with `source` = `journal-fallback` — check that field
rather than assuming you saw the pane. `journal` is the right source for
lifecycle, command state, and errors. Both accept `--after-seq` for
incremental reads.

## Drivers and kinds

`--driver pty` (alias of `generic-pty`) attaches a real terminal: screen
reads, `keys`, and interactive TUIs all work. `--driver claude-print` (the
default) is headless — no PTY, so no screen and no keys. **For coordinator
work you almost always want `--driver pty`.**

Each kind launches with its yolo preset appended when absent:

| `--kind` | binary | yolo argv |
| --- | --- | --- |
| `claude` | `claude` | `--dangerously-skip-permissions` |
| `codex` | `codex` | `--dangerously-bypass-approvals-and-sandbox` |
| `grok` | `grok` | `--always-approve` |
| `agy` | `agy` | `--dangerously-skip-permissions` |
| `gemini` | `gemini` | `--yolo` |

Placement: `--host hst_…` **or** `--labels region=sg` (mutually exclusive);
neither means "any online host". Check `remuda instance list` for what exists.

## Host Claude login inventory

`remuda doctor` and host `cli[]` report whether Claude is installed and
whether a native login or API gateway is configured. `auth` is
`gateway-native`, `logged_in`, `logged_out`, or `unknown`. Token and URL
values are never reported.

## Provider scoping (D-021)

Providers created in Remuda are **universal**: Hub stores the encrypted
token and every Node receives the profile through SecretBroker at launch.
A remote host can also have **host-scoped** profiles or bind to **native**
(its own CLI login, no Hub key). Hosts page: 自动 / 原生登录 / 指定
provider. New Session follows the host unless you pick 原生 or 网关.

## Workspace registration (D-023)

Human and Bot operators can list, add, or remove project directories on a running
Node using `GET/POST/DELETE /v1/hosts/{hostId}/workspaces`. Mutation bodies are
`{"path":"/home/dev/projects/example"}`. Agent credentials receive HTTP 403.
The Node validates absolute, existing, canonical directories against its
`workspace_roots` allowlist (default: its user's home directory), and rejects
registration inside another workspace's worktree directory. CLI `--workspace`
entries merge into `workspaces.json` in the Node data directory. Configure
`[node] workspace` (legacy primary), `workspaces` (additional roots), and
`workspace_roots` (allowlist); `REMUDA_WORKSPACE_ROOTS` accepts a JSON array.
Both `remuda node` and `remuda dev` accept repeated `--workspace` and
`--workspace-root` flags. Startup roots use the same validation as registration.

Mutation success means the Node prepared and committed the change and the Hub
persisted its acknowledged registry snapshot. Refresh the list after a lost
connection before retrying. Removing a workspace leaves files and running
sessions intact; new sessions must use a remaining registered workspace.
New Session selects a workspace and accepts an optional relative subpath. API
cwd values beginning with `~` or `$HOME` expand against the Node user's home;
containment within a registered workspace still applies.

## keys

```bash
remuda instance keys reviewer esc
remuda instance keys reviewer down enter
```

Names are validated before any byte is written: `enter`/`return`, `tab`,
`esc`, `space`, `backspace`, `delete`, `up`/`down`/`left`/`right`, `home`,
`end`, `ctrl+<letter>`, or a single character. An unknown name fails the
whole call. Requires a tty-attach driver.

## Fleet and teardown

```bash
remuda fleet send --all "PAUSE commits ~5 min for a history rewrite"
remuda fleet send --labels region=sg --file /tmp/resume.md
remuda fleet send --all --kind codex --idempotency-key pause-1 "PAUSE"
remuda fleet keys --all --host hst_a esc
remuda instance stop reviewer --scope run       # cancel the run, keep the agent
remuda instance stop reviewer --scope instance  # close it
remuda instance rm reviewer                     # same as --scope instance
```

`--all` and/or `--labels` / `--host` / `--kind` select the targets; the
filters intersect and at least one is required. `fleet send` broadcasts a
prompt and `fleet keys` a validated key to every matching **running**
instance — use them for pauses and fleet-wide notices, not per-agent
steering. Output lists every instance plus an `accepted` / `failed` /
`skipped` summary: read it, a partial fan-out is not an error.
`--idempotency-key` makes a retry replay instead of double-sending. Don't
`rm` instances you didn't create unless asked.

## Verify and merge a completed branch

A worker's `DONE <sha>` is a claim, not a gate. After reviewing the branch,
verify and land it with one command — never merge on the report alone:

```bash
remuda merge wt/reviewer/work --dry-run --json   # plan only
remuda merge wt/reviewer/work --gate --json      # verify, then advance main
# --no-push advances only local main; --web forces web checks.
```

`--gate` or `--dry-run` is required. What `--gate` does, in order:

1. Fetches origin. Rejects a source worktree with staged changes or an active
   merge/rebase (including a detached rebase HEAD). Unstaged and untracked
   worker files do not participate in the merge.
2. Pins local main's SHA and the committed source SHA, then merges `--no-ff`
   in a detached worktree under an OS-temp `remuda-mq-*` scratch root (never under the checkout) — your checkouts are never
   touched.
3. Runs `scripts/ci/gate.sh`, the single definition of gate order: secret
   scan → `cargo fmt --all --check` → `cargo check --workspace --all-targets
   --locked` → `cargo clippy … -D warnings` → `cargo test --locked` for
   changed crates and their reverse dependencies (`--affected`, the default).
   Use `--full` for all workspace tests. **Only tests retry** (once, reported as `retried` /
   `attempts: 2`); any other failure stops immediately and later steps report
   `skipped`.
4. When the merge touches `web/` — or with `--web` — adds `pnpm install
   --frozen-lockfile` → `pnpm build` → `pnpm test` inside `web/`.
5. Only on a green gate: `git update-ref refs/heads/main <merged> <expected>`
   (compare-and-swap), then pushes exactly that verified commit to origin
   main, no force, unless `--no-push`.
6. Removes the temporary worktree on every outcome and reports cleanup
   errors. The build cache is kept for the next gate.

Gate builds set `CARGO_INCREMENTAL=0` and their own `CARGO_TARGET_DIR`
(default `<repo>/target-gate`, override `--target-dir`, absolute or relative
to `--repo`); worker Cargo settings are not inherited. Concurrent
coordinators each need their own. Leave the test-only
`REMUDA_MERGE_GATE_COMMAND` unset for real verification — `gateOverride:
true` in the report means a substituted gate ran and the result proves
nothing.

Exit codes: `0` success or dry-run, `1` gate/operational failure, `2` merge
conflicts (listed in `conflicts`), `3` main CAS lost. **Read `mainUpdated`
and `pushed` before retrying** — a failed push can leave local main already
advanced. A CAS loss means someone else moved main: re-inspect it, don't
retry blind. Fetch never silently fast-forwards local main.

`--dry-run` inspects local refs and worktree safety and prints the plan. It
does not fetch, merge, run the gate, update refs, or push — so it proves
neither that the merge is conflict-free nor that the gate would pass.
`--json` puts one object on stdout (progress to stderr) with `status`,
`exitCode`, `expectedMain`, `source`, `merged`, `mainUpdated`, `pushed`,
`conflicts`, and per-step `name`/`status`/`durationMs`/`attempts`/`retried`.

MCP `remuda_merge` takes `branch`, `gate: true` or `dryRun: true`, plus
optional `repo`, `targetDir`, `web`, `noPush`, `affected`, `full`. It runs git **on the MCP
server's machine**, not through Hub, and returns the same JSON as text and
`structuredContent`; a nonzero `exitCode` sets `isError: true`.

The `cargo-test` step reports selected `crates` and a `selection` reason.
Docs-only changes skip tests; shared build inputs select the full workspace.
See `docs/design/merge-affected.md` for selection and dry-run semantics.

## MCP equivalents

| CLI | MCP tool |
| --- | --- |
| `worktree create` | `remuda_worktree_create` |
| `merge <branch> --gate` / `--dry-run` | `remuda_merge` (`branch`, `gate` / `dryRun`, `web`, `noPush`, `repo`, `targetDir`) |
| `instance create` | `remuda_instance_create` |
| `instance list` | `remuda_instance_list` |
| `instance send` | `remuda_instance_send` (`text` or `file`) |
| `instance wait` | `remuda_instance_wait` (`until`, `timeoutMs`) |
| `instance read` | `remuda_instance_read` (`lines`, `source`) |
| `instance keys` | `remuda_instance_keys` (`keys: []`) |
| `instance stop` / `rm` | `remuda_instance_stop` (`scope`) / `remuda_instance_rm` |
| `fleet run` / `fleet send` | `remuda_fleet_run` / `remuda_fleet_send` |
| `fleet keys` | `remuda_fleet_keys` (`keys: []`, `all` / `labels` / `hosts` / `kinds`) |

Arguments are camelCase (`timeoutMs`, `promptFile`, `workspaceId`,
`afterSeq`). Claude Code exposes them as `mcp__remuda__<tool>`. Paths in
`file` / `promptFile` are read by the `remuda mcp` process, so they must
exist on the machine running it.

`remuda mcp` resolves the Hub itself — `--hub` / `REMUDA_HUB` /
`$REMUDA_DATA_DIR/dev-hub/listen` / `./data/dev-hub/listen` / `:18080` when a
`bootstrap-token` or `access-code` file exists, else `:8080`. Token:
`REMUDA_TOKEN`, else `REMUDA_BOOTSTRAP_TOKEN` or the dev access-code file.
After `remuda dev` the committed config works unedited; it holds no URL or
secret. Never put a token in a prompt. See `docs/design/remuda-mcp.md`.

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

## See also

- `docs/design/coordinator-guide.md` — merging with gates, per-agent target
  dirs, secret scan, evidence files
- `docs/design/remuda-mcp.md` — `--mcp-config` and runtime resolution
- `docs/design/dogfood.md` — herdr-parity checklist and D0 status
