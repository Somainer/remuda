# watch-failed-1: a failed first turn must not report the worker as working

Date: 2026-09-17 (UTC).
Scope: `remuda watch` / `remuda report` classification for screenless
(`claude-print`) workers whose first turn errors. Code change plus a
deterministic reproduction; no driver, dispatch or gate changes.

## The incident

On 2026-09-17 three workers dispatched through `remuda dispatch` failed their
first turn:

- assistant message: `API Error: 400 requested model is not available`;
- turn result: error (`stream-json` result frame `is_error: true`);
- Hub instance lifecycle converged to `failed` (the Node-mirrored journal
  drives that projection in
  `crates/remuda-hub/src/store.rs` `derive_instance_state`);
- no pid / live carrier (print worker).

`remuda watch --once` nevertheless printed all three as `working` with no
detail, and `remuda report` showed nothing actionable. Two causes:

1. **The classifier was screen-gated.** A print driver answers
   `tty.screen` with `supported: false` (see
   `crates/remuda-node/src/runtime.rs` `screen_read`), so the observation had
   no lines; report-token and API-error matching only ever looked at screen
   lines. The mirrored journal — the same source
   `remuda instance read --source screen` already falls back to — was never
   consulted by watch.
2. **A stale carrier lifecycle masked a terminal Hub row.** When the screen
   answer carried a non-empty lifecycle (a disconnecting print carrier still
   says `ready`), the Hub used it verbatim and only fell back to the instance
   row when it was empty. A `failed` row with a stale `ready` screen answer
   therefore classified as working.

## The fake harness

The reproduction uses a fake `claude` harness that fails its first turn with
exactly the production shape — the same stream-json transcript the real
`claude -p --output-format stream-json` prints:

```sh
#!/usr/bin/env bash
# fake-claude: speak just enough stream-json to fail the first turn.
set -u
sid="fake-$(date +%s)"
cat <<EOF
{"type":"system","subtype":"init","sessionId":"$sid","cwd":"$PWD",
 "version":"0.0.0-fake","tools":[],"mcpServers":[]}
{"type":"assistant","message":{"id":"msg_0001","type":"message","role":"assistant",
 "model":"unavailable-model",
 "content":[{"type":"text",
 "text":"API Error: 400 requested model is not available"}]}}
{"type":"result","subtype":"error","is_error":true,
 "result":"API Error: 400 requested model is not available",
 "sessionId":"$sid","numTurns":1}
EOF
exit 0
```

The print driver maps those two frames onto the wire journal
(`crates/remuda-driver/src/claude_print.rs`):

| Fake harness stdout | Journal observation | Where |
|---|---|---|
| `assistant` message, text block | `{"kind":"message","payload":{"role":"assistant","blocks":[{"type":"text",…}]}}` | `map_assistant` → `remuda-journal` `assistant_message` |
| `result`, `is_error:true` | `{"kind":"lifecycle","payload":{"type":"native","topic":"turn","nativeName":"result","status":{"value":"error"}}}` | `map_result` (`status = "error"`) |

The deterministic tests below script those two journal events from a fake Node
(the same events the driver above emits), so the reproduction needs no
credentials, network or model: the Hub projection, the watch classification
and the report rendering all run for real.

## Reproduction

From the repository root, with a scratch target dir:

```sh
CARGO_TARGET_DIR=/tmp/remuda-agents/target-watchfailed cargo test -p remuda-hub \
  --test watch failed_first_turn
CARGO_TARGET_DIR=/tmp/remuda-agents/target-watchfailed cargo test -p remuda \
  --test watch_cli screenless_failed_first_turn
```

The scripted worker has **no screen** (`tty.screen` answers `supported:false`
and a stale `ready`) and a journal tail containing the two frames above.

Before the fix this is exactly the incident: empty screen lines, stale
`ready`, journal ignored → `working`. After the fix:

### `remuda watch --once`

```
NAME              STATUS  SHA/REASON                                   DETAIL
c-fail-<pid>      failed  API Error: 400 requested model is not a…      screen-unavailable; API Error: 400 requested mo…
```

The JSON row carries the untruncated reason and records the observation
timestamp on the roster:

```json
{
  "name": "c-fail-<pid>",
  "state": { "state": "working" },
  "watch": {
    "status": "failed",
    "reason": "API Error: 400 requested model is not available",
    "detail": "screen-unavailable; API Error: 400 requested model is not available",
    "observedAt": "2026-09-17T…Z",
    "lastActivityAt": "2026-09-17T…Z"
  }
}
```

`failed` is a watch status, not a new worker lifecycle: the durable roster
state stays `working` (a worker can be resumed/replaced; only
DONE/BLOCKED lines and explicit retirement move lifecycle state, as before).

### `remuda report`

```
== fleet ==
{"failed":1}

== needs attention ==
  c-fail-<pid> (failed): API Error: 400 requested model is not available screen-unavailable
```

### `remuda report --for-owner --json`

```json
{"items":[{"kind":"failed","name":"c-fail-<pid>",
"branch":"wt/c-fail-<pid>/brief-md",
"reason":"API Error: 400 requested model is not available"}]}
```

The failed worker appears once; a second `--for-owner` run is silent (the
change marker is unchanged).

## Rules now encoded

In `crates/remuda-protocol/src/worker.rs` (`classify_screen`) and
`crates/remuda-hub/src/worker_watch.rs` (`observe_one`):

1. lifecycle `failed`, or `exited` together with an errored turn / error
   message, or a last turn result of `error` → `failed <reason>`, regardless
   of screen availability and even if the carrier is briefly unreachable.
2. The reason is the first line of the last assistant error message from the
   screen or the journal; else the instance row's lifecycle reason code;
   else `instance-failed`.
3. A *clean* exit (print worker exits 0 after reporting DONE) stays `gone`;
   the journaled `DONE <sha>` is still classified and moved to done.
4. With no screen, DONE/BLOCKED report matching, echo suppression and the
   idle-after-API-error rule run over journaled assistant text; the detail is
   prefixed `screen-unavailable` so the provenance is visible even in the
   48-char table column.
5. A terminal Hub row (`failed`/`exited`/`closed`) now wins over a stale
   non-empty lifecycle from the screen carrier; the carrier only wins while
   the Hub row is non-terminal.
6. `remuda report` lists failed workers under needs attention and emits a
   `failed` owner ask (diffed, once per change). A journal showing an API
   blip while the turn is still retrying remains `idle-api-error` — a nudge,
   not an owner ask (`screenless_idle_after_api_error_is_nudge_not_failure`).

The journal scan reads a capped tail (256 events) from the instance journal
via the new `Store::read_journal_tail`, so a long-lived worker never pays for
a full journal replay on a watch tick.

## Coverage

- Hub roster tests (`crates/remuda-hub/tests/watch.rs`): failed first turn
  persisted with timestamps and sticky across observations; failed carrier
  lifecycle without a journal; screenless idle-after-API-error; all previous
  classes stay green.
- CLI tests (`crates/remuda/tests/watch_cli.rs`): screenless failed first turn
  in the table, full digest and `--for-owner` (exactly once); screenless
  idle-after-API-error excluded from owner asks; screenless DONE from the
  journal with the second observation correctly treated as an echo.
- Pure classifier unit tests in `remuda-protocol`: failed lifecycle reason
  selection, turn error beating a stale `ready`/unreachable carrier,
  failed-exit vs clean-exit, screenless journal DONE/BLOCKED/echo.
