## Coordinator loop

The whole job, per agent. Steps 3–6 repeat until the agent reports.

```bash
# 1. Isolate. One worktree per agent, never the coordinator checkout.
remuda worktree create reviewer --base main
# → {"name":"reviewer","path":"…/remuda-wt/reviewer","branch":"wt/reviewer/work"}

# 2. Write the brief to a file, then start the agent with it.
remuda instance create --name reviewer --kind codex --driver pty \
  --worktree reviewer --prompt-file /tmp/brief-reviewer.md
# → {"instanceId":"ins_…", …}   later commands take ins_… or --name

# 3. Wait for the agreed completion line.
remuda instance wait reviewer --until 'line:(?m)^DONE' --timeout 300000

# 4. On timeout: look before you steer.
remuda instance read reviewer --lines 120 --source screen

# 5. Steer — a nudge, an answer, or a key.
remuda instance send reviewer --text "Print DONE <sha> as its own line now."
remuda instance keys reviewer enter

# 6. Collect the sha from matchedLine, then tear down.
remuda instance stop reviewer --scope instance

# 7. Verify and land the branch (see "Verify and merge" below).
remuda merge wt/reviewer/work --gate --json
```

`wait` returns JSON with `reason` (`condition-met` | `timeout`),
`matchedLine` (the **raw** journal line), `lifecycle`, `activity`, `asOfSeq`.
Exit 0 only means the Hub answered — always read `reason`.

Fan out by running steps 1–2 for each agent, then waiting on each. Waits are
independent; nothing is shared but the Hub.
