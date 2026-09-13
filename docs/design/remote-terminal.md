# Remote terminal

## Web ↔ Hub contract (x-term / x-termui)

This is the attach / output / input / resize contract the web TerminalView must speak. Hub relays it to the Node. Do not invent a second envelope.

### Output

WebSocket **binary** messages: existing `tty-binary-v1` 32-byte header + payload (`protocol.md` §7.4).

| Byte | Value |
| --- | --- |
| 0 | framing version `1` |
| 1 | **channel output = `1`** |
| 2–3 | reserved `0` |
| 4–19 | stream UUIDv7 (no `tty_` prefix) |
| 20–27 | big-endian `u64` byte offset |
| 28–31 | big-endian `u32` payload length |

Payload is raw PTY / ANSI bytes (including mouse-related output). Do not interpret as UTF-8. JSON `tty.frame` with `dataBase64` is the stdio-carrier equivalent; the browser follow socket uses binary.

### Input

WebSocket **binary** messages: same 32-byte header, **channel input = `3`**.

- Payload is raw bytes **exactly as xterm.js emits them** (keyboard **and** mouse escape sequences). Do not filter, rewrite, or drop CSI / SS3 / SGR mouse reports.
- Stream UUID is the stream from attach (the UUID in output frames). Input `offset` may be `0`; the server ignores it.
- Size limit: `limits.maxTtyInputBytes` from hello (default **4096**). Larger frames are rejected.
- Write permission: only a device that can `POST /v1/instances/:id/commands` may send input. Other followers are output-only.

Convenience: JSON `tty.write` `{ "keys": ["enter", "esc", "ctrl+c", …] }` remains valid and is mapped onto the same raw bytes (`enter` = `\r`, `esc` = `\x1b`, `ctrl+c` = `\x03`, …). Prefer binary channel input from the web terminal.

### Resize

JSON text frame on the follow socket:

```json
{ "type": "tty.resize", "cols": 80, "rows": 24 }
```

`cols` and `rows` are positive `u16`. Hub relays Hub→Node `tty.resize { instanceId, cols, rows }`. Same write permission as input.

### Attach

`GET /v1/follow?instanceId=<ins_…>&tty=1` (device cookie, or `token=` query as today).

Optional JSON on an already-open follow socket:

```json
{ "type": "subscribe", "instanceIds": ["ins_…"], "tty": 1 }
```

On attach the Hub/Node **replays a snapshot first**, then live frames.

- Snapshot = a full-screen ANSI paint (herdr `terminal.frame` with `full: true`) **or** a bounded replay of the last **256 KiB** of PTY/ANSI bytes.
- The client **must reset the emulator** before writing snapshot bytes, then treat later frames as live.
- Snapshot and live frames use the same binary output envelope. The first output frame after attach has `offset = availableFrom`. Live frames continue from `nextOffset`.
- When the Node cannot be reached (offline, `tty.attach` failed), the Hub falls back to **its own** bounded output cache and sends it as a JSON text frame, because with no `streamId` from the Node there is no UUID to address a binary frame to:

  ```json
  { "type": "tty.snapshot", "instanceId": "ins_…", "source": "hub-cache", "dataBase64": "…" }
  ```

  Treat it exactly like a snapshot: reset the emulator, write the decoded bytes, then wait for live frames. It is a last resort — a live attach always wins — and it is the only snapshot frame that is not binary.

### Diagnostics

Input and resize failures are reported instead of being dropped. The Hub sends a JSON text frame on the follow socket:

```json
{ "type": "tty.diagnostic", "instanceId": "ins_…", "operation": "tty.write",
  "reason": "node-offline", "detail": "no live Node session for this host" }
```

`operation` is `tty.write` or `tty.resize`. `reason` is one of:

| `reason` | Meaning |
| --- | --- |
| `malformed-frame` | the binary input frame did not decode |
| `wrong-channel` | input arrived on a channel other than `3` |
| `payload-too-large` | payload exceeds `limits.maxTtyInputBytes` |
| `unbound-stream` | no instance is bound to that stream UUID and nothing is subscribed |
| `unknown-instance` | the Hub has no record of the instance |
| `lookup-failed` | the Hub could not read the instance row |
| `node-offline` | no live Node session for the owning host |
| `node-error` | the Node rejected the call; `detail` carries its message |
| `call-failed` | the Hub→Node RPC itself failed |

