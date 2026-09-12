# Coordinator guide

How a Claude Code coordinator merges work produced by Remuda agents. The
agent-facing surface (create worktree, dispatch, wait, read, stop) is
[`skills/remuda/SKILL.md`](../../skills/remuda/SKILL.md); this document is the
other half — what the coordinator does with a branch once a worker replies
`DONE <sha>`.

The rule behind all of it: **an agent's `DONE` is a claim, not a gate.** The
coordinator re-runs the gates itself, on the exact tree it is about to merge.

## Layout

One worktree per agent, branch `wt/<agent>/<topic>`, never the coordinator
checkout:

```
repo/                       coordinator checkout, on main
remuda-wt/<agent>/          worker worktrees (remuda worktree create)
/tmp/<proj>-target-<agent>/ per-agent CARGO_TARGET_DIR
```

`remuda worktree create <name>` runs `git worktree add -b wt/<name>/work
../remuda-wt/<name> main` and records the absolute path in
`<git-common-dir>/remuda-worktrees.json`. It is reuse-on-repeat: a second call
with the same name returns the existing record instead of failing.
`instance create --worktree <name>` creates or reuses it and sets the
instance's `cwd` and `workspaceId` to that path.

### Per-agent target directories

Every agent gets its own `CARGO_TARGET_DIR`, exported in its brief:

```sh
export CARGO_TARGET_DIR=/tmp/remuda-target-<agent>
```

Cargo takes a lock per target directory. Agents sharing one serialize behind
it — four parallel workers become one, and a long `cargo test` stalls every
other agent's `cargo check`. The cost is disk, which is cheaper than the
wall-clock. Set `CARGO_INCREMENTAL=0` for build agents; incremental artifacts
are large and worthless across one-shot worktrees.

The coordinator keeps its own separate target dir so verification never waits
on a worker's build. `remuda merge --gate` enforces this for itself: it sets
`CARGO_INCREMENTAL=0` and its own `CARGO_TARGET_DIR` (default
`<repo>/target-gate`, override `--target-dir`) rather than inheriting a
worker's. Two coordinators gating at once each need their own.

## Merge with gates

