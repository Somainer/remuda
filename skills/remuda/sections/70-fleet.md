## Fleet and teardown

```bash
remuda fleet send --all "PAUSE commits ~5 min for a history rewrite"
remuda fleet send --labels region=sg --file /tmp/resume.md
remuda instance stop reviewer --scope run       # cancel the run, keep the agent
remuda instance stop reviewer --scope instance  # close it
remuda instance rm reviewer                     # same as --scope instance
```

`--all` and `--labels` are mutually exclusive and one is required. `fleet
send` broadcasts to every matching **running** instance — use it for pauses
and fleet-wide notices, not per-agent steering. Don't `rm` instances you
didn't create unless asked.
