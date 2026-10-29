# Agent/user capability ledger: the human half

> Status: proposed with ADR D-060 (2026-10-29), decision 12. The rows are derived from origin/main `c27e0eac`: the PWA routes in `web/src/app/router.tsx` and the Hub calls behind them (`web/src/lib/api.ts`, `web/src/lib/store.ts` and the feature modules), the clap tree in `crates/remuda/src/cmd/mod.rs`, and the Hub's Agent route filter `restrict_agent_routes` in `crates/remuda-hub/src/agent_scope.rs`. The machine half is a later task: a table generated from the Hub router, `restrict_agent_routes` and the clap registry, with a gate check that the two halves do not drift.

## The rule

What a person can do through Remuda, an agent can do through a Remuda route, unless a real safety property stops it. Grants gate what an agent may do (D-050 item 6), so a missing grant is configuration the owner chose, not a gap. Limits belong in grants, scope and approvals, not in withholding routes. The rule follows Cindy's parity ledger (`docs/product-rules/agent-user-parity.md`).

The ledger makes that rule auditable, and it keeps the seat's skill (`ma-skill`, `cb-skill`) from promising verbs the seat cannot use.

## Classes

- **Agent route.** The middleware admits the path for Agent origin, and the handler admits it under the grant and scope named in the row. Per-call D-017 approvals are noted.
- **Safety.** A real safety property stops it, and the row names the property:
  - approvals are answered only by humans (D-017, D-051(a));
  - keys cannot stand in for an approval (keystrokes);
  - cross-host reach needs the one-shot human approval (D-017);
  - delegation never widens authority, so an agent never mints or holds Human, host or provider credentials.
- **Gap.** Nothing stops it except a missing route or a missing rule. The row names the task that closes it, or says "no task yet", or says "not planned" with the reason. A not-planned row is a deliberate exclusion the owner can revisit.

How a row is decided on origin/main:
- the middleware `restrict_agent_routes` admits a fixed list of path shapes for Agent origin and refuses the rest with 403;
- within that list, handlers call `require_grant`, `caller_project_scope`, `owns()` or `require_instance_read`;
- `require_operator` refuses Agent origin outright;
- `authorize_command`, `prepare_create`, `check_broadcast` and the fleet create ask for the D-017 one-shot approval where the row says so.

## Sessions and conversation

