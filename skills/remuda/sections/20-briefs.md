## Task briefs

Write a file, not a one-line prompt, for anything longer than a sentence.
`--prompt-file` (create) and `--file` (send) read it locally. A brief that
omits these produces an agent that pushes, edits `main`, or never terminates:

- work **only** inside its own worktree; the path is its cwd
- per-agent build dir: `export CARGO_TARGET_DIR=/tmp/<proj>-target-<name>`
  (a shared `target/` serializes every agent behind one lock)
- commit on `wt/<name>/…` with explicit pathspecs, never `git add -A`
- run the gates for the crates touched (fmt, test, clippy `-D warnings`) and
  the repo secret scan
- **never push**, never touch `main` — the coordinator fetches the branch
- finish with the DONE report below, or `BLOCKED <reason>`

Tell the agent the literal completion line. `DONE <task> tip=<sha>` is what
makes `wait --until 'line:^DONE'` work, and the tip sha is what you merge.

## DONE report

A completion line is a claim; the report is its evidence. Ask for this shape
and tell the worker to print it verbatim as its last message:

```text
DONE <task> tip=<sha>
item 1: commit=<sha> test=<name> red=<the assertion that failed on the base tip>
item 2: commit=<sha> test=<name> red=<the assertion that failed on the base tip>
checks: <check> — <the verbatim final result line>
ui: <UNCOVERED list from `pnpm --dir web run coverage:ui -- --base origin/main`, or "no web/src changes">
rc: <what ran outside the unit suites> — <observed result>; evidence: <path>
flake: <test name> — <failure message>
```

- **First line.** `DONE <task> tip=<sha>` names the full tip commit that is
  already on the pushed branch — not the short sha, not the first commit of
  the series.
- **One line per item.** The `commit` that implements it, the `test` that
  proves it, and the exact failure that test produced on the base tip (`red`).
  "It failed before my change" is not a RED proof; the failing assertion line
  is.
- **Checks as result lines.** Paste the final line each check prints (`passed`,
  `0 unhandled errors`, the exit status). Do not paraphrase or summarise a
  log. A check that did not run says `skipped` and why; a check that failed is
  reported as failed, not omitted.
- **Web coverage.** A branch that changes `web/src` runs
  `pnpm --dir web run coverage:ui -- --base origin/main` and pastes the
  UNCOVERED list verbatim (or `none`). The table above it is optional
  evidence. "Covered" there means a hub spec navigates a route that
  transitively imports the file — an upper bound, not proof the spec drives
  that code. A branch with no `web/src` change writes
  `ui: no web/src changes`.
- **Real-condition check.** Name what ran outside the unit suites (a live
  round trip, a real service, a browser flow), the observed result, and the
  path to the evidence file under `scratch/proof/`. The path is the evidence;
  a summary of a log is not.
- **Flakes.** An unrelated suite that failed is reported with its test name
  and failure message and left untouched. Do not fix it on a branch that does
  not own it.
- **Missing pieces are explicit.** If an item has no test, write `test=none`
  and why. Silence is indistinguishable from an omission.
- **BLOCKED is the only alternative** to DONE: one line, `BLOCKED <reason>`.
  Never dress a blocked task as a partial DONE report.