A client that ignores these frames behaves exactly as before; a client that shows them tells the user why the keyboard stopped working. Every case also logs a `tracing::warn!` on the Hub with the instance, host, and operation.

### Stream binding across a Node reconnect

The stream UUID → instance binding is held by the Hub against the **host**, not against the Node socket. A Node restart replaces the socket but keeps the host id, so a follow socket that was already open keeps receiving output as soon as the Node re-announces its streams (`tty.frame` with a `streamId`, i.e. `TtyEvent::Open`) — a re-attach also re-binds. A different host can still never push onto a stream it did not register.

### Instance kind

- `kind: "terminal"`, `driver: "shell-pty"`: a login `$SHELL` in a PTY (`TERM=xterm-256color`, mouse-capable). cwd / worktree honored. No agent detection. Fallback when an agent CLI is not recognized.
- `generic-pty` / `claude-pty` (herdr-carried): the same Web contract; Node attaches `herdr terminal session observe|control`.

---

## Design notes

### Attach / snapshot / backlog

One TTY bridge per live instance (not per follower). Node keeps a 256 KiB ring of output bytes (and, for herdr, prefers a `full` ANSI frame as the reconnect paint). `tty.attach` (Hub→Node) returns `streamId`, `streamEpoch`, `availableFrom`, `nextOffset`, and `snapshotBase64` so Hub can emit snapshot frames to **that** follower only. Later `tty.frame` binary output is multicast to every follow socket that subscribed with `tty=1`.

Closing the terminal panel is detach, not `instance.close`. A new attach never runs `instance.open_terminal` and never wakes a background job.

### Input channels

| Path | Envelope | Use |
| --- | --- | --- |
| Web follow WS | binary channel `3` | xterm.js raw bytes, including mouse |
| Hub→Node WSS / stdio | `tty.write` `{ instanceId, dataBase64 }` or the same binary envelope | relay of those bytes |
| CLI / MCP | `tty.write` `{ keys: […] }` | logical keys mapped to bytes |

All three hit the same PTY write with `acceptance.scope = tty-bytes`. ACK does not mean the agent understood a prompt.

### Resize

`tty.resize` is a state setting, not a Command. Older `resizeRevision` values (when present) are ignored; a body of `{cols, rows}` is enough on the follow socket. Default size before the first resize is 80×24.

### Placement capacity

A terminal is an Instance, so it is subject to the host's `maxInstances` ceiling. Two rules keep that ceiling honest:

- **Only Node-confirmed instances hold a slot** — `preparing`, `starting`, `ready`/`running`, `closing`, `reconciling`. A `requested` row is a Hub-side intent the Node has not acknowledged; counting those let stale creates wedge a host at its cap indefinitely. A `requested` row still counts inside the insert guard while it is younger than five minutes, so a burst of concurrent creates cannot overshoot.
- **Unacknowledged creates expire.** A `requested` instance with no Node receipt after `requestedGraceMs` (default five minutes) becomes `failed` with `lastError: "create-never-acknowledged"` and a Hub-authored journal diagnostic. The Hub also sweeps once at startup, so a restart does not inherit yesterday's zombies.

The operator ceiling is set with `PATCH /v1/hosts/{id} {"maxInstances": N}`. It is stored separately from the value a Node advertises in its inventory, so neither a `node.hello`/heartbeat nor a Hub restart resets it.

### Deleting a session

`DELETE /v1/instances/{id}` permanently removes a session. Human and Bot
devices only — agents receive `403`, including for their own instance, so an
agent can never erase its own trail.

| Condition | Result |
| --- | --- |
| lifecycle `exited` / `failed` / `closed` | deleted |
| still live, no `force` | `409`, nothing removed |
| still live, `?force=1` | stopped (settled `exited`, `lastError: deleted-by-operator`) then deleted |
| already deleted | `404` — a repeated `DELETE` is idempotent |

What is removed:

- **Hub**: the instance row, its journal, queued commands, interactions, and
  fleet membership. Leaving any of them behind would resurrect the session in a
  list view or keep a command queued against an id that no longer exists.
