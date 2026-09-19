# computer-use per-launch capability: materialization, gates, and live-probe status 2

Date: 2026-09-19.
Scope: task `c-cua-launch` — the explicit opt-in `capabilities: ["computer-use"]`
on `InstanceSpec`, per-instance delivery (skill tree + MCP config + child env),
the CLI/Hub/Node refusals, and the state of the D-045 merge gate (Q1).
Source: code in this worktree and the tests listed below; no live macOS probe
was run on the build host.

Design references: [codex-cua.md](../codex-cua.md) §2–§4,
[D-045](../decisions.md).

## What shipped

Wire surface:

- `InstanceSpec.capabilities: Vec<String>` on
  [`launch.rs`](../../../crates/remuda-protocol/src/launch.rs) — `#[serde(default)]`,
  so an older Hub reads "no grant" rather than failing; regenerated
  `protocol.schema.json` + `web/src/types/generated.ts` via `just gen-types`.
- `CreateInstanceRequest.capabilities` (remuda-node) and the Hub
  `POST /v1/instances` / `/v1/workers/dispatch` bodies carry it; the Node's
  `apply_spec_launch_fields` copies the Hub-stored spec field verbatim.

Materialization (all in
[`crates/remuda-driver/src/launch/skills.rs`](../../../crates/remuda-driver/src/launch/skills.rs)
+ [`materializer.rs`](../../../crates/remuda-driver/src/materializer.rs)):

- The five skill files are embedded in the binary (`include_str!`); a
  granted claude launch with a **managed** home writes them to
  `<native_home>/skills/codex-computer-use/**` at 0600, dirs 0700.
- An inherited operator home (`native_home_managed == Some(false)`) gets **no**
  skill bytes; delivery falls back to leg (b) only.
- Both launchers are written to `<launch_dir>/cua/scripts/` (0755) and
  `<launch_dir>/mcp-cua.json` (0600) names the cua-repl launcher with
  `env.REMUDA_CAPABILITY_COMPUTER_USE = "1"`.
- claude (any carrier) gets `--mcp-config <launch_dir>/mcp-cua.json` exactly
  once; a caller-supplied `--mcp-config` collides and is refused.
  `--strict-mcp-config` is still never emitted.
- codex gets the server as `[mcp_servers."codex-computer-use"]` in the shadow
  `config.toml` (`render_mcp_servers` in
  [`shadow.rs`](../../../crates/remuda-driver/src/launch/shadow.rs)); `[features]
  hooks = true` and every `[hooks.state.*]` trust entry survive (parsed back
  with the `toml` crate in the test).
- `LaunchRecipe.capabilities` / `mcp_servers` / `materialized_files` carry the
  audit record (new `FileRole`s `CapabilityMcpConfig`/`CapabilityScript`/
  `CapabilitySkill`, all `FileLifetime::Launch`), and cleanup shreds them.
- `REMUDA_CAPABILITY_COMPUTER_USE=1` is pushed as a new
  `EnvAllowlistSource::Capability` entry. It deliberately passes the
  `REMUDA_` deny prefix at each spawn site (claude-print `resolve_env`,
  claude-bg `scrub_env`, shell-pty `agent_env`, claude-pty/generic-pty pane
  env): that prefix guards spec- and host-supplied names, not values the
  driver itself attaches after the gates. The variable is a signal, not a
  boundary — the boundary is that no launcher exists when ungranted.

Refusals (each before persistence; messages name the value/host/path):

- Unknown capability string — materializer gate and both CLIs.
- `LaunchOrigin::Agent` — materializer gate (the yolo-gate shape).
- `bypassPermissions` + `computer-use` — materializer gate; Hub create and
  dispatch paths too. Dispatch normally hardcodes `bypassPermissions`
  (`worker_launch_spec`); a dispatched computer-use worker gets `default`
  (host approvals) instead — documented inline as the deliberate Q4 trade-off.
- kind grok/agy/generic/terminal — materializer + CLI + Hub.
- Host not macOS / no `computer-use` cli row / row `installed=false` — CLI
  preflight reads `GET /v1/hosts/{id}` (`os` is a new stored/exposed host
  field; `cli[]` was already exposed); the Hub runs the identical check after
  placement resolution, covering direct API calls and placement-resolved
  dispatch; the Node runs a local-inventory gate in the driver factory
  (`crates/remuda-node/src/computer_use.rs`) against the stale-proof live
  probe.

## c-cua-hostcap interop (what is and isn't landed)

The hostcap task adds the `computer-use` probe row to `crates/remuda-node/src/
inventory.rs`; it had **not landed** when this work was done. Per the brief
("if it has not landed, read the row when present and refuse with
capability-unknown otherwise"):

- The Node gate treats a missing row as "this Node has not reported the
  capability" and refuses — no special-casing in the CLI/Hub.
- The Hub needs the host `os` string; this batch added the smallest plumbing
  for it (`hosts.os` column + migration, `HostInventoryUpdate.host_os`,
  `host_view.os`). When a Node doesn't report `os`, the row check still runs;
  a non-macOS value is its own named refusal.
- The fake Node in `crates/remuda/tests/cua_cli.rs` already speaks the exact
  inventory shape hostcap will produce (`{kind:"computer-use", installed,
  path, auth}` + host `os`), so the two batches interlock without edits.

## Tests

- `crates/remuda-driver/tests/cua_capability.rs` (12): granted/inherited/
  ungranted claude delivery, codex shadow table, all four refusals, argv
  collision, launch-file cleanup, recipe JSON; and the
  embedded-bytes-vs-`skills/codex-computer-use/` digest test (same file set +
  content; fails on either kind of drift).
- `crates/remuda-driver/src/launch/shadow/tests.rs`: codex config keeps
  features/trust while parsing the new MCP table.
- `crates/remuda-testing/tests/cua_capability.rs` (2): real
  `ClaudePrintDriver` spawn of an env/argv-dumping stub — granted child sees
  the handshake and the config path; ungranted sees neither.
- `crates/remuda-node/tests/computer_use_preflight.rs` (5): the pure
  `(kind, os, cli-row)` gate (the workspace forbids `unsafe`, so the live
  facts are kept out of the tested core instead of `set_var`).
- `crates/remuda/tests/cua_cli.rs` (7): `--capability` help text; unknown
  value; grok; row missing; `installed:false` with probed path; non-macOS;
  and the happy-path preflight passing to the Node boundary.

## Q1 live probe — NOT PASSED on this host

The merge gate requires a real remuda-launched claude session on a host where
Codex Computer Use is installed, listing MCP tools. The build host is Linux
with no CUA install, and the hostcap probe (the installed-row producer) is a
separate, unmerged task. The probe therefore could not be performed; the
feature ships refusal paths and materialization behind the explicit opt-in,
default off, exactly as codex-cua.md §8 prescribes for this case. The
remaining verification on a Mac is:

1. Node reports `cli[kind=computer-use].installed = true` and `os = macos`.
2. `remuda instance create --host <mac> --capability computer-use …` starts;
   the child sees `REMUDA_CAPABILITY_COMPUTER_USE=1`, `mcp-cua.json` on argv,
   the skill tree in its managed home, and lists the CUA MCP tools.
3. The hardened `launch-cua-repl.sh` runs under the injected env.

A redacted LaunchRecipe from that run (claude/codex on/off, generated
`mcp-cua.json`, refusal messages) is still to be captured here.
