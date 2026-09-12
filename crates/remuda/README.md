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

## `remuda dispatcher`

The dispatcher runs the `remuda-feishu` session router with a `remuda-hub-client` `HubInstanceApi`. It authenticates with the Hub before starting one supervised `lark-cli event consume` child for each of `im.message.receive_v1` and `card.action.trigger`. Both consumers and outbound calls use the same dedicated app profile and `--as bot`. Set up that profile and its subscriptions as described in [remuda-feishu](../remuda-feishu/README.md).

```toml
data_dir = "./data"
shutdown_timeout_secs = 10

[dispatcher]
hub_url = "http://127.0.0.1:8080"
# A scoped, revocable Hub device token. The bootstrap credential is refused for
# this role: the dispatcher holds it for the process lifetime, and the bootstrap
# grants both device login and node enrollment.
token = "env:REMUDA_DISPATCHER_TOKEN"
profile = "remuda"
lark_cli = "lark-cli"
owner_open_ids = ["ou_your_owner_open_id"]
outbound = "dry-run"
# Optional: session_db = "./data/dispatcher/sessions.sqlite"
# Optional group routing:
# chat_allowlist = ["oc_your_chat_id"]
# bot_open_id = "ou_your_bot_open_id"
# bot_name = "Remuda"
# allow_unaddressed = false
# Default route pins, changed per topic by /host, /agent, /model:
# host = "hst_your_host_id"  # empty selects any eligible host
# agent = "claude"
# model = "your-model"
```

```text
remuda --config remuda.toml dispatcher
remuda --config remuda.toml dispatcher --outbound live
remuda --config remuda.toml hub --with-dispatcher --listen 127.0.0.1:8080
```

DryRun records outbound message argv in memory; **inbound consume and Hub instance operations still run**. Selecting `outbound = "live"` or `--outbound live` explicitly enables outbound `lark-cli` calls. No configuration command is run for Lark. An explicit profile and a nonempty owner allowlist are required in both modes. An empty chat allowlist permits owner p2p messages; groups require an allowlist entry and a bot mention unless `allow_unaddressed` is enabled.

`hub --with-dispatcher` connects to the actual bound Hub listener, including an OS-assigned port, and uses that Hub's bootstrap credential. It ignores the standalone dispatcher's URL and credentials. SIGINT/SIGTERM stops event intake and sends SIGTERM to both consume children while retaining their stdin until the supervisor shuts them down. The active dispatch and already-buffered inbound events are allowed to finish up to `shutdown_timeout_secs`; the closed receiver accepts no further events during the drain. A deadline failure exits with an error that identifies the uncertain in-flight operation. The local Hub stays available through this drain. Existing remote instances remain managed by their Nodes.

File configuration is overridden by environment and then dispatcher CLI flags. `--hub-url`, `--profile`, `--lark-cli`, `--session-db`, `--token-file`, `--owner-open-id` (repeatable), `--chat` (repeatable), and `--outbound` are available on the standalone subcommand. Owner/chat flags replace the corresponding lists. There is no bootstrap-token flag: standalone mode requires a scoped device token.

| Environment override | Value |
| --- | --- |
| `REMUDA_DISPATCHER_HUB_URL` | Hub HTTP(S) base URL without a path, userinfo, query, or fragment |
| `REMUDA_DISPATCHER_PROFILE`, `REMUDA_DISPATCHER_LARK_CLI` | Dedicated profile and executable |
| `REMUDA_DISPATCHER_SESSION_DB` | SQLite session map path |
| `REMUDA_DISPATCHER_TOKEN` | Resolved only by the Hub consumer; omitted from config diagnostics |
| `REMUDA_DISPATCHER_TOKEN_FILE` | Credential path; the direct token variable takes precedence |
| `REMUDA_DISPATCHER_BOOTSTRAP_TOKEN`, `…_BOOTSTRAP_TOKEN_FILE` | Still parsed, then **refused** at validation — the dispatcher requires a scoped device token |
| `REMUDA_DISPATCHER_OWNER_OPEN_IDS`, `REMUDA_DISPATCHER_CHAT_ALLOWLIST` | JSON string arrays |
| `REMUDA_DISPATCHER_BOT_OPEN_ID`, `REMUDA_DISPATCHER_BOT_NAME` | Mention matching |
| `REMUDA_DISPATCHER_ALLOW_UNADDRESSED` | `true` or `false` |
| `REMUDA_DISPATCHER_OUTBOUND` | `dry-run` (default) or `live` |

TOML paths are relative to the config file; environment and CLI paths are relative to the working directory. A bare `lark_cli` name uses PATH. Without `session_db`, routing is persisted at `<data_dir>/dispatcher/sessions.sqlite`, so restarting the dispatcher reuses topic-to-instance bindings. Plaintext credentials in TOML are rejected; use `env:NAME` or `file:PATH` references.

The section also accepts positive `startup_timeout_secs` (30), `outbound_timeout_secs` (30), `follow_interval_ms` (1000), `restart_initial_ms` (1000), `restart_max_ms` (30000), and `line_max_bytes` (1048576). The maximum restart delay must be at least the initial delay. Journal polling runs between inbound events and on the configured interval; it can deliver progress and completion cards while no new IM arrives.

TODO in the adapter API: idle ticket expiry still waits for an inbound event, and ticket/inbound deduplication state is process-local. The root does not automatically replay an inbound operation after an error. `RunningHub` still exposes shutdown by Drop without an awaited drain handle.

Offline tests replay [the inbound recording](tests/fixtures/dispatcher-inbound.jsonl), whose header identifies its schema-derived provenance; it is not a live Feishu capture. A local fake consume executable verifies profile selection, restarts, stdin lifetime, and SIGTERM. DryRun checks cover routing persistence and shutdown with an active dispatch; built-binary tests cover the combined Hub mode and authentication validation. These tests do not run a real model or contact Feishu.

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

## `remuda fleet send` / `remuda fleet keys`

```text
remuda fleet send --all "PAUSE git commits"
remuda fleet send --all --kind codex --idempotency-key pause-1 --file /tmp/resume.md
remuda fleet keys --all --host hst_a esc
```

Both `POST /v1/fleet/broadcast`, which selects running instances Hub-side and
fans `instance.send` / `tty.write` out to each. Selection is `--all` and/or
`--labels` / `--host` / `--kind`; the filters intersect, and at least one is
required. Instances in `exited`, `failed`, or `closing` are skipped.

Stdout carries per-instance `results` plus an `accepted` / `failed` /
`skipped` summary, so a partial fan-out is visible without re-querying. With
`--idempotency-key`, each instance is queued under `<key>:<instanceId>`, so
re-running the same broadcast replays the original commands instead of
sending twice. `keys` validates every key name before any bytes are sent.

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
`remuda_fleet_run`, `remuda_fleet_send`, `remuda_fleet_keys`. Coordinator skill:
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
