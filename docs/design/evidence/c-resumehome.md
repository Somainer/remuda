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
