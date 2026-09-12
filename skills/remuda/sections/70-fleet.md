## Fleet and teardown

```bash
remuda fleet send --all "PAUSE commits ~5 min for a history rewrite"
remuda fleet send --labels region=sg --file /tmp/resume.md
remuda fleet send --all --kind codex --idempotency-key pause-1 "PAUSE"
remuda fleet keys --all --host hst_a esc
remuda instance stop reviewer --scope run       # cancel the run, keep the agent
remuda instance stop reviewer --scope instance  # close it
remuda instance rm reviewer                     # same as --scope instance
```

`--all` and/or `--labels` / `--host` / `--kind` select the targets; the
filters intersect and at least one is required. `fleet send` broadcasts a
prompt and `fleet keys` a validated key to every matching **running**
instance — use them for pauses and fleet-wide notices, not per-agent
steering. Output lists every instance plus an `accepted` / `failed` /
`skipped` summary: read it, a partial fan-out is not an error.
`--idempotency-key` makes a retry replay instead of double-sending. Don't
`rm` instances you didn't create unless asked.
