# D-028 §9.2 follow-up: renderer launch choice and observed mode

This follow-up makes the native PTY renderer a launch preference and reports its actual alternate-screen state separately. It supersedes §9.2's former instruction to retain `--setting-sources` and refuse `/tui`. The owner explicitly selected in-session switching and the updated helper text.

## Baseline and scope

The starting main was `6f7dfe8`, the merge of `wt/c-hookgap/promoted-claude-hook-events`. The first twenty commits included that merge and its hook-binding fixes. The explicit-settings shim therefore already merged a caller's relay overlay; new executed-shim tests assert that both renderer values and both terminal signal keys win that merge.

The ux-b merge was absent at task start, so the initial `NewSessionPage.tsx` change was limited to additive renderer state, a request field, and a control inside its existing advanced block. The final branch was rebased onto main `9df9099`, which includes ux-b. The resolved page preserves batch B's Sheet layout, draft restore/persistence, guarded create flow, and distinct confirmed/uncertain create outcomes. Its existing create payload carries an explicit renderer choice; an untouched choice stays omitted so the host default still applies. Host settings persist `defaultTui`; creation resolves session value, then host value, then `fullscreen`. Resume preserves the requested renderer.

The rebase also exposed an inherited macOS filename collision between `QuickFind.tsx` and the `quickFind.ts` helper. Renaming the helper to `quickFindSearch.ts` and updating its two imports removed that collision before the final web gates.

The renderer pin/release mechanism applies to native shell/agent PTY with hook overlays enabled (`REMUDA_PTY_HOOKS=1`). Actual mode requires the terminal emulator (`REMUDA_PTY_EMULATOR=1`); raw-ring fallback is unknown. Legacy print/herdr launch paths do not have this authenticated binding/release mechanism. The web indicator never converts the request into an actual-mode claim.

## Coordinator research and native control probe

The coordinator's research was independently checked against the installed Claude Code **2.1.270** macOS arm64 bundle. The minified `cPn` predicate restricts renderer switching if any of `systemPrompt`, `systemPromptFile`, `appendSystemPromptFile`, `permissionPromptTool`, `settingSources`, or `managedSettings` is defined, or a tools entry differs from `default`. `settings` is absent from that predicate. Normal `user,project,local` loading excludes nothing, so injecting its explicit flag only introduces the restriction.

There is no renderer launch flag or environment variable that forces fullscreen. `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` forces classic mode; `CLAUDE_CODE_NO_FLICKER=1` overrides selected automatic disables. The probe initially set the latter, so its successful fullscreen entry does not prove every host environment will choose fullscreen. The native resolution gives merged `settings.tui` priority over `CLAUDE_CODE_TUI_JUST_SWITCHED`. `CLAUDE_CONFIG_DIR` relocates the entire configuration directory and is used here only for the isolated probe, not as the production renderer control.

The probe merged the authorized relay settings into private worktree scratch files, copied no native credential files, submitted no model prompt, and captured no screenshots. Two synthetic SessionStart hooks wrote only their origin labels. Full sanitized facts, raw escape-sequence counts, and cleanup results are in [native-pty-9b-real.json](./native-pty-9b-real.json).

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

The final web gates were run after the rebase onto main `9df9099` and the batch B integration:

| Gate | Result |
|---|---|
| `pnpm --dir web lint` | **VERIFIED**: passed with seven existing React warnings |
| `pnpm --dir web exec tsc -b` | **VERIFIED**: passed |
| `pnpm --dir web test` | **VERIFIED**: 83 test files and 656 tests passed |
| `cargo fmt --all -- --check` | **VERIFIED**: passed |
| `cargo clippy --locked -p remuda-driver -p remuda-node -p remuda-hub -p remuda-protocol --all-targets -- -D warnings` | **VERIFIED**: passed |
| `cargo test --locked -p remuda-driver -p remuda-node -p remuda-hub -p remuda-protocol` | **VERIFIED**: 900 passed, zero failed, 11 existing ignored cases |
| `cargo test --locked -p remuda-testing --test fake_harness` | **VERIFIED**: full rerun passed all 21 cases, including two renderer relaunches |
| `pnpm --dir web test:e2e:hub` | **VERIFIED**: final complete run passed 33 cases; 14 configuration-specific cases skipped; zero failures |
| `pnpm --dir web gen:api` | **VERIFIED**: regenerated output matched the committed API files |
| `./scripts/ci/secret-scan.sh` and `git diff --check` | **VERIFIED**: passed after fixture cleanup |

