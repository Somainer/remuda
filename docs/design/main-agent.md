# Main agent: a resident Remuda instance on the always-on Hub

> Status: design v3, 2026-10-05. Decision record: ADR D-057 in [decisions.md](./decisions.md). This document lands before any Phase 1 code (§15). Owner decisions D1–D4 and the owner amendments OA1–OA6 (§0) are binding. Where this document and an older design disagree, the owner decisions win.

## 0 Binding owner decisions (2026-10-05)

- **D1 Placement.** The Hub runs on an always-on machine. It is the existing [deploy/intranet](../../deploy/intranet/README.md) Hub, behind the existing Caddy and intranet HTTPS, upgraded from `9dd7ec7` to current main. The owner's laptop is just another Node, reachable by the Hub like any other host. The devbox Node attaches too. The owner or ops runs every deploy, Caddy and Compose step. Workers never run any of them, and never run anything under `deploy/`.
- **D2 Data.** Start from the intranet Hub's existing data. The laptop demo Hub becomes history and is not migrated. Nodes re-attach; the laptop Node keeps its enrolled identity (OA5).
- **D3 No special permission design.** The main agent relies on each agent loop's own permission controls (Claude Code permission modes, Codex approval policy, and so on). There is no per-harness posture map, no OS-user isolation requirement and no land-grant machinery; at most they are later options (§14.4). The safety properties Remuda already has stay:
  - approvals reserved for humans stay human (D-017, D-051(a));
  - delegation never widens authority (made precise for permission modes by OA1);
  - keys cannot stand in for an approval.

  Like any agent, the main agent uses the existing gate and land verbs within its existing scope.
- **D4 Cutover.** One cutover from the human-run coordinator, after Phase 1's exit criteria pass (§16).

Owner amendments (2026-10-05, made after design v3). They override any text below that disagrees. They are numbered OA1–OA6 because A1–A8 already name the failures in §1.
- **OA1 Never beyond the creator.** A child created by an Agent may use a mode that switches its harness's own permission control off (Claude `bypassPermissions`/`dontAsk`, Codex `never`, Grok `always-approve`, Agy `always-proceed`) only if its Agent creator itself runs with that control off. Otherwise any mode that keeps harness control on is allowed. When the mode is omitted, the child inherits the creator's own mode; the framework picks no new mode. The Hub's `restrict_permission` and the Node materializer move together to this rule. It is the one place Phase 1 changes the existing Agent-origin permission rules (§4.2 item 3).
- **OA2 The seat's own configuration.** The main agent's permission mode and its grants, `land` included, are configuration chosen when it is created, like any instance's. This design states no default, recommendation or policy for them (§3.2, §3.3).
- **OA3 Delegated decisions.** The D-051 switch is on for the self-development project. D-051(b) is amended: questions and plan reviews route to the Agent parent even when the parent or the child runs bypass. Approvals stay human-only (D-017, D-051(a)). OA1 is what keeps this delegation from widening authority (§4.2 item 6, §8.1).
- **OA4 Reachability.** The phone has an approved route to the intranet Hub off the office network, and the Hub host can reach Web Push services (§2).
- **OA5 Defaults taken.**
  - A successor's launch uses the seating Human authorization as a standing grant (§6.3.1).
  - The suggested restart cap is 3 per hour (§3.2, §6.1).
  - The laptop Node keeps its enrolled identity (§2).
  - Cross-host Agent dispatch keeps the D-017 one-shot approval in Phase 1 (§4.2).
- **OA6 Failed is not the process exiting.** Only process-end evidence makes an instance terminal: an exit or exit code, the PTY or tty gone, a launch that never started, or the Node reporting the instance gone. Every other failure is retryable in place, is never a restart cause and sends no end push. Activity changes only on root-turn end evidence: a root-session turn failure ends the root turn with outcome `failed`, so activity goes idle and the owner can retry. A main-session StopFailure without an `agentId` counts as one only after protocol.md §5.6 settles it; until then the turn stays unknown, activity is unchanged and held prompts do not flush. Subagent, workflow, configure and diagnostic failures leave activity unchanged and are recorded in their own scope. C1 restarts only on process loss or start failure (§6.2, §6.3, §8.5).

Owner principles applied throughout:
- the harness supplies capabilities and the agent decides;
- no new verb, mode or policy gate where an existing capability works;
- records are truthful;
- public-repo hygiene.

## 1 Goal and the failures it removes

The owner chats with one main agent from the phone or the laptop. It keeps working when the laptop is closed. It operates the machines and development environments Remuda knows, through Remuda's own verbs.

Today a person runs that role as a Claude Code session on the laptop, with a Human device token. Its recurring failures are the acceptance tests:

| # | Failure today | Answer | Phase |
|---|---|---|---|
| A1 | Laptop sleep froze reviews and gate polling | Hub on the intranet host; seat, workers, gate lanes and project home host on the always-on devbox Node | 1 |
| A2 | A reboot wiped /tmp and killed every watcher | Seat workspace on persistent disk; restart after process loss (§6); Hub state in SQLite | 1 (watchers: 2) |
| A3 | A long gate inside one ssh session died with sleep | Gate jobs are Hub jobs on Node lanes (D-034); land from the always-on home host | 1 |
| A4 | Watchers expire; cron is session-only; state is rebuilt from files | Phase 1: the always-on seat polls Hub reads. Phase 2: Hub-delivered facts and durable wake-ups | 2 |
| A5 | Workers stalled on 429 or context limits sat idle for days | Phase 1: the seat's own turn error no longer marks it failed. Phase 2: facts to the parent | 2 |
| A6 | Append-only markdown memory; compaction loses detail | Journals are the verbatim archive. Phase 3: Hub-held documents and agent-opened chapters | 3 |
| A7 | Outward actions need judgement and sometimes the owner | Existing grants and scope; approvals stay human; fencing (§7); honest push (§8) | 1 |
| A8 | Chat only from a terminal | Main in the PWA on every device, reply-ready push, Pause/Resume | 1 (terminal client: 2) |

## 2 Placement (D1, D2)

- **Hub.** The deploy/intranet Compose service (`restart: unless-stopped`), behind the existing Caddy. The owner or ops upgrades it in place, Hub image only, following the deploy-runbook section «升级到当前 main 并接入 Node». It keeps its data directory, origin, passkeys and paired devices. This is not a machine move, so the bootstrap rotation in hub-topology §7 is not required. Rotate only if the access code was exposed.
- **Nodes.** The devbox Node is always on. It runs the seat, its workers, the project's gate lanes and the project home host (land pushes). The laptop is an ordinary Node for the owner's own sessions. It keeps its enrolled identity and is not re-enrolled (OA5). When it sleeps, only work placed on it pauses, and the Hub shows it offline. Both are D-019 daemons that dial the Hub outbound over WSS. Nothing dials a Node.
- **Clients.** The phone and the laptop browser reach the Hub only over approved routes (D-031: no tunnels, no public front door). The phone has an approved route to the intranet Hub off the office network too, and the Hub host can reach Web Push services (OA4).
- **Not used.** The intranet host runs only the Hub; it has no agent CLIs. The laptop demo Hub and its data are history (D2).

## 3 The seat

### 3.1 What it is

The main agent is an ordinary instance: `kind: claude`, `driver: claude-sdk` (D-037). It runs on the devbox Node in a persistent workspace, never /tmp, and the owner creates it from a Human device. claude-sdk gives exact turn boundaries. It also turns the agent's questions and permission prompts into Interactions the phone can answer.

There is no new agent kind and no seat table. "Main" is the lineage (§5) whose live chapter holds the `address-owner` grant. The Hub already allows at most one active holder (`enforce_grant_uniqueness`), and the PWA resolves Main from the instance list. A paused lineage keeps its chapters and can be resumed. A new seat can be created while it is paused.

A person can still take the seat (coordinator-hierarchy §2.0). Continuity then does nothing.

### 3.2 Seating

The owner creates the seat with `remuda instance create` and the flags that the Phase 1 CLI task adds. The flags expose fields `CreateInstanceBody` already accepts, plus `restart`:

~~~text
remuda instance create --host <devbox-host-id> --workspace-id <workspace-id> \
  --kind claude --driver claude-sdk --name main --title Main \
  --grant address-owner [--grant <grant> …] \
  --scope-project <project-id> --scope-host <devbox-host-id> \
  --permission-mode <mode> --model <model> \
  --restart process-loss:3 --prompt-file <launch brief>
~~~

Main is the `address-owner` holder (§3.1). Its other grants (`land` included) and its permission mode are configuration chosen at seating, like any instance's (OA2); this design states no default, recommendation or policy for them. The verbs in §3.4 need whatever grants their existing handlers require: worker verbs and dispatch need `dispatch`, and land needs `land`. Grants are explicit, and grants are what enforcement reads; the role is display-only. Scope is the existing delegation box of projects, hosts and workspaces. `--restart process-loss:3` sets the `restart` property (§6.1); 3 per hour is the suggested cap (OA5).

