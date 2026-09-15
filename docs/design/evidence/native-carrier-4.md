# native-carrier-4 — screen read, macOS credential namespace, bypass spelling, PTY release

- **When:** 2026-09-16
- **Where:** task worktree `remuda-wt/c-native3` (branch `wt/c-native3/native-carrier-macos`)
- **Agent:** c-native3
- **Host:** the owner's **macOS** Mac (Darwin 25.4.0, arm64), claude **2.1.272 → 2.1.273** (auto-updated mid-session; both reproduce), demo Hub `127.0.0.1:18080`
- **Predecessors:** `native-carrier-3.md` (verified on Linux only), D-028 §4.2/§4.6/§5.1/§5.3/§5.5
- **Flags under test:** `REMUDA_PTY_CARRIER=native REMUDA_PTY_EMULATOR=1 REMUDA_PTY_HOOKS=1`

The coordinator's macOS retest reported: a `shell-pty` / `bypass` claude session
sat `running / idle` for two minutes with no reply, `DELETE ?force=1` logged
`node rejected instance.purge`, and the claude process stayed `?Es` as a child
of the Node. `remuda instance read` could not show the screen, so nobody could
see *why*.

Four independent defects, each reproduced on the live demo and each fixed and
re-verified there.

---

## 1. The screen read (built first — it is what diagnosed the rest)

`remuda instance read --source screen` used to filter the journal for
screen-shaped events and, finding none, answer
`screen/keys require a tty-attach driver such as generic-pty`. A native session
that parks *before* `SessionStart` has an empty journal by construction, so the
one tool that could explain the failure was the one that refused to run.

`tty.screen` is a new read-only RPC returning the emulator grid the Node
already keeps: `Driver::screen_read` (default `None`), `ShellPtyDriver`'s
implementation over `PtyState::screen_grid()`, `GET /v1/instances/{id}/screen`,
and the CLI branch. It opens no stream and moves no offset — unlike
`tty.attach` — so it is safe against a session a human is watching, and it
reports whether the grid came from the emulator or a raw-ring fallback rather
than passing a degraded read off as a repaint.

Pointed at a fresh native session on the demo, it answered immediately, and the
answer contained both remaining defects at once:

```text
source=emulator size=80x24 altScreen=True

 ▐▛███▛█   Claude Code v2.1.273
▝▜██████▀  Opus 5 (1M context) · API Usage Billing
  ▝▝ ▝▝    $HOME/Documents/Projects/Community/hybrid-harness

❯ Reply with exactly the single word PONG and nothing else.
  ⎿  Not logged in · Please run /login

✻ Brewed for 0s · done

                                                    Not logged in · Run /login
────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏸ manual mode on · ? for shortcuts · ← for agents
```

**No trust dialog and no bypass disclaimer** — the two screens
`native-carrier-3` predicted. The agent had reached its composer, run the turn,
and answered `Not logged in`; and the mode line said `manual`, not `bypass`.

### Wiring note

