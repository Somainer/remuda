# Remuda surface inventory — three-tier agent hierarchy & scheduling

Read-only inventory of `<repo>`
at `HEAD = 3f68061` ("merge: wt/ux-d/quickfind into main"), 2026-09-15. Nothing in
the repo was edited, built, committed or run. All `file:line` references are from
that checkout.

Purpose: map the owner's stated three-tier product vision (T1 统领 Coordinator behind a
chat bot → T2 per-project Coordinator → T3 workers, with model **supply** profiling
driving a mixed-harness scheduler) onto what Remuda already has, and name the gaps.

---

## 0. One-paragraph verdict

**Tier 3 is essentially complete and productized.** Everything a worker needs — create
on a chosen host, deliver a brief, wait on a completion line, read the screen, steer,
key, stop, resume, gate-and-merge — exists as protocol + Hub REST + CLI + MCP + a Claude
skill, and has live dogfood evidence (D-015). **Tier 1 exists only as a channel adapter**:
`remuda-feishu` is explicitly "a channel adapter, not an agent loop"
(`crates/remuda-feishu/src/lib.rs:1-13`), it has one global default host/agent/model, and
the Bots page in the web UI is static mock data. **Tier 2 does not exist at all.** There is
no *project* entity anywhere in the protocol, the Hub schema, or the CLI; "Space" is a
client-side `localStorage` grouping of `(hostId, workspaceId)`. Placement is a
single-dimension least-loaded-host chooser with no notion of harness cost, model supply,
provider quota, or task shape. The manual loop under `<coord-scratch>/` is doing the
whole of T1 and T2 by hand today, in bash.

---

## 1. Tier → existing surface map

Legend: **OK** = exists and is the real path · **PARTIAL** = exists but not in the shape
the tier needs · **GAP** = nothing in the repo does this.

### 1.1 Tier 1 — 统领 Coordinator (owner-facing bot, accountable, task intake)

| T1 responsibility | Existing entity / API / CLI | Status |
| --- | --- | --- |
| Chat channel in/out (the "pipeline exit") | `crates/remuda-feishu/` — inbound NDJSON from `lark-cli event consume` (`consume.rs:124` supervisor, `inbound.rs:209` `RawEvent`), outbound `lark-cli im +messages-send/+messages-reply` (`outbound.rs:76` `LarkCli`, DryRun default `outbound.rs:94`) | **OK** (Feishu only; Telegram is D-008 "later, Hub side") |
| Run it | `remuda dispatcher` (`crates/remuda/src/cmd/dispatcher.rs:26`) or `remuda hub --with-dispatcher` (`crates/remuda/src/cmd/hub.rs:21,97-113`) | **OK** |
| Topic → session identity | `SessionKey = feishu:{chat_id}:{thread_id||root_id||main}` (`remuda-feishu/src/inbound.rs:185-196`), persisted `SessionStore` SQLite (`dispatcher.rs:34,78`), `SessionBinding` (`dispatcher.rs:40`) | **OK** |
| Owner gating / allowlist | `InboundPolicy` (`inbound.rs:218`), `DropReason` (`inbound.rs:233`), `GateDecision` (`inbound.rs:248`); config `owner_open_ids` / `chat_allowlist` / `allow_unaddressed` (`crates/remuda/src/config.rs:122-126`) | **OK** |
| Owner hands it a task | `Intent::Prompt` → `InstanceApi::create/send` (`dispatcher.rs:265-283`) | **PARTIAL** — a prompt becomes **one worker instance**, verbatim. No decomposition, no plan, no fan-out, no routing decision. |
| Owner steers routing | `/new /host /agent /model /status /stop /yes /no` (`inbound.rs:304` `ExplicitCommand`) | **PARTIAL** — the *human* picks host/agent/model; the bot never does. |
| Defaults when unpinned | `RouteDefaults { host, agent, model }` (`dispatcher.rs:246-264`), from `[dispatcher] host/agent/model` (`crates/remuda/src/config.rs:128-130`) | **PARTIAL** — exactly **one** global default triple for all chats, all projects. This is the single clearest T2 hole. |
| Approvals surfaced to owner | `CardTicket`/`TicketStore` (`remuda-feishu/src/tickets.rs:36,122`), card renderers (`cards.rs:122-403`), `/yes` `/no` shortcuts (`inbound.rs:330-341`) | **PARTIAL** — `TicketStore` is in-memory (`tickets.rs:122-130`): a dispatcher restart drops every open card binding. |
| Owner answers an approval | `POST /v1/interactions/{id}/answer` (`crates/remuda-hub/src/interactions.rs:23,146`) | **BLOCKED** — `interactions.rs:154` requires `InputOrigin::Human`; a bot-kind device token gets 403. Documented with a live probe in `docs/design/feishu-session-card.md §1`. |
| Live progress back to owner | `follow_live` polling `GET /v1/instances/{id}/journal?afterSeq=` (`cmd/dispatcher.rs:630-638,1029-1103`; interval `follow_interval_ms` default 1000, `config.rs:133`) → `FollowEvent` (`dispatcher.rs:348`) | **BROKEN** — `map_journal_event` (`remuda-feishu/src/hub_api.rs:177`) matches `"tool"/"tool_use"/"tool-boundary"` and `"result"/"completion"`, none of which are real `ObservationKind` values (`remuda-protocol/src/enums.rs`, real kinds are `tool_call`/`tool_result`/`lifecycle`). Progress and completion cards effectively never fire on a real journal. |
| In-place card updates (a live session view) | `CardKitStream`/`CardKitOp` (`cards.rs:42-119`) are placeholder types; `cards.rs:302,332` hardcode `"streaming_mode": false` | **GAP** — full design exists (`docs/design/feishu-session-card.md`, ~10 person-days scoped, CardKit `batch_update` at 500 ms tick), unimplemented. |
| Bot configuration UI | `web/src/pages/BotsPage.tsx` + `web/src/features/bots/channels.ts:32` `BOT_CHANNELS` | **MOCK** — a hardcoded `const`, not wired to any Hub endpoint. Notably its `BotChannel` type already carries `defaultProject: string` (`channels.ts:24`) — the *only* place the word "project" appears as a routing concept in the whole repo. |
| Accountability / task ledger the owner watches | — | **GAP** — no Task/Job/Goal entity above `Instance`. `Fleet` (`crates/remuda-hub/src/fleet.rs:16-21`) is one spec fanned across N hosts, not a task with sub-tasks. |
| Bot is a distinct principal with its own rules | `InputOrigin::{Human,Bot,Agent}` derived from device kind + instance binding (`crates/remuda-hub/src/agent_scope.rs:19-27`); D-011 "bot never bypasses"; D-017 agent-origin scoping (`agent_scope.rs:125-263`) | **OK** (except the interaction-answer 403 above) |

