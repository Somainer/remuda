# Prior art for Remuda's three-tier coordinator and model-supply scheduler

**Date:** 2026-09-15 · **Author:** prior-art survey subagent · **Repo state:** read-only survey of
`<repo>` at `origin/main` (`3f68061`); nothing
edited, built or run. Network access was WebFetch/WebSearch on public docs only. Scratch dir:
`<coord-scratch>/coordinator/scratch-priorart/`.

**What this answers.** The owner's vision is a three-tier hierarchy — Tier 1 统领 Coordinator behind a
chat bot, Tier 2 per-project Coordinators owning their workspaces/hosts/policies and deciding
remote-vs-local, harness and model, Tier 3 workers (claude/codex/grok/agy). Cross-cutting: **model
profiling**, where *capability* is common knowledge the coordinator already has but *supply* (quota,
rate limits, concurrency, cost, reset windows) is user-declared and differs per user. Today that loop
is run by hand by a Claude Code session with shell scripts under `<coord-scratch>/`.

Sections: **(A)** hierarchical orchestration + human-facing accountability; **(B)** model supply and
capability routing; **(C)** what transfers to Remuda given it drives *CLI harnesses it cannot
instrument at the HTTP layer*; **(D)** recommended vocabulary for supply, capability and the task spec.

The single most load-bearing finding is in §C.2: **two of the three harnesses already emit
structured supply telemetry**, and one of them (Codex) emits it in exactly the shape a scheduler
wants. Remuda does not have to invent the supply vocabulary — it should adopt Codex's.

---

## Part A — Hierarchical / multi-agent orchestration with a human-facing bot

### A.1 Comparison table: how systems structure orchestrator → sub-orchestrator → worker

