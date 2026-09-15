# native-carrier-3 — shell-pty hooks, trust/onboarding, transcript binding, `Es` reap

- **When:** 2026-09-15/16
- **Where:** task worktree `/tmp/remuda-agents/wt/r-native2` (branch `wt/r-native2/native-carrier-journal-and-reap`)
- **Agent:** r-native2
- **Host:** bolt-devbox-sg (Linux), claude **2.1.221** installed (coordinator's macOS demo pinned 2.1.272; the defects and fixes are harness-version independent)
- **Predecessors:** `native-pty-2c.md` (fast durable accept + zombie-aware stop ladder), D-028 §4.2/§5.1/§5.3/§5.5
- **Flags under test:** `REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`

---

## 1. The macOS retest, reproduced on Linux

The coordinator's retest created a `driver: shell-pty`, `permissionMode: bypass`
instance with the PONG probe and saw, on macOS:

1. create returned immediately and the instance sat `running / idle` for 126 s
   with **zero journal items** — no lifecycle, no hook, no transcript, no
   message; the Node log showed the emulator starting but no hook overlay;
2. `DELETE ?force=1` logged
   `stop-incomplete: the process group outlived SIGKILL rung="sigkill"
   pgid=16146 survivors=[16146]` and pid 16146 (the claude process, `?Es`)
   stayed a child of the Node for >3 minutes.

Both were reproduced against the real 2.1.221 binary on Linux through
`ShellPtyDriver` (ignored live test
`tests/shell_pty_native_live.rs::real_claude_through_native_shell_pty`,
scenarios `baseline`/`fixed`), reading the Node's own emulator screen.

### 1.1 Empty journal: the folder-trust dialog (then a second modal)

With a fresh cwd outside the auto-trusted prefix the emulator grid was the
exact dialog:

```
Quick safety check: Is this a project you created or one you trust?
❯ 1. Yes, I trust this folder
  2. No, exit
Enter to confirm · Esc to cancel
```

The agent parks **before `SessionStart`**, so no hook fires and the journal is
empty. On a trusted `/tmp/.tmp*` cwd the first run got further and revealed
the second parking screen for bypass launches:

```
WARNING: Claude Code running in Bypass Permissions mode
❯ 1. No, exit
  2. Yes, I accept
```

Three configuration facts were established empirically (they explain why the
carrier behaved differently from `claude-pty`):

| Fact | Evidence |
| --- | --- |
| Trust decisions are per-cwd in `projects.<cwd>.hasTrustDialogAccepted`, and Claude walks **ancestor** project entries; an ancestor explicitly set to `false` blocks a descendant | copied host config containing `/home/<user> → false` parked a fresh `~/…/workspace` run; logarithmic bisection over the host config isolated it |
| On a migrated 2.1.x config (`migrationVersion: 14`) the global `bypassPermissionsModeAccepted` is **ignored** at the TUI disclaimer; the honoured key is `skipDangerousModePermissionPrompt` in **settings.json** — the same key Claude writes itself when a human accepts | direct TUI probes: global key alone → disclaimer; settings key → composer; interactive acceptance writes `settings.json {"skipDangerousModePermissionPrompt": true}` |
| The shell-pty agent recipe never exported `CLAUDE_CONFIG_DIR`, and the Node pointed `options.claude_home` at the node-wide override rather than the per-instance native home | a trusted-cwd run fired hooks but journaled `transcript_unbound`: the hook reported a transcript under `$HOME/.claude/projects/…` while the poller scanned the registered home |

## 2. Fixes

### 2.1 Launch path honours the hook opt-in and records standing up

- `prepare_launch_blocking` now reads the operator overlay for a Claude agent
  whenever it exists (hooks on or off), and when a hook session is built it
  logs at INFO exactly what materialised:
  `hooks overlay materialised path=… socket=… bin=…`. The macOS log lacked this
  line — it is now the unambiguous proof the socket/shim/overlay stood up.
- A bypass launch merges `skipDangerousModePermissionPrompt: true` into the
  merged settings overlay (the hook overlay when hooks are live, otherwise a
  private `<launch_dir>/settings.json` written through the existing audited
  path). The operator's own settings file is never modified; this mirrors the
  key the herdr carrier's session overlay already carries.
- `ShellPtyOptions` gains `pin_native_home`; the recipe env for a Claude agent
  now exports `CLAUDE_CONFIG_DIR=<native home>` when the Node prepared a
  scoped home, and the promotion poller's `claude_home` is set to that same
  directory (or left at the default when the launch inherits the operator
  home). Transcript binding then looks where Claude actually writes.

### 2.2 Trust/onboarding: pre-seed, then the exact dialog once

Exactly the two-layer defence the herdr carrier already has, applied to the
native carrier:

1. **Scoped home (Node-owned `native_home`):** before spawn the factory runs
   `seed_scoped_config` (onboarding flags) and the new
   `pre_trust_workspace(native_home, cwd)` which writes precisely
   `projects.<cwd>.hasTrustDialogAccepted = true`, absolute paths required,
   idempotent, every other key/project preserved, and only into the isolated
   config dir — never `~/.claude.json`.
2. **Inherited operator home:** no config writes. The promotion poller
   answers the folder-trust dialog on screen, **once per promoted epoch**,
   using the existing D-022 narrow matcher (`remuda_screen::trust_dialog_keys`)
   and its exact key sequence; it journaled a
   `trust-dialog / trust-dialog auto-accepted (registered workspace)`
   diagnostic. Granted by the Node only when
   `auto_trust_registered_workspaces` and the same canonical-containment
   `cwd_is_registered` gate the herdr carrier uses.

### 2.3 `E`/`X` are dead, and the ladder reaps its own exiting child

- `lifecycle::state_is_dead` now classifies macOS `E` (exiting; `?Es` includes
  the no-controlling-tty `?` prefix and the `s` session-leader flag) and
  Linux `X`/`x` (dead) alongside `Z` (zombie) as dead-but-unreaped. Both the
  `/proc/<pid>/stat` and `ps -eo` parsers use it; the parser halves were
  split into `parse_proc_stat_line` / `parse_ps_table` so the macOS `?Es`
  row has a host-independent unit test.
- After the ladder reports the group gone, `stop_tree` keeps reaping the
  leader for a bounded 3 s (`REAP_EXITING_GRACE`): an `E` process is dead in
  the kernel but not yet a zombie, and `waitpid` succeeds the moment it
  becomes one. A process wedged in uninterruptible exit beyond that bound is
  left to init with a warning rather than stalling `close` forever.

## 3. Before / after (journal channel counts)

Real binary, PONG probe, native shell-pty carrier:

| Scenario | runtime | hook | pty | transcript | message | Outcome |
| --- | --- | --- | --- | --- | --- | --- |
| retest (macOS) | 0 | 0 | 0 | 0 | 0 | parked 126 s, `stop-incomplete`, `Es` lingers |
| Linux baseline, untrusted cwd | 3 | 0 | 1 | 0 | 0 | parked on the trust dialog |
| Linux **fixed** | 5 | **4** | **4** | **10** | hook + transcript `message` rows | trust-dialog auto-accepted → SessionStart → prompt → `MessageDisplay`/`Stop`, `transcript_bound`, PONG rendered, no disclaimer |

The fixed run's ordering proves the wiring end to end:
`agent_detected → agent_promoted → transcript_unbound → trust-dialog
→ agent_status(idle) → hook SessionStart → hook UserPromptSubmit
→ transcript permission-mode/message → transcript_bound
→ hook MessageDisplay → hook Stop → idle`.

Fake harness (`fake-harness --dialect-version modern`, scenario `live.json`)
through the production Node factory re-execed under the three carrier flags:
the hooks case asserts all four channels present (`assert_all_four_native_channels`)
plus the existing latency/burst/rule-6 budgets —
prompt/toolStart/firstChunk/Stop native→journal ≤ 7 ms, 8-event tail burst over
~15 ms. No-hooks case keeps zero hook events.

## 4. Tests added / changed

- `remuda-driver`
  - `shell_pty/lifecycle.rs`: `dead_state_letters_are_dead_in_every_os_spelling`,
    `a_synthetic_ps_table_classifies_an_exiting_macos_leader_as_dead`
    (exact `?Es` row from the demo), `…_keeps_a_live_member_visible`,
    `proc_stat_dead_letters_are_classified_from_the_post_comm_tail`.
  - `claude_onboarding.rs`: `pre_trust_workspace` idempotency, other-project
    isolation, relative-path refusal.
  - `shell_pty.rs`: `a_bypass_native_launch_pins_its_home_and_suppresses_the_disclaimer`
    (overlay contains `skipDangerousModePermissionPrompt` with and without
    hooks; `CLAUDE_CONFIG_DIR` pinned only when opted in).
  - `tests/shell_pty_native_live.rs`: `#[ignore]` real-binary repro harness
    (baseline vs fixed; emulator screen dump; channel histogram).
- `remuda-node`
  - `tests/live_pipeline.rs`: hooks case now requires runtime+hook+pty+transcript
    and a hook-channel streaming message.
- Hub e2e: `promoted-claude.hub.spec.ts` (the native hook fixture) passes with
  the new factory wiring; `session-virtual` / `settings-tui` /
  `resume-overlay` hub specs run under the task env.

## 5. Timings

- First live PONG (trust accept → composer → SessionStart → reply): the
  poller answers the dialog within one 800 ms promotion tick; the reply
  rendered "Cooked for 4 s" (model time).
- Fake-harness native→journal latencies: prompt −3 ms (pre-anchor), toolStart
  7 ms, firstChunk 7 ms, Stop 6 ms; tail burst 8 events / 15 ms (budget
  400 ms single, 1.5×+300 ms burst). All inside D-028's local budgets.
- Stop: SIGKILL→group-gone within the existing 500 ms `GRACE_KILL`; leader
  reaped inside the 3 s exiting grace in all observed runs.
