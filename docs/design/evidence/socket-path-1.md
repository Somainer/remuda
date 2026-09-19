# Evidence: unix sockets no longer depend on data-dir depth (AF_UNIX sun_path)

Branch: `wt/c-sockpath/b-sockpath-md`. Date: 2026-09-19. Platform: Linux
(`5.4` x86_64). All commands run from the worktree root.

## 1 The defect

`bind(2)`/`connect(2)` addresses are `sockaddr_un.sun_path`, whose usable
space is **107 bytes on Linux and 103 bytes on macOS/BSD** (the kernel array
is one byte larger; the terminating NUL occupies it). The hook socket lives at

```
<data-dir>/instances/ins_<36-char uuid>/hook.sock
            ^^^^^^^^^^ ^^^^^^^^^^^^^^^^^^^^^^^^^^^
            10 bytes    40 bytes (`ins_` + uuid)
```

i.e. fixed overhead of **61 bytes** after the data directory. Path lengths,
using dummy segments (no real home/user/host names):

| Layout | Bytes | Result before the fix |
|---|---:|---|
| `/var/lib/remuda/instances/ins_0199…0001/hook.sock` (short data dir) | 72 | binds |
| `/home/xxxxx/…54-byte data dir…/instances/ins_…/hook.sock` (reported Node) | **115** | bind fails |
| `$TMPDIR` of 48 bytes + `.tmpXXXXXX` (10) + `/node-data/instances/ins_…/hook.sock` (62) — the `live_pipeline` failure | **120** | bind fails |
| `/Users/uuuuuuuuuuuuuuuuuuuu/Library/Application Support/remuda/.../hook.sock` (macOS-style, dummy user) | **112** | bind fails (limit 103) |

Exact failure, reproduced on this host against a 115-byte path:

```text
SOCKET_PATH_BYTES=115
BIND_ERROR_KIND=InvalidInput TEXT=path must be shorter than SUN_LEN
```

`HookSession::start` turned that into
`DriverError::SettingsIsolationUnavailable("hook socket could not be bound:
…")`, so every hooked launch on that Node failed; with a long `TMPDIR` the
`live_pipeline` hooks child never saw the working row.

### Connect has the same limit — through a symlink

An important empirical fact (verified with a throwaway probe before writing
the fix): **`connect(2)` enforces the limit on the address argument before
symlink resolution**. A listener bound at a 44-byte socket, exposed via a
169-byte symlink:

```text
link bytes=169 real bytes=44
CONNECT_VIA_LONG_SYMLINK=ERR InvalidInput: path must be shorter than SUN_LEN
CONNECT_REAL=OK
```

So "bind short, leave a long symlink, let clients connect through the link"
does not work: every wire client must use the real short path. The symlink is
discovery for operators/tooling only.

## 2 The fix

One chooser, `remuda_signal::runtime_dir::place_socket(preferred, name)`
(new `crates/remuda-signal/src/runtime_dir.rs`):

1. Preferred path ≤ **100 bytes** (`SAFE_SOCKET_PATH_BYTES`, headroom under
   both 107 and 103): bind it directly, exactly as before.
2. Otherwise bind `$XDG_RUNTIME_DIR/remuda/<name>.sock` when `XDG_RUNTIME_DIR`
   is absolute and its `remuda/` subdirectory can be secured; else
   `${TMPDIR:-/tmp}/remuda-<uid>/<name>.sock`. The runtime directory is
   created/verified 0700, owned by the process's euid, and **refused if it is
   a symlink or owned by another uid** (checked before the chmod, so an
   attacker-planted path is never written to). An unusable XDG dir logs a
   warning and falls back to tmp.
3. The conventional long path is created as a symlink to the real socket
   **after** the bind succeeds.
4. Socket file names: instance hooks use the bare `<uuid>.sock`; the Node
   daemon and token broker use a stable `node-<sha256(path)>.sock` /
   `broker-<sha256(path)>.sock`, so distinct data directories never collide
   and restarts resolve the same target.
5. If even the runtime path exceeds `sun_path` (e.g. a 110-byte
   `XDG_RUNTIME_DIR`), the chooser returns an error naming both lengths
   instead of failing at bind with the cryptic kernel text.
6. Every bind failure is reported with the path, its byte length, and the
   platform limit (`bind_io_error`).

Connect side stays on the real path:

- `HookSession.socket_path` (used by the generated overlay/relay commands,
  shadow codex/grok hooks, and the hook-silence probe in the shell-pty
  promotion poller) is now the bind path; the overlay is asserted to contain
  the short path and never the long symlink.
- Node daemon clients (`daemon_socket_path`, status probe, bridge) resolve the
  symlink target and connect there; with no daemon running (no link) the
  conventional path still yields `NotFound` as before.
- The token-broker client connects to `place_token_broker_socket()` output;
  the digest is deterministic, matching the server's own placement.

Session/daemon end removes both the real socket and the discovery symlink
(explicit owner-side cleanup; `SocketPlacement` deliberately has **no**
`Drop` — an intermediate copy dropping must not unlink a socket another owner
serves).

Node start (`compose`, the shared composition root for daemon, dev and
stdio) logs one warning when a representative instance socket
(`instances/ins_<uuid>/hook.sock`) would be redirected:

