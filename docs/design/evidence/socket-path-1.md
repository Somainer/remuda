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
   `XDG_RUNTIME_DIR` or a >100-byte socket file name), the chooser returns an
   error naming both lengths and every candidate tried, instead of failing at
   bind with the cryptic kernel text.
6. Every bind failure is reported with the path, its byte length, and the
   platform limit (`bind_io_error`).

### Round 2: the fixed `/tmp` last resort

Round 1 stopped at candidate 2, which does not hold in two real environments:
on macOS `std::env::temp_dir()` is the ~48-byte per-user
`/var/folders/xx/yyyy/T/` with `XDG_RUNTIME_DIR` normally unset, and a long
`TMPDIR` makes candidate 2 itself overflow (the round-1 evidence conceded
this under an 80-byte TMPDIR). The candidate chain is now:

1. `$XDG_RUNTIME_DIR/remuda/` (when set);
2. `${TMPDIR:-/tmp}/remuda-<uid>/`;
3. `/tmp/remuda-<uid>/` — a fixed last resort, omitted only when it equals
   candidate 2.

A per-user 0700 directory holding nothing but socket inodes is the one place
a fixed `/tmp` root is correct: `place_socket` walks the candidates and
accepts the first that (a) passes the security checks **and** (b) yields a
join with the socket name at or under `sun_path`; the 41-byte `<uuid>.sock`
name at that root is 58 bytes, under even the 103-byte macOS limit. Only when
every candidate fails does launch error.

Round 2 also hardened directory creation: new components are born `0700` via
a recursive `DirBuilder` mode (never briefly group/world accessible under the
umask), and the mode repair on a pre-existing directory goes through an
`O_NOFOLLOW|O_DIRECTORY` fd with `fchmod`, so a link swapped in after the
lstat can never be followed.

Connect side stays on the real path:

- `HookSession.socket_path` (used by the generated overlay/relay commands,
  shadow codex/grok hooks, and the hook-silence probe in the shell-pty
  promotion poller) is now the bind path; the overlay is asserted to contain
  the short path and never the long symlink.
- Node daemon clients (`daemon_socket_path`, status probe, bridge) derive the
  short target **deterministically** with the same chooser the binder uses —
  the path no longer depends on the `node.sock` symlink existing. Before the
  daemon's first start, and during the binder's startup window (the link is
  installed last), the old code handed the kernel the over-long address, got
  `InvalidInput` instead of `NotFound`, and `remuda node start` aborted
  instead of launching. The symlink target is consulted only if derivation
  itself fails.
- The token-broker client connects to `place_token_broker_socket()` output;
  the digest is deterministic, matching the server's own placement.

Session/daemon end removes both the real socket and the discovery symlink
(explicit owner-side cleanup; `SocketPlacement` deliberately has **no**
`Drop` — an intermediate copy dropping must not unlink a socket another owner
serves).

Lifecycle hygiene for redirected sockets:

- A SIGKILLed Node cannot run the drop that unlinks its runtime sockets.
  `compose` sweeps every candidate at start: remuda `*.sock` inodes that
  refuse a connection (`ECONNREFUSED`) are dead and unlinked; a listener that
  accepts the probe is a live socket and is never touched; non-socket files
  are ignored.
- `purge_instance` unlinks the resolved target of `hook.sock` before removing
  the instance directory, and only when the link resolves inside one of this
  process's own runtime directories — a crafted link pointing elsewhere is
  left intact (and logged).

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

### Remaining data-dir-dependent socket: the herdr carrier

The **herdr terminal carrier** API socket is still
`<data_dir>/herdr/herdr.sock` (`NativeDriverConfig::herdr_socket_dir`,
consumed by `remuda-herdr` which is spawned with that directory and manages
additional named-session sockets beneath it). It is not yet routed through
the chooser. Exact threshold: the suffix is the 17-byte `/herdr/herdr.sock`,
so the bind fails once

```
len(<data_dir>) > 90   on Linux    (hard sun_path 107; safe threshold 83)
len(<data_dir>) > 86   on macOS    (hard sun_path 103)
```

Impact is narrower than the hook/daemon sockets: the carrier is the PTY
terminal path (`ClaudePty`/codex/grok through herdr), not the default
`ShellPty` agent launches; round-1/2's reported failures and the
`live_pipeline` regression are on the hook socket, which is redirected.
Tests bind fake-herdr sockets under a TMPDIR-independent short root
(`remuda_testing::ShortTempDir`), so the matrix stays green under a long
TMPDIR. Routing the carrier through the chooser (a `herdr-<hash>.sock`
runtime socket plus the `herdr/herdr.sock` discovery link, handed to the
spawned herdr process) is a deliberate follow-up because herdr also creates
per-session sockets relative to the configured directory.

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
- newly created directories (including intermediate components) are born
  0700, independent of umask;
- the candidate chain ends at the fixed `/tmp/remuda-<uid>` root (deduped
  when TMPDIR is `/tmp`/unset), where the 41-byte uuid socket is 58 bytes and
  therefore fits every supported platform;
- `XDG_RUNTIME_DIR` used when set (0700, own uid, stable); unwritable XDG
  falls back to tmp (both exercised in subprocesses, since env mutation is
  unsafe and the crate forbids unsafe code);
- a 60-byte TMPDIR with `XDG_RUNTIME_DIR` unset (the macOS shape) makes the
  temp fallback overflow at ~115 bytes; the chooser skips it and binds under
  the fixed `/tmp/remuda-<uid>` root — verified with a real bind in a
  subprocess;
