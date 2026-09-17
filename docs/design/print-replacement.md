# Replacing `claude-print` with `claude-sdk`

**Date:** 2026-09-18.
**Status:** research and design only. No product code in this change.
**Inputs:** `/tmp/ccide/REPORT.md` (VS Code / Trae extension `anthropic.claude-code@2.1.273` plus the bundled CLI binary, with chunk offsets); Remuda docs and sources listed in [evidence](./evidence/print-sdk-research-1.md).
**Product constraints:** Remuda does not implement an agent loop ([D-002](./decisions.md)). Native PTY (`shell-pty`) stays the default carrier so Terminal and Structural views can stay in sync on one instance ([D-028](./decisions.md), [D-028a](./decisions.md)). `claude-print` is already explicit-only diagnostic ([D-035](./decisions.md)): a print session ends after one turn, so it is useless as a worker carrier. This document designs the structured replacement.

Unknowns stay unknown. Extension claims cite a REPORT.md section (and, where this worker re-read the installed bundle, the evidence file). Remuda claims cite a file path plus symbol.

---

## 1. How the VS Code extension talks to Claude Code

There is no separate process manager. On activate the extension starts a loopback MCP server and writes a lock file; conversation traffic is Agent SDK `query()` over stdio NDJSON; the WebSocket is editor tools only. REPORT §0.

### 1.1 Which binary is spawned

`Ti0()` / `H8$()` resolve, in order: `resources/native-binaries/<platform>-<arch>/claude` (musl variant when `Pi0()` sees musl), then `resources/native-binary/claude`, else throw `unsupported_platform`. A configured `claudeCode.claudeProcessWrapper` replaces the executable and passes the resolved binary as its first argument. REPORT §2.2.

The spawned binary is **the extension's bundled native CLI**, not `PATH`'s `claude`. First-hand: the live 2.1.273 bundle is 212 228 880 bytes at `resources/native-binary/claude`; REPORT appendix notes that CLI install `2.1.273` is byte-identical to that file (different inode). This worker did not copy install paths into this document.

### 1.2 Environment set and stripped

`L7()` builds the child env. REPORT §2.3, §8.2. First-hand in `extension.js` (same strings):