- **Node**, via `instance.purge`: its own instance/command/recipe rows and the
  per-instance data directory (`<data_dir>/instances/<id>`: launch artifacts,
  overlays, pty logs).
- **Never**: the agent's own native transcripts under the user's home
  (`~/.claude`, `~/.codex`, …). Deleting a Remuda session must not delete the
  user's own agent history.

A Node that is offline or rejects the purge does not block the delete — the Hub
row is what the user asked to remove, and the response reports which happened
in `nodePurge` (`purged` / `node-offline` / `node-rejected` / `purge-failed`).

Because the delete removes the journal, the record of *who* deleted it goes to
the Hub's `audit_log` table instead: device id, action `instance.delete`,
subject, and whether it was forced.

### Lost instances after a Node restart

A Node restart loses every in-memory instance while the Hub still holds `running` rows for them. Those rows never settle, never release their slot, and a later stop hangs. Three mechanisms close that:

1. **Reconcile on hello.** When `node.hello` announces a `nodeEpoch` different from the one recorded for the host *and* carries an `instances` inventory, every live Hub row the Node no longer lists becomes `exited` with `lastError: "node-epoch-changed"` plus a journal diagnostic `node_epoch_changed` (`payload.origin = "hub"`). A hello without an inventory reconciles nothing — a stateless Node must not wipe rows it simply never enumerates.
2. **Stops always settle.** If the Node answers `instance.close` / `instance.cancel` with a not-found error (`-32004`), the Hub projects the row to `exited` with `lastError: "node-lost-instance"` and marks the command `settled`. The caller gets an answer instead of a command that hangs forever.
3. **Requested rows expire** (see above).

All three are Hub-owned projections: they set the Hub's view and append a `payload.origin = "hub"` diagnostic, and never forge a Node journal cursor or a native completion. That is safe precisely because the Node that owned those rows will never emit another observation for them.

### Permissions

- Read: any authenticated device that can follow the instance.
- Write (input + resize): the same principal that may post instance commands. Node also enforces `maxTtyInputBytes`.
- One writer lease per instance when `tty.attach` mode is `write`; a lost lease returns `TTY_LEASE_LOST`. Other devices keep reading.

### Mobile constraints

xterm.js on a phone still emits the same byte sequences (including SGR mouse when tracking is on). The backend must not special-case mobile. Fit/IME/touch scrolling stay in the web client (`docs/design/plan-phase0.md` M3-02). Keep payloads small: one keystroke or one mouse report per input frame, under `maxTtyInputBytes`. Visual viewport and software keyboard do not change cols/rows until the client sends `tty.resize`.

---

## Terminal → agent promotion

**Decision:** [D-025](./decisions.md). **Evidence:** [terminal-promote-1.md](./evidence/terminal-promote-1.md).

A `terminal` / `shell-pty` instance is a login `$SHELL`. When the human types
`claude` in it, the session is an agent session in every way that matters — but
the instance kind, the 结构 tab and the composer still say "terminal". Promotion
closes that gap without a second driver: the PTY, the TTY bridge, the ring
buffer and the follow socket are untouched; only the *interpretation* changes.

### Detection

A poller inside `shell-pty` samples the live PTY roughly every second:

1. `tcgetpgrp(master_fd)` — the foreground process group of the PTY. This is the
   kernel's own answer to "what is the user talking to right now", so it follows
   job control (`ctrl+z`, `fg`, a pipeline) with no heuristics.
2. For every pid in that group, read the process name and argv
   (`ps -o pid=,comm=,args= -g <pgid>`).
3. Match against a small table of known agent CLIs. The first match wins.

| kind | matches | transcript hydration |
| --- | --- | --- |
| `claude` | `claude`, `claude-code` | yes (MVP) |
| `codex` | `codex` | no — detection + kind switch only |
| `grok` | `grok` | no |
| `agy` | `agy` | no |

The shell itself (`zsh`, `bash`, `sh`, `fish`, …) and the usual non-agent
foreground commands are ignored. `tcgetpgrp` failing (no controlling terminal
yet, a platform without it) is not an error: the poller falls back to a screen
signature over the ring buffer, which is weaker and only ever used as a
fallback. Detection latency is bounded by the poll interval, so a fresh `claude`
is visible within ~2 s.

