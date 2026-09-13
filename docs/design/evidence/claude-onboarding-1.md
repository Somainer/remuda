# Claude first-run onboarding, and Herdr pane reclamation on stop

Two defects observed on the demo Node at main `d71df7f` with **Claude Code
2.1.270** and **Herdr 0.9.0**, on macOS 25.4.0.

- **A.** A new `claude-pty` instance stayed with its prompt `queued` forever.
  Herdr reported the agent *idle*, there was no `SessionStart` and no session
  id, and the pane showed Claude Code's first-run onboarding ("Choose the text
  style that looks best with your terminal").
- **B.** The demo Node's Herdr session accumulated twelve idle `rmd-grok-*`
  panes from instances stopped through `POST /v1/instances/{id}/stop`. The
  stop settled, but the pane was never closed.

All probe work below ran under the worktree's ignored `.tmp/`, in a throwaway
`CLAUDE_CONFIG_DIR` with no credentials. Absolute paths are replaced with
`<worktree>`.

## A. Why a scoped config dir runs onboarding

The Node launches `claude` with a Node-scoped `CLAUDE_CONFIG_DIR` (D-022).
That directory is empty on first use, and the installed 2.1.270 binary gates
its whole first-run flow on one flag in the *global config* of that directory:

| Fact | Evidence in the 2.1.270 binary |
| --- | --- |
| `<CLAUDE_CONFIG_DIR>/.claude.json` is the global config | `globalConfig: join(CLAUDE_CONFIG_DIR ?? homedir(), ".claude.json")` |
| `<CLAUDE_CONFIG_DIR>/settings.json` is the user settings | `userSettings: join(CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude"), "settings.json")` |
| `hasCompletedOnboarding` gates the flow | `if (config.hasCompletedOnboarding && …) return null;` guards the `Onboarding` import |
| finishing it writes the same flag | the `onDone` reducer sets `hasCompletedOnboarding: true` and `lastOnboardingVersion` |
| the steps, in source order | `preflight?`, **`theme`**, `api-key?`, `oauth?`, **`security`**, **`terminal-setup`?** |
| `theme` is user settings, not global config | `saveTheme → set("userSettings", {theme})` |
| the bypass disclaimer is a separate gate | `if (skipDangerousModePermissionPrompt \|\| config.bypassPermissionsModeAccepted) return;` |

Claude Code's own `plugin eval` sandbox does exactly this for its throwaway
config dir: it writes `.claude.json` with
`{hasCompletedOnboarding: true, autoUpdates: false, bypassPermissionsModeAccepted: false}`
before launching. The fix follows that precedent.

### Live run: fresh vs seeded config dir

The real binary was launched on a real PTY (`pty.fork`, `TERM=xterm-256color`)
with `env -i` and only `PATH`, `HOME`, `LANG`, `TERM` and `CLAUDE_CONFIG_DIR`
set, once per directory. The TUI paints cells, so the captures below have lost
their inter-word spaces; that is a property of the capture, not of the screen.

`claude --version` against the fresh directory created **no files at all**, so
nothing but an interactive launch reaches this code path.

**Before — fresh scoped directory (the defect):**

```text
Welcome to Claude Code v2.1.270
Let's get started.
Choose the text style that looks best with your terminal
To change this later, run /theme
  1. Auto (match terminal)
❯ 2. Dark mode ✔
  3. Light mode
  4. Dark mode (colorblind-friendly)
  5. Light mode (colorblind-friendly)
  6. Dark mode (ANSI colors only)
  7. Light mode (ANSI colors only)
```

**After — the same directory seeded with `{"hasCompletedOnboarding": true}`:**

```text
Accessing workspace: <worktree>/.tmp/onb-live/cwd
Quick safety check: Is this a project you created or one you trust? (Like your
own code, a well-known open source project, or work from your team). If not,
take a moment to review what's in this folder first.
Claude Code'll be able to read, edit, and execute files here.
Security guide
❯ No, exit
  Yes, I trust this folder
Enter to confirm · Esc to cancel
```

Normalised marker check over the two captures:

| Marker | fresh | seeded |
| --- | --- | --- |
| `Choose the text style that looks best with your terminal` | **yes** | no |
| `Let's get started.` | **yes** | no |
| `Is this a project you created or one you trust?` | no | **yes** |

Seeding removes the wizard entirely and the launch proceeds directly to the
folder-trust dialog — which D-022 already answers automatically for a
registered workspace. The fresh directory never even reaches that dialog,
which is why D-022's handling alone could not rescue the instance.

## A. What changed

1. **Seed before launch.** `claude_onboarding::seed_scoped_config` writes
   `hasCompletedOnboarding: true` into the scoped `.claude.json`, and mirrors
   an allowlist of the host user's flags: `lastOnboardingVersion` and
   `bypassPermissionsModeAccepted` (global config), `theme` and
   `skipDangerousModePermissionPrompt` (user settings). The allowlist is
   explicit, so `oauthAccount`, `userID`, `projects` and `.credentials.json`
   are never copied; a unit test asserts each of those is absent. An existing
   key in the scoped directory always wins, so a theme changed inside the pane
   survives. Files are written `0600` inside a `0700` directory. Seeding is
   skipped when the launch inherits the user's own default config, which is
   already onboarded, and can be turned off with
   `REMUDA_CLAUDE_SEED_ONBOARDING=0` to reproduce the wizard deliberately.

2. **Detect the screens anyway.** `startup_dialog` recognises the theme
   picker, the security notes, the terminal-setup question, the login picker
   and the bypass disclaimer. Each arm requires at least two independent
   markers, because a single stock phrase can appear in ordinary model output.
   `claude-pty`'s interaction observer answers the safe screens once per
   carrier and journals a `claude-onboarding` diagnostic for every screen it
   sees. **Terminal setup is answered with `escape`, not `enter`**: enter
   there rewrites the operator's terminal profile (key bindings, audible
   bell), which is not Remuda's to change. Login and the bypass disclaimer
   carry a real decision, so they are reported and left for a human.