- runtime path still overflowing → explicit error naming every candidate;
  bind error text carries path, byte length, and limit.

remuda-signal long-dir tests: a bound hook socket under a >107-byte instance
dir serves a round trip over the short runtime path, the long symlink path is
un-connectable (`socket.rs`); the hook-silence probe reads the real socket as
listening and the long link as not listening (`hook_silence.rs`).

Driver round trip (requirement E;
`remuda-driver/src/launch/session.rs::a_long_instance_dir_…`): a
`HookSession` started under an instance path carrying a 120-byte **prefix**
ending in a real `instances/ins_<canonical-uuid>` segment binds a runtime
socket at or under `SUN_PATH_LIMIT` (the 41-byte `<uuid>.sock` name is what
matters on macOS), the under-instance `hook.sock` symlinks to it, the overlay
carries the real path, and one `SessionStart` connects, authenticates, lands
one event on the bus and binds the session; dropping the session removes
**both** the real socket and the symlink. A second test runs two long
instances on distinct sockets with independent credentials.

Node daemon (`remuda-node/tests/daemon.rs`):

- `…redirects_under_a_long_data_dir` — 80-byte filler + `node-data`
  (preferred `node.sock` well past 107 bytes): daemon binds a short hashed
  runtime socket, `node.sock` is a symlink to it, status probe and a takeover
  bridge connect through the resolved path, the real socket is 0600, and
  shutdown removes both paths.
- `…before_first_start_is_not_running` — same shape, but with **no daemon ever
  started** (no symlink yet): `daemon_socket_path` deterministically returns a
  short path under the runtime dir and `daemon_is_running` is `Ok(false)`,
  instead of propagating the kernel's `InvalidInput` from the over-long
  address.

Start-up warning (`remuda-node/tests/startup_warning.rs`, tracing capture):
composing a Node under a >90-byte data dir emits exactly one WARN carrying
`data_dir_len` and `safe_limit`; a data dir at most 39 bytes (so the
representative instance socket stays ≤100 bytes) emits none.

Lifecycle: the sweep unit test plants a dead socket (ECONNREFUSED) and a live
one in the fixed runtime dir and asserts only the dead one is reclaimed while
a non-socket file is untouched; the purge test asserts a symlinked
`hook.sock` resolves its target into a runtime dir and unlinks only that
target, while a link pointing outside every candidate (and its target) is
left intact.

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

### Round-2 matrix (per the review)

Full three-crate matrix after round 2, from the worktree root:

```text
$ env -u TMPDIR -u XDG_RUNTIME_DIR \
    cargo test -p remuda-signal -p remuda-driver -p remuda-node
# all three crates green (results in §4)

$ env -u XDG_RUNTIME_DIR \
    TMPDIR="$HOME/Projects/remuda-agents/tmp/sockpath-long-tmpdir-0123456789" \
    cargo test -p remuda-signal -p remuda-driver -p remuda-node
# TMPDIR = 80 bytes; temp fallback = 147 bytes, over both platform limits;
# every redirected socket binds under the fixed /tmp/remuda-<uid> root;
# all three crates green, including live_pipeline
```

Under the 80-byte TMPDIR the old (round-1) chooser hard-errored for the
hook/daemon sockets (its only fallback was the 147-byte temp path); the fixed
last-resort root makes the run green, which is the macOS-shaped failure mode
the review called out (the subprocess test `a_long_tmpdir_without_xdg_…`
pins the deterministic shape with a 60-byte TMPDIR).

## 4 Whole-suite verification

Round 3 matrix (after the fd-leak, sweep-security, link-first and bounded
probe changes):

```text
$ env -u TMPDIR -u XDG_RUNTIME_DIR \
    cargo test -p remuda-signal -p remuda-driver -p remuda-node
# 83 test binaries, all green; exit 0

# 68-byte TMPDIR built as /tmp/r3-<60 x's> (>= 60 bytes, per the brief):
$ D=/tmp/r3-$(python3 -c "print('x'*60)")   # byte length: 68
$ mkdir -p "$D"
$ env -u XDG_RUNTIME_DIR TMPDIR="$D" \
    cargo test -p remuda-signal -p remuda-driver -p remuda-node
# TMPDIR >= 60 bytes with XDG unset: the temp fallback is 135 bytes and is
# skipped in favour of the fixed /tmp/remuda-<uid> root; 83 test binaries
# green, including live_pipeline; exit 0
$ cargo clippy --workspace --all-targets -- -D warnings
# clean
$ cargo fmt --all
# clean (cargo fmt --all --check)
```

Notes from round 3:

- the verifier fd-leak regression runs in a subprocess (the fd count is
  process-wide) and asserts no growth across 400 verifications of a
  pre-existing directory;
- sweep dead/live assertions allow for the kernel's transient
  `EINPROGRESS` right after a listener drops (retry until the stable
  `ECONNREFUSED`), while production keeps conservatively treating anything
  non-`ECONNREFUSED` as live;
- `pty_lifecycle::a_reopened_store_vouches_for_an_empty_inventory` is a
  pre-existing sqlite journal race in a file this branch never modifies
  (`git diff origin/main -- crates/remuda-node/tests/pty_lifecycle.rs` is
  empty); it passes in isolation and in repeated binary runs, and the
  combined matrix is green on rerun.

All commands above were re-run after rebasing onto the latest
`origin/main` tip at push time.
