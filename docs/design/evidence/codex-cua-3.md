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

Hub Gate 1 — agent-origin create (http.rs, hoisted before pick_hosts /
provider resolution / prepare_create, so no Interaction is created):

```
an agent-originated launch may not grant "computer-use"; only an explicit
human or bot launch may request it
```

Hub Q4 — unattended mode (names the kind's refused spellings; claude:
`bypassPermissions` and `bypass`; codex: `never` and `no-request`):

```
refusing "computer-use" together with unattended/skipped tool approvals on
the same "codex" launch (permissionMode "never"; refused codex spellings:
never, no-request): desktop control plus auto-approved actions has no
recovery path; remove one of the two
```

Host gate, three shapes. The CLI (`crates/remuda/src/cmd/capability.rs`)
and Hub (`crates/remuda-hub/src/inventory.rs`) name the install location
**symbolically for the remote host** — `$CODEX_HOME/…SkyComputerUseClient`
(the exact file c-cua-hostcap's probe stats), or `$HOME/.codex/…` when
`CODEX_HOME` is unset. Neither expands the CLI/Hub process's own
`CODEX_HOME`/`HOME` (a Linux coordinator refusing a Mac host cannot print a
path on its own box); the Node-side gate (which runs ON the host) does expand
its real env. Never a bare `<no path reported>`.

CLI exact strings (macOS host; `--host`):

```
host hst_… is linux, but the "computer-use" capability requires macOS;
 pick a Mac with --host

host hst_… has not reported the "computer-use" capability (no computer-use
row in its inventory); enable Codex Computer Use at
$CODEX_HOME/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient
on that host (or $HOME/.codex/…/SkyComputerUseClient when CODEX_HOME is
unset), or update/run a Node that probes it, or pick another host with --host

host hst_… reports "computer-use" as not installed; enable Codex Computer
Use at $CODEX_HOME/…/SkyComputerUseClient on that host (or
$HOME/.codex/…/SkyComputerUseClient when CODEX_HOME is unset), or pick
another host with --host
```

When the row carries a real host `path` (installed=true or an explicit
host-reported path), both gates print that path verbatim instead of the
symbolic one.

Hub exact strings (the common path for placement-resolved / direct-API
launches, which never go through the CLI) are the same shape, prefixed
`host hst_…` (e.g. the not-installed form):

```
host hst_… reports "computer-use" as not installed; enable Codex Computer
Use at $CODEX_HOME/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient
on that host (or $HOME/.codex/…/SkyComputerUseClient when CODEX_HOME is
unset), or pick another host with --host
```

CLI instance create, no resolvable/known host:

```
computer-use needs an explicit, resolvable target host: no host was selected
for this placement; pick a Mac with --host (or matching labels)

host hst_… is not in the Hub's host inventory (offline or unknown);
computer-use requires a connected macOS host — pick one with --host
```

Node boundary (granted codex on any carrier except shell-pty + hooks;
the `{driver}` slot is the observed `DriverKind`, e.g. `ShellPty` with
`pty_hooks=false` or `GenericPty`):

```
the "computer-use" capability for codex on {driver} is not delivered:
only shell-pty with REMUDA_PTY_HOOKS=1 materializes the shadow CODEX_HOME
the granted MCP server needs; the other carriers would shadow the
operator's codex login
```

The materializer (`skills.rs`) carries the same refusal for any non-shell-pty
driver, so no `mcp-cua.json`/launchers are written and the recipe records no
grant for an undeliverable codex launch.

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

**Codex on every other carrier is refused at the Node factory AND the
materializer's pre-write gate, not delivered:** the non-hook paths would point
codex at a shadow home containing only the MCP table (no `auth.json`, no user
config), silently losing the operator's login. The hooks-only rule is recorded
in the design at [codex-cua.md §3.3](../codex-cua.md) (D-045 note) and in
[D-045](../decisions.md). Node tests
`granted_codex_is_refused_on_shell_pty_with_hooks_off` and
`..._on_generic_pty`, plus the driver test
`granted_codex_on_a_non_shell_carrier_is_refused_and_writes_nothing`, assert
the named refusal and that no files are written.

Ungranted (both kinds): `capabilities: []`, `mcp_servers: []`, no
`mcp-cua.json`, no `cua/`, no skill tree, no handshake env — the boundary.

## Tests

- `crates/remuda-driver/tests/cua_capability.rs` (16): embedded-vs-source
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
  classifications (5) plus absent-row/no-path default-location tests (2).
- `crates/remuda/tests/cua_cli.rs` (9) and
  `crates/remuda/tests/mcp_hub.rs::agent_scoped_…`: host/value/kind refusals;
  dispatch refused for claude and codex; an **instance-scoped agent token**
  creates without capabilities despite GET /v1/hosts returning 403, and is
  refused loudly (not silently dispatched) with capabilities when the host
  cannot be verified; `pathless_not_installed_row_…` pins the symbolic remote
  `$CODEX_HOME/…/SkyComputerUseClient` location, the `$HOME/.codex` fallback,
  and asserts no local-HOME expansion and no `<no path reported>` placeholder.
- `crates/remuda-hub/tests/workers.rs`: dispatch refuses computer-use for both
  harnesses with no provisioning; unknown value refused; create refuses
  claude `bypassPermissions`/`bypass` and codex `never`/`no-request` and names
  both; `agent_origin_create_with_computer_use_is_refused_before_approval_and_placement`
  (shell-pty shape) asserts the 400 and that **no Interaction was created**.
- Hub inventory unit tests: name validation + every host-shape classification.

## hostcap: the capability reaches the callers the route exists for

Added by `c-cua-hostcap` on this branch's history (round-3 review of its own
work); the Hub-side gate above is launch's and is unchanged by it.

