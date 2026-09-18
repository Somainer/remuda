# Codex Computer Use host capability — presence-only inventory row (2)

Date: 2026-09-19 · Branch: `wt/c-cua-hostcap/b-cua-hostcap-md` · Task: `c-cua-hostcap`
Contract: [`docs/design/codex-cua.md`](../codex-cua.md) §3.4 (branch
`wt/c-cua-adr/b-cua-adr-md`, commit `135d8aba`) · Decision D-045.

## What this lands

`computer-use` becomes an ordinary, **presence-only** `cli[]` row on every
Node's inventory, so `/v1/hosts`, `remuda hostcap`, `remuda doctor` and
`/hosts/:hostId` can all say whether this machine has the vendor client —
without a single Hub, protocol or web-type change.

Two constraints from the contract are the whole design, and both are pinned by
tests rather than by review:

1. **The version is a file read, never an exec.** Running
   `SkyComputerUseClient --version` would start a Mach service on a real host.
   The probe reads `CFBundleShortVersionString` out of the app bundle's
   `Info.plist` and never spawns any vendor binary.
2. **The row is always present.** A host without the client reports
   `installed: false` with no path, so the web can tell "no" from "not
   reported" — an older Node that omits the row entirely is the latter, and
   rendering it as "unsupported" would invent a fact.

## Probe

`crates/remuda-node/src/inventory.rs`: `ProbeEnv` gains `codex_home`
(`CODEX_HOME`, defaulting to `<home>/.codex`), and `probe()` appends one row
after the five PATH-probed CLIs (`cli[6]`, so every existing `cli[0]` /
`cli[i]` consumer keeps reading what it always did — pinned by
`computer_use_row_is_appended_last`).

### Present, from a real binary plist

A `plistlib`-written **binary** plist (what a real app bundle ships, not the
XML a hand-written fixture would use), through `remuda doctor --local --json`:

```json
{
  "kind": "computer-use",
  "version": "2.7.0",
  "path": "$CODEX_HOME/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient",
  "auth": "unknown",
  "installed": true
}
```

`row count: 6 | last kind: computer-use`. Path redacted to `$CODEX_HOME/…`.

### The client is never executed

The fake client in that fixture is a bomb — `echo SHOULD-NEVER-RUN >&2; exit 1`
— and the run left no sentinel:

```
--- the vendor binary was never run: sentinel check ---
no sentinel: probe did not exec
```

The unit test `computer_use_is_presence_only_and_never_spawns_the_client` makes
this a hard failure rather than an observation: the fixture script touches a
sentinel file, and the test asserts the sentinel is absent *and* that the
version still resolved (so a probe that silently skipped the row cannot pass).

### Absent

`installed: false`, no path, still reported:

```json
{ "kind": "computer-use", "auth": "unknown", "installed": false }
```

A `CODEX_HOME` pointing at a regular file reports the same and does not panic
(`computer_use_codex_home_pointing_at_a_file_is_not_installed`). A present
bundle with an unreadable plist is still `installed: true` with no version —
presence is the stat, not the plist.

## `remuda doctor`

A new read-only `computer-use` check. It is deliberately a **warning**, not a
blocker: the row only gates an explicitly requested per-launch capability
(D-045), so its absence must not fail an unrelated preflight.

Absent, naming the path it looked for (contract §4 — the operator's retry
should not require guessing):

```
warning  computer-use  Codex Computer Use client not found; `--capability computer-use` will refuse on this host
{"installed":false,"version":null,"path":"$CODEX_HOME/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient","auth":"unknown"}
```

Present:

```
ok       computer-use             Codex Computer Use client present; authentication is not probed {"installed":true,"version":"2.7.0","path":"…/SkyComputerUseClient","auth":"unknown"}
```

The generic `binary.<kind>` / `login.<kind>` loop **skips** this row
(`diagnostics.rs`): it is not on `PATH`, and counting it toward "an agent CLI
is installed" would let a host with no harness at all pass the `agents`
blocker.

## `remuda hostcap`

`GET /v1/hosts/{id}/hostcap` is the placement arithmetic and carries no
`cli[]`, so the CLI attaches the capability from the host row it already has
permission to read. No Hub change (contract §3.4).

```json
"computerUse": { "installed": true, "version": "2.7.0", "path": "$CODEX_HOME/…", "auth": "unknown", "reported": true }
```

