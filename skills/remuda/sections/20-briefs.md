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
- finish with a single line: `DONE <sha>`, or `BLOCKED <reason>`

Tell the agent the literal completion line. The `DONE <sha>` convention is
what makes `wait --until 'line:…'` work, and the sha is what you merge.