Hub e2e uses the isolated fake Node and fake harness on `127.0.0.1:58580` with web port `58589`. The coordinator demo was not used. Screenshots, if retained by the test runner, contain synthetic fixture output; this evidence package contains no real-terminal screenshot.

The first full Hub run passed 32 cases and failed the native fixture before launch because the worktree's absolute socket path exceeded macOS's limit. The fixture now uses macOS's existing volfs alias for its own worktree data directory, checks its device/inode identity, and creates no external directory or symlink. The focused native rerun and then the complete suite passed, including requested `default` in the created spec, both indicator states, two changed PIDs with stable binding, continued hook events, and a completed turn after switching. The coordinator's generic `.hub.spec.ts` matcher was absent on final main, so its requested fallback pattern was added; this task extends two existing specs.

The Rust suite used a short worktree-local temporary directory and two test threads. An earlier cold run exceeded macOS's Unix socket path limit with a longer temporary directory; a process-group exit assertion also passed on focused rerun. The first full fake-harness run timed out in the existing body-plus-CR batching test; both its focused rerun and the full 21-case rerun passed. A delayed reader combining the two writes is the source-supported explanation, not a captured runtime fact. No input parser or production process-lifecycle behavior was changed to bypass these failures.

## Coordinator merge follow-up: grouped settings

Merged main `3542b24` after the coordinator reported conflicts. The incoming appearance, notifications and connection groups, anchors, save feedback and rollback behavior remain intact. That main did not yet contain a host-defaults group, so an anchored group was added with one renderer field per host, using the existing `useGroupDraft` and `SaveBar`. The obsolete standalone immediate-save control was removed. A rejected PATCH restores the latest confirmed value; a failed follow-up refresh cannot relabel a confirmed save or erase another group's draft.

The Playwright configuration keeps the union of existing patterns plus `.hub.spec.ts`. The native test is now `promoted-claude.hub.spec.ts`; `settings-tui.hub.spec.ts` verifies explicit saving, server persistence and rejection rollback against the fake Hub, then restores the prior nullable default. The settings layout tests also cover the new anchor at each viewport and theme.

| Post-merge gate | Result |
|---|---|
| `pnpm --dir web typecheck`, `lint`, `build` | **VERIFIED**: all passed; `typecheck` names the existing `tsc -b` command |
| `pnpm --dir web test --maxWorkers=4` | **VERIFIED**: 86 files, 702 tests passed |
| Settings Chromium layout/interaction spec | **VERIFIED**: 14 passed, one opt-in evidence capture skipped |
| Renderer and host-defaults Hub specs | **VERIFIED**: both passed |
| Full `pnpm --dir web test:e2e:hub` | **VERIFIED**: 37 passed, 14 configuration-specific skips, zero failures |
| `cargo test --locked -p remuda-driver -p remuda-node -p remuda-hub` | **VERIFIED**: 874 passed, 11 existing ignored cases, zero failures |
| `gen:api`, secret scan and whitespace check | **VERIFIED**: generated API unchanged; scans passed after fixture cleanup |

Hub runs again used only the fake Node/fake harness on ports 58580/58589, with fresh native fixture executables. No additional real-Claude probe was needed for this settings integration.


## Coordinator merge follow-up: projects, resume overlay and live harness

Merged main `e2c919e` into the renderer branch. The HTTP create body retains both the renderer option and the new project/delegation fields. Resume retains the renderer while forwarding the current provider overlay and host-scoped secret; both native-provider absence assertions and the gateway token-rotation regression remain in the merged tests. The fake harness initializes the modern dialect before resolving the requested renderer, preserving modern OSC updates, layered settings/hooks and replacement-process `/tui` behavior.