An older Node that never reports the row yields `{"reported": false}` with **no**
`installed` key — three states stay distinguishable, and the integration test
`dispatch_retire_hostcap_cli_lifecycle` asserts the unreported shape against a
real Hub and a fake Node whose `cli[]` holds only claude.

## The row crosses the wire unchanged

`crates/remuda-node/tests/wss.rs::wss_reannounce_keeps_a_single_host_row` drives
a real outbound-WSS hello into an in-process Hub and reads `/v1/hosts` back.
The row it returns for a host with no vendor bundle, verbatim (other keys
elided):

```json
{ "kind": "computer-use", "auth": "unknown", "installed": false }
```

asserted as: the `cli[]` kinds are
`["claude", "codex", "grok", "agy", "computer-use"]`, and the capability row has
`installed == false`, `auth == "unknown"` and a null `path`.

That assertion was **not** written to pass. Adding the row to this fixture first
failed with:

```
left:  ["claude", "codex", "grok", "agy"]
right: ["claude", "codex", "grok", "agy", "computer-use"]
```

because the test injects `config.cli` directly and so bypasses the probe
entirely — which is the useful part: it proves the row reaches `/v1/hosts`
through the real hello/store path rather than only in the probe's own unit
test, and it would catch the row being dropped by the Hub's `normalize_cli`
(which honours an explicit `installed: false` instead of inferring it from
`path`).

## Web

`web/src/features/hosts/model.ts` gains `computerUseState()`, which returns a
closed union rather than a boolean:

```ts
type ComputerUseState =
  | { reported: false }
  | { reported: true; installed: false }
  | { reported: true; installed: true; version?: string; path?: string };
```

`HostDiagnostics` renders the row in all three states (`data-state` =
`installed` / `absent` / `unreported`; copy 已安装 / 未安装 / 未上报, with 未上报
explicitly reading "不代表本机不支持").

Two things were checked rather than assumed:

- **`cliSummary` does not eat the kind.** `compactCliVersion` strips a
  `<kind>-cli ` prefix; `computer-use` + a bare version survives as
  `computer-use 2.7.0`, pinned by
  `labels an installed row without eating the kind in cliSummary`.
- **A reported-absent row does not pollute the installed list.**
  `installedCli` already filters rows with neither path nor version, so an
  absent row is dropped from `cliSummary` and from New Session's
  `supportedKinds` — it can never become a launchable harness kind.

Fixtures carry all three states: `devbox-sg` installed, `devbox` absent,
`runtime-local` / `forge-doloris` omitting the row.

## Tests

| Layer | What |
|---|---|
| Rust unit (`inventory.rs`) | present+version vs binary plist; absent; `CODEX_HOME` is a file; no plist; non-executable client; appended last; **no-spawn bomb** |
| Rust unit (`diagnostics.rs`) | doctor check absent (names its path, warning) and present (ok, version) |
| Rust unit (`hostcap.rs`) | reported-installed / reported-absent / unreported row shaping |
| Rust integration (`dispatch_cli.rs`) | `remuda hostcap` against a real Hub + fake Node → `computerUse.reported == false` |
| Rust integration (`wss.rs`) | the row survives a real WSS hello → Hub store → `/v1/hosts` round trip |
| vitest (`model.test.ts`) | the three states; `cliSummary` label survival; absent row dropped |
| vitest (`HostDiagnostics.test.tsx`) | the three rendered states, incl. no "未安装" claim when unreported |

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test -p remuda-node`, `pnpm typecheck` and `vitest run` (1122 tests) were
run on this branch.

No Hub e2e spec was added for this row: `crates/remuda-hub/examples/hub_e2e.rs`
is owned by `c-cua-media` (plan §6 risk 6), and coverage here is unit +
component by design.

## Dependency

One new crate: `plist = { version = "1.10.1", default-features = false }` in
`remuda-node`, which pulls only `quick-xml` (both MIT, both `forbid(unsafe_code)`
or unsafe-free, edition 2024). It is needed because a real app bundle ships a
**binary** plist; `default-features = false` keeps the `serde` feature out —
this is a two-key dictionary read, not a deserialization framework.

## Redaction note

Every path above is shown as `$CODEX_HOME/…`. No desktop screenshot appears in
this document: per contract §6.4 a desktop capture is exactly what
`secret-scan` cannot catch, so CUA evidence carries redacted or synthetic
screens only.
