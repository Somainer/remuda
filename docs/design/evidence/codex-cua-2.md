# Codex Computer Use host capability — presence-only inventory row (2)

Date: 2026-09-19 · Branch: `wt/c-cua-hostcap/b-cua-hostcap-md` · Task: `c-cua-hostcap`
Contract: [`docs/design/codex-cua.md`](../codex-cua.md) §3.4 (branch
`wt/c-cua-adr/b-cua-adr-md`, commit `135d8aba`) · Decision D-045.

## Relationship to `c-cua-launch`

This branch rebases onto `wt/c-cua-launch/b-cua-launch-md`, which lands first.
That task added the Hub-side grant preflight, including a `hosts.os` column and
migration, so the two batches interlock through one shared fact rather than
two: its preflight reads `os` from the **Node's existing** `HostSnapshot.os`
(already emitted before either task) and reads this row from `cli[]` as
`{kind, installed, path}`. **No second os column or source is added here** —
this task contributes the row only, exactly as the coordinator directed.

One housekeeping note from that rebase:

- `c-cua-launch`'s first round wrote its document to
  `docs/design/evidence/codex-cua-2.md`, which the plan assigns to *this* task
  (launch is `codex-cua-3.md`), so the rebase surfaced an add/add conflict on
  that path. Launch's round-3 commit resolved it from their side — their
  document now lives at `codex-cua-3.md` with its own heading — so this task
  only keeps `codex-cua-2.md` and performs no relocation.
- Their interop section said this row "had not landed"; it has now, and the
  interlock was re-verified against this task's real producer rather than their
  hand-written fake node. That re-verification is a test, not an inspection:
  `crates/remuda-node/tests/computer_use_interop.rs` lays down a fake
  `CODEX_HOME`, runs the production probe over it, and hands the resulting row
  straight to the production gate. It was checked for vacuity by renaming the
  probe's `COMPUTER_USE_KIND` constant, which fails two of its three cases
  immediately; the constant was then restored byte-identically.

The two batches' own suites were run together on the rebased tree: both
`computer_use_preflight` (launch, 5) and the probe's unit tests (hostcap) pass,
as do the combined CLI suites (`cua_cli` 7, `dispatch_cli` 7, `doctor_cli` 1).

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
  `installedCli` drops it, so it can never reach `cliSummary` or New Session's
  `supportedKinds`.

`installedCli` needed a real fix here, and ui-spec §2.6 calls it out by file
and line. Its old filter was `Boolean(entry.path || entry.version)`, which
silently discarded exactly the row this task exists to report — a
reported-absent CLI has neither path nor version, so "未安装" could never have
been drawn at all. It now reads `installed` when the Node sent one and falls
back to the path/version heuristic only for a Node that predates the flag
(which is "not reported", a third thing again). Both sides are pinned:

| | `installedCli` | `absentCli` |
|---|---|---|
| `installed: true` | in | out |
| `installed: false` | out | in |
| no flag, has path/version | in | out |
| no flag, bare | out | out |

A reported-absent row also gets its own row in the `/hosts/:hostId` CLI table
(`HostsPage`), since the installed list it used to live in can no longer carry
it and a row that vanishes reads as if the host never answered.