| Action | Names |
| --- | --- |
| Set | `MCP_CONNECTION_NONBLOCKING=true`, `CLAUDE_CODE_ENABLE_TASKS=0`, `CLAUDE_CODE_ENTRYPOINT=claude-vscode` |
| Overlay | `claudeCode.environmentVariables[]`; optional detected shell `PATH` |
| Strip | `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `TRACEPARENT`, `TRACESTATE` |
| Terminal collection only (not spawn env) | `CLAUDE_CODE_SSE_PORT=<port>` via `nr$()` so a terminal-launched `claude` can find this window |
| SDK itself | `CLAUDE_AGENT_SDK_VERSION=0.3.273`; if `CLAUDE_CODE_ENTRYPOINT` is unset, the SDK sets `sdk-ts` (it does not overwrite an existing value). REPORT §2.3, first-hand `if(!K.CLAUDE_CODE_ENTRYPOINT)K.CLAUDE_CODE_ENTRYPOINT="sdk-ts"` |

The extension therefore wins the entrypoint race by writing `claude-vscode` before `query()`. Remuda must not copy `claude-vscode` (that would impersonate the IDE). See §2.1 and §4.1.

Windows terminal path also sets `NoDefaultCurrentDirectoryInExePath=1`. REPORT §8.2.

### 1.3 Argv: stream-json in and out over stdio NDJSON, not a PTY

SDK `initialize()` starts argv as:

```text
--output-format stream-json --verbose --input-format stream-json
```

Then optionally `--thinking` / `--max-thinking-tokens` / `--thinking-display`, `--effort`, `--max-turns`, `--max-budget-usd`, `--model`, `--agent`, `--betas`, `--json-schema`, `--debug-file`, `--permission-prompt-tool stdio` (when `canUseTool` is set), `--continue`, `--resume=<id>`, `--allowedTools`, `--disallowedTools`, `--tools`, `--mcp-config`, `--setting-sources=…`, `--strict-mcp-config`, `--permission-mode`, `--allow-dangerously-skip-permissions`, `--include-hook-events`, `--include-partial-messages`, `--add-dir`, `--session-id=…`, and extraArgs. REPORT §2.4; first-hand the same `let d=[...]` builder in `extension.js`.

**There is no `-p` / `--print` in that builder.** First-hand: the only `"-p"` in `extension.js` is `ps -o lstart= -p` (process start time), not Claude print. Spawn is `spawnLocalProcess({command, args, cwd, env, signal})` with pipes on stdin/stdout. REPORT §2.4: the conversation is NDJSON over stdio, not a PTY.

The bundled CLI `--help` still says `--input-format` / `--output-format` / `--include-partial-messages` "only work with `--print`". That help text is incomplete. CLI `X$r()` treats a session as non-interactive when **any** of `-p`/`--print`, `--init-only`, `--sdk-url`, **or `!process.stdout.isTTY`** is true (`chunk_00321_169560821.js` @3052). The SDK child has piped stdout, so it is non-interactive without `-p`. `-p` is the extra "Print response and exit" flag (help text; D-035). That is the print defect, not stream-json itself.

### 1.4 Terminal launch path (the other path)

Two mutually exclusive launches. REPORT §2.5.

| | Default WebView | `claudeCode.useTerminal=true` |
| --- | --- | --- |
| Process | SDK `query()` spawn | `window.createTerminal()` then `shellIntegration.executeCommand` (3 s timeout, then `sendText`) |
| UI | React webview | VS Code terminal |
| Events | SDK message stream → `postMessage` | none (user watches the TUI) |

The structured experience the owner likes is the **WebView / SDK** path. The terminal path has no SDK event stream. Remuda's differentiator (Terminal + Structural on **one** instance) is not how the extension is built; it is `shell-pty` plus hook/transcript ([D-028](./decisions.md) §4, [live-structured-view.md](./live-structured-view.md)).

### 1.5 SDK options the extension actually passes

`spawnClaude()` builds an options object and calls `Bk$({options:Z}).query($)`. REPORT §5.1. First-hand fields on `Z`:

| Option | Extension value | Notes |
| --- | --- | --- |
| `cwd` | channel cwd | |
| `resume` | session id or undefined | passed through to `--resume=` |
| `canUseTool` | `requestToolPermission` | stdio permission callback; not the IDE WebSocket. REPORT §7.1 |
| `onUserDialog` | callback `U` | **not** listed on the public TypeScript `Options` table fetched 2026-09-18 from `code.claude.com/docs/en/agent-sdk/typescript`. Bundled SDK class fields include it. Treat the public shape as unknown; the call site is observed. |
| `supportedDialogKinds` | `V` | same as `onUserDialog`: call-site only |
| `permissionMode` | launch mode | illegal / unauthorized bypass is downgraded first. REPORT §7.3 |
| `resolvePermissionModeInCli` | `!claudeProcessWrapper` | wrapper means the CLI/wrapper resolves mode |
| `allowDangerouslySkipPermissions` | settings | required for `bypassPermissions` |
| `model` | launch model | |
| `systemPrompt` | `{type:"preset", preset:"claude_code", append, snapshot:true}` | |
| `enableFileCheckpointing` | `true` | |
| `thinking` | launch thinking | |
| `includePartialMessages` | `!remoteName` | off over SSH/container. REPORT §5.1 |
| `hooks` | PreToolUse Edit/Write/MultiEdit baseline + Edit/Write/Read save; PostToolUse Edit/Write/MultiEdit diagnostics | SDK hooks, not the IDE WS. REPORT §5.1 |
| `settingSources` | `["user","project","local"]` | |
| `extraArgs` | `debug`, `debug-to-stderr`, `enable-auth-status`, `no-chrome`, `replay-user-messages` (null = flag present) | |
| `mcpServers` | `K` | |
| `pathToClaudeCodeExecutable` / `executableArgs` / `env` | from `getClaudeBinary()` after Python venv optional merge | |
| SDK version pin | `CLAUDE_AGENT_SDK_VERSION="0.3.273"` | tracks CLI 2.1.273. Public docs: SDK patch tracks bundled Claude Code patch |

Public `query({ prompt, options })` accepts `prompt: string | AsyncIterable<SDKUserMessage>`. The extension uses the iterable form so later UI turns can enqueue. REPORT §5.3.

### 1.6 How UI input re-enters the SDK input stream (one session, many turns)

WebView → extension `readFromClient()` is the only command entry. `io_message` with `message.type==="user"` calls `transportMessage`, which `enqueue`s onto a per-channel `qG` (hand-rolled `AsyncIterable`, iterate-once). `done` closes the iterable. REPORT §5.2–§5.3.

`x$$` writes a user NDJSON line to the CLI stdin, or `streamInput(iterable)` for the async source:

```text
{type:"user", session_id:"", message:{role:"user", content:[{type:"text", text}]}, parent_tool_use_id:null}
```

The child is not restarted per turn. Stdin stays open for the life of the channel. That is exactly what print does not do once `-p` has finished its one response.

`interrupt_claude` / `close_channel` / `launch_claude` are sibling commands on the same client stream. REPORT §5.2.

### 1.7 Partial-event assembly

When `includePartialMessages` is on, stdout carries `type:"stream_event"` frames. The webview feeds them to an assembler (`processStreamEvent($.event, $.parent_tool_use_id)`) and **does not render** raw `stream_event` / `tool_progress` / `auth_status` / `tool_use_summary` / `system` / `result` as transcript rows; the assembled assistant message is what paints. REPORT §5.4.

Public SDK: `SDKPartialAssistantMessage` is `{type:"stream_event", event, parent_tool_use_id, uuid, session_id}` and `parent_tool_use_id` is always null on stream events (subagent text needs complete messages + `forwardSubagentText`). Final assistant blocks are the authority; partials are deltas. That matches [D-028a](./decisions.md) item 3 and Remuda's existing `claude_print_stream.rs` `StreamState`.

### 1.8 Permissions over stdio (not the IDE socket)

Two different approval paths. REPORT §7.1.

**(A) Tool permission — `canUseTool`, stdio.** CLI control request → SDK callback → `sendRequest({type:"tool_permission_request", toolName, inputs, suggestions, …})` → webview dialog → `{type:"response"}` → Promise resolves to `{behavior:"allow"|"deny"|"ask", updatedPermissions?, updatedInput?}`. Chrome MCP tools matching `mcp__claude-in-chrome__` are auto-allowed when that MCP is connected.

**Suggestions re-check:** after the UI returns, the extension keeps only `updatedPermissions` entries that appear in this prompt's `suggestions` (`Ay$` / `zy0`). REPORT §7.1. The full `zy0` implementation is **unread** (REPORT §9 item 4); the whitelist *intent* is observed at the call site.

**Silent bypass downgrade:** (1) at launch, unrecognized `permissionMode` or `bypassPermissions` while `allowDangerouslySkipPermissions` is off → mode forced to `default` and a `system/status` `io_message` is pushed so the UI matches. REPORT §7.3. (2) on each permission response, `setMode` + `bypassPermissions` is dropped unless skip-permissions is on.

**(B) Diff visualization — `mcp__ide__openDiff`, WebSocket.** Not a permission decision. Accept/reject is a separate webview `accept_diff` / `reject` against `diffPanelRegistry` and two custom editor schemes. REPORT §7.1–§7.2. Out of scope for a Remuda driver (§1.10, §2.11).

AskUserQuestion rides the same `canUseTool` channel (tool name `AskUserQuestion`). Remuda already reconstructs typed fields from `input.questions` in `claude_print.rs` `question_request`.

### 1.9 Subagents and background tasks

- Spawn env sets `CLAUDE_CODE_ENABLE_TASKS=0`. REPORT §2.3. Why the extension disables tasks is **not** explained in the report.
- Public SDK: `agents` defines subagents; `forwardSubagentText` forwards nested text/thinking with `parent_tool_use_id`; `agentProgressSummaries` emits `task_progress`.
- Extension `extraArgs` includes `replay-user-messages`. Remuda print already passes `--forward-subagent-text` and `--replay-user-messages` (`SpawnSpec`, `claude_argv`).
- Print mapper already understands `system/task_started|task_progress|task_updated|task_notification` and `background_tasks_changed` (`claude_print.rs` `map_system`).
- `SubagentStop` on the PTY hook path must not mean working ([native-pty-first.md](./native-pty-first.md) §3.1). Same rule if those frames appear on stdout.

Whether Remuda should copy `CLAUDE_CODE_ENABLE_TASKS=0` is an open product question (§2.8). Do not copy it blindly.

### 1.10 Separate editor channel (not the conversation)

On activate, `rr$()` creates `McpServer` + `http.createServer` + `WebSocketServer` listening on `127.0.0.1`, `authToken = crypto.randomUUID()`, lock file `~/.claude/ide/<port>.lock` (mode `0600`, dir `0700`) with `{pid: process.ppid, workspaceFolders, ideName, transport:"ws", runningInWindows, authToken}`. REPORT §3.

CLI scans that directory, matches cwd / `CLAUDE_CODE_SSE_PORT`, connects `ws://127.0.0.1:<port>` with header `X-Claude-Code-Ide-Authorization` and subprotocol `mcp`, registers the client as the fixed name **`ide`**. REPORT §4.1–§4.2. Unauthorized sockets `close(1008)`. One WS client at a time. `ws-ide` has no idle timeout. REPORT §3.3, §4.5.

