# Evidence: print → claude-sdk research (2026-09-18)

Worker: c-printsdk. Docs only. No product code, no `cargo build`, no billed model session, no extension install, no auto-install path, no lock-file or token copy into the repo.

Companion design: [print-replacement.md](../print-replacement.md).

---

## What was read

### Subagent report (required)

`/tmp/ccide/REPORT.md` in full: §0–§9 and the source-location appendix (the appendix is unheaded text after §9; REPORT line 7 points at “附录 A”). Object: `anthropic.claude-code@2.1.273` running in Trae (VS Code fork). Chunks under `/tmp/ccide/chunks/` (1440 JS regions carved from the bundled CLI).

### Remuda docs (required)

- `README.md`
- `docs/architecture.md` (Claude drivers table; three mutually exclusive carriers)
- `docs/design/decisions.md`: D-002, D-011, D-012, D-017, D-026, D-027, D-028, D-028a, D-035 (next free number is D-036; no D-036 row exists yet)
- `docs/design/native-pty-first.md` §2.2, §4–§5, §6–§7, §12
- `docs/design/live-structured-view.md` (whole file, emphasis §0 and §2)
- `docs/design/testing-fake-harness.md` (fixture strategy; fake-harness is PTY, not stdio)
- `docs/design/review-claude-print.md` (stdin-across-turns vs D-035 one-turn)
- `NOTICE` (claude-wire is original; do not vendor Anthropic)
- `docs/design/plan-phase0.md` (sidecar currently tied to `claude-print` — superseded in the design)
- `docs/design/protocol.md` (session resume, stream-json argv notes)

### Remuda sources (required plus symbols used in the design)

| Path | Symbols |
| --- | --- |
| `crates/remuda-driver/src/claude_print.rs` | `ClaudePrintDriver`, `launch`, `send`, `respond_interaction`, `handle_can_use_tool`, `map_init`, `map_result`, `map_system`, `question_request`, `permission_from_answer`, `prompt_content`, `reject_bot_bypass` |
| `crates/remuda-driver/src/claude_print_stream.rs` | `StreamState` |
| `crates/remuda-driver/src/shell_pty.rs` | `ShellPtyDriver::send`, `spawn`, `cancel` |
| `crates/remuda-driver/src/materializer.rs` | `materialize`, `claude_argv`, `claude_settings_json`, `permission_plan`, `SessionAction` |
| `crates/remuda-driver/src/capabilities.rs` | `capability_matrix`, `adapter_transport` |
| `crates/remuda-driver/src/driver.rs` | `Driver` trait |
| `crates/remuda-driver/src/interaction.rs` | `InteractionBroker` |
| `crates/remuda-driver/src/child_env.rs` | `base_env`, denylist |
| `crates/remuda-driver/src/fake.rs` | `FakeDriver` |
| `crates/remuda-driver/src/lib.rs` | crate surface |
| `crates/remuda-driver/src/permission.rs` | live permission vocabulary (PTY; not sdk) |
| `crates/remuda-driver/src/usage/claude.rs` | transcript usage extractor |
| `crates/remuda-driver/src/launch/overlay.rs` | PTY settings overlay |
| `crates/remuda-claude-wire/src/lib.rs` | crate role |
| `crates/remuda-claude-wire/src/process.rs` | `SpawnSpec::argv`, `ClaudeProcess::{spawn_command,send_user,interrupt,close_stdin}` |
| `crates/remuda-claude-wire/src/types.rs` | `CanUseToolRequest` |
| `crates/remuda-claude-wire/README.md` | print template; keep reading after first `result` |
| `crates/remuda-signal/src/lib.rs` | `SignalAdapter` crate; `ASK_USER_QUESTION` |
| `crates/remuda-protocol/src/enums.rs` | `DriverKind`, `ObservationKind`, `AdapterTransport::ClaudeSdkSidecar`, `InteractionCarrier`, `SignalTier`, `SourceChannel`, `Completeness` |
| `crates/remuda-protocol/src/observation.rs` | `ObservationPayload` |
| `crates/remuda-node/src/runtime.rs` | `validate_kind_driver` |
| `crates/remuda-node/src/native.rs` | `NativeClaudeFactory`, print/pty/bg/shell-pty registration |
| `crates/remuda-node/src/interactions.rs` | `InteractionRuntime` |
| `crates/remuda-testing/src/fake.rs` | `run_fake_claude` (EOF-terminated) |
| `crates/remuda-testing/src/client.rs` | `spawn_fake_claude` still injects `-p` |

### Public SDK docs

Fetched `https://code.claude.com/docs/en/agent-sdk/typescript` (after redirects). `query({prompt, options})`, `Options` table (`canUseTool`, `includePartialMessages`, `resume`, `permissionMode`, `hooks`, `forwardSubagentText`, `allowDangerouslySkipPermissions`, …). **No `onUserDialog` / `supportedDialogKinds` on that table.**