Seat only after every Node has been upgraded to the Phase 1 commit (§6.4, §7.6), and after the devbox host record advertises herdr (§4.2).

### 3.3 Its own permission (D3, OA2)

The seat's permission mode is configuration chosen at seating, like any instance's, and its harness enforces it (OA2). These consequences follow from existing rules and the amendments:

- In a mode that asks (`manual`, `acceptEdits`, `auto`, …), every tool call the harness decides to ask about becomes an approval Interaction that pages the owner. That includes a `remuda dispatch` run through Bash. Agents never answer approvals (D-051(a)).
- The seat's children never run beyond it (OA1, §4.2 item 3). A child may switch its harness's permission control off only if the seat runs with it off, and a child created without a mode inherits the seat's mode.
- Worker questions and plan reviews route to the seat under D-051 whichever side runs bypass (OA3, §4.2 item 6). Approvals still page the owner.
- After an automatic restart, the successor runs in the same mode (§6.3.1).

### 3.4 What it acts with

No new verbs. The Node injects `REMUDA_TOKEN`, `REMUDA_HUB` and `REMUDA_INSTANCE_ID` into the seat's process (`crates/remuda-driver/src/agent_mcp.rs`). The `remuda` CLI on the devbox therefore acts with the seat's Agent credential. Available verbs:

- `task`, `dispatch`, `watch`;
- `worker`: nudge, answer, switch-model, resume, replace, stop. Resume and replace go through the scoped respawn helper (§4.2 item 5);
- `retire`;
- `gate`: verify, plus land when it holds `land`;
- `hostcap`;
- `instance`: read, send, stop;
- interactions list and answer (D-051; on for the self-development project, OA3).

The in-session MCP server keeps today's tool set; MCP mirrors of the coordinator verbs are a later option. The skill update (§15) describes these capabilities, not a procedure.

### 3.5 Watching in Phase 1

Until Phase 2, the seat watches by bounded polling of Hub reads from inside its own turns: the worker roster and observe, worker journals for DONE/BLOCKED, and gate jobs. If the owner sends a message while the seat is in a long turn, the carrier's existing new-turn/steer/queue behaviour delivers it (D-055 modes). The exit evidence records the observed delay; this document does not promise one. The agent may use its harness's own scheduling tools where they work, but they do not survive a restart. Phase 2 replaces polling with facts and durable wake-ups.

### 3.6 Memory in Phase 1

- Work memory is the Hub: tasks (D-050), the roster, gate jobs and interactions.
- The verbatim archive is the journal of every chapter and every worker. After a restart, the agent reads predecessor chapters with `remuda instance read`.
- Identity and memory documents are plain files in the seat's persistent workspace, named in its launch brief. They hold no secrets. Hub-held, revisioned, owner-editable documents are Phase 3 (§12).

## 4 Authority (D3)

### 4.1 Existing properties kept

| Property | Where it lives on origin/main | Phase 1 |
|---|---|---|
| Approvals reserved for humans are answered only by humans | `authorize_agent_answer` and `delegated_visible_items` (interactions.rs); one-shot approvals (agent_approvals.rs) | Unchanged |
| Delegation never widens authority: an Agent's child never runs with the harness's own permission control switched off unless its Agent creator does | Today stricter than that: the Node materializer's `permission_plan` refuses Claude `bypassPermissions`/`dontAsk`, Codex `never`, Grok `always-approve` and Agy `always-proceed` for Bot and Agent launch origins, and the Hub's `restrict_permission` allows Agents only `manual` or `plan` | The rule changes (OA1): Hub and Node move together to "never beyond the creator" (§4.2 item 3). Bot origin is unchanged |
| Scope narrows, grants never grow, depth and fan-out are bounded | `validate_child_delegation` (store.rs) | Read across the lineage (§5) |
| Keys cannot stand in for an approval | `authorize_command` always requires the one-shot approval for Agent `tty.write` and `instance.keys` | Extended to `worker answer` keys, which Agents can now reach |
| Cross-host and shell-driver creates by an Agent need a one-shot human approval | `prepare_create`, `shell_driver` | Unchanged, cross-host dispatch included (OA5); for dispatch, decided before any allocation (§4.2) |
| Agent callers act only on what they own | `owns()`, `authorize_command` | Lineage-aware (§5); also required for worker mutations |
| One active `address-owner` holder; one `dispatch` holder per scoped project | `enforce_grant_uniqueness` | Fenced chapters do not count (§7) |
| No computer-use on dispatch | `dispatch_core` (D-045) | Unchanged |
| The D-051 switch is the project-keyed environment list | interactions.rs | Unchanged; it is on for the self-development project (OA3) |

### 4.2 The changes Phase 1 needs, and why

On origin/main, Agent-origin dispatch cannot succeed:
- `restrict_agent_routes` does not admit `/v1/workers/*`.
- Even if it did, `worker_launch_spec` writes `permissionMode: bypassPermissions` for every harness. `restrict_permission` (Hub) and `permission_plan` (Node) both refuse that for Agent origin.
- `prepare_create` asks for its approval only after the worktree is provisioned, and the spec it asks about gets a new name and port block on every retry.
- Worker respawn goes through the operator-only resume route.

Phase 1 changes exactly these:

1. **Admit the worker routes for Agent origin.** Handlers keep `require_grant(Dispatch)` and project scope. Mutating worker verbs from Agent origin (nudge, answer, switch-model, resume, replace, stop, retire, state, brief) also require the worker's instance to be in the caller's lineage subtree (§5). Reads (list, get, observe) stay scope-wide. This is "delegation never widens": `resolve_worker` checks only project scope, so without this rule an Agent could stop or answer workers the owner dispatched by hand in the same project.
2. **Dispatch takes an optional `permissionMode`.** Human-origin dispatch keeps today's default byte for byte. An Agent-origin dispatch without a mode inherits the creator's own mode, and the framework picks no new mode (OA1). A requested mode is admitted by item 3. Values come from the worker harness's own vocabulary. Modes that keep that harness's permission control on include, for example:
   - Claude: `manual`, `auto`, `acceptEdits`, `plan`;
   - Codex: `untrusted`, `on-request`;
   - Grok: `native-prompt`, `auto`;
   - Agy: `native`, `accept-edits`, `plan`.

   The requested and effective values are recorded verbatim (D-035).
3. **Never beyond the creator (OA1), one rule in both places.** Today `restrict_permission` (Hub) allows Agents only `manual` or `plan`, and `permission_plan` (Node) refuses the control-off modes for Agent origin. Both move together to one rule for every Agent create, dispatch and respawn:
   - A child may use a mode that switches its harness's own permission control off (Claude `bypassPermissions`/`dontAsk`, Codex `never`, Grok `always-approve`, Agy `always-proceed`) only if its Agent creator itself runs with that control off. Otherwise any mode that keeps the harness's control on is allowed.
   - When the mode is omitted, the child inherits the creator's own mode. The framework picks no new mode. If the creator's mode is not a value of the child harness's vocabulary, the create is refused with a reason, and the caller names a mode.
   - The creator's mode is the creator chapter's effective mode as recorded on its instance (D-035). The Hub stamps it on the forwarded create as `creatorPermissionMode`, never taken from the body. The Node applies the same rule from that stamp in every place that applies the Agent-origin rule today: the materializer's `permission_plan`, the generic-pty Agent downgrade and the preset bypass flags (`merge_yolo_argv`). A preset's yolo argv is merged only when the child's resolved mode is exactly the control-off mode that argv implements and the Agent creator runs with control off (protocol.md §4.3); an explicit control-on mode never gets it.
   - Bot origin is unchanged: the control-off modes stay refused (D-011).

   This is the one place Phase 1 changes the existing Agent-origin permission rules. Within the modes it admits, the agent loop's own permission control governs a worker's actions, as D3 directs.
4. **The D-017 approval for an Agent dispatch is decided before any allocation.** The approval is required when the resolved host is not the caller's host, or when the resolved carrier is a shell driver. The Hub then asks for the one-shot approval on a request derived from the dispatch body: project, resolved host, harness, carrier or driver, model, permission mode, brief digest, task and name. It asks before it allocates a name, port block or worktree. An approved retry with the same body passes; a changed body does not. Nothing is provisioned or leaked while the approval is pending.
5. **Worker respawn without the operator-only route.** Today `worker resume` and `replace` call `respawn_instance`, which calls the public `POST /v1/instances/{id}/resume` with the caller's headers. That route is operator-only (D-026), so an Agent caller is refused whenever the worker has a native session id. Its D-026 edge would also make the resumed worker a child of the old worker instance, two hops from the coordinator. Phase 1 adds an internal respawn helper:
   - It is authorized by the `dispatch` grant, project scope and lineage ownership (§5).
   - It keeps the worker's carrier and its requested and effective permission mode.
   - The new instance's `parentInstanceId` is the old worker instance's parent, that is, the coordinator chapter that created it. `resumedFrom` points at the old instance.
   - It applies the same permission and approval rules as any Agent create, including the D-017 approval for a shell-driver carrier.
   - When there is no transcript, it falls back to a fresh launch in the same worktree.

   The public resume route stays operator-only and unchanged. A Human resume of a worker with no Agent parent is byte-identical to today.
