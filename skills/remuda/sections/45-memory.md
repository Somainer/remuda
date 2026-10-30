## memory

The Hub holds your only durable memory: per-entry rows keyed by lineage, so
they follow you across chapters and hosts. Read and write it only with the
`remuda memory` verbs. Do not use the harness's native auto-memory: it is
keyed by host and working directory, so it splits across chapters and Nodes.

```bash
remuda memory index                    # rendered index, one line per live entry
remuda memory search status report     # titles, descriptions, bodies
remuda memory read status-outcome-first
remuda memory read status-outcome-first --json
remuda memory write status-outcome-first --type feedback \
  --title "Lead status reports with the outcome" \
  --description "The owner wants the conclusion on the first line." \
  --body "The owner asked on 2026-10-30 to lead status reports with the outcome.
Why: they read the first line to decide whether to read on.
How to apply: put the conclusion on the first line, then the detail."
remuda memory write status-outcome-first --expected-revision 1 --body "…"
```

Write rules:

- Store a preference on the first clear statement from the owner.
- Write a statement (`The owner prefers …`), never a command to yourself
  (`Always …`).
- A `feedback` body carries `Why:` and `How to apply:`.
- Use absolute dates (`2026-10-30`), never "today" or "last week".
- Store no logs, code facts or git history — the journal and the repo keep
  those. Task state belongs in tasks (D-050), not in memory.
- Run `remuda memory index`, or `search` for the words involved, before
  writing; do not add a second entry that says the same thing.
- An update carries `--expected-revision <rev>`; without it a write creates.
  On a revision conflict, re-read, merge your change into the newer body, and
  write again with the new revision. Never overwrite another write.
- `about-me` belongs to the owner: read it, never write it.

At chapter start, if no memory index is in your context, run
`remuda memory index` once.

**The engine is not Remuda.** Claude Code and Codex run you; Remuda is the
Hub and its verbs. Change Remuda state with `remuda`, not by editing harness
files (settings, `CLAUDE.md`, agent or skill files).

**Delegate for the right reason.** Delegate when you need isolation,
parallelism or independent tracking — not because the work touches many
files. A sub-task starts with no chat history, so its brief states the goal,
the facts it needs, the files it owns and the acceptance.

**An unknown outcome is read, not retried.** After a timeout or a refusal,
read before you decide (D-055); a blind retry can apply the same action twice.
Use only the reads an Agent has today (`docs/design/agent-parity.md`): after
a timed-out `remuda memory write`, `remuda memory read <slug>` shows whether
it landed; after a send, `remuda instance read … --source journal` shows the
result, because an Agent cannot read a command receipt yet. Where no read
exists, say so plainly instead of inventing one.
