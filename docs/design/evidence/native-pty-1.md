# native-pty-1 — hook socket, launch shim and overlay, live run

Evidence for [D-028](../decisions.md) §4.2 (signal adapters, hook socket,
overlay materializer, launch shim), §4.3 (lifecycle from the hook channel),
§9.2 (tui / terminal-signal pinning), and the **P1 acceptance criterion**:
*a `claude` the user types by hand produces structured hook events in the
journal*, with zero herdr and zero protocol change.

- **When:** 2026-09-13, 21:52–22:29 UTC.
- **Where:** `remuda dev` from this worktree, Hub `127.0.0.1:62080`, Node
  `127.0.0.1:62087`, data dir and workspace under a scratch directory.
- **Agent:** `claude` 2.1.270, started by **typing `claude` — bare, no flags —
  into a `terminal` / `shell-pty` instance**.
- **Flags:** `REMUDA_PTY_HOOKS=1` (off by default), `promote_terminal_agents`
  on. Nothing else changed.

Nothing here is a fixture. The recorded payloads under
`crates/remuda-testing/fixtures/hooks/` come from the same binary.

## 1 — What the instance gets at launch

A `terminal` instance with hooks enabled materializes four things under its own
directory and nothing anywhere else:

```
instances/<id>/
  hook.sock              srw-------   the per-instance socket
  launch/                drwx------
    settings.json        -rw-------   the merged --settings overlay
    bin/                 drwx------
      claude codex grok  -rwx------   transparent exec shims
    zdotdir/             drwx------   shadow zsh rc files (§4 below)
```

Permissions are as designed: the socket is `0600` inside a `0700` directory, so
only the Node's uid can connect before any credential is checked.

The overlay registers the relay for all twelve events and pins the three §9.2
keys. One entry, verbatim from the run:

```json
"SessionStart": [{"hooks": [{"type": "command", "command":
  "'<node>/remuda' hook emit --socket '<instance>/hook.sock' --event 'SessionStart'"}]}]
```

The credential is **not** on that command line — it reaches the relay through
the child's environment, because a command line is readable by every process on
the machine via `ps`.

## 2 — Typing `claude` by hand

`instance.send` of the literal text `claude` — the same bytes a human types.
The shim resolved and the agent started with the overlay attached:

```
$ ps -o args -p 13363
/Users/…/claude --settings <instance>/launch/settings.json \
                --setting-sources user,project,local
```

Nobody passed a flag. That is the whole of P1: Remuda can put flags on a
command it spawns, but not on one a human types, and the shim closes that gap.

`command -v claude` inside the session reports the shim (expected — risk #2
says so explicitly), and the shim still resolves the real binary:

```
$ command -v claude
<instance>/launch/bin/claude
$ echo "${PATH%%:*}"
<instance>/launch/bin
```

## 3 — The journal

Every hook event, on `channel: hook`, from the hand-typed agent (`ppid=13363`
is the live `claude` process):

```
 12 ch=runtime  agent_detected    agent detected: claude   kind=claude
 13 ch=runtime  agent_promoted    claude                   kind=claude mode=promoted
 14 ch=pty      agent_status      idle
 28 ch=hook     SessionStart      idle       ppid=13363
 33 ch=hook     UserPromptSubmit  working    ppid=13363
 35 ch=hook     StopFailure       idle       ppid=13363
 42 ch=hook     UserPromptSubmit  working    ppid=13363
 44 ch=hook     StopFailure       idle       ppid=13363
 47 ch=hook     Notification      waiting    ppid=13363
 59 ch=hook     UserPromptSubmit  working    ppid=13363
 61 ch=hook     MessageDisplay    streaming  delta=hooked final=true index=0 ppid=13363
 62 ch=hook     Stop              idle       ppid=13363
```

Reading it:

- **seq 28** — `SessionStart`, carrying the native session id
  `f4447414-77c0-4ae4-8e45-aa32a1e724c6` and the real transcript path. This is
  the D-026 unlock; see §5.
- **seq 33 → 62** — the turn opens on `UserPromptSubmit` and closes on `Stop`.
  Turn state comes from the harness saying so, not from a screen guess.
