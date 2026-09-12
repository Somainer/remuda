# Remuda

Remuda is a unified remote agent runtime that hosts native agent processes and gives users a shared Web/PWA control surface, with Claude Code as the primary runtime.

The protocol and architecture live in [docs/design](docs/design). The [protocol specification](docs/design/protocol.md) defines the wire types; its implementation feedback records decisions made during bootstrap. Remuda preserves native agent loops, workflows, hooks, skills, MCP, and session storage.

The Cargo workspace contains `remuda-protocol`, `remuda-claude-wire`, `remuda-herdr`, `remuda-driver`, `remuda-journal`, `remuda-node`, `remuda-hub`, and the `remuda` binary. This bootstrap implements protocol data types. The other libraries are compilation scaffolds; `remuda hub`, `remuda node`, and `remuda dev` currently print placeholders.

Rust is pinned to 1.94.1. Run `just build`, `just test`, and `just lint`, or the equivalent Cargo commands in [justfile](justfile). `Cargo.lock` pins the resolved application dependencies; shared dependency requirements live in the root Cargo manifest. Web development is maintained separately; `just web-install` and `just web-build` require its package manifest.

The journal uses `rusqlite` with bundled SQLite: one local database connection can be owned by a dedicated worker, and CI does not depend on the host SQLite version. Future asynchronous callers must keep blocking database work off Tokio executor threads. SQLx's asynchronous pool and database configuration are unnecessary for this initial local journal. No database or journal implementation is included yet. See the [rusqlite documentation](https://docs.rs/rusqlite/latest/rusqlite/) for the binding and feature reference.

Licensed under [MIT](LICENSE). [NOTICE](NOTICE) reserves attribution requirements for future Apache-2.0 source imports.
