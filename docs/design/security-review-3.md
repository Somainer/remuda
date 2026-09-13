# x-rev security review

Stack: `wt/c-sec2/mcp-fleet-scope` (`3ba1ed3`, 11 commits) vs `origin/main` (`ad054d8`).
Read-only review of authenticated instance caller scope, one-shot approvals, origin propagation, MCP context, fleet confirm, enroll-env scrub, WSS alias, worktree restriction, D-018.

No P0 write/enroll/origin-forge path found. Writes are gated. Reads are not.


## Resolution record

The audit below is preserved as reviewed; the personal home example is replaced
with `<user-home>/`. Findings are numbered in their original order. Changes are
on `wt/c-sec2/scope-review-fixes`, started from the merged stack at `21ac15a` and validated after rebase onto `5818f7e`.

| Finding | Resolution |
|---|---|
| P1-1 — Agent Hub reads | Resolved. GET allowlist permits caller context and self/direct-child instance or journal reads. Handlers repeat ownership checks or require an operator. MCP lists fetch only caller-owned ids; unrelated read/wait and doctor are refused. WSS route matrix covers every listed private GET surface. |
| P1-2 — Shell PTY environment | Resolved. `ShellPtyOptions` carries typed `AgentMcpContext`; the child starts with `env_clear`, filtered `base_env` and overlays, then authenticated context. Re-exec isolation covers print and shell PTY with Node secret canaries and typed credentials. |
| P1-3 — Local worktree mutation | Resolved. CLI dispatch, shared create/remove/prune/ensure functions, instance worktree creation, and MCP handlers refuse a set `REMUDA_INSTANCE_ID`, including an empty value. CLI tests preserve dirty sibling files/catalog; direct MCP handler tests bypass preflight to verify the second guard. |
| P2-1 — Agent permission modes | Resolved. Single-instance and fleet creation reject every explicit Agent mode except `manual`/`plan`; omission becomes `manual`. WSS tests assert HTTP 403 before instance indexing or dispatch for bypass, dontAsk, auto, acceptEdits and unknown/default labels. |
| P2-2 — Grant capacity and listing | Resolved. Consumed and denied rows are removed, only pending rows count toward 1024, and instance-device listings match caller device and instance. Human/Bot operators retain approval visibility. A regression completes 1025 grants within one TTL and verifies caller-isolated listings and replay denial. |
| P2-3 — Scoped token in extra_env | Resolved. Node extra-env maps strip denied names and never receive the scoped token. Only the redacted typed context carries it to spawn; environment and Debug assertions cover this separation. |
| P2-4 — Policy test gaps | Resolved. Agent mock identity records all requests; forbidden fleet all/keys, sibling reads and doctor never reach their sinks. Added Hub read denials, create 403 assertions, and real shell PTY isolation. The intentional re-exec helper remains ignored by the ordinary test harness. |
| Nit 1 — remuda-dev Human origin | Deferred. This access-code authenticated operator surface is separate from Hub instance credentials; changing its authority model is outside these review fixes. The audit's same-UID/access-code limitation remains recorded below. |
| Nit 2 — Token prefix helper | Resolved. Both instance-token issuance and login now use checked `auth::token_prefix()` instead of slicing the generated token. |

