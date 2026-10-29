# Agent/user capability ledger: the human half

> Status: proposed with ADR D-060 (2026-10-29), decision 12. Derived from origin/main `c27e0eac`: one row per PWA action (the routes in `web/src/app/router.tsx`, and the Hub calls their pages make through `web/src/lib/api.ts`, `web/src/lib/store.ts` and the feature modules) and one row per clap subcommand (`crates/remuda/src/cmd/`). The Agent side is read from the Hub's route filter `restrict_agent_routes` (`crates/remuda-hub/src/agent_scope.rs`) and each handler. The machine half is a later task: a table generated from the Hub router, `restrict_agent_routes` and the clap registry, with a gate check that the two halves do not drift.

## The rule

What a person can do through Remuda, an agent can do through a Remuda route, unless a real safety property stops it. Grants gate what an agent may do (D-050 item 6), so a missing grant is configuration the owner chose, not a gap. Limits belong in grants, scope and approvals, not in withholding routes. The rule follows Cindy's parity ledger (`docs/product-rules/agent-user-parity.md`).

The ledger makes that rule auditable, and it keeps the seat's skill (`ma-skill`, `cb-skill`) from promising verbs the seat cannot use. The skill must read the **Today** column; the **Planned** column is not yet true.

## Columns and classes

- **Today (origin/main)** is what an Agent-origin caller gets on origin/main `c27e0eac`, with every prerequisite the code checks. It starts with one class:
  - **Agent route.** The middleware admits the path for Agent origin, and the handler admits it under the grant, scope and posture listed. Per-call D-017 approvals are listed too.
  - **Safety.** A real safety property stops it, and the row names the property:
    - approvals are answered only by humans (D-017, D-051(a));
    - keys cannot stand in for an approval (keystrokes);
    - cross-host reach needs the one-shot human approval (D-017);
    - delegation never widens authority, so an agent never mints, holds or exercises Human, host or provider credentials, and never chooses argv or an executable (`restrict_launch_overrides`);
    - agent callers act only on what they own (D-057 §4.1), so an agent does not change configuration shared by sessions it does not own;
    - scope narrows and never grows (`validate_child_delegation`);
    - records are truthful (D-057 §0), so an agent does not erase the record of what agents did.
  - **Gap.** Nothing stops it except a missing route or a missing rule. Every gap has a task id in **Planned**: `ma-admission` (main-agent.md §15) or an `ap-*` follow-up (§15.2).
- **Planned** is the change a named task makes, or "—" when nothing is planned to change.

How a row is decided on origin/main:
- the middleware `restrict_agent_routes` admits a fixed list of path shapes for Agent origin and refuses the rest with 403;
- within that list, handlers call `require_grant`, `caller_project_scope`, `owns()` or `require_instance_read`;
- `require_operator` refuses Agent origin outright;
- `authorize_command`, `prepare_create`, `check_broadcast` and the fleet create ask for the D-017 one-shot approval where the row says so.

Two limits recur below:
- **D-051 limits** (`delegated_visible_items`, `authorize_agent_answer` in `crates/remuda-hub/src/interactions.rs`): only items of the caller and its direct children (`owns()`), only in projects whose D-051 switch is on, never approvals, and none at all while **either side runs `bypassPermissions`**. The plan-review self-exclusion applies. `ma-admission` lifts the bypass exclusion for questions and plan reviews (OA3).
- **Send rule** (`authorize_command`): no grant; no prompt for owned targets or for targets inside an explicitly narrowed scope; a shell-driver target, or an out-of-scope target the caller does not own, needs the D-017 approval.

## PWA actions

### Session list and phone home (`/sessions`, `/m`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| See the session list | `GET /v1/instances`, `GET /v1/interactions` | Agent route. The full list needs `dispatch` and is filtered to the caller's scope; without `dispatch` an instance may list only itself or a direct child (`list_instances`). Interactions under the D-051 limits | `ma-admission` (OA3) for the interaction part |
| Preview a row's screen | `GET /v1/instances/{id}/screen` | Agent route. Owned targets (`owns()`) | — |
| Send one prompt to several selected sessions | `POST /v1/instances/{id}/commands`, `instance.send`, once per session | Agent route. The send rule, per target | — |
| Send keys from the list | commands, `instance.keys` | Safety. Keystrokes: every Agent `instance.keys` needs the D-017 approval | — |
| Close a session from the list or a space tab | commands, `instance.close` | Agent route. Owned targets; any other needs the D-017 approval | — |
| Resume a session (list, phone home, session page) | `POST /v1/instances/{id}/resume` | Gap. Operator-only (D-026) | Workers: `ma-admission` (scoped respawn helper, main-agent.md §4.2 item 5). Other owned instances: `ap-resume`. Main's own Resume stays a Human control (§6.5) |
| Delete a session (spaces panel) | `DELETE /v1/instances/{id}` | Safety. Records are truthful: the record and journal are the evidence of what agents did. Operator-only (`delete_instance`) | — |