**T1 summary.** The transport, identity, dedup, gating and approval-card plumbing are real
and tested. What is missing is the *coordinator* itself: nothing decides how to turn one
owner sentence into work, nothing reports a task's state as a task, and the defaults are a
single global triple rather than per-project.

### 1.2 Tier 2 — per-project Coordinator (its own workspaces, remotes, policies)

| T2 responsibility | Nearest existing surface | Status |
| --- | --- | --- |
| **A project exists** | — | **GAP.** No `Project` in `crates/remuda-protocol/src/entities.rs`, no `projects` table in `crates/remuda-hub/src/store.rs`, no `/v1/projects` route (full route list: §1.4 below). |
| Group sessions by project | `Space` = `(hostId, workspaceId)` pair, built client-side: `web/src/features/spaces/store.ts:44` `spaceKey`, `:53` `buildSpaces`, prefs in `localStorage` key `remuda.spaces.v1` (`store.ts:6`) | **PARTIAL / UI-only.** D-024 explicitly refuses to merge same-named dirs across hosts, so a Space can never span the two machines a project actually runs on. Nothing server-side knows about spaces. |
| Own its workspaces | D-023 registry: `GET/POST/DELETE /v1/hosts/{id}/workspaces` (`crates/remuda-hub/src/workspaces.rs:14-19`), acknowledged snapshot on the host record (`store.rs:226` `workspaces`, `:229` `workspace_revision`), Node-side allowlist `workspace_roots` (`crates/remuda/src/config.rs:99`) | **PARTIAL** — workspaces are per-**host** by construction (protocol §2.2 "Workspace 固定属于一个 Host"). A project is 1:N over workspaces across hosts; nothing models that N. |
| Own its remotes / hosts | Host registry `GET /v1/hosts` (`hosts.rs:12`), `GET|PATCH /v1/hosts/{id}` (`registry.rs:20`), SSH enroll `POST /v1/hosts/ssh` (`ssh_hosts.rs:77`, body `{target,label,labels,remudaBinaryPolicy}` at `ssh_hosts.rs:51`), one-shot enroll token `POST /v1/hosts/enroll-token` (`hosts.rs:14` → `http.rs:231`), `remuda node install --enroll-token` (D-018/D-019, `docs/design/remote-modes.md`) | **PARTIAL** — hosts are a flat global fleet. Scoping "these 3 hosts belong to project X" is only expressible as a label convention, and labels are untyped strings matched exactly (`placement.rs:231` `host_has_label`, normalizing `:`→`=`). |
| Decide remote vs local worker | `Placement::{Host,Labels,Any}` (`crates/remuda-hub/src/placement.rs:17-30`), parsed `placement.rs:34`, dry-run `POST /v1/placement/resolve` (`placement.rs:368`), resolution `pick_hosts` (`placement.rs:344`) → `select_hosts` (`placement.rs:133`) | **PARTIAL** — see §2; it is a filter + one load ratio, with no cost/capability/model dimension. |
| Decide which harness | `--kind claude|codex|grok|agy|gemini` and `--driver` on create (`crates/remuda/src/cmd/instance.rs:46-52`; Hub spec assembly `crates/remuda-hub/src/http.rs:537-560`) | **PARTIAL** — fully expressible, never *decided* by anything but a human/agent caller. |
| Decide which model / effort | `model` on the spec → `InstanceRecord.model` (`store.rs:328`); `EffortSelection {name, ultracode}` (`crates/remuda-protocol/src/launch.rs:148-231`), persisted `store.rs:340,347`; per-harness tier tables `web/src/features/session/effort.ts:44-84` (claude low/medium/high/xhigh/max + ultracode stop; codex low/medium/high/xhigh/max/ultra; grok low/medium/high/xhigh; agy default) | **OK as a knob, GAP as a policy** — nothing maps "task shape + supply" → model/effort. |
| Provider / gateway policy | D-012/D-021: `delegation ∈ none|gateway|direct`; profile scope `universal | host:<id>`; host `providerBinding ∈ auto|native|profile:<id>`; resolution waterfall `crates/remuda-hub/src/provider_resolve.rs:156-253` (request → host binding → host-scoped default → universal default → host native inventory → native fallback); REST `crates/remuda-hub/src/providers.rs:24-34`; secret release guard `provider_resolve.rs:119`; overlay written by Node as `--settings` (`docs/design/providers.md` "Node / driver overlay") | **PARTIAL** — the waterfall is *host*-keyed, never *project*-keyed. Two projects on one host cannot want different gateways. |
| Per-host launch defaults ("launchopts") | `HostRecord.default_launch_args` + `claude_binary_path` (`store.rs:220,223`), set via `PATCH /v1/hosts/{id}` (`registry.rs:137-160`, validated against the driver arg allowlist at `registry.rs:136`), folded into the spec at create time by `merge_host_launch_defaults` (`http.rs:516-534`, **replace not concat**), UI `web/src/features/hosts/HostLaunchDefaults.tsx` | **OK for hosts — this is the exact insertion slot a project-defaults layer would reuse.** |
| Isolate work (worktrees) | `POST /v1/worktrees` (`http.rs:144`), `remuda worktree create|ls|rm|prune` (`crates/remuda/src/cmd/worktree.rs`; doc `docs/design/remuda-cli.md:85-110`), catalog in `<git-common-dir>/remuda-worktrees.json`, path guard `crates/remuda-protocol/src/path_guard.rs` pinning `<repo>/../remuda-wt/<name>` | **OK** |
| Brief the worker | `--prompt-file` on create / `--file` on send (`cmd/instance.rs:70,87`); brief conventions in `skills/remuda/SKILL.md` "Task briefs" (per-agent `CARGO_TARGET_DIR`, explicit pathspecs, never push, `DONE <sha>`) | **PARTIAL** — the convention is prose in a skill, not a per-project template artifact. |
| Gate and merge | `remuda merge <branch> --gate` (`crates/remuda/src/cmd/merge.rs:19-44`: `--gate`, `--dry-run`, `--list/--pending`, `--affected` default, `--full`, `--web`, `--no-push`, `--message`, `--target-dir`), authority is `scripts/ci/gate.sh` **read from the merged temporary worktree** (`docs/design/remuda-cli.md:134-137`), plus the `gen-api-current` mirror of CI; MCP `remuda_merge` | **OK — and the strongest existing T2 primitive.** Exit codes 0/1/2/3 and per-step JSON make it automatable. |
| Report upward | — | **GAP** — no aggregation. A coordinator would have to poll `GET /v1/instances` and re-derive everything. |
| Enforce project rules on workers | Prose only: `docs/design/coordinator-guide.md:50-54` (D-031 "no deploy/ scripts, no tunnel tools"), `CONTRIBUTING.md` | **GAP** as machine-enforced policy. |
| Bind a chat topic to a project | `RouteDefaults` (global), mock `defaultProject` field (`web/src/features/bots/channels.ts:24`) | **GAP** |