Validation: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets
--locked -- -D warnings`; `cargo test --workspace --locked` (703 passed,
zero failed, 13 intentionally ignored); affected-crate build; workspace
all-target check; and `./scripts/ci/secret-scan.sh` all pass. The ignored isolation
helper is exercised by its parent re-exec test. No web files are changed by this
review-fix branch. After the rebase brought an updated generated web type,
`pnpm test` also passed (41 files, 134 tests).

---

## P1 — Agent-scoped tokens can read the whole Hub (journals of other instances)

**Where:** `crates/remuda-hub/src/agent_scope.rs:316-324` (`restrict_agent_routes`); handlers never re-check ownership on GET.

**Why:** Middleware allows every authenticated Agent `GET` except `/v1/follow`. `POST` is allowlisted to create / fleet-create / broadcast / `…/commands`. That is fail-open for reads. The same stack correctly forbids the live TTY socket (`ws.rs:176-177`) and documents that agents must not reach sibling/ancestor data, but `GET /v1/instances/{id}/journal` (`http.rs:564-589`) only calls `require_device` + existence. MCP `remuda_instance_read` / `wait` / `list` (`crates/remuda/src/cmd/mcp/scope.rs:114-116` `_ => Ok(false)`) therefore dump any instance’s prompts, tool output, and typed secrets. `GET /v1/interactions` also returns every pending one-shot grant, including the full action JSON (`interactions.rs:64-81` + `agent_approvals.rs:215-223`).

**Hub routes touched (Agent token presented):**

| Route | Scope check |
|---|---|
| `GET /healthz` | none (public) |
| `POST /v1/login` | middleware skip; bootstrap pairs a **Human/Bot** device |
| `POST /v1/devices/pair` | middleware skip; needs the 8-char pairing code |
| `GET /v1/node`, `GET /node/v1/connect` | middleware skip; `authenticate_host` on enroll/node token, not device token |
| `GET /v1/caller` | bound instance + direct children only — OK |
| `GET /v1/instances`, `GET /v1/instances/{id}` | **none** (fleet-wide) |
| `POST /v1/instances` | `prepare_create`: stamp origin, force parent, same-host/shell/approval |
| `POST /v1/instances/{id}/commands` | `authorize_command` allowlist + `owns`/approval |
| `GET /v1/instances/{id}/journal` | **none** |
| `POST /v1/instances/{id}/mcp-token` | middleware deny + Human-only handler |
| `POST /v1/fleet/instances` | stamp + same-host/shell/approval |
| `POST /v1/fleet/broadcast` | `check_fleet_all` (Agent+`all` hard-deny) + `check_broadcast` |
| `GET /v1/fleet/{id}` | **none** |
| `POST /v1/fleet/{id}/commands` | middleware deny (not in POST allowlist) + handler deny |
| `GET /v1/follow` | middleware deny + handler deny — OK |
| `GET /v1/hosts`, `GET /v1/hosts/{id}` | **none** (SSH `target` included in `host_view`) |
| `GET /v1/hosts/{id}/doctor` | **none**; triggers Node `host.doctor` |
| `PATCH /v1/hosts/{id}`, `DELETE /v1/hosts/{id}` | middleware deny |
| `POST /v1/hosts/enroll-token` | middleware deny (D-018) |
| `POST /v1/hosts/ssh` | middleware deny |
| `GET /v1/devices` | **none** (ids/kind/instanceId) |
| `POST /v1/devices/pair-code` | middleware deny |
| `DELETE /v1/devices/{id}` | middleware deny |
| `GET /v1/providers`, `GET /v1/providers/{id}` | **none** (no raw token; `last4`+fingerprint+headers) |
| `POST/PATCH/DELETE /v1/providers…`, `POST …/test` | middleware deny |
| `GET /v1/worktrees` | **none**; Node `worktree.list` |
| `POST /v1/worktrees` | middleware deny |
| `GET /v1/interactions` | **none**; all Hub tickets + all agent grants |
| `POST /v1/interactions/{id}/answer` | middleware deny + Human-only handler |
| `POST /v1/placement/resolve` | middleware deny |

Writes that *are* gated: Agent `instance.send|cancel|close` only for self/direct children without approval; `tty.write`/`instance.keys`/shell send always need a grant; `all:true` broadcast is `BadRequest` even with `confirm` or an approval id; fleet-member commands are forbidden. That part holds.

**Minimal fix:** In `restrict_agent_routes`, do not treat Agent GET as globally allowed. Allow only `GET /v1/caller` plus, if needed, `GET /v1/instances/{self}` / journal / children. In `list_instances` / `get_instance` / `get_journal` / `list_interactions` / `list_devices` / `list_hosts` / `list_providers` / `list_worktrees` / `doctor_host` / `get_fleet`, call `owns` (or an explicit Agent read set: self + `parent_instance_id == caller`) and 404/403 otherwise. Filter `agent_approvals::list` to the caller’s instance. Mirror the same filter in MCP `remuda_instance_read|list|wait|doctor`. Add a test: instance-bound token `GET`s a sibling journal → 403.

---

## P1 — `shell-pty` still inherits the Node environment (enroll wipe is incomplete)

**Where:** `crates/remuda-driver/src/shell_pty.rs:297-319`; `crates/remuda-node/src/native.rs:275-279`; `crates/remuda-node/src/origin.rs:61-96`.

**Why:** Claude print/pty/bg `env_clear()` + denylist, then inject `AgentMcpContext`. Shell-pty does neither. `CommandBuilder` gets `TERM`/`COLORTERM` plus `extra_env` overlays on the process environment. `instance_env` overwrites `REMUDA_BOOTSTRAP_TOKEN` and `REMUDA_ENROLL_TOKEN` with `""` (so those two names are blanked if the builder overlays rather than replaces), but `REMUDA_HOST_TOKEN`, `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`, gateway tokens, `AWS_SECRET_ACCESS_KEY`, etc. still reach the pane. Agent create of `shell-pty` requires a human grant (`agent_scope.rs:101-111`, `132-133`), but after Allow-once the model’s shell can `env`. Isolation tests only spawn `ClaudePrintDriver` (`child_env_isolation.rs`).

**Minimal fix:** `env_clear` (or portable-pty equivalent) in `build_command`, apply `child_env::base_env()`, skip `child_env::is_denied` names, then apply `AgentMcpContext` (shell currently never sets `agent_mcp`). Extend the isolation fixture to `ShellPtyDriver` and a PTY path. Do not put the scoped token in `extra_env` (see P2).

---

## P1 — Worktree create/rm restriction is MCP-only; CLI on inherited PATH bypasses it

**Where:** `crates/remuda/src/cmd/mcp/scope.rs:111-113` (the new gate); `crates/remuda/src/cmd/mcp/worktree.rs:24-66` (handler has no origin check); `crates/remuda/src/cmd/worktree.rs:21-24` (CLI `enter` never talks to Hub); `crates/remuda/src/cmd/instance.rs:332-334` (`worktree::ensure` before any Hub call); `crates/remuda-driver/src/child_env.rs:25-38` (`PATH` is inherited).

**Why:** Commit `6de5602` requires Human/Bot for local worktree rm even with `force` / `approvalId`. That check lives only in `mcp_requires_approval`. The MCP handler then calls `worktree::remove` with no second check. `remuda worktree create|rm|prune` never consults `/v1/caller`. `remuda instance create --worktree` calls `worktree::ensure` locally, then POSTs Hub. Agent children inherit Node `PATH`; if `remuda` is on it, `remuda worktree rm --force <sibling>` deletes any catalogued worktree on that Node (dirty trees included). Hub `POST /v1/worktrees` is correctly middleware-denied; that does not cover the local CLI.

**Minimal fix:** In `worktree::run` / `worktree::ensure` / `instance::create`, if `REMUDA_INSTANCE_ID` is set (or after `HubClient::caller_context()`), refuse create/rm/prune/ensure. Do the same inside the MCP handlers, not only `scoped_client`. Tests: agent-origin MCP *and* `Command::new("remuda").args(["worktree","rm",…])` with `REMUDA_INSTANCE_ID` set → error, no `git worktree remove`.

---

## P2 — Hub does not clamp Agent `permissionMode` (only fills null)

**Where:** `crates/remuda-hub/src/agent_scope.rs:97-99`; `crates/remuda-node/src/native.rs:702-708`; `crates/remuda-driver/src/materializer.rs:578-584`.

**Why:** Non-Human create sets `permissionMode=manual` only when the field is JSON null. Agent `POST /v1/instances` with `permissionMode: "auto"` or `"acceptEdits"` on the same host needs no approval and launches. `bypassPermissions` / `dontAsk` are rejected at materialize (WSS test `wss.rs:1062-1076` waits for settlement `"not allowed"` after HTTP 200). Protocol says omitted → manual and structured bypass is rejected; it does not stop `auto`.

**Minimal fix:** For Agent origin, overwrite any mode other than `manual`/`plan` (or reject the request at `prepare_create` with 403) so Hub fails closed instead of creating a failed/over-privileged instance. Test Hub 403, not Node settlement.

---

## P2 — One-shot grants: consumed rows occupy the 1024 cap; list is global

**Where:** `crates/remuda-hub/src/agent_approvals.rs:102-133`, `215-223`; consume path `104-118`.

**Why:** Consume is correct: `Mutex`, `caller`+exact `request`+`Allowed` → `Consumed`, replay Forbidden. TTL 900s matches broker default. `answer` is Human-only. Binding includes `device_id`+`instance_id`+`host_id`. No concurrency hole on consume.

Residual: `retain` only drops expired grants, so Consumed/Denied entries count toward `grants.len() >= 1024` for 15 minutes. An agent can open 1024 distinct unowned-send tickets and freeze new approvals. `list()` returns every Pending grant to any `GET /v1/interactions` caller (covered under the GET P1).

**Minimal fix:** Count only `Pending` toward the cap; drop `Consumed`/`Denied` immediately (keep a short replay-deny set keyed by id if you still want explicit Forbidden vs NotFound). Filter `list` by instance.

---

## P2 — Scoped token is copied into unredacted `extra_env`

**Where:** `crates/remuda-node/src/origin.rs:76-84`; driver `*Options.extra_env` is derived `Debug`.

**Why:** Comment on `AgentCredential` / `AgentMcpContext` says the token is never in recipes or logs. `instance_env` still inserts `REMUDA_TOKEN` into `extra_env`. Spawn sites denylist-skip `REMUDA_*` and then inject `agent_mcp`, so the child gets the token once; the Options/maps still hold the plaintext for any `{:?}` of launch options.

**Minimal fix:** Stop putting `REMUDA_*` in `instance_env`. Only `AgentMcpContext::environment()` after the denylist. Keep the empty enroll/bootstrap overwrite only on paths that inherit (shell), or drop inheritance entirely (P1).

---

## P2 — Tests that miss the policy or assert the Hub 200 for a denied launch

**Where:**
- `crates/remuda-node/tests/wss.rs:1062-1070` — Agent `permissionMode=bypassPermissions` expects **200**, denial only in later settlement. Hub never rejected.
- `crates/remuda/src/cmd/test_hub.rs:67` — mock `/v1/caller` is always `"human"`. MCP tests in `mcp/mod.rs` (`tools_call_list_keys_and_fleet_send_against_mock_hub`) therefore exercise fleet `all:true` as Human, not Agent.
- `crates/remuda-driver/tests/child_env_isolation.rs:160` — `#[ignore]` inner test is re-exec by design; it never covers shell/PTY/`agent_mcp`.
- No test that an instance token `GET /v1/instances/{sibling}/journal` is forbidden.

