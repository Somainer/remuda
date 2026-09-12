# Main agent control of remote Remuda workers

A Claude Code session (interactive TUI or a Workflow `agent()` child) talks to
Remuda only through the stdio MCP server `remuda mcp`. The model never holds a
Hub root secret: the MCP process logs in with a **device** token (or mints one
from `REMUDA_BOOTSTRAP_TOKEN`) and calls Hub HTTP. Hub placement then picks an
online Node; the Node runs the worker CLI.

```
Claude Code  --stdio MCP-->  remuda mcp  --HTTP-->  Hub  --JSON-RPC/WSS-->  Node worker
     |                         env: REMUDA_HUB, REMUDA_TOKEN / REMUDA_BOOTSTRAP_TOKEN
     +--mcp-config JSON (command + args + env); --strict-mcp-config recommended
```

Tools (JSON-RPC `tools/list`): `remuda_instance_create`, `remuda_instance_list`,
`remuda_instance_send`, `remuda_instance_wait`, `remuda_instance_read`,
`remuda_instance_keys`, `remuda_instance_stop`, `remuda_instance_rm`,
`remuda_worktree_create`, `remuda_fleet_run`, `remuda_fleet_send`,
`remuda_fleet_keys`. Create takes
`host` **or** `labels` (`["region=sg"]`); Hub placement resolves the host. Wait
polls until `idle` / `done` / `blocked` / `line:<regex>` (default 30s). Seq
numbers are per-instance; a fleet view does not imply cross-host order.
Checked-in `--mcp-config` file: [`remuda-mcp.json`](./remuda-mcp.json).

## `--mcp-config` JSON

Path or inline JSON. Claude Code `--mcp-config <file> --strict-mcp-config`
loads **only** this file.

```json
{
  "mcpServers": {
    "remuda": {
      "command": "/abs/path/to/remuda",
      "args": ["mcp"],
      "env": {
        "REMUDA_HUB": "http://127.0.0.1:8080",
        "REMUDA_BOOTSTRAP_TOKEN": "<device-bootstrap-or-omit-if-REMUDA_TOKEN-set>",
        "RUST_LOG": "warn"
      }
    }
  }
}
```

Interactive:

```sh
claude --mcp-config ~/.config/remuda/mcp.json --strict-mcp-config
```

Headless print (demo: `scripts/demo/mcp-workflow.sh`):

```sh
claude -p "$PROMPT" \
  --model haiku \
  --max-budget-usd 0.3 \
  --output-format stream-json \
  --verbose \
  --mcp-config /path/to/mcp.json \
  --strict-mcp-config \
  --permission-mode dontAsk \
  --allowedTools mcp__remuda__remuda_instance_create,mcp__remuda__remuda_instance_wait,mcp__remuda__remuda_instance_read
```

Claude Code names MCP tools `mcp__<server>__<tool>`. Isolate the print cwd and
`CLAUDE_CONFIG_DIR` under `/tmp/remuda-mcp-workflow/` (mode `0700`) so the demo
does not inherit the operator home MCP servers or write session files there.

Do not put the Hub bootstrap token in the **prompt**. It stays in the MCP
server env. Prefer `dontAsk` / an allowlist of `mcp__remuda__*` over
`bypassPermissions` unless the operator explicitly wants a yolo worker.

## Fan-out

1. **One host.** `remuda_instance_create` with `labels: ["region=sg"]` (or
   `host: "hst_…"`). Then `remuda_instance_wait` / `remuda_instance_read`.
2. **Many hosts, same spec.** `remuda_fleet_run` with `labels` or `hosts` and
   `max`. Hub `POST /v1/fleet/instances` returns `fleetId` + `instanceIds`.
   Wait/read each instance (or poll `GET /v1/fleet/:id`).
3. **Steer / stop.** `remuda_instance_send` and `remuda_instance_stop`
   (`scope=run` → `instance.cancel`, `scope=instance` → `instance.close`).

A fake Node used by the demo answers `instance.create` and appends a
`run.terminal` journal event. A real Node runs `claude-print` (or another
driver) on that host; the main agent still only sees Hub JSON.

## Workflow `agent()` example

Save as `.grok/workflows/remote-compile.rhai` (or a Claude Workflow that
spawns an agent with the same MCP config). The child must be allowed to call
the Remuda MCP tools.

```rhai
let meta = #{
    name: "remote-compile",
    description: "Dispatch compile to Remuda hosts matching labels and wait",
    phases: [ #{ title: "Dispatch" }, #{ title: "Collect" } ],
};

phase("Dispatch");
let labels = if args == () { () } else { args.labels };
if labels == () {
    pause("verification", "Pass args.labels, e.g. [\"region=sg\"].");
}

let prompt = "Use only Remuda MCP tools. Call remuda_instance_create with labels="
    + labels
    + ", kind=claude, driver=claude-print, prompt=`cargo test -p remuda --offline`. "
    + "Then remuda_instance_wait (condition run-terminal, timeoutMs 30000) and "
    + "remuda_instance_read. Return instanceId, waitReason, and a short journal "
    + "summary. Do not compile on this machine. Do not use Bash.";

let r = agent(prompt, #{
    label: "remote-compile",
    capability_mode: "all",
});
phase("Collect");
if r != () && r.success {
    complete(r.output);
}
```

Interactive equivalent: in a Claude Code session with the mcp-config above,
ask to “create a claude-print instance on region=sg, wait until the run is
terminal, and summarize the journal.”

Evidence of a print-mode haiku run (redacted stream-json) lives in
[`evidence/mcp-workflow.jsonl`](evidence/mcp-workflow.jsonl), produced by
`scripts/demo/mcp-workflow.sh`.