- **seq 35 / 44** — two `StopFailure`s: the host's configured default model
  (`passthrough/ark/seed-evolving`) is not available to this account, and the
  transcript says so in as many words. **The mapping handled it correctly** —
  `StopFailure` ends the turn (the composer comes back) and stays below
  `Severity::Error`, so the instance is not folded to `failed` for what is a
  bad turn rather than a broken session. Switching model in-session with
  `/model` gave the clean turn at seq 59–62.
- **seq 61** — `MessageDisplay` with `delta: "hooked"`, `index: 0`,
  `final: true`. This is §7's line-level streaming, on the wire, from a
  hand-typed agent. P3 maps these to message mutations; P1 persists them whole
  so that is a mapper change rather than a re-capture.
- **seq 47** — `Notification` journaled as an interaction observation and **not
  answered**. P1 is observe-only: the reply is always `{}` and the agent keeps
  its own prompt, so there is never a state where Remuda believes it answered
  and the agent believes it did not.
- **seq 16 / 17** — two earlier `SessionStart`s with other pids. Those are
  deliberate probes of the relay against the live socket, not agent events;
  they are what proves the transport independently of the harness.

## 4 — What the live run corrected

Four defects the live run exposed — three of them only a real login shell would
have shown. Each now has a regression test.

1. **The shim was buried by the login profile.** Putting the shim directory at
   the front of the child's `PATH` is not enough: `~/.zprofile` and `~/.zshrc`
   routinely *prepend* to `PATH`, and by the time the human had a prompt our
   entry was at **position 22 of 35** — `command -v claude` resolved to the
   user's own binary, no overlay, no hooks. P1's headline criterion was
   silently not met. Fixed with a generated shadow `ZDOTDIR` whose rc files
   source the user's real ones and then put the shim back in front. Ordering,
   not force.
2. **`ZDOTDIR` restored too early.** zsh re-reads `ZDOTDIR` before each of
   `.zshenv`, `.zprofile`, `.zshrc` and `.zlogin`. Restoring the user's value
   permanently in the first file sent zsh to `$HOME` for all the rest, so only
   `.zshenv` ever ran. Each file now swaps only while sourcing the user's rc;
   the last one hands `ZDOTDIR` back for good, so the interactive shell the
   human ends up in reports their own value.
3. **Hook evidence reached the journal but not the instance.** The Node's
   observation pump folded activity only from `agent_status` and `session`,
   both screen-derived. The hook events were being journaled while the
   instance's own `activity` kept following the screen — so §4.3's
   `Hook > Screen` ranking was true of the envelope and false of the thing the
   composer actually reads. The pump now folds turn boundaries and interaction
   events from the hook channel. Tool events and `SubagentStop` deliberately do
   not qualify, and the *channel* decides rather than the event name, so a
   screen-derived `UserPromptSubmit` cannot masquerade as one.
4. **The shim could exec-loop.** It located itself with `dirname`, which lives
   in `/usr/bin`; a `PATH` without it left the shim unable to recognise itself,
   so it resolved to *itself* and forked without bound. It now uses shell
   builtins, skips `$0` explicitly, and carries a re-entry guard that degrades
   to one pass-through. The tests that exercise it are time-bounded so a
   regression fails the test rather than the machine.

## 5 — Resume (D-026)

Before this phase a promoted `shell-pty` instance had no native session id —
`nativeRef.sessionId` held the placeholder `ins_…`, which `claude --resume`
rejects. After the `SessionStart` hook:

```json
{
  "kind": "claude", "driver": "shell-pty", "mode": "promoted",
  "lifecycle": "exited",
  "nativeSessionId": "f4447414-77c0-4ae4-8e45-aa32a1e724c6",
  "nativeTranscriptPath": "/…/.claude/projects/-private-tmp-x-p1-ev-ws/f4447414-….jsonl"
}
```

`POST /v1/instances/<id>/resume {"mode":"terminal"}` — the **existing** D-026
endpoint, unchanged — is accepted and creates a linked child:

