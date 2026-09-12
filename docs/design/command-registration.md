# Feature-owned command and route registration

Add CLI arguments, help and execution to the feature's `crates/remuda/src/cmd/`
module. Implement `registry::Entrypoint` for its argument type. One entry in
`cmd/mod.rs`'s `commands!` list declares the module, creates the Clap variant,
and connects dispatch. There is no feature-specific match in `main.rs`.
Existing command groups add subcommands entirely inside their own module.

`registry::Context` carries global `--config` and `--data-dir` options. Loading
configuration is explicit in commands that use it, so `version` remains usable
with invalid or missing config. Services call `registry::service` to share signal
installation and bounded runtime shutdown. Other commands return their own exit
code from `Entrypoint::enter`. Help metadata belongs on the argument type.

MCP tools live in `cmd/mcp/{instance,worktree,fleet,merge,doctor}.rs`. Each
`Tool::new` couples the name, description, schema and async handler in one
registration. Add a tool to its group's `tools()` function; the catalog and
dispatch pick it up together. Use `.report()` for results with an `exitCode`:
the registry exposes `structuredContent` and maps nonzero codes to `isError`.
A new group needs one entry in `mcp/mod.rs`'s `tool_groups!` list. Framing and
JSON-RPC transport stay in `mcp/mod.rs`; generic JSON helpers stay in `args.rs`.

The Hub composition root merges feature routers. Register host, instance,
provider, interaction and TTY endpoints in `hosts.rs`, `instances.rs`,
`providers.rs`, `interactions.rs` and `tty.rs`, respectively. The host module
also composes existing registry endpoints. Existing HTTP/WS handlers remain
where they were to keep this extraction small; new handlers can live beside
their feature routes. The OpenAPI coverage test discovers source modules, so
adding a feature does not require another route-source entry in the test.

Edit `skills/remuda/sections/*.md`, then run `./scripts/gen-skill.sh`. Commit
the section and regenerated skill together. CI checks that the output is
current. See `skills/remuda/README.md` for section ordering and conflict repair.

The CLI tests check registration uniqueness, owned help and existing argument
behavior. MCP tests check unique schemas, callable handlers, structured failure
mapping and both framing formats. New behavior tests belong in the owning module.