| Human action | PWA / CLI | Hub route | Class | Why, or the task |
|---|---|---|---|---|
| List sessions | `/sessions`, `/m`; `remuda instance list`, `remuda agents` | `GET /v1/instances` | Agent route | The full list needs `dispatch` and is filtered to the caller's scope; without `dispatch` an instance may list only itself or a direct child (`list_instances`) |
| Show a session | `/s/:id`; `remuda instance show` | `GET /v1/instances/{id}` | Agent route | Targets the caller owns: itself, its lineage and their children (`owns()`) |
| Read the transcript | `/s/:id` structured view; `remuda instance read`, `remuda instance wait` | `GET /v1/instances/{id}/journal` | Agent route | Owned targets |
| Read the screen | `/s/:id/tty` snapshot | `GET /v1/instances/{id}/screen` | Agent route | Owned targets |
| Follow a session live | every session view | `GET /v1/follow` (WebSocket) | Gap | Not planned: agents read by cursor, and Phase 2 facts push changes (`cb-inbox-facts`, `cb-inbox-deliver`) |
| Send a message: new turn, steer or queue | composer; `remuda instance send` | `POST /v1/instances/{id}/commands`, `instance.send` | Agent route | No prompt for owned targets or targets inside an explicitly narrowed scope. A shell-driver target, or an out-of-scope target the caller does not own, needs the D-017 approval (`authorize_command`) |
| Stop the turn | Stop; `remuda instance stop` | commands, `instance.cancel` | Agent route | Owned targets; any other target needs the D-017 approval |
| Close a session | close; `remuda instance rm`, `remuda instance stop --scope instance` | commands, `instance.close` | Agent route | As Stop. Pausing Main is the owner's control from a Human device (main-agent.md §6.5); a chapter closing itself becomes `paused{self}` |
| Send keys | key bar; `remuda instance keys` | commands, `instance.keys` | Safety | Keystrokes: every Agent `instance.keys` needs the D-017 approval (`authorize_command`) |
| Type into the terminal | `/s/:id/tty` | `/v1/follow?tty=1`; commands, `tty.write` | Safety | Keystrokes: the follow socket is not admitted for Agents, and `tty.write` always needs the D-017 approval |
| Change model or effort | session header | commands, `instance.configure` | Gap | `authorize_command` admits only send, cancel, close, `tty.write` and keys for Agent origin. No task yet. Workers get `remuda worker switch-model` with `ma-admission` |
| Change permission mode | session header | commands, `instance.configure` | Gap | Refused for Agent origin. Needs an owner decision before any task: OA1 governs creates, and no rule yet says how it applies to a mode change. Cindy's ledger holds this behind a separate product decision too |
| Resume a closed session | Resume | `POST /v1/instances/{id}/resume` | Gap | Operator-only (D-026). Workers: `ma-admission` adds the scoped respawn helper (main-agent.md §4.2 item 5). Main's own Resume stays a Human control (§6.5) |
| Delete a session record | session menu | `DELETE /v1/instances/{id}` | Gap | Not planned: operator-only (`delete_instance`); the record and journal are the audit of what agents did |
| Upload an attachment | composer | `POST /v1/objects` | Gap | Operator-only (`objects.rs`, `upload`). No task yet. Agents hand files to workers with `remuda worker nudge` and `remuda brief send` once `ma-admission` lands |
| Read an attachment | transcript chips | `GET /v1/attachments`, `GET /v1/attachments/{id}/content` | Agent route | The caller's own session only (D-028) |
| Read a subagent transcript | `/s/:id/agents/:agentId` | `GET /v1/instances/{id}/subagents/{agentId}/transcript` | Gap | The middleware's `read_target` lists only the `/journal` and `/screen` suffixes, so this read is refused for Agent origin even though the handler re-checks ownership. No task yet |
| Read a command's outcome | send receipts; D-055 reconciliation | `GET /v1/instances/{id}/commands`, `GET /v1/instances/{id}/commands/{commandId}` | Gap | Refused by `read_target`, as above. An agent cannot reconcile an unknown outcome by commandId, which D-055 and main-agent.md §10 rely on. `cb-errors` admits it for owned targets, because its `read-then-decide` hint points there |
| Read usage and context | session header | `GET /v1/instances/{id}/usage` | Gap | Operator-only (`instance_usage_http`). No task yet; context-percentage crossings reach the parent as Phase 2 facts (main-agent.md §10) |
| Answer a question or a plan review | `/approvals`, `/m/inbox`; `remuda instance respond` | `GET /v1/interactions`, `POST /v1/interactions/{id}/answer` | Agent route | D-051: items one hop down, in projects whose switch is on; the handler re-checks |
| Answer an approval | same | same | Safety | Approvals are answered only by humans (D-017, D-051(a)) |
| Browse files in a workspace | `/s/:id/files`; `remuda host files list`, `get`, `search` | `GET /v1/hosts/{id}/files`, `POST /v1/hosts/{id}/files/read`, `POST /v1/hosts/{id}/files/search` | Safety | Cross-host: operator-only (`host_files.rs`). The seat reads its own host through its harness; reading another host through the Hub would skip the D-017 cross-host approval |
| Read workspace changes | Files view | `GET /v1/hosts/{id}/workspaces/{workspaceId}/changes`, `…/diff`, `…/file` | Safety | Cross-host, as above (`workspaces.rs`, `proxy_scm`) |
| Browse directories | new-session directory picker | `GET /v1/hosts/{id}/dirs` | Safety | Cross-host, as above (`host_dirs.rs`, D-058) |

## Creating and broadcasting

| Human action | PWA / CLI | Hub route | Class | Why, or the task |
|---|---|---|---|---|
| Create a session | `/sessions/new`; `remuda instance create` | `POST /v1/instances` | Agent route | `dispatch` and scope (`prepare_create`). A cross-host or shell-driver create needs the D-017 approval. Agents may use only `manual` or `plan` (`restrict_permission`) until `ma-admission` moves it to OA1 |
| Create a worktree | new-session form | `POST /v1/worktrees`, `…/lease`, `…/return` | Agent route | Through a task binding (`POST /v1/tasks` with `workspaceBinding`, D-050) or dispatch; the raw worktree routes stay operator-only (main-agent.md §7.5) |
| Create one instance per host | `/fleet` (its create form is not wired to the Hub yet: `web/src/features/fleet/store.ts`); `remuda fleet run` | `POST /v1/fleet/instances` | Agent route | As create. The D-017 approval is needed when any chosen host is not the caller's host or the driver is a shell driver (`create_fleet`) |
| Broadcast a prompt | `/fleet` broadcast box; `remuda fleet send` | `POST /v1/fleet/broadcast` | Agent route | Filtered targets. A target the caller does not own, or a shell-driver target, needs the D-017 approval (`check_broadcast`). `--all` is refused for Agent origin (`check_fleet_all`) because it addresses instances the caller does not own |
| Broadcast keys | `remuda fleet keys` | `POST /v1/fleet/broadcast` with keys | Safety | Keystrokes |
| Read a fleet | `/fleet` | `GET /v1/fleet/{id}` | Gap | Operator-only (`get_fleet`). No task yet |