**The defect.** `remuda hostcap` read its `computerUse` block from
`GET /v1/hosts/{id}`. That route is operator-only (`registry.rs::get_host` is
behind `require_operator`, which returns Forbidden for any device carrying an
instance id), and every `remuda` invocation from inside a launched coordinator
sets `REMUDA_INSTANCE_ID` (`hub_client.rs`). So the coordinator — the caller
the pre-dispatch host fact is *for* — always read a refusal, never the
capability.

**The fix, in three parts**, because the second was not in the original
finding and only surfaced when the test failed:

1. `host_capacity` now carries `computerUse` in its own response
   (`crates/remuda-hub/src/workers.rs`), shaped by
   `inventory::computer_use_capability` — the same three states the CLI used to
   build by hand.
2. `agent_scope::restrict_agent_routes` admits `GET /v1/hosts/{id}/hostcap`
   as a read target (`hostcap_read_target`). Without this the route 403s in the
   middleware *before* the handler runs, so part 1 alone would have looked
   correct and changed nothing. Verified: removing this line fails the new
   agent-origin test with that 403.
3. `crates/remuda/src/cmd/hostcap.rs` issues one read and no longer does a
   second `GET`. Admitting the path does not loosen the handler — it still
   requires the `dispatch` grant and re-checks host scope.

**Tests** (`crates/remuda/tests/dispatch_cli.rs`, 9 cases total):

- `hostcap_reports_the_capability_to_an_agent_origin_caller` — creates a
  claude instance holding `dispatch`, runs the real CLI with
  `REMUDA_INSTANCE_ID` set against a fake Node reporting an installed
  `computer-use` row, and asserts `computerUse.installed == true` with **no**
  `error` key. Non-vacuous: fails 403 without part 2.
- `hostcap_reports_an_installed_computer_use_row` — the same row through an
  operator caller, covering the reported-installed path end to end.
- `dispatch_retire_hostcap_cli_lifecycle` — the not-reported case now also
  asserts `computerUse` has **no** `error` key, so an unreported row and a
  failed read cannot be confused (they passed identically before).
- `inventory.rs` unit: `computer_use_capability_reports_all_three_states`.

### Web, same round

Four smaller findings from the same review, all in `c-cua-hostcap`'s files:

- `computerUseState` now derives `installed` by the same rule as
  `installedCli`/`absentCli`, so a flagless row with neither path nor version
  no longer claims `installed: true` (a state the list helpers drop, i.e. the
  silent disappearance ui-spec §2.6 forbids).
- The absent-row block in `HostsPage` is scoped to the capability kind. The
  Node reports every agent CLI it looked for, so an un-scoped list gave a
  claude-only host five empty 未安装 rows for codex/grok/agy/gemini. Pinned by
  a new component test that counts rows for exactly that host shape — it fails
  with 5 rows instead of 1 without the filter.
- A model test that compared two string literals (and so could never fail) was
  replaced with one asserting the real derivation.

### Restack base

This branch carries `c-cua-hostcap`'s commits restacked onto **this branch's
head** (`git rebase --onto <launch-head> 67362396`), replacing the older copies
of the launch commits it previously replayed. No launch commit is re-pushed
under a new hash: launch's real head is an ancestor of the hostcap branch.

## Q1 live probe — NOT PASSED

The merge gate requires a real remuda-launched claude session on a host with
Codex Computer Use installed, listing MCP tools. The build host is Linux with
no CUA install, and the hostcap probe row is an unmerged sibling task. The
probe could not be performed; the feature therefore ships refusal paths and
materialization **behind the explicit opt-in, default off**, as
codex-cua.md §8 prescribes. Remaining Mac verification:

1. Node reports `cli[kind=computer-use].installed = true` and `os = macos`.
2. Attended `remuda instance create --host <mac> --capability computer-use`:
   for a claude launch the child sees the handshake, `mcp-cua.json` on argv
   (`--mcp-config`, no HookSession involved), the skill tree in its managed
   home; the cua-repl MCP server starts (it resolves the vendor app via
   `REMUDA_CODEX_HOME`, not the shadow `CODEX_HOME`) and lists tools.
3. Granted **codex** is delivered only on **shell-pty**, and requires the
   Node to run with `REMUDA_PTY_HOOKS=1` / `pty_hooks = true` (defaults
   **off**): with that precondition set, run
   `remuda instance create --host <mac> --kind codex --driver shell-pty
   --capability computer-use`; the HookSession materializes
   `<launch_dir>/codex-home/config.toml` with the granted server, agent state
   stays in the shadow home while the MCP server finds the vendor app via
   `REMUDA_CODEX_HOME`. Codex on generic-pty (and shell-pty without hooks)
   must be refused with the named "not delivered" message — verify both.
4. `dispatch --capability computer-use` refuses for both harnesses.
