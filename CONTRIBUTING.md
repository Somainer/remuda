# Contributing

Remuda is pre-alpha. Small, crate-scoped changes with tests beat sweeping
refactors. Read [docs/design/decisions.md](docs/design/decisions.md) and
[docs/design/protocol.md](docs/design/protocol.md) before changing wire
behavior. If implementation disagrees with those docs, patch the spec and
fixtures first — do not grow a second protocol in code.

## Setup

- Rust **1.94.1** (`rust-toolchain.toml`), edition 2024.
- Optional: `just`, `pnpm` (web), `cargo-zigbuild` + Zig 0.13 (Linux
  cross-compile), a local `herdr` for ignored PTY tests.

```bash
just build && just test && just lint
just web-install && pnpm --dir web test && pnpm --dir web lint
just secret-scan
```

Do not call paid models unless a task explicitly allows it. Then use
`--model haiku`, `--max-budget-usd 0.3`, and an isolated directory under
`/tmp/remuda-<crate>/`.

## Conventional commits

```
<type>(<scope>): <summary>
```

Types: `feat`, `fix`, `docs`, `test`, `refactor`, `chore`. Scope is a crate
or area (`remuda-protocol`, `web`, `deploy`, `scripts`). Subject in the
imperative, no trailing period.

Keep the body short. Trailer lines used in this tree:

```
Co-Authored-By: <name> <noreply@…>
Coordinator: Claude Fable 5.1
```

Do not `git add -A`. Do not `git pull --rebase` on this shared worktree;
retry the commit if another agent landed first.

## Crate ownership

Stay inside the crate (or `web/`, `deploy/`, `scripts/`, `docs/`) named in
the task. Touch the root `Cargo.toml` / `Cargo.lock` / `justfile` only for a
minimal dependency or recipe the crate needs.

| Area | Owns |
| --- | --- |
| `remuda-protocol` | Wire types, schema, golden JSON, `gen-types` |
| `remuda-claude-wire` | Claude print NDJSON / control frames |
| `remuda-acp-wire` | Grok ACP |
| `remuda-herdr` | Herdr client + terminal bridge (no Herdr server copy) |
| `remuda-driver` | Driver trait, materializer, Claude print/pty/bg |
| `remuda-journal` | SQLite journal, blobs, tails |
| `remuda-node` | Instance runtime, local HTTP/WSS, outbound link |
| `remuda-hub` | Hub HTTP/WSS, auth, registry, embed |
| `remuda-testing` | `fake-claude`, `fake-herdr`, fixtures |
| `remuda` | CLI composition root only |
| `web/` | PWA |
| `deploy/` | Image/compose/Caddy/unit templates |
| `scripts/` | Acceptance and CI helpers |

Library code: `thiserror` internally, `anyhow` at process boundaries,
`tracing` for logs, no `unwrap()`. Clippy `-D warnings`. Fixtures live in
`crates/<crate>/tests/fixtures/` with a source note in the file header.

## Staged-content verification

Several agents share one worktree. **Verify what you are about to commit,
not the dirty tree.**

1. `git status --short` and `git diff --stat`.
2. Stage **only** your files. If a shared file also has someone else’s hunks
   (`Cargo.toml`, `Cargo.lock`, a page you both touched), use `git add -p`
   or leave it.
3. Prove the **index** builds:

   ```bash
   git stash push --keep-index -q
   cargo build -p <crate> && cargo test -p <crate>
   cargo clippy -p <crate> -- -D warnings
   git stash pop -q
   ```

   For web: the same stash pattern, then `pnpm --dir web test`, `lint`, and
   `build`. Alternatively `git worktree add /tmp/verify-<crate> HEAD` and
   apply the staged diff there.
4. `just secret-scan` (or `./scripts/ci/secret-scan.sh`).
5. Commit only after that is green. Add lines; do not reformat whole files
   or reorder other people’s code.

## Lifted Apache-2.0 / MIT code

Do not copy AGPL sources (including claudecodeui). Do not fork Paseo or
vibe-kanban as a product.

When you **lift** Apache-2.0 code (vibe-kanban executors, future Herdr
detect, …):

1. Keep the upstream copyright and `SPDX-License-Identifier: Apache-2.0`
   header.
2. Comment the source path, revision, and Remuda modifications.
3. Append a block to [NOTICE](NOTICE): project, copyright, URL, revision,
   lifted path, local path, what changed.
4. Ship Apache-2.0 license text with source and binary distributions of
   those files.

Calling Herdr over a Unix socket is **not** a lift. Copying `herdr/src/detect`
would be.

MIT algorithm rewrites (herdrx fit/touch/viewport) keep the copyright note
in the file header and a NOTICE entry even when the code was rewritten.

## Pull requests

Link the protocol/UI/ADR paragraph you implemented. Fixture or live canary
output belongs in the PR text; fixture green is not live green. New secrets
need a redaction test and a mount story — examples use placeholders only.