**Minimal fix:** Assert 403 at create for Agent bypass/auto. Add a mock caller `origin=agent` case that fleet `--all --confirm` never hits `/v1/fleet/broadcast`. Add sibling-journal 403. Keep the ignore with the re-exec comment.

Live `#[ignore]` tests (`claude_print_process`, `live_claude`, `generic_pty` haiku, etc.) are pre-existing and not in this stack’s security claims.

---

## nit — `remuda-dev` local RPC stamps Human origin

**Where:** `crates/remuda-node/src/server.rs:960`, `1025`.

**Why:** Loopback `instance.send` / `tty.write` hard-code `InputOrigin::Human`. Protocol already calls this an operator access-code surface, not Hub Agent auth. Dangerous only if an agent obtains the dev access code (same UID / file). Fail closed on that server: default Agent unless the access-code session is proven Human.

---

## nit — `instance_token` prefix vs `token_prefix()`

**Where:** `crates/remuda-hub/src/agent_scope.rs:293` (`token[..16]`) vs `mint_bot_device_token` at `273-275` (`auth::token_prefix`).

**Why:** `random_token` is 64 hex so both work. A shorter generator would panic or skip the prefix index.

**Minimal fix:** Use `token_prefix(&token).ok_or(…)?` in `instance_token` and login.