---

## What was run (read-only)

1. `git status` / `git log` / listing `docs/design` and `crates/remuda-driver/src` — worktree clean on `wt/c-printsdk/print-replacement-research`.
2. Python walk of installed `anthropic.claude-code-*` extension directories: versions only, no paths committed. Found 2.1.270, 2.1.271, 2.1.273; REPORT is correct that 2.1.273 is the live gallery version. File sizes for 2.1.273: `extension.js` 3 506 717 bytes, `webview/index.js` 5 226 984 bytes, `resources/native-binary/claude` 212 228 880 bytes. `package.json` activationEvents `onStartupFinished`, `onWebviewPanel:claudeVSCodePanel`. Accept/reject commands: `claude-vscode.acceptProposedDiff` / `rejectProposedDiff` and `claude-code.acceptProposedDiff` / `rejectProposedDiff` (REPORT §7.2 pair confirmed).
3. String extraction from 2.1.273 `extension.js` (minified). Offsets below are character offsets in the decoded file.
4. Bundled CLI `--help` only, with `CLAUDE_CODE_AUTO_CONNECT_IDE=false` and `CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL=1`. Exit 0. No prompt, no session, no network call intended. Help includes `-p, --print` “Print response and exit” and the stale “only works with --print” notes on `--input-format` / `--output-format` / `--include-partial-messages`.
5. Read-only search of `/tmp/ccide/chunks/` for `X$r`, `Da`, `CLAUDE_CODE_ENTRYPOINT`. No session.

Not run: `claude` with a prompt, any Hub/Node, `scripts/ci/gate.sh`, cargo, extension `--install-extension`, anything under `deploy/`.

---

## Versions observed

| Component | Version |
| --- | --- |
| Extension (live) | `anthropic.claude-code` 2.1.273, publisher Anthropic, display name “Claude Code for VS Code” |
| Bundled CLI | 212 228 880-byte native binary (help run succeeded; version string not separately printed beyond the extension pin) |
| SDK pin inside extension | `CLAUDE_AGENT_SDK_VERSION="0.3.273"` |
| Older extension dirs still on disk | 2.1.270, 2.1.271 (not used) |
| Remuda worktree tip at research start | `6f7d987a` (matches `origin/main` at that moment) |
| Print mapper comments | Claude Code 2.1.268–2.1.273 in-tree notes (`permission.rs` header: 2.1.273 six modes) |

---

## First-hand snippets (redacted)

No home paths, hostnames, usernames, auth tokens, or lock-file JSON.

**SDK argv builder** (`extension.js` ~offset 2303461):

```text
let d=["--output-format","stream-json","--verbose","--input-format","stream-json"];
```

Subsequent `d.push` includes `--permission-prompt-tool stdio` when `canUseTool` is set, `--resume=`, `--include-partial-messages`, `--include-hook-events`, `--permission-mode`, `--allow-dangerously-skip-permissions`. No `--print` / `-p` in this function. Spawn log: `Spawning Claude Code: ${c} ${P.join(" ")}`.

**SDK version and options destructure** (~2862037):

```text
process.env.CLAUDE_AGENT_SDK_VERSION="0.3.273";
let { abortController, additionalDirectories, agent, agents, allowedTools, betas,
      canUseTool, continue, cwd, ... includePartialMessages, forwardSubagentText,
      onElicitation, onUserDialog, ...}
```

**Child env `L7()`** (~3353338):

```text
J.MCP_CONNECTION_NONBLOCKING="true"
J.CLAUDE_CODE_ENABLE_TASKS="0"
J.CLAUDE_CODE_ENTRYPOINT="claude-vscode"
delete J.CLAUDECODE
delete J.CLAUDE_CODE_CHILD_SESSION
delete J.TRACEPARENT
delete J.TRACESTATE
```

**SDK default entrypoint if unset:**

```text
if(!K.CLAUDE_CODE_ENTRYPOINT) K.CLAUDE_CODE_ENTRYPOINT="sdk-ts"
```

**`spawnClaude` options object** (REPORT §5.1 confirmed ~3322984): `cwd`, `resume`, `canUseTool`, `onUserDialog`, `supportedDialogKinds`, `permissionMode`, `resolvePermissionModeInCli:!F0("claudeProcessWrapper")`, `allowDangerouslySkipPermissions`, `systemPrompt:{type:"preset",preset:"claude_code",…}`, `enableFileCheckpointing:!0`, `includePartialMessages:!E$.env.remoteName`, PreToolUse/PostToolUse hooks, `settingSources:["user","project","local"]`. Follow-on log: `Spawning Claude with SDK query function`.

