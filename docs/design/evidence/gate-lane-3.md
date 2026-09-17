# Evidence: gate-lane-3

Date: 2026-09-18. Branch: `wt/c-lanetty/lane-child-env`.
Design: [coordinator-hierarchy.md](../coordinator-hierarchy.md) §3.2/§8 row 6;
decision: [D-034](../decisions.md). Builds on
[gate-lane-1](gate-lane-1.md) and [gate-log-1](gate-log-1.md).

Context: the fixture `tty_endpoint_emits_protocol_v1_binary_fixture`
(`crates/remuda-node/tests/local_api.rs`) failed **deterministically** on lane
`sg-lane1` for every branch verified there — three runs, two branches, one of
them unrelated to any TTY code — while the same test passed in the coordinator
ssh-driven gate on the same remote host for every branch. The panic was
`expected binary TTY frame` after the test's 4s deadline (exit 101 after
retry).

## Root cause — two halves

### 1. The lane leaks its own carrier flags into the gate child

The lane Node is started with `REMUDA_PTY_CARRIER=native
REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`. The lane runner
(`crates/remuda-node/src/gate.rs`) built the merge child with
`Command::new(binary)` and only *layered* the lane env, `CARGO_TARGET_DIR`,
`CARGO_INCREMENTAL`, `VITE_NO_WATCH`, `PW_*`, `HUB_E2E_*` and a PATH prefix on
top of the **inherited** process environment. So those three carrier flags rode
into `remuda merge --gate`, into cargo, and into every test binary. The
coordinator ssh gate inherits none of them — that is the whole difference
between the two lanes.

### 2. The fixture depended on ambient env

`ShellPtyOptions` defaults `emulator` to `emulator_enabled()`, which reads
`REMUDA_PTY_EMULATOR` from the process env
(`crates/remuda-driver/src/shell_pty.rs`). With the emulator on,
`TtyRegistry::attach` returns a `Repaint` snapshot, so `alt_screen` is `Some`
(`crates/remuda-node/src/tty.rs`), and `tty_socket` then sends a JSON
`tty.mode` **text** message *before* the first binary frame
(`crates/remuda-node/src/server.rs`). The fixture accepted only a
`Message::Binary` as the first message it read and otherwise reconnected, so it
spun until the 4s deadline and panicked. The fixture is really asserting
"protocol v1 binary framing", not "no mode notice arrives first" — the
notice-before-snapshot ordering is intended behaviour and is **not** changed.

## Three-way repro (remote host, 2026-09-18)

Single test, scratch worktree at `origin/main` `7cd8d854`, one shared target
dir:

| Environment | Result |
| --- | --- |
| plain ssh session env | **pass** (0.43s) |
| lane env + `REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1` | **FAIL** (4.08s — the 4s deadline) |
| same, detached with `setsid nohup`, stdin `/dev/null` | **FAIL**, identical |

Bisect of the three flags (all else = lane env):

| Flag set | Result |
| --- | --- |
| `REMUDA_PTY_CARRIER=native` only | pass |
| `REMUDA_PTY_HOOKS=1` only | pass |
| `REMUDA_PTY_EMULATOR=1` alone | **FAIL** |

No PTY, controlling terminal, `TERM`, `HOME` or process-group condition is
involved: `REMUDA_PTY_EMULATOR=1` is necessary and sufficient.

## Fix

### Gate child environment is now built explicitly (`gate.rs`)

Both gate children — the merge CLI (`run_gate_typed` → `execute_gate`) and the
`gate.then` bash step (`run_gate_then_typed`) — now `env_clear()` the command
and set only an explicitly composed map. No ambient Node variable, carrier flag
included, survives.