6. **The D-051(b) amendment (OA3).** Today `delegated_visible_items` and `authorize_agent_answer` (interactions.rs) refuse a delegated decision whenever the caller or the target runs in bypass. From Phase 1, questions and plan reviews route to the Agent parent even when the parent or the child runs bypass. Approvals stay human-only (D-017, D-051(a)), and the plan-review self-exclusion and the one-hop edge are unchanged. OA1 keeps this from widening authority: a child runs bypass only under a creator that runs bypass. The D-051 switch is on for the self-development project.

**Which dispatches need no Remuda approval.** A seat on the devbox can dispatch same-host workers without a Remuda prompt when it names an interactive carrier that is not a shell driver, on a host whose record advertises herdr:
- `--carrier herdr` for Claude (`claude-pty`);
- a codex or grok harness (always `generic-pty`, which is herdr-backed).

A Claude dispatch without `--carrier` prefers the Node's native `shell-pty` whenever the Node reports it launchable (`select_carrier`). `shell-pty` is a shell driver (`shell_driver`), so that dispatch asks the owner once, as D-017 does today. Telling an agent-target `shell-pty` apart from a raw shell is a later option. Cross-host dispatch, for example to the laptop, asks once per dispatch; Phase 1 keeps that approval (OA5).

### 4.3 Known limitations (stated, not designed away)

- No OS isolation (D3). The seat's harness has a shell on the devbox and can read whatever its OS user can read there, including the Node's own configuration and any local git credentials. Hub-side checks bound Hub-mediated actions only.
- Remuda compares a child's permission mode with its creator's on one axis only: whether the harness's own control is switched off (OA1). It does not rank modes that keep control on. The creator's own harness control gates the create call. Once launched, the child's own harness control governs it.
- The D-051 switch is project-wide. It also reaches agent trees the owner launches personally in that project (existing behaviour). It is on for the self-development project (OA3).

## 5 Lineage and chapters

- A **lineage** is one agent across process lifetimes. Each process is a **chapter**: an instance with these fields:
  - `lineageId`, the first chapter's id;
  - `generation`, counting 1, 2, …;
  - `resumedFrom`, its predecessor;
  - `chapterCause`.
- **A continuation edge is not a delegation edge.** A new chapter's `parentInstanceId` is its predecessor's parent (the human root, for a seat), so depth does not grow with restarts. A worker's `parentInstanceId` stays the chapter that created it, so the audit keeps showing who created what.
- **A worker respawn is not a new delegation edge either.** A respawned worker keeps its creator as parent and points at its previous instance with `resumedFrom` (§4.2 item 5).
- **One store helper resolves lineages, and every parent-edge rule reads it:**
  - `owns()`: a caller owns a target that is in its own lineage, or whose parent is in its lineage;
  - the D-051 one-hop edge;
  - worker ownership;
  - fan-out in `validate_child_delegation`, which counts the active children of every chapter.

  A later chapter therefore keeps reading, controlling and answering what earlier chapters created, and it cannot exceed fan-out by restarting.
- **Continuity instances** hold any grant or carry a `restart` property. Lineage semantics apply to them: continuation resume, and fencing on close. Plain sessions keep D-026 behaviour.
- **Resuming a claude-sdk parent relaunches claude-sdk with `--resume`.** Today `ResumeMode::driver` maps it to claude-print, which ends after one response (D-035). This carrier fix reaches every claude-sdk session.
- A continuation resume copies the predecessor's role, scope, grants, task and `restart`. It happens only inside the fence transaction (§7.2).

## 6 Continuity (C1)

### 6.1 The property

`restart: {onProcessLoss: true, maxPerHour: n}` is set at create time. The suggested `maxPerHour` is 3 (OA5). In Phase 1 only a Human creator may set it; an Agent or Bot gets 403. Letting creators set it on children, bounded by their own, is a later option. The property is copied to every chapter.

### 6.2 Lineage states

Stored states:
- `starting`: a chapter exists and has not reported running;
- `running`;
- `paused{by, at}`. `paused.by` is one of: a Human device, the chapter itself, an Agent ancestor, `restart-cap`, or `process-exit`.

Derived state: `host-offline` means the current chapter's host has no live link. The UI shows "restarting" for `starting` after a restart decision.

**Failed is not the process exiting (owner rule OA6).** Only process-end evidence makes an instance terminal:
- the process exited (an exit, or an exit code);
- its PTY or tty is gone;
- its launch never started;
- the Node reports the instance gone.

No other failure is an end. Each is retryable in place, is never a C1 restart cause, and sends no end push (§8.5). The rule holds for every instance; the `ma-sdk-state` task applies it to the claude-sdk projection (§15). C1 acts only on process-end evidence: it restarts on process loss or start failure (§6.3).

What a non-terminal failure does to `activity` depends on its scope:
- **Root-session turn failure.** Settled evidence that the root turn itself ended in failure: the root turn's matching result reporting an API error such as 429 (protocol.md §5.7), or a main-session StopFailure without an `agentId` once protocol.md §5.6 has settled it from an exact native error under a version rule. It ends the root turn with outcome `failed`. Activity goes idle with a turn-error marker, never `lifecycle=failed`; the composer can retry, and held prompts may flush, as after any root-turn end.
- **Unsettled root StopFailure.** A main-session StopFailure that §5.6 cannot settle (no versioned exact native error) is not a root-turn failure. The turn stays `unknown`, `activity` is unchanged, the composer is not offered as idle and held prompts do not flush. The StopFailure is recorded as a diagnostic until later evidence settles the turn.
- **Scoped failures.** A subagent or workflow failure, a configure failure and a `severity=error` diagnostic do not change `activity`. Each is recorded in its own scope: the workflow member's state, the configure command's outcome, the diagnostic event. A background subagent can fail while the root agent keeps working, so a scoped failure never shows the turn as ended, never offers the composer as idle, and never flushes held prompts.

Only settled root-turn end evidence sets `activity=idle`, by the scopes protocol.md already defines: `StopFailure` settles its turn only from an exact native error under a version rule and otherwise stays unknown (§5.6), a `SubagentStop` is not a root `Stop`, and a workflow member's end does not prove the root has no more work (§5.7, §5.8).

### 6.3 Cause table

Evidence is always Node-attested and about the lineage's current chapter at the live generation.

| Observed | Transition | Action |
|---|---|---|
| The process ended by itself after reporting running (native exit code, crash or external signal) | → starting | Restart decision (§6.3.1): F with successor (§7.2); restart notice (§6.4) |
| The chapter failed to start: the Node settled its create failed or rejected, or journaled a start failure, or the process ended before its first running event | starting → starting | Same, cause `start-failed` |
| `node-epoch-changed`, and the Node's inventory no longer holds the chapter | running, starting or host-offline → starting | Same, cause `node-restarted` |
| Any of the three rows above, when restart decisions in the last hour have reached `maxPerHour` | → paused{restart-cap} | F without successor; C1 push |
| Any of the first three rows, on a continuity lineage without `restart.onProcessLoss` | → paused{process-exit} | F without successor; today's exited push |
| The chapter's host lost its link | running → host-offline (derived) | Nothing. The process may be alive (D-019), and the row is not settled `host-lost`. Same-epoch reconnect with the chapter present → running. Epoch change without it → row 3 |
| A close admitted from a Human device (PWA Pause, CLI, fleet) | → paused{device} | F without successor at admission; the close is then forwarded |
| A close by the chapter itself | → paused{self} | Same; C1 push |
| A close by an Agent ancestor | → paused{ancestor} | Same; C1 push |
| A settled root-session turn failure (§6.2): the root turn's result reporting an API error such as 429, or a main-session StopFailure without an `agentId` that protocol.md §5.6 has settled | none | Not an exit and not a restart cause. The root turn ends with outcome `failed`: `activity=idle` plus a turn-error marker, never `lifecycle=failed`; retryable in place |
| A main-session StopFailure that §5.6 cannot settle (§6.2) | none | Not an exit and not a restart cause. The turn stays `unknown`; `activity` is unchanged and held prompts do not flush |
| A scoped failure (§6.2): a subagent or workflow failure, a configure failure, a `severity=error` diagnostic | none | Not an exit and not a restart cause. `activity` is unchanged; the failure is recorded in its own scope (workflow member, configure outcome, diagnostic) |
| Owner Resume (existing resume verb) on a paused lineage | paused → starting | F with successor; 409 while the chapter's host is offline |
| Owner Resume on a lineage that is not paused | — | 409: pause it first |
| `DELETE` of the current chapter of a lineage that is not paused | — | 409: close it first |

#### 6.3.1 The supervisor

