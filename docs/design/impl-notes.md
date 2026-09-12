# Implementation notes

## M1 SSH stdio → Hub hosts (2026-09-12)

End-to-end path: `just linux-musl` → `remuda ssh bootstrap` → local `remuda hub`
→ `remuda ssh node <alias> --hub http://127.0.0.1:…` → `GET /v1/hosts`.

Glue lives in `remuda-ssh` (`enroll_stdio`) and `crates/remuda/src/cmd/ssh.rs`.
No remuda-hub / remuda-node files were rewritten.

### Gaps

- **Frame dialects.** `remuda node --stdio` (`StdioCarrier`, owner: node) speaks
  NDJSON `node.hello` then `{type: hub.hello|hub.ping}`. Hub `/v1/node`
  (owner: hub) speaks JSON-RPC 2.0 with `id` and a Bearer token. The CLI
  bridge translates; the two crates are not yet the same codec.
- **Reachability.** A Node on `devbox-sg` cannot dial a Mac loopback Hub.
  SSH stdio is the M1 carrier. Outbound WSS (`OutboundWssCarrier`) still
  returns “planned for M1 enrollment”.
- **Online window.** Hub marks the host offline when the Node WS drops. The
  host is visible on `GET /v1/hosts` only while `remuda ssh node --hub` holds
  the session (omit `--no-hold`).
- **Stdio is inventory-only.** `StdioCarrier` does not dispatch
  `instance.create` / send / journal. Fleet placement against an SSH host
  will not run a driver until node stdio grows those methods.
- **Host id.** Each `node --stdio` process generates a new `hst_` id. Re-enroll
  after a restart is a new host row unless the Node persists enrollment
  (not implemented).
- **Composition `remuda` binary.** `crates/remuda` currently fails to build
  against the split Node crate (`HubConfig.push_block_ms`, missing exports).
  M1 used `remuda-ssh` (same clap as `remuda ssh`) plus
  `remuda-node-stdio` uploaded as `/tmp/remuda-m1/remuda`.

### Verified (2026-09-12)

`GET /v1/hosts` against a Mac loopback Hub showed `online=true`,
`label=devbox-sg`, `hostname=devbox`, `labels=["region=sg"]`,
`transport=ssh-stdio`, plus CLI/herdr inventory from the SG node. Remote
files were only under `/tmp/remuda-m1/` and were removed afterwards.

## M0 demo gaps

<!-- m0-demo-gaps:start -->
Recorded by `scripts/demo/m0.sh` at 2026-09-12T08:14:56Z. The script stops at the first
failing step and does not patch `crates/remuda` or `crates/remuda-node`.

**Owner:** crates/remuda (codex-astra), crates/remuda-node (codex-sol)

**Command:**

```sh
cargo build --workspace
```

**Error:**

```
   |
66 | /                 remuda_node::run_stdio(labels, max_instances)
67 | |                     .await
   | |__________________________^ cannot infer type

Some errors have detailed explanations: E0282, E0425.
For more information about an error, try `rustc --explain E0282`.
error: could not compile `remuda-node` (bin "remuda-node-stdio") due to 2 previous errors
warning: build failed, waiting for other jobs to finish...
error[E0432]: unresolved imports `crate::cmd::hub`, `crate::cmd::node`
 --> crates/remuda/src/cmd/dev.rs:5:11
  |
5 |     cmd::{hub, node},
  |           ^^^  ^^^^ no `node` in `cmd`
  |           |
  |           no `hub` in `cmd`
  |
  = help: consider importing this module instead:
          crate::hub
  = help: consider importing this module instead:
          crate::node

warning: unused import: `run_cli as run`
 --> crates/remuda/src/cmd/ssh.rs:3:45
  |
3 | pub use remuda_ssh::{SshArgs, run_blocking, run_cli as run};
  |                                             ^^^^^^^^^^^^^^
  |
  = note: `#[warn(unused_imports)]` (part of `#[warn(unused)]`) on by default

error[E0063]: missing field `push_block_ms` in initializer of `HubConfig`
  --> crates/remuda/src/cmd/hub.rs:39:23
   |
39 |     remuda_hub::spawn(remuda_hub::HubConfig {
   |                       ^^^^^^^^^^^^^^^^^^^^^ missing `push_block_ms`

Some errors have detailed explanations: E0063, E0432.
For more information about an error, try `rustc --explain E0063`.
warning: `remuda` (bin "remuda") generated 1 warning
error: could not compile `remuda` (bin "remuda") due to 3 previous errors; 1 warning emitted
```
<!-- m0-demo-gaps:end -->

Control-plane (`crates/remuda/src/cmd/{instance,fleet,mcp,hub_client}.rs`) is
not in the compile failure above. Instance create now sends Hub placement
(`host` / `labels[]` / `kind:any`) and still includes `hostId` when
`GET /v1/hosts` can resolve one. `crates/remuda/tests/mcp_hub.rs` drives
`remuda mcp` tools/list + tools/call `remuda_instance_create` against
`remuda_hub::spawn` plus a fake Node WS client.

Once those crates compile, `scripts/demo/m0.sh` continues to `remuda dev`,
fake-claude PATH/`FAKE_CLAUDE_SCRIPT=ok.jsonl` override, Hub `/v1/follow`,
`remuda instance create|send|wait|read`, `remuda mcp` tools/list+call, and
`pnpm --dir web build` + Hub `--web-root`. Source TODOs likely to fail next
(still do not patch from this demo task):

- `crates/remuda/src/cmd/dev.rs`: Hub-linked local host is inventory/heartbeat
  only; FakeDriver instance API stays on the Node listener, not Hub
  `POST /v1/instances`.
- `crates/remuda-node` FakeDriver does not exec `fake-claude`; a PATH `claude`
  + `FAKE_CLAUDE_SCRIPT` override is a remuda-driver concern not wired through
  `remuda dev`.

## remuda-node status

As of `0f96dbd`, the local Node source layer is committed: strict dev
configuration, an in-memory `LocalStore`, FakeDriver registry, one bounded task
per Instance with panic containment, REST/JSON-RPC routes, snapshot-first
multiplexed follow, access-code/CORS enforcement, and protocol-v1 TTY framing.
Before the commit, the combined working tree passed 14 unit tests, 6
`local_api` tests, and the existing outbound-WSS test.

Still stubbed: persistence is in-memory (no SQLite adapter); the M0 TTY endpoint
replays one fixed read-only frame; FakeDriver is the only local driver;
`StdioCarrier` handles inventory/hello/ping but not Instance dispatch; and the
outbound Hub link does not yet route Hub commands into `DevNode`.

Remaining steps: after the concurrent SSH stdio codec change lands, wire these
modules through `remuda-node`'s `lib.rs` and dependency manifest, commit the API
fixture tests, compose `remuda dev` plus `remuda node --stdio` in the binary
crate, then run build/test/clippy and the loopback/LAN CLI smoke checks against
the exact staged tree.
