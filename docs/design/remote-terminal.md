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

### Permissions

- Read: any authenticated device that can follow the instance.
- Write (input + resize): the same principal that may post instance commands. Node also enforces `maxTtyInputBytes`.
- One writer lease per instance when `tty.attach` mode is `write`; a lost lease returns `TTY_LEASE_LOST`. Other devices keep reading.

### Mobile constraints

xterm.js on a phone still emits the same byte sequences (including SGR mouse when tracking is on). The backend must not special-case mobile. Fit/IME/touch scrolling stay in the web client (`docs/design/plan-phase0.md` M3-02). Keep payloads small: one keystroke or one mouse report per input frame, under `maxTtyInputBytes`. Visual viewport and software keyboard do not change cols/rows until the client sends `tty.resize`.
