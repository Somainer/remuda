# computer-use per-launch capability: materialization, gates, refusal artifacts 3

Date: 2026-09-19 (handback revision).
Scope: task `c-cua-launch` — the explicit opt-in `capabilities: ["computer-use"]`
on `InstanceSpec`, per-instance delivery (skill tree + MCP config + child env),
the CLI/Hub/Node refusals, the handback fixes, and the status of the D-045
merge gate (Q1).
Source: code in this worktree and the tests listed below; no live macOS probe
was run on the build host.

Design references: [codex-cua.md](../codex-cua.md) §2–§4,
[D-045](../decisions.md).

> File ownership note for reviewers: the Hub-side preflight
> (`crates/remuda-hub/src/inventory.rs` gates, `hosts.os` column + migration,
> `host_view.os`) lives in this branch by handback agreement; c-cua-hostcap
> will rebase on it. Hub scope was not extended beyond what that gate needs.

## What shipped

Wire: `InstanceSpec.capabilities: Vec<String>` (canonical — always serialized,
the protocol round-trip snapshot requires the empty array),
`CreateInstanceRequest.capabilities` (Node), Hub create/dispatch bodies.

Delivery (`remuda-driver/src/launch/skills.rs` + `materializer.rs`):

- Embedded skill (5 files) → `<managed_home>/skills/codex-computer-use/**`,
  every created directory **0700**, files 0600. Files in a home outside the
  instance dir (configured `REMUDA_CLAUDE_CONFIG_DIR` / explicit operator dir)
  get `FileLifetime::NativeStore` and survive one instance's cleanup;
  per-instance homes are Launch lifetime and are shredded.
- Launchers → `<launch_dir>/cua/scripts/` (0755); `mcp-cua.json` (0600) with
  the handshake `REMUDA_CAPABILITY_COMPUTER_USE=1` and — for codex — the
  launcher-only `REMUDA_CODEX_HOME` pointing at the operator's real codex home.
- claude: `--mcp-config <mcp-cua.json>` once; `--strict-mcp-config` never;
  caller `--mcp-config path` and `--mcp-config=path` both refused before any
  file is written.
- codex: `[mcp_servers."codex-computer-use"]` in the shadow `config.toml`.

## Handback fixes (2026-09-19)

1. **Codex leg was dead.** The agent's `CODEX_HOME` points at the shadow home,
   while the launchers resolved the vendor app from `CODEX_HOME` → every
   granted codex MCP server exited with app-not-found. Fixed: both skill
   launchers (`launch-cua-repl.sh`, `launch-mcp.sh`) resolve the vendor app
   from `REMUDA_CODEX_HOME` (set only on the MCP server entry, never on the
   agent environment; the agent keeps shadowed state). The real home is
   resolved in the Node/driver process before child shadow env is applied:
   `$REMUDA_CODEX_HOME` override, else `$CODEX_HOME`, else `$HOME/.codex`.
2. **codex delivery is hooks-on shell-pty only.** Granted codex on any
   carrier without a HookSession is refused at the Node factory by name:
   a shadow home populated with only the MCP table would contain no
   `auth.json` / user config, silently losing the operator's codex login.
   The recipe itself writes no codex shadow config; `HookSession::start`
   materializes the complete home (`[features]` + `[hooks.state.*]` + the
   granted server) from `recipe.mcp_servers`.
3. **Q4 is harness-agnostic and refuses, never downgrades.** The materializer
   gate now matches `PermissionMode::Codex { approval_policy: Never }` as well
   as Claude `BypassPermissions`. Dispatch (bot/unattended) refuses
   `computer-use` for **every** harness at CLI and Hub, naming both; the
   previous silent claude `bypassPermissions`→`default` flip is removed —
   `worker_launch_spec` always persists bypass + `capabilities: []`.
4. **CLI preflight fails loud.** `list_hosts()` errors propagate; a capability
   request with no resolvable target host, or a host id absent from the
   inventory, refuses with a `--host` retry direction instead of being skipped.
5. **Deny-prefix hole narrowed.** Spawn sites allow only the exact
   `CAPABILITY_COMPUTER_USE_ENV` name (not any `EnvAllowlistSource::Capability`
   name); the value rides `CapabilityGrant.env` → the allowlist entry's value
   slot, with no hardcoded `1` at the five spawn sites. A test asserts another
   `REMUDA_*` Capability-tagged name stays denied.