### 1.3 Tier 3 — workers

| T3 responsibility | Surface | Status |
| --- | --- | --- |
| Launch a harness on a host | `POST /v1/instances` (`crates/remuda-hub/src/instances.rs:13-14` → `http.rs:537`), `remuda instance create` (`cmd/instance.rs:38-75`), MCP `remuda_instance_create` | **OK** |
| Launch spec & materialization | `InstanceSpec` (`crates/remuda-protocol/src/launch.rs:264-330`): kind/driver/binaryPath+sha/cwd/worktree/providerProfile/modelId/effort/permissionMode/env/args/settingsOverlay/nativeHome/carrier/requiredCapabilities/completionScope/parent | **OK** |
| PTY / native carrier | D-028 native PTY first (`portable-pty` + `vt100` + ring), herdr optional via `REMUDA_PTY_CARRIER`; `CarrierSpec` (`launch.rs:616`); per-session launch shim + per-instance hook socket | **OK** |
| Deliver prompt, steer, keys | `POST /v1/instances/{id}/commands` (`instances.rs:20`) with `instance.send`/`instance.cancel`/`instance.close`/`tty.write`/`instance.keys`; `remuda instance send|keys|stop` | **OK** |
| Wait on completion | `remuda instance wait --until idle|done|blocked|line:<regex>` (`cmd/instance.rs:102-117`), bullet-tolerant matcher + brief-echo suppression (`cmd/instance.rs:718-740`, `skills/remuda/SKILL.md` "wait conditions"); default 30 s, hard max 300 s | **OK** |
| Read output | `remuda instance read --source screen|journal --lines N --after-seq` (`cmd/instance.rs:119-131`), `GET /v1/instances/{id}/journal` (`instances.rs:22`), `GET /v1/follow` WS (`crates/remuda-hub/src/tty.rs:8`) | **OK** |
| Approvals / interactions | Interaction broker (protocol §6), `GET /v1/interactions` + answer (`interactions.rs:22-23`), hook relay `remuda hook emit` (`crates/remuda/src/cmd/hook.rs:1-40`) | **OK** |
| Session continuity | D-026 `nativeRef` + `POST /v1/instances/{id}/resume` (`instances.rs:21` → `http.rs:674`), 30-day cutoff (`http.rs:133`) | **OK** |
| Fan-out | `POST /v1/fleet/instances` + `/v1/fleet/broadcast` (`fleet.rs:16-21`), `remuda fleet send|keys` | **OK** |
| Attachments / images | D-027 `POST /v1/objects`, `GET /v1/objects/{id}` (`objects.rs:42-43`), `/v1/attachments` (`attachments.rs:43-44`) | **OK** |
| Coordinator-facing diagnostics | `remuda doctor [--local|--host] --json` (`cmd/doctor.rs`; doc `remuda-cli.md:6-54`), `remuda agents --watch` (`cmd/agents.rs`, doc `remuda-cli.md:56-83`), `remuda journal diff` parity gate (`cmd/journal_diff.rs:25-67`), `remuda dev` (`cmd/dev.rs:26`) | **OK** |
| Agent-as-caller safety | D-017: `agent_scope.rs:19` origin derivation, `:125-263` per-verb restrictions, one-shot human approvals (`agent_approvals.rs`), `POST /v1/instances/{id}/mcp-token` (`agent_scope.rs:282`) | **OK** |
| Agent control plane | `remuda mcp` stdio server, tools: `remuda_instance_{create,list,send,wait,read,keys,stop,rm,respond}`, `remuda_worktree_{create,rm}`, `remuda_fleet_{run,send,keys}`, `remuda_doctor`, `remuda_merge`, `remuda_attachment(s_list)` (`crates/remuda/src/cmd/mcp/`; doc `docs/design/main-agent-control.md`) | **OK** |
| Coordinator playbook for a model | `skills/remuda/SKILL.md` + `skills/remuda/sections/*` | **OK** |