`host_env_snapshot()` reads the Node process env **once** and keeps only a
build passthrough whitelist. `gate_child_base_env()` layers, lowest precedence
first: `TERM=dumb`, then the host passthrough, then the lane env from the
request (the only channel that may (re)introduce a `REMUDA_PTY_*` variable or
override `PATH`/passthrough). `PATH`'s base is derived from the explicit env
(the lane's own `PATH` or the whitelisted `GATE_BASE_PATH`), never from a
captured `std::env::var_os("PATH")` of the Node — so a lane is reproducible from
its config alone — with the toolchain prefix composed ahead of it (unchanged
semantics).

**Before → after child env key set** (representative lane: toolchain prefix
`/opt/toolchain/bin`, lane env `CARGO_BUILD_JOBS=8`):

| Key | Before (inherited) | After (explicit) |
| --- | --- | --- |
| `REMUDA_PTY_CARRIER` | **native** (leaked) | *absent* |
| `REMUDA_PTY_EMULATOR` | **1** (leaked) | *absent* |
| `REMUDA_PTY_HOOKS` | **1** (leaked) | *absent* |
| every other ambient Node var | inherited | *absent* |
| `CARGO_TARGET_DIR` | set | set |
| `CARGO_INCREMENTAL` | `0` | `0` |
| `VITE_NO_WATCH` | `1` | `1` |
| `CARGO_BUILD_JOBS` (lane env) | set | set |
| `PW_*` / `REMUDA_E2E_LOCK` / `HUB_E2E_*` / `REMUDA_GATE_STEP_TIMEOUTS` | set when requested | set when requested |
| `TERM` | inherited (e.g. `xterm-256color`) | `dumb` |
| `HOME` `USER` `SHELL` `TMPDIR` `LANG` `SSH_AUTH_SOCK` | inherited | passthrough whitelist (when present) |
| `PATH` | prefix + **Node's ambient PATH** | prefix + lane `PATH` or `GATE_BASE_PATH` |

The only way a `REMUDA_PTY_*` variable now reaches a gate child is the lane env
setting one deliberately (the escape hatch) — asserted by
`lane_env_is_the_only_channel_for_a_pty_flag`.

### Fixture hardened (`local_api.rs`)

`first_binary_tty_frame()` reads frames in a loop and returns the first
`Message::Binary`, ignoring `Message::Text` notices such as `tty.mode`, before
`assert_protocol_v1_frame()` checks the 32-byte header, the `1,1,0,0` prefix
and the payload length. The endpoint's ordering and the emulator default are
untouched.

## Tests

- `gate.rs` unit tests:
  - `gate_child_env_drops_ambient_pty_flags_and_composes_path` — no
    `REMUDA_PTY_*` in the built child env; derived build keys present; `PATH`
    leads with the toolchain prefix over the lane's own base.
  - `gate_child_path_falls_back_to_whitelisted_base` — with no lane `PATH` and
    no prefix, `PATH` == `GATE_BASE_PATH` (reproducible from config).
  - `lane_env_is_the_only_channel_for_a_pty_flag` — a lane that sets
    `REMUDA_PTY_EMULATOR` is honored.
  - `host_env_snapshot_selects_only_the_whitelist` — the snapshot never carries
    a carrier flag.
  - `gate_then_child_env_is_lane_env_plus_base_path` — the bash step gets the
    lane env + base `PATH`, no toolchain/cargo/port keys.
  - `ambient_pty_flag_never_reaches_the_gate_child` — the required regression:
    re-execs the ignored `gate_child_env_from_real_process_env` with
    `REMUDA_PTY_EMULATOR=1` (plus the other two flags) genuinely exported into
    the parent, and asserts the flag is absent from the child env built through
    the real `host_env_snapshot()`. (`unsafe`/`set_var` is forbidden
    workspace-wide, so the sentinel is set by re-exec, not `set_var`.)
- `local_api.rs`:
  - `tty_endpoint_emits_protocol_v1_binary_fixture` — the hardened fixture.
  - `tty_fixture_survives_emulator_on_and_off` — re-execs the ignored
    `tty_binary_frame_under_ambient_emulator_state` once with
    `REMUDA_PTY_EMULATOR=1` and once with it unset; a protocol-v1 binary frame
    is observed in both. This is the direct regression for the deterministic
    lane failure.

## Verification (this host, 2026-09-18)

- `cargo test -p remuda-node --lib gate` — 30 passed, 1 ignored (the re-exec'd
  inner test).
- `cargo test -p remuda-node --test local_api tty` — 2 passed, 1 ignored.
- Fixture run once with `REMUDA_PTY_EMULATOR=1` exported and once with `env -u
  REMUDA_PTY_EMULATOR`: both pass (0.42s / 0.43s). On `main` the exported case
  fails at the 4s deadline — the fix is what makes it pass.
- `cargo test -p remuda-node` — green.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all` — clean.
</content>
</invoke>
