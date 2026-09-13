# Intranet acceptance 2 — shell and Claude PTY replies verified

Status: **PASS for the bounded Mac acceptance described below**. Both instances
returned `PONG` and reached `exited` after explicit close. The Mac Node remained
online through outbound WSS at the final check. Target: `https://remuda.<zone>`.

This record follows [intranet-enroll-1.md](intranet-enroll-1.md), whose last
Claude result was blocked on native SessionStart. That historical record remains
unchanged. The earlier Caddy activation is in
[intranet-hub-1.md](intranet-hub-1.md).

## Scope and provenance

The new worktree branch was `wt/c-deploy/intranet-acceptance-2`, starting from
fetched `origin/main` at `2f569cb6b2c7f59a603181c0a0fee9936ac3aadc`.
Live work ran on 2026-09-13, from 09:26:33 through the final 09:30:20 UTC check
(17:26:33–17:30:20 Asia/Shanghai).

| Component | Recorded revision or identity |
| --- | --- |
| Installed Mac Node | `2f1e58026cb1afb0f661f05a3d0fff09a3fb22e9` |
| Mac binary target | `aarch64-apple-darwin` |
| Mac binary SHA-256 | `9b522e8e7cf233f37b1a8473b35313e97d2321859c14e3c12f08e46c83ce87c1` |
| Existing Hub revision | `9dd7ec7b59cd43ea325c2bbe2404210ffe31ff2c` |
| Node carrier | `outbound-wss` |

The user reported manually granting Full Disk Access before this attempt.
This run did not operate the permission GUI, change TCC state, or independently
audit that grant. The subsequent successful workspace probe is an observed
result; it does not by itself establish which permission change caused recovery.

The daemon retained the real repository, shown as `~/<repo>`, as its sole
workspace root. That selection was configured during the preceding stopgap; the
throwaway root was already deselected. This run verified the configuration and
restarted the local daemon. No Node code or binary was changed during this
milestone. No Caddy change, Hub upgrade, SG/bolt Node rollout, or tunnel was used.

## Restart and workspace checks

`restart.private.json` records restart exit 0, a new daemon PID, and an online
Hub host afterward. The host ID and persisted host token matched their
pre-restart values; only the equality results are retained here.

The initial workspace probe passed in **0.184 seconds**. It reported one
configured workspace root and two entries in the separate worktree catalog.
The final probe passed in **0.307 seconds**, with the same counts. These are
successful probes of the real repository, not additional workspace registration.

## Session results

| Check | Shell | Claude |
| --- | --- | --- |
| Driver / kind | `shell-pty` / `terminal` | `claude-pty` / `claude` |
| Created, UTC | 09:28:09.468 | 09:28:12.196 |
| Create accepted | true | true |
| Captured `PONG` marker | true | true |
| Recorded capture wait | 1.017 s | 31.507 s |
| Captured TTY frames | 46 | 97 |
| Captured TTY bytes | 823 | 19,262 |
| Capture error | null | null |
| Journal watermark before close | 7 | 12 |
| Journal watermark after close | 10 | 16 |
| Lifecycle after close | `exited` | `exited` |
| Closed, UTC | 09:28:11.609 | 09:28:46.572 |

The wait values are the capture helper's recorded waits to its marker condition;
they are not provider API latency measurements. Terminal output was captured
through the authenticated Hub TTY path. It was not inferred from a successful
create/send response or from the journal's copy of the submitted input.

Claude requested `delegation: none`, model `haiku`, permission mode `default`,
and `maxBudgetUsd: 0.3`. The native reply identifies the actual model below.
The requested budget was USD 0.30; actual cost was not measured.

## Journal evidence: queued input and delivery

The private Hub journal response stores each Node observation under
`.events[].event`. The excerpts below retain only selected fields and replace
opaque identities with stable labels. `<shell-message>` and `<claude-message>`
each refer to the same message before and after its revision change.
Prompt text, local paths, credentials, and unrelated event fields are omitted.

Common predicates for these rows are `kind: message`, `source.channel: runtime`,
`payload.role: user`, and `payload.phase: input`. Fields in these projections
come from the row's `seq` and the observation's `payload`:

```json
[
  {"instance":"<shell-instance>","seq":5,"messageId":"<shell-message>",
   "status":"queued","operation":"open","revision":"1","baseRevision":null},
  {"instance":"<shell-instance>","seq":6,"messageId":"<shell-message>",
   "status":"complete","operation":"replace","revision":"2","baseRevision":"1"},
  {"instance":"<claude-instance>","seq":6,"messageId":"<claude-message>",
   "status":"queued","operation":"open","revision":"1","baseRevision":null},
  {"instance":"<claude-instance>","seq":10,"messageId":"<claude-message>",
   "status":"complete","operation":"replace","revision":"2","baseRevision":"1"}
]
```

For each pair, `payload.nodeId` also equals the corresponding message label.
The later replacement is evidence of input delivery through the PTY driver.
It is not an assistant reply. Create can settle before its optional input is
delivered, so create settlement alone is not used for this conclusion.

Claude journal seq 9 separately records `kind: lifecycle`, `source.channel: hook`,
`payload.type: native`, `nativeName: SessionStart`, and
`status: {"state":"known","value":"recorded"}`. The session metadata contains
a transcript path. Its native session ID matched the captured transcript context;
the private path is not reproduced. SessionStart precedes delivered-input seq 10.

## Native Claude assistant reply

This is a separately captured **native transcript excerpt**, not an assistant
message synthesized from the Hub journal or an echoed prompt:

```json
{
  "role": "assistant",
  "text": "PONG",
  "timestamp": "2026-09-13T09:28:43.862Z",
  "model": "claude-haiku-4-5-20251001"
}
```

`native-reply-proof.private.json` contains this exact assistant text.
The captured native transcript has one exact assistant `PONG` text block and
zero `tool_use` blocks. This establishes the bounded reply observed in this
capture; it is not an audit of unrelated native sessions.

The installed Claude PTY driver emits native lifecycle and TTY observations;
it does not wire the transcript mapper into structured assistant Hub-journal
messages. The source distinction matters: queued/delivered input is supported
by the Hub journal, while assistant text is supported by the native transcript
and terminal capture. A Herdr `idle` status alone would not prove the reply.

## Explicit close and final state

Both close commands have journal `kind: lifecycle`, `payload.type: entity`,
and `payload.entityType: command`. Their selected command fields are:

```json
[
  {"instance":"<shell-instance>","seq":10,"commandId":"<shell-close>",
   "operation":"instance.close","state":"settled","resolution":"clear",
   "authority":"node-ledger",
   "settlement":{"state":"known","value":{"outcome":"completed","error":null}}},
  {"instance":"<claude-instance>","seq":16,"commandId":"<claude-close>",
   "operation":"instance.close","state":"settled","resolution":"clear",
   "authority":"node-ledger",
   "settlement":{"state":"known","value":{"outcome":"completed","error":null}}}
]
```

Separate instance entity events provide the closure evidence: shell seq 9 and
Claude seq 14 each record `state: exited`, `reasonCode: explicit-close`, and
an instance entity with `lifecycle: exited` and known `activity: idle`.
Claude seq 15 also records native `carrier_closed` with known status `exited`.
The private query of the Node SQLite PTY-resource ledger returned zero rows
after closure. This records resource-ledger cleanup, not a separate OS process
inventory or proof about unrelated processes.

The captured Hub instance views retained stale activity labels: shell
`unknown`, Claude `working`, although both lifecycles were `exited`.
Stop PASS rests on the completed close commands and explicit-close journal
events above. This record does not claim the Hub activity projection became idle.

At **09:30:20 UTC**, `final-state.private.json` records Hub `/healthz` as
`{"ok":true}`, the Mac host online with `outbound-wss`, the successful real-repo
probe, and both final lifecycles `exited`. The 20 requests recorded in
`api-meta.private.json` all had curl exit 0, HTTP 200, and certificate
verification result 0. These are the captured checks, not continuous monitoring.

## Evidence retention and limits

Private operator artifacts include the restart/workspace results, API metadata,
session summaries, TTY captures, journal snapshots before and after close,
SessionStart metadata, native transcript/reply proof, final-state result, and
PTY-resource query. Only allowlisted excerpts and aggregate results appear here.
No private artifact, credential, cookie value, personal absolute path, hostname,
or address is copied into the repository.

This milestone did not repeat WSS disconnect/replay acceptance, exercise a
browser/PWA or phone, install another Node, or modify the shared gateway.
Earlier replay evidence remains in the historical enrollment record.

Documentation checks: `./scripts/ci/secret-scan.sh` and `git diff --check` passed.
Only evidence and the deployment status were edited; no Rust changes or new
Rust test run were required for this acceptance.