### Promotion

On the first match the driver emits, in order:

1. a native diagnostic `agent detected: claude` (topic `diagnostic`, severity
   `info`), carrying `kind`, `pid` and — when known — the native session id;
2. an `agent_promoted` lifecycle whose `relatedIds` hold `kind`, `mode`
   (`promoted`), `promotedAt` and the session id.

Node folds the second into the Instance row: `kind` becomes the detected agent,
`driver` **stays** `shell-pty`, and two new fields appear —

| field | values | meaning |
| --- | --- | --- |
| `mode` | `native` \| `promoted` | `native` = the instance was created as this kind; `promoted` = a terminal became one |
| `promotedAt` | `Timestamp?` | when detection fired; absent on `native` |

`mode` is on `Instance` in `remuda-protocol`, mirrored through the Hub
instance row and the OpenAPI `Instance` schema, and read by the web client.
It is deliberately *not* a new `DriverKind`: the launch recipe, the capability
snapshot and the TTY contract above are all still `shell-pty`'s.

Promotion is idempotent. A second match for the same kind and pgid emits
nothing. A different agent in the same terminal (exit `claude`, run `codex`)
demotes and re-promotes, so the journal always has a matched pair.

### Demotion

When the foreground process group no longer holds the agent — `/exit`, `ctrl+d`,
a crash — the driver emits `agent detected: none` plus an `agent_demoted`
lifecycle, and Node restores `kind: terminal`, `mode: native`, clearing
`promotedAt`. Transcript tailing stops. The PTY and the terminal tab keep
working exactly as before: demotion is not a close.

### Structured messages

Promotion alone gives status and a kind. The 结构 tab needs the conversation,
and a native `claude` TUI does not speak stream-json. It does, however, write
the same transcript the SessionStart hook reports for `claude-pty`
([pty-trust-1.md](./evidence/pty-trust-1.md)):
`~/.claude/projects/<cwd with every '/' and '.' replaced by '-'>/<session>.jsonl`.

Locating the session:

1. If argv carried `--session-id <uuid>` or `--resume <uuid>`, that is the
   session, no guessing.
2. Otherwise: the newest `*.jsonl` under the encoded projects dir for the
   instance's cwd whose mtime is at or after the detection instant. A file that
   predates detection belongs to an earlier session and is never adopted.

A tailer then follows that file from byte 0, parses each line, and maps the
`user` / `assistant` records through the **existing** claude transcript mapper
(`claude_print::review::map_stdout_json`, the same one `claude-print` uses for
its stdout frames) into `message` / `thought` / `toolCall` observations with the
usual open→append→close mutations. Non-message records (`mode`, `atis-latch`,
`file-history-snapshot`, hook attachments) are skipped rather than journaled as
opaque noise. Truncation or rotation of the file resets the tailer to 0.

Observations from this path carry `channel: transcript` and
`completeness: structured` — they are the native record, not a screen scrape,
which is what makes the 结构 tab trustworthy here.

### Input

The composer switches from a terminal to a prompt box on a promoted instance.
`instance.send` on a promoted `shell-pty` writes the prompt to the PTY as text
followed by `\r`, bracketed-paste-wrapped (`ESC[200~` … `ESC[201~`) when the
text spans lines, so the TUI receives it as one paste and not as N submits. The
D-022 queue applies unchanged: while the screen heuristics report `blocked`, the
prompt waits rather than being typed into a dialog.

The terminal tab keeps full raw-byte input the whole time. Promotion adds a way
to talk to the agent; it never takes the keyboard away.

### Web

- **Header** shows the promoted kind and a `promoted` marker next to the driver,
  so `terminal` → `claude · shell-pty · promoted` is visible without opening the
  instance detail.
- **结构 tab** renders the hydrated transcript through the normal `Transcript`
  component instead of `ScreenView`. A promoted instance with no transcript yet
  shows the screen snapshot until the first message lands.
- **Composer** switches to agent mode (prompt box, permission/model chips) while
  the 终端 tab continues to speak the binary TTY contract above.