- **When it runs.** It runs on the Hub's existing 1 s scheduler tick and once at boot. It evaluates lineages that are `starting`, `running` or `host-offline`. A start failure while the lineage is still `starting` is eligible.
- **Evidence is bound to one chapter.** Evidence names the chapter's instance id, which belongs to exactly one generation. The supervisor ignores evidence about a chapter that is no longer current or whose generation is not the lineage's live generation.
- **Exactly one decision per failed chapter.** The decision is made inside F, or inside the cap's F without successor, with a CAS on the failed chapter's generation. It is recorded in the lineage's restart-decision log, keyed by the failed chapter id, which is unique. A second evaluation cannot decide again: another tick, a Hub restart or a racing Resume either fails the CAS or finds the key already present.
- **Recovery source.** The supervisor picks the first that applies:
  1. the failed chapter's native session id, if it reported one;
  2. otherwise, the most recent earlier chapter of the lineage that reported one, so the conversation continues from its last durable point;
  3. otherwise (the first chapter never reported one), a fresh launch from the lineage's original create spec and launch brief.

  The successor always runs claude-sdk on the same host.
- **The cap** counts restart decisions of every cause in the last hour. At the cap, the supervisor runs F without successor, which also fences the failed chapter.
- **Close always wins.** A close fences at admission, so an exit that follows a close finds the lineage already paused and is ignored. The Hub never undoes the owner's stop.
- **Hub restart.** Evidence lives in the journal and in command rows. The boot evaluation finds a pending failure and decides it once.
- **Launch authority of a successor.** Only a Human device can set `restart`, and setting it is the owner's standing authorization to relaunch the same spec (OA5: the restart uses the seating Human authorization as a standing grant). The successor's create therefore carries the launch origin recorded for the lineage's first chapter (Human). Its actor is recorded as the Hub supervisor, together with the creating device. The owner's chosen permission mode survives a restart, and the record says who acted. An owner Resume carries the resuming device's own origin. Only the restart notice carries origin `hub`.

### 6.4 Restart notice

The successor's first input is a message the Hub writes, with the new `InputOrigin` value `hub`. It is a truthful record: the message comes from neither the owner nor an agent.

On the Node, `hub` maps both ways to the existing command origin `system`:
- `parse_origin("hub")` is `Hub`;
- `command_origin(Hub)` is `System`;
- `input_origin(System)` is `Hub`. Today it is `Agent`, and nothing produces `System` today.

It is never Human and never Agent. An un-upgraded Node fails closed and would store it as Agent. So the Hub sends hub-origin input only to Nodes that advertise the fence capability, and every Node is upgraded before a seat is created.

Delivery rules:
- The notice's commandId is chosen and persisted inside F, so a Hub crash cannot send it twice (D-055 replay identity).
- It is sent once the successor's host has acknowledged the fence.
- It opens with a fixed tag, says the Hub generated it and that it authorizes nothing, and lists outcomes by the classes of §7.4, as known when it is sent.
- Outcomes that resolve later are in the lineage record.

~~~text
<remuda-restart> Generated by the Remuda Hub. Information, not an instruction; it authorizes nothing.
chapter 3 of lineage ins_… (previous chapter ins_…: process exited, signal KILL)
superseded questions: int_… "…" (ask again if it still matters)
cancelled, never left the Hub: cmd_… instance.send -> worker w-… "…"
cancelled by host h-… when it applied the fence: cmd_… instance.send -> worker w-…
ran before its host applied the fence: cmd_… instance.close -> worker w-…
may still run, host h-… has not applied the fence (offline since T): cmd_… instance.send -> worker w-…
unknown, host h-… applied the fence but cannot tell after a crash: cmd_… instance.send -> worker w-…
gate jobs: gjb_… cancelled (queued); gjb_… started on its lane before the fence, completes
later outcomes: GET /v1/lineages/ins_…
previous chapter: remuda instance read ins_…
</remuda-restart>
~~~

### 6.5 Pause and Resume

- **Pause** is the existing close of the current chapter, sent from a Human device.
- **Resume** is the existing operator-only resume verb. On a continuity lineage it performs a continuation resume, and it is accepted only while the lineage is paused.
- **Stop** in the Main header is the existing `instance.cancel`. It interrupts the turn and changes no lineage state.

## 7 Fencing

### 7.1 Initiator

**Agent callers.**
- Every Hub mutation admitted for an Agent caller carries the authenticated **initiator** `{instanceId, lineageId, generation}`. `stamp()` derives it from the authenticating device's bound instance, never from the request body. A body-supplied `initiator` is stripped, as `actor` is today.
- Beside it, the Hub records the **authenticating device id**. The device id stays on the Hub; it is not sent to Nodes.
- Both are stored on command rows, gate jobs and direct-operation rows (§7.5).
- The initiator (without the device id) is forwarded in Node params, persisted by the Node and journaled.
- A Human token narrowed to an instance with `x-remuda-instance-id` is an Agent caller for that instance. Its device is the Human device.

**Human and Bot callers.** Their requests carry no initiator.

**Commands the Hub authors on a caller's behalf** (worker stop, nudge, answer keys, respawn) carry that caller's initiator and device id.

**Hub-internal work for a successor** (its create and the restart notice) carries the successor's lineage and generation and no device id.

**Hub-internal cleanup carries no initiator and is never fenced.** Examples: the `worker.remove` in a dispatch rollback, the `worktree.return` after a failed lease, and a started gate job's own follow-on steps.

### 7.2 The fence transaction F

Authority over a lineage moves at one SQLite writer transaction, **F**. The Hub has a single writer thread, so F is totally ordered with every other write. F runs with a compare-and-set on the lineage generation G and does the following:

1. Sets the generation to G+1, and the state to `starting` (with a successor) or `paused{by}` (without one).
2. Marks the predecessor chapter fenced and records why: superseded by its successor, or paused by whom.
3. Deletes every Agent device row bound to the predecessor: its launch credential and any MCP token minted for it.
4. Cancels at the Hub every command initiated by the lineage at generation ≤ G that has no committed forward intent (`queued`, `forwarded=0`, any operation). These become `rejected` with reason `fenced`. D-055 replay identity, and the forward of still-queued rows, apply to every operation except `instance.configure`, so every operation must be covered here.
5. Records as **potentially executed** every command initiated at ≤ G whose forward intent committed before F and that is not settled.
6. Handles gate jobs initiated at ≤ G:
   - queued jobs are cancelled (reason `fenced`);
   - jobs the Hub has claimed (Hub `running`) but that have no Node-attested start are recorded as potentially executed;
   - jobs that are Node-started are recorded as running; they complete.
7. Handles direct operations initiated at ≤ G (§7.5): admitted but not sent → cancelled; sent and not settled → potentially executed.
8. Marks the predecessor's pending interactions superseded, pointing at the successor or at the paused lineage.
9. Inserts a durable fence `{fenceId, lineageId, liveGeneration: G+1, reason, actor}`, with a pending delivery row for every known host.
10. With a successor: inserts the new chapter (continuation edge, copied delegation and `restart`, `chapterCause`), its resume create command and the restart notice's commandId. For C1 it also records the restart decision (§6.3.1).
11. Writes audit rows with the actor chain.

After commit, in this order:
1. update the Hub's in-memory fence map, which the send gate uses (§7.4);
2. revoke the predecessor's API egress contexts (D-048);
3. enqueue the fence on every online host's link;
4. forward the successor's create once its host has acknowledged the fence.

The address-owner and dispatch uniqueness checks ignore fenced chapters. The successor's insert in the same transaction therefore passes, and two unfenced holders cannot exist.

### 7.3 Commit-time authority check

Authentication happens at the start of a request, and F can commit while the request is still in flight. So every writer job that admits an Agent-initiated mutation calls `check_initiator(initiator, deviceId)` inside that same job. The check requires all of these:
- the initiator's instance exists and is not fenced;
- its generation equals the lineage's live generation;
- the lineage is not paused;
- the specific authenticating device row (by id) still exists. Hub-internal successor work has no device id and skips this clause.

Otherwise the job refuses with the existing 409 shape and reason `fenced`, and writes nothing.

The covered set:
- every write route that `restrict_agent_routes` admits for Agent origin: instance create and commands, fleet create and broadcast, interaction answers, project and gate writes, and task writes including land;
- the worker routes;
- `queue_command`;
- `mark_forward_intent`, which re-checks the initiator and device id stamped on the row, so a same-id replay of a held row is refused;
- gate job insert and gate job claim;
- direct-operation admission (§7.5).

Handlers with several steps fail at their next commit and use their existing unwind, for example dispatch's rollback of a provisioned worktree.

### 7.4 Two boundaries, and the outcome of every operation

**Hub admission boundary (strict).** The writer orders F against every admission: `queue_command`, `mark_forward_intent`, gate claim and direct-operation admission. After F commits, nothing initiated at ≤ G is admitted.

