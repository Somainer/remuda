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


## M1 Hub↔Node wire (2026-09-12)

Shared JSON-RPC 2.0 frames live in `remuda_protocol::hubnode` (new module +
one `lib.rs` line). Hub `/v1/node` parses those types and still accepts
`runtime.hello` plus single-event `journal.append`. Auth is Bearer on WS;
stdio may send `node.auth` first. `journal.append` accepts a batched
`events` array and returns a seq watermark. Binary `tty.frame` uses the
32-byte envelope in `hubnode::TtyBinaryEnvelopeSpec`.

Node: `src/enroll.rs` persists `hostId` + `nodeToken` under the data dir;
`src/transport/hubnode_codec.rs` encodes/decodes the same frames and
dispatches `instance.*` to `DevNode`. `run_stdio` / `StdioCarrier` emit
`node.auth` (when a token exists) then `node.hello` with `id`, `hostId`,
`transport`, `label`, and `version`. `OutboundWssCarrier` dials `/v1/node`
with Bearer and dispatches Hub requests. `remuda-ssh` `enroll_stdio` no
longer translates: `node.auth` sets WS Bearer and is not forwarded; other
JSON-RPC frames pass through.

### Remaining for remuda-node (codex-sol)

- Bind stdio `DevNode` to the persisted enrollment `hostId` (today
  `DevNode::new` mints a second host). `crates/remuda/src/cmd/node.rs`
  should pass `config.data_dir` into `StdioCarrier` / `run_stdio_opts`.
- After Hub hello, write `nodeToken` from the JSON-RPC result on both
  stdio and `WssLink` so the next enroll presents `node.auth` instead of
  bootstrap (bootstrap + a persisted `hostId` is rejected).
- Share one `DevNode` between stdio/`WssLink::connect_runtime` and
  `attach_runtime` so journal pumps and `instance.*` hit the same store.
- Stdio does not yet `journal.append` observations back to Hub; that
  still rides `WssLink`. Binary tty on stdio is unspecified.
- `cargo test -p remuda-node` needed `CARGO_INCREMENTAL=0` once after
  adding `pub mod hubnode` (stale protocol rlibs omitted the module).

### Verified (local + SG)

`cargo test` + `clippy -D warnings` green for remuda-protocol, remuda-hub,
remuda-node, remuda-ssh. SSH enroll tests forward `node.hello` as-is and
use `node.auth` for Bearer.

SG e2e (musl `remuda-node-stdio` at `/tmp/remuda-m1/remuda`, local Hub
`127.0.0.1:18080`): `node.hello` JSON-RPC with `id`, `version`,
`transport=ssh-stdio`, persisted `hostId`, nested inventory. `GET /v1/hosts`
showed `online=true`, `label=devbox-sg`, `hostname=devbox`,
`labels=["region=sg"]`, `transport=ssh-stdio`. Remote files only under
`/tmp/remuda-m1/` (cleaned after).

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

## claude-pty/bg live evidence

Date: 2026-09-12. Host binaries: `herdr 0.9.0`, `claude 2.1.269`. Isolation: cwd/launch under `/tmp/remuda-driver/`; native login copied into `/tmp/remuda-driver/native-home/.claude.json` (never pointed `CLAUDE_CONFIG_DIR` at `~/.claude`, which would look for `~/.claude/.claude.json` and skip oauth). Herdr session `remuda-test` only; user `default` session left running. Model: haiku, `--max-budget-usd 0.3`, once each. Prompt: `Reply with exactly OK`.

### Commands

```
cargo test -p remuda-driver --test live_claude live_claude_pty_start_prompt_idle_read_close -- --ignored --nocapture --test-threads=1
cargo test -p remuda-driver --test live_claude live_claude_bg_start_prompt_idle_read_close -- --ignored --nocapture --test-threads=1
herdr session stop remuda-test && herdr session delete remuda-test
```

### pty

