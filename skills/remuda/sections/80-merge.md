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
   in a detached worktree under `<repo>/data/tmp` — your checkouts are never
   touched.
3. Runs `scripts/ci/gate.sh`, the single definition of gate order: secret
   scan → `cargo fmt --all --check` → `cargo check --workspace --all-targets
   --locked` → `cargo clippy … -D warnings` → `cargo test --workspace
   --locked`. **Only tests retry** (once, reported as `retried` /
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
optional `repo`, `targetDir`, `web`, `noPush`. It runs git **on the MCP
server's machine**, not through Hub, and returns the same JSON as text and
`structuredContent`; a nonzero `exitCode` sets `isError: true`.
