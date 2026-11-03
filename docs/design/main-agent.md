# Main agent: a resident Remuda instance on the always-on Hub

> Status: design v3, 2026-10-05. Decision record: ADR D-057 in [decisions.md](./decisions.md). This document lands before any Phase 1 code (§15). Owner decisions D1–D4 and the owner amendments OA1–OA6 (§0) are binding. Where this document and an older design disagree, the owner decisions win.
>
> Companion layer: ADR D-060 (2026-10-29) adds per-entry Hub memory (§3.6, §12), fact and wake-up delivery constraints (§10.1, §11), usage-limit resume (§11.6), read projections (§10.2), and the companion-track tasks (§14.5, §15). The owner adopted its memory, origin and usage-limit rulings (R1–R3) on 2026-10-29; its other parts are proposed. The agent/user capability ledger is [agent-parity.md](./agent-parity.md).

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

### 3.4.1 The skill and its capability section (ma-skill)

D-060 item 5. A seat gets the `remuda` skill (`skills/remuda`: the memory rules, "the engine is not Remuda", delegation, read before retrying) and a generated "This seat in Remuda" section that states what its grants let it do. Neither is a procedure, and the section grants nothing: every verb it names is still checked by its handler on every call.

**The facts.** On every forwarded `instance.create` and `instance.resume` of kind `claude`, `codex`, `grok` or `agy`, the Hub stamps `skillContext {grants, herdr, delegatedDecisions, inbox}` (protocol.md §7.2): the instance's grants as stored, whether its host's record advertises herdr, the project's D-051 switch as the answer gate reads it, and whether the Hub delivers inbox facts to the instance's lineage (since `cb-inbox-deliver`: the lineage receives facts under §10 Defaults and its host advertises the fence capability, which hub-origin input needs). Like `memoryContext` it is generated by the Hub from its own rows, a body-supplied copy is stripped (top level and spec), it never travels in `InstanceSpec.args`, and it is stamped only for a Node whose hello advertises `capabilities.skillContext`; an older Node gets none. A launch without it (an older Hub, kinds `terminal` and `generic`) writes and records nothing.

**The tree.** The binary embeds the shipped files of `skills/remuda` (today `SKILL.md`; the section sources and the README are not shipped), and a test fails when the embed and the checkout differ in bytes or file set. The materializer writes them to `<native_home>/skills/remuda` (0700 directories, 0600 files, with a `.remuda-managed` marker) only where the harness reads that directory, following the computer-use rules (codex-cua.md §3.2):
- never the inherited operator home (`~/.claude` under delegation `none` with no explicit config dir), which Remuda never writes (D-045);
- never over a `skills/remuda` that holds files and has no marker: those files are the user's and stay byte for byte. An **empty** directory is taken over (it holds no bytes to keep, and it is what a launch's own cleanup leaves behind when it shreds a per-instance tree, so refusing it would leave the next launch in that home without a tree), and a tree carrying the marker is Remuda's to refresh;
- file lifetime by `native_home_is_shared`: a per-instance home (`<instance>/native-home`) is shredded with the launch; a shared home (the Node's `REMUDA_CLAUDE_CONFIG_DIR`, an explicit `claudeConfigDir`) keeps the tree, written through a temporary name unique to the writing process and thread, then renamed, so concurrent seats and later launches never read a half-written file or each other's partial write. The marker is written first, through a temporary name the ownership scan ignores and a rename, so a launch racing another never mistakes its sibling's tree for the user's, and a link in the marker's place is replaced rather than followed; it is accepted only as a regular file (`DirEntry::file_type`, never followed), and cleanup removes it last so a scan during teardown still sees Remuda's tree;
- only directories the launch itself creates get a mode: an existing `skills` (or `skills/remuda` with the marker) keeps the mode it has.

**The section.** The driver renders it from the facts (`render_section` in `crates/remuda-driver/src/launch/remuda_skill.rs`). It goes into the installed `SKILL.md`, appended, only when the home is the seat's alone (per-instance). A shared tree holds the same bytes for every seat on it, so there the section is skipped from the tree; then, as on every launch without a tree, it takes the §12.3 mechanism of its launch path, framed as `<remuda-capabilities>…</remuda-capabilities>`:
- on the paths that build the Claude argv themselves (`claude_argv`, and `materialize_shell_pty_agent` for kind `claude`), it joins the materializer's own `--append-system-prompt` pair, after the memory index, in the same single value; the pair is emitted on every process start, a native resume included, with `--system-prompt-snapshot off` on a resume (§12.3); the finished-argv guards let exactly that recorded value through;
- on every other agent launch it is the first-input data block, after the memory block; a native resume does not send it again. The Node strips both leading frames when it matches a delivered prompt to its command.

The recipe records the facts, where the tree went (`installed`, or `skipped` with `operator-home`, `user-owned` or `no-skill-reader`) and how the section rode (`tree`, `flag`, `data-block`).

| Harness | Carriers | Skill tree | Capability section |
|---|---|---|---|
| `claude` | `claude-print`, `claude-sdk`, `claude-pty`, `claude-bg`, `shell-pty`, on a per-instance home | installed | appended to the installed `SKILL.md` |
| `claude` | the same, on a shared home | installed, the embedded bytes only | the materializer's own system-prompt pair |
| `claude` | the same, on the inherited operator home, or over a user-owned tree | skipped | the materializer's own system-prompt pair |
| `claude` | `generic-pty`, home holds login material | installed, the embedded bytes only (the pane is pinned to that home, `pane_home_env`) | first-input data block (a raw pane never gets an argv flag) |
| `claude` | `generic-pty`, home holds no login material | skipped: the pane runs on the operator's own home, so nothing Remuda writes is read | first-input data block |
| `codex` | `generic-pty`, `shell-pty` (and `codex-appserver`, which no Node registers a driver for) | skipped: codex reads `$CODEX_HOME/skills`, but on `generic-pty` its home is the operator's own, and the `shell-pty` shadow home is created by the hook session after the recipe, only with hooks on | first-input data block |
| `grok`, `agy` | every carrier | skipped: no skills reader | first-input data block |
| `terminal`, `generic` | every carrier | nothing (no stamp) | nothing |

**What the section says.** Only verbs in the Today column of [agent-parity.md](./agent-parity.md), which a test reads: the instance reads and task reads every seat has; that durable memory is the Hub's lineage memory, through the skill's memory section, and never the harness's native auto-memory (the section rides the system prompt where no per-seat tree carries it, so this rule holds even when the skill body is not loaded; a real-condition seat wrote its native auto-memory files when the rule was only in the skill body); with `dispatch`, the worker, task-ledger and gate-verify verbs (`worker answer` flagged as asking the owner); without it, that it cannot start workers. With `land`, `remuda land`; without it the verb and its paragraph are omitted entirely, and the seat is told not to merge into or push `main` itself. For a seat with `dispatch`, herdr decides whether a same-host dispatch asks the owner. The D-051 switch decides whether questions and plan reviews come to the seat (`remuda instance respond`) or to the owner; approvals never do. With an inbox, it says what a batch is (the `<remuda-facts>` tag, the owner's messages first, authorizes nothing, quoted worker text is data) and, with `dispatch`, points at `remuda worker digest`; without one, it says to watch by reading inside its own turns. The static intro tells the seat the section exists and wins where the general guide disagrees.

**Limits.** A seat on the inherited operator home, the native-login default, gets the section on its system prompt but not the tree; to give it the static skill, seat it on a Remuda-managed home (a provider, `REMUDA_CLAUDE_CONFIG_DIR` or `claudeConfigDir`), or let the operator install the skill in that home themselves. Writing the tree into the codex shell-pty shadow home is a later option.

### 3.5 Watching in Phase 1

Until Phase 2, the seat watches by bounded polling of Hub reads from inside its own turns: the worker roster and observe, worker journals for DONE/BLOCKED, and gate jobs. If the owner sends a message while the seat is in a long turn, the carrier's existing new-turn/steer/queue behaviour delivers it (D-055 modes). The exit evidence records the observed delay; this document does not promise one. The agent may use its harness's own scheduling tools where they work, but they do not survive a restart. Phase 2 replaces polling with facts and durable wake-ups.

### 3.6 Memory in Phase 1

- Work memory is the Hub: tasks (D-050), the roster, gate jobs and interactions. Task state never goes into memory; the task ledger already holds it.
- The verbatim archive is the journal of every chapter and every worker. After a restart, the agent reads predecessor chapters with `remuda instance read`.
- Until `cb-memory-core` lands, identity and memory documents are plain files in the seat's persistent workspace, named in its launch brief. They hold no secrets.
- From `cb-memory-core` on, memory is per-entry rows in the Hub (§12, D-060 R1). This is not deferred to Phase 3 and does not wait for the fence: it depends on no `ma-*` task. `cb-memory-guard` imports the Phase 1 files once. A file the import cannot read (a permission or I/O error) is an error and deletes nothing; only a missing file counts as empty.
- Remuda memory is the seat's only durable memory. The skill (`cb-skill`) tells the agent not to use its harness's native auto-memory: native memory is keyed by host and working directory, so it splits across chapters and Nodes, which is failure A6 again.
- The identity document stays a workspace file until Phase 3 (§14.3).

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

### 4.2.1 Implementation notes (ma-admission, 2026-11-01)

Interpretations the implementation made, for reviewers and later tasks:

- **One rule, one module.** "Never beyond the creator" lives in `remuda_protocol::creator_permission`, which the Hub (`restrict_permission`, `admit_agent_mode`) and the Node (`permission_plan`, the claude print/sdk pre-check, the generic-pty Agent downgrade, `merge_yolo_argv`) both read. A harness's vocabulary is its canonical wire words (Claude `manual`, `auto`, `acceptEdits`, `plan`, `dontAsk`, `bypassPermissions`; Codex `untrusted`, `on-request`, `never`; Grok `native-prompt`, `auto`, `always-approve`; Agy `native`, `accept-edits`, `plan`, `always-proceed`). An Agent's requested word must be one of them; spelling aliases are not admitted for Agent requests. Inheritance copies the creator's word only when it is a word of the child's vocabulary, so `auto` passes to Grok and `plan` to Agy, while Claude `acceptEdits` does not become Agy `accept-edits`.
- **The creator's mode** is the creator chapter's transcript-observed `permissionEffective.mode` when its Node reported one, else the mode its launch ran for the recorded `permissionMode` (the Node's own spelling map, `launched_mode`, which `native.rs` now also uses). A Human seat seated with an alias such as `bypass` therefore reads as `bypassPermissions`.
- **The stamp and the commit point.** Admission (before any allocation for dispatch and respawn) writes the creator's mode into the spec; the insert's writer job (after the worktree or pool lease) re-reads it with the same function and aborts with 409 `creator-mode-changed` when it differs. The generation re-check is `check_initiator` in the same writer job: a superseded or fenced creator chapter is refused there. The forward stamps `creatorPermissionMode` on the top level of the create/resume params from the committed instance row and strips any payload copy; a Human or Bot create carries none. The Node reads only that top-level field, and an absent stamp reads as a creator that keeps its control on. "The Node refuses a stale stamp" is realised as: the Node never admits an Agent control-off launch without a control-off stamp, and its fence refuses work from a superseded creator generation; the Node has no independent source for the creator's current mode, so staleness itself is decided at the Hub's commit point.
- **Yolo argv.** `merge_yolo_argv` merges only the Claude row's `--dangerously-skip-permissions`, for exactly `bypassPermissions`. The Codex, Grok and Agy control-off modes reach their CLIs through the materializer's own permission flags, which `permission_plan` gates by the same rule, so no harness gets a second, conflicting yolo flag.
- **Lineage subtree.** Worker mutations require the worker's instance, or an ancestor reached over parent edges, to be a chapter of the caller's lineage (`lineage_subtree_owns`). This is a superset of `owns()`'s one hop: a seat also reaches the workers of a sub-coordinator it created.
- **Approval before allocation.** For an Agent dispatch the D-017 request is `{operation: "worker.dispatch", projectId, hostId, harness, driver, model, permissionMode, briefDigest, taskId, name}`, with the resolved host, driver, model and effective mode. A placement outside the seat's scope is refused before the owner is paged. `worker replace` decides the replacement's approval before it retires the old worker. `worker answer` asks for every Agent use; `worker switch-model` does not (its keystrokes are Hub-composed, not caller-chosen).
- **Respawn.** An Agent caller, or any caller resuming a worker that has a parent instance, uses the scoped helper; a Human or Bot resume of a parentless worker keeps the D-026 path. The helper re-admits the kept effective mode under the caller's current mode, so a control-off worker is not respawned control-off under a seat that now keeps its control on. A Claude worker with a native session inside the D-026 age window resumes it; otherwise the respawn is a fresh launch in the same worktree.
- **OA3** applies to questions and plan reviews only; an elicitation keeps the D-051(b) bypass exclusion, and approvals never route.
- **Not covered here.** A continuation successor (§5) of an Agent-created chapter carries no creator stamp, so the Node refuses a control-off successor launch (fail closed); re-admitting a successor's mode belongs with `ma-restart` and `ap-resume`. A permission-mode change by `instance.configure` stays outside Agent reach until `ap-configure-mode`.

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
- **Host loss is contact loss, recorded with two marker spellings (2026-10-10, ma-lineage r7 item 5(b) / r8 item 1).** When a host stays unreachable past grace, the sweep touches only chapters that reached a live lifecycle and writes `lifecycle=exited` with `lastError=host-contact-lost` and NO `ended_at`: the process may still be running (D-019), so the row is potentially live and revives through the existing return-to-live paths — a Node journaled instance entity reporting a live state (`ready`/`running`/`starting`, applied in `apply_instance_projection` (then `apply_instance_lifecycle`) in the journal append; the projection deliberately KEEPS `lastError=host-contact-lost` as contact-loss history and clears only `ended_at`), or the SSH daemon reconcile reporting `RUNNING` in the same epoch (`reconcile_daemon_instances`, ssh_hosts.rs, which revives the row and clears `ended_at`/`lastError` — the only path that clears the marker). (A same-epoch hello-inventory revival, `revive_host_lost_instances`, arrives with c-cardsettle after this lands.) The default `GET /v1/instances` list hides only rows that are BOTH terminal and marked (r9 item 4), so a return-to-live event brings the chapter straight back even though its marker stays; `includeHistory=true` shows the terminal-and-marked rows. Rows swept by older builds carry the legacy spelling `lastError=host-lost`; the one-time `PRAGMA user_version` v2 migration stamps their `ended_at` from `updated_at` (they keep their old ended meaning), but the list filter hides BOTH spellings unless history is requested, and the web labels both 主机失联. The sweep never rewrites a `requested` row (that is the separate create-never-acknowledged reaper) or a terminal `failed` launch row.

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

#### 6.3.2 Implementation notes (ma-restart, 2026-11-01)

Where the text above left a choice open, `crates/remuda-hub/src/restart.rs` took these:
- **Causes are spelled** `process-exit`, `start-failed` and `node-restarted`, on the decision and as the successor's `chapterCause`. `node-epoch-changed` on the chapter decides `node-restarted` whatever the lineage state; otherwise an end while the lineage's stored state is still `starting` (no running report from the current chapter) or an attested launch-failure marker is `start-failed`; any other end is `process-exit`.
- **Stored state.** The lineage moves `starting` → `running` in the journal append transaction when its current chapter at the live generation reports `ready` or `running` (a native session `started` counts); the boot migration lifts lineages whose current chapter already runs.
- **The decision log** is `lineage_restart_decisions`, keyed by the failed chapter: cause, outcome (`restarted`, `restart-cap`, `process-exit`), successor, recovery source (`chapter-session`, `earlier-session`, `fresh`), the create and notice command ids, the fence id, and `decidedAt`. `GET /v1/lineages/{id}` returns it as `restartDecisions`. Inside the deciding transaction, before F, the supervisor re-checks that the failed chapter is the current chapter at the expected generation, unfenced, still carries process-end evidence, the lineage is not paused, and no row is keyed by it.
- **The cap** counts `restarted` rows of the lineage whose `decidedAt` is within the last hour. Owner Resumes are not restart decisions and do not count.
- **First seen and the boot rule.** `instances.end_seen_at` records, on the Hub's clock and once, when the Hub first held process-end evidence for a chapter; a trigger writes it on every path that records an end (contact loss is not an end), and the supervisor stamps any end the trigger missed. `journal.observed_at` is not used: the append path stamps it at insert, so it never ages. The tick also refreshes a persisted run marker (`hub_run_marker.last_alive_at`, every 15 s, whatever the supervisor setting). At boot, before this run writes its own marker, an end first seen more than 30 minutes before the previous run was last alive, or with no first-seen record (history from before the upgrade), is decided `stale-at-boot`: no F, no fence, no successor, no push; the owner can Resume it. A death seen in the last 30 minutes of the previous run's Hub time is decided normally, and so is an end that arrives after boot with a reconnecting Node's journal. With no marker at all the boot clock stands in for it; a marker that cannot be read or parsed is logged as an error and that boot classifies nothing as stale, rather than guessing.
- **A paused lineage is never restarted**: the supervisor's F starts only from `starting` or `running`. `paused{restart-cap}` and `paused{process-exit}` carry `{kind, instanceId, cause}`, and close nothing (the process already ended).
- **A successor that cannot be prepared: bounded retry, then a visible pause (r3).** `prepare_continuation` returns a typed error that carries the original refusal. Permanent: the via route names a proxy host the Hub no longer knows (`api-via-unknown-host`), on either the chapter's observed route or the profile's own via delivery, or the chapter's own host row is gone; the supervisor decides `paused{process-exit}` at once, with the reason on the decision and the C1 push "Main paused: could not restart" (kind `restart-failed`), since the generic exited push is replaced for this lineage. Everything else is retryable, a missing provider profile included (a profile re-created in time restarts the seat), as are store and lookup errors and an offline via host: no row and no fence, a retry on every tick (one warning per chapter per minute) until `end_seen_at` plus 30 minutes, then the same paused decision and push, never a silent expiry. The same bound covers any error that stops a decision before or after preparation (loading the candidate, counting the hour's restarts, the host and recovery-source lookups, the decision's own write): inside the window the next tick retries; once it has closed, the pause is decided from a fresh read with that error as the reason. The owner Resume answers with the original refusal, exactly as before `ma-restart`: `api-via-unknown-host` stays 400 with its code, and a missing provider profile stays 422 `PLACEMENT_UNSATISFIABLE` from `resolve_and_attach`.
- **Launch authority.** At create, the lineage's origin spec reference records the seating device (`deviceId`) and that device's authenticated origin (`seatedOrigin`, always `human`, since only a Human may set `restart`). The successor's create carries `seatedOrigin`, never the create spec's own `origin` key; a lineage seated before the record existed is Human-seated by construction. The fence actor is `{by: hub-supervisor, cause, seatedBy}`.
- **Owner Resume of an unpaused lineage** whose current chapter has process-end evidence (§7.9.1) stays possible in the window before the supervisor decides; it and the supervisor race on the generation CAS, and the loser decides nothing.
- **The restart notice** is an `instance.send` to the successor, queued inside the restart's F (origin `hub` on the envelope and the input, the successor as initiator, no device id). The supervisor forwards it once the successor's host has no unacknowledged fences, advertises `lineageFence`, is live, and has accepted the successor's create. Just before it is sent, its text is re-rendered from the fence record (`render_notice`); once its forward intent commits the text never changes. A host without `lineageFence` gets nothing: the row settles `rejected(node-lacks-fence)`. A notice whose successor create was rejected or withheld for a reason other than a fence can never follow it; each evaluation first settles such a notice `cancelled`, reason `successor-create-rejected`, before any new F could cancel it as fenced.
- **C1 pushes** are recorded per fence in `lineage_c1_pushes`, inside the F that owes them, and sent through the existing fanout after commit. A row is marked sent (`sentAt` and the recipient count) only after the fanout delivered it or found nothing to deliver to (push off, a device following, no live subscription); a failed fanout leaves `recipients` NULL and the next evaluation retries it, including the boot evaluation after a Hub restart. A crash between send and mark resends it, and the lineage's push tag replaces the first notification. The seat (the chapter holding `address-owner`) is "Main"; any other continuity lineage is named by its title. A lineage with `restart.onProcessLoss` gets no generic "Session exited" push.
- **Test configuration.** `HubConfig::restart_supervisor` is on by default and off in `HubConfig::for_test`, so suites that end continuity chapters on purpose stay deterministic; the C1 suite (`crates/remuda-hub/tests/restart.rs`) turns it on or drives one evaluation with `HubTestHandle::test_supervise`. The `restart-read` continuation hook fires after the evidence read and before any writer job.

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
- **Root turn** (D-060 decision 1, `cb-rootturn`). The initiator also carries `rootTurn {commandId, origin, continuation}`: the root turn the instance was in when the Hub admitted the call. The Hub derives it from its own command rows and the journal it has ingested (protocol.md, "根回合记录"), never from a request body. It is written on command rows, audit rows (an Agent's child dispatch writes `instance.dispatch`), the forwarded Node frame and the Node's command journal rows, gate jobs, the Hub's one-shot approval cards and memory writes. It is never a gate: no admission, grant, approval or fence rule reads it, and a test proves the same calls get the same decisions with and without it.
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

### 7.9.1 Implementation notes (ma-fence, 2026-10-31)

Where the text above left a choice open, the implementation (`crates/remuda-hub/src/fence.rs`, `crates/remuda-node/src/fence.rs`) took these:
- **Generations stay consecutive.** Every F that fences a chapter advances G to G+1. Resume of a paused lineage fences nothing new (the pause's F already did), so it does not advance again: the CAS moves `paused` at G to `starting` and the successor takes generation G. Chapters therefore number 1, 2, 3, … and the restart notice's "chapter 3" is the chapter's generation.
- **`paused{process-exit}` before the supervisor.** A current chapter with process-end evidence counts as paused for Resume and for `DELETE`; any other unpaused lineage gets 409 with `reason: "not-paused"`. Since `ma-restart` the supervisor records `paused{process-exit}` (or restarts) within a tick, so this only covers the window before its decision (§6.3.2).
- **The pause's close is Hub-internal.** F queues the `instance.close` of the current chapter in the same transaction, without an initiator (the close follows the fence; neither the send gate nor the Node fence refuses it), and forwards it once that host acknowledged the fence. The command id and idempotency key are the caller's, so a retry by either replays that close, even when the retry addresses an older chapter of the lineage.
- **Fence record.** `fences` holds one row per F, `fence_deliveries` one row per host known at F, and `fence_ops` one row per unfinished operation of the fenced generation, keyed by `(fenceId, kind, opId)` and stamped with the op's own initiator generation. An op is recorded under exactly one fence, and an acknowledgement settles only `unknown` rows of the fence it names, so a late result about an old chapter never settles a newer one. A command row's own later settlement refines its `unknown` op on read (`completed` → ran, `rejected(fenced)` → cancelled-by-node); nothing else turns `unknown` into "not executed". `GET /v1/lineages/{id}` returns the fences with `pendingHosts` and every op's outcome.
- **The unacknowledged window.** The hold is decided in the same writer job as the forward intent. Rows held this way are forwarded after the host's acknowledgements (an in-memory list; after a Hub restart they fall back to the same-id retry). A host without `lineageFence` keeps its delivery rows pending but is not held: it never receives continuity work (409 `node-lacks-fence`), and the rows are delivered once it advertises the capability.
- **After a hello** the host's unacknowledged fences are delivered in creation order before any work reaches it: the ordering guarantee is the hold above (no forward, host-offline for direct operations, no lane claims until every fence is acknowledged). Host-level frames that carry no initiator — the API egress reinstall, read-only Landing probes — keep their own post-hello order and may precede the fence frame.
- **A failed acknowledgement** (an error reply or no reply within the bound) leaves the host's delivery pending rather than dropping the link: forwarding to that host stays held, and the next hello or the 1 s scheduler tick re-delivers, in creation order.
- **Node persistence.** The Node keeps fences, each instance's lineage and generation (from create/resume `lineage`), and the direct-operation log in `lineage-fences.sqlite` beside its workspace registry (the data dir), loaded before it serves anything. The dispatch claim that writes the durable `transport-written` marker and the fence's cancellation pass run under one lock, so a command is either written or cancelled, never both.

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

### 8.8 Implementation notes (ma-push, 2026-11-03)

Where §8.1–§8.6 left a choice open, `crates/remuda-hub/src/push_route.rs` took these:
- **Routing at fire time.** The alert's dispatch decides in one writer transaction that also writes the suppression row, so it is totally ordered against every lift. Condition 1 is a create-time parent edge naming an instance outside I's own lineage. Condition 2 is `interactions::routed_to_parent_conn` (the D-051 predicate, switch, bypass and plan-review rules included) for the kinds `question`, `plan-review` and `elicitation`; every other kind (approval, a legacy `permission` row, an unknown kind) pages. Condition 3 reads: lineage state `running`, its current chapter at the live generation, unfenced and not `exited`/`failed`/`closed` (a host-contact-lost row does not count as running), on a host whose row is `online`. The interaction must still be pending. A store error pages.
- **Suppression row.** `push_suppressions`: interaction, child instance, kind, lineage, chapter, generation, rule (`routed-to-running-parent`), `suppressedAt`, and `liftedAt`/`liftCause` once lifted. It is the §8.6 audit row. A repeated alert for an unlifted row stays suppressed; for a lifted row it pages.
- **The one transition function** is `push_route::transition(conn, Transition)`. Its callers, each in the cause's own writer job: `fence_core` (every F); the supervisor, for each candidate before it decides (a stale-at-boot decision or a retried preparation runs no F); the journal append, when a lifecycle event lands on a lineage's current chapter (`ChapterEnded`, §8.2); `mark_host_offline_rows` (the WS session end, the SSH carrier loss and host removal); `expire_lost_hosts`, once per host past the grace; boot step 2 (`Boot`); and every hello, after its inventory reconcile (`Hello`). A structural unit test checks every call site. Each cause names its scope (the lineage, the chapter's lineage, or the lineages whose current chapter is on the host), so the boot-time mark-all-offline bookkeeping lifts nothing. Hooks run in a savepoint: a push bookkeeping error never fails an F, a link loss or an append.
- **The lift** covers still-pending suppressed interactions only; one answered or settled meanwhile is left as it is. Each lifted row inserts the outbox row `lift:<interaction>` (`INSERT OR IGNORE`), so a second transition adds nothing.
- **Boot.** "Not running by state" (step 2) is condition 3 without the host link. Step 3's hello keeps P running only for the same Node epoch with the chapter in the hello's live inventory (a same-epoch hello without it lifts too). The grace path is `expire_lost_hosts` at boot and on the tick, under `host_lost_grace_ms` measured as today.
- **Delivery.** The writer loop wakes the outbox drive after the committing job; the 1 s tick retries; boot drives once before and once after its lift. A row is marked delivered after the fanout delivered it or found nothing to deliver; a failed fanout leaves it for the next drive. A lift row whose interaction is no longer pending at send time is marked delivered without a push. The lift resends today's interaction push: tag `interaction:<id>`, landing `/sessions?focus=<id>`.
- **Per device (§8.4).** `alerts::Followers` records each follow socket (device, instances, visible; a socket starts visible). The frame is `{type:"visibility", state:"hidden"|"visible"}`; the PWA sends it on every per-instance follow socket (journal and terminal) when the socket opens and on `visibilitychange` (`web/src/lib/followVisibility.ts`). A fanout skips only the subscriptions of devices with a visible follow of the instance, for every push kind.
- **Reply-ready (§8.5).** In the append transaction, a root user input (role `user`, `open`, no parent tool call) naming a command whose origin the root-turn ledger classifies Human arms one tracker for that input on the `address-owner` instance. The settled turn end (activity busy → idle, the same edge the seen mark uses) takes the tracker of the input it answered: an end that names its turn (`relatedIds.completedTurn`) takes only that input's tracker, so a Human input that lands mid-turn waits for its own turn; an end that names none takes every armed tracker. On the live `address-owner` chapter (current, live generation, unfenced) it then queues the outbox row `reply:<instance>:<seq>`: title "Main replied: <first non-empty line of the last root assistant message>" (80 characters, then "…"), tag `instance:<chapter>`, landing `/s/<chapter>`. A turn reached only by Hub or Agent input, or by no command, arms nothing.
- **End pushes (OA6).** The exited push needs process-end evidence on the instance's durable row: a bare `exited` marker or a subagent's exit pushes nothing. A store error while reading that evidence fails open: the push is sent and the error logged by category, never silently dropped. A C1 push is sent only once its fence's predecessor chapter has that evidence, so "Main closed itself" follows the end its close brings; the exit alert drives the C1 pushes, the supervisor tick as before.
- **Push log (§8.6).** `push_log` holds one row per fanout: category (`interaction`, `lift`, `reply`, `c1`, `exited`, `turn-error`, `waiting`, and `inbox-fallback` for a fact row that fell back to the owner push, §10.4), tag, instance, outcome, counts, and the device ids delivered to and skipped. No title or body; the newest 5000 rows are kept. Log lines carry categories and counts only.
- **Interaction id.** The interaction alert also reads the id at `payload.interaction.id`, where a Node journals it; before, such a push carried an empty id in its tag and landing URL.
- **Scratch push sink.** A Hub built with `test-faults` and started with `REMUDA_TEST_PUSH_ALLOW_LOOPBACK` accepts loopback push endpoints, so a real-condition run can send real encrypted pushes to a local sink. Production builds have no such switch.

## 9 Chat surface

Phase 1:
- **Main entry.** It resolves to the live address-owner chapter. When the lineage is paused, it resolves to the latest chapter of the most recent address-owner lineage, labelled for example "paused by you at T". With no address-owner lineage nothing is rendered.
- **On the home, Main is a card plus an input, not a separate tab** (D-061; [home.md](./home.md) §6). On `/m` and `/sessions`, Main is one card above the "needs you" queue, plus one 问 Main input. On the phone, that input is the 56px compact Composer docked above PhoneNav. On the desktop it is an inline input in the right column at ≥1440, or at the top of the main column below 1440. There is no Main tab, no fifth PhoneNav slot and no pinned list row.
  - **The card.** It shows a status phrase and the first line of Main's latest reply. That line becomes a send echo right after the owner sends something.
  - **The input.** It sends through `POST /v1/instances/{id}/commands` with a commandId, and reconciles an unknown outcome through `GET /v1/instances/{id}/commands/{commandId}`.
  - **Main's own questions.** They sit first in the home queue and count once.
  - **Shared resolution.** `ma-main-ui` and the home card resolve Main in one shared place.
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

### 10.1 Delivery constraints (D-060)

The inbox tasks (`cb-inbox-facts`, `cb-inbox-deliver`) implement these on the Hub writer thread. They add acceptance items to Phase 2; they do not widen its scope. The mechanisms follow Cindy's completion and watch path (`apps/desktop/src/main/maker-ipc/botDelegationService.ts`, `apps/desktop/src/main/bot-autonomy/events.ts`, `apps/desktop/src/main/bot-autonomy/runtime.ts`, `apps/desktop/src/main/bot-autonomy/taskWatch.ts`, `apps/desktop/src/main/bot-autonomy/followUpPrompt.ts`).

1. **Stable dedupe key.** `dedupeKey = kind:instance:stateEnteredAt`. `stateEnteredAt` is the journal time at which the subject entered the state, so the key is the same after a Hub restart.
2. **Persistent occurrence cap.** The inbox table keeps an occurrence count per `(subscriber, instance, kind)` and a cap per 24 hours. The cap is a project setting with default 6. It only stops re-delivering the same fact; it limits nothing the agent can do. The count survives a Hub restart, and the next delivered fact carries it.
3. **Run generation in the fact key.** A fact names the run it is about: the instance and its run generation. A late result of an earlier run is delivered as that run's outcome and never settles a newer run.
4. **Subscriber resolution.**
   - No subscriber, or a subscriber that muted the kind: the fact falls back to the existing owner push (`alerts.rs`) where that kind pages today. The fact is neither held forever nor recorded as consumed.
   - The subscriber is re-checked when the batch is formed, not when the row is written: it is still the parent, it has not muted the kind, and the subject has not been re-parented.
   - **A lineage never receives facts about its own chapters.** Waking yourself only queues behind your own failing turn.
5. **Paused lineages hold.** While the subscriber lineage is paused, its facts are held and enter no retry loop. Resume, or a successor chapter, re-batches them, as F already does (§10, Fencing).
6. **Visible row first.** The batch's hub-origin row (§9, Phase 2) is written before its `instance.send` is forwarded.
7. **Hub voice.** Fact text is written in the Hub's voice, never in the owner's first person. The batch row records the opening tag and the footer template version.
8. **Tool in flight.** A tool call is in flight from its call to its result; in the journal they share a `toolCallId` (protocol.md §5.2), and `cb-stall-tooluse` makes that state available to the stall fallback for every carrier. The Hub picks no number for it:
   - While a tool call is in flight, **no stall fact is produced** unless the project sets an explicit, finite `stallToolThresholdMins`. With it set, one stall fact follows once the call has been in flight that long.
   - Stall timing restarts from the tool result: the ordinary `stallThresholdMins` window (default 30, `DEFAULT_STALL_THRESHOLD_MINS` in `crates/remuda-protocol/src/worker.rs`) counts quiet time from the result, not from the call.
   - **The backstop for a runaway tool is the task's wall budget.** When a task's `TaskBudget.max_wall_mins` (`crates/remuda-protocol/src/task.rs`) elapses, its parent gets one budget-expired fact. Nothing is killed; the parent decides. On origin/main the field is stored but nothing acts on it.
9. **Logs carry categories only.** Delivery logs record the kind, counts and error codes, never fact text or worker output.

### 10.2 Digest and glance (D-060)

A woken seat reads one bounded projection per worker instead of a whole journal. Both routes are read-only and need no migration (protocol.md §8.4). They follow Cindy's digest and glance (`packages/lizi-mcps/src/xdt-helper/get_task_digest.ts`, `apps/desktop/src/main/bot-autonomy/glance.ts`).

- **Worker digest.** `GET /v1/workers/{id}/digest`, CLI `remuda worker digest <worker>`. It has a fixed shape of at most 2048 UTF-8 bytes and is always valid JSON. When it is too long, text fields shrink first; it is never cut into invalid JSON. Fields:
  - the roster state and the observe classification;
  - the latest turn result;
  - the head and tail of the last assistant text;
  - the pending interaction: kind, question, the tool awaiting approval, and a bounded summary of its arguments quoted as untrusted data;
  - the requested and effective model and effort, and the pending state and requestId of `instance.configure`;
  - the cooling-down supply window and its `resetsAt`;
  - the number of queued commands;
  - task, branch and sha;
  - the time since the last progress, and whether a tool call is in flight.
- **Argument summary.** It excludes secrets (the shapes of `scripts/ci/secret-scan.py`) and input-handoff values, and nothing else. Cindy gives no arguments at all, which is its own privacy and size trade-off. Main needs the command text to decide whether to answer for the worker or escalate.
- **Lineage glance.** `GET /v1/lineages/{id}/glance`, CLI `remuda glance`: at most 50 waiting rows and 50 working rows across the lineage's children, plus the number done today.
- **Who reads them.** The PWA board reuses the same projection. Phase 1 polling (§3.5) switches from journal reads to one digest per worker, and a fact batch's footer points at the digest. The middleware admits exactly the digest path shape for Agent origin, as it does for hostcap; the handler requires `dispatch` and project scope, like the other scope-wide worker reads (§4.2 item 1). The glance sits under the existing lineage read predicate. Neither waits for `ma-admission`.

### 10.3 Implementation notes (cb-inbox-facts, 2026-11-02)

Where §10 and §10.1 left a choice open, `crates/remuda-hub/src/inbox.rs` took these. Delivery (batching, the idle gate, the paused hold, the owner-push fallback, visible row first, Hub voice) is `cb-inbox-deliver`'s; this task writes and keeps the rows it reads.

- **Tables.** Idempotent DDL in `hub.sqlite`, so `scripts/hub-backup.sh` carries them whole:
  - `inbox_facts`: subscriber lineage, kind, `dedupe_key` (unique per subscriber inbox), payload, `created_at`, `batch_id` (NULL until a batch carries the row), the subject instance and its run generation, and `state_entered_at`;
  - `inbox_occurrences`: one row per occurrence, recorded or kept out by the cap, for the rolling count;
  - `inbox_subscriptions`: the owner's opt-in and the lineage's muted kinds;
  - `inbox_trackers`: per-instance journal-derived state (run generation, the commands of recent root turns, tool calls in flight);
  - `inbox_sources`: Hub-side state changes queued by SQLite triggers in the writing transaction;
  - `inbox_host_links` and `inbox_supply_parks`: the host link state last announced, and children parked on a cooling supply window.
- **Kinds.** `started`, `exited`, `waiting` (blocked with no interaction the Hub holds), `turn-done`, `turn-failed`, `interaction` (with `routedToParent`), `interaction-superseded`, `gate`, `supply-reset`, `host-online`, `host-offline`, `stalled`, `budget-expired`. `wake-due` and `context-threshold` are named for the wake tasks; nothing records them yet.
- **Sources.** Journal facts are folded in the append transaction (`fold_append`); a replayed append writes none. A process end follows the `end_seen_at` stamp (§6.3.2), which a trigger turns into a source row, so every path that records an end is covered. A gate job's move into `passed`, `failed`, `landed` or `canceled` is queued the same way, whichever of the gate queue's writers made it; the fact goes to the lineage in the job's initiator (a Human's job has none). The writer drains the source rows after every job and once at open; a row whose fact fails is retried on the next job and dropped, by error code, after five attempts. F's step 8 records `interaction-superseded` for each interaction it supersedes. A 429 a child's own journal showed records the park in the same write that cools its family; the reset fact follows once the reset time has passed and no window covering the family is cooling.
- **Host facts.** A drop only stamps the link as lost. `host-offline` follows once the link has stayed down for the host-lost grace (`host_lost_grace_ms`, the window after which its chapters are marked contact-lost), counted from no earlier than the Hub's own start; the next hello after it is `host-online`. A reconnect inside the grace, the boot bookkeeping that marks every host offline (§8.3) and the Hub's own restart announce nothing, and a host's first hello announces nothing. Recipients are the lineages with a live current chapter whose scope covers the host: continuity lineages and opted-in lineages.
- **Keys and time (§10.1 item 1).** `dedupeKey = kind:subject:run:stateEnteredAt`. The subject is the instance, or `instance/interaction` for interaction kinds, the job, the host, `instance/toolCallId` for a tool stall, or the task for a wall budget; a stall's time is when the tool call opened, or when the quiet period started, never the computed threshold time, so changing the project's window mid-stall records no second fact (the threshold is in the payload only). A tool stall's key carries no run generation either (its run segment is always 0): the call and its opened time are the stall, so a run that begins while the call is still tracked cannot make it a second fact; the run it was found in stays in `runGeneration` and the payload (r3). `stateEnteredAt` is the event's `observedAt` for journal facts, the evidence's `ended_at` (else the first-seen stamp) for exits, and the Hub's commit time for Hub-observed facts, always in the fixed-width UTC shape. A re-sent request for an interaction the Hub already holds is not a new fact.
- **Run generation (§10.1 item 3).** The subject's root-turn count: a structured carrier's root `turn_started` records, or a busy edge out of idle on a carrier without turn boundaries. A root result names its turn (`completedTurn`), so a late result of an earlier run is recorded as that run's outcome (`late: true`, with `currentRun`), leaves the instance's state alone and does not count as the newer run's end, whose own result is still recorded. A result naming a turn the tracker's window has dropped is resolved against the durable root-turn ledger; when that cannot place it, it is recorded as unattributed (`unattributed: true`, run generation 0) and settles nothing and clears no tool in flight (r2).
- **Cap (§10.1 item 2).** Per `(subscriber, subject, kind)`, over occurrences whose `stateEnteredAt` lies in the 24 h before the new one's, from the subject's project setting `factOccurrenceCap` (default 6). An occurrence over the cap is counted and writes no row; the next recorded fact carries `occurrence {count, capped, cap, windowHours}`. The count is durable, so it survives a Hub restart.
- **Subscribers (§10.1 item 4, §10 Defaults).** The subscriber is the subject's parent lineage, recorded at write time; `cb-inbox-deliver` re-checks it at batch formation. A parent edge that leads back into the subject's own lineage names none, so a lineage never receives facts about its own chapters. A lineage with a `lineages` row (continuity) or created with origin `agent` receives facts; a Human-created instance without continuity receives them only after the owner opts in. A muted kind is not recorded.
- **Tool in flight (§10.1 item 8).** The run tracker is the single source of tool-in-flight state for every carrier (§10.5 lists what each carrier journals): a call is in flight from its `tool_call` to its final `tool_result`, and the current root turn's end or a process end clears it. A root `turn_started` also drops every call still tracked: a root turn starts only after the previous one ended, so such a call belongs to an earlier run whose result was never journaled, and keeping it would suppress the new turn's quiet-window stall (r3). While one is in flight there is no stall fact unless the project sets a finite `stallToolThresholdMins`; then exactly one per call in flight, each once it has been in flight that long. Otherwise a busy child yields one stall fact per quiet period once no journal row arrived for the project's `stallThresholdMins` (a new project stores 20; `DEFAULT_STALL_THRESHOLD_MINS`, 30, without a project); on a screen carrier that quiet only makes the child a candidate for the screen fallback (§10.5). Both clocks are the Hub's own row times, so the window restarts at the tool's result and a Node's clock or a replayed backlog cannot make a child look stalled. A task's `TaskBudget.max_wall_mins` elapsing since its first worker was created yields one `budget-expired` fact for the dispatcher; nothing is stopped.
- **Time-driven facts.** Supply resets, hosts past the grace, stalls and wall budgets are evaluated by one writer job every 15 schedule ticks: due-time checks over the rows above, each claimed once by its key. The job itself never polls a Node or a screen; the screen fallback (§10.5) reads screens afterwards, off the writer, and only for the candidates the job named.
- **Configured windows are bounded and the tick never panics on one (r2).** `stallToolThresholdMins` is accepted only in `1..=525600` minutes and `stallThresholdMins` and `TaskBudget.maxWallMins` in `0..=525600` (`400` outside it; 0 keeps its earlier meaning, the default window and no wall budget; absent means the default), and `host_lost_grace_ms` is clamped to a year at load. The tick still treats a stored value outside that range (an older build, a hand edit, a restore) as unusable: it skips that subject, logs the category once per process, and uses checked arithmetic throughout, so the Hub's only writer thread cannot be panicked by a configuration value.
- **Logs (§10.1 item 9).** The kind, the outcome (`recorded`, `duplicate`, `capped`, `not-received`), the occurrence count, and for failures a category and the SQLite extended error code; never a payload, worker text or subject id.
- **Reads and settings.** `GET /v1/lineages/{id}/inbox?after=&limit=` and `PATCH /v1/lineages/{id}/inbox` (protocol.md §8.4) sit under the lineage read predicate: a Human device for any lineage, an Agent for its own; a Bot credential is refused. The opt-in is Human-only; a lineage may mute kinds for itself.

### 10.4 Implementation notes (cb-inbox-deliver, 2026-11-03)

Where §10 and §10.1 left a choice open, `crates/remuda-hub/src/inbox_deliver.rs` and `inbox_voice.rs` took these.

- **Tables.** Idempotent DDL in `hub.sqlite`, so `scripts/hub-backup.sh` carries them whole: `inbox_batches` (batch id = the send's commandId, lineage, chapter, generation, host, opening tag, template version, fact ids, how many are listed, the verbatim text, created, the settlement outcome and when the rows were released), `inbox_deliver_due` (lineages whose gate may have opened), `inbox_fallback_pushes` (owner pushes a fallen-back row owes), and two `inbox_facts` columns, `fallback_at` and `fallback_reason`.
- **Triggers, not polling.** The gate is evaluated after the writer job whose committed state change can open it. SQLite triggers queue the lineage in `inbox_deliver_due` in that transaction: a new fact row; the chapter turning idle or live (`instances.activity`, `lifecycle`); an interaction leaving `pending` (or a pending one deleted); a command settling; the one instance of a lineage without continuity ending or being deleted; a lineage changing state, current chapter or generation (pause, resume, a successor); a host coming online. The writer drains the set after every job and once at open (every lineage with waiting rows is marked at open). A lineage whose gate is closed is dropped from the set; only its next state change re-queues it, so a held or paused lineage costs nothing and has no retry loop.
- **The gate (§10, Delivery).** The live chapter is the lineage's current chapter while the lineage is `running` (a lineage without continuity is the one instance whose id it is): not fenced, not ended, `ready` or `running`, at the live generation, activity `idle`. It must have no `pending` interaction of its own and no owner input queued or in flight. "Owner input" is a Human-origin (`origin: human`, no initiator) `instance.send`, `instance.respond`, `instance.keys` or `tty.write` that has not settled. The chapter's own launch and the owner's control verbs (create, close, configure, cancel) hold nothing, because a create that is still `accepted` would otherwise hold the seat's first batch forever. Two more conditions: no unsettled hub-origin `instance.send` to the chapter (its previous batch, or a successor's restart notice, which stays its first input), and its host online and advertising `lineageFence` (§6.4: hub-origin input goes only to such Nodes).
- **Coalescing and the budget.** Every waiting row goes into one batch. The text keeps within 4096 bytes. Facts are listed in order while they fit, and the rest are summarized by count per kind, with the inbox route that lists them. The batch still carries every row, so none waits for a second batch.
- **Ids (§10, D-055).** The command row (`instance.send`, `queued`, origin `hub`, with the Hub-internal initiator `{chapter, lineage, generation}` and no device) and the batch row commit in one transaction, and the facts' `batch_id` is stamped in the same one. The forward is the ordinary `forward_if_online`, from a driver task woken by the writer, by a host's hello, at boot and by a periodic retry check (every 5 schedule ticks, an in-memory look). A row already forwarded is never written again. An unknown outcome (a reply lost after the frame left) stays on its id until the Node's journal settles it, and the Hub never resends it under a new id.
- **A forward that provably did not reach the chapter (r2).** For Hub-written input (a batch or a restart notice, `instance.send` with origin `hub`) no caller holds the id to retry it, so a refusal that proves it never ran does not settle it: the Hub refused it before queuing (NODE_BUSY, the host's 32 in-flight slots full), the link was gone before the frame was queued, or the Node refused it with a JSON-RPC error before recording it. The forward intent is released (`forwarded = 0`, still `queued`) and the same id is tried again: 1 s, 2 s, 4 s, up to 60 s. The retry is armed inside `forward_if_online` before the release commits (r3), whichever path forwarded it: the batch driver, the fence acknowledgement's forward of held work (`deliver_host`), or the restart notice's drive, which also honours the backoff instead of re-forwarding on every tick. A prune drops an entry only once its command has left `queued` (r4): the release arms the entry before its commit clears the forward mark, so a prune that reads in between keeps it. The periodic check (every 5 s) drives any unsent batch whose backoff has passed or that has nothing armed at all, provided its host is live and it is not held behind the host's unacknowledged fences or behind input that goes first; restart notices whose successor's create was accepted get the same check (r4). So an unsent send with nothing armed is still forwarded, whatever interleaving lost its entry, and held work is left to what releases it. The supervisor tick also drives notices. A fence still cancels a batch, which releases its rows. Every unsent send ends (r3): after ten attempts (about five minutes) it is settled `rejected` with reason `not-delivered`, a batch's rows go back to waiting, and that category is logged once per process. The attempt count is kept on the command row (`commands.hub_attempts`, r4) and incremented in the release's own transaction, so neither a lost backoff entry nor a Hub restart resets it. The tenth attempt settles the send in that same transaction, and the in-memory entry changes only after the commit. A Node answer of "instance not found" is terminal for the chapter: the send is settled `instance-not-found`, never retried, and that chapter takes no more batches (a lineage without continuity is then gone, a continuity lineage keeps its rows for its successor). Before r2, NODE_BUSY settled the batch `rejected` with `forwarded = 1`, which kept its rows and every later batch of the chapter back for good.
- **Owner input that arrives before the forward (r2).** The owner-input and Hub-input conditions of the gate are checked again in the writer job that would mark the batch's forward intent (`ForwardMark::HeldBehindInput`), so owner input committed after the batch formed goes first even when it races the driver. The batch stays queued under its id, and the settling of that input re-drives it: the command-settle trigger also queues a lineage whose formed batch waits unsent, and that evaluation wakes the driver.
- **Hold while a batch is in flight.** The chapter's next batch forms only after the previous one settled. Known limit: a batch whose outcome the Hub can never learn (the frame was lost and the Node holds no record of it) holds that chapter's later rows until the chapter changes (Pause and Resume, or a restart). The rows stay readable in the inbox route meanwhile. The Hub picks no deadline, because a lost ACK must not be read as "not delivered" (§7.4).
- **Released rows (§10, Fencing).** When a batch's command settles, a trigger records its outcome. If the settlement proves the send never reached the chapter, the trigger hands the rows back (`batch_id` cleared, `released_at` set), and they form a new batch, under a new id, when the next chapter's gate opens. That proof is one of four things: the row was never forwarded (F's step 4 cancels it), the send gate withheld it, it settled with reason `fenced` (the Node refused it at entry, or its fence cancelled it before delivery), the Node settled it with `execution: not-dispatched` (its queue was full, or it was interrupted before the driver saw it), which the Hub records on the command row (`settlement_not_dispatched`), or the Hub ended it `not-delivered` (its attempts spent, or its lineage gone) or `instance-not-found`. Any other outcome keeps the rows with that batch: at most once per batch.
- **Re-check (§10.1 item 4).** At formation, a row of a kind the lineage has muted since the row was written falls back with reason `muted`. For the kinds routed by the parent edge, a subject whose `subscriber_of` is now another lineage falls back as `re-parented`, and one with none falls back as `no-subscriber`. The kinds not routed by the parent edge are not re-checked: a gate fact goes to the job's initiator lineage, a host fact to every lineage whose scope covers the host. A lineage without continuity whose one instance ended or was deleted will never take a batch, so its waiting rows fall back as `no-subscriber` (a trigger on that end queues it) instead of waiting forever; a batch still queued for it is ended `not-delivered` in the same transaction first, so its rows fall back with them, once (r3). A continuity lineage whose chapter ended keeps its rows for Resume or a successor. A fallen-back row keeps no batch id and is never offered to a batch again.
- **Which kinds page today.** `alerts.rs` pages the owner for every interaction request ("Need your input"), for an exit ("Session exited"; on a lineage that restarts on process loss the C1 push replaces it), and for an instance left in `waiting-interaction` for 30 s ("Still waiting"). It pages a turn error only for the legacy `turn_done` event with `isError`, which no current carrier journals, so a structured `turn-failed` row does not page. Since ma-push (§8.8) an interaction routed to an effectively running Agent parent is suppressed at event time, and an exit pages only with process-end evidence. Gate, host, supply, stall, budget and wake facts never paged. A fallen-back `interaction` row pages while its interaction is still pending, `exited` unless the C1 push replaces it, and `waiting` while the subject is still blocked; any other row pages nothing. The page uses that alert's own title, body and tag through `alerts::page_owner`, logged in the push log under the category `inbox-fallback`. A fallen-back interaction whose page §8 owns is not paged here (r3): one with a `push_suppressions` row (unlifted, it is still routed to an effectively running parent and §8.3's lift pages it if that parent stops; lifted, the lift already paged it), or one that §8.1 routing suppresses at fallback time (which writes that row). Otherwise the owner was already paged at event time and, with the same tag, this page replaces that notification rather than adding one. The push is a durable row, sent after commit and marked only once delivered. A failed fanout schedules a retry on the row (1 s, 2 s, 4 s, up to 5 min) that any drive or the periodic check (a cheap EXISTS, then the driver's lock, so one pass sends a row at a time) sends; each failure is counted with a conditional `attempts = attempts + 1` read back in the same writer job, and the eighth gives it up (`gave_up_at`, logged by code), so no row is retried forever.
- **Visible row first (§10.1 item 6).** The batch row is the Hub's visible row. It commits before any forward intent, and `GET /v1/lineages/{id}/inbox` returns it under `batches` for the rows it carried. In the transcript, the delivered text is the Node's user message for that command. Before this task the Node wrote it as `origin: human`, so the PWA drew the restart notice and any Hub input as the owner's bubble. `MessageOrigin` gained `hub`: the Node stamps it for a command whose origin is hub, keeps it on every PTY-queue revision and on the transcript or stdout echo it joins, and the PWA's collapsed system row shows it as "Hub 送达" with the text one tap away. The Owner-thread / All-activity toggle and the per-batch background row with its triggered turn (§9) are left to the web task.
- **Hub voice (§10.1 item 7).** Template v1 opens with `<remuda-facts>`. The header says the Hub wrote it, not the owner, that it is information and authorizes nothing, and how quoted text reads, then names the batch, the lineage and the chapter. One line per fact follows, in the third person. The footer gives a snapshot of the lineage's live children by activity, then `remuda worker digest <worker>` for a dispatched (roster) worker, `remuda instance read <instance>` for any other session (a real-condition seat followed the digest hint with an instance id and got 403, since only roster workers have a digest), and the inbox route, and closes with `</remuda-facts>`. Text from a worker or a tool (an interaction's question, the last assistant text of a finished turn, an error, a branch name, an id that is not a plain token or is shaped like a credential) has its secrets redacted (`digest::redact_secrets`) and is then quoted as `untrusted: "…"`: a JSON string of at most 200 characters in which quotes, newlines, controls, `<`, `>`, `&` and invisible formatting characters are escaped, so it cannot close its quote, start a line, or spell a tag. A turn-done line quotes `payload.tail`, the text that turn ended with, which the fold records when the turn's result is appended (read back to that turn's `turn_started`, redacted, its last 200 characters), so a batch carrying several turns quotes each one's own conclusion. The only texts read are the subject's own; the owner's input to a worker and any task or dispatch text are never read.
- **The skill flag (§3.4.1).** `skillContext.inbox` is true when the instance's lineage receives facts (§10, Defaults) and its host advertises `lineageFence`.
- **Logs (§10.1 item 9).** The formation logs the number of facts, how many are listed, the byte count, the template version and `kind=count` pairs. A hold logs its category; a fallback logs the kind, the reason and whether it paged; a forward logs the command state, or the HTTP status and error code on failure. Logs never carry fact text, worker output or ids.

### 10.5 Tool in flight per carrier and the screen fallback (cb-stall-tooluse, 2026-11-03)

`crates/remuda-hub/src/inbox.rs` keeps the run tracker of §10.3 as the single source of tool-in-flight state; the worker digest's `toolInFlight` (§10.2) reads it too, so the digest and the stall rules cannot disagree. `crates/remuda-hub/src/stall_screen.rs` is the screen fallback of §10.

**Carrier inventory.** What reaches the journal as `tool_call` and a final `tool_result` sharing a `toolCallId` (protocol.md §5.2), per carrier and channel. `crates/remuda-hub/tests/stall_tooluse.rs` replays a recorded fixture through each channel's own mapper into the Hub (every row marked "yes").

| Carrier (driver, agent) | Channel | Start → `tool_call` | End → final `tool_result` | Stall rule |
|---|---|---|---|---|
| `claude-sdk`, `claude-print` | stream-json stdout | yes: the `tool_use` block (streamed `content_block_start`, or the whole assistant message), before the tool runs | yes: the user record's `tool_result` block. A backgrounded subagent's launch result is `partial`; its `<task-notification>` is the final (on the hook channel, `SubagentStop` too) | journal |
| `claude-pty` | transcript tail, bound through the `SessionStart` hook | yes: the assistant transcript record, written when the message completes and before the tool runs; read on the tail's poll | yes: the user record's `tool_result` (an error result too) | journal |
| `shell-pty`, agent `claude` | hook relay (live) plus the transcript tail of the promotion | yes: `PreToolUse`, before the tool runs; the transcript's copy names the same call | yes: `PostToolUse`. `PostToolUseFailure` is not registered, so a failed call is closed by the transcript's error result | screen carrier |
| `shell-pty` or `generic-pty`, agent `codex` | rollout file adapter | yes: `response_item` `function_call` / `custom_tool_call` / `local_shell_call`, written before the call runs | shell commands: `item_completed` `CommandExecution`. A call whose only result record is `function_call_output` gets no `tool_result` (the adapter uses that record only for a call it never announced): it stays in flight until the turn's end clears it | screen carrier on `shell-pty`, journal on `generic-pty` |
| `shell-pty` or `generic-pty`, agent `grok` | session `updates.jsonl` adapter | yes: `tool_call` | yes: `tool_call_update` with a terminal status | screen carrier on `shell-pty`, journal on `generic-pty` |
| `generic-pty`, agent `claude` | `SessionStart` hook only | no | no | journal, no tool state |
| `shell-pty`, agent `agy`, `generic` or `terminal` | none | no | no | screen carrier, no tool state |
| `generic-pty`, agent `agy` or `generic` | none | no | no | journal, no tool state |
| `claude-bg` | job `state.json` | no | no | journal, no tool state |
| `codex-appserver`, `grok-acp`, `agy-print` | protocol mappings exist (protocol.md §5.7) | — | — | the Node's native factory refuses to build them, so no instance runs on them |

How reliable each "yes" is:
- **Subagents.** A subagent's calls, when its records reach the channel, carry `parentToolCallId` and are tracked like any other call. The call that launched it (`Task`/`Agent`) is in flight until its own final result either way, so a foreground subagent run is never stalled for being quiet in the parent.
- **Parallel calls.** Each block or item is its own `tool_call` with its own result; the child is in flight until the last one. A call journaled on two channels (the hook and the transcript of `shell-pty` claude) is one call: both derive the same `toolCallId` from the native id. The tracker remembers the last 64 calls whose final result it saw, so a copy that lands after the result does not reopen the call.
- **A kill mid-call.** No result is journaled. A journaled end (`exited`, `failed`, `closed`) clears every call, and so does an end the Hub records without a journal lifecycle (the `end_seen_at` evidence of §6.3.2). Host contact loss is not an end and clears nothing: the process may still be running the call.
- **Resume and replay.** The Claude channels derive `toolCallId` deterministically in the instance scope, so a re-read record names the same call. The codex and grok adapters allocate ids per adapter process; a re-read rollout journals each call with its result again, as a balanced pair. A call whose result never arrives is cleared by the next root `turn_started` (§10.3), the turn's end, or a process end.
- **Before a channel binds.** An unbound `shell-pty` promotion epoch journals no tool events; the screen rule below covers it. A `claude-pty` session whose transcript is not yet bound journals none either; the journal window covers it.

**Stall rules.** The journal rule is §10.1 item 8 as §10.3 implements it: with `stallToolThresholdMins` set, every call in flight past the window gets its own fact (keyed by the call and when it opened, so a later tick repeats none); otherwise one quiet stall per quiet period. A carrier with "no tool state" cannot report a call in flight: on a screen carrier a silent long call under a still screen yields one stall fact once the window has passed; on a journal carrier (`generic-pty`, `claude-bg`) the journal window decides, so a silent long call yields one stall fact once the window has passed. Nothing invents a window for them; `stallToolThresholdMins` applies only to calls the journal shows.

**The screen fallback (§10).** Screen carriers are the drivers whose Node answers `tty.screen` with a grid, the drivers that implement `Driver::screen_read`: today only `shell-pty`. The Hub derives this from the driver capability `remuda_driver::reads_screen`, which the started-driver tests pin against each driver's real `screen_read`. `claude-pty` and `generic-pty` read their herdr pane through other paths and keep the trait's default (none), so they follow the journal rule.
- The inbox tick names a busy child on a screen carrier as a candidate only when no journaled call is in flight and its journal, and its last observed screen change, have been quiet for the project's `stallThresholdMins`. It records no quiet stall for it itself.
- After the tick, candidates are grouped by host and each host is swept in its own task, each read bounded by the screen timeout `observe_one` uses (`SCREEN_TIMEOUT`, 5 s). A host whose sweep is still running is skipped by the next tick and delays no other host; a host that does not answer ends its own sweep for that tick.
- A screen is compared by a digest of the whole grid. When `remuda_screen::screen_live` finds a spinner line on it (joining a line wrapped across rows), the status region (`STATUS_REGION_ROWS`, the bottom non-empty rows it searches) counts only by the line's signature, never its glyph or printed elapsed time; the rows above count as they are. The first read in a quiet period is the baseline, as `observe_one` takes its first observation: it records the digest, decides nothing, and the screen's quiet counts from it. A later read that finds it changed restarts the quiet period at that read. One that finds it unchanged hands the screen to `classify_screen`, the classifier `observe_one` uses, with the quiet period's start as the last activity; only a `stalled` class records a fact. A DONE or BLOCKED report, a dialog or an ended carrier is not a stall.
- The fact is `stalled` with `cause: "quiet"` and `source: "screen"`, keyed like the journal quiet stall by the quiet period's start, and recorded through the same record path and occurrence cap: one fact per quiet period. The last read is kept in `inbox_screen_reads` (`hub.sqlite`), so a Hub restart neither repeats the fact nor starts a second one for the same period.
- A Node that answers `supported: false` for a screen carrier's instance (an adopted PTY, a dropped one) makes it a journal carrier for that evaluation: the journal quiet fact is recorded, keyed like any other. Any other unreadable screen (host offline, a timeout) records nothing; the host and exit facts cover a dead carrier. A status line `remuda_screen` does not parse as a spinner counts as progress whenever it repaints, so such a TUI is never screen-stalled while it repaints: no fact rather than a guessed one.
- Logs carry the category only: the outcome class, the unreadable category, the error code. Never a screen row.

## 11 Wake-ups (Phase 2)

No existing capability survives a restart: harness cron and monitors are session-only. Phase 2 therefore adds wake-up rows that the agent writes. A wake-up fires at a time, at every interval, or when a condition over facts holds (including context percentage). The agent manages them with a small CLI (`remuda wake add|list|rm`). The writer thread claims each one once, and delivery goes through the inbox. A recurring wake-up that missed occurrences fires once and reports how many it missed. There are no quiet hours, and the Hub chooses no cadence or thresholds. `/healthz` reports the last tick and the delivery age, and returns 503 when the tick is stale.

The subsections below are D-060. Their mechanisms follow Cindy's routine engine and scheduler host (`packages/maker-scheduler/src/routines.ts`, `packages/maker-scheduler/src/routine-engine.ts`, `apps/desktop/src/main/routines/service.ts`, `apps/desktop/src/main/scheduler-host/pre-run-hook.ts`, `apps/desktop/src/main/scheduler-host/runner.ts`, `apps/desktop/src/main/scheduler-host/silent-output.ts`, `apps/desktop/src/main/scheduler-host/notifier.ts`, `apps/desktop/src/main/maker-ipc/usageLimitAutoResume.ts`).

### 11.1 Rows and the run state machine

- `wake_ups`: id, lineage, name (unique per lineage, so `remuda wake add --name` is idempotent), trigger (`at`, `every` or `when`), optional `check` (§11.2), mode (§11.4), `notify` (§11.5), state, next fire time, missed count, creator.
- `wake_runs`: id, wake-up, scheduled time, state, start and end times, check exit code, the inbox batch it produced, and the occurrences missed before it.
- Run states: `queued` → `running` → `success`, `failed` or `skipped`.
- **Interrupted, never replayed.** At boot the Hub sets every `running` run to `interrupted` and does not replay it. The next occurrence is computed as usual.
- **Missed occurrences** fire once with their count, as above. Remuda keeps that count; it does not silently merge queued runs.
- **Busy lineage.** A due run is delivered through the inbox, so it waits for the idle point like any fact (§10). There is no separate busy deferral.
- **Result writes.** If persisting a run's result fails, only the write is retried, never the run.

### 11.2 Optional check

A wake-up may carry a `check`, a shell command that decides whether the model runs at all.
- It runs on a named Node in the lineage's scope, with a **mandatory timeout**: there is no default, because a hung check would stall the wake-up forever. The Node runs `sh -n` on it first.
- Exit 0: the model turn runs, and the check's bounded stdout, quoted as untrusted data, is the run's summary. Exit 2: `skipped`, no model turn. Any other exit, or the timeout: `failed`, with a failure fact to the lineage.
- `skipped` does not refresh the wake-up's "last real run" time. Otherwise a check that tests "time since the last real run" would lock itself out.
- **Admission.** A check is a shell command that does not pass through the harness's own permission control. It is therefore admitted on the same axis as OA1: an Agent may add a check without approval only on its own host, and only if the creator itself runs with its harness's permission control switched off. Any other check, a cross-host one included, needs the D-017 one-shot human approval when it is added. Changing the command needs a new add.

### 11.3 The unattended envelope

Each delivered wake run opens with an envelope in the Hub's voice:
- the fire time, the wake-up's id and name, and the missed count;
- "The owner is not watching. Put open questions in your reply or use `remuda owner notify`; do not ask an interactive question.";
- "End the turn when the work is done; do not sleep-poll."

The envelope restricts no action. Unlike Cindy's, it carries no turn-authority line (D-060 R2).

### 11.4 Fresh mode

`mode: persistent` (the default) delivers to the live chapter. `mode: fresh` makes the Hub create a short-lived child instance under the lineage, briefed with the wake-up's text. The create is an ordinary Agent create made on the lineage's behalf, with its initiator; it passes the same admission (grants, scope, OA1, D-017) as any other, and its facts come back to the lineage.

### 11.5 Silence and `owner notify`

- **Hub-origin turns are silent by default.** Reply-ready pushes only when Human-origin input reached the turn (§8.5), so a fact- or wake-triggered turn pushes nothing by itself.
- **`remuda owner notify --title --body [--dedupe-key]`.** The caller must hold `address-owner`. The Hub writes a claim row first, then pushes. A repeat with the same dedupe key returns the original row and pushes nothing. A crash between claim and push is resent at boot under the same push tag, which replaces the notification instead of adding a second one (as §8.3). The PWA renders it as a "to you" card in the Owner thread.
- **Each wake-up declares `notify` when it is added:** `never`, `failure` or `always`. The Hub does not choose for it. For a reminder, the notification is the point. A failure that repeats while the previous one of the same kind has not recovered pushes only once.
- Exits and turn errors keep paging through `alerts.rs`, and approvals always page (D-017).

### 11.6 Resume after a usage-limit reset (A5)

D-060 R3 (owner-adopted, 2026-10-29).
- **The reset time** is the first available of:
  1. the harness's structured rate-limit event;
  2. a subscription window that is actually full (`resetsAt` in `crates/remuda-hub/src/supply.rs`);
  3. a time in the error text, only when it states its time zone.
- **Only subscription windows wait.** A 429 from an API key or a gateway relay is usually a per-minute rate limit. It creates no wait row, even when it carries a retry time; it produces the usual error fact.
- **`resume_on_reset`** rows hold instance, run generation, supply window, fire time, attempts and state. The writer tick claims each row once. The fire time is `resetsAt` plus 60 s plus a jitter in [0, 30 s). A reset more than 8 days away creates no row, only the fact. The wait survives a Hub restart.
- **The reset fact is always sent.** At the fire time the Hub first writes a supply-reset fact to the parent, whether or not `autoContinue` is on.
- **`autoContinue`** is a per-instance property, default off. The agent may turn it on at dispatch or create, and so may the owner. When it is on, the Hub sends one continuation input with origin `hub` and a persisted commandId: `continue` if the failed turn produced output, otherwise the original input again. The failed turn is already settled as a root-turn failure (OA6), so this is a new command, not a replay under a new id (D-055).
- **Cap.** At most 3 automatic continuations between two pieces of substantive journal progress.
- **Cancel.** Any Human or Agent input to the instance cancels the wait. A switch-model is such an input.
- A remote Node uses only the `resetsAt` from its own supply report.
- Hub-authored input is a new actor, so this needs the `hub` origin (`ma-restart`) and F (`ma-fence`). The continuation carries the Hub-internal initiator of the subscriber lineage's live generation, and F re-attributes a pending row as it re-batches facts.

### 11.7 Implementation notes (cb-owner-notify, 2026-11-03)

Where §11.5 left a choice open, `crates/remuda-hub/src/owner_notify.rs` took these.

- **Who may notify.** `POST /v1/owner/notify` (protocol.md §8.4) takes an Agent holding `address-owner` or the owner's own Human device; the owner can leave themselves a note, and it is the e2e path that needs no seat. An Agent without the grant gets 403 `grant-required` with `next.grant`; a Bot credential gets 403 `address-owner-required`, decided on how it authenticated, so naming the seat with `x-remuda-instance-id` does not help. An Agent call runs `check_initiator` in the claim's writer job: a fenced, superseded or paused chapter writes nothing (409 `fenced`).
- **Bounds and the lint.** A one-line title of at most 120 characters, a body of at most 2000, an optional dedupe key of at most 128 plain-token characters. The shared secret lint (§12.5's shapes) runs over all three before the bounds, so a credential is always refused as one: 422 `OWNER_NOTIFY_SECRET_REJECTED` with `field` and `rule`, never the text.
- **Claim row, then push.** One writer transaction writes the `owner_notices` row (lineage, chapter and generation, the chapter's latest `turn_started` seq, title, body, dedupe scope and key, source, origin, created), an `owner.notify` audit row without the text (its `device_id` is the device the call authenticated as, `hub` for a wake run; the chapter is in its detail), and the §8.3 outbox row `notice:<id>` (`push_route::enqueue`). The outbox sends it after commit under the tag `notice:<id>` (a new `PushTag` kind), title as written, the body flattened to one line of at most 160 characters, landing `/sessions?focus=<id>`; a crash before the send or before the mark is resent at boot under that tag. The fanout names the writing chapter, so per-device suppression (§8.4) skips a device following Main with its page visible. A notice acknowledged before its push went out is marked delivered without a push. The push log records the category `notice`.
- **Dedupe.** The key is unique per scope: the writing lineage, or `owner` for a Human call. A repeat returns the original row with 200 and `created: false`, writes nothing and queues nothing. There is no window: a key names one notice for good.
- **Repeating failures and the wake engine.** `notify_wake(conn, wake_id, policy, outcome, input)` is the call `cb-wake-engine` makes in its run-settling writer job. `policy` is the wake-up's declaration (`NotifyPolicy`: never, failure, always). A success clears the wake-up's failure latches and notifies only under `always`; a failure under `failure` or `always` goes through `notify_failure`, which latches per `(wake:<id>, failure kind)` in `owner_notify_latches`: the first failure claims a notice, a repeat while the latch is set only counts (`repeats`), and `recover` clears it. Different failure kinds latch apart. The engine names the kinds and may pass the run id as the dedupe key so a retried result write claims nothing twice.
- **The cards.** `GET /v1/owner/notices` (`unacked=1`, `instanceId`) and the idempotent `POST /v1/owner/notices/{id}/ack` are Human-only (r2): a notice and its acknowledgement are the owner's, so a Bot credential and an Agent (a narrowed Human token included) get 403 `owner-only`. Relaying notices to a chat Bot is a later, explicit decision (T24). The PWA reads the unacknowledged list once per settled list refresh while a home queue is mounted (`web/src/features/home/notices.ts`) and draws each as a 「告诉你」 card in ④ after 任务待你 and before 回合失败, uncounted (home.md §4.1, §5.1), labelled with its voice (Main, Hub, or 备忘 for the owner's own note). The ④ read asks for the Hub's cap of 200 rather than the default page of 50 (r2). In Main's transcript a chapter's notices, acknowledged or not, are read when the journal grows and on every successful list poll (r2: a device visibly following Main gets no push for the notice, and writing one journals nothing, so the poll is what brings it the card), and hang under the reply of the turn that wrote them (the newest assistant message when that turn has none in view) as a labelled card in Main's voice, never the owner's bubble. 知道了 in either place acknowledges on the Hub; the card leaves ④ on every device after the Hub answered, and stays in the transcript marked 已确认; a read already in flight when the ack landed does not repaint it (r2). A stale queue greys 知道了 (home.md §8).
- **Logs** carry the category and outcome only (`claimed`, `deduped`, `refused-grant`, `refused-bot`, `acked`), never a title, body or key. The tables ride `hub.sqlite`, so `scripts/hub-backup.sh` carries them.

### 11.8 Implementation notes (cb-usage-resume, 2026-11-03)

Where §11.6 left a choice open, `crates/remuda-hub/src/usage_resume.rs` took these.

- **Tables.** Idempotent DDL in `hub.sqlite`, so `scripts/hub-backup.sh` carries them whole: `resume_on_reset` (instance, run generation, the supply window, the source of its reset time, `resets_at`, `fire_at` and the jitter drawn, `attempts`, state and reason, the failed turn's command and whether it produced output, the continuation's commandId, the Hub-internal initiator), `resume_trackers` (per-instance journal-derived state: the current root turn's output, its structured rejection and limit notice, the progress mark, the last failed turn's verdict), and an `instances.auto_continue` column.
- **Where the wait is decided.** In the journal append transaction, before the inbox fold (`fold`), at a failed root `result` (a carrier without turn boundaries: its busy→idle edge, when the turn showed limit evidence). The verdict rides the turn's own `turn-failed` fact as `payload.usageLimit {class, window, resetsAt, source, waits, fireAt, autoContinue}` or `{class, waits: false, reason}`: that fact is the one limit fact; the Hub writes no second kind for the limit itself.
- **The structured event (source 1).** The `claude-sdk` and `claude-print` carriers (and the Claude stream-json journal mapper) now keep `rate_limit_info` on the `rate_limit_event` diagnostic: its status becomes the lifecycle status and `relatedIds` carries `rateLimitStatus`, `rateLimitResetsAt` (epoch seconds), `rateLimitType` (the window), `rateLimitOverageStatus` and `rateLimitUsingOverage` (`remuda_protocol::rate_limit_event`). A `rejected` event in the failed turn is source 1.
- **A full window (source 2)** is a window of the instance's own `native` provider profile that covers its model family, at 100 % with a reported `resetsAt` still ahead. The Hub's backoff `cooldownUntil` alone never counts.
- **Text (source 3)** is the failed turn's `lastError` or the harness's limit notice, read only when it states its zone: an IANA name in parentheses (`resets 3pm (Asia/Shanghai)`), `UTC`/`GMT` with or without an offset, a bare offset, or an RFC 3339 time with an offset. The wall time resolves to its next occurrence in that zone after the append (a date earlier than a day ago is next year's). A dated time resolves to its next occurrence too, so a date and time earlier today is next year's. A time without a zone (`resets 3am`) is never used: that subscription limit makes no row and its fact says `reason: no-reset-time`. Whatever the source, a reset time not after the turn's end writes no row (`reason: reset-passed`): it would fire at once.
- **Only subscription windows wait (`classify`).**

  | credential | evidence | class | row |
  |---|---|---|---|
  | an API key or a gateway: a `gateway` or `direct` profile; with no profile, a session whose harness reported an `apiKeySource` other than `none`; with neither, a host whose Claude inventory says `gateway-native` | any | `api-key-or-gateway` | none |
  | a subscription: a `native` profile, or no profile and a session reporting `apiKeySource: none` (a claude.ai login), or nothing reported | a `rejected` structured event, or the subscription notice ("hit your limit", "session/weekly/Opus/Sonnet limit", "usage limit reached") | `subscription` | yes |
  | a subscription, as above | an API rate limit only (`rate_limit_error`, `API Error: 429`, `Request rejected (429)`, a bare 429) | `api-rate-limit` | none |
  | any | none of these | not a limit | none, and no `usageLimit` |

  The credential is read in that order: the instance's provider profile, then the harness's own report (the `claude-sdk` and `claude-print` carriers now carry `system/init`'s `apiKeySource` on the session-start lifecycle's `relatedIds`), then the host's inventory.

  A retry time in an API-key or gateway 429 changes nothing. Such a 429 keeps its usual `turn-failed` fact, and a gateway's cooling park keeps its existing `supply-reset` fact (§10.3); neither is a wait.
- **Remote Nodes.** The Hub has no notion of which Node shares its machine, so every Node is treated as remote: a wait only uses a reset time a Node reported (its journal's structured event, a window's reported `resetsAt`, the zoned time in its own error text), never a time the Hub derived.
- **The row.** `fire_at = resetsAt + 60 s + jitter`, the jitter drawn uniformly from [0, 30 s) when the row is written and stored on it (a test injects it). `resetsAt` more than 8 days after the append writes no row (`reason: beyond-8-days`). A newer wait of the same instance cancels an older open one (`superseded`), and an open wait suppresses the cooling park's own reset fact for that instance, so one limit yields one reset fact.
- **Firing.** The inbox tick (every 15 schedule ticks, §10.3) claims each due `waiting` row once in its writer transaction: it records the `supply-reset` fact first (`payload {window, resetsAt, fireAt, source, profileId, autoContinue: off | continue | capped}`, keyed by the reset time, so a replayed tick or a restart records none twice), then moves the row to `done` (autoContinue off), `capped`, or `holding`. A `holding` row forms its continuation in the same transaction when the instance is live (`ready` or `running`, not fenced, not ended) and idle, has no pending interaction and no input queued or in flight, its host is online and advertises `lineageFence`, and the initiator lineage is running at the row's generation; otherwise it is tried again on the next tick, and given up (`done`, `expired`) once it has waited 8 days past its fire time. An instance that is no longer live ends the row `done` (`not-live`).
- **The continuation (D-055).** A new `instance.send` row (`queued`, origin `hub`, no device) whose commandId is chosen and stored on the wait in the same transaction. Its text is `continue` when the failed turn journaled output, else the prompt of the failed turn's own command (the root result's `completedTurn`): a send's `input`, or the create's or resume's `initialInput` when the launch prompt started the turn, stamped `origin: hub`; `continue` when the Hub holds no prompt for it. It is forwarded and retried under its id by the delivery driver, exactly as a batch (§10.4): a release on NODE_BUSY or a lost link keeps the id. Its root turn reads `origin: hub`, `continuation: hub` (cb-rootturn).
- **The initiator and F.** The row carries the Hub-internal initiator of the subscriber (parent) lineage's live generation; an instance without a parent lineage uses its own continuity lineage's, and one with neither carries none. F re-attributes every `waiting` or `holding` row of the fenced lineage to the live chapter and generation in its transaction (`fence_core` after step 4, and `resume_paused`); a paused lineage holds the continuation until Resume. A continuation that provably never reached the instance (F cancelled it unforwarded, the send gate withheld it, the Node refused it as fenced or settled it not dispatched) hands its row back to `holding`, and it is formed again, under a new id, with the re-attributed initiator. One the Hub gave up on (`not-delivered`, `instance-not-found`) ends the wait. Any other outcome keeps it: at most one continuation per wait.
- **The cap, and what counts as substantive progress.** Substantive progress is a root turn of the instance that ends (its result, or the busy→idle edge on a carrier without turn boundaries) after it journaled at least one `tool_call` or one non-empty assistant text that is not the harness's usage-limit notice; the failed turn itself counts when it did work before the limit. Progress records a mark (the newest wait row of the instance), and a continuation is formed only while fewer than 3 waits after the mark are `continued`. A continuation whose turn hits the limit again without doing anything is not progress, so a loop stops at the 4th reset: its fact still goes (`autoContinue: capped`), the row is `capped`. A row mark rather than a time, so neither the tick's clock nor the journal's decides.
- **Cancel.** An `AFTER INSERT ON commands` trigger: any `instance.send`, `instance.respond`, `instance.keys`, `tty.write` or `instance.configure` to the instance that the Hub did not write (`payload.origin` other than `hub`: a Human's, an Agent's, a Bot's, or the Hub's on a caller's behalf) cancels its `waiting` and `holding` rows (`reason: input`) in the same transaction, and settles a formed continuation still queued and unforwarded `cancelled` (`resume-cancelled`). A switch-model (`POST /v1/workers/{id}/switch-model`) types into the worker's terminal, so it reaches the instance as `tty.write` rows (`esc`, `esc`, `/model <id>`, then `enter`) and an `instance.send` nudge: its first write cancels the wait. A model change by `instance.configure` cancels too. An interaction answer goes to its Node as a direct `interaction.answer` RPC and writes no command row, so the answer path cancels the wait itself, in the writer transaction that records the answer (`usage_resume::cancel`, r2); every other input route writes a command row. A cancelled wait writes no reset fact. A deleted instance cancels its rows.
- **`autoContinue`.** `autoContinue` on `POST /v1/instances` (any creator: an Agent for its child, the owner) and on `POST /v1/workers/dispatch`; a worker's respawn, relaunch and replace keep the previous instance's value; a continuity lineage's successor copies it. The owner changes it with `PATCH /v1/instances/{id} {autoContinue}` (Human only; an Agent and a Bot get 403), CLI `remuda instance set <instance> --auto-continue on|off`; `remuda instance create --auto-continue` and `remuda dispatch --auto-continue` set it at creation. Turning it off recalls no continuation already formed.
- **Logs.** The class, the source and the outcome (`row`, `beyond-8-days`, `no-reset-time`), the fire plan, the hold category, the continuation's ordinal and kind (`continue` or `original`), and the count of rows given up. Never error text, worker output or ids.

## 12 Memory (D-060): Hub entries, now

D-060 R1 (owner-adopted, 2026-10-29) replaces the earlier Phase 3 design, a whole memory document (Facts, Preferences, Commitments, Lessons) replaced at once under a revision check. Memory becomes per-entry rows in the Hub, built now on the companion track (§14.5), independent of F and of every `ma-*` task. Commitments are not memory: they are D-050 tasks. The identity document and the instruction block stay in Phase 3 (§14.3): one replaceable instruction block in the launch brief names the identity document, which is data and is never injected as instructions.

The entry model follows Cindy's memory store (`packages/maker-core/src/memory/types.ts`, `packages/maker-core/src/memory/storage.ts`, `packages/maker-core/src/memory/manager.ts`, `packages/maker-core/src/memory/fts.ts`, `packages/lizi-mcps/src/memory/consolidate.ts`). Its owner-edit conflict check and write receipts follow `apps/desktop/src/main/maker-ipc/botMemoryService.ts` and `apps/desktop/src/main/maker-ipc/botLearningTracker.ts`.

### 12.1 The store

- **Table** `memory_entries`: lineage, slug, type, title, one-line description, body, revision, origin, source references, writing instance, turn reference, created, updated and deleted times. `(lineage, slug)` is unique.
- **Types:** `user`, `feedback`, `project`, `reference`.
- **Revision CAS per entry.** A write carries `expectedRevision`; a mismatch is a 409 that returns the current revision. A PWA edit and a seat write therefore never collide on a whole document. Conflicts are shown, never overwritten.
- **Sizes.** A body over 2 KiB, or a rendered index over 4 KiB, is accepted, and the response states the actual sizes. A body over 8 KiB is refused. `consolidate` merges several entries into one atomically, in one writer job.
- **Search.** SQLite FTS5 `MATCH` first; the bundled SQLite build compiles FTS5 in (rusqlite `bundled` in the root `Cargo.toml`). When `MATCH` finds nothing, or the query contains CJK characters, the search falls back to `LIKE`. The `unicode61` tokenizer treats a run of Chinese text as one token, and a trigram tokenizer returns nothing for queries under 3 characters.
- **Backup.** Entries live in the Hub's SQLite file, which `scripts/hub-backup.sh` already bundles. A backup and restore must round-trip them.
- **CLI.** `remuda memory write|read|search|rm|consolidate`, and the owner's one-time `remuda memory import` (§12.4); routes in protocol.md §8.4.

### 12.2 Writers and origin

- **The seat** writes with its own Agent credential through the CLI. Only the lineage's current chapter may write; once F lands, the writer job checks `check_initiator` (§7.3).
- **The owner** writes from the PWA (§12.3).
- **`about-me`** is one entry the owner writes; the agent reads it (open question Q1 in D-060, recommended default).
- **Origin.** Every write records the origin of the turn it was made in, plus the writing instance and the turn reference. For an Agent write that origin is `rootTurn.origin` (`cb-rootturn`), and the entry also carries the `rootTurn`; when the Hub knows no turn it falls back to the caller's `InputOrigin`. The owner can then tell a preference written in a turn they started from one written in a turn that worker text triggered.
- **Rules the skill teaches** (`cb-skill`): store on the first clear statement; write statements, not commands to yourself; a `feedback` entry carries **Why** and **How to apply**; use absolute dates; store no logs, code facts or git history; check the index before writing, to avoid duplicates. The Hub enforces none of these: they are guidance, not policy.

### 12.3 Reading

- **At chapter start** the Hub renders the index (each entry's title and one-line description) and `about-me`, framed as data, not instructions. Bodies are read on demand with the CLI.
- **Delivery is new launch plumbing** (`cb-memory-inject`); nothing on origin/main delivers it today. `crates/remuda-driver/src/flags.rs` lists `system-prompt` and `append-system-prompt` in `BANNED` ("must never be emitted or accepted from `InstanceSpec.args`"), and the Hub refuses them in a create body with the same table (`validate_launch_args` in `crates/remuda-hub/src/http.rs`). That ban on caller-supplied prompt overrides is deliberate and stays.
  - **The channel.** The Hub stamps the rendered text and its revision on the forwarded create and resume params as a dedicated field, `memoryContext {rev, text}`. Like `creatorPermissionMode`, it is generated by the Hub and never taken from the request body; a body-supplied value is stripped. It never travels in `InstanceSpec.args`. Create params do not reject unknown fields, so an older Node would drop it silently; the Hub therefore stamps it only for Nodes that advertise `capabilities.memoryContext` in hello, and otherwise records on the lineage that the chapter started without the index (protocol.md §7.2): the chapter's row in `GET /v1/lineages/{id}` carries `memoryIndex: withheld` (`stamped` when the Node got the field) and `memoryStartRev`, and one hub diagnostic in the chapter's journal says why. The revision is the lineage memory revision, which every memory write bumps; the first decision for a chapter is final, so a re-forward of the same create carries the same revision and text. The renderer is the only caller of the memory read interface outside the memory module's handlers (§12.5).
  - **The materializer is its only writer, and emits the flag itself** (the coordinator's ruling: `append-system-prompt` is treated like the `RESERVED` flags, which the materializer emits and callers may not). Exactly what changes:
    - `BANNED` in `crates/remuda-driver/src/flags.rs` keeps both entries. `validate_spec_args` on the Node and its Hub mirror `validate_launch_args` (`crates/remuda-hub/src/http.rs`) still refuse `--system-prompt` and `--append-system-prompt` in `InstanceSpec.args`, with today's errors, which differ by side and stay as they are: the Hub answers HTTP 400 with code `BAD_REQUEST` and the message `native feature disabled: flag --… is prohibited` (`validate_launch_args` maps the driver error to `HubError::BadRequest`; `crates/remuda-hub/src/error.rs`), while the Node refuses with the driver error `DriverError::NativeFeatureDisabled` (`crates/remuda-driver/src/error.rs`). Only the Node's error is `NativeFeatureDisabled`; the Hub's wire code is `BAD_REQUEST` and is not to be changed. These are the errors for the flag token itself, bare or `=`-joined (`--append-system-prompt`, `--append-system-prompt=…`). The two-token spelling `--append-system-prompt <value>` (and `--system-prompt <value>`) is refused one step earlier, by `parse_argv`: it has no value rule for a banned flag, so `<value>` is a positional argument and the error is `DriverError::InvalidLaunchSpec` (`invalid launch spec: positional argv "<value>" is not allowed (prompts are not launch flags)`) on the Node and HTTP 400 `BAD_REQUEST` with that message at the Hub. Both errors stay as they are. Only `BANNED`'s doc comment changes, from "must never be emitted or accepted from `InstanceSpec.args`" to "must never be accepted from `InstanceSpec.args`; the materializer itself emits `append-system-prompt` only from `memoryContext`".
    - The materializer's scan of caller-derived arguments (`is_banned_flag_token` over `extras` in `claude_argv` and `materialize_shell_pty_agent`, `crates/remuda-driver/src/materializer.rs`) is unchanged.
    - New: when the params carry `memoryContext`, the materializer pushes `--append-system-prompt <text>` itself, once, after the caller-derived arguments, on both Claude launch paths: `claude_argv` (claude-print, claude-sdk, claude-pty, claude-bg) and `materialize_shell_pty_agent` (a `claude`-kind instance on the native `shell-pty` carrier, which never reaches `claude_argv`). Each records the pair on the recipe as its own emission (`LaunchRecipe.memory`, `RecipeMemory::Flag`). It is the last system-prompt section, so the prompt cache of everything before it is kept. (ma-skill, §3.4.1: when the seat's capability section rides the pair, it follows the index in the same single value, and a launch with only `skillContext` emits the pair for the section alone.)
    - **The flag is emitted on every Claude process start, a native resume included,** because the system prompt is supplied per process: it is not one of the conversation's messages, and a resumed process that is not given the pair runs without its chapter's index. Claude Code 2.1.265 and later do record it, though: by default the prompt rendered on a conversation's first request (with the text of any system-prompt flag) is recorded in the session and reused verbatim on every later request and on `--resume`, and different `--append-system-prompt` text on a later launch takes effect only after the conversation is compacted (Claude Code CLI reference, "System prompt flags in resumed conversations"; before 2.1.265 any system-prompt flag turned recording off). Since a lineage's continuation chapters are native resumes of one conversation, every chapter would otherwise keep the first chapter's index while the Hub records a newer start revision. So on a native resume the materializer also pushes `--system-prompt-snapshot off` immediately before its pair (recorded as `fresh_prompt` on the recipe), which renders the prompt fresh on every request of that process. It is gated on the pinned binary: the switch exists from Claude Code 2.1.257, so a build that reports an older version, or a version that cannot be read, gets no switch (an unknown flag would fail the launch). A fresh session keeps recording on, which pins the chapter's prompt for the prompt cache. The switch costs one partial cache write per native resume and nothing after it: measured on a real claude-sdk seat (2.1.291), the first request after the resume wrote about 12k cache tokens, and the next requests read the cache as before (about 12 written, 39k read). Launches that carry the switch: `claude_argv` resumes (claude-print, claude-sdk and claude-pty with `--resume <uuid>`) and `materialize_shell_pty_agent` resumes for kind `claude`. claude-bg never resumes natively across chapters (each chapter is a fresh `--bg` session, and its follow-up turns reuse that chapter's own record), so it never carries it. `system-prompt-snapshot` is in `RESERVED`: a caller may not pass it in `InstanceSpec.args`.
    - The guards that scan the finished argv, `refuse_prohibited_argv` (`crates/remuda-driver/src/claude_print.rs`, for claude-print and claude-sdk) and `refuse_bare` (`crates/remuda-driver/src/claude_pty.rs`, for claude-pty and claude-bg), refuse every `--append-system-prompt` today. They change to skip exactly the one pair the materializer records on the recipe as its own emission, and keep refusing any other occurrence (a second pair, a different value, the `=` spelling). `--system-prompt-snapshot off` is not a banned flag and passes them. A `shell-pty` launch runs no finished-argv guard on origin/main (`crates/remuda-driver/src/shell_pty/launch.rs` hands the recipe to the PTY), so its only check is the `extras` scan, which stays; a finished-argv guard added there later takes the same one-pair exemption. `--system-prompt` is never emitted and stays refused everywhere.
    - **Which launch path gets what (exhaustive).** The flag is used only on the materializer paths that build the Claude argv themselves: `claude_argv` and `materialize_shell_pty_agent` for kind `claude`. Every other agent launch gets the framed data block instead: `generic-pty` for every agent kind (a raw PTY whose binary need not be `claude`, so no flag may go into its argv; its arm sets `argv = extras`) and every non-Claude harness. Kinds `terminal` and `generic` get neither: no agent reads their first input, and the text would be typed into a shell or an arbitrary program; the Hub stamps no `memoryContext` for them.
    - **The data block.** The materializer records the framed text on the recipe, and the driver puts it, framed as data, ahead of the first input it delivers (a stdio turn, a deferred argv prompt, or a PTY prompt). A native resume does not send it again: the block is part of the conversation, which the resume carries over. On origin/main only the PTY drivers (`generic-pty`, `shell-pty`) deliver input; `codex-appserver`, `grok-acp` and `agy-print` have no consuming driver, so their block stops at the recipe.
    - **The table.** It covers every arm of the driver match in `materialize_inner` (:239), plus the shell-pty agent branch before it (:229), crossed with every kind the Node admits for that driver (`validate_kind_driver`, `crates/remuda-node/src/runtime.rs:3047`). The Node refuses every other (kind, driver) pair before `materialize` runs, and both entry points, `materialize` (:158) and `materialize_with_token_broker` (:167), go through `materialize_inner` (:174). The table is therefore exhaustive over the match: a new `DriverKind` arm, or a newly admitted (kind, driver) pair, must add its row here in the same change.

The launch-path table referred to above (exhaustive over the match; 18 rows):

| # | Driver | Kind | Match arm in `crates/remuda-driver/src/materializer.rs` (origin/main `c27e0eac`) | Mechanism |
|---|---|---|---|---|
| 1 | `claude-print` | `claude` | the Claude arm (`ClaudePrint`, `ClaudeSdk`, `ClaudePty`, `ClaudeBg`, :240-243); argv built by `claude_argv` (:1356, called at :287) | flag |
| 2 | `claude-sdk` | `claude` | the Claude arm (`ClaudePrint`, `ClaudeSdk`, `ClaudePty`, `ClaudeBg`, :240-243); argv built by `claude_argv` (:1356, called at :287) | flag |
| 3 | `claude-pty` | `claude` | the Claude arm (`ClaudePrint`, `ClaudeSdk`, `ClaudePty`, `ClaudeBg`, :240-243); argv built by `claude_argv` (:1356, called at :287) | flag |
| 4 | `claude-bg` | `claude` | the Claude arm (`ClaudePrint`, `ClaudeSdk`, `ClaudePty`, `ClaudeBg`, :240-243); argv built by `claude_argv` (:1356, called at :287) | flag |
| 5 | `codex-appserver` | `codex` | `DriverKind::CodexAppserver` (:344) | data block |
| 6 | `grok-acp` | `grok` | `DriverKind::GrokAcp` (:369) | data block |
| 7 | `agy-print` | `agy` | `DriverKind::AgyPrint` (:406) | data block |
| 8 | `generic-pty` | `claude` | `DriverKind::GenericPty` (:430), `argv = extras` | data block |
| 9 | `generic-pty` | `codex` | `DriverKind::GenericPty` (:430) | data block |
| 10 | `generic-pty` | `grok` | `DriverKind::GenericPty` (:430) | data block |
| 11 | `generic-pty` | `agy` | `DriverKind::GenericPty` (:430) | data block |
| 12 | `generic-pty` | `generic` | `DriverKind::GenericPty` (:430) | not applicable |
| 13 | `shell-pty` | `claude` | the shell-pty agent branch before the match (:229) into `materialize_shell_pty_agent` (:576) | flag |
| 14 | `shell-pty` | `codex` | the shell-pty agent branch before the match (:229) into `materialize_shell_pty_agent` (:576) | data block |
| 15 | `shell-pty` | `grok` | the shell-pty agent branch before the match (:229) into `materialize_shell_pty_agent` (:576) | data block |
| 16 | `shell-pty` | `agy` | the shell-pty agent branch before the match (:229) into `materialize_shell_pty_agent` (:576) | data block |
| 17 | `shell-pty` | `terminal` | `DriverKind::ShellPty` (:439), the login shell | not applicable |
| 18 | `shell-pty` | `generic` | `DriverKind::ShellPty` (:439), the login shell | not applicable |

- **Mid-chapter changes** do not restart anything. The next input or fact batch is preceded by one line, 「记忆自本章开始已变更（rev N）」, N being the lineage memory revision. It is decided where the Hub queues the input (`Store::queue_command`, in the command's own writer job, against the newest revision the chapter has seen) and applied to the forwarded frame, so it rides exactly one input per new revision whatever the input's origin. A native resume does not inject the index twice.
- **PWA memory page.** Grouped by type and searchable. An edit carries `expectedRevision`; on a conflict the owner picks "use latest" or "keep mine". Under a reply that wrote memory the transcript shows "Remembered: <title>", for every turn, using the instance and turn reference recorded at write time.

### 12.4 Lifecycle

- **Retiring a lineage archives its entries; it never deletes them** (D-060 R1). Archived entries are read-only and the owner can import them into a new lineage. Deleting a single entry is a separate explicit owner action.
- **Derived entries.** `source_refs` point at journal rows. When a journal row is deleted or redacted, the Hub lists the entries derived from it and flags them for the owner (privacy deletion).
- **Phase 1 import** (`cb-memory-guard`) reads the workspace files once. A read failure other than "file not found" is an error and deletes nothing.
  - *The layout.* The Phase 1 files follow the layout of Claude Code's own memory directory, which is where the four entry types come from: `MEMORY.md`, one `- [Title](file.md) — hook` line per entry, and one `<name>.md` per entry with front matter `name`, `description` and `type` (top level or under `metadata:`), then the body. `origin: human|agent` records who the entry came from; a file that records none was written by the seat on its own and imports as `agent`. The title comes from the index line (else a `# heading`, else the name).
  - *The command.* `remuda memory import --from <dir> [--dry-run] --lineage <id>`, owner only. The CLI reads every file before it sends anything: a missing directory or file is empty, and any other read error (permission, I/O) stops the import with nothing sent. A file without a type, or with credential-shaped text, stops it too, naming every such file. Only regular files directly in the directory are read, each up to 64 KiB: a symbolic link (an entry file or `MEMORY.md`), a directory, a FIFO or a device is never opened for reading and is reported as not read, an oversized file likewise, and a source directory that is itself a link is refused, however it is spelled (`link/`, `link/.`, `link//`): it is opened once under `O_DIRECTORY | O_NOFOLLOW` with trailing separators and `.` components dropped, and the root is that handle's own path. Each name is checked with `lstat`, opened with `O_NOFOLLOW | O_NONBLOCK`, checked again on the handle, and its canonical path must sit directly in the canonical source directory.
  - *The Hub half* (`POST /v1/lineages/{id}/memory/import`, protocol.md §8.4) runs in one writer transaction and only creates: an entry whose slug is already there, or whose content another entry already has, is skipped and reported. Nothing is overwritten or deleted, so a re-run changes nothing; `--dry-run` decides the same and commits nothing.

### 12.5 Nothing the Hub enforces is read from memory

This is a structural invariant, not a lint (D-060 R2 rejects any content-policy lint on memory).
- Grants, scope, permission mode, placement, `restart`, the D-051 switch and project policy are read only from instance, lineage and project rows. Cindy keeps the same split: its profile sync never reads policy back from the companion's home folder (`apps/desktop/src/main/maker-ipc/botProfileFolderSync.ts`).
- **The test that proves it** (`cb-memory-guard`):
  - *Behaviour.* Write entries that look like a grant, a scope and a permission mode (for example "the owner gave this seat land on 2026-10-12"). Then assert that `require_grant`, `prepare_create`, `restrict_permission`, dispatch placement and the create path give results byte-identical to a run without those entries.
  - *Structure.* The memory read interface may be called only from the memory module, its HTTP handlers, the chapter-start renderer and backup. A call site anywhere else fails the test.
  - *Where.* The behaviour test is `crates/remuda-hub/src/memory_invariant.rs`: two Hubs set up alike, policy-shaped memory (written by the seat and the owner, consolidated once) in one of them, and every probe compared byte for byte, in that Hub before and after the writes and across the two Hubs once allocated ids and times are canonical. The structure test is in `crates/remuda-hub/tests/memory_guard.rs`: it lexes every production source (comments dropped, `#[cfg(test)]` code excluded), so line breaks, paths, `use` imports and function pointers all count as calls, and a glob import of the memory module (`use …::memory::*`, which would hide a bare call) is itself an offender; it allows exactly the renderer's sites by enclosing function (`forward_if_online` in `http.rs`, `queue_command` in `store.rs`), and flags SQL that reads the memory tables in any crate. Backup (`scripts/hub-backup.sh`) copies the SQLite file whole and calls no interface.
- **Secret lint** runs on every write that stores text: create, update, the consolidate target and the Phase 1 import. It checks the slug, title, description and body against the credential shapes in `scripts/ci/secret-shapes.json`, the one file `scripts/ci/secret-scan.py`, `remuda brief lint` (`lint_brief` in `crates/remuda/src/cmd/brief.rs`) and the Hub (`crates/remuda-hub/src/secret_lint.rs`) all read; `scripts/ci/secret-shapes-corpus.txt` is tested on both sides, so the scanner and the Hub cannot drift. A hit is 422 `MEMORY_SECRET_REJECTED` naming the field and the shape, never the text, and nothing is stored. It does not apply the repository's private-token denylist: that list keeps internal host and user names out of a public repository, and the same names are ordinary facts in the owner's memory.
- **Legitimate facts stay writable.** "The owner granted X land on 10-12" can be stored; it simply grants nothing.

### 12.6 Fresh context (Phase 3)

An agent that wants a fresh context writes a handoff and closes itself. Phase 3 turns that self-close into a fresh chapter; Phase 1 pauses instead. The Hub never opens a chapter on a threshold of its own.

A self-close is refused with 409 and a reason while the chapter has a pending interaction or an in-flight D-017 approval (D-060 decision 10; part of `ma-restart`). This follows Cindy's canonical-replacement guard (`apps/desktop/src/main/maker-ipc/botCanonicalReplacementGuard.ts`).

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

### 13.1 Companion layer budget (D-060)

Additive, like the Phase 1 budget. Each item lands with the task named in brackets (§15).

**State.**
- Hub tables:
  - `memory_entries` and its FTS5 table [`cb-memory-core`]. Per entry: a body of at most 8 KiB, with warnings above 2 KiB and above a 4 KiB rendered index (§12.1). The Hub sets no cap on the number of entries.
  - inbox occurrence counts per `(subscriber, instance, kind)`, and batch rows with their template version [`cb-inbox-facts`, `cb-inbox-deliver`]. The inbox table itself is §10.
  - `wake_ups` and `wake_runs` [`cb-wake-engine`].
  - `resume_on_reset` and its per-instance `resume_trackers` [`cb-usage-resume`].
  - `owner notify` claim rows, unique per dedupe key [`cb-owner-notify`].
- Hub columns: instances `auto_continue` [`cb-usage-resume`]; memory write references (instance, turn) [`cb-memory-ui`].
- Task and dispatch documents gain `requestedBy` inside the existing `doc_json`, with a serde default and no migration: a reference only, `{commandId}` for an owner-origin root turn and `{origin}` for any other [`cb-requested-by`]. The same record rides the approval card the request opened. The owner's words are never stored outside the Hub's command row: a Human read of the approval card source, the board and a task detail is hydrated with a read-time `excerpt` (one line, at most 280 chars, secret-scrubbed) from that row, and every Agent or Bot read, on any route, gets the reference alone. It is never a gate.
- Project policy gains an optional `stallToolThresholdMins` (unset by default: no stall fact while a tool call is in flight) and the 24-hour fact cap (default 6) [`cb-stall-tooluse`, `cb-inbox-facts`].
- On the Node: nothing for memory, digest or glance. `check` runs keep their exit code and a bounded stdout [`cb-wake-check`].

**Wire, all additive.**
- Memory routes under `/v1/lineages/{id}/memory` [`cb-memory-core`].
- `memoryContext {rev, text}` on forwarded create and resume params, stamped by the Hub and never taken from a body; it is not an `InstanceSpec.args` flag. It is sent only to Nodes that advertise the hello capability `memoryContext` (protocol.md §7.2) [`cb-memory-inject`].
- `GET /v1/workers/{id}/digest` (at most 2048 bytes) and `GET /v1/lineages/{id}/glance` (at most 50 + 50 rows) [`cb-digest`].
- Error bodies gain `reason` and `next`; the existing `error` and `code` are unchanged [`cb-errors`].
- `rootTurn {commandId, origin, continuation}` on the initiator, journal rows, audit rows and gate jobs. It is derived by the Hub, never read from a request body, and read by no admission rule [`cb-rootturn`, after `ma-initiator`].
- `autoContinue` on dispatch and create, and the owner's `PATCH /v1/instances/{id}` [`cb-usage-resume`].
- `remuda wake …` routes and `/healthz` tick fields [`cb-wake-engine`]; `remuda owner notify` [`cb-owner-notify`].

New frames and bodies are documented in protocol.md inside `~~~text` fences (§8.4, §9.1), for the same reason as above.

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
- clock health;
- from D-060: the delivery constraints (§10.1), the wake run state machine, `check`, envelope and `notify` declaration (§11.1–§11.5), usage-limit resume (§11.6), the stall window for a tool in flight, and `requestedBy` on approval cards.

Exit:
- A synthetic 429 on a worker yields exactly one fact to its parent within one sweep, and the reset fact after `resetsAt`.
- Restarting the Hub mid-delivery loses nothing and duplicates nothing.
- A wake-up set for T+10 min fires once across a Hub restart.
- Facts arriving during a 20-minute busy turn arrive as one message at the next idle point, after the owner's message sent during that turn has been answered.
- A hung fake Node delays no facts for other hosts.
- Laptop sleep yields a host-offline fact.
- Owner-thread mode on the phone shows only owner turns, replies and "to you" cards.
- The session-only monitors and cron are no longer used.

Exit additions from D-060, each in the gated suite against fake Nodes unless it says otherwise:
- **Dedupe and cap.** The occurrence cap still holds after a Hub restart. A worker that enters waiting seven times within the window yields six deliveries, and the next delivered fact carries the count.
- **Run generation.** A late result of an earlier run does not settle a newer one.
- **Own chapters.** A lineage gets no fact about its own chapters.
- **No subscriber.** With the subscriber muted, a fact of a paging kind reaches the owner's phone and is not recorded as consumed.
- **Pause.** Facts held while the lineage is paused arrive as one batch after Resume.
- **Order.** The visible hub-origin row exists before the batch's `instance.send` is forwarded, and the batch row records its template version.
- **Tool in flight, cap unset.** A worker in a tool call that stays in flight far past `stallThresholdMins` yields no stall fact. Once the tool result arrives and the ordinary window then passes quietly, it yields one.
- **Tool in flight, cap set.** With `stallToolThresholdMins` set, a tool call in flight longer than that yields exactly one stall fact.
- **Wall budget.** A task whose `max_wall_mins` elapses yields one budget-expired fact to its parent, and its worker keeps running.
- **Wake runs.** A Hub restart during a running run leaves it `interrupted` and does not replay it. A `check` that times out yields a failure fact. Exit code 2 runs no model turn and does not refresh the last real run time.
- **Owner notify.** The claim row exists before the push; a repeat with the same dedupe key pushes once; a hub-origin turn pushes no reply-ready.
- **Usage limits (A5).** A subscription 429 with a known `resetsAt` yields exactly one limit fact, then one reset fact after `resetsAt`. A gateway or API-key 429 creates no wait row. A reset more than 8 days away yields the fact only. With `autoContinue` on, the fourth automatic continuation without substantive progress is not sent, any Human or Agent input cancels the wait, and the wait survives a Hub restart.
- **Approvals.** An approval raised in a hub-origin turn is labelled "agent-initiated"; one raised in an owner turn shows the owner's words (`requestedBy`).

### 14.3 Phase 3: durable memory, agent-opened chapters, idempotent writes

Scope:
- the Hub identity document and the instruction block (memory itself moved to the companion track, §14.5, by D-060 R1);
- self-close becomes a fresh chapter, briefed with the identity document, the memory index, a snapshot and predecessor ids; self-close is refused while an interaction or a D-017 approval is pending (§12.6);
- caller idempotency keys on dispatch, create, gate enqueue and land, with intent recorded before acting and reconciliation from Node and git state;
- the gate's `conflict` status passed through with its file list (today `crates/remuda-node/src/gate.rs` folds it into `failed`);
- reviews as child tasks.

Exit:
- A context-threshold wake-up leads to a handoff and a fresh chapter that continues a multi-worker task without losing a commitment.
- A duplicate gate enqueue and a duplicate dispatch from a Human-token session each collapse to one.
- Rebooting the devbox resumes the seat, with worker states arriving as facts.
- An owner edit to the identity document on the phone is reflected in the next turn.
- A self-close with a pending interaction gets 409, and the chapter keeps running.

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

D-060 records two of these as open owner questions: relocating a seat to another host (Q2; it needs memory in the Hub first) and the Feishu owner route as an inbound channel (Q3).

### 14.5 Companion track (D-060)

**Goal.** Memory survives chapters and Nodes (A6), the seat reads bounded projections instead of whole journals, and refusals tell the agent what to do next. The track runs alongside Phase 1 and Phase 2 and needs no `ma-*` task.

**Scope.** `cb-memory-core`, `cb-memory-guard`, `cb-memory-inject`, `cb-memory-ui`, `cb-digest`, `cb-skill` and `cb-errors` (§15).

**Exit**, in the gated suite unless it says otherwise:
- **Memory CAS.** Two writes at the same revision: one succeeds, one gets 409 with the current revision.
- **Search.** A two-character Chinese query, for example 边界, finds its entry through the `LIKE` fallback.
- **Backup.** A backup and restore round-trips every entry.
- **Writers.** A write from a chapter that is not the lineage's current one is refused.
- **Retire.** Retiring a lineage archives its entries and deletes none.
- **Secrets.** A write containing a secret shape is refused.
- **Invariant.** The §12.5 behaviour and structure tests pass.
- **Import.** An unreadable Phase 1 file (permission or I/O error) deletes nothing; only a missing file counts as empty.
- **Injection, on the real launch path: one test per row of the §12.3 table (18).** Each test creates through the Hub for a lineage with entries, so the Node receives `memoryContext`, and asserts on what the launched fake actually received, not on a rendered string:
  - *flag rows* (1-4, 13): the argv the fake `claude` records (`FAKE_CLAUDE_ARGV_FILE`, `crates/remuda-testing/src/fake.rs`) contains exactly one `--append-system-prompt`, after the caller-derived arguments, whose value is the index and no bodies; the launch passes its finished-argv guard (`refuse_prohibited_argv` for rows 1-2, `refuse_bare` for rows 3-4; row 13 has none on origin/main);
  - *data-block rows* (5-11, 14-16): the argv carries no `--append-system-prompt`, and the first input the fake receives begins with the framed block. Where a fake does not yet record its first input, `cb-memory-inject` adds that recording;
  - *not-applicable rows* (12, 17, 18): neither the argv nor the first input carries the index.

  Across rows: a create body with `--append-system-prompt` or `--system-prompt` in `args` is still refused with today's errors, at the Hub (HTTP 400, `BAD_REQUEST`) and on the Node (`NativeFeatureDisabled` for the flag token; the two-token spelling is refused earlier as `InvalidLaunchSpec`, §12.3), and a body-supplied `memoryContext` is stripped. On a real claude-sdk chapter the index is present at start; a mid-chapter edit produces the "memory changed" line before the next input; a native resume does not inject twice.
- **PWA.** The owner edits an entry on the phone while the seat writes the same one. The conflict dialog appears, "keep mine" wins, and the seat sees the "memory changed" line on its next turn.
- **Skill.** A seat that hears a preference from the owner stores it with `remuda memory write`, with Why, How to apply and an absolute date; the harness's native memory directory for that working directory gains no file; a new chapter reads the entry.
- **Digest.** For inputs with multi-byte characters the digest is always valid JSON of at most 2048 bytes. The argument summary is bounded and holds no handoff values and no known secret shape. It shows `resetsAt` for a cooling-down supply and the pending `instance.configure`. On real workers, a digest taken mid-turn, while an approval is pending and after DONE matches the journal field by field.
- **Errors.** Existing clients still parse every error body. The 403 for a missing grant, the approval-required 409 and the out-of-scope 403 carry `reason` and `next`. A seat without `land` that tries to land acts on `next` in its following turn instead of retrying blindly.

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

### 15.1 Companion-layer tasks (D-060)

Each keeps the full gated suite green on its own, like the tasks above. `cb-adr` (D-060 and these documents) comes first.

**Companion track (§14.5).** It depends on no in-flight or unstarted `ma-*` task and is the critical path.

| Task | Content | After |
|---|---|---|
| `cb-memory-core` | `memory_entries` and its FTS5 table, the memory routes and CLI, Agent scope (own lineage only), current-chapter writes, archive on retire, `source_refs` back-references, backup round trip (§12.1, §12.2, §12.4) | `cb-adr` |
| `cb-digest` | `GET /v1/workers/{id}/digest`, `GET /v1/lineages/{id}/glance`, `remuda worker digest`, `remuda glance`, generated web types (§10.2) | `cb-adr` |
| `cb-skill` | a memory section in `skills/remuda/sections/`, "no native auto-memory", "the engine is not the identity", when to delegate, "read before retrying an unknown outcome". It merges into `ma-skill` if that is dispatched first | `cb-adr` |
| `cb-errors` | `reason` and `next` on error bodies, CLI rendering of `next.hint` (protocol.md §9.1); no new refusal path | none |
| `cb-memory-guard` | the secret lint, the §12.5 invariant tests, the one-time Phase 1 import | `cb-memory-core` |
| `cb-memory-inject` | new launch plumbing: the Hub renders the index and stamps `memoryContext` on create and resume params for agent kinds; the mechanism per launch path is the §12.3 table (flag on `claude_argv` and `materialize_shell_pty_agent` for kind `claude`; the framed data block on `generic-pty` and for every non-Claude harness; nothing for `terminal` and `generic`); `BANNED` and the spec-args checks are unchanged, and the finished-argv guards (`refuse_prohibited_argv`, `refuse_bare`) skip only the materializer's own pair; the "memory changed" line; no double injection on native resume; one launch-path test per table row (§14.5) | `cb-memory-core` |
| `cb-memory-ui` | the PWA memory page, the `about-me` editor, "Remembered: <title>" rows, write references (§12.3) | `cb-memory-core` |

**After `ma-initiator` lands.**

| Task | Content | After |
|---|---|---|
| `cb-rootturn` | `rootTurn {commandId, origin, continuation}` on the initiator, derived by the Hub from the turn record; written to audit rows, journal rows and gate jobs; never a gate (D-060 R2). Until `ma-restart` adds `hub`, a fact-triggered turn is recorded as `agent` with `continuation` | `ma-initiator` |
| `cb-requested-by` | `requestedBy` stamped from `rootTurn` on Agent-created dispatches, tasks and approval cards: `{commandId}` in an owner turn, else `{origin}`; the owner's words stay in the command row and are hydrated for Human reads only. `/approvals` and the task detail show the owner's words, or "agent 自发" for an agent/hub-origin turn. It never replaces the approval and no decision reads it | `cb-rootturn` |

**Phase 2 (after `ma-initiator`, `ma-fence` and `ma-restart`).**

| Task | Content | After |
|---|---|---|
| `cb-inbox-facts` | the inbox, journal-driven fact sources, dedupe key, persistent occurrence cap, run generation in the key, no facts about a lineage's own chapters (§10, §10.1 items 1–4) | `ma-initiator`, `ma-fence`, `ma-restart` |
| `cb-inbox-deliver` | batching, hold while paused, re-check at batch formation, owner-push fallback, visible row first, Hub voice and template version (§10.1 items 4–7, 9; built, §10.4) | `cb-inbox-facts` |
| `cb-stall-tooluse` | tool-in-flight state for every carrier; no stall fact while a tool is in flight unless `stallToolThresholdMins` is set; stall timing restarts at the tool result; the `max_wall_mins` budget-expired fact (§10.1 item 8) | `cb-inbox-facts` |
| `cb-usage-resume` | `resume_on_reset`, the reset fact, `autoContinue` and the continuation input (§11.6; built, §11.8) | `cb-inbox-deliver` |
| `cb-wake-engine` | `wake_ups`, `wake_runs`, the run state machine, the envelope, fresh mode, `remuda wake`, `/healthz` tick fields (§11, §11.1, §11.3, §11.4) | `cb-inbox-deliver` |
| `cb-wake-check` | `check` on the Node: mandatory timeout, `sh -n` precheck, exit-code routing, its admission rule (§11.2) | `cb-wake-engine` |
| `cb-owner-notify` | `remuda owner notify`, claim rows, dedupe, the "to you" card, the `notify` declaration (§11.5; built, §11.7) | `cb-inbox-deliver` |

**Amendments to unstarted `ma-*` briefs** (made before each is dispatched; the in-flight `ma-initiator` brief is not changed):
- `ma-admission`: read the creator's permission mode at the commit point, inside the create transaction after the worktree or pool lease, and re-check its generation. If the creator's mode changed while the create was being prepared, the create aborts, and the Node materializer refuses a stale stamp. This follows Cindy's submit-time read (`apps/desktop/src/main/maker-ipc/botDelegationService.ts`).
- `ma-skill`: merge `cb-skill`, and generate the capability section from launch facts: grants held, `land` or not, herdr on the host, the project's D-051 switch, whether the inbox exists. A seat without `land` sees no land verb. This follows Cindy's flag-gated guide (`apps/desktop/src/main/maker-ipc/teammateGuide.ts`, `apps/desktop/src/main/maker-ipc/botProfileRuntime.ts`).
- `ma-restart`: the self-close guard (§12.6).
- `ma-fence`: §10.1 item 3 (run generation in the fact key) in its acceptance.
- `ma-main-ui`: origin labels on audit rows and the status line.

### 15.2 Parity follow-ups (agent-parity.md)

Each closes Gap rows of the capability ledger ([agent-parity.md](./agent-parity.md), "Gap index"). Like every Agent route, each keeps the existing admission rules: grants, scope, lineage ownership, the D-017 approval where a create or keys would need it, and, once F lands, `check_initiator`. None adds a policy gate.

| Task | Content | After |
|---|---|---|
| `ap-command-read` | admit `GET /v1/instances/{id}/commands` and `GET /v1/instances/{id}/commands/{commandId}` for owned targets, by listing the `/commands` suffixes in the middleware's `read_target` (today it lists only `/journal` and `/screen`); the handlers already call `require_instance_read`. This is what D-055 reconciliation by commandId needs, and what `cb-errors`' `read-then-decide` hint points at | none |
| `ap-gate-reads` | admit `GET /v1/gate/jobs` (already filtered to the caller's scope) and `GET /v1/gate/logs/{id}` (checked against the job's project scope) for Agent origin. A seat that gates needs the failure log (§14.1, E2) | none |
| `ap-agent-list` | make `remuda instance list` and `remuda agents` work for Agent origin: `agents::list` skips the host inventory for an Agent caller, as the CLI's `list_instances` helper in `crates/remuda/src/cmd/instance.rs` already does (or uses `ap-host-read`'s scope-filtered list once it lands), and `--watch` polls `GET /v1/instances` instead of opening the all-instances follow socket | none |
| `ap-configure` | admit `instance.configure` for model, effort and ultracode on owned targets in `authorize_command`; the permission mode is not included | none |
| `ap-configure-mode` | an owner decision on how OA1 applies to a permission-mode change, then that rule in the Hub and the Node | the owner's decision; `ma-admission` |
| `ap-resume` | resume an owned instance that is not a worker, under `dispatch` and the same admission as a create (OA1; the D-017 approval for cross-host or shell-driver); the public operator route (D-026) is unchanged | `ma-admission` |
| `ap-attachments` | Agent upload to `POST /v1/objects`, and reading the attachments of owned targets | none |
| `ap-own-scope` | validate the target task's project against the caller's scope in both ownership mutations, `claim_own` and `release_own` (`crates/remuda-hub/src/tasks.rs`), as `set_task_state` does through `require_project_scope`; today they check only the `dispatch` grant. Negative tests: an Agent scoped to project A that claims or releases paths on a task in project B gets 403, and the task's claims and `task_paths` rows are unchanged; the same calls inside project A still succeed | none |
| `ap-subagent-read` | admit `GET /v1/instances/{id}/subagents/{agentId}/transcript` in the middleware's `read_target` for owned targets | none |
| `ap-host-read` | host detail, the workspace list and doctor for hosts in the caller's scope, and the host list filtered to that scope, under `dispatch` | none |
| `ap-supply-read` | read-only provider, supply, catalog and usage views and the dry-run `POST /v1/supply/resolve`, under `dispatch`, with no secret material | none |
| `ap-supply-event` | Agent supply evidence (`POST /v1/providers/{id}/supply/events`) only for a supply its own workers use, recorded with the caller's initiator | none |

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
- project-wide switches changed on the seat's behalf;
- any policy gate on fact- or wake-triggered turns, and any content-policy lint on memory (D-060 R2);
- routine templates built into the Hub.

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

Rejected by D-060:
- **An autonomous-turn allowlist that refuses everything else with "needs an owner request"** (Cindy `apps/desktop/src/main/maker-ipc/taskActionHost.ts`, `apps/desktop/src/main/maker-ipc/taskActionAuthorization.ts`): a policy restriction, not a safety property. Its rules reversed themselves several times in a few days (`docs/product-rules/teammate-authority.md`), and a per-turn gate does not stop injected content. Remuda takes only the separation of origin from authority (`rootTurn`, recorded and never a gate; R2).
- **Forbidding wake-up writes in fact-triggered turns:** a wake-up carries no authority of its own and uses the same credential as the turn that wrote it. The paths that could widen authority (grants, scope, `restart`) are already closed (`validate_child_delegation`, §6.1).
- **A "looks like a grant" lint on memory content:** a policy restriction; the real property is §12.5 (R2).
- **Routine templates built into the Hub** (follow-up, todo check, nightly): they contradict "the Hub chooses no cadence" (§11). The skill teaches the agent to add its own with `remuda wake add` under stable names.
- **A review after every reply, and a commitments ledger the model writes:** Cindy dropped its per-reply review after about 22 fix commits (`docs/product-rules/cindy-bots-runtime.md`). Commitments are D-050 tasks.
- **Hidden synthetic continuation triggers:** every continuation is a visible hub-origin row (§9).
- **Each Node following its own tasks:** one central Hub lets one lineage follow every Node.
- **A whole-document memory:** whole-document replacement makes an owner edit and a seat write collide on the entire document. Per-entry CAS is what makes a conflict always visible and never an overwrite (§12).
- **Resetting the permission mode when a seat moves:** OA2 makes the mode the owner's choice at creation.
- **Binding the Feishu owner on first private message:** keep the explicit allowlist.
- **Completion or convergence judged only by a prompt:** `DONE <sha>`, and dependencies unlocked only by a landed sha (`crates/remuda-hub/src/tasks.rs`), are harder evidence.

Reference studied: open-muse (open source).
- Adopted: the loop on always-on infrastructure; a service that is a clock and never reasons; proactive turns as ordinary messages that never interrupt a conversation waiting on the person; notifications suppressed only where the app is in front.
- Not adopted: a fixed check-in cadence, hidden machine turns, and rejecting work when busy.

Reference studied: Cindy (open source, Apache-2.0; D-060). Designs only; no prompt or code text is copied.
- Adopted: host-side deterministic bookkeeping (detection, dedupe, receipts, validation, version CAS) with the model deciding from bounded, sourced data; per-entry typed memory with FTS and a `LIKE` fallback; completion delivery in the order "settle, visible row, then wake"; a run state machine where interrupted runs never replay; a timed pre-run check; usage-limit resume only for subscription windows; a fixed-size digest; recording whose turn it is without gating on it; a capability ledger.
- Not adopted: its permission policy layer, built-in routines, per-reply learning review, hidden triggers, per-machine following, permission reset on relocation, first-message owner binding, and prompt-only convergence.
