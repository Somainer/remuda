# native-pty-2b — the WSS hello finally advertises the driver inventory

Follow-up to [native-pty-2](./native-pty-2.md) §6.1: the Node already built a
`Host.driverInventory` whose `shell-pty` row says whether Remuda can actually
launch an agent, and the stdio hello already sent it under `capabilities`. The
outbound-**WSS** hello hardcoded `capabilities: None`, and `remuda dev` — plus
every real outbound Node — uses WSS. So on the live demo `GET /v1/hosts`
showed `capabilities: {}`, `driverMatrix.shellPtyAllowed()` was false, and New
Session defaulted `claude` to the legacy herdr `claude-pty` carrier instead of
native `shell-pty` — exactly the fallback P2 was meant to retire.

- **When:** 2026-09-14, 13:00–13:09 UTC.
- **Where:** task-owned `remuda dev` on the dev host, scratch data dirs under
  `/tmp/remuda-r-hellocaps/`, dedicated loopback ports, killed after each run.
- **Binaries:** *before* = origin/main merge `981b670` built unchanged;
  *after* = this branch. Same workspace and PATH both sides.
- Nothing here is a fixture; every quoted field is from the live Hub response.

## 1 — The fix

1. **One helper, both transports.**
   `hubnode_codec::hello_capabilities(host: &Value) -> Option<Value>` maps a
   nested host descriptor's `driverInventory` to
   `{"driverInventory": …}` and returns `None` when it is absent — never `{}`.
   `stdio_hello_params` and the WSS `encode_hello_params` both call it, so the
   NDJSON and WebSocket hellos cannot drift again.
2. **The descriptor now actually reaches the WSS config.** The collected host
   snapshot (`HostSnapshot`, used by `remuda dev`'s
   `WssConfig::loopback(...).with_collected_inventory_from(...)` and the
   SSH-stdio `NodeHello`) had no inventory field at all — only the in-process
   `DevNode` host record did. The builder (`inventory::driver_inventory()`,
   reading `remuda_driver::shell_pty::native_carrier_enabled()`
   ⇒ `REMUDA_PTY_CARRIER=native`) was moved out of `runtime.rs` into
   `inventory.rs`, and both the snapshot and `carrier::HostInventory` now
   carry it. Normal outbound Nodes (`remuda node`, daemon mode) serialise that
   same `HostInventory`, so all three connect paths advertise identically.
3. **Heartbeat refreshes too.** The Hub writes `capabilities` on every
   `node.heartbeat` exactly as on hello, so the WSS heartbeat builder uses the
   same helper instead of `None`, keeping the host view honest after a flag
   change and reconnect.
4. **`host.report` needs no change:** nothing in `remuda-node` ever sends it
   (the method exists only in the protocol enum/types). The only periodic
   inventory refresh is `node.heartbeat`, covered by ③. The Hub-side handler
   already reads `params.capabilities`.
5. The Hub side needed no change: `ws.rs` hello / heartbeat / host.report
   already pass `params.capabilities` to `store.apply_inventory`, which stores
   it verbatim and leaves the stored value untouched when the field is absent
   (an empty heartbeat cannot wipe a hello's claim).

## 2 — Before/after on `GET /v1/hosts`

Each row below is the local host's `capabilities` after a fresh enroll, taken
through the real HTTP API (bootstrap login → `GET /v1/hosts`).

### Before — binary `981b670`, flag unset (the live demo state)

```json
{}
```

### Before — same binary, `REMUDA_PTY_CARRIER=native`

```json
{}
```

The flag changes nothing on the old binary because the WSS frame never reads
it. `driverMatrix.shellPtyAllowed(host, "claude")` → **false** regardless of
the flag: there is no reported matrix at all.

### After — flag unset

```json
{
  "driverInventory": [
    {
      "kind": "shell-pty",
      "launchable": false,
      "reasonCode": "carrier-not-enabled",
      "adapterVersion": "0.1.0",
      "binaryPath": "",
      "binaryVersion": "",
      "binaryDigest": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    }
  ]
}
```

The host now says *why* it will not launch an agent on the native carrier
(the per-driver `capabilities` snapshot rides along on the same descriptor).
The UI can show the flag is off instead of silently typing the first prompt
into a login shell.

### After — `REMUDA_PTY_CARRIER=native`

```json
{
  "driverInventory": [
    {
      "kind": "shell-pty",
      "launchable": true,
      "reasonCode": "carrier-native",
      "adapterVersion": "0.1.0",
      "binaryPath": "",
      "binaryVersion": "",
      "binaryDigest": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    }
  ]
}
```

`shellPtyAllowed(host, "claude")` → **true** (CLI inventory also reports
claude installed on this host): New Session defaults to `shell-pty`, the
native carrier P2 built.

A fresh `GET /v1/hosts` one 15 s heartbeat interval later (new device session,
so an expired cookie could not mask anything) still returned the same row with
`online: true` and `launchable: true` — the heartbeat refresh in fix ③ is
exercised live, not just unit-tested.

## 3 — Automated proof

- `remuda-node` unit tests: the stdio test stays; WSS twins were added —
  the WSS hello carries the inventory, a host without one sends no
  capabilities (and neither does a bare loopback config before collection),
  and the heartbeat refreshes it.
- `crates/remuda-node/tests/wss.rs`
  `wss_hello_driver_inventory_reaches_hub_host_view`: spins the **existing**
  Hub + loopback-WSS runtime-Node harness with the same collected-inventory
  config `remuda dev` uses, re-executed in a child process with a controlled
  `REMUDA_PTY_CARRIER` (the workspace forbids `unsafe`, so env mutation in a
  shared test process is unavailable — same pattern as
  `child_env_isolation.rs`). With `native`,
  `GET /v1/hosts` shows `capabilities.driverInventory[kind=shell-pty].launchable
  == true` / `reasonCode: carrier-native`; unset shows
  `launchable == false` / `carrier-not-enabled`. It also asserts the
  `DevNode` runtime host record describes the same value, so the runtime and
  the transport cannot disagree.
- Web: no change needed; `pnpm --dir web test` passes (422 tests), including
  the `driverMatrix` cases keyed on `driverInventory[].launchable`.

## 4 — Not covered here

- A live New Session click-through in the browser: the matrix function and
  the host view are both covered, and the P2 evidence covered the resulting
  launch; restating the click would add the Playwright flakiness noted for
  this shared dev box without exercising new code.
- `codex` / `grok` rows: only `shell-pty` is described, by design — it is the
  one driver whose launchability is a runtime flag rather than a binary probe.