```text
WARN Node data directory is long: per-instance hook sockets will bind in the
per-user runtime dir and appear as symlinks under each instance directory
```

Bind sites covered: remuda-signal `socket.rs`, remuda-driver `secrets.rs`
(token broker) and `launch/session.rs`, remuda-node `daemon.rs`.
`remuda-testing`'s fake-herdr bind keeps tempdir paths (they fit under the
tested TMPDIRs) and its error now also carries path/byte length/limit. No
`UnixDatagram::bind` sites exist in the workspace. `web/` untouched.

## 3 Tests

Chooser unit tests (remuda-signal `runtime_dir.rs` + integration
`tests/runtime_dir.rs`, remuda-driver `launch/runtime_dir.rs`):

- short preferred path used directly; long path redirected;
- discovery symlink present, points at the real socket, idempotent
  re-install;
- two instances get distinct runtime sockets;
- cleanup removes both files (asserted in the driver round trip below);
- foreign-uid runtime dir refused with `PermissionDenied`;
- symlinked runtime dir refused; own 0777 dir tightened to 0700;
- `XDG_RUNTIME_DIR` used when set (0700, own uid, stable); unwritable XDG
  falls back to tmp (both exercised in subprocesses, since env mutation is
  unsafe and the crate forbids unsafe code);
- runtime path still overflowing → explicit error; bind error text carries
  path, byte length, and limit.

remuda-signal long-dir tests: a bound hook socket under a >107-byte instance
dir serves a round trip over the short runtime path, the long symlink path is
un-connectable (`socket.rs`); the hook-silence probe reads the real socket as
listening and the long link as not listening (`hook_silence.rs`).

Driver round trip (requirement E;
`remuda-driver/src/launch/session.rs::a_long_instance_dir_…`): a
`HookSession` started under a 120-character instance directory binds a
≤100-byte runtime socket, the under-instance `hook.sock` symlinks to it, the
overlay carries the real path, and one `SessionStart` connects, authenticates,
lands one event on the bus and binds the session; dropping the session
removes **both** the real socket and the symlink. A second test runs two long
instances on distinct sockets with independent credentials.

Node daemon (`remuda-node/tests/daemon.rs::daemon_socket_redirects_…`):
80-byte filler + `node-data` (preferred `node.sock` well past 107 bytes):
daemon binds a short hashed runtime socket, `node.sock` is a symlink to it,
status probe and a takeover bridge connect through the resolved path, the
real socket is 0600, and shutdown removes both paths.

Token broker (`remuda-driver/tests/secrets.rs`): a 90-char filler
`broker.sock` is served at the chooser's short runtime path, the long path is
a symlink, and a secret round trip succeeds over the real path.

### Long-TMPDIR `live_pipeline` (the reported regression)

The shell's own `TMPDIR` is deliberately short (23 bytes). Two runs were
performed from the worktree root.

Run 1 — 80-byte TMPDIR (as specified in the brief), with a short
`XDG_RUNTIME_DIR` so the per-user runtime directory itself stays within
`sun_path` (the fallback tmp tree would itself overflow under an 80-byte
TMPDIR):

```text
$ mkdir -p "$HOME/Projects/remuda-agents/tmp/sockpath-long-tmpdir-0123456789"
$ mkdir -p /tmp/remuda-xdg-<uid> && chmod 700 /tmp/remuda-xdg-<uid>
$ TMPDIR="$HOME/Projects/remuda-agents/tmp/sockpath-long-tmpdir-0123456789" \
  XDG_RUNTIME_DIR=/tmp/remuda-xdg-<uid> \
  cargo test -p remuda-node --test live_pipeline
running 1 test
test budgets_and_rule_six_over_a_real_pty has been running for over 60 seconds
test budgets_and_rule_six_over_a_real_pty ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 183.52s
```

Run 2 — tmp fallback, TMPDIR above 40 characters and no `XDG_RUNTIME_DIR`
(real sockets land in `${TMPDIR}/remuda-<uid>/<uuid>.sock`, which fits):

```text
$ env -u XDG_RUNTIME_DIR TMPDIR="$HOME/Projects/remuda-agents/tmp" \
  cargo test -p remuda-node --test live_pipeline   # TMPDIR byte length: 48
test budgets_and_rule_six_over_a_real_pty has been running for over 60 seconds
test budgets_and_rule_six_over_a_real_pty ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 183.03s
```

Before the fix this test died with the hooks child never seeing the working
row ("journal condition never met") whenever TMPDIR exceeded ~25 characters;
with the fix the hook session binds under the per-user runtime dir and the
full real-PTY pipeline completes at both TMPDIR lengths.

## 4 Whole-suite verification

```text
$ cargo test -p remuda-driver -p remuda-signal -p remuda-node
# driver: 446 lib tests + all integration tests green
# signal: 104 lib tests + all integration tests green
# node:   all lib tests (288) + daemon/live_pipeline integration tests green
$ cargo clippy --workspace --all-targets -- -D warnings
# clean
$ cargo fmt --all
# clean (cargo fmt --all --check)
```

All commands above were re-run after rebasing onto the latest
`origin/main` tip at push time.
