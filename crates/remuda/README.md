# remuda

Composition root for Hub, Node, SSH transport, and the **main-agent control plane**.

This crate talks to the Hub HTTP API (`crates/remuda-hub`). Fleet routes
(`POST /v1/fleet/instances`, `GET /v1/fleet/:id`, `POST /v1/fleet/:id/commands`)
are specified in `docs/design/proposal.md` §4.6 and may not be deployed yet;
the CLI still sends those shapes and reports a clear TODO on HTTP 404.

## Hub connection

All control-plane commands accept:

| Flag | Environment | Default |
| --- | --- | --- |
| `--hub` | `REMUDA_HUB` | `http://127.0.0.1:8080` |
| `--token` | `REMUDA_TOKEN` | (none) |
| `--bootstrap-token` | `REMUDA_BOOTSTRAP_TOKEN` | (none; used to `POST /v1/login`) |

Device bearer auth is sent as `Authorization: Bearer`. Mutating Hub calls omit
`Origin` so the CLI is treated like curl.

## `remuda instance`

```text
remuda worktree create reviewer --base main --path ../remuda-wt/reviewer
remuda instance create --name reviewer --kind codex --driver generic-pty --worktree reviewer --prompt-file brief.md
remuda instance list
remuda instance send reviewer --file brief.md
remuda instance wait reviewer --until idle --timeout 120000
remuda instance wait reviewer --until 'line:DONE ' --timeout 120000
remuda instance read reviewer --lines 120 --source journal
remuda instance keys reviewer enter
remuda instance stop reviewer --scope run
remuda instance rm reviewer
remuda fleet send --all "PAUSE git commits"
```

`--worktree <name>` runs `git worktree add -b wt/<name>/…` when needed and
records the path as the instance workspace `cwd`. `pty` is an alias for
`generic-pty` (tty-attach). `wait --timeout` is milliseconds (alias
`--timeout-ms`). `send --file` is an alias of `--input-file`.

`--host` and `--labels` are mutually exclusive. Until Hub placement ships,
`--labels` (or neither flag: `placement.any`) is resolved client-side from
`GET /v1/hosts` and `hostId` is still sent because `POST /v1/instances`
currently requires it.

Stdout is one JSON object. Exit 0 means the Hub request returned; command
state lives in that JSON.

## `remuda fleet run`

```text
remuda fleet run --hosts hst_a,hst_b --prompt "cargo test"
remuda fleet run --labels region=sg --max 3 --prompt "cargo test"
```

Always `POST /v1/fleet/instances` with `{spec, hosts|labels, max}` as in
proposal §4.6.

`remuda fleet send --all|--labels` broadcasts `instance.send` to matching
running instances (no fleet id required).

## `remuda mcp`

stdio JSON-RPC 2.0 MCP server for Claude Code `--mcp-config`. Framing is LSP
`Content-Length` (Claude Code) or NDJSON (one JSON object per line).

```json
{
  "mcpServers": {
    "remuda": {
      "command": "remuda",
      "args": ["mcp"],
      "env": {
        "REMUDA_HUB": "http://127.0.0.1:8080",
        "REMUDA_TOKEN": "<device-token>"
      }
    }
  }
}
```

Checked-in example: `docs/design/remuda-mcp.json` (`docs/design/remuda-mcp.md`).

Tools: `remuda_instance_create`, `remuda_instance_list`, `remuda_instance_send`,
`remuda_instance_wait`, `remuda_instance_read`, `remuda_instance_keys`,
`remuda_instance_stop`, `remuda_instance_rm`, `remuda_worktree_create`,
`remuda_fleet_run`, `remuda_fleet_send`. Coordinator skill:
`skills/remuda/SKILL.md`.

## Workflow example

A parent Workflow `agent()` can dispatch a compile to a remote host and wait
for the journal without compiling locally. The child must be allowed to call
the Remuda MCP tools (Claude Code `--mcp-config` as above).

```rhai
let meta = #{
    name: "remote-compile",
    description: "Dispatch a compile to a Remuda host and wait for the journal",
    phases: [ #{ title: "Dispatch" } ],
};

phase("Dispatch");
let host = if args == () { () } else { args.host };
if host == () { pause("verification", "Pass args.host — a Remuda host id (hst_…)."); }

let prompt = "Call MCP tool remuda_instance_create with host=" + host
    + " and prompt `cargo test -p remuda --offline`. "
    + "Then call remuda_instance_wait on the returned instanceId "
    + "with condition run-terminal. Return instanceId, reason, and "
    + "a short journal summary. Do not compile on this machine.";

let r = agent(prompt, #{
    label: "remote-compile",
    capability_mode: "all",
});
if r != () && r.success {
    complete(r.output);
}
```

Run as `/remote-compile` with `args.host` set to a registered Hub host.
The child uses `remuda_instance_create(host=…)` then `remuda_instance_wait`
to recover the remote result.