**Permission response filter** (~3066641): `sendRequest({type:"tool_permission_request", toolName, inputs, suggestions, …})` then drop `updatedPermissions` not in suggestions; drop bypass `setMode` unless skip-permissions is on.

**Launch bypass downgrade** (~3054789): unrecognized mode or `bypassPermissions` while allow-skip is off → `Downgrading launch to default mode` and `io_message` `system/status` `permissionMode:"default"`.

**Lock writer `W5$`:** writes `<port>.lock` mode 384 (`0600`) with `pid`, `workspaceFolders`, `ideName`, `transport:"ws"`, `runningInWindows`, `authToken`. **Contents not copied here.**

**CLI non-interactive detector** (`/tmp/ccide/chunks/chunk_00321_169560821.js` @3052):

```js
function X$r(t=process.argv.slice(2)){
  let e=vP("-p",t)||vP("--print",t),
      n=vP("--init-only",t),
      o=egn((s)=>s.startsWith("--sdk-url"),t)!==-1;
  return e||n||o||!process.stdout.isTTY
}
```

**SDK entrypoint set** (`chunk_00583_182979420.js` @109768):

```js
var Da=new Set(["sdk-ts","sdk-py","sdk-cli","local-agent","claude-desktop","claude-desktop-3p"]);
```

`claude-vscode` is **not** in `Da`. A nearby `im()` switch maps `claude-vscode` → `claude_code_vscode` and `sdk-ts` → `claude_code_sdk` for telemetry (`chunk_00278_167823501.js` @1163).

**CLI help (bundled binary, `--help` only):** `-p, --print` “Print response and exit”. `--input-format` description still says “only works with --print”. That description is inconsistent with `X$r` (non-TTY is enough) and with the SDK argv (no `-p`).

---

## Unconfirmed / open questions

Each item names the next file and offset to read. Do not treat these as decided.

1. **`zy0` / `Ay$` whitelist implementation.** Call site in `extension.js` ~3066641; bodies unread. REPORT §9 item 4. Next: search `extension.js` for `function zy0` / `function Ay$` (minified names may differ; REPORT used those).
2. **`onUserDialog` public contract.** Present on the bundled SDK class and `spawnClaude` options; absent from the public TypeScript `Options` table. Next: bundled SDK `sdk.mjs` (compressed; REPORT appendix) or a newer public docs revision.
3. **Does omitting `-p` plus piped stdout equal SDK multi-turn on a Remuda spawn without `CLAUDE_CODE_ENTRYPOINT=sdk-ts`?** `X$r` says non-TTY is enough for non-interactive; `-p` is separately “print and exit” (`E.print` / `No` in `chunk_00577_182436722.js` ~line 188). Next: that chunk around `let No=E.print` and the code that exits after a `result` when `No` is true — offset ~ start of the `No` uses in `chunk_00577_182436722.js`. A live two-turn spawn (billed; disclose in evidence) is the only behavioral proof.
4. **`--await-initialize`.** Hidden flag, “only works with `--input-format=stream-json`” (`chunk_00583_182979420.js` @137244). Whether Remuda’s existing initialize-first write needs this flag: unread. Next: same chunk, callers of `--await-initialize`.
5. **`CLAUDE_CODE_ENABLE_TASKS=0`.** Set by the extension; reason unread. Next: CLI reads of that env (grep chunks for `CLAUDE_CODE_ENABLE_TASKS`).
6. **Custom `CLAUDE_CODE_ENTRYPOINT`.** `im()` / `Da` list known values. Whether an unknown value breaks stream-json: unread. Next: `chunk_00278_167823501.js` @1163 and all `Da.has` uses (`chunk_00583_182979420.js` @111205).
7. **Gateway without `-p`.** `claude_settings_json` is proven on print; not on sdk argv. Next: billed probe, not a chunk.
8. **Steer/queue on stream-json without `-p`.** No fixture. Next: fake two-turn is insufficient; need a recorded `queue-operation` or a disclosed live session.
9. **REPORT §9 items 1–3, 5–10** (liveness helpers, `Len` sort, `eGe`, `Tot`, `_8`, dual diff triggers, webview union, IPv6 WSL) — still open; not required to start batches 1–2.
10. **`sdkUrl` forcing print** (`if(no){… if(!E.print) No=!0}` in `chunk_00577_182436722.js` ~189). Extension spawn did not show `--sdk-url` in the argv builder window. Next: whether `query()` ever adds it.

---

## What this worker did not do

- Did not install or update the extension.
- Did not start a Claude session that would bill.
- Did not copy lock files, tokens, workspace paths, or hostnames into git.
- Did not modify `decisions.md`, `crates/`, `web/`, or `deploy/`.
- Did not kill processes it did not start.
