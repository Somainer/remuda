# Affected tests in the coordinator gate

`remuda merge <branch> --gate` defaults to `--affected`. It keeps the secret
scan, formatting, workspace check and workspace clippy gates, then runs
`cargo test --locked -p <crate> …` for changed crates and their transitive
reverse dependencies. `--full` restores `cargo test --locked --workspace`.
Tests retain the single automatic retry; web checks retain their existing
change detection and `--web` override.

The candidate worktree supplies both the gate script and `cargo metadata
--format-version 1 --locked --all-features`. Selection compares the pinned
expected main commit to the actual merge result. The resolved dependency
graph includes optional, development, build and platform dependencies; only
workspace members become test targets. Package IDs, rather than dependency
names, connect edges, including renamed dependencies and local path crates.

Changes outside known crate directories conservatively select the whole
workspace, except documentation, skills, web files and root Markdown files.
Workspace manifests, the lockfile, toolchain settings, shared scripts and
removed crate directories therefore request a full test set. A crate's own
documentation still selects that crate. A docs-only merge skips the Cargo
test step and keeps the other Rust checks. Metadata or Git failures stop
the gate instead of silently selecting no tests.

The `cargo-test` step's JSON contains `crates` (sorted selected workspace
names) and `selection` (the selection reason), together with its execution
status, duration and retry fields. The human report prints the same names.
A skipped step lists its planned targets, if any; it does not claim they ran.
Dry-run selection uses the current checkout's metadata and the source ref,
so it is provisional until the actual merge has been constructed.

CI continues to use full tests by calling `scripts/ci/gate.sh` without a test
selection flag. The command calls that same script with `--affected --base
<expected-main>` or `--full`. MCP `remuda_merge` accepts `affected` (default
true) and `full`; `full: true` takes precedence. The Python graph tests run
in CI, and CLI integration tests use real Cargo metadata for a tiny local
workspace while substituting gate executables.
