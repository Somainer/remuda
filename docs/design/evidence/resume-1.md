# D-026 session continuity acceptance

Exercised through an isolated `remuda dev` (Hub `127.0.0.1:60680`, native Node
`127.0.0.1:60687`) with data dir `/tmp/remuda-x-resume-evidence`, an isolated
registered workspace, and the host's own Claude login
(`claude 2.1.270`, model `sonnet`, `permissionMode: dontAsk`). Real model
requests were made. Access codes and device tokens are omitted; local paths are
shown as they appeared.

## The two reported problems

1. 「Resume 没用」— an exited `claude-print` session's Resume button did
   nothing useful.
2. 「agent 会话有没有回到 Terminal view 的可能性」— a structured-only session
   has no terminal.

Both were reproduced from the same root cause chain and are shown fixed below.

## Why Resume did nothing

`instance.resume` was never handled. It fell through the Hub→Node dispatch
catch-all and returned `{"ok": true}`:

```text
WARN node did not durably accept command; will not resend
  command_id=cmd_01a09b12-00dd-73a1-9065-da22dc46a740
  response={"jsonrpc":"2.0","result":{"ok":true}}
```

The instance created by that resume stayed at `lifecycle: requested` with one
journal event and never materialized. This log line is from the run *during*
this work, after the Hub route existed but before the WSS runtime dispatch
carried `instance.resume`; the pre-change behaviour was the same silent
`{ok: true}`, reached one layer earlier.

Even with dispatch fixed, resume could not have worked: `nativeRef.session_id`
held a placeholder minted from the Instance id at create time, which
`claude --resume` rejects. Nothing ever replaced it with the id the driver
reported.

## A bug this evidence run caught

The first live run recorded a session id that did not match any transcript:

```text
recorded nativeSessionId   13cffc9d-7ed3-4f2f-af70-4336ee0ce223
actual transcript file     2a99b5d4-f182-425f-ab5b-7f3541dee9bc.jsonl
```

The journal showed why — claude-print emits hook lifecycles whose `nativeId` is
a **hook** id, and the original filter accepted any `session`/`hook` topic:

```text
session  session       {"value": "2a99b5d4-f182-425f-ab5b-7f3541dee9bc"}
hook     hook_started  {"value": "f121381b-c2c9-47fd-a253-f5efea4e4c94"}  hookEvent=SessionStart
hook     hook_started  {"value": "7057b9b1-ad93-4285-b1b7-0f254056e9a0"}  hookEvent=SessionStart
```

A later hook overwrote the correct session. The filter now matches only
`session`/`session` and claude-pty's `hook`/`SessionStart` (which proves itself
by naming a transcript path), covered by
`runtime::tests::only_session_bearing_lifecycles_are_read_as_the_native_session`.
After the fix the recorded id stayed stable across 15 polls spanning many hook
events, and matched the transcript file on disk:

```text
nativeSessionId  e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e
transcript       ~/.claude/projects/-private-tmp-remuda-x-resume-evidence-workspace/
                 e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e.jsonl
```

## 1. Structured resume continues the prior context

Parent `ins_01a09b15-76ff-774a-9862-8c588ae39c30` (claude-print) was given a
phrase, answered, then closed:

```text
user       Remember this exact phrase: PURPLE-ANCHOR-7731. Reply with just OK.
assistant  OK
lifecycle  exited   session e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e
```

`POST /v1/instances/{parent}/resume {"mode":"structured"}` returned a **new**
instance, not the exited one:

```text
mode structured  replayed false
child ins_01a09b16-085b-766b-a045-b258c3faeb14  driver claude-print
resumedFrom ins_01a09b15-76ff-774a-9862-8c588ae39c30
```

Asked what it had been told to remember, the resumed session answered from the
prior context:

```text
user       What exact phrase did I ask you to remember? Reply with just the phrase.
assistant  PURPLE-ANCHOR-7731
```

## 2. 在终端中继续 — the same session, now with a terminal

`{"mode":"terminal"}` against the same exited parent produced a `claude-pty`
instance on the same native session:

```text
mode terminal  child ins_01a09b16-5ae3-72bb-b99c-b3b0da25adb8
driver claude-pty  resumedFrom ins_01a09b15-76ff-774a-9862-8c588ae39c30
lifecycle running  nativeSessionId e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e
```

It is a real terminal (herdr pane) whose own SessionStart hook reports the
inherited session and the same transcript:

```text
session  session        herdrSession=remuda-node-d8ebe8d36b22527731549818  paneId=w1:p2
hook     SessionStart   nativeId  e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e
                        transcript .../e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e.jsonl
```

The live process confirms the flag contract — exact session id behind
`--resume`, no `--continue`, no `--session-id`:

```text
claude --permission-mode default --setting-sources user,project,local \
  --model sonnet --resume e50e12ca-c1a9-4605-9bb9-f1bd1f4bfc1e \
  --settings /tmp/remuda-x-resume-evidence/data/dev...
```

One transcript file holds all of it — original session, structured resume, and
the terminal continuation write to the same 39-line JSONL:

```text
user       Remember this exact phrase: PURPLE-ANCHOR-7731. Reply with just OK.
assistant  OK
user       What exact phrase did I ask you to remember? Reply with just the phrase.
assistant  PURPLE-ANCHOR-7731
```

## Links, idempotency, and refusals

The exited parent keeps its history and records where the conversation went;
each child records where it came from:

```text
parent lifecycle   exited
parent journal     resumed-into -> ins_01a09b16-085b-766b-a045-b258c3faeb14
                   resumed-into -> ins_01a09b16-5ae3-72bb-b99c-b3b0da25adb8
child  journal     resumed-from -> ins_01a09b15-76ff-774a-9862-8c588ae39c30
```

Repeating the structured resume inside the window reused the existing child
rather than launching a second process against the same conversation:

```text
replayed true  child ins_01a09b16-085b-766b-a045-b258c3faeb14
```

A session with nothing to resume is refused with a reason instead of quietly
starting an empty conversation:

```text
HTTP 409
{"error":"resume supports Claude sessions; this one is terminal"}
```

The same 409 shape covers a Claude session that never reported a session id
(`this session never reported a native session id, so there is no transcript to
resume`) and one whose transcript is past the 30-day retention cutoff; the Hub
route test asserts the first, and the web store surfaces the message verbatim
so the user sees which case they hit.

## Scope

Agent callers are refused (`403`) by the Hub route test
(`resume_creates_a_linked_child_on_both_targets_and_refuses_agents`); this run
used a Human device token throughout. The `claude-bg`, `generic-pty` and
`shell-pty` drivers report resume unsupported and were not exercised here
beyond the `terminal` 409 above.