6. Skill tree dirs are 0700 (including `skills/` parents); shared-home skills
   are `NativeStore` lifetime; Hub persists `capabilities: []` on ordinary
   creates; the `--mcp-config=path` form collides and refuses pre-write.

## Artifacts

### Refusal messages (exact strings, 2026-09-19)

Unknown capability (CLI + materializer):

```
unknown --capability "desktop"; this build accepts only "computer-use"
```

Agent origin / bypass / kind (materializer):

```
an agent-originated launch may not grant the "computer-use" capability
(origin=Agent); only an explicit human or bot launch may request it

refusing to grant "computer-use" together with unattended/skipped tool
approvals on the same Codex launch: desktop control plus auto-approved
actions has no recovery path; remove one of the two
```

Dispatch (CLI and Hub, both harnesses):

```
refusing --capability computer-use on dispatch for harness "codex":
dispatched workers run unattended, and desktop control without per-action
human approvals has no recovery path; use `remuda instance create
--capability computer-use` for an attended launch
```

Host gate (CLI/Hub), three shapes:

```
host hst_… is linux, but the "computer-use" capability requires macOS;
 pick a Mac with --host

host hst_… has not reported the "computer-use" capability (no computer-use
row in its inventory); update/run a Node that probes it, or pick another
host with --host

host hst_… reports "computer-use" as not installed; the Node probed
/Users/u/.codex/computer-use/SkyComputerUseClient — enable Codex Computer
Use on that Mac or pick another host with --host
```

CLI instance create, no resolvable/known host:

```
computer-use needs an explicit, resolvable target host: no host was selected
for this placement; pick a Mac with --host (or matching labels)

host hst_… is not in the Hub's host inventory (offline or unknown);
computer-use requires a connected macOS host — pick one with --host
```

Node boundary (shell-pty codex, hooks off):

```
the "computer-use" capability for codex on shell-pty requires
REMUDA_PTY_HOOKS=1: without the hook session the granted MCP server cannot
be delivered; enable pty hooks or launch codex on generic-pty
```

### Generated `mcp-cua.json` (granted, redacted)

```json
{
  "mcpServers": {
    "codex-computer-use": {
      "command": "/bin/sh",
      "args": ["/data/…/instances/ins_…/launch/cua/scripts/launch-cua-repl.sh"],
      "env": {
        "REMUDA_CAPABILITY_COMPUTER_USE": "1",
        "REMUDA_CODEX_HOME": "/Users/<operator>/.codex"
      }
    }
  }
}
```

The agent process itself does **not** receive `REMUDA_CODEX_HOME`; it is on
the MCP server entry only, so the launcher finds the vendor app while agent
codex state stays in the shadow `CODEX_HOME`.

### Redacted LaunchRecipe (key fields)

Claude, granted, managed home:

```text
driver: ClaudePrint; kind: claude
argv: … --mcp-config /…/launch/mcp-cua.json
env_allowlist:
  - { name: "REMUDA_CAPABILITY_COMPUTER_USE",
      source: "capability",
      # the handshake value rides a dedicated non-secret slot:
      value: "1", secretRef: null }
capabilities: ["computer-use"]
mcp_servers:
  - { name: "codex-computer-use",
      configPath: "/…/launch/mcp-cua.json",
      env: [ { "REMUDA_CAPABILITY_COMPUTER_USE": "1" },
             { "REMUDA_CODEX_HOME": "/Users/<operator>/.codex" } ] }
materialized_files:
  0600 /…/launch/mcp-cua.json                 CapabilityMcpConfig Launch
  0755 /…/launch/cua/scripts/launch-cua-repl.sh CapabilityScript   Launch
  0755 /…/launch/cua/scripts/launch-mcp.sh      CapabilityScript   Launch
  0600 <native-home>/skills/codex-computer-use/{5 files} CapabilitySkill
        (Launch for per-instance home; NativeStore for a shared dir)
audit:
  credentialRefs: []          # handshake rides value, never secretRef
  prohibitedOptionsChecked: true
  approvalAuthority: RuntimeHost
```

Codex, granted (shell-pty + hooks ON — the only delivered codex shape).

The **LaunchRecipe** itself (what the materializer produces; faithful fields):