**Send gate (narrows the window, guarantees nothing).** Immediately before writing an admitted operation's frame to a link, the sender checks the in-memory fence map. If the initiator is fenced, it writes nothing and settles the operation as `withheld` (reason `fenced`). An operation that passed the gate before the map was updated can still reach the wire after F. That is why the gate is not a guarantee.

**Node execution boundary (eventual).** Each Node applies a fence when it receives it, or at daemon start for a persisted fence. Until then, an operation admitted before F may still execute on that Node:
- an online Node has the fence's transit window;
- a disconnected Node keeps working its own queues. For example, a prompt in the PTY queue is delivered when the worker's busy turn ends.

From the moment a Node applies the fence, it:
- cancels its not-yet-delivered operations from fenced initiators;
- refuses operations that arrive later;
- reports in its acknowledgement what it cancelled, what had already run, what it refused, and what it cannot establish (`unknown`).

A Node reports `unknown` when it holds a record of an operation but cannot establish whether it reached its executor. That typically follows a Node or process crash between the Node's durable intent and any evidence: a PTY prompt whose write was attempted as the process died, a direct operation whose entry was logged but whose outcome was not, or a command left at `intent-durable` with no native evidence (§2.5 of protocol.md: never presumed not to have run).

| Where the operation is when F commits | Outcome | Known at F? |
|---|---|---|
| In a request handler, not yet admitted | Refused at commit (§7.3) | Yes |
| Admitted, but never forward-intended or sent (held command, queued gate job, admitted direct operation) | Cancelled at the Hub by F | Yes |
| Forward-intended before F, then stopped by the send gate | Withheld | Yes, at the gate |
| Sent, or still to be written after passing the gate, and the target Node has not applied the fence | **Potentially executed**: unknown until that host's acknowledgement or the operation's own reply gives a definite outcome: `ran`, `cancelled-by-node` or `refused-by-node`. It stays `unknown` when the Node reports `unknown`, when the acknowledgement does not mention it, or when the host never returns. Absence of a record is never read as "not executed" | No |
| Reached its executor before the target Node applied the fence (prompt delivered, instance spawned, gate run started on its lane, answer committed, worktree provisioned or removed). This may be after F | Ran; it completes and is reported. If a crash leaves the Node unable to tell, it reports `unknown` instead | Resolved by the acknowledgement or reply only when the Node can establish it |

Unknown outcomes stay in the fence record and are shown truthfully as unknown to the successor and in the UI. A host acknowledgement does not always resolve them. A later reply or the existing read-only reconciliation (`reconcile.instance`, protocol.md §7.2) may still resolve one. Nothing resolves an operation as "not executed" by inference, and a host that never returns leaves its operations unknown.

A strict execution boundary would need every executor to acknowledge against the fence before F may commit. That is a coordinated protocol, and it is not part of Phase 1 (§14.4).

### 7.5 Direct mutating RPCs

These Hub→Node methods bypass the command table and the Node's `insert_command`, and Agent-initiated work can reach them in Phase 1:

| Method | Hub caller | Agent path |
|---|---|---|
| `interaction.answer` | `answer_interaction` (interactions.rs) | `POST /v1/interactions/{id}/answer` |
| `worker.provision` | dispatch (workers.rs) | `POST /v1/workers/dispatch` |
| `worker.remove` | retire and replace (workers.rs) | worker retire, replace |
| `worktree.lease`, `worktree.return` | `lease_on_host` (http.rs) from task binding (tasks.rs); workers.rs | task writes, retire |
| `gate.run` | `dispatch` (gatequeue.rs) | gate enqueue, task land |
| `gate.then`, `gate.land`, `gate.unpin`, `gate.cancel` | gatequeue.rs | follow-on steps or cancel of a job |

Operator-only methods are unchanged and carry no initiator. These are `workspace.register` and `workspace.unregister`, `instance.purge`, the worktree routes not admitted for Agents, and host-file writes.

One Hub helper is the shared boundary for every row above:

1. **Admission.** In one writer job, it runs `check_initiator` and persists an admission record with the initiator and device id, before any external effect. For most methods this is a `node_ops` row with an op id. For `interaction.answer` the op id is the answer's commandId. For gate methods, the gate job row itself is the admission record and its claim is the admission. The answer path no longer calls the Node before anything is written.
2. **Host gating.** A host with unacknowledged fences is not sent to. Request paths get the existing host-offline shape. The gate tick does not claim on that host's lanes.
3. **Send gate** (§7.4).
4. **Wire.** Params carry `initiator` and `opId`.
5. **Node entry.** The Node's dispatcher (`dispatch_frame`) checks persisted fences at entry for these methods, before any side effect, and refuses a fenced initiator with reason `fenced`. The Node writes each initiator-carrying direct operation's entry (op id, method, initiator) durably at entry, before any side effect, and its outcome afterwards, so its fence acknowledgement can report it. An entry without an outcome is reported `unknown`.

Gate jobs are distinguished as follows:
- Hub `running` only means claimed.
- Node execution starts when the lane Node's gate runner admits the run after its own fence check. The Hub records this as `nodeStartedAt`, from the job's first `phase` gate event.
- A claimed job without a Node start is potentially executed. It is cancellable until the lane starts it, by the send gate or by the lane's fence check.
- The follow-on steps of a Node-started job (`gate.then`, `gate.land`, `gate.unpin`, `gate.cancel`) carry the job id. The Node admits them as continuations of an operation that reached its executor before the fence.

### 7.6 Node fence and reconnect ordering

**The fence method.** A new Hub→Node method, `lineage.fence`, is documented in protocol.md §7.2. Its params are `{fenceId, lineageId, liveGeneration}`. Its result lists the instances the Node closed and, per operation it holds a record of, `cancelled`, `ran`, `refused` or `unknown` (§7.4). An operation the result does not mention stays unknown. Create and resume params carry the instance's lineage and generation, so the Node knows them.

**What a Node does with a fence.**
- It persists the fence, idempotently by fenceId.
- It closes its local instances of that lineage below the live generation.
- It cancels undelivered work from initiators of that lineage below the live generation, with reason `fenced`, and journals each cancellation. This covers command rows that were accepted but not started, and PTY prompt-queue entries.
- It cancels interactions owned by the instances it closed.
- It refuses later work from a fenced initiator, at three points:
  - `insert_command`;
  - PTY delivery, which re-checks before writing;
  - the dispatcher entry for the methods in §7.5.
- After a daemon restart, it applies persisted fences before resuming any queued delivery.

**Delivery and acknowledgement.** The Hub delivers each fence to every host that existed at F. When a Node says hello, the Hub sends that host's unacknowledged fences first, in creation order. Until all of them are acknowledged:
- `forward_if_online` ignores its `host_online` argument for that host. It neither marks forward intent nor writes, and returns the row queued, exactly as for an offline host. Every caller computes its own live bit from the socket registry (`post_command`, fleet.rs, placement.rs, worker_watch.rs, workers.rs), so the check must live inside `forward_if_online`.
- Direct operations get host-offline.
- The gate tick skips that host's lanes.

After the acknowledgements arrive:
- the acknowledgement handler forwards Hub-internal held work for successors (the create and the notice);
- other held rows follow the existing same-id retry;
- a failed acknowledgement drops the link, and the next hello retries.

**Capability.** Nodes advertise fence support in hello. The Hub refuses (409, reason `node-lacks-fence`) to send a Node without it any command or direct operation initiated by a continuity lineage, and sends it no hub-origin input. All other traffic to such a Node is unchanged. The Phase 1 deployment upgrades every Node.

### 7.7 Hub restart

F is atomic. Fence delivery rows survive a restart and are re-sent on boot and on hello. The supervisor re-evaluates at boot (§6.3.1). The generation CAS, the restart-decision key, and the persisted resume-create and notice ids make a restart idempotent. Releasing a forward intent (`release_forward_intent`) on a row whose initiator is fenced settles that row `rejected(fenced)` instead of returning it to the queue.

### 7.8 Races

- **Human close vs automatic restart.** Both go through F with a generation CAS.
  - If the close commits first, the restart's CAS sees a paused lineage and aborts.
  - If the restart commits first, the close addresses a superseded chapter. A Human close of any chapter of a lineage applies to its current chapter, so the successor is fenced without a successor.

  Either order ends in `paused{device}`, with no live chapter and nothing admitted from either chapter after its fence.
- **Two restarts** (tick re-entry, Hub restart), and **Resume vs restart**: the second CAS fails, or the decision key already exists.
- **A forward whose intent committed just before F.** It is potentially executed:
  - The send gate withholds it if the in-memory map already shows the fence.
  - If its frame reaches the link after the fence frame, the Node refuses it.
  - If it reaches the link before the fence frame, the Node runs or queues it. Applying the fence then cancels it if it is still queued.

  Every path settles it truthfully from what the Node reports, and that may be `unknown`.

### 7.9 Guarantees and non-guarantees