### Session (`/s/:id` and its tabs)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| Read the transcript; load earlier; the events tab | `GET /v1/instances/{id}/journal` | Agent route. Owned targets | — |
| Follow the session live | `GET /v1/follow` (WebSocket) | Agent route by another path: the socket is not admitted for Agent origin, and an agent reads the same journal by cursor (`remuda instance read`, `remuda instance wait`) | Phase 2 facts push changes to the parent (`cb-inbox-facts`, `cb-inbox-deliver`) |
| See usage and context | `GET /v1/instances/{id}` (the projection's `usageRollup`) | Agent route. Owned targets | — |
| Send a new turn | commands, `instance.send` | Agent route. The send rule | — |
| Queue a prompt while the session works | held in the browser, then `instance.send` in queue mode | Agent route. The send rule | — |
| Steer with a held prompt | `instance.send` in steer mode | Agent route. The send rule | — |
| See a send's receipt | `GET /v1/instances/{id}/commands/{commandId}` | Gap. The middleware's `read_target` admits only `/journal` and `/screen` below an instance, so command reads are refused for Agent origin; an agent cannot reconcile an unknown outcome by commandId (D-055) | `ap-command-read` admits both command reads for owned targets |
| Stop the turn | commands, `instance.cancel` | Agent route. Owned targets; any other needs the D-017 approval | — |
| Close the session | commands, `instance.close` | Agent route. As Stop. A chapter closing itself becomes `paused{self}`; pausing Main is the owner's control (main-agent.md §6.5) | — |
| Answer a question or a plan review inline | `POST /v1/interactions/{id}/answer` | Agent route under the D-051 limits | `ma-admission` (OA3) |
| Answer an approval inline | `POST /v1/interactions/{id}/answer` | Safety. Approvals are answered only by humans (D-017, D-051(a)) | — |
| Change the model | commands, `instance.configure` | Gap. `authorize_command` admits only send, cancel, close, `tty.write` and keys for Agent origin | `ap-configure` |
| Change the effort | commands, `instance.configure` | Gap. As above | `ap-configure` |
| Toggle ultracode, independently of effort (D-056; `onUltracode` in `web/src/pages/SessionPage.tsx`) | commands, `instance.configure` | Gap. As above | `ap-configure` |
| Change the permission mode | commands, `instance.configure` | Gap. As above | `ap-configure-mode`, after an owner decision on how OA1 applies to a mode change |
| Attach a file | `POST /v1/objects` | Gap. Operator-only (`upload` in `crates/remuda-hub/src/objects.rs`) | `ap-attachments` |
| Open an attachment | `GET /v1/objects/{id}` | Gap. Operator-only for devices; an agent reads only its own session's attachments, through `GET /v1/attachments/{id}/content` (D-028) | `ap-attachments` (owned targets) |
| Send keys from the key bar | commands, `instance.keys` | Safety. Keystrokes | — |
| Type into the terminal | `/v1/follow?tty=1` | Safety. Keystrokes: the socket is not admitted for Agents, and `tty.write` always needs the D-017 approval | — |
| See workspace changes (Files tab) | `GET /v1/hosts/{id}/workspaces/{workspaceId}/changes`, `…/diff`, `…/file` | Safety. Cross-host: operator-only (`proxy_scm`). On its own host the seat reads the same through its harness's shell; reading another host through the Hub would skip the D-017 cross-host approval | — |
| Open a subagent's transcript | `GET /v1/instances/{id}/subagents/{agentId}/transcript` | Gap. Refused by `read_target`, as for command reads, though the handler re-checks ownership | `ap-subagent-read` |

### New session (`/sessions/new`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| Create a session | `POST /v1/instances` | Agent route. `dispatch` and scope (`prepare_create`); a cross-host or shell-driver create needs the D-017 approval; only `manual` or `plan` permission (`restrict_permission`); no argv or executable (`restrict_launch_overrides`) | `ma-admission`: "never beyond the creator" (OA1) replaces the `manual`/`plan` rule |
| Create a worktree for it | `POST /v1/worktrees` | Agent route by another path: a task binding (`POST /v1/tasks` with `workspaceBinding`, D-050, `dispatch`) or a dispatch leases the worktree; the raw worktree routes stay operator-only (main-agent.md §7.5) | — |
| Pick a host | `GET /v1/hosts` | Gap. Operator-only (`list_hosts`); an agent reads hosts in its scope one by one through `GET /v1/hosts/{id}/hostcap` | `ap-host-read` |
| Pick a workspace | `GET /v1/hosts/{id}/workspaces` | Gap. Operator-only (`workspaces.rs`) | `ap-host-read` |
| Browse directories | `GET /v1/hosts/{id}/dirs` | Safety. Cross-host, as for workspace changes (`host_dirs.rs`, D-058) | — |
| Register a directory as a workspace | `POST /v1/hosts/{id}/workspaces` | Safety. Act only on what you own: a registered root is open to every session on that host | — |
| Load the model and supply catalog | `GET /v1/supply/catalog` | Gap. Operator-only (`catalog_http`) | `ap-supply-read` |

### Approvals and inbox (`/approvals`, `/m/inbox`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| List pending items | `GET /v1/interactions` | Agent route under the D-051 limits | `ma-admission` (OA3) |
| Answer a question or a plan review | `POST /v1/interactions/{id}/answer` | Agent route under the D-051 limits | `ma-admission` (OA3) |
| Answer an approval | `POST /v1/interactions/{id}/answer` | Safety. Approvals are answered only by humans | — |

### Board and projects (`/board`, `/projects`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| See the board and a task's panel | `GET /v1/board`, `GET /v1/tasks`, `GET /v1/tasks/{id}`, `GET /v1/tasks/{id}/placements` | Agent route. Project scope only, no grant (`require_project_scope`) | — |
| Move a card to another state | `PATCH /v1/tasks/{id}` | Agent route. `dispatch` and scope (`require_dispatch`) | — |
| Archive a card | `POST /v1/tasks/{id}/archive` | Agent route. `dispatch` and scope | — |
| See projects | `GET /v1/projects`, `GET /v1/projects/{id}` | Agent route. `dispatch` and scope (`require_coordinator`) | — |

### Fleet (`/fleet`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| Broadcast a prompt | `POST /v1/fleet/broadcast`, `instance.send` | Agent route. No grant. A target the caller does not own, or a shell-driver target, needs the D-017 approval (`check_broadcast`); `all` is refused for Agent origin (`check_fleet_all`) because it addresses instances the caller does not own | — |
| Broadcast keys | `POST /v1/fleet/broadcast`, `tty.write` | Safety. Keystrokes: `tty.write` always needs the D-017 approval | — |

The page's create form is not wired to the Hub yet (`web/src/features/fleet/store.ts`); `remuda fleet run` is the create.

### Hosts (`/hosts`, `/hosts/:id`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| See hosts | `GET /v1/hosts` | Gap. Operator-only (`list_hosts`) | `ap-host-read` (hosts in the caller's scope) |
| Add an SSH host | `POST /v1/hosts/ssh` | Safety. Credentials: enrolment mints a host credential | — |
| Remove a host | `DELETE /v1/hosts/{id}` | Safety. Act only on what you own: it retires the host and tears down its live link and relay streams, which every session on that host depends on (`remove_host`) | — |
| Change a host's provider binding | `PATCH /v1/hosts/{id}` | Safety. Credentials: it picks which provider credential every session on the host uses | — |
| Change a host's launch defaults: default args, Claude binary, default TUI | `PATCH /v1/hosts/{id}` | Safety. An agent never chooses argv or an executable (`restrict_launch_overrides`), and the defaults apply to every session on the host | — |
| Unregister a workspace | `DELETE /v1/hosts/{id}/workspaces` | Safety. Act only on what you own, as for registering | — |

### Providers (`/providers`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| See providers | `GET /v1/providers` | Gap. Not admitted for Agent origin | `ap-supply-read` (no secrets) |
| Add a provider | `POST /v1/providers` | Safety. Credentials: the body carries a provider token | — |
| Edit a provider | `PATCH /v1/providers/{id}` | Safety. Credentials | — |
| Make a provider the default gateway | `PATCH /v1/providers/{id}` (`defaultGateway`) | Safety. Credentials and shared configuration: every gateway session then uses it | — |
| Delete a provider | `DELETE /v1/providers/{id}` | Safety. Credentials | — |
| Test a provider | `POST /v1/providers/{id}/test` | Safety. Credentials: it exercises a stored token the agent was never given | — |
| Discover a gateway's models | `POST /v1/providers/discover` | Safety. Credentials: the body carries a token | — |

### Settings (`/settings`)

| Human action | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| Issue a pair code | `POST /v1/devices/pair-code` | Safety. Credentials: it mints a Human credential | — |
| List devices | `GET /v1/devices` | Safety. Credentials: device rows are the Human credentials | — |
| Revoke a device | `DELETE /v1/devices/{id}` | Safety. Credentials | — |
| Add a passkey | `POST /v1/auth/passkeys/register/start`, `…/finish` | Safety. Credentials | — |
| Rename a passkey | `PATCH /v1/auth/passkeys/{id}` | Safety. Credentials | — |
| Delete a passkey | `DELETE /v1/auth/passkeys/{id}` | Safety. Credentials | — |
| Set a host's default terminal UI | `PATCH /v1/hosts/{id}` (`defaultTui`) | Safety. As launch defaults | — |

## CLI subcommands

### Sessions: `remuda instance`, `remuda agents`

| Subcommand | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| `instance create` | `POST /v1/instances`; `GET /v1/hosts` only for label or unplaced creates | Agent route, as "Create a session". With an explicit `--host` the CLI reads no host list; with `--labels` or no host it tries `GET /v1/hosts`, which is refused for Agent origin, and lets the Hub place (`crates/remuda/src/cmd/instance.rs`). A computer-use create needs the list and fails for Agent origin | `ma-admission` (OA1) |
| `instance list` | `GET /v1/hosts` and `GET /v1/instances`; with `--watch`, `GET /v1/follow` | Gap. The CLI path (`agents::list` in `crates/remuda/src/cmd/agents.rs`) joins the host inventory, `GET /v1/hosts` (operator-only, `list_hosts`), with `GET /v1/instances`, so it fails for Agent origin even though `GET /v1/instances` itself admits a `dispatch` holder. `--watch` also opens the all-instances follow socket, which is not admitted for Agent origin | `ap-agent-list` |
| `agents` | as `instance list` | Gap. The same `agents::list` path, with the same host-inventory and follow-socket dependencies | `ap-agent-list` |
| `instance show` | `GET /v1/instances/{id}` | Agent route. Owned targets | — |
| `instance send` | commands, `instance.send` | Agent route. The send rule | — |
| `instance respond` | `GET /v1/interactions?instanceId=`, `POST /v1/interactions/{id}/answer` | Agent route under the D-051 limits for questions and plan reviews; approvals: Safety | `ma-admission` (OA3) |
| `instance wait` | `GET /v1/instances/{id}/journal` | Agent route. Owned targets | — |
| `instance read` | `GET /v1/instances/{id}/journal` | Agent route. Owned targets | — |
| `instance keys` | commands, `instance.keys` | Safety. Keystrokes | — |
| `instance stop` | commands, `instance.cancel` or `instance.close` | Agent route. Owned targets; any other needs the D-017 approval | — |
| `instance rm` | commands, `instance.close` | Agent route. As `instance stop` | — |

### Projects, tasks and ownership

| Subcommand | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| `project list` | `GET /v1/projects` | Agent route. `dispatch` and scope | — |
| `project show` | `GET /v1/projects/{id}` | Agent route. `dispatch` and scope | — |
| `project create` | `POST /v1/projects` | Safety. Scope never grows: the new project would lie outside the creator's delegation box. Operator-only (`create_project`) | — |
| `project set` | `PATCH /v1/projects/{id}` | Agent route. `dispatch` and scope (`set_project`). main-agent.md §17 keeps "project-wide switches changed on the seat's behalf" out of the design; the route admits a `dispatch` holder today | — |
| `project add-member` | `POST /v1/projects/{id}/members` | Agent route. `dispatch` and scope | — |
| `project remove-member` | `DELETE /v1/projects/{id}/members` | Agent route. `dispatch` and scope | — |
| `task add` | `POST /v1/tasks` | Agent route. `dispatch` and scope | — |
| `task list` | `GET /v1/tasks` | Agent route. Project scope only, no grant | — |
| `task show` | `GET /v1/tasks/{id}` | Agent route. Project scope only, no grant | — |
| `task split` | `POST /v1/tasks/{id}/split` | Agent route. `dispatch` and scope | — |
| `task set-state` | `PATCH /v1/tasks/{id}` | Agent route. `dispatch` and scope | — |
| `own claim` | `POST /v1/tasks/{id}/own` | Agent route. **`dispatch` is enforced; the target task's project scope is not.** `claim_own` calls `require_dispatch` (grant only) and then mutates the named task; unlike every other task mutation (`create_task`, `set_task_state`, `archive_task`, `split_task`, `add_placement`), it never calls `require_project_scope` (`crates/remuda-hub/src/tasks.rs`). An Agent with `dispatch` can therefore claim paths on a task in a project outside its scope | `ap-own-scope` |
| `own release` | `DELETE /v1/tasks/{id}/own` | Agent route. **`dispatch` is enforced; the target task's project scope is not**, exactly as for `own claim` (`release_own`). An Agent with `dispatch` can release another project's claims | `ap-own-scope` |
| `own check` | `POST /v1/own/check` | Agent route. Project scope only, no grant | — |
| `own list` | `GET /v1/own` | Agent route. Project scope only, no grant | — |

### The gate

| Subcommand | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| `gate <branch>` (verify) | `POST /v1/projects/{id}/gate`, then `GET /v1/projects/{id}/gate/jobs/{jobId}`; on failure `GET /v1/gate/logs/{id}` | Agent route for the enqueue (`dispatch` and scope) and the wait (scope). The failure log is refused (see `gate log`); without `--project` the project lookup reads `GET /v1/projects` (`dispatch`) | `ap-gate-reads` for the log |
| `gate list` | `GET /v1/projects/{id}/gate` with `--project`; otherwise `GET /v1/gate/jobs` | Agent route with `--project` (scope). Gap without it: `/v1/gate/jobs` is not admitted for Agent origin, although the handler filters by scope | `ap-gate-reads` |
| `gate log` | `GET /v1/gate/logs/{id}` | Gap. Not admitted for Agent origin. A seat that gates (main-agent.md §14.1, E2) needs it | `ap-gate-reads` |
| `gate cancel` | `POST /v1/projects/{id}/gate/jobs/{jobId}/cancel` | Agent route. `dispatch` for verify jobs, `land` for land jobs | — |
| `land` | `POST /v1/projects/{id}/gate` (land mode) | Agent route. `land` and scope | — |

### Workers: `remuda dispatch`, `worker`, `watch`, `report`, `retire`, `brief send`, `hostcap`

| Subcommand | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| `dispatch` | `POST /v1/workers/dispatch` | Gap. `restrict_agent_routes` does not admit `/v1/workers/*`, and dispatch writes a control-off mode for every harness (main-agent.md §4.2) | `ma-admission` |
| `watch` | `POST /v1/workers/observe`, `GET /v1/gate/jobs`, `GET /v1/instances` | Gap. The worker and cross-project gate reads are not admitted | `ma-admission`; `ap-gate-reads` |
| `report` | `GET /v1/workers`, gate job reads | Gap. Worker reads are not admitted; without `--project` it also reads `GET /v1/gate/jobs` | `ma-admission`; `ap-gate-reads` |
| `retire` | `POST /v1/workers/{id}/retire` | Gap. Not admitted | `ma-admission`, with lineage-subtree ownership (main-agent.md §4.2 item 1) |
| `worker nudge` | `POST /v1/workers/{id}/nudge` | Gap. Not admitted | `ma-admission`, as `retire` |
| `worker answer` | `POST /v1/workers/{id}/answer` | Gap. Not admitted | `ma-admission` admits it; it writes keys, so every Agent use then needs the D-017 approval (Safety, main-agent.md §13) |
| `worker switch-model` | `POST /v1/workers/{id}/switch-model` | Gap. Not admitted | `ma-admission`, as `retire` |
| `worker resume` | `POST /v1/workers/{id}/resume` | Gap. Not admitted | `ma-admission` (scoped respawn helper) |
| `worker replace` | `POST /v1/workers/{id}/replace` | Gap. Not admitted | `ma-admission` (scoped respawn helper) |
| `worker stop` | `POST /v1/workers/{id}/stop` | Gap. Not admitted | `ma-admission`, as `retire` |
| `brief send` | `POST /v1/workers/{id}/brief` | Gap. Not admitted | `ma-admission`, as `retire` |
| `hostcap` | `GET /v1/hosts/{id}/hostcap` | Agent route. `dispatch` and host scope (D-045) | — |

### Fleet, host files and diagnostics

| Subcommand | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| `fleet run` | `POST /v1/fleet/instances` | Agent route. `dispatch`, checked as each child is inserted (`validate_child_delegation`); the D-017 approval when any chosen host is not the caller's host or the driver is a shell driver (`create_fleet`); only `manual` or `plan` permission | `ma-admission` (OA1) |
| `fleet send` | `POST /v1/fleet/broadcast`, `instance.send` | Agent route, as "Broadcast a prompt"; `--all` is refused | — |
| `fleet keys` | `POST /v1/fleet/broadcast`, `tty.write` | Safety. Keystrokes | — |
| `host files list` | `GET /v1/hosts/{id}/files` | Safety. Cross-host, as for workspace changes (`host_files.rs`) | — |
| `host files get` | `POST /v1/hosts/{id}/files/read`, `GET /v1/objects/{id}` | Safety. Cross-host, as above | — |
| `host files search` | `POST /v1/hosts/{id}/files/search` | Safety. Cross-host, as above | — |
| `doctor` | `GET /v1/hosts/{id}/doctor` | Gap. Operator-only (`doctor_host`) | `ap-host-read` (hosts in the caller's scope) |

### Providers and supply: `remuda profile`

| Subcommand | Hub route | Today (origin/main) | Planned |
|---|---|---|---|
| `profile catalog` | `GET /v1/supply/catalog` | Gap. Operator-only | `ap-supply-read` |
| `profile list` | `GET /v1/providers` | Gap. Not admitted for Agent origin | `ap-supply-read` |
| `profile show` | `GET /v1/providers/{id}`, `GET /v1/providers/{id}/supply` | Gap. Not admitted | `ap-supply-read` |
| `profile usage` | `GET /v1/providers/{id}/usage` | Gap. Not admitted | `ap-supply-read` |
| `profile probe` (dry-run admission: what would run where) | `POST /v1/supply/resolve` | Gap. Operator-only (`resolve_supply_http`); it is read-only | `ap-supply-read` |
| `profile event` (report observed supply evidence: a 429 cools a window) | `POST /v1/providers/{id}/supply/events` | Gap. Not admitted for Agent origin | `ap-supply-event`: evidence from workers the caller owns, recorded with the caller |
| `profile declare` | `PUT /v1/providers/{id}/supply` | Safety. Act only on what you own: it rewrites the supply envelope every session on that provider is admitted against | — |
| `profile delivery` | `PATCH /v1/providers/{id}` (`delivery`) | Safety. Credentials: it moves where a provider credential's traffic leaves from (D-047) | — |

### Processes and local commands

These call no Hub route as an Agent action. "Local" commands run on the caller's own host with the caller's OS user: the seat can run them through its own shell, under its harness's permission control, and running one against another host is outside Hub mediation (main-agent.md §4.3). "Deployment" commands start or install Remuda processes, which the owner or ops run (D-057 D1).

| Subcommand | Kind | Today (origin/main) | Planned |
|---|---|---|---|
| `hub` | Deployment: runs a Hub | Not an agent action (D-057 D1) | — |
| `hub rotate-bootstrap` | Credential, on the Hub host | Safety. Credentials: it replaces the device pairing access code | — |
| `node` | Deployment: runs a Node against a Hub, or over stdio for development | Not an agent action (D-057 D1) | — |
| `node daemon` | Deployment: the durable daemon under a service supervisor | Not an agent action (D-057 D1) | — |
| `node run` | Deployment: starts a detached daemon | Not an agent action (D-057 D1) | — |
| `node install` | Deployment: installs the user service | Not an agent action (D-057 D1) | — |
| `node uninstall` | Deployment: removes the user service | Not an agent action (D-057 D1) | — |
| `node status` | Local: reads the local daemon socket's health | Local | — |
| `node bridge` | Deployment: forwards stdio to the daemon | Not an agent action (D-057 D1) | — |
| `dev` | Deployment: a loopback development Hub and Node | Not an agent action (D-057 D1) | — |
| `dispatcher` | Deployment: the Feishu dispatcher, a Bot credential | Not an agent action (D-057 D1) | — |
| `ssh list` | Local: reads the caller's SSH config | Local | — |
| `ssh probe` | Local: probes a host over SSH | Local; reaching another host this way is outside Hub mediation | — |
| `ssh bootstrap` | Deployment: installs a binary on another host | Not an agent action (D-057 D1) | — |
| `ssh node` | Enrols a Node through `/v1/node` | Safety. Credentials: enrolment mints a host credential | — |
| `worktree ls` | Local: lists the checkout's git worktrees | Local | — |
| `worktree rm` | Local: removes a linked worktree, keeping its branch | Local | — |
| `worktree prune` | Local: prunes stale worktree registrations | Local | — |
| `worktree create` | Local: `git worktree add` under the checkout's sibling directory | Local | — |
| `merge` | Local: verifies a merge and compare-and-swaps `main` with the host's own git credentials | Local. It bypasses the Hub's `land` grant on a host that holds push credentials (main-agent.md §4.3); the Hub route for landing is `remuda land` | — |
| `brief lint` | Local: lints a brief file | Local | — |
| `journal diff` | Local: compares two journal dumps | Local | — |
| `version` | Local | Local | — |
| `hook emit` | Local: the relay a harness hook runs inside a session | Local | — |
| `hook launch` | Local: merges a Claude settings file and execs Claude in place | Local | — |
| `mcp` | Local: the in-session MCP server, which calls the Hub with the session's own credential | Local; its tools reach the Hub routes above under the same rules | — |

## Not agent actions

These change only a person's own browser or device:
- logging in, redeeming a pair code, passkey login, logging out, and this browser's push subscription;
- appearance, density and other local preferences;
- retracting a held prompt: it never left the browser;
- editing a host's label, labels or instance cap on `/hosts`: the page keeps them in the browser's host registry and sends nothing to the Hub;
- the Bots pages, which render a static channel list and call no Hub route.

## Agent-first verbs from D-060

These are not on origin/main. Each is built for the seat first; the owner's counterpart is listed.

| Verb | Hub route | Owner counterpart | Task |
|---|---|---|---|
| `remuda memory write`, `read`, `search`, `rm`, `consolidate` | `/v1/lineages/{id}/memory…` (protocol.md §8.4) | PWA memory page, `about-me` editor | `cb-memory-core`, `cb-memory-ui` |
| `remuda worker digest`, `remuda glance` | `GET /v1/workers/{id}/digest`, `GET /v1/lineages/{id}/glance` | PWA board projection | `cb-digest` |
| `remuda wake add`, `list`, `rm` | wake-up routes (main-agent.md §11) | wake-ups shown in Main's status | `cb-wake-engine`, `cb-wake-check` |
| `remuda owner notify` | owner-notify route (main-agent.md §11.5), `address-owner` only | "to you" cards | `cb-owner-notify` |

## Gap index

Every Gap row above, by the task that closes it. The `ap-*` tasks are defined in main-agent.md §15.2.

| Task | Closes |
|---|---|
| `ma-admission` | `dispatch`; `watch`, `report` (worker reads); `retire`; `worker nudge`, `answer`, `switch-model`, `resume`, `replace`, `stop`; `brief send`; worker resume from the PWA; the D-051 bypass exclusion (OA3) |
| `ap-command-read` | a send's receipt and every command read (`GET /v1/instances/{id}/commands`, `…/commands/{commandId}`) |
| `ap-gate-reads` | `gate log`; `gate list` without `--project`; the gate parts of `watch` and `report`; the failure log of `gate <branch>` |
| `ap-agent-list` | `instance list` and `agents`, including `--watch` |
| `ap-configure` | model, effort and ultracode changes |
| `ap-configure-mode` | permission-mode changes, after the owner decides how OA1 applies |
| `ap-resume` | resuming an owned instance that is not a worker |
| `ap-attachments` | attaching a file; opening an attachment of an owned session |
| `ap-subagent-read` | a subagent's transcript |
| `ap-own-scope` | the missing target-project scope check on `own claim` and `own release` (an over-broad Agent route today, not a missing one) |
| `ap-host-read` | the host list, a host's workspaces and `doctor`, for hosts in the caller's scope |
| `ap-supply-read` | `profile catalog`, `list`, `show`, `usage`, `probe`; the provider list; the model and supply catalog |
| `ap-supply-event` | `profile event` |
