# remuda-herdr

Herdr JSON-RPC client (Unix socket, NDJSON) plus a bridge to
`herdr terminal session observe|control`.

Remuda treats Herdr as the PTY carrier: process lifecycle, pane geometry,
screen-detected agent status, and ANSI frames. Structured transcripts and
permission prompts live elsewhere.

## We do not use Herdr resume

`agent.start` extra argv (`--settings`, `--dangerously-skip-permissions`,
`--model`, …) is **not** persisted by Herdr. On restore Herdr rebuilds
`claude --resume <id>` and drops those flags (see
`docs/research/herdr-herdrx.md` §6).

The upper layer (journal / instance spec) must store the full `agent.start`
parameter list and pass it again on rebuild. Do not set
`resume_agents_on_restore` and expect gateway or yolo flags to survive.

## Sockets

| Session | API socket |
|---|---|
| named `foo` | `~/.config/herdr/sessions/foo/herdr.sock` |
| `default` (explicit only) | `~/.config/herdr/herdr.sock` |
| custom `socket_dir` | `{socket_dir}/herdr.sock` via `HERDR_SOCKET_PATH` |

`HerdrServer::ensure("remuda-test", None)` starts
`herdr --session remuda-test server` if needed and **will not** touch the
user default session unless the name is `"default"`.

There is no protocol-level auth. Socket mode `0600` is the trust boundary.
Never expose the socket on a network.

## Events

`events.subscribe` uses a dedicated connection. Subscription types are
dotted (`pane.agent_status_changed`); the `event` field on the wire may be
dotted or underscored — `Event::name` is always underscored.

`pane.agent_status_changed` **requires** `pane_id`. New panes need a new
subscribe connection.

Recommended bootstrap: subscribe → wait for `subscription_started` →
`session.snapshot` → replay buffered events.

## Terminal stream

`TerminalObserver::open` runs `herdr terminal session observe <pane> --cols --rows`.
Each stdout line is `{"type":"terminal.frame","encoding":"ansi","bytes":"<base64 ANSI>",…}`.
The observer yields decoded `Bytes`. Control mode writes
`terminal.input` / `terminal.resize` JSON to stdin.

## Tests

```
cargo test -p remuda-herdr
cargo test -p remuda-herdr -- --ignored   # needs local herdr + claude
```

The ignored test uses isolated session `remuda-test` and cwd
`/tmp/remuda-herdr`.
