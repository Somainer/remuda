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