`computer-use` cannot become a launchable harness kind: New Session tests
membership against the harness ids (`claude`/`codex`/`grok`/`agy`/`terminal`),
and the capability is not one of them — pinned by
`cannot turn the capability row into a launchable harness kind`.

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
| Rust integration (`computer_use_interop.rs`) | **the seam**: the row the probe really produces is fed to `c-cua-launch`'s gate — accepted on a simulated Mac, refused for absence, and "not reported" for a missing row |
| vitest (`model.test.ts`) | the three states; `cliSummary` label survival; `installedCli`/`absentCli` truth table; not a launchable kind |
| vitest (`HostDiagnostics.test.tsx`) | the three rendered states, incl. no "未安装" claim when unreported |

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test -p remuda-node` (33 suites, 0 failures), `pnpm typecheck` and
`vitest run` (1183 tests) were run on this branch, all green — on the tree
rebased onto `c-cua-launch`, so both batches' tests ran together and against
launch's gate.

**Base of each figure, since the launch branch moved during this work:** the
unit/integration figures above were re-run last on launch's **round-4** tip
(`8d25bca8`); the hub-e2e runs in the table below were measured on its
**round-3** tip (`c900f499`), which is the base the last full green run
(`rc=0`) was taken on. Round 4 changed only driver/hub internals and docs
(`materializer`, `launch/skills.rs`, `hub/http.rs`, `hub/store.rs`, their
tests) — **no `web/` and no `hub_e2e.rs`** — so it cannot move an e2e verdict,
and the e2e figures are not restated as if they were measured on it.

### Hub-e2e flakes on this shared devbox, recorded rather than smoothed over

**The suite does not produce a clean full pass on this host, and that is not a
property of this change.** Consecutive full runs of the same tree content:

| Run | Result | The failure |
|---|---|---|
| r13 | 116 passed / 0 failed | — |
| r12 | 115 passed / 1 failed | `ux-code.hub.spec.ts:235` |
| r14 | 117 passed / 1 failed | `ux-steer.hub.spec.ts:149` |
| r15 | **120 passed / 0 failed** (`rc=0`, 17.1m) | — |

Different specs, same content, alternating outcomes. `ux-steer.hub.spec.ts:149`
passed in r13 and failed in r14; `ux-code` failed in r12 and passed in r13, r14
and r15. The host is a contended 64-core shared box (peak load observed ~27
with several other sessions' suites running), which is the documented flake
class here — the same contention that earlier killed my `hub_e2e` process
mid-run (r8: `Error: socket hang up`, then every later spec
`connect ECONNREFUSED`).

r15 is the run that counts: the whole suite green, including this task's
nearest specs (`node-inventory.hub.spec.ts` ✓ 37, `new-session.spec.ts`
✓ 34-36) and every spec that had flaked before (`ux-code` ✓ 63 and ✓ 64,
`ux-steer` ✓ 115-118). Its one `✘` line is spec `ux-touchhit.hub.spec.ts:315`,
an intentional `test.fail()` xfail ("xfail until c-sessionchrome, D-040") that
main added, not a failure.

**Ownership is settled by content, not by accumulating runs.** For each failure
above, the spec and everything it depends on are byte-identical to pristine
main, and this branch's entire web diff is `features/hosts/*`,
`HostsPage.tsx` and launch's `generated.ts`:

```
git diff --quiet origin/main HEAD -- web/tests/e2e/ux-code.hub.spec.ts
git diff --quiet origin/main HEAD -- web/tests/e2e/ux-steer.hub.spec.ts
#   → both exit 0: byte-identical to main
git diff --name-only origin/main...HEAD -- web/tests/e2e/hub-auth.ts \
    web/src/features/session web/src/lib crates/remuda-hub/examples
#   → prints nothing: no dependency of either spec differs from main
git diff --name-only origin/main...HEAD -- web/src
#   → prints only web/src/features/hosts/*, web/src/pages/HostsPage.tsx and
#     launch's web/src/types/generated.ts — never features/session or lib
```

A green full run on this branch was also achieved (r13, `rc=0`, 16.0m) with
this task's nearest specs green (`node-inventory.hub.spec.ts` ✓ 37,
`new-session.spec.ts` ✓ 34-36) — so the branch can pass the suite; the host
decides whether it does.

#### The `ux-code` screenshot failure specifically

Across runs it failed at three different declaration lines (158, 230, 235),
always `locator.screenshot: Element is not attached to the DOM` while capturing
an **evidence screenshot** of the code-block workbench. Main has since
diagnosed that cause and the diagnosis matches:

> Re-query the block right before capturing: a late follow re-render can
> detach the element resolved earlier in the test.  (`5d11de1b`, `2cc1fa15`)

That fix is present in the spec on this branch (lines 222-224 and 294-296) and
one run (r12) still caught the race at line 296, so it narrowed rather than
fully closed it — an open upstream residual, not this change's.

No Hub e2e spec was added for this row: `crates/remuda-hub/examples/hub_e2e.rs`
is owned by `c-cua-media` (plan §6 risk 6), and coverage here is unit +
component by design.

### Run hygiene

Hub e2e runs here use this worker's assigned ports
(`HUB_E2E_LISTEN=127.0.0.1:59030`, `HUB_E2E_WEB_PORT=59039`,
`HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59031`) and this worker's assigned lock slot
(`/tmp/remuda-local-e2e.lock-b`; the host runs three slots now that a single
shared lock had queued every run for hours). `58980/58989/58981` are the
**remote gate's own** ports; two earlier runs on them were voided by collision
with a live gate (`connect ECONNREFUSED 127.0.0.1:58980` mid-suite), and their
results are not used here. A third early run died at startup on
`Address already in use` — a teardown race from the run just before it — and is
likewise not counted.

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
