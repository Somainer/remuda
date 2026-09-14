# D-028 §9.2 follow-up: renderer launch choice and observed mode

This follow-up makes the native PTY renderer a launch preference and reports its actual alternate-screen state separately. It supersedes §9.2's former instruction to retain `--setting-sources` and refuse `/tui`. The owner explicitly selected in-session switching and the updated helper text.

## Baseline and scope

The starting main was `6f7dfe8`, the merge of `wt/c-hookgap/promoted-claude-hook-events`. The first twenty commits included that merge and its hook-binding fixes. The explicit-settings shim therefore already merged a caller's relay overlay; new executed-shim tests assert that both renderer values and both terminal signal keys win that merge.

The ux-b merge was absent at task start. `NewSessionPage.tsx` received only an additive renderer state, request field, and control inside its existing advanced block. Host settings persist `defaultTui`; creation resolves session value, then host value, then `fullscreen`. Resume preserves the requested renderer.

The renderer pin/release mechanism applies to native shell/agent PTY with hook overlays enabled (`REMUDA_PTY_HOOKS=1`). Actual mode requires the terminal emulator (`REMUDA_PTY_EMULATOR=1`); raw-ring fallback is unknown. Legacy print/herdr launch paths do not have this authenticated binding/release mechanism. The web indicator never converts the request into an actual-mode claim.

## Coordinator research and native control probe

The coordinator's research was independently checked against the installed Claude Code **2.1.270** macOS arm64 bundle. The minified `cPn` predicate restricts renderer switching if any of `systemPrompt`, `systemPromptFile`, `appendSystemPromptFile`, `permissionPromptTool`, `settingSources`, or `managedSettings` is defined, or a tools entry differs from `default`. `settings` is absent from that predicate. Normal `user,project,local` loading excludes nothing, so injecting its explicit flag only introduces the restriction.

There is no renderer launch flag or environment variable that forces fullscreen. `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` forces classic mode; `CLAUDE_CODE_NO_FLICKER=1` overrides selected automatic disables. The probe initially set the latter, so its successful fullscreen entry does not prove every host environment will choose fullscreen. The native resolution gives merged `settings.tui` priority over `CLAUDE_CODE_TUI_JUST_SWITCHED`. `CLAUDE_CONFIG_DIR` relocates the entire configuration directory and is used here only for the isolated probe, not as the production renderer control.

The probe merged the authorized relay settings into private worktree scratch files, used no copied credentials, submitted no model prompt, and captured no screenshots. Two synthetic SessionStart hooks wrote only their origin labels. Full sanitized facts, raw escape-sequence counts, and cleanup results are in [native-pty-9b-real.json](./native-pty-9b-real.json).

| Observation | Result |
|---|---|
| User/project/local each request `default`; overlay requests `fullscreen` | **VERIFIED**: raw `ESC[?1049h` observed, with and without explicit setting-sources |
| User and overlay SessionStart hooks | **VERIFIED**: both actually executed |
| Legacy explicit setting-sources + `/tui default` | **VERIFIED**: “Cannot switch renderers in this session”; no alt-screen exit |
| No setting-sources; remove only overlay `tui` after SessionStart; `/tui default` | **VERIFIED**: allowed; raw `ESC[?1049l` observed |
| User-only renderer preference after release; `/tui fullscreen`, then `/tui default` | **VERIFIED**: alternate-screen enter count 1 → 2, exit count 1 → 2; hooks continued |
| Project/local retain `tui=default`; `/tui fullscreen` writes user fullscreen | **VERIFIED LIMITATION**: command allowed but higher-priority settings still kept classic mode |
| Native replacement identity | **VERIFIED**: this binary retained its OS PID and emitted fresh SessionStart IDs before resume was available; new-PID behavior is separately exercised by the fake harness |
| Owned processes and private relay copies after probe | **VERIFIED**: no remaining owned process-group members; private config, transcript and relay copies removed |

The standalone native probe exercises Claude's behavior, not Remuda Node binding or model task completion. Managed settings precedence is represented in the isolated fixture contract and coordinator research; it was not exercised against a managed native installation.

## Implementation and causal tests

The driver pins `tui`, `showStatusInTerminalTab`, and `terminalProgressBarEnabled` at launch. The supervisor waits for an authenticated SessionStart whose PID matches the observed foreground process, then removes only `tui` from the base overlay and generated per-invocation overlays. Private files are rewritten atomically. A release marker and post-write check cover concurrent shim merges; caller files and symlinks are untouched. The launch audit digest continues to identify the original launch bytes.

Generated interactive argv no longer includes any fork-predicate trigger. Explicit caller source restrictions remain explicit caller intent. The legacy print-only `--permission-prompt-tool stdio` permission transport remains in its noninteractive path; it is not injected into native PTY argv.

The fake harness loads isolated user/project/local/overlay settings, emits native-shaped hooks and DEC alternate-screen bytes, writes the new user preference for `/tui`, and starts a replacement process with `CLAUDE_CODE_TUI_JUST_SWITCHED`. Its integration test requires changed PIDs, stable native session identity, both observed modes and a completed fixture turn after two switches. The production binding test also covers the native probe's same-PID/new-session case, including rejection of another PID or an out-of-workspace transcript.

Node sends actual mode edges without building an extra repaint for every output chunk. Hub authenticates host, instance and stream ownership, retires replaced streams, and forwards mode changes only to terminal followers. A fresh attach resets unknown state and may introduce a new stream; stale live updates cannot replace that stream. UI tests cover unknown/default/fullscreen, mismatch notes, host defaults, launch selection and the updated helper.

## Acceptance

Validation results are recorded after the final gates. Hub e2e uses the isolated fake Node and fake harness on `127.0.0.1:58580` with web port `58589`. The coordinator demo was not used. Screenshots, if retained by the test runner, contain synthetic fixture output; this evidence package contains no real-terminal screenshot.