This channel carries **editor tools and notifications only**: `mcp__ide__openDiff`, diagnostics, tabs, open file, selection, Jupyter `executeCode`; notifications `ide_connected`, `diagnostics_changed`, `selection_changed`, `at_mentioned`. REPORT §4.4, §6. **It does not carry the conversation.** Conversation is stdio NDJSON.

**Diff accept / reject** is UI → `accept_diff` / `rejectProposedDiff` on the custom scheme providers, not a stdio control request. REPORT §7.1–§7.2. Two parallel command families (`claude-code.*` and `claude-vscode.*`) are both live. REPORT §7.2, §9 item 7 (trigger conditions between the two still unknown).

Remuda is not an IDE. This driver does **not** implement the lock-file MCP server, does not write lock files, does not auto-install the extension (`MNn` / `--install-extension`; REPORT §8.4 — do not trigger that path), and does not copy tokens, lock contents, hostnames, or home paths into the repo.

### 1.11 What this path cannot do

- A Terminal view on the same child. Stdio is not a PTY. REPORT §2.4–§2.5.
- IDE tools, selection, diagnostics, proposed-diff chrome.
- Keeping Terminal and Structural in sync on one process — the extension splits those into two launch paths.
- A stable public transport contract. The wire is the CLI's stream-json plus the SDK's argv/env conventions. It moves with the pinned CLI. §4.1.
- `onUserDialog` as a documented public option (call site only).
- IPv6-only WSL IDE discovery (REPORT §9 item 10; not our path).

### 1.12 What the report left open (plus SDK API surface)

REPORT §9 still unread or inferred, restated without guessing:

1. `ydn(pid)` / `y()` liveness — call sites only; two different `ydn` symbols exist.
2. `Len()` sort key — `stat` observed, comparator not read.
3. `eGe()` before `v_("ide", w)`.
4. `dh$` / `Ay$` / `zy0` whitelist implementation — **inferred** from the filter call.
5. `Tot(e)` display-name fallback.
6. `_8` IDE registry contents.
7. Which of the two diff schemes is chosen when.
8. Chunk boundaries ≠ module boundaries.
9. Full webview message union (5.2 MB bundle, directed grep only).
10. IPv6 WSL `ip route` fallback untested.

**SDK API surface (this worker):** public `query()` / `Options` / `startup()` / `CanUseTool` / `includePartialMessages` / `resume` / `permissionMode` / `hooks` / `forwardSubagentText` are documented. `onUserDialog` and `supportedDialogKinds` are **not** on that table. Exact mapping from every `Options` field to CLI argv is the minified `initialize()` builder, not a versioned schema. `ClaudeSDKClient` vs `query(AsyncIterable)` for long sessions: public Python docs distinguish them; the extension uses `query` + iterable. Whether a custom `CLAUDE_CODE_ENTRYPOINT` other than the known set is accepted is unread (known set for one telemetry helper: `sdk-ts|sdk-py|sdk-cli|local-agent|claude-desktop|claude-desktop-3p` in `Da`, and a larger switch in `im()`).