| System | Tier shape | How a task is handed down | What comes back up | Shared state | Sub-orchestrators? | Notes for Remuda |
|---|---|---|---|---|---|---|
| **Anthropic multi-agent research system** | Lead agent → subagents → CitationAgent | A *task description* containing "an objective, an output format, guidance on the tools and sources to use, and clear task boundaries" | Subagents "return a list [of findings] to the lead agent so it can compile a final answer" | Lead saves its plan to **external memory** because context can exceed 200k and be truncated | No (flat, one level) | The four-field task description is the best-specified task spec in public prior art. Adopt verbatim. |
| **Claude Code subagents** | Main session → subagent (≤3 levels deep, ≤20 concurrent, configurable) | A delegation prompt Claude writes + the agent's markdown body as system prompt; `tools`/`model`/`permissionMode`/`maxTurns`/`effort`/`isolation: worktree` in YAML frontmatter | A **summary** + an agent ID; "Claude doesn't see intermediate tool calls and results" | None — non-fork subagents get no conversation history ("clean slate") | Yes, nesting to 3 layers | This *is* the Tier-2/Tier-3 shape, already productised, including `isolation: worktree` and per-agent `model`. |
| **Claude Code agent teams** (experimental, `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`) | Team lead → teammates (peer sessions) | Spawn prompt; teammates load CLAUDE.md/MCP/skills but *not* the lead's history | Idle notification carrying the teammate's final answer; failures notify the lead with the error text | **Shared task list** (`~/.claude/tasks/{team}/`) with pending/in-progress/completed + dependencies + file-locked claiming; **mailbox** JSON per agent | **No — "No nested teams: teammates cannot spawn their own teammates. Only the lead can manage the team."** | Closest existing analogue to Tier 1↔Tier 2, but the explicit no-nesting limitation is exactly the gap Remuda's three tiers fill. |
| **OpenAI Agents SDK — handoffs** | Triage agent → specialist (control transfers) | A tool call named `transfer_to_<agent_name>`, optionally typed via `input_type` (e.g. `EscalationData{reason}`) with an `on_handoff` callback | Control, not a result — the specialist becomes the active agent | Full prior conversation by default; `input_filter` (e.g. `remove_all_tools`) trims what carries over | Chainable | The typed handoff payload is the right model for "coordinator states *why* it is delegating", which is what a ledger entry needs. |
| **OpenAI Agents SDK — agents-as-tools** | Manager keeps control, calls specialists via `Agent.as_tool()` | Tool arguments | Tool result | Manager owns the thread | Yes | SDK explicitly frames the choice: LLM-orchestration for open-ended work vs **code-orchestration** for "speed, cost efficiency, and reliability". Remuda's scheduler should be code, its briefing should be LLM. |
| **LangGraph supervisor / `langgraph-supervisor`** | Supervisor → workers; supervisors can supervise supervisors ("research teams and writing teams report to a top-level supervisor") | `create_handoff_tool` produces `delegate_to_<worker>`; routing via `Command(goto=…, update=…)` | Worker messages merged back | `output_mode` picks **`"full_history"`** (all worker messages) vs **`"last_message"`** (final response only) | **Yes — explicit multi-level hierarchies** | `output_mode` is the cleanest public naming of the context-budget knob between tiers. Remuda needs the same switch per tier edge. |
| **CrewAI hierarchical process** | Manager agent (`manager_llm`) → crew members | Manager allocates by role/capability | Manager performs "result validation… to ensure they meet the required standards" | Sequential task flow under managerial oversight | Single level | Notable constraint: tools belong to agents, not the manager; "Delegation is now disabled by default to give users explicit control." Mirrors Remuda's rule that the coordinator gates but does not fix. |
| **Magentic-One / AutoGen `MagenticOneGroupChat`** | Orchestrator → WebSurfer / FileSurfer / Coder / ComputerTerminal | Inner loop assigns "who acts next, what instruction" | Per-step progress answers | **Two ledgers**: Task Ledger (facts, guesses, plan) in the outer loop; Progress Ledger (complete? progressing? who's next?) in the inner loop | Single level | The **stall counter** ("Stall count > 2" → exit inner loop, re-plan, update Task Ledger) is the single best runaway guard in the survey. |
| **GitHub Copilot coding agent** | Human → agent (one repo, one session) | Issue assignment, chat, `@copilot` PR comment, Teams/Slack/Jira/Linear, MCP, scheduled automations | **Draft pull request** + commit history + session logs | The repo and PR are the state | No | The product answer to "what is the accountability artifact": a draft PR the human must approve. It "cannot approve its own pull requests"; max 59-minute session. |
| **Devin** | Human → session | Session prompt | Session + PR | Session insights | No | Only system that publishes an explicit *effort* unit (ACU) and size buckets, see A.3. |
| **Nicholas Carlini's 16 parallel Claudes (C compiler)** | **No coordinator at all** — decentralised | Agents self-select: "Claude picks up the 'next most obvious' problem" | Push to upstream git | **Lock files in `current_tasks/`**; git itself as the mutex; READMEs/progress files as the ledger | N/A | 16 agents, ~2 weeks, 2B input + 140M output tokens, $20,000. The counter-example: it works, but only because the *gates* (test harness, CI, GCC oracle) were strong. "An agent's DONE is a claim, not a gate" is the same lesson Remuda already encodes. |

### A.2 How accountability is surfaced to a human, in chat

| Mechanism | Who does it | Concrete form | Transferable to a Feishu dispatcher? |
|---|---|---|---|
| **Task ledger** (plan + facts + guesses) | Magentic-One outer loop; Anthropic lead agent's plan in external memory | Structured, re-written on stall | Yes — this is the bot's pinned "what are we doing and why" card. |
| **Progress ledger** (per-step: complete? progressing? who's next? what instruction?) | Magentic-One inner loop | Re-evaluated every step | Yes — this is the digest payload. Emit on change, not on a timer. |
| **Shared task list with states + dependencies** | Claude Code agent teams (`~/.claude/tasks/…`, pending/in-progress/completed, file-locked claiming, auto-unblocking of dependents) | Local files, `Ctrl+T` to toggle | Yes — Remuda already has instances and worktrees; a task list keyed by `(hostId, workspaceId, branch)` is the missing layer. |
| **Draft PR as the accountability artifact** | Copilot coding agent | Branch + draft PR + commit history + logs; human review mandatory before merge | Partially — Remuda's analogue is `remuda merge <branch> --gate --json`, which is stronger (it re-runs gates on the merged tree). The bot should post the *gate report*, not a claim. |
| **Approvals/questions via the human channel** | Claude Code teams: "Teammate permission prompts appear in the lead session, so approve them there yourself"; a teammate "can't approve a permission prompt or supply consent on your behalf" | Permission prompt bubbling | Yes, and Remuda already decided this (D-005: interaction broker, `can_use_tool` frames, **bot never bypasses**). Prior art confirms the design: relayed approval claims from another agent are treated as untrusted input. |
| **Quality gates as hooks at tier boundaries** | Claude Code teams: `TeammateIdle`, `TaskCreated`, `TaskCompleted` — "Exit with code 2 to send feedback and keep the teammate working" / prevent creation / prevent completion | Exit-code protocol | Yes — a Tier-2 coordinator should express its policy as these three hooks, not as prose in a brief. |
| **Cost/usage digest** | Claude Code `/usage`, `/cost`, `/insights`; OTel metrics; Devin Session Insights | Per-model token + cost breakdown, attribution to skills/subagents/MCP servers, behaviour flags at ≥10% of recent usage | Yes — but see §C on why Remuda must compute it from files, not headers. |
| **Escalation on stall** | Magentic-One stall counter; Claude Code "Agents stopping early… tell it to keep going" | Re-plan or ask | Yes — stall→re-plan→escalate-to-owner is the bot's third message type after "started" and "landed". |

### A.3 Known failure modes, with sources and the guard each system uses

| Failure mode | Evidence | Guard used in prior art |
|---|---|---|
| **Runaway fan-out** | Anthropic: early agents were "spawning 50 subagents for simple queries" | Effort heuristics *written into the prompt*: "Simple fact-finding requires just 1 agent with 3-10 tool calls, direct comparisons might need 2-4 subagents with 10-15 calls each, and complex research might use more than 10 subagents". Claude Code teams: "Start with 3-5 teammates… If you have 15 independent tasks, 3 teammates is a good starting point." Hard caps: ≤20 concurrent subagents, ≤3 nesting levels. |
| **Cost blowup** | Anthropic: "Agents typically use about 4× more tokens than chat interactions, and multi-agent systems use about 15× more tokens than chats"; multi-agent "require tasks where the value of the task is high enough". Claude Code: "Agent teams use approximately 7x more tokens than standard sessions when teammates run in plan mode". C-compiler run: $20,000 | Hard budget: `claude -p --max-budget-usd 5.00` — "Spend from subagents counts toward the cap. Once spend reaches the cap, spawning another subagent fails with `Budget limit reached`, and Claude Code stops background subagents that are still running". Also `--max-turns` (exits with an error at the limit). Devin: keep sessions under 10 ACUs. |
| **Duplicated work / gaps between workers** | Anthropic: vague instructions caused subagents to "misinterpret the task or perform the exact same searches as other agents" | Detailed decomposition with explicit boundaries; Claude Code: "Two teammates editing the same file leads to overwrites. Break the work so each teammate owns a different set of files"; C-compiler: lock files in `current_tasks/` with git as the mutex. |
| **Lost context between tiers** | Claude Code subagents: non-fork subagents get no conversation history — the documented "Clean Slate Problem"; agent teams: "The lead's conversation history does not carry over" | Put it in the spawn prompt (Claude Code "Give teammates enough context"); or use a **fork** to inherit everything; or LangGraph `output_mode` / OpenAI `input_filter` to choose how much history crosses the edge. |
| **Silent scope drift** | Remuda's own `docs/design/coordinator-guide.md`: "a diff that has grown beyond the files the task named is the most common reason to reject, and **no gate catches it**" | Human/coordinator reads `git diff --stat main...<branch>` before merging. Copilot: agent "can only make changes in the repository specified when you start a task". Declare owned paths in the task spec and diff against them. |
| **Stalling / no progress** | Magentic-One | Progress Ledger asks "is progress occurring?"; "Stall count > 2" exits the inner loop and re-plans. |
| **Path dependency / prompt fragility across tiers** | Anthropic: "small changes to the lead agent… cascade into large behavioral changes" in subagents | Evals on ~20 representative queries; LLM-as-judge rubric (factual accuracy, citation accuracy, completeness, source quality, tool efficiency); full production tracing of "agent decision patterns and interaction structures". |
| **Error compounding in long runs** | Anthropic: "Minor system failures can be catastrophic for agents"; "errors compound" | Checkpoint/resume ("systems that can resume from where the agent was"); graceful degradation — tell the agent a tool is failing and let it adapt; **rainbow deployments** so a redeploy doesn't kill running agents. |
| **Over-reporting / coordination noise** | Anthropic: agents "distracting each other with excessive updates" | Return summaries, not transcripts. C-compiler: "Minimal logging to stdout; aggregate summaries pre-computed; ERROR messages on single line for grep". |
| **Anchoring on the first hypothesis** | Claude Code teams use-case: "Sequential investigation suffers from anchoring" | Deliberately adversarial teammates: "Have them talk to each other to try to disprove each other's theories, like a scientific debate." |
| **Lead does the work itself / stops early** | Claude Code teams troubleshooting: "Sometimes the lead starts implementing tasks itself instead of waiting for teammates"; "The lead can stop early too, deciding the team is finished before all tasks are actually complete" | Explicit instruction, plus `TaskCompleted` hook returning exit 2. |
| **Task-status lag blocking dependents** | Claude Code teams limitation: "teammates sometimes fail to mark tasks as completed, which blocks dependent tasks" | Coordinator verifies the work, not the status flag — which is exactly Remuda's "`DONE` is a claim, not a gate". |
| **Prompt injection through agent-to-agent messages** | Claude Code: subagent reports are scanned, backslashes inserted to "neutralize instruction-shaped patterns"; in auto mode, "an approval claim relayed from another agent" is treated as "untrusted input rather than confirmation from you" | Sanitise worker output before it reaches a higher tier. Magentic-One: run in containers, human in the loop, restrict internet access. |
| **Synchronous bottleneck** | Anthropic: the lead "executes subagents synchronously, waiting for each set… to complete before proceeding," which "simplif[ies] coordination but limit[s] parallelism" | Known tradeoff; Remuda's `instance wait` per agent with independent waits already avoids it. |

---

## Part B — Model supply / capability routing

### B.1 Comparison table: routers and gateways

| Capability | **LiteLLM Router** | **OpenRouter provider preferences** | **Portkey configs** |
|---|---|---|---|
| Unit of supply | A *deployment* under a `model_name` alias, with `litellm_params` | A *provider endpoint* for a model slug | A *target* (provider + params) |
| Declared limits | `tpm`, `rpm`, `max_parallel_requests`, `weight`, `order`, `region_name`, `model_info.base_model` | — (OpenRouter knows provider capacity itself) | `weight` per target |
| Routing strategies | `simple-shuffle` (default; uses rpm/tpm, else random, with `weight` multiplier), `least-busy` (fewest active calls), `usage-based-routing-v2` (lowest TPM this minute, Redis-tracked), `latency-based-routing` (lowest observed response time, TTL-windowed), `cost-based-routing` (lowest cost from the cost map or `input_cost_per_token`) | Default: filter providers with no outage in the last 30 s → weight by **inverse square of price** ("lower-cost options 9× more likely than 3× costlier alternatives") → remainder as fallbacks. `sort: price\|throughput\|latency`, `order: [...]`, `only`/`ignore`, `quantizations`, `max_price`, `preferred_min_throughput`, `preferred_max_latency` (p50/p75/p90/p99). Setting `sort` or `order` **disables** load balancing. | `strategy.mode`: `fallback`, `loadbalance`, `conditional` (routes on query params/metadata); nestable |
| Pre-flight filtering | `enable_pre_call_checks=True`: drops deployments whose **context window** is too small for this prompt, whose **rpm/tpm** would be exceeded, or whose `region_name` doesn't match | `require_parameters: true` (only providers supporting all requested params), `data_collection: deny`, `zdr` | `on_status_codes` per target |
| Failure handling / cooldown | Cooldown on 429 immediately; on ">50% failures in current minute"; on non-retryable 401/404/408. `allowed_fails` (default 3/min), `cooldown_time` (default 5 s), per-deployment overrides (`cooldown_time: 0` disables), `AllowedFailsPolicy` per exception type (e.g. `RateLimitErrorAllowedFails`) | `allow_fallbacks: true` walks down the provider list on error/rate-limit; `false` fails immediately | `retry` with `attempts` + `on_status_codes`; `request_timeout` |
| Retries | `num_retries` resolved in priority order (request header → body → deployment → router); `retry_after`; `RetryPolicy` per error class (e.g. `AuthenticationErrorRetries=0`) | Implicit in fallback walk | `retry.attempts` |
| Fallback classes | `fallbacks`, **`context_window_fallbacks`**, **`content_policy_fallbacks`**, `default_fallbacks`; `order` creates retry *tiers* within a group | `:nitro` = sort by throughput + priority tier; `:floor` = sort by price + flex tier | Nested `fallback` strategies |
| Budgets | `max_budget` + `budget_duration` (`30s`/`30m`/`30h`/`30d`/`1mo` calendar reset) at proxy/team/user/key/team-member scope; `model_max_budget: {model: {budget_limit, time_period}}`; `tpm_limit`/`rpm_limit`/`max_parallel_requests` per key/user/team; `token_rate_limit_type: input\|output\|total`; `soft_budget` for alerts; cost **reservation** before dispatch and `fail_closed_budget_enforcement` | `max_price` ceiling | Cache + timeout |
| Output-length estimation | `default_estimated_output_tokens` and `…_per_model`, resolved request `max_tokens` → key-per-model → key → team-per-model → team → built-in | — | `default_params` |

### B.2 Provider rate-limit semantics *as a client sees them*

| | **Anthropic (Claude API)** | **OpenAI** | **Gemini API** |
|---|---|---|---|
| Dimensions | RPM, **ITPM**, **OTPM**, per model class; separate Batch API limits; separate Managed Agents + Files limits; separate fast-mode limits | RPM, RPD, TPM, TPD, IPM, audio-minutes/min | RPM, TPM, RPD (+ IPM, TPD for some models) |
| Scope | Organization-level, per model; workspaces can be capped lower; shared across `inference_geo` values | Per model, per tier; "Some model families have shared rate limits" | **"per project, not per API key"**, per model |
| Algorithm | **Token bucket** — "capacity is continuously replenished up to your maximum limit, rather than being reset at fixed intervals". "a rate of 60 requests per minute (RPM) might be enforced as 1 request per second" | Not specified; any dimension first to exhaust triggers 429 | Not specified; also rolling **10-minute spend windows** by tier |
| Cache accounting | **Cache-aware ITPM**: `input_tokens` ✓, `cache_creation_input_tokens` ✓, `cache_read_input_tokens` ✗ (except Haiku 3.5). "With a 2,000,000 ITPM limit and an 80% cache hit rate, you could effectively process 10,000,000 total input tokens per minute" | Combined TPM | TPM is input tokens |
| `max_tokens` effect | None — "OTPM rate limits are evaluated in real time… The `max_tokens` parameter does not factor into OTPM rate limit calculations" | Reserved against TPM (hence LiteLLM's output estimator) | — |
| Headers | `retry-after`, `anthropic-ratelimit-requests-limit/-remaining/-reset`, `anthropic-ratelimit-tokens-limit/-remaining/-reset`, `anthropic-ratelimit-input-tokens-limit/-remaining/-reset`, `anthropic-ratelimit-output-tokens-limit/-remaining/-reset`, `anthropic-priority-*` (Priority Tier), `anthropic-workspace-id`, `request-id`. Resets are **RFC 3339 timestamps**; remaining token counts are **rounded to the nearest thousand** | `Retry-After`, `x-ratelimit-limit-requests`, `x-ratelimit-limit-tokens`, `x-ratelimit-remaining-requests`, `x-ratelimit-remaining-tokens`, `x-ratelimit-reset-requests`, `x-ratelimit-reset-tokens`, `x-ratelimit-limit-project-tokens` (+ remaining/reset) | 429 `RESOURCE_EXHAUSTED` |
| 429 vs 5xx | `429 rate_limit_error` = rate limit **or** tier monthly spend cap (the spend-cap variant has **no `retry-after`** and carries `error.details.error_code = "enforced_spend_limit_reached"`); `400 invalid_request_error` = a *self-set* spend limit; `500 api_error` = internal; **`529 overloaded_error` = "The API is temporarily overloaded"**, i.e. a *fleet-wide* condition, not your quota; also "acceleration limits" 429s on a sharp usage increase — "ramp up your traffic gradually" | 429 on any exhausted dimension | 429 |
| Tiers | Evaluation → Start → Build → Scale → Custom; monthly spend caps $500 / $1,000 / $200,000. Example Build-tier per-model: Opus 5 = 5,000 RPM / 5M ITPM / 1M OTPM; Fable 5.x = 2,000 / 1.5M / 300k. Rate limits are "maximum allowed usage, not guaranteed minimums" | Free → Tier 5, $100–$200,000 monthly | Free / Tier 1 ($250) / Tier 2 ($2,000) / Tier 3 ($20,000+); "Specified rate limits are not guaranteed and actual capacity may vary" |
| Family bucketing | Combined buckets: Opus 4.8/4.7/4.6/4.5 share one; Opus 5 separate; Sonnet 4.6/4.5 share one; Sonnet 5 separate; Fable 5.1+5 share one | "shared rate limits" for some families | per model |

**Subscription (non-API) supply — the case Remuda actually lives in.** Claude Code on a
subscription is metered by *windows*, not by RPM/TPM, and the client sees no headers at all:

- "each member's Claude Code usage draws from a per-seat allowance that **resets on a rolling
  five-hour window and a weekly window**", shared with Claude chat and Cowork, sized by seat tier.
- The distinction that matters for scheduling: **"You've hit your session limit" / "You've hit your
  weekly limit"** is "a seat-based usage window… **shared across all models, so the developer can't
  restore access by switching models with `/model`**", whereas after **"You've hit your Opus limit"
  or "You've hit your Sonnet limit"**, "switching to a model outside that family with `/model` does
  keep the developer working."
- Recovery options are enumerated: usage credits (`/usage-credits`), or **wait for the reset and
  continue automatically** — `autoContinueAtUsageLimit`, controllable fleet-wide via managed settings.
- Cache TTL is supply-relevant: "The lifetime is an hour on a subscription and drops to five minutes
  once you're drawing on usage credits; on an API key or cloud provider, it's five minutes by default."
- Per-user planning numbers for API orgs (useful as a sanity table): 1–5 users → 200k–300k TPM and
  5–7 RPM per user; 100–500 users → 15k–20k TPM and 0.37–0.47 RPM per user.

**Effort as a supply unit.** Devin bills in **ACUs** ("how much compute Devin consumed during the
session… Lower ACU usage for a given task generally indicates a more efficient session"), covering
"planning, context gathering, task execution, browser actions, code execution" plus VM time. Session
Insights buckets sessions XS (≤2 ACU or ≤2 messages) / S (≤5) / M (≤10) / L (≤20) / XL (>20), and
**flags L and XL as problematic**, recommending they "be subdivided into smaller, more targeted
subtasks". This is the only published prior art that treats *task size* as a first-class,
measured-after-the-fact scheduling signal — exactly what Remuda can compute from its own history.

### B.3 Published model-profile / model-card metadata schemas

| Schema | Fields relevant to capability routing | Notes |
|---|---|---|
| **OpenRouter `/api/v1/models`** | `id`, `canonical_slug`, `name`, `created`, `description`, `context_length`; `architecture{input_modalities, output_modalities, tokenizer, instruct_type, modality}`; `pricing{prompt, completion, request, image, web_search, internal_reasoning, input_cache_read, input_cache_write}`; `top_provider{context_length, max_completion_tokens, is_moderated}`; `per_request_limits`; `supported_parameters` | The most complete public *machine-readable* capability schema. Note that `context_length` appears **twice** — model-level and provider-level — which is precisely Remuda's "gateway model vs native model" problem. |
| **LiteLLM `model_prices_and_context_window.json`** | `max_tokens`, `max_input_tokens`, `max_output_tokens`, `input_cost_per_token`, `output_cost_per_token`, `cache_read_input_token_cost`, `cache_creation_input_token_cost`, `cache_creation_input_token_cost_above_1hr`, `input_cost_per_audio_token`, `input_cost_per_image`, `input_cost_per_image_token`, `input_cost_per_pixel`, `input_cost_per_query`, `input_cost_per_video_per_second`, `output_cost_per_image`, `output_cost_per_pixel`, `output_vector_size`, `litellm_provider`, `mode`, `deprecation_date`, and a family of `supports_*` capability booleans | The de-facto community price/capability table. Its **separation of `max_input_tokens` from `max_output_tokens` from `max_tokens`**, and its **1-hour cache-write price as a distinct field**, both match what Remuda's `crates/remuda-driver/src/usage/prices.rs` already models. |
| **Hugging Face model card metadata** | YAML frontmatter: `language`, `license`/`license_name`/`license_link`, `library_name`, `tags`, `datasets`, `base_model` (+ `base_model_relation: adapter\|merge\|quantized\|finetune`), `new_version`, `pipeline_tag`, `model-index` (task/dataset/metrics/source) | Governance/provenance-oriented, not routing-oriented. The transferable idea is `base_model_relation` and `new_version` — i.e. **aliasing and succession** between model ids, which Remuda needs when a gateway renames or silently substitutes a model. |
| **Anthropic model config surface** (as a capability vocabulary, from the API error catalogue) | `effort: low\|medium\|high\|xhigh\|max`; `thinking.type: adaptive\|enabled\|disabled` (4.7+ removed `enabled`; Fable/Mythos are always-on); `tool_choice: auto\|none\|any\|tool` (Fable 5.1 rejects `any`/`tool`); structured outputs; prefill unsupported on 4.6+ | These are *capability booleans that change per model generation and produce 400s if got wrong* — the strongest argument for a capability table rather than hard-coded assumptions. |

---

## Part C — What transfers to Remuda

### C.1 The constraint: Remuda is a *process* supervisor, not an HTTP proxy

Every router in §B.1 works because it sits on the request path: it can count tokens before dispatch,
read `x-ratelimit-remaining-*` after, and cool a deployment down on a 429 status code. Remuda drives
`claude` / `codex` / `grok` CLIs over PTY and stream-json (D-028: native PTY is the default carrier
for all harnesses). It therefore sees:

| Router primitive | What Remuda can actually observe |
|---|---|
| Pre-call token count | **Nothing.** The harness builds the request. Remuda can estimate from the brief + repo size only. |
| `x-ratelimit-remaining-tokens` after each call | **Nothing at the HTTP layer** — but see §C.2, two harnesses report it out-of-band. |
| 429 status code | Text on a screen, a stream-json `rate_limit_event` frame, or a process exit. |
| Cost per request | After the fact, from transcript/rollout/usage files — already built: `crates/remuda-driver/src/usage/` parses Claude transcript `message.usage`, Codex `token_usage_record`, and Grok `usage.json`/`updates.jsonl`/headless frames into a `UsageEvent`, and prices them from a versioned local table (`PRICE_TABLE_REVISION`), with cost **always labelled estimated**. Parity against a real `total_cost_usd` is −7.0% (gate ±10%). |
| Deployment cooldown | Achievable — but the cooling unit is a **credential/account + window**, not an endpoint. |
| Fallback to another deployment | Achievable — but it means *starting a different CLI on a different host*, which costs a fresh context load, not a retried HTTP request. |

The last row is the deepest asymmetry and should shape the whole design: **for Remuda, a fallback is
a migration, not a retry.** LiteLLM can fall back mid-request for free; Remuda would have to re-brief
a new worker with a clean slate ("Clean Slate Problem"). So Remuda's scheduler must be
**admission-control-heavy and failover-light**: spend the intelligence on *choosing well before
spawning*, and treat a mid-flight 429 as "park and resume after `resetsAt`", not "reroute".

That parking behaviour has a direct product precedent: Claude Code's own
`autoContinueAtUsageLimit` — wait for the usage limit to reset and then continue the interrupted
task automatically. A Tier-2 coordinator should do the same at the fleet level, and should tell the
bot it did.

### C.2 The finding: two harnesses already export structured supply telemetry

This repo has already vendored the evidence, and it has not been wired to anything.

**Codex.** `docs/research/cli-help/codex-app-server-schema/v2/GetAccountRateLimitsResponse.json` and
`AccountRateLimitsUpdatedNotification.json` define an `account/rateLimits/read` request and a
**sparse rolling push notification**. The core type is:

```json
"RateLimitWindow": { "usedPercent": int32, "resetsAt": int64|null, "windowDurationMins": int64|null }
```

wrapped in a `RateLimitSnapshot` carrying `primary` + `secondary` windows, `limitId`, `limitName`,
`planType` (`free|go|plus|pro|prolite|team|business|enterprise|edu|…`), `credits{balance, hasCredits,
unlimited}`, `individualLimit` (`SpendControlLimitSnapshot{limit, used, remainingPercent, resetsAt}`),
`spendControlReached`, and `rateLimitReachedType` (`rate_limit_reached | workspace_owner_credits_depleted
| workspace_member_credits_depleted | workspace_owner_usage_limit_reached | workspace_member_usage_limit_reached`).
The response additionally has `rateLimitsByLimitId` — a **multi-bucket view keyed by metered
`limit_id`** — plus `ordinaryUsageAllowed` and a `rateLimitResetCredits` summary with a
`ConsumeAccountRateLimitResetCredit` call (outcomes `reset | nothingToReset | noCredit |
alreadyRedeemed`, idempotency-keyed).

Two contract notes in that schema are worth copying verbatim into Remuda's own protocol:
`ordinaryUsageAllowed` — "Null means unavailable; **clients must not infer recovery from percentages
or reset times**"; and the notification's "Nullable account metadata may be unavailable in a rolling
update and **does not clear a previously observed value**." Both are exactly the kind of rule a
scheduler gets wrong first.

**Claude.** `crates/remuda-claude-wire/src/types.rs:371` already models a `rate_limit_event` stdout
frame (`rate_limit_info`, `session_id`, `uuid`), commented "seen in live probes; not in SDK 0.3.268".
`crates/remuda-driver/src/claude_print.rs:764` currently maps it to a bare diagnostic lifecycle
(`"rate_limit_event"`) and drops the payload. Claude Code also has the interactive `/usage` view
(plan usage bars, 24h/7d toggle, attribution by subagent/skill/MCP, behaviour flags at ≥10%) and
OTel export with `claude_code.token.usage` (attributes `type` ∈ input/output/cacheRead/cacheCreation,
`model`, `query_source` ∈ main/subagent/auxiliary, `speed`, `effort`, `agent.name`, …),
`claude_code.cost.usage`, `claude_code.active_time.total`, and events
`claude_code.api_request` / `claude_code.api_error` correlated by `prompt.id`.

**Conclusion.** Remuda should define exactly one supply primitive — Codex's `RateLimitWindow` — and
fill it from three sources of decreasing quality:

1. **observed-structured** — Codex `account/rateLimits/read` + `AccountRateLimitsUpdated`; Claude's
   `rate_limit_event.rate_limit_info` once its shape is probed (open question, §E).
2. **observed-textual** — the documented Claude Code limit strings (`You've hit your session limit`,
   `You've hit your weekly limit`, `You've hit your Opus limit`, `You've hit your Sonnet limit`,
   `You've hit your monthly spend limit`, `Request rejected (429)`, `Server is temporarily limiting
   requests`, `Budget limit reached`), matched on the PTY screen/journal the way
   `instance wait --until 'line:…'` already matches `DONE`.
3. **declared** — the user's YAML, which is the only source for a supply that has not yet spoken.

Everything the scheduler reasons about should be the *merged* window, with `source` recorded per field.

### C.3 Transfer verdicts

| Idea from prior art | Verdict | How it lands in Remuda |
|---|---|---|
| Anthropic's four-part task description (objective / output format / tool+source guidance / boundaries) | **Adopt verbatim** | The `TaskSpec` in §D.3. Remuda's `skills/remuda/SKILL.md` already enforces the output-format half (`DONE <sha>` + `wait --until 'line:(?m)^DONE'`); the boundaries half is what stops scope drift. |
| Magentic-One Task Ledger + Progress Ledger + stall counter | **Adopt** | Tier-2 keeps both; the Progress Ledger delta is the bot digest payload; stall > 2 → re-plan → escalate. |
| Agent-teams shared task list (states + dependencies + file-locked claiming + auto-unblock) | **Adopt, relocate to the Hub** | Remuda already has the Hub as a serialisation point; a task table keyed to `(hostId, workspaceId, branch)` removes the need for file locks. Fixes the documented "task status can lag" failure by making the coordinator's gate, not the worker's flag, authoritative. |
| Agent-teams `TeammateIdle` / `TaskCreated` / `TaskCompleted` hooks (exit 2 = reject with feedback) | **Adopt as the Tier-2 policy interface** | A project coordinator's policy becomes three executable hooks rather than prose. |
| Claude Code subagent frontmatter (`model`, `tools`, `permissionMode`, `maxTurns`, `effort`, `isolation: worktree`, `skills`) | **Adopt as the worker profile schema** | Remuda already has worktree isolation and per-kind recipes; this is the field list to mirror so a Remuda worker profile and a Claude subagent definition are the same document. |
| `--max-budget-usd` semantics (subagent spend counts toward the cap; exceeding kills background subagents) | **Adopt** | Per-task and per-goal USD cap, enforced by Tier 2 using the existing `UsageAggregator`. Because Remuda's cost is *estimated* (±10% gate), the cap must be advisory-then-hard: warn at the estimate, stop at estimate × 1.15. |
| LiteLLM **pre-call checks** (filter deployments by context window / rpm-tpm / region before selecting) | **Adopt, re-timed** | Becomes **admission control at spawn**: filter supplies by `contextNeed`, remaining window headroom, and host binding before creating the instance. This is the single highest-value transfer, because Remuda cannot re-route mid-flight. |
| LiteLLM cooldowns (`allowed_fails`, `cooldown_time`, per-exception policies) | **Adapt** | Cool a **supply×window**, not an endpoint; cooldown until `resetsAt` when known, exponential otherwise. Never cool on 529-equivalents — that is fleet overload, not your quota, and cooling would wrongly burn a different supply. |
| LiteLLM `context_window_fallbacks` | **Adopt** | Distinct from a quota fallback: "this task needs 1M context" selects a different *capability class* (the repo's provider model catalog already tags `1m` and records `contextWindow`). |
| LiteLLM cost **reservation** before dispatch + `fail_closed_budget_enforcement` | **Adapt** | Reserve an *estimate* against the task budget at spawn; reconcile from `UsageAggregator` at turn boundaries. |
| OpenRouter default inverse-square-of-price weighting | **Reject as default** | Remuda's supply is mostly *included-in-subscription*, where marginal price is 0 until a window is exhausted and then effectively infinite. Price-weighting is the wrong shape; **window headroom × user priority** is the right one (see the owner's own stated ordering: grok > codex > claude-relay > agy). |
| OpenRouter `sort`/`order` disabling load-balancing | **Adopt the principle** | An explicit `harness:` or `supply:` pin in a TaskSpec must disable automatic selection entirely and be reported as such. |
| OpenRouter `require_parameters` | **Adopt** | "Only consider supplies whose model supports every capability this task needs" (e.g. `tool_choice: any`, structured output, 1M context). |
| LangGraph `output_mode: full_history \| last_message`; OpenAI `input_filter` | **Adopt as a per-tier-edge setting** | Tier1↔Tier2 defaults to `last_message` (digest only); Tier2↔Tier3 is *always* `last_message` (a `DONE <sha>` plus a gate report), because a PTY transcript must never be replayed upward. |
| Copilot "draft PR is the artifact; agent cannot approve its own PR" | **Already held; keep** | `remuda merge --gate` re-runs `secret-scan`, `no-tunnel-scan`, fmt, check, clippy, test, web checks, `gen-api-current`, `verify-tree` on the merged tree. Tier 2 must never merge on a worker's claim. |
| Copilot 59-minute hard session cap | **Adopt** | `maxWallMins` in the TaskSpec; a worker that exceeds it is stopped and reported, not silently left running. |
| Devin ACU + XS/S/M/L/XL bucketing, L/XL flagged for subdivision | **Adopt as a post-hoc metric** | Remuda can compute a comparable size from `UsageAggregator` totals + turn counts and feed it back into the next dispatch's `effort` and decomposition. This closes the "用 remuda 迭代 remuda" loop with data rather than vibes. |
| Anthropic effort heuristics as prompt text ("1 agent / 3-10 tool calls" … ">10 subagents") | **Adopt, but as code** | The OpenAI SDK's own guidance is that code-orchestration buys "speed, cost efficiency, and reliability". Fan-out width belongs in the scheduler with a hard cap, not in the coordinator's prompt where it drifts. |
| Rainbow deployments | **Adopt for the Hub** | Remuda upgrades must not kill in-flight PTY workers; Node reconnect + seq-watermark journal replay (D-019) is already the right substrate. |
| OTel metric/attribute names (`claude_code.token.usage` with `type`/`model`/`query_source`) | **Mirror the attribute names** | Free interoperability: if Remuda's own usage payloads carry the same attribute vocabulary, an existing dashboard works on both. |
| CrewAI "tools belong to agents, not the manager" | **Adopt** | Tier-1 and Tier-2 coordinators should hold *dispatch* and *gate* tools, not editing tools. Prevents the documented "lead starts implementing tasks itself" failure. |
| Multi-level supervisors (langgraph-supervisor) vs "No nested teams" (Claude Code) | **This is Remuda's differentiator** | Claude Code teams explicitly cannot nest and are "one team per session", with "no session resumption with in-process teammates". A Hub-persisted, three-tier hierarchy whose state outlives any one session is the actual product gap. |

---

## Part D — Recommended vocabulary

Naming rules used below: **capability** = a property of a *model* the coordinator can know from
public docs; **supply** = a property of a *credential in a time window* that only the user can
declare or the harness can report; **placement** = the scheduler's decision joining a TaskSpec to a
supply, a harness and a host. Field names follow the repo's existing camelCase JSON convention
(`providerBinding`, `defaultGateway`, `contextWindow`).

### D.1 `SupplyProfile` — declared by the user, refined by observation

```yaml
supply:
  - id: sup_codex_pro_personal
    harness: codex                 # claude | codex | grok | agy | gemini
    auth: subscription             # subscription | apiKey | gateway
    accountLabel: "personal pro"   # opaque; never the credential
    planType: pro                  # free|go|plus|pro|team|business|enterprise|unknown  (Codex PlanType)
    scope: universal               # universal | host:<hostId>          (reuses D-021)
    serves:                        # which capability ids this supply can run
      - { model: gpt-5.x-codex, alias: workhorse }
    priority: 10                   # user's preference weight; higher wins ties
    concurrency: { max: 3, inFlight: 1 }
    windows:                       # ← Codex RateLimitWindow, verbatim
      - id: primary                # primary | secondary | weekly | weekly-<family> | monthly
        appliesTo: ["*"]           # or ["opus"] for a family-scoped window
        windowDurationMins: 300
        usedPercent: 41
        resetsAt: 1757900000       # unix seconds, null if unknown
        source: observed           # observed | declared | inferred
        observedAt: 1757897000
    credits: { hasCredits: true, unlimited: false, balance: null }
    spendControl:                  # ← Codex SpendControlLimitSnapshot
      { limit: "50.00", used: "12.40", remainingPercent: 75, resetsAt: 1759276800 }
    ordinaryUsageAllowed: true     # null = unknown; MUST NOT be inferred from percentages
    cost: { accounting: included }  # included | metered; metered adds usdPerMTok{in,out,cacheRead,cacheWrite5m,cacheWrite1h}
    state: available               # available | degraded | cooling | exhausted | unknown
    cooldownUntil: null
    lastError: null                # { kind: rateLimitReached|spendLimit|overloaded|auth, at, detail }
```

Rationale for each non-obvious choice:

- **`windows[]` over a flat `rpm`/`tpm`.** LiteLLM's `tpm`/`rpm` assume per-minute, per-endpoint,
  per-request accounting Remuda cannot do. Codex's `{usedPercent, resetsAt, windowDurationMins}` is
  both what one harness already emits and what subscription plans actually enforce (rolling 5-hour
  + weekly). `usedPercent` also degrades gracefully: it is meaningful even when token counts are not.
- **`appliesTo`** exists because of the documented Anthropic distinction: session/weekly limits are
  "shared across all models, so the developer can't restore access by switching models", whereas an
  Opus/Sonnet family limit *can* be escaped by switching family. A scheduler that cannot express
  this will retry into a wall.
- **`source` per observation.** Declared and observed values must never be silently merged; the
  Codex notification contract ("does not clear a previously observed value") depends on it.
- **`ordinaryUsageAllowed` and `state` separate from the windows.** Copying Codex's rule: do not
  infer recovery from percentages or reset times.
- **`priority`** encodes the user's own supply ordering, which is the whole point of model profiling.
- **`cost.accounting: included`** makes explicit that for subscription supply the marginal price is
  0 until exhaustion — which is why OpenRouter-style price weighting is rejected.

### D.2 `CapabilityProfile` — known to the coordinator, verified against the gateway

```yaml
capability:
  - id: claude-opus-5
    family: opus                   # the rate-limit bucket, not the marketing name
    aliases: [opus, gw/wide]       # gateway ids that resolve here   (cf. HF base_model/new_version)
    class: frontier                # frontier | workhorse | cheap     ← the "min tier" axis
    contextWindow: 1048576         # cf. providers.md `contextWindow` + `1m` tag
    maxOutputTokens: 64000
    reasoning: adaptive            # adaptive | budget | none
    effortLevels: [low, medium, high, xhigh, max]
    supports:
      toolChoiceAny: true          # Fable 5.1 = false; getting this wrong is a 400
      structuredOutput: true
      parallelTools: true
      vision: true
      promptCaching: true
      assistantPrefill: false      # 4.6+ = false
    cacheTtl: { subscription: 1h, usageCredits: 5m, apiKey: 5m }
    price: { usdPerMTokIn, usdPerMTokOut, cacheRead, cacheWrite5m, cacheWrite1h, revision, status }
    harnesses: [claude, gateway]
    suitedFor: [planning, long-refactor, review]    # coarse, human-maintained
```

`class` is deliberately a three-value enum rather than a score. Every task spec then names a
**floor** (`minClass`), which is the only ordering the scheduler needs and the only one a user can
maintain by hand across four vendors. `family` is separate from `id` because rate limits bucket by
family (Opus 4.8/4.7/4.6/4.5 share a limit; Opus 5 does not) — a supply window must attach to
`family`, never to `id`. `price.status` mirrors the repo's existing `Published | Provisional`
discipline in `prices.rs`, so an estimate is never presented as a fact.

### D.3 `TaskSpec` — what a coordinator hands the scheduler

```yaml
task:
  id: tsk_01J…
  parent: tsk_… | goal_…
  tier: 2                          # which coordinator owns it
  project: remuda                  # Tier-2 config namespace: workspaces, hosts, policies

  # --- the brief (Anthropic's four fields) ---
  objective: "Wire UsageAggregator into the claude transcript tail."
  outputFormat: "Final line `DONE <sha>`; nothing after it."
  guidance:
    tools: [cargo, git]
    sources: [docs/design/usage-adapter.md, crates/remuda-driver/src/usage/]
  boundaries:
    owns: [crates/remuda-driver/src/usage/**, crates/remuda-driver/tests/usage.rs]
    mustNotTouch: [deploy/**, scripts/ci/**]
    forbidden: [tunnel-tools]      # D-031

  # --- routing inputs ---
  class: implement                 # research | implement | review | test | merge-gate | triage | docs
  minClass: workhorse              # capability floor
  contextNeed:
    expectedInputTokens: 250000    # estimate; reconciled after the fact
    needsLongContext: false        # true ⇒ require contextWindow ≥ 1_000_000
    repoScope: crate               # file | crate | workspace
  costSensitivity: normal          # low | normal | high  (high ⇒ prefer cheap class / included supply)
  latencySensitivity: low          # low | normal | high  (high ⇒ prefer local host, warm cache, no queue)
  effort: high                     # low|medium|high|xhigh|max — mirrors Claude Code's `effort`
  concurrencyClass: exclusive-paths  # shared | exclusive-paths  (drives the no-two-agents-one-file rule)

  # --- limits (Copilot / --max-budget-usd / --max-turns) ---
  budget: { maxUsd: 5.00, maxTurns: 40, maxWallMins: 45 }
  onBudgetExceeded: stop-and-report  # stop-and-report | ask-owner

  # --- placement constraints ---
  isolation: { worktree: "wt/usage/tail", targetDir: "/tmp/remuda-target-usage" }
  host: auto                       # auto | <hostId>          (auto respects providerBinding, D-021)
  pin: null                        # {harness, model, supplyId} — pinning DISABLES auto-selection

  # --- gates and escalation ---
  gates: [secret-scan, no-tunnel-scan, fmt, check, clippy, test, verify-tree]
  scopeCheck: { diffMustStayWithin: owns }   # the guard no existing gate provides
  escalation:
    stallThreshold: 2              # Magentic-One: stall > 2 ⇒ re-plan
    onGateFail: return-to-worker   # never fix a worker's crate inside the merge
    onRateLimit: park-until-reset  # park-until-reset | migrate | ask-owner
    onScopeDrift: ask-owner
  reportTo: coordinator:crd_project_remuda
```

And the scheduler's answer, which is also the ledger row and the bot's "started" card:

```yaml
placement:
  taskId: tsk_01J…
  supplyId: sup_codex_pro_personal
  harness: codex
  model: gpt-5.x-codex
  hostId: <remote-host>
  instanceId: ins_…
  reasons:                          # ← LiteLLM's 422 "reasons list" pattern, already in providers.md
    - "minClass=workhorse satisfied by class=workhorse"
    - "supply primary window 41% used, resetsAt in 78m > estimated 20m of work"
    - "host binding auto → host-scoped default gateway"
  rejected:
    - { supplyId: sup_claude_max_personal, reason: "weekly window exhausted, resetsAt 2026-09-16T04:00Z" }
    - { supplyId: sup_agy_free, reason: "priority 1 below threshold; costSensitivity=normal" }
  estimatedUsd: 1.80
  decidedAt: 1757897100
```

### D.4 Scheduling algorithm (admission-control-first)

1. **Filter by capability** — drop models below `minClass`, lacking a `supports.*` the task needs, or
   with `contextWindow` under `contextNeed` (LiteLLM pre-call context check).
2. **Filter by supply** — drop supplies whose `state` is `cooling`/`exhausted`, whose applicable
   window has insufficient headroom for `estimatedUsd`/`expectedInputTokens`, or whose `resetsAt`
   falls inside the expected work duration; drop host-scoped supplies for the wrong host (D-021).
3. **Filter by host** — `providerBinding` (`auto|native|profile:<id>`), worktree availability,
   per-agent `CARGO_TARGET_DIR`, and `latencySensitivity`.
4. **Rank** — `priority` first (the user's declared ordering *is* the objective function), then
   window headroom, then warm-cache affinity (a host that recently ran this workspace has a live
   prompt cache — 1h TTL on subscription, 5m otherwise), then estimated cost when
   `costSensitivity: high`.
5. **Reserve** — decrement an estimate against both the task budget and the supply window; record
   the `placement` with `reasons` and `rejected`.
6. **Spawn, then never silently re-route.** On a limit signal: park until `resetsAt` (default),
   migrate only if `escalation.onRateLimit: migrate` *and* the task is early enough that a clean
   slate is cheap. On `overloaded`/529-equivalent: retry in place with backoff and **do not cool the
   supply**.
7. **Reconcile** — at each turn boundary, fold `UsageAggregator` snapshots into the reservation and
   into the supply's observed windows; recompute the Devin-style size bucket for the next dispatch.

Hard caps that live in code, not prompts: max concurrent workers per project, max per supply
(`concurrency.max`), max fan-out per parent task, max nesting depth 3, `maxWallMins`, `maxUsd`.

### D.5 Ledger and bot contract (Tier 1 ↔ owner)

Two documents per goal, mirroring Magentic-One, plus three message types:

- **Task ledger** (rewritten on re-plan): goal, known facts, assumptions, decomposition, owner of
  each task, gates that will be run. Pinned in the chat; edited in place.
- **Progress ledger** (delta-driven): per task `state` ∈ `pending|placed|running|stalled|done|failed|
  parked`, the `placement` row, elapsed, estimated USD so far, last gate result.
- **Messages**: (1) **placement** — one card when a task is placed, carrying `reasons` so the owner
  can see *why* this harness/model; (2) **decision** — approval/question, routed through the existing
  interaction broker (D-005; the bot never bypasses, and a relayed approval from an agent is not
  consent); (3) **outcome** — gate report + diff-scope check + `DONE <sha>`, or the failure with the
  failing gate output.

Digest cadence should be **event-driven, not periodic**: prior art's explicit failure mode is agents
"distracting each other with excessive updates", and Claude Code's cost doc notes that a scheduled
task or cross-session message "fires on its interval even while the session is idle, sending your
full context each time" — a periodic digest loop is itself a cost blowup.

---

## Part E — Open questions to resolve before building

1. **Shape of `rate_limit_info`.** `crates/remuda-claude-wire/src/types.rs:371` types it as opaque
   `Value` and `claude_print.rs:764` discards it. A probe capturing one real frame would tell us
   whether Claude's supply can be read structurally (source = observed) or only textually.
2. **Does `codex app-server` expose `account/rateLimits/read` on the installed build?** The schema is
   vendored; D-003 leaves open whether Remuda spawns the user's installed `codex app-server` or
   embeds `codex-core`. If the former, the notification stream is free supply telemetry today.
3. **Grok/agy supply signals.** `usage.json`'s body was never captured (usage-adapter.md §6 records
   this as a known deviation). Until then, grok supply must be `declared` only.
4. **Gateway supply vs native supply.** A gateway profile (D-007/D-012) has no plan windows at all —
   its supply is the gateway's own quota, invisible to Remuda. Recommendation: model it as a supply
   with `windows: []` and `state: unknown`, so it is used only when explicitly prioritised or when
   every windowed supply is exhausted.
5. **Estimation error vs hard budgets.** Cost is estimated within ±10% against one anchor. A hard
   `maxUsd` must therefore carry a tolerance band, and the bot must label every figure "估算", as
   usage-adapter.md already requires.

---

## Sources

**A — orchestration**
- Anthropic, *How we built our multi-agent research system* — https://www.anthropic.com/engineering/multi-agent-research-system
- Anthropic, *Building a C compiler with a team of parallel Claudes* — https://www.anthropic.com/engineering/building-c-compiler
- Claude Code docs, *Subagents* — https://code.claude.com/docs/en/sub-agents
- Claude Code docs, *Orchestrate teams of Claude Code sessions* — https://code.claude.com/docs/en/agent-teams
- Claude Code docs, *CLI reference* — https://code.claude.com/docs/en/cli-reference
- Claude Code docs, *Error reference* — https://code.claude.com/docs/en/errors
- OpenAI Agents SDK, *Handoffs* — https://openai.github.io/openai-agents-python/handoffs/
- OpenAI Agents SDK, *Orchestrating multiple agents* — https://openai.github.io/openai-agents-python/multi_agent/
- LangGraph supervisor library — https://github.com/langchain-ai/langgraph-supervisor-py
- CrewAI, *Hierarchical process* — https://docs.crewai.com/en/learn/hierarchical-process
- Microsoft Research, *Magentic-One* — https://www.microsoft.com/en-us/research/articles/magentic-one-a-generalist-multi-agent-system-for-solving-complex-tasks/
- AutoGen, *Magentic-One* — https://microsoft.github.io/autogen/stable/user-guide/agentchat-user-guide/magentic-one.html
- GitHub Docs, *About Copilot coding agent* — https://docs.github.com/en/copilot/concepts/agents/coding-agent/about-coding-agent
- OpenHands, *CLI mode* — https://docs.openhands.dev/usage/how-to/cli-mode
- Devin Docs, *Session Insights* — https://docs.devin.ai/product-guides/session-insights ; *Billing* — https://docs.devin.ai/admin/billing

**B — supply and capability**
- LiteLLM, *Routing* — https://docs.litellm.ai/docs/routing ; *Reliability* — https://docs.litellm.ai/docs/proxy/reliability ; *Budgets & rate limits* — https://docs.litellm.ai/docs/proxy/users ; price/capability map — https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json
- OpenRouter, *Provider routing* — https://openrouter.ai/docs/features/provider-routing ; models API — https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties
- Portkey, *Configs* — https://portkey.ai/docs/product/ai-gateway/configs
- Anthropic, *Rate limits* — https://platform.claude.com/docs/en/api/rate-limits ; *Errors* — https://platform.claude.com/docs/en/api/errors
- OpenAI, *Rate limits* — https://developers.openai.com/api/docs/guides/rate-limits
- Google, *Gemini API rate limits* — https://ai.google.dev/gemini-api/docs/rate-limits
- Claude Code docs, *Manage costs effectively* — https://code.claude.com/docs/en/costs ; *Monitoring usage (OTel)* — https://code.claude.com/docs/en/monitoring-usage
- Anthropic Help Center, *Use Claude Code with your Pro or Max plan* — https://support.claude.com/en/articles/11145838 ; *Models, usage, and limits in Claude Code* — https://support.claude.com/en/articles/14552983
- Hugging Face Hub, *Model cards* — https://huggingface.co/docs/hub/model-cards

**In-repo (read-only, `origin/main`)**
- `docs/design/decisions.md` (D-005, D-007, D-012, D-019, D-021, D-028, D-031)
- `docs/design/coordinator-guide.md`, `docs/design/providers.md`, `docs/design/usage-adapter.md`
- `skills/remuda/SKILL.md`
- `docs/research/cli-help/codex-app-server-schema/v2/{GetAccountRateLimitsResponse,AccountRateLimitsUpdatedNotification,ConsumeAccountRateLimitResetCreditParams}.json`
- `crates/remuda-claude-wire/src/types.rs:371` (`RateLimitEvent`), `crates/remuda-driver/src/claude_print.rs:764`
- `crates/remuda-driver/src/usage/` (`mod.rs`, `prices.rs`, `claude.rs`, `codex.rs`, `grok.rs`)