## Tasks, ownership, projects and the gate

| Human action | PWA / CLI | Hub route | Class | Why, or the task |
|---|---|---|---|---|
| List and read tasks; the board | `/board`, task panels; `remuda task list`, `show` | `GET /v1/tasks`, `GET /v1/tasks/{id}`, `GET /v1/tasks/{id}/placements`, `GET /v1/board` | Agent route | Scope-filtered |
| Add, split, move or archive a task | board; `remuda task add`, `split`, `set-state` | `POST /v1/tasks`, `POST …/split`, `PATCH /v1/tasks/{id}`, `POST …/archive`, `POST …/placements` | Agent route | `dispatch` |
| Claim, release or check ownership | `remuda own claim`, `release`, `check`, `list` | `POST /v1/tasks/{id}/own`, `DELETE /v1/tasks/{id}/own`, `GET /v1/own`, `POST /v1/own/check` | Agent route | Scope |
| List and read projects | `/projects`; `remuda project list`, `show` | `GET /v1/projects`, `GET /v1/projects/{id}` | Agent route | `dispatch` and scope |
| Change project settings | `remuda project set` | `PATCH /v1/projects/{id}` | Agent route | `dispatch` and scope (`set_project`). main-agent.md §17 keeps "project-wide switches changed on the seat's behalf" out of the design; the route admits a `dispatch` holder today |
| Add or remove members | `remuda project add-member`, `remove-member` | `POST /v1/projects/{id}/members`, `DELETE /v1/projects/{id}/members` | Agent route | `dispatch` and scope |
| Create or delete a project | `remuda project create` | `POST /v1/projects`, `DELETE /v1/projects/{id}` | Gap | Not planned: operator-only (`create_project`, `delete_project`). A new project would lie outside the creator's scope, and scope never grows (`validate_child_delegation`) |
| Gate verify | `remuda gate <branch>` | `POST /v1/projects/{id}/gate` | Agent route | `dispatch` |
| Land | `remuda land` | `POST /v1/projects/{id}/gate` (land), `POST /v1/tasks/{id}/land` | Agent route | `land` |
| Cancel a gate job | `remuda gate cancel` | `POST /v1/projects/{id}/gate/jobs/{jobId}/cancel` | Agent route | `dispatch` for verify jobs, `land` for land jobs |
| List a project's gate jobs | `remuda gate list --project` | `GET /v1/projects/{id}/gate`, `GET /v1/projects/{id}/gate/jobs/{jobId}` | Agent route | Scope |
| List gate jobs across projects | `remuda gate list`; `remuda watch` | `GET /v1/gate/jobs` | Gap | Not admitted for Agent origin, although the handler filters by scope. No task yet; the per-project route works |
| Read a gate failure log | `remuda gate log`; printed by `remuda gate` on failure | `GET /v1/gate/logs/{id}` | Gap | Not admitted for Agent origin. No task yet. A seat that gates (main-agent.md §14.1, E2) needs it; the suggested home is `cb-digest`, which already changes the Agent read predicates |

## Workers

| Human action | PWA / CLI | Hub route | Class | Why, or the task |
|---|---|---|---|---|
| Dispatch a worker | `remuda dispatch` | `POST /v1/workers/dispatch` | Gap | `ma-admission`: `restrict_agent_routes` does not admit `/v1/workers/*`, and dispatch writes a control-off mode for every harness (main-agent.md §4.2) |
| Read the roster, observe, watch, report | `remuda watch`, `remuda report` | `GET /v1/workers`, `GET /v1/workers/{id}`, `POST /v1/workers/observe` | Gap | `ma-admission`; reads stay scope-wide. `remuda watch` also needs `GET /v1/gate/jobs` (above) |
| Nudge, switch model, resume, replace, stop, retire, set state, send a brief | `remuda worker …`, `remuda retire`, `remuda brief send` | `POST /v1/workers/{id}/…` | Gap | `ma-admission`, with lineage-subtree ownership for every mutation (main-agent.md §4.2 item 1) |
| Answer a worker's dialog | `remuda worker answer` | `POST /v1/workers/{id}/answer` | Safety | Keystrokes: once admitted, Agent use needs the D-017 approval (main-agent.md §13) |
| Read a host's capacity | `remuda hostcap` | `GET /v1/hosts/{id}/hostcap` | Agent route | `dispatch` and host scope (D-045) |