---

## 2. Proposed driver: `claude-sdk`

The report supports an SDK entrypoint: `spawnClaude()` → Agent SDK `query()`, log line `Spawning Claude with SDK query function`, argv from SDK `initialize()`, env `CLAUDE_AGENT_SDK_VERSION`. **Name the driver `claude-sdk`.** Wire value `claude-sdk`. Kind remains `claude`.

Do **not** vendor `@anthropic-ai/claude-agent-sdk` and do **not** start with a Node sidecar. Speak the same NDJSON the SDK already writes, from Rust, through `remuda-claude-wire`. [D-003](./decisions.md) already reserved `AdapterTransport::ClaudeSdkSidecar` (`enums.rs`) as the fallback if handwritten stream-json cannot track the CLI; [plan-phase0.md](./plan-phase0.md) currently says a sidecar would still use `driverKind=claude-print` — that sentence is superseded: sidecar, if ever, is `driverKind=claude-sdk` with `adapterTransport=claude-sdk-sidecar`. Default transport is `native-rust-wire` (same as print).

The driver is a transport and a mapper. It does not choose tools, does not write prompts, does not implement Workflow. [D-002](./decisions.md).

### 2.1 Process model (the print defect)

One long-lived child per instance. Stdin held open for the instance lifetime. Each `Driver::send(Prompt)` is another `user` NDJSON line, the same as `qG.enqueue` / `streamInput`. The child exits when stdin is closed (after in-flight work) or when killed — `ClaudeProcess::close_stdin` (`process.rs`).

Print's argv **always** starts with `-p` (`SpawnSpec::argv`, `materializer.rs` `claude_argv` for `DriverKind::ClaudePrint`). `-p` is "Print response and exit". Combined with D-035 (a print worker cannot be nudged or steered; resume is mandatory after one turn), that is the carrier failure. The wire crate already keeps reading after the first `result` (Workflow emits two) and `send_user` can write another turn; the CLI still leaves when print mode is on. Review of print even documented "stdin stays open across turns" in-process — the process nonetheless ends. **claude-sdk omits `-p`.** Non-interactive comes from piped stdout (`X$r`), matching the extension.

Do not pass `--bare`, `--safe-mode`, `--no-session-persistence`, `--continue` (`flags.rs`, `claude_argv` refuse). The SDK *can* pass `--no-session-persistence` when `persistSession===false`; Remuda never does.

Handshake: keep `ClaudeProcess::spawn_command`'s `initialize` + wait for matching `control_response`. The hidden CLI flag `--await-initialize` exists for "first stdin line is initialize"; the wire crate already does that write. Adding the flag is optional parity, not a new protocol.

`CLAUDE_CODE_ENTRYPOINT`: do **not** set `claude-vscode`. Prefer leaving it unset (CLI default telemetry `cli`) or setting a Remuda-specific value if a later probe shows the CLI requires a known SDK token. `sdk-ts` is the SDK default; using it while we are not the TS SDK is slightly dishonest. Unset is the honest default until probed. Strip the same identity-leak names print already strips via `child_env.rs` (`env_clear` + allowlist). Do not inherit `CLAUDE_CODE_EFFORT_LEVEL` ([D-028a](./decisions.md) item 5). Do not copy `CLAUDE_CODE_ENABLE_TASKS=0` unless product decides it (§2.8).

### 2.2 Resume across turns, and where the session id comes from

Same rules as [D-026](./decisions.md): do not resurrect a dead process; spawn a new instance with `--resume <id>`; parent/child bookkeeping unchanged.

Session id authority is native:

1. Live: `system/init.session_id` → `map_init` already copies it onto the mapper and emits lifecycle `session` / `started` (`claude_print.rs` `map_init`). Node already lifts that into `nativeRef` for print.
2. Launch: `SessionAction::New` still passes `--session-id` so the CLI does not invent a different one; `SessionAction::Resume` passes `--resume` (RESERVED to materializer).
3. SDK also accepts `options.resume` → `--resume=`. Use the two-arg form Remuda already uses unless a wire-parity fixture shows the equals form is required.
4. Missing id → 409, expired → 409. No silent empty conversation.

In-session turns do **not** use resume. They are further `user` frames on the open stdin. Resume is only for a new process continuing the same native JSONL.

### 2.3 Permissions, AskUserQuestion, broker, steer / queue / interrupt

Reuse, do not invent.