- start 8833ms, `dispatch=TransportWritten`, pane `w1:p1`, session `01a094c0-e867-7768-ba1c-372880ad24ce`.
- First-run bypass warning (`Yes, I accept`) blocked `agent.prompt` (`agent_blocked`). Driver now starts in the workspace root pane (split pane was `agent_pane_busy`) and retries `agent.prompt` on `agent_not_ready`. Live harness waits for the warning then `pane_send_keys down, enter`.
- prompt 310ms after dismiss. Herdr lifecycle events: `["Lifecycle", "Lifecycle"]`. idle observed 18892ms from t0.
- `agent.read` showed the launched argv (redacted): `claude --dangerously-skip-permissions --setting-sources user,project,local --model haiku --session-id 01a094c0-… --max-budget-usd 0.3 --settings /tmp/remuda-driver/pty-launch/settings.json`.
- SessionStart hook located JSONL: `exists=true lines=17 types={"assistant":1,"user":1,"system":1,"attachment":10,…}`.
- close + `herdr session stop/delete remuda-test` total 20473ms. Default herdr session untouched.

### bg

- prepare 7621ms, `dispatch=NotDispatched`, argv has `--bg`, no `--session-id`.
- first send (deferred argv) 5651ms, `jobId=0e10a95d`, `sessionId=0e10a95d-59f5-437a-9349-5074037a4752`.
- SessionStart hook wrote `session-meta.json`: `transcript_path=/tmp/remuda-driver/native-home/projects/-private-tmp-remuda-driver-bg-live/0e10a95d-….jsonl`.
- job `state.json` stayed `state=blocked` / `tempo=blocked` (no TTY to accept bypass/trust UI). Overlay now also sets `bypassPermissionsModeAccepted`. `claude stop` via `close()` at 194542ms, never `rm`.

### Driver fixes from this run

- Use workspace root pane for `agent.start` (real herdr rejects a freshly split pane as not a shell).
- Retry `agent.start` on `agent_pane_busy` / `agent_not_ready`; retry `agent.prompt` on `agent_not_ready`.
- Persist SessionStart `transcript_path` on the live handle; poll hook output up to ~180s.
- Isolated native home must be a directory that contains `.claude.json`, not `~/.claude` itself.

## CI failures for crate owners

### GitHub Actions `34683616964` (`c01250c`, 2026-09-12)

Tests passed. Clippy `-D warnings` failed.

**remuda-node** — `clippy::too_many_arguments` (8/7)

```
error: this function has too many arguments (8/7)
   --> crates/remuda-node/src/transport/wss.rs:376:1
    |
376 | / async fn session_task(
377 | |     mut config: WssConfig,
...
384 | |     ready: Option<oneshot::Sender<Result<Value, NodeError>>>,
385 | | ) {
    = note: `-D clippy::too-many-arguments` implied by `-D warnings`

error: could not compile `remuda-node` (lib) due to 1 previous error
```

### GitHub Actions `34683772080` (`7ab8278`, 2026-09-12)

Required jobs: rust, m0-stub, web failed; secret-scan passed.

**remuda-testing** — `m0-print-stub` missing crate deps (also fails `cargo test --workspace` via `bin "m0-print-stub" test`):

```
error[E0433]: failed to resolve: use of unresolved module or unlinked crate `remuda_driver`
 --> crates/remuda-testing/src/bin/m0-print-stub.rs:5:5
error[E0432]: unresolved import `clap`
error[E0432]: unresolved import `remuda_driver`
error[E0432]: unresolved import `remuda_journal`
error[E0432]: unresolved import `remuda_protocol`
error[E0432]: unresolved import `sha2`
error: cannot find attribute `command` in this scope
  --> crates/remuda-testing/src/bin/m0-print-stub.rs:28:3
error: could not compile `remuda-testing` (bin "m0-print-stub" test) due to 10 previous errors
```

**web / remuda-hub** — CI step `Generated OpenAPI client is current`:

```
git diff --exit-code -- web/src/lib/api.generated.ts
##[error]Process completed with exit code 1
```

Committed `web/src/lib/api.generated.ts` is stale vs Hub OpenAPI. Crate owners should regenerate and commit the client (do not drop the CI check).

## remuda composition root status

The first composition increment implements typed `remuda.toml` configuration with environment/CLI precedence, reference-only provider credentials, Hub startup with `embed-web`, WSS/stdio Node inventory, a local Hub plus FakeDriver Node protected by one development access code, stderr tracing via `RUST_LOG`, signal-driven driver close/drain, and build identity through `remuda version --json`.