```json
{"kind": "claude", "driver": "claude-pty", "lifecycle": "requested",
 "resumedFrom": "ins_01a09cd6-b6c6-724b-8a28-1da0c12aa592"}
```

The parent stays `exited` with its history; the child records where its
conversation came from. **Caveat:** resume still routes `mode: "terminal"` to
the `claude-pty` (herdr) driver, so the child does not start on a Node without
herdr. That routing is pre-existing D-026 behaviour and out of this phase's
scope — §5.6 assigns it to P2, which makes resume a new session with a
pre-filled `--resume`. What P1 had to unlock, and did, is the session identity
that makes the call possible at all.

## 6 — Security notes

**Socket permissions.** `<instance dir>/hook.sock` is `0600` inside a `0700`
directory. The directory mode carries the guarantee: on some filesystems the
socket mode is advisory, so the containing directory is the boundary that
actually holds. Both verified on the live run.

**Credential scope.** Per-instance, 32 bytes from the OS CSPRNG, minted at
launch and never persisted or written to configuration. It is **not** the
Node's device or bootstrap token, and there is no code path by which it could
be one — it is generated in `launch/session.rs` and read from nowhere. What it
authorises is exactly one thing: appending hook events to one instance's
journal, for as long as that instance lives. It is compared length-then-bytes
so a wrong credential costs the same whatever it is, and a rejected event never
reaches the journal. Requests are capped at 4 MiB before being buffered.

**What env is injected, and why it bypasses the allowlist.** `child_env`
inherits a closed allowlist (§4.2) whose purpose is to keep the Node's own
credentials, `LD_PRELOAD`, proxies and CA overrides out of a process the model
can read. Three names are added on the *driver-computed* side of that line,
which is where §4.2 says new variables go:

| Name | Why it cannot be inherited |
|---|---|
| `PATH` | rewritten to put the shim directory first; the inherited value is the input, not the output |
| `REMUDA_HOOK_CREDENTIAL` | a secret this driver minted; `REMUDA_` is a denied prefix precisely so an *inherited* one can never reach a child |
| `ZDOTDIR` | points at the generated shadow rc directory |

`REMUDA_USER_ZDOTDIR` carries the user's own value through so the shadow rc
files can source their real configuration. `ZDOTDIR` was added to the inherit
allowlist for that reason — without it, someone who keeps zsh config outside
`$HOME` would silently get a bare shell.

**The user's own configuration is never written.** The overlay only ever merges
and only ever lives under the instance directory; `~/.claude/settings.json` is
read by the harness through `--setting-sources` and never by Remuda. A user's
own `SessionStart` hook still fires — verified against claude 2.1.270, where
both the user's and the overlay's hooks run. `instance.purge` removes the
instance directory and never touches `~/.claude`.

**Opting out.** `REMUDA_PTY_HOOKS` is off by default: no socket, no overlay, no
shim. `REMUDA_SHIM=off` disables the shim while keeping the rest. An absolute
path (`/usr/local/bin/claude`) bypasses the shim entirely — a documented
degradation, not a defect: the signal tier drops to screen and the UI must say
so.

## 7 — Not covered here

- `PermissionRequest` and `Elicitation` are registered and journaled but **not
  answered**; deciding them is P5. The recorded fixture includes a real
  `PermissionRequest` payload so that phase starts from evidence.
- `codex` and `grok` get pass-through shims only. Their `CODEX_HOME` /
  `GROK_HOME` shadow directories are named but deliberately **not created**: an
  empty one would lose the user's real config without replacing it, which is
  worse than leaving the variable unset. P6 fills them in.
- The shadow `ZDOTDIR` is zsh-only. bash's login sequence has no equivalent
  single hook (`--rcfile` does not apply to login shells, and `BASH_ENV` is
  denied as a code-execution vector), so a bash login shell keeps the plain
  `PATH` injection and lands wherever its profile leaves it.
- Binding a hook stream to an instance is by agent pid, and the Node currently
  re-derives the match from the journal. `TODO(x-promote-bind)` marks where
  `bind_by_session` replaces that once it lands.
- No web screenshot: the UI surface for hook-derived state is P3/P7.
