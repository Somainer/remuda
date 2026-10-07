# c-resumehome — 2026-10-05

Owner-reported bug: 继续 an ended Claude session on a local Node always failed.
The resume correctly created a NEW instance (`ins_NEW`, `resumedFrom=<OLD>`,
`mode=native`, driver `shell-pty`) and launched the fake/real harness with
`claude --resume <session>`, but every managed instance owns a fresh native
config home (`<data>/dev-hub/node/instances/<NEW>/native-home`) with no
`projects/` directory — so the harness died within about a second with
`No conversation found with session ID: <id>`.

## Fix

Before launching a resume, the Node now stages the predecessor conversation
into the new instance's native home:

1. **`crates/remuda-node/src/resume.rs` (new)** — resolves the predecessor
   transcript by walking the `resumedFrom` chain (so a resume-of-a-resume
   continues the newest generation), using recorded evidence only:
   - the instance's recorded `nativeTranscriptPath` first — the sole source for
     a promoted session whose transcript lives OUTSIDE any Remuda-managed
     native home;
   - then the durable launch recipe's `<native_home>/projects/<encoded
     cwd>/<session>.jsonl` layout (structured print/sdk drivers never report a
     transcript path, but their recipe records the home).
2. **`runtime.rs`** — runs the resolution before the create command is
   durably accepted. Local native carriers
   (`DriverFactory::requires_local_resume_transcript`) that cannot locate the
   transcript get a `Conflict` refusal naming the session id and every path
   checked, instead of an accepted instance that fails one second later.
3. **`claude_transcript::stage_for_resume` (new, remuda-driver)** — the native
   factory calls it in `build()` right after preparing the fresh home, before
   any process starts. It copies (never hardlinks: the resumed process
   appends, and a link would write into the predecessor's file) the
   transcript into `projects/<encoded cwd>/<session>.jsonl`, plus the
   `<session>/` sidecar dir (subagent transcripts, tool results) and project
   `memory/`. Existing destination files are never overwritten (build retry
   safety). The source may carry an arbitrary file name; the destination is
   always named `<resume session id>.jsonl`. Inherited-home resumes are a
   no-op.
4. **Fakes enforce the real contract.**
   - `fake-claude` (stream-json): parses `--resume` and exits with
     "No conversation found with session ID" when the file is absent in the
     projects layout; fresh sessions now persist their transcript under the
     config home like the real CLI.
   - `fake-harness` already enforced the same lookup via `open_resume`.

## Terminal resume (「继续（终端）」) finding

Confirmed intended: a promoted native `shell-pty` parent resumes as
`shell-pty` on BOTH modes (hub `ResumeMode::driver`, D-028 §5.6) — it already
has both projections, so routing it to herdr `claude-pty` would move a
native-carrier session onto herdr just because of which button was clicked.
A `claude-pty` parent routes terminal-mode resumes to `claude-pty`. Staging
covers all local carriers through one factory hook.

## Verification

- `resume_home.rs` (node e2e, real fake-claude processes, print + sdk):
  three-generation resume chain; transcript + sidecars + memory staged at
  each hop; each child appends only in its own home; all predecessors stay
  byte-frozen; a pruned transcript is refused pre-acceptance with a clear
  message and no instance row; recorded identity survives; an externally
  recorded `nativeTranscriptPath` (outside the managed home) is followed
  verbatim.
- `resume_home_pty.rs` (node e2e, real fake-harness in a PTY, re-exec under
  `REMUDA_PTY_CARRIER=native`, hooks on): the same chain for terminal
  resume — a resumed child stays Ready (it used to exit within a second),
  and continued turns land in the child homes only.
- `claude_transcript` unit tests: copy/freeze/chain/no-op/keep-existing/
  clear-missing-error.
- `sdk_process` resume test now seeds the transcript the fake requires.
- `cargo fmt`, `cargo clippy -D warnings` on changed crates; full
  `remuda-driver`/`remuda-node`/`remuda-hub`/`remuda-testing` suites green
  (workspace run once under `nice`).

## Round 2 — security/hardness review fixes (2026-10-06)

Follow-up review on 2792100e (codex REJECT; grok timeout):

1. **Path traversal + predecessor binding.** `resumeSessionId` is validated
   before acceptance (`is_safe_session_id`: one file-name component,
   alnum/`-`/`_`, ≤64 chars) in the Node resolver, the staging factory, the
   driver's exact-id binds and `fake-claude`; staging verifies destination
   containment lexically (no `canonicalize`) and requires the source
   transcript to be a real regular file (`symlink_metadata`). The id must
   equal the immediate predecessor's recorded native session (empty/`ins_`
   placeholders stay inconclusive).
2. **Sidecar symlinks.** Links are never recreated verbatim. They are
   resolved hop by hop (≤32, cycle-checked), every hop containment-checked
   against the source project dir, and permitted regular-file targets are
   copied as independent files; escaping/absolute/broken/directory links and
   symlinked sidecar roots are hard errors, never silent skips.
3. **Renamed transcript sidecars.** A promoted transcript whose file name is
   not the session id now finds `<dir>/<S>/` first, falling back to the
   file-stem dir only for older layouts.
4. **Provenance + atomic publish.** A `.remuda-staging/<S>.json` marker
   (source path, size, sha256) gates every existing destination: unmarked or
   mismatching files are a clear conflict and are never overwritten; the
   transcript is published through same-directory temp file + rename after
   sidecars and the marker land.
5. **No chain rollback.** The resolver consults only the immediate
   predecessor (newest chapter); a missing newest transcript is refused
   naming that instance instead of falling back to a grandparent.
6. **Readability before acceptance.** Both candidate paths are checked with
   `symlink_metadata` plus an actual O_RDONLY open; permission/IO errors are
   refused pre-acceptance.
7. **Staging bounds.** 256 MiB / 10 000 files / 32 directory levels,
   transcript included; clear errors when exceeded.
8. **Fake sandbox.** `fake-claude` and `fake-harness` write only inside the
   system temp tree (`$TMPDIR`, `/tmp`, `/var/tmp`); explicit out-of-temp
   targets are refused loudly, implicit operator homes are skipped (claude)
   or replaced with a private per-process temp home (harness). Guard tests
   verify all three behaviours; an mtime audit of `~/.claude` before/after
   the runs shows no fake-shaped writes attributable to them — the only
   synthetic-id transcripts that appeared were concurrent workers on
   pre-fix checkouts (vdb `TMPDIR` cwds, one under another worker's
   scratch); the audited runs used `TMPDIR=/tmp` and an isolated
   `shell_pty_fake_send` rerun produced no writes at all.

Owner rule preserved: nothing changes when a session counts as ended — only
process-end evidence is terminal.

Tests: driver unit tests in `claude_transcript.rs` (28), node e2e in
`resume_home.rs` (10 — the extra case rewinds the predecessor to its
create-time `ins_…` placeholder recording and asserts the resume is accepted
as inconclusive rather than refused; it fails with the old equality check and
passes with the placeholder carve-out), the existing `resume_home_pty.rs`
chain, new `fake_home_guard.rs` (3), sandbox unit tests. `cargo fmt`,
`cargo clippy --workspace -D warnings`, four-crate suites and a full
`nice cargo test --workspace` all green.

## Round 3 — descriptor-relative filesystem hardening (owner bug follow-up)

Review of `ac69721d` again REJECTed the staging design: every check was still
a path-based `stat` followed by a path-based open/copy, so symlinks, a
stat→open swap, FIFOs and retries could all defeat the round-2 rules.

### Foundation: `remuda-fdsafe` (new self-contained crate)

`crates/remuda-fdsafe/` — only `std` + `nix` ("fs","dir"), no Remuda
knowledge — exposes the one primitive the staging and the fake sandbox share:

- `DirFd` pins a trusted directory and walks every component from `/` with
  `openat(O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)`; a symlink at ANY level
  fails with a precise `FdErrorKind::Symlink` (fstatat pre-classification
  distinguishes it from the kernel's `ENOTDIR` once `O_DIRECTORY` is set);
- leaves are fstatat-classified, opened `O_NOFOLLOW|O_NONBLOCK` (the
  NONBLOCK is what stops a FIFO blocking the open), and re-classified by
  `fstat` on the OPENED fd — no stat→open window; `identity()` returns
  `(st_dev, st_ino)` from the opened fd for same-file verdicts;
- `create_leaf_excl`, `open_append_leaf`, `renameat`, `mkdirat`, `unlinkat`
  are all descriptor-relative; `remove_private_tree` unlinks links without
  following them. The crate owns the workspace's one audited
  `File::from_raw_fd` transfer and therefore documents (Cargo.toml) why it
  does not inherit `forbid(unsafe_code)`. c-dirpicker builds the same
  Linux/macOS pattern (no `O_PATH` on macOS); this crate is staged as the
  future shared home.

### Staging rewrite (`stage_for_resume`)

1. **Destination symlinks (item 1).** `projects/<slug>` and every intermediate
   are created/walked `O_NOFOLLOW`; the destination `<S>.jsonl` is classified
   as a leaf BEFORE the `(dev,ino)` same-file inherited-home no-op, and an
   existing destination sidecar that is a symlink aborts publishing.
2. **Source escapes (item 2).** Sidecar symlinks are no longer hop-resolved at
   all — every non-regular source entry is skipped and REPORTED in
   `StagedResume.skipped` (`"kind:rel"`), never opened, never recreated. The
   `<session>/exfil -> ../bridge/passwd`, `bridge -> /etc` attack lands
   nowhere; a regression scans every staged byte for `/etc/passwd` content.
3. **TOCTOU (item 3).** The transcript and every sidecar is read from the
   opened fd; provenance is the sha256 of the bytes ACTUALLY streamed
   (`stream_hashed` tees into the file and a Sha256 and re-opens nothing).
   Regression: rename a different file over the source path after the fd is
   opened and assert the streamed bytes and hash are the old inode's.
4. **FIFOs / non-regular leaves (item 4).** Pre-open fstatat means a FIFO is
   skipped before it is ever opened; `O_NONBLOCK` is the second line of
   defence. Regression uses `mkfifo` and asserts sub-5s completion, no
   reproduction, and a report entry.
5. **Atomic private temp tree + manifest (item 5).** The whole conversation
   is copied into `<slug>/.stage-<id>-<uuid>/` with EXCL creates, a manifest
   of `(rel, size, sha256)` is written and verified by re-walking the temp
   tree (sizes and hashes read back from temp fds; any unrecorded file or
   non-regular entry fails), and only then are empty root dirs created,
   sidecars renameat'd into place (identical existing files kept, diverging
   files a hard conflict), and the transcript published last. A crashed
   attempt leaves no file under a final name; stale `.stage-*` trees from
   killed attempts are descriptor-unlinked before every run, and a symlink or
   regular file wearing the private prefix is refused rather than touched.
6. **Retries charged from scratch (item 6).** Enumeration charges the
   COMPLETE selected conversation (count + recorded size, retained files
   included) before any byte is copied; each attempt starts with a zero
   budget and removes its own temp tree on failure. Regression repeats
   staging after a byte-limit and after a file-limit failure.
7. **Oversized transcript (item 7).** The opened fd's size is refused before
   the first read or hash; the live byte cap independently aborts streaming.
8. **Recorded transcript is authoritative (node `resume.rs`, item 8).** A
   non-empty recorded `nativeTranscriptPath` that is missing, a symlink, a
   FIFO, or unreadable is now a Conflict refusal BEFORE any instance row is
   created — no fallback to a stale same-session managed-home file. The
   recipe-derived path is consulted only when nothing was recorded (as with
   structured drivers reporting late). `readable_regular_file` is itself an
   fd walk now. E2E regression: B recorded-missing with A stale-home-present
   is refused, names the recorded file, creates no row, and stages nothing.

### Fake-home sandbox (items 9–10, `remuda-testing`)

- Authorization is now a **sentinel-marked allocated root**
  (`.remuda-fake-root`) produced by `sandbox::TempHome::allocate` /
  `TempHome::adopt`, found by climbing the real ancestor fds; `/tmp` ancestry
  alone and `$TMPDIR` (even `TMPDIR=$HOME`) authorize nothing, and the shared
  `/tmp` mount itself plus world-writable dirs cannot be marked.
- Every fake write (fresh/resume transcript, `FAKE_CLAUDE_TRANSCRIPT_DIR`,
  argv/pid files, harness home, events log, codex/grok artifacts, terminal
  logs, active-sessions, usage.json) goes through `remuda-fdsafe` fd walks —
  a `projects` symlink inside an allocated home cannot redirect a byte.
- The guard tests are now MANDATORY and placement-independent: the explicit
  refusal test uses `/dev/shm` (never under `/tmp`), asserts the exact
  PermissionDenied/non-zero-exit behaviour, and a symlinked-`.claude` test
  proves the fd walk blocks the create even inside an otherwise authorized
  home. Test spawn helpers allocate their own roots
  (`TempHome::adopt(root)` in the shared install/harness support).

Tests: fdsafe unit 7, driver `claude_transcript` unit 35 (8 new round-3
regressions plus the r2 symlink cases rewritten to the skip+report contract),
node `resume_home.rs` 11 (recorded-missing refusal; verified it fails with the
r2 fallback restored), sandbox unit 4, `fake_home_guard.rs` 4 mandatory
process tests. `cargo fmt`, four-crate `clippy -D warnings`, and the
driver/node/testing suites are green.

## Round 3 → Round 4 hardening (2026-10-07)

Review of `69af1f88` (codex REJECT, grok REJECT; remuda-fdsafe direction
accepted). Round 4 moves the trust boundary from "walk every path from `/`"
to a **realpath-once trusted anchor + `O_NOFOLLOW` walk below it**, closes
the hardlink/re-open/type/FIFO gaps, and tightens fake-root authorisation.

### remuda-fdsafe (items 1, 5, 6, 7, 10)

- **Trusted-root anchors (item 1).** `DirFd::anchor_existing` /
  `anchor_or_create` canonicalize the deepest existing ancestor ONCE (so
  macOS `/tmp → /private/tmp`, `/var → /private/var` mount symlinks are
  crossed), then walk/create every component below it with
  `O_NOFOLLOW|O_DIRECTORY`. `subpath`/`ensure_subpath` walk relative paths
  and reject `RootDir`/`ParentDir`/`CurDir` components explicitly (item 7),
  so an absolute path containing `..` cannot escape the anchor.
- **Exact S_IFMT typing (item 5).** Classification is
  `(mode & S_IFMT) == S_IFREG/S_IFDIR/S_IFLNK`; a unix socket (0140000) and
  char/block devices classify `Other` and are never read/copied.
- **FIFO-safe leaves (item 6).** `fstatat(AT_SYMLINK_NOFOLLOW)` precedes
  every open; read and append leaves use `O_NONBLOCK` and re-`fstat` the
  opened fd. A FIFO can never block an open.
- **Live byte budget (item 10).** `copy_capped` streams against the caller's
  REMAINING aggregate budget and probes one extra byte at the cap; it
  returns the real byte count. A file grown after enumeration fails the
  aggregate cap instead of overflowing it.
- `create_subdir_excl` returns a distinct EEXIST error (never adopts or
  deletes an existing name); directory uid/mode/identity accessors.

### Driver staging (items 2, 4, 9, 10, 11)

- **Hardlink no-op tightened (item 2).** Same-file is a no-op ONLY when the
  source and destination leaves share `(dev,ino)` AND live in the SAME
  pinned directory fd (the inherited home). A cross-directory hardlink is a
  different retained file: the staged copy replaces it with an independent
  inode at publish.
- **Held fd end to end (item 4).** The source `OpenedLeaf` opened during
  validation is streamed and hashed straight through `build_verify_and_publish`;
  the source name is never re-opened after validation. Sizes/hashes are the
  actual streamed values.
- **Recoverable publish (item 9).** A retained transcript with NO marker but
  byte-identical (size+sha) to the just-streamed source is accepted and the
  marker written — recovering a crash between transcript rename and marker
  write. An existing non-matching marker remains a hard conflict. Two
  `cfg(test)` thread-local seams inject crashes after-stage and
  pre-marker without the workspace-forbidden `env::set_var`.
- **Aggregate charging (item 10).** Sidecars are no longer charged at
  enumeration; the real stream charges actual bytes via `StageAccount`
  against the remaining budget.
- **Cleanup assertion (item 11).** The after-stage seam leaves a populated
  private tree; the regression asserts the caller removes it and publishes
  nothing. Skipped sidecars surface from the Node materializer
  (`crates/remuda-node/src/native.rs`) as `tracing::warn` launch
  diagnostics naming the skipped entry and session.

### Fake sandbox/harness (items 3, 8)

- **Exclusive randomised allocation (item 8).** `TempHome::allocate`
  `mkdirat`s a fresh randomised 0700 name, verifies uid+mode on the opened
  fd, and removes only what it created. A planted 0777 dir at a predictable
  name is never adopted or deleted (EEXIST → new name). A sentinel is
  trusted only in a directory owned by this euid that is not
  world-writable and is not `/tmp`, `/var/tmp` or `/private/tmp` itself, so a
  planted `/tmp/.remuda-fake-root` authorises nothing.
- **fd-resolved Claude home.** Explicit `CLAUDE_CONFIG_DIR` is the anchor;
  implicit `$HOME/.claude` anchors `$HOME` and WALKs the fallback
  link-free, so a symlinked `.claude` inside an allocated home is refused
  (this was the symlink the r3 anchor canonicalized through).
- **Harness writes (item 3).** Every resume append
  (`open_resume` claude/codex/grok), settings merge, events log,
  active-sessions and usage.json write goes through fd-relative
  `open_append_file`/`write_allowed_file`. A real fake-harness `--resume`
  with the whole home replaced by a symlink to an external directory
  appends zero bytes to the target.

Tests: fdsafe 10; driver `claude_transcript` 42 (6 new round-4: cross-home
hardlink replaced, same-dir inode no-op, socket sidecar skipped, pre-marker
crash recovered, post-enumeration growth capped, populated stage cleanup);
`fake_home_guard` 6 (real fake-process FIFO resume bounded, allocation never
the mount); `fake_harness` 25 (symlinked-home resume appends nothing).
fmt + four-crate clippy `-D warnings` clean; gated `nice cargo test
--workspace` green under gate-e2e with `TMPDIR=/tmp`.

macOS-specific code paths (to be run on the Mac before gating): the anchor
`canonicalize` (`remuda-fdsafe/src/lib.rs:302`), the
`/private/var/folders` fixed-temp base (`sandbox.rs:51-63`), and the
POSIX flags shared by Linux/macOS (`O_NOFOLLOW|O_DIRECTORY|O_NONBLOCK`,
`fstatat(AT_SYMLINK_NOFOLLOW)`, exact `S_IFMT`, `lib.rs:234,267-286,522`).

## Round 5 — pinned-root walks, macOS physical slug, owned inodes (2026-10-07)

Review of `38ec51ef` (codex REJECT, grok REJECT) plus the coordinator's macOS
aarch64-darwin run. One principle covers most findings: **never canonicalise
or trust anything below a pinned root; walk every descendant from the pinned
fd with O_NOFOLLOW; never keep an inode you did not just write unless
provenance proves it.**

### Part 1 — macOS

The coordinator run failed 5 driver and 3 fake_harness tests; the first four
driver failures shared one cause: the anchor canonicalised the trusted ROOT,
but the project SLUG was derived inconsistently. On macOS `getcwd()` in a
shell that `cd`'d through a `/var -> /private/var` symlink returns the
PHYSICAL path, which is exactly the slug Claude Code uses. Fix: the slug is
computed once from the physical cwd (`slug_for`) and agrees for source
enumeration, destination publish and tests; canonicalising the home ROOT no
longer changes the cwd slug. The unix-socket sidecar test bound in a long
std-`tempfile` path exceeding macOS `sun_path` (104); it now binds in a
randomised short `/tmp/f<hex>` 0700 scratch dir with an explicit path-length
assertion. The FIFO process regression derives the physical slug before
spawning and asserts the specific non-regular-file refusal.

### Part 2 — fd-hardening

- **Anchors (item 1).** `anchor_existing`/`anchor_or_create` lstat the path:
  only a real directory is pinnable (a trailing symlink-to-directory is
  refused); that one ancestor is realpath'd once; every component below is
  walked/created O_NOFOLLOW. Source staging and the Node's
  `readable_regular_file` pin the predecessor HOME and walk `projects/<slug>`
  (a `home/projects -> /outside` link fails); the destination native-home
  anchor refuses a symlinked home itself.
- **Owned inodes (items 3/4).** Markerless byte-identical recovery renameats
  the freshly written O_EXCL temp copy over the destination, so a planted
  hardlink to an identical outside file is replaced (published inode has
  nlink==1). A matching marker re-hashes the OPENED destination fd and
  replaces when `nlink > 1` or the inode changed. Cross-home SIDECAR
  hardlinks are always replaced by the independent staged copy.
- **Inherited no-op (item 5).** The same-file same-directory no-op validates
  the `<S>/` and `memory/` sidecar roots through pinned fds before returning.
- **Fake sandbox (item 2).** `open_append_file`/`write_allowed_file`
  authorise as (pinned allocated-root fd, relative path) and walk
  descendants O_NOFOLLOW — they never re-anchor a descendant.
  `private_temp_home` uses a random exclusive name, verifies uid+mode on the
  fd, and cleans only what it created.
- **Random allocation (item 6).** fdsafe test scratch dirs and the fake
  private home are randomised exclusive mkdirat with uid/0700 verification.
- **Staging diagnostics (item 8).** Skipped sidecars propagate into the
  materialisation result; the observation pump journals a warning
  `NativeLifecycle` diagnostic (`severity: Warning`,
  `affects_completion: false`, `skippedSidecars`) on the native lifecycle
  channel, mirrored by `tracing::warn`.

Tests: fdsafe 11 (anchor trailing-symlink refusal, exclusive mkdir, short
socket path); driver `claude_transcript` 48 (source/destination projects
symlink refusal, foreign-inode markerless hardlink replacement, cross-home
sidecar hardlink replacement, no-op sidecar-root validation, physical slug
under a symlinked ancestor, plus all r3/r4 cases); `fake_home_guard` 6
(physical-slug FIFO process, short-path socket); `fake_harness` 25 (Claude
resume with only `H/projects` replaced by a symlink into a valid external
`<slug>/<S>.jsonl` tree: non-zero exit, external bytes unchanged). Linux
fmt + four-crate clippy `-D warnings` and the gated `nice cargo test
--workspace` green; the coordinator re-runs fdsafe/testing/driver
claude_transcript on the Mac.

## Round 6 — canonical allocation base, root-fd writers, proven inode + prefix, per-dest locking (2026-10-07)

Review of `6edbc93a` (codex REJECT, grok REJECT) plus the coordinator's macOS
run. The rule is unchanged: pin the trusted root, walk everything below with
`O_NOFOLLOW`, never adopt an inode or path you did not prove.

### macOS (coordinator)

- **A — `/tmp` is a symlink.** Three sandbox tests failed at allocate because
  `/tmp -> /private/tmp` and a trailing symlink is not pinnable.
  `TempHome::allocate`/`private_temp_home` now canonicalise the deliberately
  trusted BASE once (`canonical_temp_bases`; `/private/tmp` on macOS, `/tmp` on
  Linux) and anchor THAT; everything below is walked O_NOFOLLOW. Membership
  tests use both logical and physical forms (`temp_base_forms`). A direct
  default-base allocation regression (`allocate_against_the_default_base_pins_a_physical_dir`)
  runs on every platform.
- **B — socket `sun_path`.** The short-tmp leaf appeared twice in the staged
  path plus the 36-char session id. The sidecar socket is now bound at
  `/tmp/<fhex>/s` (one leaf, asserted ≤100 bytes) and the bound socket node is
  `rename(2)`d into the sidecar tree on the same filesystem; the long final
  path is never passed to `bind(2)`.

### Findings

1. **Re-anchoring fake writers (codex, high).** `append_or_create` anchored
   the leaf's PARENT, and the events-log/config-home writers re-anchored
   descendants — `fstatat(NOFOLLOW)` follows INTERMEDIATE links (only the
   final component is protected), so a configured home reached THROUGH a link
   (`H/home-link -> R/sink`, with the real final component beyond the link)
   pinned the sink and created `.claude/projects/...` there. Every writer now
   takes the allocated-root fd + relative path: `append_or_create` walks from
   `root_fd_and_relative` (parent creation included), `claude_config_home_fd`
   and the explicit transcript-dir resolution pin the root and
   `ensure_subpath` below it (`rooted_ensure_subdir`). Process regressions: a
   FRESH session and an EXPLICIT `CLAUDE_CONFIG_DIR`, each reached through an
   intermediate symlink with a real component beyond it — non-zero exit, the
   specific "is a symlink" refusal, sink bytes byte-identical. Both FAIL when
   run against the pre-fix `sandbox.rs`/`fake.rs` (verified in an isolated
   target) and pass after; a unit regression covers the rooted walk directly.
2. **Marker is not proof (codex, high).** The provenance marker now records
   the `(st_dev, st_ino)` of the inode the publishing launch renamed into
   place (marker v2; a v1 marker fails closed). A kept destination must prove,
   against the OPENED fd, BOTH the staged PREFIX (size+sha256; child-appended
   suffix allowed) AND the recorded inode with nlink==1. Changed bytes with a
   matching marker are refused; a replaced single-link inode with the exact
   staged bytes is replaced by the staged copy; unproven appended bytes on a
   foreign inode are refused. The marker is written only after the FINAL fd
   passes the same checks. Regressions: changed-bytes matching marker;
   swapped single-link inode matching marker; child append survives against
   the proven inode.
3. **Inherited no-op walks the whole trees (both).**
   `validate_inherited_sidecar_trees` + the new fdsafe
   `reject_symlink_descendants` recurse the complete `<S>/` and `memory/`
   trees from the pinned project fd (depth-bounded) and refuse ANY symlink or
   special-file descendant before the same-file no-op. Regressions for
   `memory/MEMORY.md -> outside` (nested leaf) and `<S>/subagents -> outside`
   (nested dir); fdsafe covers both at the primitive level too.
4. **Harness regression race (both, medium).** The symlinked-`projects`
   resume test now canonicalises the workspace before seeding AND spawning
   (`HarnessBuilder::cwd`, child `cmd.cwd` + physical `--cwd`), waits for
   startup exit WITHOUT sending input, and asserts the captured
   "is a symlink" refusal (the PTY drain now captures stdout+stderr into
   `captured_text`), non-zero status, and unchanged external bytes.
5. **Concurrent resumes (codex, medium).** A new fdsafe RAII
   `DirFd::lock_exclusive` (`flock` on a dup'd independent open description,
   released on drop; contention proven by a two-pin test) is held for the
   whole staging sequence on the destination project dir — the per-attempt
   `.stage-*` sweep, copy and publish. One attempt can no longer delete
   another's live tree; stale trees are removed only under the lock. A
   deterministic two-attempt barrier regression repeats the race 6×.
6. **FIFO test specificity (grok, medium).** `FakeClaudeProcess` now pipes and
   drains stderr (`wait_with_stderr`); the FIFO resume regression asserts the
   exact "not a regular file" refusal, so an unrelated startup failure can't
   satisfy it.
7. **Warning dropped when the pump never starts (grok, medium).** The staging
   result rides on the built driver (`Driver::take_skipped_sidecars`) instead
   of a process-global map; `materialize_instance` drains and journals the
   `skippedSidecars` warning BEFORE `driver.start()`, so it is durable on the
   start-fails path and the per-instance entry is consumed on every path. Node
   e2e: staging with a symlinked sidecar, a "claude" that exits 7 — the
   warning is in the journal even though the process never started.
8. **Web shows skipped sidecars (grok, low).** New pure
   `skippedSidecars` extractor (`features/session/skippedSidecars.ts`); the
   session view renders a neutral `run-details-skipped-sidecars` notice
   (non-error, entries in `data-entries`/title) and the mobile home row
   carries `noticeEntries` rendered by `home-row-skipped-sidecars` alongside,
   never replacing, the body. Unit tests cover the extractor, the session
   chip and the home-row derivation.

Tests: fdsafe 13 (dir lock contention, recursive link rejection); driver
`claude_transcript` 54 (items 2/3/B/5 regressions above); testing lib+bin
49/28, `fake_home_guard` 8 (fresh+explicit symlinked projects, specific FIFO
refusal), `fake_harness` 25; node `resume_home` 12 (item 7 start-fails
journaling); web 2140 unit tests (3 new files, skipped-sidecar cases).
fmt + workspace clippy `--all-targets -D warnings` clean; the fdsafe/driver/
node/testing suites green under `TMPDIR=/tmp`. The coordinator re-runs the
three Mac groups.