3. **Do not treat a wizard as idle.** Herdr reports a wizard step as `idle`
   and `interactive_ready` — it is a TUI at a menu — so `prompt_ready` alone
   would have let the D-022 queue type the user's prompt into the theme
   picker. `prompt_ready_for` keeps the queue blocked while a recognised
   screen is on the viewport. The watch is disarmed once Claude's own
   `SessionStart` hook proves the session is real, so no later model output
   can be mistaken for a wizard.

### A. Tests

`fake-herdr` gained an `onboarding` script that reports the agent **idle and
interactive-ready** while walking the three wizard steps, then a real
composer — the defect's exact shape. Screen fixtures for all five screens (and
an ordinary composer) live in `crates/remuda-testing/tests/fixtures/`, with
provenance in that directory's `SOURCES.md`.

- `claude_onboarding_is_detected_answered_and_blocks_prompt_dispatch` asserts
  the pane really is idle and interactive-ready, that `wait_control` refuses
  anyway, that all three steps are journalled and auto-answered in order, that
  the terminal-setup receipt is `KEYS escape`, and that delivery unblocks only
  after `SessionStart`.
- `startup_screens_are_recognised_with_the_right_default` and
  `an_ordinary_screen_is_never_a_startup_dialog` pin the detectors against the
  fixtures, a composer, the D-022 trust dialog, a mid-session `/theme` picker
  (no intro line, so a human's own theme change is never answered from under
  them), and model output quoting a marker.
- `seeding_completes_onboarding_and_copies_only_allowlisted_host_keys` covers
  the allowlist, the credential exclusions, idempotency and precedence.

## B. Why a stopped instance kept its pane

A driver reclaims only the Herdr resources it holds **in memory**
(`PtyResources`). The durable `pty_resources` row is the only record that
outlives a rebuilt driver, an adopted pane, or a close that returned an error
— and it was consulted at Node startup (`reconcile_herdr`) and at shutdown,
but **never on a stop**. So a stop whose driver had no in-memory record closed
nothing, and nothing looked at that row again until the Node restarted. Twelve
stops, twelve surviving panes.

A second, smaller gap: `PtyResource::close` relied on `tab.close` to reclaim
the panes. Real Herdr does honour that (probed below), but `pane.split` can
place the agent outside the recorded tab, and the close verified only that the
*workspace* was gone — never that no owned pane survived.

A real Herdr 0.9.0 probe, in a fully isolated server
(`HERDR_SOCKET_PATH` + `XDG_CONFIG_HOME` under `.tmp`, empty session),
replayed the old close sequence against a pane running a process that traps
`SIGINT` — a stand-in for a TUI that ignores `ctrl+c`:

```text
workspace present: True
agents on pane after grace: []
tab.close -> ok
workspace still present after tab.close: False
FINAL workspaces: []   FINAL panes: []
```

So `tab.close` is sufficient in the common case; the durable-ownership gap,
not the close sequence, is what made the leak permanent. The probe server was
stopped and its directory removed afterwards.

## B. What changed

- `reclaim_instance_carriers` closes and forgets every carrier a stopped
  Instance still owns durably. It runs when the PTY queue observes the
  Instance exited, and from `close_ended_instance`, so both the explicit-stop
  and ended-instance paths reclaim. It is best effort per carrier: one that
  will not close **keeps** its ownership row so the next sweep retries, rather
  than being forgotten while its pane survives.
- `PtyResource::close` closes the workspace's panes explicitly (agent pane
  first) instead of relying on `tab.close` alone, and verifies that no owned
  pane survived. The agent still gets `ctrl+c` first, now with a named 2 s
  grace: the Instance is already gone from the Hub's point of view, so an
  agent that ignores the interrupt must not keep the pane alive. Ownership is
  still matched on the unique creation label, so a workspace id reused after a
  server restart is never touched — `stale_workspace_id_never_closes_a_replacement`
  still passes.
- The startup orphan sweep (`--no-herdr-orphan-sweep` disables it) no longer
  aborts on the first carrier it cannot close, and closes a workspace's panes
  before the workspace itself.

### B. Tests

Against `fake-herdr`:

- `stopping_a_pty_instance_reclaims_its_durable_carrier_ownership` gives an
  Instance a durably-owned pane that the driver does not know about — the
  rebuilt-driver case — stops it through the ordinary command path, and
  asserts the pane, tab and workspace are gone and the ownership row dropped.
  **This test fails without the reclaim call** (the ownership row is still
  present and the pane still listed) and passes with it.
- `stopping_a_pty_instance_closes_its_agent_pane_and_forgets_ownership`
  covers the stubborn-agent case — `fake-herdr` never drops an agent on
  `ctrl+c` — and asserts a second Instance's pane is untouched.
- `startup_sweep_reclaims_panes_of_exited_instances` builds three exited
  `rmd-*` instances with live panes, the accumulated-orphan shape, and asserts
  the sweep reclaims all of them.

## Checks

`cargo fmt --all`, `cargo clippy --workspace --all-targets --locked -D warnings`
and `scripts/ci/secret-scan.sh` are clean.

Two `remuda-node` lib tests fail **identically on `origin/main`** in this
environment and are not related to these changes:
`native::tests::registry_constructs_all_three_native_claude_drivers` and
`stdio::tests::composed_stdio_dispatches_create_and_streams_journal`. Both
stem from a macOS Full Disk Access denial on `~/.claude` (`access probe timed
out`). Granting that is a system-settings change, which this task may not
make.