Validation used an isolated archive of `63d6cf4801c46f3c8b46d2661eeefc8b740189e3` plus the selected composition files and dependency manifest. `cargo build --offline -p remuda` and `cargo test --offline -p remuda` passed (26 unit tests and 5 integration tests). Localhost binary smokes verified configuration precedence, Hub health and exact embedded Web index bytes, authenticated dev creation and Hub enrollment, SIGINT/SIGTERM, persistent stdio host identity, and shutdown while stdin remains open. Build metadata also passed a `SOURCE_DATE_EPOCH=0`/explicit-SHA check. No real model was run by this task.

The Clippy workaround is `cargo clippy --offline -p remuda --no-deps -- -D warnings -A unused-imports`. The unowned `cmd/ssh.rs` re-exports an unused `run_cli as run`; the earlier full dependency lint also found `remuda-node::transport::session_task` exceeding Clippy's argument count (subsequently addressed by its owner). This exception changes the validation command, not lint policy in source. Other command modules are unchanged by this task.

The composition source landed in `8bd73c5c7b7a80bc10c8e271ae90ee31c48348e2` after the history rewrite. The coordinator synchronized its `Cargo.lock` dependencies in `ccec0b9`; the committed Remuda lock entry matches the dependencies used by the isolated validation.

The configuration follow-up defers semantic validation until the command has applied CLI overrides. A valid `--hub-url` or `--max-instances` can therefore replace an invalid value from `remuda.toml`; unchanged invalid settings still fail before carrier startup. The regression uses the real Node argument parser and override method.

Follow-up validation started from committed `b39c12e` in `/tmp/v-remuda-config`, with `CARGO_TARGET_DIR` set to the shared repository `target/`. Its initial `cargo build --offline --locked -p remuda` failed because the committed Node declares an absent `interactions` module and does not handle its two new interaction errors. Only in the verification worktree, `crates/remuda-node/src/lib.rs` and `src/error.rs` were restored from `74248ec`. With those two dependency files restored, build and tests passed (27 unit tests and 5 integration tests). Binary smokes confirmed rejection of an unchanged zero capacity, successful stdio startup with `--max-instances 3`, and one-object `remuda version --json` output even with invalid config/tracing settings. Full Clippy reached the unchanged SSH re-export warning; `cargo clippy --offline --locked -p remuda --no-deps -- -D warnings -A unused-imports` passed. The verification-only Node substitutions are excluded from this task's commit; the Node owner must land its missing implementation.

Remaining steps:

- Adopt the Node owner's evolving runtime WSS dispatcher after it supports configured labels/maxInstances and bounded cancellation during reconnect. The current root uses `WssCarrier` for enrollment, private host-token persistence and heartbeat; unsupported Hub commands receive JSON-RPC `-32601` rather than a success ACK. A disconnect exits without replay.
- `StdioCarrier` exposes inventory/hello/ping only. Provider profiles and their secret references are loaded and validated, but the current local Node composition does not register those profiles or enforce configured placement/capacity. Native driver/provider integration belongs in the next composition increment after the Node API lands.
- `RunningHub` exposes shutdown on Drop but no awaited server-drain handle. Local Node driver close commands are awaited with a deadline; the Node API still needs cancellation of upgraded WebSockets and a shutdown gate for existing command streams.

### GitHub Actions `34684080230` (`c73bd06`, 2026-09-12)

**remuda-driver** — `cargo test --workspace --locked` failed compiling `tests/live_claude.rs`:

```
error[E0599]: no method named `session_transcript` found for struct `ClaudePtyDriver` in the current scope
   --> crates/remuda-driver/tests/live_claude.rs:247:49
247 |             if !hook && let Some(path) = driver.session_transcript().await {
error[E0277]: the size for values of type `str` cannot be known at compilation time
   --> crates/remuda-driver/tests/live_claude.rs:247:34
error[E0599]: no method named `session_transcript` found for struct `ClaudePtyDriver`
   --> crates/remuda-driver/tests/live_claude.rs:309:32
309 |     if let Some(path) = driver.session_transcript().await {
error: could not compile `remuda-driver` (test "live_claude") due to 5 previous errors
```

web job also failed: `pnpm-lock.yaml` still has `registry.npmjs.org` tarball URLs (lockfile; rewritten separately).

