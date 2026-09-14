# promote-bind-1 — Deterministic transcript binding on terminal promotion

Evidence for [D-025](../decisions.md) promotion follow-up: a hand-started Claude
in a `shell-pty` terminal must hydrate **its own** session transcript, never the
newest-mtime file in the cwd's slug directory. Companion to
[terminal-promote-1.md](terminal-promote-1.md).

- **When:** 2026-09-14, 08:36–08:47 local.
- **Where:** this worktree, `claude` 2.1.270 on macOS (Darwin 25.4).
- **Method:** on-metal probes against a scratch cwd `/tmp/promote-bind-probe`
  (canonical `/private/tmp/...`), a throwaway `--settings` file carrying a
  SessionStart shim, and the existing `~/.claude/sessions/` registry of running
  sessions. Nothing here is mocked; the payloads are verbatim Claude output.

## The bug, restated

The old locator picked the newest-mtime `*.jsonl` under the encoded-cwd slug
directory. Every Claude session started in the same directory lands in the same
slug dir, so the busiest *other* session won and the 结构 tab showed a stranger's
conversation. Start times and mtimes do not identify a process, so the fix binds
by **exact identity** only, in precedence:

1. argv `--session-id` / `--resume <uuid>`;
2. SessionStart hook whose `ppid` is the detected foreground pid;
3. `~/.claude/sessions/<pid>.json`, the pid-keyed registry;
4. nothing — the epoch stays **unbound** and the human gets an explicit picker.

A claim is locked for the promotion epoch; a later poll never swaps files.

## Channel B — the pid-keyed registry (`sessions/<pid>.json`)

Interactive Claude writes an exact pid → session record (a busy session from a
neighbouring worktree, shown verbatim):

```
$ cat ~/.claude/sessions/22990.json
{"pid":22990,"sessionId":"7426a992-45ce-47c3-9e02-c6e39f3f5f21",
 "cwd":"<home>/Documents/Projects/Community/example-worktree",
 "startedAt":1789323192836,"procStart":"Sun Sep 13 18:13:12 2026",
 "version":"2.1.270","kind":"interactive","entrypoint":"cli","pidDomain":"darwin",
 "status":"busy", ...}
$ ps -p 22990 -o pid,ppid,command
22990 22857 claude --settings <home>/.claude/settings.relay.json …
```

The registry key equals the foreground pid, and the record names the session id
and the cwd. `bind_by_pid_file` requires all three: file exists for the exact
pid, session file exists under the terminal cwd's slug dir, and the record's cwd
matches. A pid with no registry entry stays unbound (covered by
`a_promoted_session_is_bound_by_exact_identity_never_by_newest_mtime`).

Print mode (`claude -p`) does **not** write a pid file — it is not interactive
and is never a foreground promoted agent; this was observed in the first probe
(no `sessions/<ppid>.json` appeared).

## Channel A — the SessionStart hook, matched by exact `ppid`

The shim (documented on `SessionStartReport` in `claude_transcript.rs`) adds
only the parent pid to the standard hook payload:

```jsonc
// settings: hooks.SessionStart ->
// python3 -c 'import json,os,sys; d=json.load(sys.stdin); d["ppid"]=os.getppid();
//             print(json.dumps(d))'
{
  "session_id": "a2dacc06-fcb2-48d0-a030-b1ee0294a925",
  "transcript_path": "<home>/.claude/projects/-private-tmp-promote-bind-probe/a2dacc06-….jsonl",
  "cwd": "/private/tmp/promote-bind-probe",
  "hook_event_name": "SessionStart",
  "source": "startup",
  "ppid": 45890
}
```

The hook's parent **is** the foreground Claude process the promotion detector
already found by process group, so binding is an exact pid equality plus a
file-exists proof and a canonical cwd cross-check — no timestamp anywhere.

A real interactive PTY run (Python `pty.fork`, merged into a copy of the relay
settings) confirmed both identity channels agree while Claude is alive:

```json
{
  "ppid": 60216,
  "hook_session": "20f97b23-0b84-4633-b240-554afce6b085",
  "hook_cwd": "/private/tmp/promote-bind-probe",
  "pid_file_exists": true,
  "pid_session": "20f97b23-0b84-4633-b240-554afce6b085",
  "session_match": true,
  "cwd_match": true,
  "kind": "interactive",
  "transcript_exists": true
}
```

This channel is dormant in production until D-028's P1 launch shim emits via the
x-p1-signal worker (`remuda hook emit`); it was exercised here only with a
throwaway settings file, per the coordinator plan. The wire shape and pid match
are pinned by the fixture
`crates/remuda-driver/tests/fixtures/session-start-hook.json` and
`the_session_start_hook_fixture_has_the_documented_shape`.

## Channel ruled out — open file descriptors

```
$ lsof -p 22990 | grep -c jsonl
0
```

Claude never keeps the jsonl open (it writes and closes per append), so an
fd-based locator sees nothing. 26 fds total on the live process; the only
Claude-related REG entry is the executable itself. This is why the pid-keyed
registry (channel B), not lsof, is the fallback.

## The actual collision, reproduced

By the end of the verification runs the slug dir contained three transcripts —
exactly the old bug's topology:

```
305437 .../-private-tmp-promote-bind-probe/20f97b23-….jsonl   ← foreground session (correct)
305435 …/-private-tmp-promote-bind-probe/29a887fc-….jsonl   ← another interactive session, same cwd
165749 …/-private-tmp-promote-bind-probe/a2dacc06-….jsonl   ← earlier print-mode session
```

Identity binding claims `20f97b23` from the foreground pid even though none of
the files has any time-based claim; a "newest mtime" rule could pick either of
the other two depending on which session happened to write last. The
second-poll stability is pinned by the promotion test
`the_pid_file_binds_the_foreground_session_and_a_second_poll_does_not_switch`.

## Unbound behaviour and the manual picker

With no argv id, no matching hook, and no pid registry entry, the epoch stays
**unbound**: nothing is hydrated (so a stranger's transcript can never be
shown), the header chip reads 「未绑定 transcript」, and a SingleSelect question
lists candidates with session id, started time, first-user-prompt excerpt and
last activity. Only an explicit answer binds (`source: manual`); the slug dir is
rescanned every 3 s for late-appearing candidates. A vanished file or a
content-record cwd mismatch degrades the locked claim with a chip state change
rather than silently rebinding. Picker transport is the existing Interaction
question round-trip — no protocol additions.

## D-028 relationship

D-028's P1 launch shim starts Claude with the session id and transcript path
known up front, which binds on channel 1 (argv) before any detection happens.
The hook and pid-file channels then exist for hand-started sessions in
shell-pty terminals (this bug's case) and as defence in depth.

## Test surface

- `crates/remuda-driver/src/claude_transcript.rs`: identity binding, cwd
  canonicalization, candidate listing/excerpts, tail truncation.
- `crates/remuda-driver/src/shell_pty/promotion.rs`: precedence, epoch lock,
  second-poll no-switch, `--resume`, late-appearing pid file, unbound picker,
  manual answer validation, first-answer-wins, vanished-file degrade, epoch
  reset on demote.
- `crates/remuda-driver/tests/terminal_promote.rs`: two-channel identity
  fixture test and the hook payload shape test.
- `web/src/lib/transcriptBinding.test.ts` and
  `web/src/pages/SessionPage.transcript-binding.test.tsx`: chip states
  (bound + channel label, unbound, hidden when not promoted).