Guaranteed by the mechanism:
- At any instant, the Hub admits mutations for at most one chapter of a lineage. The writer orders F against every admission.
- After F commits, the Hub admits, queues, marks for forwarding and claims nothing initiated by the fenced generation. This includes requests authenticated before F.
- Every operation of the fenced generation that was unfinished at F gets a truthful outcome from §7.4: cancelled at the Hub, withheld, cancelled by the Node, refused by the Node, ran, or unknown. Unknown covers both "not yet acknowledged" and "the host cannot establish it"; it is never turned into "not executed" by inference.
- A Node that has applied the fence drops undelivered work from the fenced generation and refuses new work from it.
- The owner's close is never undone automatically.

Not guaranteed:
- F is the Hub's admission boundary, not a global execution boundary. Work admitted before F may still run on a Node until that Node applies the fence. This covers a disconnected Node's own queues, a fence in transit, and a frame written after passing the send gate.
- A fenced process whose Node is unreachable can keep acting on its own host through its shell, until the Node reconnects and applies the fence. A process orphaned by a Node restart is unknown to that Node, and only its revoked credential bounds it.
- Effects that ran are not recalled.
- That a host acknowledgement resolves every operation. After a Node or process crash, whether an operation ran can be unknowable; it then stays unknown.
- Exactly-once outward writes: idempotency keys for dispatch, create, gate and land are Phase 3.

### 7.10 Test matrix (fake Nodes, gated suite, deterministic hooks, no millisecond thresholds)

- **Held command.** A held command is cancelled by F, and a same-id replay after reconnect reaches nothing.
- **Prompt still queued when the Node reconnects.** A prompt is queued behind a busy turn on a disconnected Node and is still queued at reconnect. The fence is the first frame after reconnect, the prompt is cancelled by the Node, and the worker never receives the text.
- **Turn ends during the partition.** The worker's busy turn ends during the partition, after F and before reconnect. The prompt is delivered. It shows `unknown` until reconnect, then `ran`, never `cancelled`.
- **Send gate.** Forwarding pauses after `mark_forward_intent` commits until F has committed. The send gate withholds the frame and nothing reaches the Node.
- **Link order.** Forwarding pauses after the send gate has passed and before the link write, until the fence frame is enqueued. The Node refuses the command and the worker never receives it.
- **Unacknowledged-fence window.** During it, a caller passes `live=true` to `forward_if_online`. The command stays queued and no frame is written.
- **Answer.** A fence between authentication and `interaction.answer` admission, and one between admission and send: the Node's interaction stays pending.
- **Gate.** A fence between gate-job claim and `gate.run`: the lane never starts. A Node-started job completes, including its land step.
- **Worker provision and remove.** A fence before `worker.provision` or `worker.remove` reaches the Node: no worktree is created or removed.
- **Node dispatcher.** Each §7.5 method with a fenced initiator is refused at entry and has no side effect.
- **Credentials.** The predecessor's launch credential and its MCP tokens return 401. A request authenticated with a device row that was deleted mid-request is refused at commit.
- **Superseded interaction.** Answering it is refused.
- **Node without fence support.** Gets 409.
- **Hub restart between F and fence delivery.** The fence is delivered once.
- **Human close vs restart, both orders.** Driven by a test hook, not by timing.
- **Seat's Node changes epoch.** The seat's Node reconnects with a changed epoch while a worker's Node holds an in-flight prompt.
- **Crash ambiguity.** A Node is killed after a PTY prompt's write is attempted and before any delivery evidence, and a direct operation's entry is logged without an outcome. After restart and fence application, the acknowledgement reports both `unknown`; the Hub keeps them unknown, the UI and the restart notice show unknown, and nothing shows them as not executed. An operation the acknowledgement omits also stays unknown.

## 8 Push

### 8.1 Routing rule

An alert about instance I pages the owner exactly as today, unless all three of these hold when it fires:

1. I has an Agent parent whose lineage is P.
2. The item was **actually routed** to P. For an interaction, that means:
   - it is not approval-kind;
   - the D-051 predicate admits it for P's current chapter. The predicate covers the project switch, the one-hop lineage edge and the plan-review self-exclusion. Since the D-051(b) amendment (OA3), bypass on either side no longer excludes questions and plan reviews.

   In Phase 1 nothing else is routed (turn errors, exits, blocked timers), because no channel delivers it to P.
3. P is **effectively running**: its state is `running`, its current chapter has not ended, and that chapter's host has a live link.

If all three hold, the alert is not pushed. It still appears in /approvals and in the transcript.

Approval-kind interactions are never routed (D-051(a)) and always page. Instances with a Human parent page as today.

### 8.2 Restart window

Whenever P is not effectively running, condition 3 fails and its children's alerts page the owner. That covers `starting` or "restarting", `host-offline`, `paused`, and the time after its chapter ended but before the supervisor has acted.

### 8.3 Suppression rows and the lift

- **Suppression rows.** Every suppression writes a durable row: interaction, lineage, chapter and rule. The row is also the audit row (§8.6).
- **One transition function.** A single store function computes when "effectively running" turns from true to false for a lineage. Every place that can cause that transition calls it:
  - F (pause, close, restart, cap);
  - the supervisor;
  - host link loss: `mark_host_offline`, from the end of the WS session in ws.rs and from ssh_hosts.rs;
  - host-loss detection (`expire_lost_hosts`);
  - boot reconciliation.
- **The lift.** In the same writer job, the function marks every still-pending interaction that was suppressed because of that lineage as lifted. For each one it inserts one push outbox row, unique per suppression, so a second transition cannot add another.
- **Delivery.** After commit, the outbox row is sent with today's body and then marked delivered. Undelivered rows are sent after a Hub restart. The push tag is the interaction's, so if a crash falls between send and mark, the resend replaces the notification instead of adding a second one.
- **Boot.** At start the Hub marks every host offline. That is bookkeeping, not an observation of the parent. Boot reconciliation then does three things:
  1. sends undelivered outbox rows;
  2. lifts for lineages that are not running by state;
  3. for lineages that are running by state, waits for the first of these:
     - the parent host's hello. If the epoch is the same and the chapter is in its inventory, P is still effectively running and nothing is lifted; otherwise the lift happens;
     - host-loss detection under the existing grace.
- **No re-suppression.** A lifted interaction is not suppressed again when P returns to running. New alerts are evaluated normally.

### 8.4 Per device

A follow suppresses pushes only for the device that is following, and only while that device reports the page visible. The PWA reports visibility with an additive follow frame: document hidden or shown. A client that never sends the frame counts as visible, which matches today's behaviour for that one device. A desktop tab left open on Main no longer silences the phone. This replaces the statement in ui-spec §4.5 that the follow suppression rule does not change. It is a delivery fix for every session.

### 8.5 The seat's own pushes

- **Its own interactions** page as today, because its parent is the human.
- **Reply-ready.** When a turn of the live `address-owner` chapter ends and at least one Human-origin input reached that turn, the Hub pushes "Main replied: <first line of the final message>" (truncated). Per-device suppression applies. Turns reached only by Hub or Agent input do not push. The `address-owner` grant already means "the agent that addresses the owner", so this needs no new property.
- **C1 pushes** replace the generic "Session exited" push for lineages with `restart`: "Main restarted (cause)", "Main paused: restart cap" and "Main closed itself". A pause from a Human device sends nothing.
- **End pushes need process-end evidence (OA6).** The exited push and the C1 pushes fire only when an instance ends by the evidence in §6.2. No other failure is an end, and none sends either push. A root-turn failure shows as a turn-error marker in the transcript and the status line; a scoped failure shows in its own scope (workflow member, configure outcome, diagnostic).

### 8.6 Audit

Every suppression writes an audit row (subject: the interaction; detail: lineage, chapter, rule). The Phase 1 push audit is derived from these rows, the lift outbox, the journal and the push log.

### 8.7 Phase 2

Routed facts (§10) extend condition 2 to turn errors, exits and blocked timers. Such an alert is suppressed only once its fact has been written to P's inbox while P is effectively running. The lift rule also covers undelivered routed facts.

## 9 Chat surface

Phase 1:
- **Main entry**, pinned in `/m` and in the desktop session list. It resolves to the live address-owner chapter. When the lineage is paused, it resolves to the latest chapter of the most recent address-owner lineage, labelled for example "paused by you at T".
- **One conversation across chapters.** Chapters render in order, separated by dividers. Each divider shows the cause, the time, the predecessor's superseded interactions (linked to /approvals) and a summary of fence outcomes: cancelled, ran, and still unknown with the host named. Earlier chapters are collapsed and load on demand. A new read route returns a lineage's chapters, states and fence records.
- **Status line.** Shows the chapter number, lifecycle and activity, restarts in the last 24 h with their causes, the last activity, and the lineage state: paused by you at T, restart cap, host offline since T, or restarting.
- **Controls**, on Human devices only: Stop (cancel the turn), Pause (close the current chapter), and Resume (continuation resume, shown only while the lineage is paused).
- **Hub-origin input** (the restart notice) renders verbatim as a distinct system row, never as the owner's bubble.
- **A held owner message addressed to a superseded chapter** settles as rejected. The UI offers an explicit "send to current Main", which uses a new commandId (D-055: never an automatic resend under a new id).
- **/approvals** shows superseded items with a link to the current chapter.