### GitHub Actions `34684629841` (`b39c12e`, 2026-09-12)

**remuda-node** — `cargo test --workspace --locked` failed compiling the lib (incomplete module / match):

```
error[E0583]: file not found for module `interactions`
 --> crates/remuda-node/src/lib.rs:8:1

error[E0004]: non-exhaustive patterns: `NodeError::InteractionExpired` and `NodeError::InteractionSuperseded { .. }` not covered
    --> crates/remuda-node/src/server.rs:1065:28
    --> crates/remuda-node/src/error.rs:7:10

error: could not compile `remuda-node` (lib) due to 2 previous errors
```

`mod interactions` is declared but `src/interactions.rs` was not in the pushed tree. Do not treat this as a lockfile/CI issue.

### GitHub Actions `34685778926` (`f3ac6aa`, 2026-09-12)

**remuda-driver** — `cargo test --workspace --locked` failed:

```
test tty_bypass_strips_print_flag ... FAILED
thread 'tty_bypass_strips_print_flag' panicked at crates/remuda-driver/tests/claude_pty_review.rs:408:74:
called `Result::unwrap()` on an `Err` value: Io(Os { code: 26, kind: ExecutableFileBusy, message: "Text file busy" })
error: test failed, to rerun pass `-p remuda-driver --test claude_pty_review`
test result: FAILED. 12 passed; 1 failed; 0 ignored
```

Likely ETXTBSY while replacing a still-running stub binary. Crate test isolation, not a workflow/lockfile issue.

## Outbound WSS runtime peer (2026-09-12)

`WssLink::connect_runtime` is the stdio peer for Hub `GET /v1/node`: it
dispatches `instance.create` / `send` / `cancel` / `respond` into `DevNode`,
streams `journal.append` with seq watermarks and a bounded queue, and on
reconnect resumes from the Hub-acked watermark (no Command replay).

Frames use `remuda_protocol::hubnode` (`rpc_request` / `NodeHelloParams` /
`JournalAppendParams` / `HubNodeMethod`, landed in `f8826c6`). This Node
transport does not commit the protocol crate.

Stdio now has the matching composed runtime: `run_stdio_runtime_opts` binds a
caller `DevNode` (native or fake) so SSH `node --stdio` and outbound WSS share
the same dispatch + journal pump.

## hubnode codec status

Landed on main: `remuda_protocol::hubnode` (`f8826c6`, `pub mod hubnode` +
`src/hubnode.rs`), Hub `/v1/node` (`b578aba` `ws.rs`), Node
`src/enroll.rs` + `src/transport/hubnode_codec.rs`, SSH enroll without
translation. There is no uncommitted `src/hubnode/` directory; the module
is the tracked file `crates/remuda-protocol/src/hubnode.rs`.

**Stdio now:** `run_stdio` emits optional `node.auth` then JSON-RPC
`node.hello` (`id`, persisted `hostId`, `transport`, `label`, `version`).
Inbound `instance.create` / `send` / `cancel` / `respond` dispatch into a
local `DevNode`. `journal.append` and `tty.frame` are acked; interaction
methods go through `dispatch_interaction`. Legacy `{type:hub.hello|hub.ping}`
is still answered.

**Left:** bind stdio `DevNode` to the enrollment `hostId`; persist Hub
`nodeToken` for the next `node.auth`; share one runtime with
`WssLink::connect_runtime`; pump journal observations back to Hub over
stdio; binary tty on stdio; schema/TS generation still exempts hubnode types.

### GitHub Actions `34687481986` (`48083c3`, 2026-09-12)

**remuda-driver** — `cargo test --workspace --locked` failed:

```
test recipe_round_trip_json_has_no_env_values ... FAILED
thread 'recipe_round_trip_json_has_no_env_values' panicked at crates/remuda-driver/tests/materializer.rs:74:43:
called `Result::unwrap()` on an `Err` value: Io(Os { code: 26, kind: ExecutableFileBusy, message: "Text file busy" })
error: test failed, to rerun pass `-p remuda-driver --test materializer`
test result: FAILED. 12 passed; 1 failed; 1 ignored
```

Same ETXTBSY class as `claude_pty_review` (writing a stub binary while it is still mapped). Crate test isolation, not workflow/lockfile.

