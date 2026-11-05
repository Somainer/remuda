## read

```bash
remuda instance read reviewer --lines 120 --source screen   # pane text
remuda instance read reviewer --lines 200 --source journal  # lifecycle events
```

`screen` selects tty observations (generic-pty journals a bounded 80-line
snapshot, `completeness=screen-derived`). With no tty observations the
response comes back with `source` = `journal-fallback` — check that field
rather than assuming you saw the pane. `journal` is the right source for
lifecycle, command state, and errors. Both accept `--after-seq` for
incremental reads.

### search, then read

```bash
remuda search "migration 0042"                 # your lineage's chapters and their workers
remuda search 迁移 --lineage ins_… --since 2d   # narrow by lineage, chapter or time
remuda instance read <instance> --source journal --after-seq <seq-1> --lines 40
```

Use `search` when you know the words but not where or when they were said: a
phrase a worker wrote chapters ago, a tool result, the owner's request. Each
hit names the instance, its seq and a short snippet; read the rows around that
seq before you act on it. A hit is a pointer, not the record. Use `read` when
you already know the session and want what happened next. Search covers only
sessions whose journal you may read. If `indexComplete` is false, older
history is still being indexed (`remuda search --status`). Something the next
chapter must know without searching belongs in memory, with the hit as its
`--source`.
