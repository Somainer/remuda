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