---

## Checked, not filed

**Approvals (replay / TTL / bind / single-use):** Consume under the grants mutex requires `ApprovalCaller` equality (device+instance+host), exact JSON `request`, and `GrantState::Allowed`, then sets `Consumed` (`agent_approvals.rs:104-118`). Second consume Forbidden. Pending duplicate returns the same interaction id. Broker first-answer-wins; mismatched digest → Denied. 15-minute `Instant` TTL, fail-closed on Hub restart. Human-only `answer` (`interactions.rs:154-156`). No P0/P1 here.

**Origin spoofing on Hub→Node:** `stamp` overwrites `origin` / `input.origin`, strips `actor` and `agentCredential` (`agent_scope.rs:51-61`). Node `wire_origin` reads only the envelope, unknown → Agent (`origin.rs:25-28`). WSS overwrites `credential.hub` from the Node’s own URL (`runtime_wss.rs:132-134`). Driver defaults are Agent (`LaunchOrigin::default`). Local remuda-dev Human stamp is the nit above.

**MCP env vs another instance’s token / Hub operator token:** `AgentMcpContext` is not in the recipe; overlays cannot replace it (`agent_mcp.rs:107-122`). `HubOpts::connect` drops bootstrap when `REMUDA_INSTANCE_ID` is set (`hub_client.rs:58-66`); empty `REMUDA_TOKEN` does not fall back to `data/*/bootstrap-token`. `x-remuda-instance-id` cannot retarget a bound token (`agent_scope.rs:33-41`). No on-disk scope file; isolation is env. Same-UID `/proc/<node>/environ` remains an OS boundary (protocol already excludes it).

**D-018:** Agent `POST /v1/hosts/enroll-token` is 403 (middleware + test `wss.rs:1027-1035`). Alias `/node/v1/connect` uses the same `node_socket` / `authenticate_host`. Dispatcher combined mode mints a Bot device token, not bootstrap (`dispatcher.rs:108-118`). Login `deviceKind` only `human|bot`.

**Fleet `confirm`:** Web always sends `confirm: true` (`BroadcastBox.tsx:44`) as a Human UI. Agent `all:true` is still rejected (`agent_scope.rs:172-176`). Filtered broadcast to any unowned/shell/tty target requires a grant (`186-207`).

**Personal paths / tokens in this stack:** none in the 11 commits. Fixtures use `http://hub.example`, `scoped-process-token`, `/tmp` via tempfile. Pre-existing `<user-home>/` in unrelated Claude fixtures is outside this diff.

---

DONE 3