The Hub OpenAPI document is a hand-maintained source in this repository (`tests/openapi.rs` and `web/scripts/gen-api.mjs`), not a Rust-generated artifact. It was rebuilt with a structural three-way JSON merge, preserving every incoming main value and adding only the five renderer schema entries. `pnpm --dir web gen:api` regenerated the API client, and `cargo run --locked -p remuda-protocol --example gen_types` regenerated both protocol artifacts from the merged Rust types.

The incoming Files view exposed another macOS import collision: `FilesView.tsx` and `filesView.ts`. Renaming only the helper to `filesViewModel.ts` and updating its two imports restores unambiguous module resolution. The component and all helper behavior are unchanged. The Playwright Hub configuration remains exactly as on incoming main; both owned specs use the `.hub.spec.ts` convention.


The first combined Rust run and a focused rerun both timed out in `claude_artifacts_parse_with_the_transcript_mapper`, before any approval event; only `session_start` was captured. That test immediately sent input using fixed delays while the child was starting. Waiting for its existing SessionStart event before the first body/Enter pair made the focused regression pass. This is a fixture startup synchronization fix. The hypothesis that the early bytes were combined as one paste follows the reader/parser implementation; no raw input-read trace was captured, so that exact batching is not claimed as an observed fact.


The first Hub run passed the host-defaults spec but stopped the promoted fixture at instance creation. A diagnostic rerun captured `PLACEMENT_UNSATISFIABLE`: the new placement guard rejected the disposable native Node's initial CPU report of 100% against its 90% ceiling. The inventory implementation samples load average at startup and reuses that report in heartbeats. This machine-load result says nothing about renderer switching, because no instance was launched.

The promoted spec now uses a dedicated `remuda-node` example, `native_hub_e2e`, with the public native runtime and WSS APIs. Only its synthetic resource report is fixed for deterministic placement; the production guard is unchanged. The example keeps its workspace, home, token and data inside the disposable fixture, restricts inventory to the fake executable directory, and explicitly supplies the real Remuda hook-relay helper. The native runtime still performs PTY dispatch, hook binding and renderer observation against fake-harness. Its resource values are fixture inputs, not a measurement of this Mac.


| Final merge gate | Result |
|---|---|
| `cargo test --locked -p remuda-hub -p remuda-testing` | **VERIFIED**: final complete rerun passed 266 tests across 33 suites; zero failures or ignored cases; all 24 fake-harness integration cases passed |
| `pnpm --dir web typecheck`, `lint`, `build` | **VERIFIED**: passed; lint reports existing warnings |
| `pnpm --dir web test --maxWorkers=4` | **VERIFIED**: 90 files, 766 tests passed |
| `cargo clippy --locked -p remuda-node --example native_hub_e2e -- -D warnings` and `cargo fmt --all -- --check` | **VERIFIED**: passed |
| Owned `promoted-claude.hub.spec.ts` and `settings-tui.hub.spec.ts` | **VERIFIED**: both passed in the final combined run; 43.2 seconds, zero skips or retries |
| Protocol generator `--check` and `pnpm --dir web gen:api` | **VERIFIED**: protocol schema/types match the merged source; regenerated API client is diff-clean |

The final Hub run used only the isolated Hub/native fixture/fake-harness at `127.0.0.1:59080`, web port `59089` and synthetic upstream port `59081`, with `HUB_E2E_EXTERNAL=0` and `REMUDA_EVIDENCE=0`. It verified both observed renderer states, replacement PIDs with stable session binding, post-switch continuation, explicit host-default saving, persistence and rejected-save rollback. The tests left no tracked-file changes or new committed-evidence screenshots. The owned native fixture directories and short worktree scratch directory were removed before delivery.

`./scripts/ci/secret-scan.sh` and the whitespace check against incoming main passed after cleanup.