### 1.4 Complete Hub HTTP route inventory (for gap-checking)

`healthz`, `/v1/login`, `/v1/worktrees`, `/v1/auth/passkeys/*`, `/v1/devices*`,
`/v1/caller`, `/v1/instances`(+`/{id}`, `/commands`, `/resume`, `/journal`,
`/mcp-token`), `/v1/interactions`(+`/{id}/answer`), `/v1/hosts`(+`/{id}`, `/doctor`,
`/ssh`, `/enroll-token`, `/{id}/workspaces`), `/v1/placement/resolve`, `/v1/fleet/*`,
`/v1/providers`(+`/discover`, `/{id}`, `/{id}/test`), `/v1/objects`, `/v1/attachments`,
`/v1/follow`, `/v1/node` + `/node/v1/connect`. Composed at
`crates/remuda-hub/src/lib.rs:345-360`.

**No project, no task, no schedule, no quota, no usage route exists.**

---

## 2. Data available today for scheduling — and where it lives

### 2.1 What the scheduler can see right now

| Signal | Where produced | Where stored | Where consumed |
| --- | --- | --- | --- |
| Host liveness (real WS link, not SQLite) | Hub node registry | derived `placement::hosts_with_live_links` (`placement.rs:285-299`) | `consider()` rejects offline (`placement.rs:180-194`) |
| `maxInstances` ceiling | Node config, default **8** (`crates/remuda-node/src/inventory.rs:122`, `carrier.rs:280`), advertised in `node.hello` (`carrier.rs:333,376`), overridable `PATCH /v1/hosts/{id} {"maxInstances":N}` (`registry.rs:144`) | `HostRecord.max_instances` (`store.rs:205`) | hard reject in `consider()` (`placement.rs:200-214`) |
| Live instance count | `store.running_count` (`store.rs:2412`) over `LIVE_INSTANCE_COUNT_SQL` (`store.rs:505`) + `requested` rows younger than `REQUESTED_SLOT_WINDOW_MS = 300_000` (`store.rs:496`) | SQLite | `load_of` (`placement.rs:162-170`) |
| **Load ratio** `running / max(maxInstances,1)` | — | — | **the only ranking key** (`placement.rs:153-158`), tie-broken by `host_id` |
| Placement labels | Node config `[node] labels` (`crates/remuda/src/config.rs:89`) or `PATCH /v1/hosts/{id}` | `HostRecord.labels` (`store.rs:197`) | exact match after `:`→`=` normalization (`placement.rs:231-235`) |
| CPU / memory | `detect_resources()` (`crates/remuda-node/src/inventory.rs:723-743`): `cpu_count`, `mem_bytes`, `cpu_pct` (loadavg/cpus), `mem_pct`; shipped on hello + heartbeat (`carrier.rs:379`) | `HostRecord.resources` (`store.rs:203`) | **nothing** — `select_hosts` never reads it |
| Agent CLI inventory + version + path | Node probe, normalized `crates/remuda-hub/src/inventory.rs:30-51` | `HostRecord.cli` (`store.rs:189`) `{kind,version,path,auth}` | `remuda doctor`; `host_reports_native_claude` (`provider_resolve.rs:137`) |
| Per-host **login state** per CLI | credential-marker heuristics (no provider validation) | `cli[].auth ∈ gateway-native | logged_in | logged_out | unknown` (`docs/design/remuda-cli.md:20-27`) | provider waterfall step 5 (`provider_resolve.rs:244-250`) |
| herdr availability | `HostRecord.herdr` (`store.rs:200`) | SQLite | driver feasibility gate (`placement.rs:246-262`): `claude-pty` needs herdr; remote `generic-pty` over SSH needs a herdr `path` |
| Driver/delegation constraint of the request | `PlaceSpec {driver, delegation}` (`placement.rs:104-127`) | — | `consider()` |
| Model id in use | create spec / `instance.configure` | `InstanceRecord.model` (`store.rs:328`) | UI only |
| Effort tier in use | `EffortSelection` (`launch.rs:148`) | `InstanceRecord.effort_name` / `effort_ultracode` / `effort_index` (`store.rs:340,347,358`), legacy names normalized on read | UI only |
| Provider profile + why it was chosen | `provider_resolve::resolve` (`provider_resolve.rs:156`) | `InstanceRecord.delegation`, `provider_profile_id`, `provider_source`, `provider_source_hint` (`store.rs:316-325`) | UI hint line |
| **Provider catalog metadata** | `/v1/providers/discover` probing **both** OpenAI and Anthropic surfaces and unioning (`providers.rs:27`; `docs/design/providers.md` "Two listings per gateway") | profile `models[] = {id, enabled, label, contextWindow, tags[], surfaces[]}` | New Session picker; `defaultModel` must be an enabled id |
| **Provider health** | `POST /v1/providers/{id}/test` (`providers.rs:34`) → `{ok, reachable, status, latencyMs, message, models}` | not persisted | operator-triggered only |
| Instance lifecycle / activity / connectivity | Node journal → Hub | `InstanceRecord.lifecycle/activity/connectivity` (`store.rs:301-305`) | `remuda agents --watch`, UI |
| Host-lost grace | `host_lost_grace_ms` default 600 000 (`crates/remuda-hub/src/config.rs:103-105`) | config | stale-instance reaper (`lib.rs:313`) |