| Concern | Existing symbol | claude-sdk |
| --- | --- | --- |
| Host permission policy | `PermissionPolicy::Host` + `handle_can_use_tool` (`claude_print.rs`) | same: `can_use_tool` → `ObservationPayload::InteractionRequested`, `InteractionCarrier::ClaudeControl` |
| Broker | `InteractionBroker` (`interaction.rs`), Node `InteractionRuntime` (`remuda-node/src/interactions.rs`) | same first-writer-wins CAS; `respond_interaction` writes `control_success` |
| AskUserQuestion | `question_request` / `permission_from_answer` | same; tool name `AskUserQuestion` |
| PTY hook path | `remuda-signal` `approval_interaction` / `question_interaction`, `InteractionCarrier::HarnessHook` | **not** this driver. shell-pty keeps hooks |
| Screen path | D-022, `NativeTty` | **not** this driver. No TTY |
| `onUserDialog` | none today | if a control subtype arrives that print already maps as `Unknown`, keep opaque until a fixture names it. Do not guess a new InteractionKind |
| Steer | `DriverInput::Steer` — print returns `CapabilityUnknown` | until measured on this transport, stay `unknown` ([D-028a](./decisions.md) item 2). A second `user` frame during a turn *may* be native queue/steer; do not claim it |
| Queue | D-022 `pty_queue` is PTY | claude-sdk has no PTY queue. Composer "queue" is either native (unverified) or Remuda-held until `result`/`turn_done`. Capability: `unknown` until a fixture shows `queue-operation` on this transport |
| Interrupt | `ClaudeProcess::interrupt` (`control_request` / `interrupt`) | **native**. Map `instance.cancel` here, not `\x03`. `instance.close` still closes stdin then wait, then process-group kill if needed |
| Model switch | print `set_model` control | keep. Effort in-session is unsupported on print (measured 2.1.221 `set_settings` ignored); do not lie — `CapabilityUnsupported` until a stream-json effort channel is measured. Launch `--effort` still works |

Bot/Agent never bypass ([D-011](./decisions.md), [D-017](./decisions.md)): `reject_bot_bypass` + materializer `BypassNotAllowedForBot` stay on this driver.

### 2.4 Suggestions whitelist and bypass downgrade (Remuda-side guards)

Implement the extension's two guards in Remuda, not by trusting the CLI.

1. **Launch downgrade.** If the spec asks `bypassPermissions` and origin is Human/Bot but the equivalent of `allowDangerouslySkipPermissions` is off, **refuse or downgrade to `default` with a journaled diagnostic** — do not spawn bypass. Prefer refuse with a reason code over silent downgrade when the caller is an API (Hub/MCP); the extension silences because it owns the UI. Remuda's Hub can show the error. Agent origin already errors (`BypassNotAllowedForBot`).
2. **Per-answer whitelist.** When `can_use_tool` carries `permission_suggestions` (`CanUseToolRequest` in `types.rs`), drop any `updatedPermissions` the human/UI did not just get offered. Drop `setMode`/`bypassPermissions` unless this instance was launched with explicit Human/Bot bypass. Print's `permission_from_answer` currently never sends `updatedPermissions` (always `None`) — keep that as the safe default; if a later batch threads "always allow" through, the whitelist is mandatory.

`zy0` itself stays unread. The Remuda guard is specified here; the CLI helper is not copied.

### 2.5 Observations → `ObservationKind` and the Structural view

Reuse print's `Mapper` / `StreamState` / `TranscriptMapper`. Stamp `driver_kind: ClaudeSdk` and `channel: SourceChannel::Stdout`. Do not grow `ObservationPayload`.

| Native stdout | `ObservationKind` | Notes |
| --- | --- | --- |
| `assistant` text / `stream_event` text delta | `message` | `StreamState`: open/append/close; `ContentStatus::Streaming` then `Complete`. Final assistant block authoritative ([D-028a](./decisions.md) item 3; [live-structured-view.md](./live-structured-view.md) §2.5) |
| thinking / redacted_thinking | `thought` | |
| tool_use | `tool_call` | |
| tool_result | `tool_result` | own node, `tool_call_id` join (`NativeIds::tool_result_*`) |
| `can_use_tool` | `interaction.requested` | broker answers → `interaction.answered` |
| `system/init` | `lifecycle` (session started) + session id | |
| `result` | `lifecycle` turn_done / error + `usage` | `affects_completion` only when terminal (`map_result`) |
| `system/task_*` | `lifecycle` Task / tool_result replace | |
| `permission` / permission-mode records | `permission` | |
| assistant `model` / `effort` | `model` / `effort` | effective only; never display requested as fact |
| unknown `type` | `opaque` | decode never fails (`remuda-claude-wire`) |

Structural view is this journal. Completeness `Structured` on control frames, `Partial` on stream deltas until the final block. No `raw_tty`. Signal tier: this carrier has no hook/file/OSC/screen ladder; report `SignalTier::None` (or a future stdout-equivalent if protocol grows one — do not overload `Hook`). Runtime capabilities win over the static matrix ([D-028](./decisions.md) §4.3 `capability_set_with_runtime`).

### 2.6 Coexistence with `shell-pty` under one instance id

**Decision: `claude-sdk` is a second carrier for kind `claude`, not a structured source bolted onto a `shell-pty` instance.**

Reasons:

1. The SDK child is stdio NDJSON, not a PTY (REPORT §2.4). Interactive TUI rejects the headless flags ([native-pty-first.md](./native-pty-first.md) §3.1). One child cannot be both.
2. The extension itself uses two launch paths, not one fused process (REPORT §2.5).
3. Two children (TUI + SDK) would be two conversations, two session ids — not "one instance".
4. [D-028](./decisions.md) unification ("one agent session is one terminal session") continues to mean **`shell-pty`**. That is where Terminal and Structural stay in sync (hooks > transcript > OSC > screen). `claude-sdk` replaces **print's** job: typed structured control on hosts with no TTY, and for callers that want `can_use_tool` without scraping.

Implications:

| Surface | Rule |
| --- | --- |
| Kind/driver matrix | Add `(claude, claude-sdk)`. Keep `(claude, shell-pty)` as the default when `driverInventory` says launchable ([D-035](./decisions.md) order). Keep `(claude, claude-print)` explicit diagnostic until retired. No hot-swap on a live instance (`architecture.md`: mutually exclusive carriers). |
| `validate_kind_driver` | New arm in `remuda-node/src/runtime.rs`. |
| `NativeClaudeFactory` | New `DriverKind::ClaudeSdk` factory next to print (`native.rs`). |
| Capabilities | `TtyAttach` / `LiveAttach` **NotProvided**. `InteractiveApproval` / `Question` / `Resume` / `CompletionNativeTurn` **Supported** (same evidence as print, plus multi-turn stdin). `Steer` / `Queue` **Unknown** until measured. `Interrupt` **Supported** via control `interrupt` (stronger than print's matrix, which still says Unknown for interrupt — tell the truth once the control request is wired). |
| Signal-tier reporting | `None` on this carrier. Promoted `shell-pty` keeps Hook/File/OSC/Screen. Do not pretend stdout is `Hook`. |
| Terminal view | **Does not exist** on a `claude-sdk` instance. UI must not show an empty xterm or offer `tty.attach`. Structural view is the surface. Resume-into-terminal remains D-026: a **new** `shell-pty` instance with `--resume`, not a second view on the dead stdio child. |
| Defaulting | Never silent-fallback to print or to sdk. Omitted driver follows D-035: `shell-pty` if launchable, else herdr `claude-pty`, else refuse. `claude-sdk` is explicit (`--driver claude-sdk` / web picker), or a later owner decision to make it the structured-only default for non-TTY hosts — not this design silently flipping Hub `driver_for`. |

### 2.7 Gateway and model overlay ([D-012](./decisions.md))

Reuse print/shell-pty materializer:

- `Delegation::None` (default): native login, `inherit_default_config` allowed the same way as print (`ClaudePrintOptions.inherit_default_config`, Node `native.rs`). No `ANTHROPIC_BASE_URL` invented.
- `Delegation::Gateway`: `claude_settings_json` writes `ANTHROPIC_BASE_URL` + `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` and optional `apiKeyHelper` (`materializer.rs`). Same 0700 helper, same secret broker. No astergate-named APIs.
- `Delegation::Direct`: still `DirectDelegationV2` error.

Gateway probe before trusting live: whether stream-json **without** `-p` honours the same `--settings` overlay and `apiKeyHelper`; whether `includePartialMessages` `ping` frames keep a slow gateway from looking dead (public SDK note around v2.1.257). Until probed, mark gateway-on-sdk `unknown` in capabilities rather than shipping it as supported. Print's gateway path remains the evidence baseline.

### 2.8 What stays print-only, and what retiring print loses

Stay print-only until the batches in §3 land and a fixture parity gate is green:

- `DriverKind::ClaudePrint` and every test that hard-codes `-p` (`remuda-testing/src/client.rs` `spawn_fake_claude`, `SpawnSpec::argv`).
- M0 `dontAsk` auto-deny debt (`PermissionPolicy::AutoDeny`) — do not advertise it on sdk; Host or explicit bypass only.
- Any operator who still passes `--driver claude-print` (D-035).

After sdk is the structured carrier, retiring print loses nothing unique **if** the mapper, usage-from-`result`, base64 image blocks (`prompt_content`, D-027), AskUserQuestion, and non-TTY spawn all moved over. Those are print's remaining specials ([native-pty-first.md](./native-pty-first.md) §2.2). If sdk ships without image blocks or without usage, the usage page and clipboard path regress — so batch 2 must take both.

[native-pty-first.md](./native-pty-first.md) §12's "print retires when PTY parity is green" is the PTY track. This design is the **other** track: print's structured-only role is replaced by sdk, which unblocks retiring print even on hosts where `pty.fork` is still unproven — that was print's last non-TTY justification. Do not delete print source in these batches; stop using it as soon as sdk is registered and explicit-only print has a successor.

### 2.9 Out of scope for the driver

IDE lock files and MCP `ide` client; diff accept/reject; auto-install of the VS Code extension; vendoring the 212 MB binary or `extension.js`; billed live sessions in CI; entering the agent loop; fusing with `shell-pty`; `deploy/` anything; tunnel tools ([D-031](./decisions.md)).

---

## 3. Batches

Three worker-sized batches. Protocol enum additions in batch 2 are the only schema bump; run `just gen-types` there. No edits to `decisions.md` in those PRs — the row draft is below for the owner/coordinator.

### Batch 1 — Wire argv and a fake that survives two turns

**Owns:** `crates/remuda-claude-wire/src/process.rs` (`SpawnSpec::argv`), `crates/remuda-claude-wire/README.md`, `crates/remuda-claude-wire/tests/process.rs`, `crates/remuda-testing/src/fake.rs`, `crates/remuda-testing/src/flags.rs`, `crates/remuda-testing/src/client.rs`, new fixtures under `crates/remuda-testing/fixtures/claude/` (sdk-shaped, no secrets).

**Does:**

- SDK argv template: **no `-p`**. `--input-format stream-json --output-format stream-json --verbose`, plus the existing host permission flags when asked, `--include-partial-messages` default on, `--include-hook-events` / `--forward-subagent-text` / `--replay-user-messages` as today. Forbidden flags unchanged.
- `fake-claude` accepts that argv (does not require `-p`), keeps stdin open, emits a second `result` when a second `user` frame arrives, exits only on stdin EOF. Today `run_fake_claude` already loops until EOF; `spawn_fake_claude` still injects `-p` — stop requiring it.
- Golden argv test: print template still has `-p`; sdk template must not.

**Does not:** DriverKind, Node, Hub.

**Tests:** `cargo test -p remuda-claude-wire`; `cargo test -p remuda-testing` for fake two-turn; no network.

### Batch 2 — Driver, mapper reuse, permission guards, capabilities

**Owns:** `crates/remuda-protocol/src/enums.rs` (`DriverKind::ClaudeSdk => "claude-sdk"`), generated schema/TS, `crates/remuda-driver/src/claude_print.rs` (extract shared mapper or parameterize `driver_kind`), new `crates/remuda-driver/src/claude_sdk.rs`, `crates/remuda-driver/src/lib.rs`, `crates/remuda-driver/src/materializer.rs` (`claude_argv` arm, `permission_plan` host+stdio like print, `InputDelivery::Stdio`), `crates/remuda-driver/src/capabilities.rs`, `crates/remuda-driver/src/fake.rs`, `crates/remuda-driver/tests/` (new `claude_sdk_*.rs` plus recorded NDJSON under `tests/fixtures/`), `crates/remuda-testing/fixtures/scripts/` if a new script is easier than duplicating print's `ok`/`askuser`/`approval`.

**Does:**

- `ClaudeSdkDriver` is print without `-p`, `DriverKind::ClaudeSdk`, stdin left open across `send`. Share `Mapper`, `handle_can_use_tool`, `prompt_content` (keep D-027 base64 images), `usage_from_result`.
- Suggestions whitelist + launch bypass refuse (§2.4).
- Capability matrix: TTY not provided; approval/question/resume/usage supported; steer/queue unknown; interrupt supported once `ClaudeProcess::interrupt` is the cancel path.
- `FakeDriver` can stamp `claude-sdk` for loopback.

**Tests (no network, no billed session):**

- Replay `ok.jsonl` / `askuser.jsonl` / `approval.jsonl` / a new `partial-then-final.jsonl` through the sdk driver: expected `ObservationKind` sequence, one message id across stream deltas, final `complete` block.
- Two `send`s, two `result`s, process still alive (fake-claude).
- Agent origin + bypass → error.
- `updatedPermissions` outside suggestions dropped (unit).
- `attach` / `write_tty` → unsupported.

### Batch 3 — Node/Hub matrix, resume, gateway probe hook, wire-parity pin

**Owns:** `crates/remuda-node/src/runtime.rs` `validate_kind_driver`, `crates/remuda-node/src/native.rs` factory registration, Hub `driver_for` / web picker labels (sdk is explicit, never a silent default — D-035 order unchanged), `crates/remuda-driver/src/binary.rs` pin note if a version string is recorded, `docs/design/parity-whitelist.toml` only if journal-diff needs an sdk vs print allowance. Not `decisions.md`.

**Does:**

- Register the factory. Resume uses `map_init` session id (D-026). Gateway overlay reused; capability stays unknown until the probe in §4.2 is run under an explicit billed session.
- Wire-parity test: recorded NDJSON (redacted, no tokens, no home paths) captured from fake-claude **or** from a documented live capture in evidence. Assert argv template and frame `type`s against the pin: CLI **2.1.273** / SDK **0.3.273** as observed. Test fails if `-p` reappears or if `initialize` handshake is skipped.
- Inventory: `driverInventory` may list `claude-sdk` as launchable on non-TTY hosts; Hub still does not default to it in this batch.

**Tests:** Node create `(claude, claude-sdk)` accepted; `(claude, shell-pty)` unchanged; omitted driver still not print and not sdk; resume without session id → 409; secret-scan clean fixtures.

### Fixture strategy

- **Source of truth for protocol tests:** `fake-claude` + checked-in NDJSON under `crates/remuda-testing/fixtures/` and `crates/remuda-driver/tests/fixtures/`. Same pattern as print (`fake.rs`, `client.rs`, `ok.jsonl`).
- **Do not** replay bytes carved from the 212 MB binary or from `extension.js`.
- **Do not** hit the network or a billed model in CI. A live capture, if needed for parity, is an evidence file with `LIVE` disclosure, `--max-budget-usd`, redaction, and is not a gate.
- `fake-harness` ([testing-fake-harness.md](./testing-fake-harness.md)) stays the PTY double. It is the wrong layer for sdk stdio. Do not teach `fake-harness` stream-json.
- `remuda-driver` `fake.rs` remains in-process Observation replay for Hub/Web loopback; add a `claude-sdk` kind stamp so UI fixtures do not say `claude-print`.

### D-036 row draft (do not edit `decisions.md` in this worktree)

| # | 日期 | 决策 | 由谁 | 依据 |
|---|---|---|---|---|
| D-036 | 2026-09-18 | **用 `claude-sdk` 替换 print 作为 Claude 的结构化 carrier。** 扩展走 Agent SDK `query()`：自带 CLI、`--input-format/--output-format stream-json`、**没有 `-p`**、stdio NDJSON、stdin 跨多轮保持打开；IDE WebSocket 只承载编辑器工具，不是对话。Remuda 的 `claude-sdk` 用同一条传输（默认 `native-rust-wire` 手写 NDJSON，D-003 的 `claude-sdk-sidecar` 仅作跟不上官方变化时的退路），**不**把 SDK 打进仓库、**不**进 agent loop（D-002）。`shell-pty` 仍是默认 TTY carrier（D-028）：Terminal 与 Structural 同步是 PTY 实例的事。`claude-sdk` 是 kind `claude` 的**第二条 carrier**，不是挂在 `shell-pty` 上的结构化源；该实例没有 Terminal view。`claude-print` 维持 D-035 的显式诊断，直到 sdk 夹具对账变绿后再退役。权限/AskUserQuestion 复用 `InteractionRuntime` 与 `ClaudeControl`；suggestions 白名单与 bypass 降级做成 Remuda 侧护栏（D-011/D-017）。Steer/queue 未在该传输上实测则报 `unknown`（D-028a）。非目标：IDE lock/MCP、diff 接受拒绝、扩展自动安装、把 TTY 与 stdio 熔成一个进程。 | 用户（owner ask 2026-09-18） | [print-replacement.md](./print-replacement.md)；[print-sdk-research-1.md](./evidence/print-sdk-research-1.md)；REPORT.md §0–§9；D-002 / D-012 / D-028 / D-028a / D-035 |

**Impact on D-028:** does not reverse native-PTY-first. Adds an honest structured-only exception with no Terminal view, instead of stretching print.

**Impact on D-035:** print stays explicit-only until sdk exists; sdk must not become a new silent default.

**Non-goals:** listed in the row and §2.9.

---

## 4. Risks

### 4.1 SDK / CLI version coupling

The transport is not a stable public contract. It is "whatever this CLI build emits on stream-json stdio when stdout is not a TTY", plus the SDK's argv builder. Extension 2.1.273 pins `CLAUDE_AGENT_SDK_VERSION=0.3.273`. Public docs: SDK patch number tracks the bundled CLI.

**Pin:** record CLI version + digest on `BinaryPin` as today (`binary.rs`). Record the argv template in `SpawnSpec` tests. A wire-parity test (batch 3) asserts: no `-p`, stream-json in and out, `initialize` handshake, `system/init` then user/assistant/result shapes from fixtures. When `pin_binary` sees a newer CLI, fail the parity test until fixtures are re-recorded — do not silently accept a new `type` by dropping it on the floor without an `opaque` observation (the codec already keeps unknown as `Unknown(Value)`).

Do not vendor the SDK or the 212 MB binary. Optional future sidecar uses the published npm package behind `AdapterTransport::ClaudeSdkSidecar`, still as a mapper.

### 4.2 Gateway compatibility — what to probe

[D-012](./decisions.md) gateway is Anthropic-Messages compatible, settings overlay, `apiKeyHelper`. Probes (explicit billed session, evidence file, no tokens committed):

1. `Delegation::Gateway` + claude-sdk argv (no `-p`): does the CLI honour `ANTHROPIC_BASE_URL` from `--settings` the same as print?
2. With `includePartialMessages`, do `stream_event` `ping` frames arrive while the gateway holds the HTTP connection (public SDK note)? Remuda must not treat ping as content or as silence-timeout.
3. `can_use_tool` still appears with `--permission-prompts host --permission-prompt-tool stdio` without `-p` (print required both; 2.1.268 notes in `remuda-claude-wire/README.md`).
4. Resume after gateway turn: `system/init.session_id` still present.

Until 1 and 3 pass, do not advertise gateway-on-sdk as supported.

### 4.3 Licensing and attribution

- Do **not** copy `extension.js`, `webview/index.js`, the bundled CLI, lock files, or carved `/tmp/ccide/chunks`.
- `remuda-claude-wire` is already an original implementation; [NOTICE](../../NOTICE) says vibe-kanban was a protocol cross-check, not a copy. Keep it that way. New sdk argv is still original.
- If a sidecar is added later: NOTICE must name `@anthropic-ai/claude-agent-sdk` and its license; do not commit the SDK's native binary. Extract-from-bunfs helpers are Anthropic's; do not vendor them.
- Recorded fixtures are Remuda-generated (fake-claude) or redacted live captures we produced. No Anthropic source.

### 4.4 Stay out of the agent loop ([D-002](./decisions.md))

The driver maps frames to `Observation` and writes user/control bytes the Node already authorized. It does not:

- choose `allowedTools` / `disallowedTools` beyond what the spec/materializer already passes;
- inject a Remuda system prompt (the extension's `preset:"claude_code"` append is IDE-specific — do not copy `jn$`);
- auto-allow Chrome MCP or any tool;
- implement Workflow, skills, or compaction;
- answer `can_use_tool` except by forwarding the broker's decision.

Hooks in the extension capture baselines and diagnostics for the editor. Remuda's hooks live on the PTY overlay (`launch/overlay.rs`) for `shell-pty`. claude-sdk may pass `--settings` for gateway/helper the same as print; it does not register IDE PreToolUse matchers.

---

## Key decisions

1. **Name `claude-sdk`.** The extension's structured path is Agent SDK `query()`, not `claude -p`.
2. **Rust NDJSON, not a sidecar, by default.** Same child the SDK would spawn; `ClaudeSdkSidecar` remains the D-003 escape hatch.
3. **Omit `-p`; hold stdin open.** That is the print defect. Non-interactive comes from piped stdout.
4. **Second carrier, not a PTY overlay.** No Terminal view on sdk instances. `shell-pty` remains the default and the dual-view differentiator.
5. **Reuse print's mapper, broker, materializer gateway overlay, image blocks, usage.**
6. **Suggestions whitelist and bypass refuse are Remuda guards.**
7. **Steer/queue stay `unknown` until measured on this transport.**
8. **Print stays explicit diagnostic until sdk fixture parity; then it can retire.**