Phase 2 adds:
- one collapsed background row per Hub-delivered fact batch, plus the turn it triggered, with the verbatim text one tap away;
- an Owner-thread / All-activity toggle;
- "to you" cards for `owner notify`;
- `remuda chat` for the terminal.

## 10 Hub-delivered facts (Phase 2)

- **Inbox.** A durable table `(subscriber lineage, kind, dedupeKey unique, payload, createdAt, batchId)`. Rows are written on the writer thread from journal appends, never by polling. Kinds:
  - child lifecycle, activity and turn results; exits;
  - interactions, with whether each was routed to the parent;
  - terminal gate states;
  - supply-window resets for model families a child was parked on;
  - hosts in scope going online or offline;
  - superseded interactions after a chapter change;
  - the subscriber's own due wake-ups, and context-percentage crossings of thresholds it wrote.

  Screen classification (`observe_one`) is only a fallback, for screen-reporting carriers whose journal has been quiet for the project's `stall_threshold_mins`. It is swept per host in its own task with the existing timeout, so one unreachable host delays nothing else.
- **Delivery.** Delivery happens when all three hold:
  - the live chapter is idle (Phase 1 projection);
  - it has no pending interaction of its own;
  - it has no owner-origin command queued or in flight. Owner messages always go first.

  All undelivered rows are then coalesced into one `instance.send` with origin `hub`, a fixed opening tag, worker text quoted as untrusted data, and a short snapshot footer. If the chapter is busy the batch waits; it is never rejected.
- **Ids and replay.** A batch's commandId is chosen and persisted when the batch is formed. D-055 replay identity covers every operation except `instance.configure`: a re-POST with the same id and payload returns the original row, and forwards a still-held row once. An unknown outcome is reconciled through `GET /v1/instances/{id}/commands/{commandId}` and is never resent under a new id. Delivery is at most once per batch, not exactly once.
- **Fencing.** Batches target the live chapter and carry the Hub-internal initiator for its generation. F cancels an undelivered batch, and its rows are re-batched for the successor under a new id.
- **Defaults.** Facts are delivered to continuity lineages and to coordinators created by agents. A Human-created instance without continuity gets none unless its creator opts in, so the owner's own sessions see no new behaviour. The agent can mute each kind.

## 11 Wake-ups (Phase 2)

No existing capability survives a restart: harness cron and monitors are session-only. Phase 2 therefore adds wake-up rows that the agent writes. A wake-up fires at a time, at every interval, or when a condition over facts holds (including context percentage). The agent manages them with a small CLI (`remuda wake add|list|rm`). The writer thread claims each one once, and delivery goes through the inbox. A recurring wake-up that missed occurrences fires once and reports how many it missed. There are no quiet hours, and the Hub chooses no cadence or thresholds. `/healthz` reports the last tick and the delivery age, and returns 503 when the tick is stale.

## 12 Memory documents (Phase 3)

The Hub holds an identity document and a memory document (Facts, Preferences, Commitments, Lessons) as objects with a revisioned pointer:
- their size is bounded;
- writes replace the whole document and carry a revision check; conflicts are shown, never overwritten;
- a lint keeps secrets out;
- they are included in hub-backup and editable from the PWA.

One replaceable instruction block in the launch brief names them. They are data and are never injected as instructions.

An agent that wants a fresh context writes a handoff and closes itself. Phase 3 turns that self-close into a fresh chapter; Phase 1 pauses instead. The Hub never opens a chapter on a threshold of its own.

## 13 Phase 1 wire and state budget

**State.**
- Hub tables:
  - `lineages`: id, current chapter, generation, state, paused-by and paused-at, restart, and a reference to the original create spec;
  - `lineage_restart_decisions`: failed chapter id (unique), cause, decided at, outcome;
  - `node_ops` (direct-operation admissions);
  - `fences`, with per-operation outcomes and per-host delivery rows;
  - `push_suppressions`, with a lifted marker, and a lift push outbox.
- Hub columns:
  - instances: `lineage_id`, `generation`, `chapter_cause`, `fenced_at`, `restart_json`;
  - commands: initiator columns and `initiator_device_id`;
  - gate jobs: `initiator`, `initiatorDeviceId` and `nodeStartedAt`;
  - interactions: `superseded_by`.
- Audit rows.
- On the Node: persisted fences, the initiator on its Command record, and a direct-operation log.

**Wire, all additive.**
- `InputOrigin` value `hub`. The Node maps it to and from the command origin `system`.
- `initiator` on forwarded command params.
- `initiator` and `opId` on the params of `interaction.answer`, `worker.provision`, `worker.remove`, `worktree.lease`, `worktree.return` and `gate.run`.
- Lineage and generation on create and resume params.
- `creatorPermissionMode` on the params of an Agent-origin create, stamped by the Hub (OA1).
- The `lineage.fence` method and result, and a hello capability.
- Dispatch `permissionMode`.
- The follow visibility frame.
- `GET /v1/lineages/{id}`.
- Instance projection fields `lineageId`, `generation`, `chapterCause`, `restart`, and a turn-error marker for a root-turn failure.
- Refusal reasons `fenced` and `node-lacks-fence`, on existing error shapes.

New frames are documented in protocol.md inside `~~~text` fences, because a `json` fence there may contain only a complete frame of an existing type.

**Changed rules.**
- Worker routes are admitted, with lineage ownership required for mutations.
- Worker respawn goes through the scoped helper.
- `restrict_permission` and the Node materializer apply "never beyond the creator" (OA1).
- D-051(b): bypass no longer excludes questions and plan reviews from delegation (OA3).
- Dispatch approval is decided before allocation.
- Agent `worker answer` keys need the one-shot approval.
- `owns()`, the D-051 edge and fan-out read the lineage.
- claude-sdk resume stays claude-sdk.
- Continuity rows are not settled `host-lost`.
- Only process-end evidence makes an instance terminal; other failures stay live and are never a restart cause; only settled root-turn end evidence sets `activity=idle`, and scoped failures leave it unchanged (OA6).
- Uniqueness ignores fenced chapters.
- Resume works only on paused lineages, and `DELETE` of an unpaused current chapter returns 409.
- `forward_if_online` withholds while fences are unacknowledged.
- The send gate.
- Per-device push suppression.
- The effectively-running, restart-window and lift rules.
- Reply-ready and C1 pushes.

**Unchanged.**
- The Human-origin dispatch default, including today's default carrier selection.
- D-045.
- The D-051 switch mechanism (the switch is set on for the self-development project, OA3).
- Plain sessions' D-026 resume, apart from the claude-sdk carrier fix.
- The public operator resume route.
- Everything not listed.

Every new route and CommandRecord field is reflected in `crates/remuda-hub/openapi/openapi.json`.

## 14 Phases and exit criteria

### 14.1 Phase 1: a resident main agent you can chat with from the phone

**Goal.** The owner closes the laptop and keeps chatting from the phone with one main agent on the intranet Hub. The agent:
- dispatches, watches, gates and lands through existing verbs;
- after process loss, including a failed start, comes back on its own;
- never has Hub mutations admitted alongside a fenced predecessor;
- reports truthfully what the predecessor's in-flight work did;
- pages the owner honestly.

**Exit criteria**, recorded (redacted) in `docs/design/evidence/main-agent-1.md`:
- **E1 Placement.** The intranet Hub runs the Phase 1 commit. `remuda version --json` in the container reports that commit as `git_sha`, and `/healthz` reports that binary's package `version`. The devbox and laptop Nodes are attached as D-019 daemons and advertise fence support; the laptop Node kept its enrolled host id. The devbox host record advertises herdr. The D-051 switch is on for the self-development project. The phone is paired over the approved route and has received a test push, including once off the office network.
- **E2 Real task, laptop lid closed for at least 2 h.**
  - From the phone, the owner asks for a real task.
  - Using only Hub verbs with its Agent credential, the seat dispatches at least 2 workers on the devbox without any Remuda one-shot approval: same host; `--carrier herdr` for Claude workers, or a codex/grok harness; a permission mode admitted by §4.2 item 3, requested or inherited from the seat.
  - It reads at least one DONE from a worker journal.
  - It runs gate verify and land on the project's lanes, with the devbox as home host. It lands only if it holds `land`; otherwise it enqueues verify and the owner lands.
  - It reports in Main.
  - The owner learns of the reply from the reply-ready push on the phone, while a desktop tab follows Main.
- **E3 Process loss.** Killing the seat's process yields a resumed claude-sdk chapter in the same lineage, with the same grants, scope and permission mode. The new chapter:
  - receives the restart notice;
  - accepts a second turn;
  - stops and messages its predecessor's workers, and resumes one of them with `worker resume`.

  The predecessor's credential is refused, and the restart push reached the phone. The observed time to restart is recorded, not asserted in CI.