### 2.2 Usage / cost — built, not wired

- Protocol: `UsagePayload` (`crates/remuda-protocol/src/observation.rs:576-611`) —
  `scope ∈ turn|session`, `mode ∈ snapshot|…`, `metricRevision`, `input/output/reasoning/
  cacheRead/cacheWrite/total` each as `Knowledge<U64>`, `cost: Knowledge<Cost>`,
  `accounting ∈ estimated|reported`, `inputAccounting`, `nativeFieldsRef`.
- Adapter: `crates/remuda-driver/src/usage/` — `UsageEvent` (`mod.rs:103`),
  `UsageAggregator` (`mod.rs:215`, dedups by native id, **rejects** cumulative snapshots at
  `mod.rs:231`/`:269`), `to_usage_payload` (`mod.rs:328`), price table `prices.rs:14`
  (`PRICE_TABLE_REVISION = 1`, `PRICE_TABLE_UPDATED = "2026-09-14"`).
- **Status (verbatim from `docs/design/usage-adapter.md:3`): "模块已落地、已测，尚未接入任何
  driver 的 tail"** — the only live emitter in the whole repo is `claude-print` reading
  `total_cost_usd` from the stream-json `result` frame
  (`crates/remuda-driver/src/claude_print.rs`, `usage_from_result`). Claude transcript tail,
  codex rollout tail, grok `usage.json`/`updates.jsonl` are all on the TODO list
  (`usage-adapter.md:112-118`).
- **The Hub neither stores nor aggregates usage.** A grep for `usage` across
  `crates/remuda-hub/src/` returns nothing; usage exists only as a journal observation,
  rendered per-session in `web/src/features/session/usage.ts` + `UsageFooter.tsx`.
- Accuracy caveats already recorded (`usage-adapter.md:101-110`): Haiku estimate runs ~7 %
  under the native bill; Opus-5/Sonnet-5 rows are `Provisional` and one measured frame was
  off by **1.79×**; cache-write TTL bucketing is the largest sensitivity; grok's
  `usage.json` body has never been observed.

### 2.3 429 / overload / quota surfaces