```text
driver: ShellPty; kind: codex
argv: []                      # codex never gets --mcp-config
env_allowlist:
  - { name: "CODEX_HOME", source: "nativeHome" }
        # resolved value = <instance>/launch/codex-home (the SHADOW home)
  - { name: "REMUDA_CAPABILITY_COMPUTER_USE", source: "capability",
      value: "1", secretRef: null }
capabilities: ["computer-use"]
mcp_servers:
  - { name: "codex-computer-use",
      env: [ { "REMUDA_CAPABILITY_COMPUTER_USE": "1" },
             { "REMUDA_CODEX_HOME": "/Users/<operator>/.codex" } ] }
materialized_files from the recipe: NO codex-home/config.toml
        # the recipe does not write a partial shadow home; only HookSession does
```

The shadow value and the `[mcp_servers.codex-computer-use]` table are applied
**after** the recipe, by `HookSession::start` →
`launch::shadow::materialize_codex` (session.rs), which reads
`recipe.mcp_servers` and writes the complete shadow config at
`<instance>/launch/codex-home/config.toml` (`[features]` + `[hooks.state.*]` +
the granted server). The `child_env_with` shims export the recipe's
`CODEX_HOME` (shadow) to the child, while the MCP server entry's
`REMUDA_CODEX_HOME` (real home) reaches only the launcher process. The
session.rs test `codex_hook_session_splices_granted_mcp_server_into_shadow_config`
asserts the real file parses with features+trust+server (table exactly once).

**Codex on every other carrier is refused at the Node factory, not delivered:**
the non-hook paths would point codex at a shadow home containing only the MCP
table (no `auth.json`, no user config), silently losing the operator's login.
Node tests `granted_codex_is_refused_on_shell_pty_with_hooks_off` and
`..._on_generic_pty` assert the named refusal.

Ungranted (both kinds): `capabilities: []`, `mcp_servers: []`, no
`mcp-cua.json`, no `cua/`, no skill tree, no handshake env — the boundary.

## Tests

- `crates/remuda-driver/tests/cua_capability.rs` (17): embedded-vs-source
  digest + file set + 0700 dirs (incl. `native_home/skills`); managed/
  inherited/shared homes and cleanup; claude argv mount; codex MCP entry
  carries the absolute real home (ambient-env-independent) while agent env
  doesn't; non-shell codex writes/pins nothing; unknown/origin/claude-bypass/
  **codex-never**/kind refusals; both `--mcp-config` forms before writes;
  narrow deny hole (no other Capability-tagged REMUDA_ name); handshake rides
  `value`, no `secretRef`, empty `credentialRefs`.
- `crates/remuda-driver/src/launch/session.rs` test: the real shell-pty
  HookSession output parses with features+trust+server (table exactly once),
  including overwrite of a pre-existing partial config.
- `crates/remuda-testing/tests/cua_capability.rs` (2): real ClaudePrintDriver
  spawn of an env/argv dumper — granted child sees handshake + config path;
  ungranted sees neither.
- `crates/remuda-node` lib tests (2): factory refusal for granted codex on
  shell-pty hooks-off and on generic-pty; pure `(kind, os, cli-row)` gate
  classifications (5).
- `crates/remuda/tests/cua_cli.rs` (8) and
  `crates/remuda/tests/mcp_hub.rs::agent_scoped_…`: host/value/kind refusals;
  dispatch refused for claude and codex; an **instance-scoped agent token**
  creates without capabilities despite GET /v1/hosts returning 403, and is
  refused loudly (not silently dispatched) with capabilities when the host
  cannot be verified.
- `crates/remuda-hub/tests/workers.rs`: dispatch refuses computer-use for both
  harnesses with no provisioning; unknown value refused; create refuses
  claude `bypassPermissions` and codex `never`/`no-request` and names both.
- Hub inventory unit tests: name validation + every host-shape classification.

## Q1 live probe — NOT PASSED

The merge gate requires a real remuda-launched claude session on a host with
Codex Computer Use installed, listing MCP tools. The build host is Linux with
no CUA install, and the hostcap probe row is an unmerged sibling task. The
probe could not be performed; the feature therefore ships refusal paths and
materialization **behind the explicit opt-in, default off**, as
codex-cua.md §8 prescribes. Remaining Mac verification:

1. Node reports `cli[kind=computer-use].installed = true` and `os = macos`.
2. Attended `remuda instance create --host <mac> --capability computer-use`:
   child sees the handshake, `mcp-cua.json` on argv, skill tree in its managed
   home; the cua-repl MCP server starts (it resolves the vendor app via
   `REMUDA_CODEX_HOME`, not the shadow `CODEX_HOME`) and lists tools.
3. Granted codex (generic-pty and shell-pty+hooks): agent state stays in the
   shadow home while the MCP server finds the vendor app.
4. `dispatch --capability computer-use` refuses for both harnesses.