## Hosts, providers and devices

| Human action | PWA / CLI | Hub route | Class | Why, or the task |
|---|---|---|---|---|
| List hosts; read a host; run doctor | `/hosts`, `/hosts/:id`; `remuda doctor` | `GET /v1/hosts`, `GET /v1/hosts/{id}`, `GET /v1/hosts/{id}/doctor` | Gap | Not planned: operator-only. Hostcap covers the hosts in an agent's scope |
| Edit host settings: labels, provider binding, launch defaults, default TUI | `/hosts`, `/settings` | `PATCH /v1/hosts/{id}` | Gap | Not planned: host-wide configuration for every session on the host (main-agent.md §17) |
| Register or unregister a workspace | `/hosts/:id`, new-session form | `POST /v1/hosts/{id}/workspaces`, `DELETE /v1/hosts/{id}/workspaces` | Gap | Not planned: operator-only (`workspaces.rs`); it changes which directories every session on the host may use |
| Add or remove an SSH host; enroll a Node | `/hosts` add form; `remuda ssh …`, `remuda node` | `POST /v1/hosts/ssh`, `DELETE /v1/hosts/{id}`, `POST /v1/hosts/enroll-token`, `/v1/node` | Safety | Credentials: enrolment mints a host credential, and delegation never widens authority |
| Create, edit, delete or test a provider; discover its models | `/providers` | `POST /v1/providers`, `PATCH /v1/providers/{id}`, `DELETE /v1/providers/{id}`, `POST …/test`, `POST /v1/providers/discover` | Safety | Credentials: these carry provider tokens |
| Read providers, supply and usage; declare supply; set model-API delivery | `/providers`; `remuda profile …` | `GET /v1/providers…`, `PUT /v1/providers/{id}/supply`, `/v1/supply/*` | Gap | Not planned for the full view (operator configuration). `cb-digest` gives a seat the one supply fact it needs: each worker's cooling-down window and its `resetsAt` |
| Pair, list or revoke devices; manage passkeys; rotate the access code | `/settings`; `remuda hub rotate-bootstrap` | `/v1/devices…`, `/v1/auth/passkeys…` | Safety | Credentials: these mint or revoke Human credentials. An agent holding one would act with Human authority, and keys cannot stand in for an approval |

## Not agent actions

These are not rows, because they act on a person's own device or process rather than on Remuda state:
- logging in, redeeming a pair code, passkey login, and this browser's push subscription;
- appearance and other local preferences;
- the Bots page, which renders a static channel list and calls no Hub route;
- starting a Hub, a Node, the development stack or the Feishu dispatcher (`remuda hub`, `remuda node`, `remuda dev`, `remuda dispatcher`): the owner or ops run deployment (D-057 D1).

Local commands call no Hub route: `remuda worktree`, `remuda merge`, `remuda brief lint`, `remuda journal diff`, `remuda version`, `remuda hook` (run by harness hooks) and `remuda mcp` (the in-session MCP server). The seat runs them through its own shell on its own host, under its harness's permission control. Running one on another host is the cross-host case.

## Agent-first verbs from D-060

These are not on origin/main. Each is built for the seat first; the owner's counterpart is listed.

| Verb | Hub route | Owner counterpart | Task |
|---|---|---|---|
| `remuda memory write`, `read`, `search`, `rm`, `consolidate` | `/v1/lineages/{id}/memory…` (protocol.md §8.4) | PWA memory page, `about-me` editor | `cb-memory-core`, `cb-memory-ui` |
| `remuda worker digest`, `remuda glance` | `GET /v1/workers/{id}/digest`, `GET /v1/lineages/{id}/glance` | PWA board projection | `cb-digest` |
| `remuda wake add`, `list`, `rm` | wake-up routes (main-agent.md §11) | wake-ups shown in Main's status | `cb-wake-engine`, `cb-wake-check` |
| `remuda owner notify` | owner-notify route (main-agent.md §11.5), `address-owner` only | "to you" cards | `cb-owner-notify` |

## Gaps with no task yet

For the owner and the coordinator to assign or close:
- changing an instance's model or effort;
- changing an instance's permission mode (an owner decision comes first);
- a general Agent attachment upload;
- reading a subagent transcript;
- reading instance usage;
- listing gate jobs across projects;
- reading a gate failure log (needed for E2; suggested home `cb-digest`);
- reading a fleet.

Not planned, by decision: an Agent follow socket, deleting session records, creating or deleting projects, and host, workspace and provider administration.