| Source | What exists | Verdict |
| --- | --- | --- |
| Claude | `rate_limit_event` stdout frame typed at `crates/remuda-claude-wire/src/types.rs:369-374,1287`; mapped to a **Diagnostic lifecycle** observation (`crates/remuda-driver/src/claude_print.rs:764-766`, `crates/remuda-journal/src/claude.rs:217-226`); `rate_limit_info/status` lifted into the envelope `status` field (`crates/remuda-journal/src/claude.rs:982`) | **Visible but inert** — a log line, not a scheduler input, and only on the `claude-print` path that D-028 is retiring. |
| Codex | `account/rateLimits/updated` notification variant decoded at `crates/remuda-codex-wire/src/notification.rs:106-107,333` | **Dead end** — grep shows **zero consumers** anywhere in the workspace. |
| Grok / agy / gemini | — | **GAP** |
| Hub | `crates/remuda-hub/src/rate_limit.rs:69` `AuthRateLimits`, wired at `lib.rs:258,371` | **Unrelated** — per-IP/global login brute-force buckets only. |
| Push | `code == 429 || 5xx` → retry (`crates/remuda-push/src/notify.rs:257`) | **Unrelated** — APNs/WebPush delivery. |
| Provider profile | `/test` reports `reachable/ok/status/latencyMs` on demand | **Not persisted, not periodic, not per-model.** |

### 2.4 The scheduling gaps, named

1. **No supply declaration.** Nowhere in protocol, store, or config can a user say "I have
   N Gemini units/day, M GPT units, resets at T, max K concurrent". The word does not exist
   in the repo (`grep -i quota|supply|配额` over `docs/design` returns only attachment
   quotas and login rate limits).
2. **Concurrency is keyed by host, never by account.** `maxInstances` is the only ceiling
   (`placement.rs:200`). One gateway profile is shared by *all* hosts (`scope: universal`),
   so the resource that actually saturates — the provider account — has no ceiling at all.
   This is the single most important missing primitive.
3. **No cost in the loop.** Price table exists but is never consulted at placement time, and
   costs are not aggregated per host/project/day.
4. **No history.** No per-(host, harness, model) latency, success rate, or throughput record
   to learn from; `select_hosts` is stateless.
5. **No queue.** `RESOURCE_LIMIT`/`Unsatisfiable` is a rejection (`placement.rs:147-152`),
   not an admission queue — a full fleet means the caller retries by hand.
6. **`resources` (cpu/mem) collected and stored but unused** — the cheapest available
   improvement to `load_of`.
7. **Effort has no declared cost multiplier.** The tiers are labels
   (`web/src/features/session/effort.ts:44-84`) with prose descriptions
   ("最高档 · 慢且贵"); no machine-readable relative cost that a scheduler could trade off.

---

## 3. What a per-project configuration needs, and where to put it

### 3.1 The required content (derived from what the manual loop encodes today)

The `<coord-scratch>/` scripts are the ground truth for what a project coordinator must
know. From `remote-spawn.sh`, `spawn-claude.sh`, `rgate3.sh`, `remote-gate.sh`, `poll-all.sh`:

| # | Needs to be configured | Evidence it's per-project today | Today's home |
| --- | --- | --- | --- |
| 1 | Repo identity + base branch | `git branch -f main origin/main`, `origin/main` base for every worktree | hardcoded in each script |
| 2 | **Per-host workspace path for the same repo** | local `<repo>` vs remote `<remote-agents>/repo` | hardcoded (`remote-spawn.sh:7`, `spawn-claude.sh:6`) |
| 3 | Worktree root + branch naming | `remuda-wt/<name>` / `<remote-agents>/wt/<name>`, branch `wt/<agent>/<topic>` | `path_guard.rs` pins the local root; the remote root is script-local |
| 4 | Per-agent build env | `CARGO_TARGET_DIR=/tmp/…-target-<name>`, `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=16`, `OPENSSL_DIR`, `RUSTFLAGS` | `--env` flags in `remote-spawn.sh` |
| 5 | **Model profile per role** | `spawn-claude.sh` flavors `opus → claude-opus-5[1m]` vs `seed → passthrough/<gateway-model-B>[1m]`; `remote-spawn.sh` flavors `es1 → <gateway-model-A>[1m]` vs `seed → <gateway-model-B>[1m]` | a bash `case` statement |
| 6 | Provider/settings binding | `--settings $HOME/.claude/settings.relay.json` on the local relay path only | hardcoded per script |
| 7 | Gate lane + serialization | two lanes with `mkdir` lockdirs `/tmp/coord-queue-lane{1,2}.lock` and a shared land lock `/tmp/coord-land.lock`, CAS retry when main moves (`rgate3.sh`) | bash |
| 8 | Completion protocol | `grep -E '^\s*(●|•)?\s*(DONE|BLOCKED)[ -]'` with echo-suppression (`poll-all.sh`) | bash, duplicated by `wait --until 'line:'` |
| 9 | Worker rules | D-031 no-deploy/no-tunnel, never push, explicit pathspecs | prose in `coordinator-guide.md:50-54` and each brief |
| 10 | Approval posture | `--dangerously-skip-permissions` / `--always-approve` chosen per spawn | bash |
| 11 | Bot topic → project binding | — | does not exist |