Do not merge on the strength of a worker's report. `remuda merge <branch>
--gate` automates the whole sequence — isolated merge, shared gate,
compare-and-swap onto main, push — and is the default path:

```sh
remuda merge wt/<agent>/<topic> --dry-run --json   # plan
remuda merge wt/<agent>/<topic> --gate --json      # verify and land
```

Read the branch diff first regardless (`git log --oneline
main..wt/<agent>/<topic>`, `git diff --stat main...wt/<agent>/<topic>`): a
diff that has grown beyond the files the task named is the most common
reason to reject, and no gate catches it. Command surface, exit codes, and
report fields are in [`skills/remuda/SKILL.md`](../../skills/remuda/SKILL.md)
and [`dogfood.md`](./dogfood.md).

The gate itself is defined once, in
[`scripts/ci/gate.sh`](../../scripts/ci/gate.sh), and shared with CI — so a
local gate pass and a CI pass mean the same thing. Order, with only
`cargo-test` retrying (once):

```text
./scripts/ci/secret-scan.sh
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
pnpm install --frozen-lockfile / pnpm build / pnpm test   # web/ only
```

Run `./scripts/ci/gate.sh --list` to print the plan as JSON without running
it. To change what the gate checks, edit that file — not a copy here.

Doing it by hand (no `remuda` binary, or a merge needing conflict
resolution): merge into a staging branch rather than `main`, run the gate
commands above on the **merged** tree, then merge to `main` with `merge:
wt/<agent>/<topic> into main` as the subject, matching existing history.

If a gate fails, the branch goes back to its agent with the failing output —
don't fix a worker's crate inside the merge. Independent branches merge in
any order; when two agents touched one file, land the smaller diff first.

Several agents sharing one worktree is a different situation, covered by
[CONTRIBUTING.md](../../CONTRIBUTING.md) "Staged-content verification":
stage only your files, then prove the *index* builds with
`git stash push --keep-index` around the gates.

## Secret scan

Run before every commit, on the tree you are committing:

```sh
just secret-scan          # → ./scripts/ci/secret-scan.sh [paths…]
```

It scans staged content plus the worktree for undeclared secrets and checks
against the hashed denylist in `scripts/ci/private-tokens.sha256`. It is the
first step of `scripts/ci/gate.sh` and a required CI job — but it runs there
on the *merged* tree, after the commit exists. Run it yourself before
committing; on remote workers the pre-commit hook is frequently not
installed, so don't rely on it.

What it protects against, in this repo's history: Hub bootstrap tokens and
device tokens pasted into docs, evidence transcripts committed unredacted,
and personal absolute paths (`/Users/<name>`, `/home/<name>`) or internal
hostnames baked into examples. Normalize those to `$HOME`, `$WORKTREE`, or a
placeholder before committing.

Secrets belong in the MCP server env or a mode-0600 file, never in a prompt,
a brief, or a committed config. `docs/design/remuda-mcp.json` is committed
precisely because it contains no URL and no token; `remuda mcp` resolves both
at runtime.

## Evidence files

A claim about live behavior needs an artifact. Fixture green is not live
green — that distinction is what the dogfood rounds exist to enforce.

Evidence lives in [`evidence/`](./evidence/) and is referenced from the report
that interprets it:

| Kind | Example | Produced by |
| --- | --- | --- |
| Coordinator session transcript | `evidence/dogfood-3.jsonl` | `claude -p --output-format stream-json` |
| MCP workflow transcript | `evidence/mcp-workflow.jsonl` | `scripts/demo/mcp-workflow.sh` |
| UI screenshots + notes | `evidence/ui-pty-1*.png`, `ui-pty-1.md` | manual run |

Rules for committing evidence:

- **Redact before committing.** Tokens, access codes, and absolute personal
  paths out; `[REDACTED]` / `$HOME` in. Then run the secret scan.
- **Keep runtime state out of the repo.** Hub and Node data dirs, logs, and
  worktrees live under `/tmp/…` and are removed afterwards. Only the
  transcript is repository state.
- **Pair evidence with a report** that states what ran, the result, and what
  is still broken — `dogfood-{1,2,3}-report.md` are the model: each has a
  "what ran" list, a parity table, remaining gaps, and a verdict.
- **Record cost and model** for any run that called a paid model.

Bounded-cost convention for live runs: `--model haiku`,
`--max-budget-usd 0.3`, an isolated cwd and `CLAUDE_CONFIG_DIR` under
`/tmp/remuda-*` (mode 0700) so the run cannot inherit the operator's MCP
servers or write session files into their home.

## Port and host hygiene

Concurrent demos collide. The coordinator demo owns `:18080` / `:18787`;
dogfood overrides to `:28080` / `:28787` via `REMUDA_DOGFOOD_HUB_LISTEN` and
`REMUDA_DOGFOOD_NODE_LISTEN`. Pick a high port for anything new, and don't
bind below `:50000` on a shared remote host.

Remote workers write only under their assigned scratch root and clean up
afterwards. Do not upgrade or restart a shared `herdr` a human is using; an
isolated `HERDR_SOCKET_PATH` plus `XDG_CONFIG_HOME` under `/tmp/remuda-*`
gives a headless server that leaves the default session alone.

## See also

- [`skills/remuda/SKILL.md`](../../skills/remuda/SKILL.md) — dispatch surface
- [CONTRIBUTING.md](../../CONTRIBUTING.md) — commit format, crate ownership,
  staged-content verification
- [`dogfood.md`](./dogfood.md) — herdr-parity checklist, D0 acceptance
- [`impl-notes.md`](./impl-notes.md) — dated changelog and current status