Three Node dispatchers end in `_ => Ok(json!({"ok": true}))`
(`runtime_wss.rs`, `runtime_link.rs`, and the codec's siblings). An unhandled
`tty.screen` would have returned a cheerful empty success there rather than an
error, so each got an explicit arm. `agent_scope::read_target` also had to learn
the `/screen` suffix or agent-scoped callers get 403.

## 2. `Not logged in`: the scoped config dir moves the keychain namespace

Claude 2.1 namespaces its OS credential store **by config directory**. From the
2.1.272 binary:

```js
var iQ = "-credentials";
function aP(n = "") {
  let e = process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR,
      t = e !== void 0 ? !e : !process.env.CLAUDE_CONFIG_DIR,
      r = e !== void 0 ? e.normalize("NFC") : be(),
      c = t ? "" : `-${a("sha256").update(r).digest("hex").substring(0, 8)}`;
  return `Claude Code${Xt().OAUTH_FILE_SUFFIX}${n}${c}`;
}
```

The suffix is empty only when the config dir is **unset**. So the per-instance
native home the carrier pins (`native-carrier-3` §2.1) sends the CLI to
`Claude Code-credentials-<hash>`, a keychain service nobody has ever logged
into. This machine's keychain holds the unsuffixed `Claude Code-credentials`
and nothing else.

Reproduced with no Remuda in the picture:

| Command | Result |
| --- | --- |
| `CLAUDE_CONFIG_DIR=<tmp> claude -p "…PONG…"` | `Not logged in · Please run /login` |
| the same plus `CLAUDE_SECURESTORAGE_CONFIG_DIR=""` | `PONG` |

**Fix:** a launch that pins `CLAUDE_CONFIG_DIR` for Claude now also exports
`CLAUDE_SECURESTORAGE_CONFIG_DIR=""`, selecting the default namespace. The
config stays scoped; only the credential *lookup* follows the operator. Remuda
neither reads, copies nor writes the credential — the value never leaves the OS
keychain — and an inherited (unpinned) home is left alone, since it already
resolves to the default namespace and overriding it could contradict an
operator who set the variable themselves.

This is why the Linux run passed: Linux has no keychain, so the same code path
falls back to a file the scoped home does not namespace away.

## 3. `permissionMode: "bypass"` silently became manual

The Hub forwards the wire value verbatim; the Node matched only
`bypassPermissions` / `bypass-permissions`, so `"bypass"` — what the
coordinator's probe, `hub/tests/lifecycle.rs`, and the Hub's own project-default
normalizer all use — fell through to `Manual`. The stored recipe for the failed
run records it: `"permission":{"cliMode":"default"}`.

The consequence was not only the mode line: `spec_bypass_permissions()` was
false, so the launch never seeded `skipDangerousModePermissionPrompt`. The
disclaimer suppression added in `native-carrier-3` was dead code for every
request that used this spelling. (The demo avoided the disclaimer anyway,
because `seed_scoped_config` copies that key from the host settings — so the
defect was masked until the screen showed `manual mode on`.)

## 4. The PTY was never really closed, so the leader could not be reaped

`portable-pty` hands out **three** file descriptors on the master: the box, a
`take_writer` dup, and the reader thread's `try_clone_reader` dup. The stop
ladder's `close_master` dropped only the box. The slave therefore never saw its
hangup, and on macOS the SIGKILLed session leader parked in `E` (*trying to
exit*) **indefinitely** — never becoming a zombie, so `waitpid` could never
reap it. The kernel will not finish an exit while the terminal still has a live
endpoint.

Measured on the demo before the fix: the process entered `?Es` 6 s into the
delete and was still `?Es` 60 s later; it disappeared only when the Node itself
exited.

The old code called this "left for init", which is wrong: init adopts orphans
only once the *parent* exits, and the Node keeps running, so an abandoned
leader stays its child for the life of the Node. That warning is now an
`ERROR: reap-incomplete`.

**Fix:** the ladder releases the writer alongside the box; the reader's dup then
sees EOF and closes itself. The ladder now finishes at the **SIGHUP** rung — it
no longer needs `SIGKILL` at all — and the leader is reaped in ~1 s.

### `?Es` is not the run state `E`

macOS `ps` puts the run state first (`I R S T U Z`, or `?` when the kernel
state is none of those) and **flags** after: `E` = trying to exit, `s` = session
leader. So `?Es` is "state unknown, exiting, session leader", and the
`native-carrier-3` classifier reads it correctly only because it strips `?`
before looking at the first character. A unit test now pins the real spellings,
including that `Ss` must stay *live* — a sleeping session leader is the most
common row in the table, and reading it as dead would report every healthy
agent as gone.

## 5. `instance.purge` rejected on a forced delete

`purge_instance` waited a fixed 50 × 100 ms = 5 s for `Exited`/`Failed`, then
refused. The shell-pty ladder needs longer (SIGINT 2 s + SIGHUP 2 s + SIGKILL
0.5 s + reap grace), so the purge timed out. Demo timings: purge rejected at
`19:23:49.071`, driver close finished `19:23:50.953`.

Refusing did not keep the session alive. The Hub only purges behind a delete it
has already decided to perform, and it drops its row regardless — so the refusal
just orphaned the process and leaked the instance directory.

**Fix:** a purge that finds a live instance closes its driver and proceeds. It
still refuses if the close itself fails, so a directory is never removed out
from under a running process.

---

## 6. Before / after

Real 2.1.27x binary, PONG probe, native shell-pty carrier, on macOS:

| | retest (native-carrier-3 §3) | this branch |
| --- | --- | --- |
| screen visible to an operator | not at all | `GET …/screen`, emulator grid |
| auth | `Not logged in · Please run /login` | `Claude Max` |
| permission mode | `⏸ manual mode on` | `⏵⏵ bypass permissions on` |
| journal channels | 0 / 0 / 0 / 0 | runtime + hook + pty + transcript |
| reply | none after 126 s | **PONG at t+13 s** |
| forced delete | `node rejected instance.purge` | `200`, no rejection |
| stop ladder | `stop-incomplete`, outlived SIGKILL | stops at `sighup` |
| leader after delete | `?Es` until the Node exited | reaped in ~1 s, none left |

The fixed screen:

```text
source=emulator size=80x24 altScreen=True

 ▐▛███▛█   Claude Code v2.1.273
▝▜██████▀  Opus 5 (1M context) · Claude Max
  ▝▝ ▝▝    $HOME/Documents/Projects/Community/hybrid-harness

❯ Reply with exactly the single word PONG and nothing else.

⏺ PONG

✻ Cooked for 1s · done

                                                              ● high · /effort
────────────────────────────────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
```

Node journal for that run, in order: `SessionStart → UserPromptSubmit →
agent_status(working) → MessageDisplay(delta "PONG") → Stop →
agent_status(idle)`.

`coord-hookcheck.py shell-pty` (a session that runs `sleep 20` as a tool, then
replies) also passes: PONG at t+29 s, 20 events,
`{runtime: 7, hook: 10, pty: 3}`, cleanup 200, no process left.

> `coord-native-test.sh` prints `journal: 0` even on a good run: it reads
> `.items` from `GET …/journal`, which returns `.events`. The reply and all four
> channels are present — confirmed from the Hub body and the Node's own journal
> file. Its `stop-incomplete lines total: 3` counts the whole historical log;
> all three are from 2026-09-14/15, before this branch.

## 7. Tests added / changed

- `remuda-driver`
  - `shell_pty.rs`: `a_live_pty_reads_its_screen_and_an_unstarted_one_says_it_has_none`;
    `a_bypass_native_launch_pins_its_home_and_suppresses_the_disclaimer` extended
    to assert `CLAUDE_SECURESTORAGE_CONFIG_DIR` is empty exactly when the home is
    pinned and absent when it is not.
  - `shell_pty/lifecycle.rs`: `macos_flag_letters_do_not_change_the_run_state_reading`
    (the `?Es` / `?E` / `Es` spellings, and `Ss` staying live).
  - `spawn_leader_unreaped` narrowed to the Linux test that uses it — it was
    `cfg(unix)` with a Linux-only caller, hence dead code on macOS under
    `-D warnings`.
- `remuda-node`
  - `tests/pty_lifecycle.rs`: `purging_a_live_pty_stops_it_instead_of_orphaning_it`.
    Verified it **fails** against the old refuse-on-live behaviour before keeping it.
  - `native.rs`: `every_bypass_spelling_reaches_bypass_permissions`, including
    that an unknown posture still falls back to `Manual`.
- `remuda-hub`: `openapi.json` documents `/v1/instances/{id}/screen`; the
  existing contract test enforces it.

## 8. Test results

`cargo test -p remuda-driver -p remuda-node -p remuda-hub -p remuda`: all suites
pass except two failures that are **pre-existing on `main` (f76614f6)** and
untouched by this branch — verified by checking out the base sources and
re-running:

- `remuda-driver --test adapters_parity::codex_and_grok_adapter_dumps_are_journal_diff_parity`
  (grok session-file paths; fails identically at the base)
- `remuda --test merge_queue_cli::queue_killed_lane_is_a_failed_branch_while_others_land`
  (fails identically at the base)

One further flake, also in untouched code:
`lifecycle::tests::a_cooperative_group_stops_at_the_first_rung` reaches the
`Hangup` rung instead of `Interrupt` under parallel load (its `SIGINT` grace is
2 s). 8/8 green when run alone.

`cargo clippy --workspace --all-targets -- -D warnings`: clean.
`just secret-scan`: pass. Web untouched, so no web checks were needed.

## 9. Timings

- First PONG on the fixed build: **13 s** from create (hook `SessionStart` →
  prompt → reply), model time "Cooked for 1s".
- Forced delete: HTTP 200 in ~2 s; leader reaped within 1 s of the ladder's
  SIGHUP rung; `ps` shows no `2.1.27x` process afterwards.
- Screen read: answered within the 5 s Hub budget on every call (the Node
  serves it from the in-memory emulator grid).

## 10. Not done / follow-ups

- `coord-native-test.sh` reads `.items` instead of `.events`; the script is the
  coordinator's, so it is reported rather than edited here.
- The two pre-existing test failures in §8 are left for their owners.
- The credential-namespace fix is keyed to Claude's 2.1 scheme. If a future
  release changes how `aP()` derives the service name this needs revisiting;
  the unit test pins Remuda's half (the env var is exported) but cannot pin
  Claude's.