### 3.2 Candidate homes

**(a) Promote `Space` to a control-plane entity.**
*Pros*: the name and the UI already exist; users already think in spaces; tab/selection
prefs are already modeled (`web/src/features/spaces/store.ts:27-37`).
*Cons*: **wrong cardinality** — a Space *is* `(hostId, workspaceId)` and D-024 deliberately
refuses to merge across hosts, so it can never represent one project on two machines.
It is also purely `localStorage` today; promoting it means inventing persistence,
auth scoping, and sync for a concept that would still need a parent.

**(b) Extend `Workspace`.**
*Pros*: already server-side, already carries `rootPath`, `repository`, `writePolicy`,
`writerLeases` (protocol §2.2), already has REST CRUD (`workspaces.rs:14`) and a Node-side
allowlist.
*Cons*: protocol §2.2 fixes `hostId` on a Workspace. Project fields would be duplicated
across every host's workspace row with no owner of truth, and any edit would need an
N-host two-phase commit (the registry is already prepare/commit per host, `workspaces.rs`).
Works only if you accept "project == one host", which contradicts the vision.

**(c) New Hub entity `Project` (recommended core).**
*Pros*: correct cardinality (Project 1:N Workspaces spanning hosts); every field it needs
already exists as a foreign key (hostId, workspaceId, providerProfileId, labels); it gives
the bot the routing key it is already typed for (`defaultProject`,
`web/src/features/bots/channels.ts:24`); it gives placement a real constraint to add next
to `Placement::{Host,Labels,Any}`; and there is a clean, precedented injection point —
`merge_host_launch_defaults` (`http.rs:516`) already folds *host* defaults into a create
spec, so a `merge_project_defaults` in the same slot is a ~1-function change on the hot path.
*Cons*: a new table + migration (`store.rs` uses `ensure_column`/table-create migrations
already), a new REST surface, new agent-scope rules (D-017: can an Agent-origin caller pick
a project?), new OpenAPI + generated web client (`gen-api-current` is a gate step), and a
new UI. Realistically the largest single item, but nothing about it is novel.

**(d) Repo file `.remuda/project.toml` (recommended companion, advisory only).**
*Pros*: version-controlled with the code it governs; reviewable in the same PR; already the
repo's own pattern — `scripts/ci/gate.sh` is authoritative *and is read from the merged
temporary worktree*, so "a branch that changes the gate is tested with that version"
(`docs/design/remuda-cli.md:134-137`). Brief templates, completion-line convention, build
env, per-agent `CARGO_TARGET_DIR` pattern, worker rules and the gate command belong here.
Zero Hub changes; the coordinator CLI reads it locally.
*Cons*: **must never carry authority.** D-017 says an agent instance is not a trusted
principal, and workers can write to the repo — so host allowlists, provider bindings,
permission posture and secrets cannot live here. The Hub also cannot read it without a Node
round-trip, so it is useless for placement.

**(e) `remuda.toml` `[project.<name>]` blocks.**
*Pros*: operator-owned, zero new API, mirrors the existing `[dispatcher]` section
(`crates/remuda/src/config.rs:115-136`).
*Cons*: requires a Hub restart to change, no API for the web UI or the bot, no per-project
audit trail, and it puts a fleet-wide concern in a process-local file. Fine as a *bootstrap*
for the first project, wrong as the destination.

