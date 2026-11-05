# Herdr claim: adopting an external herdr pane (D-062)

Status: **adopted**. Owner request 2026-11-04 ("support claiming an external
herdr, with workspace and tab"); the defaults below were set by the
coordinator and are recorded in [D-062](./decisions.md#d-062).

Until now a Node only drove panes it launched itself, in its own isolated
herdr session (`crates/remuda-node/src/carrier_recovery.rs`: "A Node's Herdr
session is named after its data dir"). A claim adopts a pane a human started
in **another** herdr session on the same host, so it shows up in the ledger,
on `/s/:id` and in the CLI like any other instance, without Remuda ever owning
the process inside it.

## 1. Who may claim

Human operators only, in this first cut. An Agent, a Bot, and a Human device
narrowed to an instance (`x-remuda-instance-id`) all get **403** with a
`reason` and a `next` step on discovery, claim and release.

The pane runs with whatever permission mode and credentials the human gave it
when they started it. Remuda never set them and cannot see them, so letting an
agent claim panes (or drive a claimed one) would widen its authority to
whatever that pane can do. For the same reason the Hub refuses every
`/v1/instances/{id}/commands` call on a claimed instance from a non-Human
caller (403, reason `human-only`).

The rule is structural (rounds 2–3). Every operation that writes to a
claimed pane or ends its claim — `instance.send`, `instance.keys`, `tty.write`
and `tty.resize` (from the commands route **and** the follow socket's TTY
input), `instance.configure`, `instance.cancel`, `instance.respond`,
`instance.close`, `instance.purge`, release, `DELETE /v1/instances/{id}`,
resume and fleet broadcast/run — passes one Hub gate,
`herdr_claims::authorize_pane_write`, with the authenticated, narrowed device.
Only a Human device passes; the gate stamps the payload (`claimWrite`,
`origin: human`). Two backstops refuse an unstamped pane write to a claimed
instance before anything is sent, so a route that forgets the gate fails
closed: `forward_if_online` for every queued command (the row settles
rejected) and `node_ops::call_admitted_frame` (`verify_direct`) for direct
RPCs. A fleet broadcast from a non-Human skips claimed targets with a
per-instance `human-only` result. The Node refuses the same writes to a
claimed pane unless the frame's Hub-stamped `origin` is `human` — a frame with
**no** origin is refused too, so an omission cannot downgrade the check. A
source-scan test (`herdr_claims::gate_scan`) fails if a Hub function sends a
pane-write Node RPC without the gate: it reads every `.call(` and `.notify(`
on any Node transport whatever the receiver is named, their UFCS forms
(`ConnectedNodes::call(&state.nodes, …)`), every call of a method-forwarding
helper (`call_node`, `call_node_pinned`, `call_admitted`,
`call_admitted_frame`, the host-files and SCM proxies, the workspace mutation),
and treats any non-literal method — a variable, or a constant not on an
explicit read-only allowlist whose values the test checks — as possibly a pane
write; RED fixtures prove each shape is caught. A claimed pane cannot be resumed at all (400).

**Hub-origin input never reaches a claimed pane** (round 3). Remuda did not
start the process in it, so Hub voice typed into it is unsafe. The gate above
already refuses an unstamped `origin: hub` write; beyond that every
Hub-origin source refuses a claimed target when its row would be created, so
nothing is formed, refused and formed again:

| Source | Refusal |
| --- | --- |
| inbox fact batches | a claimed lineage never receives facts, whatever its settings; `PATCH /v1/lineages/{id}/inbox` with `optIn: true` is 409 `claimed-pane`; the delivery gate treats a claimed chapter as gone, so a left-over fact falls back to the owner push |
| wake-ups (persistent and fresh) | `POST /v1/lineages/{id}/wake-ups` is 409 `claimed-pane`; a fresh run never creates a child on a claimed chapter's behalf |
| usage-resume continuations | `PATCH /v1/instances/{id}` with `autoContinue: true` is 409 `claimed-pane`; no wait row is recorded; a left-over one ends `claimed-pane` |
| restart notices | a claimed row never gets continuity (no grants, no restart policy) and the C1 supervisor skips `herdr-claimed` |
| owner-notify | a push to the owner; it never writes to any instance |

## 2. Identity

A claim names:

| Field | Meaning |
| --- | --- |
| host | The Remuda host whose Node can reach the herdr socket. Claims are local: the Node and the herdr server share one machine. |
| herdr session | A session **name** or an absolute **API socket path**. `default` is the user's default socket (`~/.config/herdr/herdr.sock`); any other name is `~/.config/herdr/sessions/<name>/herdr.sock`. Both are resolved against the herdr config dir of the user the Node runs as. |
| workspace id | herdr workspace id, e.g. `w2`. |
| tab id | herdr tab id, e.g. `w2:t19`. Must belong to the workspace. |
| pane id | Optional herdr pane id, e.g. `w2:p5K`. Must belong to the tab. Without it the tab must have **exactly one** pane; otherwise the claim is refused with 400 and the error lists the tab's pane ids. |

The Node validates the chain against one `session.snapshot`: the workspace
exists, the tab is in that workspace, the pane is in that tab. Any mismatch is
400 (`herdr-claim-invalid`).

At claim time the Node records, on the instance (`herdrClaim`):

- `session` (as the operator wrote it) and the resolved `socketPath`;
- `workspaceId`, `tabId`, `paneId`;
- `agent`: herdr's detected agent kind for the pane, if any;
- `process`: the claimed process from `pane.process_info` — its `pid`, its
  `command` (the executable's base name only; full argv is never read into
  Remuda, it may carry secrets) and, on Linux, the kernel start time of that
  pid (`/proc/<pid>/stat` field 22), so a recycled pid is never mistaken for
  the original. Which process that is depends on the kind (round 2):
  - an **agent** kind claims the pane's **foreground process-group leader**,
    which must not be the shell. While herdr reports only the shell in the
    foreground the pane runs no agent yet, and the claim is refused with 409
    `not-ready` (`herdr-claim-not-ready`); it never falls back to the shell;
  - **`--kind shell`** (`terminal`) claims the pane's **shell itself**. Any
    foreground job the human starts in it is fine; the shell being replaced
    (another pid, or the same pid with another start time) ends the claim.

The claimed instance files under the registered Remuda workspace whose root
contains the pane's cwd (`herdr.resolve` reports it as `remudaWorkspaceId`, and
the Hub indexes the row with it), so it appears under that project; a pane
outside every registered workspace gets no workspace.

The Node's **own** herdr session is never claimable (400, `own-session`) and
is not listed by discovery: its panes already are Remuda instances.

## 3. Kind, driver and permission mode

- `--kind claude|codex|grok|agy|shell` (`shell` is the `terminal` kind). Without
  `--kind` the kind is herdr's agent kind for the pane when it reports
  `claude`, `codex`, `grok` or `agy`; `generic` for any other agent herdr
  reports; `terminal` when herdr reports no agent.
- The driver is the new `DriverKind` **`herdr-claimed`**. It is registered
  wherever driver kinds are enumerated (protocol enum, generated types, the
  capability matrix, the web driver tables). `instance.create` refuses it:
  a claimed instance only comes from `instance.claim`.
- `permissionMode` is recorded as **`"external"`**: unknown, owned by the
  human who started the pane. Remuda never invents a mode for it and never
  sends a permission-mode switch to it.

## 4. Ownership: a claim is not ownership

Remuda does **not** own the claimed process.

- **Release** — `remuda instance release <id>`, `POST /v1/instances/{id}/release`,
  or an ordinary close on a claimed instance — **detaches only**. It stops the
  Node's watcher and TTY bridge and ends the instance with reason `released`.
  It never closes the pane, never kills or signals its process, and never stops
  the herdr server. The detach is an idempotent `instance.close` command:
  forwarded at once when the host is online, otherwise left queued and
  forwarded on the host's next `node.hello` and by the Hub's 1 s tick (each
  command is sent at most once). Release is idempotent, and what decides is
  the detach's **delivery**, read from its command rows, never the release
  audit. Delivered means the Node **answered**: it accepted or settled the
  close without refusing it. Releasing an instance that already ended (or
  whose host was lost) answers 200 with `released: false` when a detach was
  delivered or is still queued (it is driven again now). A close that was
  forwarded but never answered is *unconfirmed* — its frame may have died with
  the link (`node rpc dropped`), and nothing journals a frame the Node never
  saw — so the release sends it again (`released: true`), and so do the hello
  and tick passes once the host is on a newer link. Sending again means a
  fresh idempotent close carrying the original gate stamp, not a resend of the
  old command id, so the durable forward mark keeps meaning "never resend"; a
  Node without the claim answers it harmlessly. With no detach at all, or only
  ones the Node refused, the release queues one and answers `released: true`.
  A claim whose commit reply is lost (the Node may hold it) is failed on the
  Hub and its detach is queued at once, so no claim is left that the Hub
  cannot release.
- The only things that act on the pane are `instance send` (the prompt
  input), `instance keys` and interrupt, and TTY control/observe from the PWA.
- A Node shutdown is not a release: the claimed rows stay live and the next
  Node start re-attaches them (§6).
- None of the Node's carrier machinery applies to a claimed pane: it is never
  recorded as a `pty_resources` row, so the orphan sweep, the stop ladder,
  `reclaim_instance_carriers` and carrier recovery cannot reach it.

## 5. Driving a claimed pane

| Operation | herdr call | Notes |
| --- | --- | --- |
| `instance send` | `agent.prompt` (target = pane id) when herdr reports an agent in the pane; otherwise `pane.send_text(text)` then `pane.send_keys(["enter"])` | Queued like any PTY prompt: it waits until herdr reports the agent idle. |
| `instance keys` | `pane.send_keys` | |
| interrupt (cancel) | `pane.send_keys(["esc"])` for agent kinds, `["ctrl+c"]` for `terminal` | |
| TTY observe/control in the PWA | `herdr terminal session control <pane>` with `HERDR_SOCKET_PATH=<socketPath>`, never `--takeover` | Same bridge as the Node's own panes. It opens at the pane's current size (herdr's viewport rows, the widest visible line), so claiming never reflows the human's terminal; a resize from the PWA resizes the pane the same way another herdr client would. |
| screen snapshot (`tty.screen`) | `pane.read` (visible viewport, text) | Used by the stall fallback (herdr-claimed is a screen carrier) and by the 结构 (screen) view on `/s/:id`, which re-reads it every 2 s while shown; the journal never stores it. |
| activity | `session.snapshot` agent status | herdr `working` → working, `blocked` → waiting on input, `idle`/`done` → idle. |

There is **no agent-output transcript** in this cut. The journal holds
lifecycle, agent-status and contact events, plus a user-message row for each
`instance send` (the PTY prompt queue records what was typed, as for any PTY
instance); nothing the agent prints is journaled. The session page shows the
live screen and the TTY. Binding a claimed claude pane to its native session
JSONL is out of scope (§10).

All calls go to the socket recorded in the claim, and every one names the
claimed pane id: nothing reaches another pane, tab or workspace.

**Every write re-checks the claimed process** (round 3). Probes run every
2 s, so between two of them a replaced process could otherwise receive input.
Before `instance send`, `instance keys`, interrupt and **every** raw TTY input
write, the Node re-reads `pane.process_info` and the kernel start time and
compares them with the recorded identity (§2). A mismatch ends the claim
(`herdr-process-gone`, §6) and the write is dropped; an unknown answer (below)
refuses the write. The TTY bridge carries this check itself (a write guard on
the bridge), and the claimed driver never hands out an unguarded bridge.

## 6. Liveness, end and restart

The Node polls each claimed pane every 2 s (`session.snapshot` plus
`pane.process_info`):

- **Pane gone** — the pane is no longer in the session snapshot. This is
  process-end evidence (OA6): the instance ends `exited` with reason code
  `herdr-pane-gone`, and the evidence names the session and pane.
- **Process gone** — the process now in the claimed role (the foreground
  leader for an agent claim, the shell for a shell claim) is not the recorded
  one: a different pid, no such process, or the same pid with a different
  kernel start time (a reused pid). For an agent claim a changed foreground
  leader — the agent exited and the pane is back at its shell, or another
  program now runs there — is process-gone, so send, keys and the TTY never
  drive whatever replaced the claimed agent. The instance ends `exited` with
  reason code `herdr-process-gone` and evidence naming both pids.
- **Process unknown** — the session snapshot answered but `pane.process_info`
  failed or reported no process info. Nothing can be said about the claimed
  process, so it is never read as alive: every write is refused, and after 3
  such probes in a row the instance ends `exited` with reason code
  `herdr-process-unknown`. A claim row that recorded no process at all (made
  before round 2) is ended the same way at re-attach and never re-attached:
  there is nothing to compare with.
- **Server unreachable** — the socket refuses, times out or answers
  `server_unavailable`. This is contact loss, never process end: the instance
  stays live, the journal gets a `herdr_claim_contact` = `lost` diagnostic, the
  Hub shows `herdrClaim.contact = "lost"` (「外部 herdr 失联」), and it flips back
  to `ok` when the server answers again. Nothing is failed by guess.

In every case the pane is never touched.

**Node restart.** A graceful shutdown leaves claimed rows live; a crash leaves
them as they were. At start, before any command is served, the Node checks
each live claim: pane present and process alive → re-attach (watcher, TTY
bridge, command queue); pane gone or process gone, or no recorded process →
end with evidence as above; process unknown → re-attach with every write
refused, the first of the 3 probes counted; server unreachable → re-attach in
the contact-lost state. Either way the
re-attach journals the contact it observes (`ok` or `lost`), so a `lost` that
survived the restart flips back to `ok` once the server answers. The Hub sees
the re-attached instances as live in the hello inventory.

## 7. Wire

Node RPCs (Hub→Node, additive, same family as `host.dirs.list`):

| Method | Params | Result |
| --- | --- | --- |
| `herdr.discover` | `{session?}` | `{sessions:[{session, socketPath, reachable, error?, workspaces:[{workspaceId, label, tabs:[{tabId, label, panes:[{paneId, agent?, agentStatus, process?, claimedBy?}]}]}]}]}` |
| `herdr.resolve` | `{session, workspaceId, tabId, paneId?, kind?}` | `{claim: HerdrClaim, kind, agentStatus, remudaWorkspaceId?, cwd?}` — read-only validation; `remudaWorkspaceId` is the registered workspace holding the pane's cwd |
| `instance.claim` | `{instanceId, session, workspaceId, tabId, paneId, kind}` | `{instance}` — idempotent per `instanceId` |

Refusals carry a stable prefix in the Node's error text, which the Hub maps:
`herdr-claim-invalid` and `herdr-claim-ambiguous` → 400, `herdr-claim-own-session`
→ 400, `herdr-claim-conflict` → 409 (`already-claimed`),
`herdr-claim-not-ready` → 409 (`not-ready`: an agent claim whose pane has no
foreground agent yet), `herdr-unreachable` → 400.

Hub routes (Human only):

- `GET /v1/hosts/{id}/herdr?session=S` — discovery.
- `POST /v1/instances/claim` — `{hostId, herdrSession, workspaceId, tabId, paneId?, kind?, name?, title?, projectId?}` → `{instance, claim}`. A live claim on the same (host, socket, pane) is **409** with the existing instance id.
- `POST /v1/instances/{id}/release` → `{instance, released}`.

CLI:

```text
remuda host herdr ls --host H [--session S] [--json]
remuda instance claim --host H --herdr-session S --workspace W --tab T [--pane P] [--kind K] [--name N] [--project P]
remuda instance release <id>
```

The instance record gains `herdrClaim` (the §2 fields plus `contact`), `driver:
"herdr-claimed"`, `launchedBy: "user"` and `permissionMode: "external"`.

## 8. PWA

A claimed instance shows in the ledger and on `/s/:id` like any instance, with a
small 「外部 herdr」 badge carrying `session · workspace · tab` (and 失联 while
contact is lost). Its TTY works like any herdr-carried terminal. The close
action is labelled 「释放」 and its confirm says the pane keeps running. There
is no claim UI in this cut; the CLI is the only way to claim.

## 9. Audit and logs

The Hub writes an audit row for every claim (`instance.claim`) and release
(`instance.release`, including a close on a claimed instance): the device
that did it, host, session, socket, workspace, tab, pane and kind; a claim also
records the pid and command name, and a release names its detach command
(`commandId`), whose state says whether the Node got it. Logs and audit rows never carry pane
content. Discovery never calls `pane.read` or `agent.read`. The Node reads a
claimed pane's visible screen in three places, never logging the text: once at
claim and at each re-attach, only to size the TTY bridge (the widest visible
line); for `tty.screen` (the stall fallback); and for the `/s/:id` screen view.

## 10. Threat model

- **Same-uid trust boundary.** herdr's API socket is unauthenticated ("can
  connect" is "can control"); its `0600` mode is the boundary
  ([security-review-2.md](./security-review-2.md) G1). The Node runs as the
  same user, so it can already reach every session that user owns. A claim
  adds no new reach; it routes an operator's existing reach through the Hub,
  which is why only an authenticated Human may use it.
- **Authority of the pane.** The pane's agent may be in a permissive mode with
  the human's own credentials. Remuda does not try to see or change that; it
  confines who can drive the pane (Human only) and records `permissionMode:
  "external"` so nothing downstream assumes a Remuda-set posture.
- **No destructive reach.** Release, close, shutdown, orphan sweep and carrier
  recovery never close a claimed pane or signal its process (§4), and tests
  prove the fake herdr records no close or kill.
- **Confusion with a reused id.** Identity is the socket plus the pane id,
  re-validated against the recorded workspace and tab on every re-attach, and
  the process by pid plus kernel start time; a pane id herdr reused after its
  own restart in another tab is not re-attached.
- **Content exposure.** Discovery is metadata only; full argv is never
  collected; logs never carry pane content.

## 11. Out of scope

- Agent-initiated claims (and Agent/Bot commands to claimed instances).
- Binding a claimed claude pane to its native session JSONL for a structured
  transcript.
- Claiming panes on a remote herdr through `--remote` / `--machine`.
- Moving a claimed pane into the Node's own session.
- A claim UI in the PWA.