- **E4 Pause and Resume.** Pause from the phone leaves the lineage paused with no restart, and the status line shows "paused by you at T". Resume starts a new chapter.
- **E5 Fencing and continuity suites** pass in the gated suite: §7.10, plus the start-failure, cap and Hub-restart fixtures of §6.3.1.
- **E6 Push suite** passes:
  - the §8 truth table: routed vs not routed, approvals, restart window;
  - the lift on pause and on host disconnect, across a Hub restart;
  - per-device suppression with visibility;
  - reply-ready;
  - C1 pushes.
- **E7 Admission suite** passes with Agent tokens:
  - the §4.2 items, including the carrier cases and the respawn helper;
  - "never beyond the creator" across creator and child modes, explicit and inherited, in Hub and Node (OA1);
  - delegated questions and plan reviews with bypass on either side, and approvals still refused to Agents (OA3);
  - 403 on mutating an owner-dispatched worker;
  - the keys approval;
  - Human-origin dispatch byte-identical to today.
- **E8 Projection fixtures.** claude-sdk goes working → idle on settled root-turn end evidence. A settled root-session turn failure (an API error such as 429 on the root turn's result, or a main-session StopFailure without an `agentId` that §5.6 settles from a versioned exact native error) ends the root turn with outcome `failed` and goes idle with a turn-error marker. A main-session StopFailure that §5.6 cannot settle leaves the turn `unknown`: activity is unchanged and held prompts do not flush. A background subagent or workflow member fails while the root keeps working: activity stays `working`, the composer is not offered as idle, held prompts do not flush, and the failure appears on the workflow member. A configure failure and a `severity=error` diagnostic change only their own record. None of these triggers a restart or an end push, and the seat keeps its address-owner grant; only the process-end evidence of §6.2 ends it.
- **E9 Push audit** for the E2 run. Every push sent and every suppression is accounted for by §8. Nothing was suppressed that had not been routed to an effectively running chapter.

### 14.2 Phase 2: facts, not polling

Scope:
- the inbox and journal-driven fact sources;
- idle delivery with owner-first precedence;
- wake-ups;
- routed-fact push suppression;
- the layered transcript;
- `owner notify`: an `address-owner`-gated push for fact-triggered turns, rendered as a "to you" card;
- `remuda chat`;
- clock health.

Exit:
- A synthetic 429 on a worker yields exactly one fact to its parent within one sweep, and the reset fact after `resetsAt`.
- Restarting the Hub mid-delivery loses nothing and duplicates nothing.
- A wake-up set for T+10 min fires once across a Hub restart.
- Facts arriving during a 20-minute busy turn arrive as one message at the next idle point, after the owner's message sent during that turn has been answered.
- A hung fake Node delays no facts for other hosts.
- Laptop sleep yields a host-offline fact.
- Owner-thread mode on the phone shows only owner turns, replies and "to you" cards.
- The session-only monitors and cron are no longer used.

### 14.3 Phase 3: durable memory, agent-opened chapters, idempotent writes

Scope:
- Hub memory documents and the instruction block;
- self-close becomes a fresh chapter, briefed with the documents, a snapshot and predecessor ids;
- caller idempotency keys on dispatch, create, gate enqueue and land, with intent recorded before acting and reconciliation from Node and git state;
- the gate's `conflict` status passed through with its file list (today `crates/remuda-node/src/gate.rs` folds it into `failed`);
- reviews as child tasks.

Exit:
- A context-threshold wake-up leads to a handoff and a fresh chapter that continues a multi-worker task without losing a commitment.
- A duplicate gate enqueue and a duplicate dispatch from a Human-token session each collapse to one.
- Rebooting the devbox resumes the seat, with worker states arriving as facts.
- An owner edit to the memory document on the phone is reflected in the next turn.

### 14.4 Later options (not scheduled)

- a per-harness posture map that ranks the modes which keep control on (OA1 bounds only the control-off axis);
- OS-user isolation of agent processes, and Node deny lists;
- land separation or a land-grant policy;
- a per-subtree delegated-decisions property;
- MCP mirrors of the coordinator verbs;
- waiving the cross-host approval for hosts named in the seat's scope;
- distinguishing an agent-target `shell-pty` from a raw shell for the D-017 approval;
- a strict execution-acknowledgement fence, where every executor confirms against the fence before F commits;
- relocating a seat to another host as a fresh chapter;
- `restart` set by Agent creators, bounded by their own;
- dispatch-holder uniqueness across a lineage, for recursive coordinators;
- credential brokering;
- inbound channels;
- content in push bodies;
- binding devices to an owner.

## 15 Phase 1 task order

Docs come first. Each task keeps the full gated suite (Rust, web, and hub e2e against fake Nodes) green on its own.

1. `ma-docs`: this document, the ADR, and the protocol, ui-spec, runbook, coordinator-hierarchy and CLI doc updates.
2. `ma-ops-attach`: the owner or ops upgrade the intranet Hub, attach the Nodes, pair the phone and turn the D-051 switch on for the self-development project (no worker).
3. `ma-sdk-state`: claude-sdk working, idle and turn-error projection; only process-end evidence is terminal, and only settled root-turn end evidence sets idle (OA6).
4. `ma-lineage`: lineages; continuation resume (claude-sdk to claude-sdk) as one transaction; lineage-aware ownership and fan-out; `restart` stored.
5. `ma-initiator`: initiator and device stamping, the commit-time authority check, and the Hub side of the direct-RPC boundary.
6. `ma-fence`: F, the send gate, the Node fence at every executor entry, reconnect ordering, outcome resolution, close becomes pause, Resume only from paused.
7. `ma-admission`: worker routes, ownership, dispatch permission, "never beyond the creator" in the Hub and the Node (OA1), the D-051(b) amendment (OA3), approval before allocation, keys, the scoped respawn helper.
8. `ma-restart`: the C1 supervisor (including start failure and the cap), the restart notice, the `hub` origin, C1 pushes.
9. `ma-seat-cli`: seating flags on `instance create`.
10. `ma-push`: routing, the effectively-running rule, the centralized lift, per-device suppression, reply-ready, audit.
11. `ma-main-ui`: Main entry, chapters, status line, controls, hub-origin rows.
12. `ma-skill`: the skill update for an Agent seat.
13. `ma-evidence-cutover`: upgrade to the Phase 1 commit, seat Main, collect E1–E9, cut over.

## 16 Cutover (D4)

- **Before the exit criteria pass,** the human-run coordinator keeps the self-development loop. During the exit-evidence window it is quiesced (no dispatch, gate or land), and the scripted landing gate stays idle on the devbox. The two share ports and the e2e lock. So there is never a second actor on the project.
- **At cutover, in one step:**
  - the human coordinator stops dispatching, gating and landing;
  - its scripted gate, monitors and session cron leave the loop path;
  - the owner's Human devices remain, for reading and answering.

  "Read only" is a procedure, not a mechanism: Remuda has no read-only Human token. Protection against duplicate actors beyond that procedure comes with Phase 3's idempotency keys.
- **Rollback:** Pause Main from the phone and resume the human coordinator's loop.

## 17 What stays out, and rejected alternatives

Out:
- a Hub-side agent loop, planner or workflow engine (D-002);
- a new agent kind or a seat table;
- posture machinery (D3);
- harness cadence, quiet hours, or thresholds chosen by the Hub;
- hidden messages;
- tunnels or a public front door (D-031);
- delegating approvals;
- any keystroke path around approvals;
- project-wide switches changed on the seat's behalf.

Rejected:
- **Keep the main agent in a laptop session with better scripts:** fails A1–A4 by placement.
- **Run the seat on the owner's Human token:** borrows human authority and makes the audit untruthful (D-017).
- **Restart on `host-lost`:** under D-019 the process may still be alive, which creates two actors.
- **Suppress every non-approval alert of an Agent-parented instance:** questions that were never routed would page nobody.
- **Lift suppression only at F and in the supervisor:** a question suppressed while Main ran would never page when Main's Node disconnects.
- **Fence only the predecessor's row and credential:** requests authenticated before the revoke, commands already queued or held, and direct RPCs would still run.
- **Claim F as a global execution boundary:** a disconnected Node's own queues keep delivering after F, and an online Node applies the fence only after its transit. Only an acknowledgement protocol could make that claim true, and it is a later option.
- **Rewrite workers' `parentInstanceId` on restart, or make a respawned worker a child of its previous instance:** the audit would stop showing who created what, or the worker would leave its coordinator's one-hop ownership.
- **Reject facts while busy:** starves a coordinator that is almost always mid-turn.
- **A per-harness posture map, OS-user isolation and land-grant machinery in Phase 1:** removed by D3 and listed in §14.4.

Reference studied: open-muse (open source).
- Adopted: the loop on always-on infrastructure; a service that is a clock and never reasons; proactive turns as ordinary messages that never interrupt a conversation waiting on the person; notifications suppressed only where the app is in front.
- Not adopted: a fixed check-in cadence, hidden machine turns, and rejecting work when busy.