**(f) Model supply: extend the provider model catalog (recommended).**
Supply is per-**account**, not per-project, so it belongs on the provider profile, and the
catalog is already a structured, optional-field, migrated-in-place array
(`{id, enabled, label, contextWindow, tags[], surfaces[]}`; `docs/design/providers.md`
"Model catalog", legacy `["id"]` lists auto-migrated). Adding `{role, maxConcurrency, rpm,
tpm, dailyBudgetUsd, resetWindow, priority}` per model entry is additive, needs no new
entity, and immediately gives the scheduler the cross-host ceiling it lacks (§2.4 #2).
A project then *references* roles ("workhorse", "reviewer"), not model ids — which is
exactly the user-differs-in-supply requirement: same project config, different supply
declaration, different resolved model.

### 3.3 Recommended least-invasive shape

1. **`Project` entity in the Hub, thin** — nothing but bindings and defaults:
   `{projectId, name, repoRemote?, defaultBaseBranch, members: [{hostId, workspaceId, role}],
   placement: {labels[]|hostIds[]}, provider: {delegation, profileId?} , modelRoles:
   {workhorse, reviewer, cheap}, defaultEffort, permissionPosture, briefRef?}`.
   Everything it points at already exists. Routes: `GET|POST /v1/projects`,
   `GET|PATCH|DELETE /v1/projects/{id}`.
2. **One new placement variant** `Placement::Project { project_id }` in
   `placement.rs:17`, resolving to the project's member hosts before the existing
   filter/rank runs — no change to `select_hosts`' shape.
3. **One new defaults fold** `merge_project_defaults` beside `merge_host_launch_defaults`
   (`http.rs:516`), precedence: explicit request > project > host > global. Reuse the same
   "replace, don't concatenate" rule already documented there.
4. **Supply on the provider catalog** (§3.2f) + a second admission check in `consider()`
   keyed by `(providerProfileId, modelId)` alongside the existing `maxInstances` check
   (`placement.rs:200`).
5. **`.remuda/project.toml` in the repo** for brief template, completion line, build env,
   gate command and worker rules — read by the CLI, never by the Hub, never authoritative.
6. **Bot routing**: `RouteDefaults` (`dispatcher.rs:246`) becomes a lookup
   `sessionKey/chat_id → projectId → defaults`, replacing the single global triple. Add
   `/project <name>` next to `/host` `/agent` `/model` in `ExplicitCommand`
   (`inbound.rs:304`).

### 3.4 Prerequisites that block the vision regardless of where config lives

| Blocker | Location | Why it blocks |
| --- | --- | --- |
| Bot cannot answer interactions | `interactions.rs:154` requires `InputOrigin::Human` | T1's whole value is the owner approving from chat; today that returns 403 (measured, `feishu-session-card.md §1`). |
| Dispatcher journal mapping matches non-existent kinds | `remuda-feishu/src/hub_api.rs:177` | T1 cannot report progress or completion. Two-line fix, high leverage. |
| `TicketStore` is in-memory | `remuda-feishu/src/tickets.rs:122` | Approvals do not survive a restart. |
| Usage never emitted outside `claude-print` | `usage-adapter.md:3,112-118` | No cost/throughput signal for any scheduler, and `claude-print` is the path being retired. |
| No cross-host provider concurrency ceiling | `placement.rs:200` (host-only) | A supply-aware scheduler cannot exist without it. |
| Codex rate-limit notification has no consumer | `remuda-codex-wire/src/notification.rs:106` | Free 429 signal currently discarded. |

---

## 4. Appendix — the manual loop, mapped

| Manual step (today) | Script | Product surface that already covers it |
| --- | --- | --- |
| Create remote worktree + herdr pane + start agent with flavor-selected model | `<coord-scratch>/remote-spawn.sh`, `spawn-claude.sh` | `remuda worktree create` + `remuda instance create --kind --driver pty --worktree --prompt-file` (covers everything **except** the flavor→model policy) |
| Auto-confirm the folder-trust dialog | `grep 'trust this folder' && send-keys enter` | D-022 `auto_trust_registered_workspaces` (`crates/remuda/src/config.rs:101`) |
| Poll N agents for the first `DONE`/`BLOCKED` | `poll-all.sh` | `remuda instance wait --until 'line:(?m)^DONE'` with the bullet-tolerant matcher — **but there is no multi-instance wait**; the script's "first among N" has no equivalent |
| Per-lane remote gate with lock dirs, CAS onto main, retry when main moved | `rgate3.sh`, `remote-gate.sh` | `remuda merge <branch> --gate` does isolated-merge + shared gate + CAS + push (exit 3 = lost CAS). **No lane/queue serialization** — the lock dirs are pure T2 scheduling and have no product equivalent |
| Choose which of two lanes / which host a branch is gated on | `rgate3.sh` `$LANE` → `<remote-host>` | **GAP** |
| Pick `es1_orange_o50[1m]` vs `<gateway-model-B>[1m]` vs `claude-opus-5[1m]` per worker | bash `case` | **GAP** — this is the model-profiling feature, in its entirety |

---

## 5. File index for follow-up work

- Placement / scheduling: `crates/remuda-hub/src/placement.rs`
- Provider resolution & scoping: `crates/remuda-hub/src/provider_resolve.rs`, `providers.rs`, `provider_models.rs`
- Host registry & launch defaults: `crates/remuda-hub/src/registry.rs`, `hosts.rs`, `ssh_hosts.rs`, `inventory.rs`
- Workspaces: `crates/remuda-hub/src/workspaces.rs`
- Instance create hot path: `crates/remuda-hub/src/http.rs:494-674`
- Records/schema: `crates/remuda-hub/src/store.rs:175` (Host), `:287` (Instance)
- Wire types: `crates/remuda-protocol/src/launch.rs`, `observation.rs`, `entities.rs`, `enums.rs`, `capabilities.rs`
- Usage & prices: `crates/remuda-driver/src/usage/`
- Bot: `crates/remuda-feishu/src/`, `crates/remuda/src/cmd/dispatcher.rs`
- Coordinator CLI: `crates/remuda/src/cmd/{merge,doctor,agents,worktree,journal_diff,hook,instance,fleet,mcp}.rs`
- Docs: `docs/design/{decisions,coordinator-guide,remuda-cli,providers,usage-adapter,remote-modes,protocol,native-pty-first,feishu-session-card,dogfood}.md`
- Skill: `skills/remuda/SKILL.md` + `skills/remuda/sections/`
