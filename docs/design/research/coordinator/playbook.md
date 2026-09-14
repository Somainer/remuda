# The Remuda coordination playbook

*Distilled from the live, hand-run coordinator loop of 2026-09-14/15 — the shell scripts under
`<coord-scratch>/`, the 105 worker briefs `p-*.txt`, the gate logs, the design docs and the
operator memory files. Everything below is descriptive first (what the human-driven Claude Code
coordinator actually did today) and prescriptive second (what Remuda must grow so a coordinator
**agent** can do it without shell scripts).*

Read-only pass. No repo file was modified; the checkout stayed at `origin/main`.

---

## 0. Sources

| Class | Files |
|---|---|
| Spawn | `<coord-scratch>/{spawn.sh,spawn2.sh,spawn-claude.sh,spawn-now.sh,spawn-d028-p1.sh,remote-spawn.sh,remote-respawn.sh}` |
| Supervise | `<coord-scratch>/{relay-supervisor.sh,resume-relay.sh,poll-all.sh,q.sh,gaps.sh,gateway-ok.py}` |
| Verify / land | `<coord-scratch>/{rgate.sh,rgate2.sh,rgate3.sh,remote-gate.sh,remote-gate2.sh}`, `<coord-scratch>/{rgate-uxplan.log,gate-cclaudequeue.log}` |
| Briefs | `<coord-scratch>/p-*.txt` (105 files: `p-c*` = codex/local, `p-r*` = Claude on `<remote-host>`, `p-x*` = earlier local grok/relay wave, `p-ux*` = web batches) |
| State | `<coord-scratch>/relay-workers.tsv`, `<coord-scratch>/reported/<name>`, `<coord-scratch>/nudged-<name>` |
| Design | `<coord-scratch>/realtime/design.md` §3.0 (owner-boundary map), §5 (phased plan), §6 (refusals); `.../remuda-pipeline.md` |
| Product docs | `<repo>/docs/design/coordinator-guide.md`, `.../workbench-ux-plan.md` §0, `.../skills/remuda/SKILL.md` |
| Operator memory | `~/.claude/projects/<home-slug>/memory/{hybrid-harness-project,remote-claude-fanout-sg,agent-quota-priority,subagent-model-policy}.md` |

---

## 1. The three tiers, as today's evidence shows them

Today **one** Claude Code session played tier 1 and tier 2 at once, with `<coord-scratch>/*.sh`
as its runtime and `p-*.txt` as its tier-2 → tier-3 wire format. Separating the tiers is mostly a
matter of naming what that session already did in two different registers.

| | Tier 1 — 统领 Coordinator | Tier 2 — 项目 Coordinator | Tier 3 — Worker |
|---|---|---|---|
| **Interface to the layer above** | the owner, through a chat bot (Feishu dispatcher, `crates/remuda/src/cmd/dispatcher.rs`) | a typed *task envelope* from tier 1 | a *brief* (today: one `p-*.txt`, delivered as one prompt) |
| **Owns** | the owner's intent, the global supply ledger, cross-project arbitration, escalation, the accountability trail | one project's config: workspaces, hosts, lanes, ports, gate definition, ownership map, in-flight registry, branch naming, evidence conventions | one worktree, one branch, one task |
| **Decides** | which project gets scarce quota; what is worth doing at all; what gets shown to the owner; when to interrupt the owner | local vs remote, harness + model flavor, brief content, ownership partition, sequencing, gate lane, land order, retire | nothing about placement; only how to do the task |
| **Never does** | spawn a worker, merge a branch, touch a repo | change project policy the owner set (e.g. D-031), spend supply another project was granted | push to `main`, touch files outside its worktree, kill processes it did not start |
| **Output** | one report the owner reads, plus escalations | `DONE`/`BLOCKED` roll-up, landed shas, evidence links, incident notes | exactly one final line: `DONE <sha>` or `BLOCKED <reason>` |
| **Today's embodiment** | the owner's chat messages + the coordinator's summaries | the `/tmp/remuda-coord` scripts + the coordinator's judgement | `herdr agent` panes on mac + `<remote-host>` |

### 1.1 The four typed interfaces the product needs

Today all four are prose in a chat window. Each needs a shape:

1. **Task envelope (T1 → T2).** `{intent, project, acceptance, urgency, budget_hint, owner_visible: bool}`. Deliberately *not* a file list — the ownership map is tier-2 work.
2. **Report (T2 → T1).** Per task: `{state, worker, host, harness, model, branch, landed_sha|blocked_reason, evidence[], cost, elapsed}` plus project-level `{lanes, in-flight, incidents, supply_used}`. Today this is hand-written prose; `rgate*.log` and `reported/<name>` are its raw material.
3. **Escalation (T2 → T1 → owner).** Only for things a coordinator may not decide: policy changes (D-031 was exactly this: owner said 「关停，禁止使用隧道工具」 and it became `p-cnotunnel.txt` + a gate step), credentials, OS permissions (`c-deploy` tried to grant itself Full Disk Access through System Settings and was stopped — the rule "权限问题一律 BLOCKED 交用户" is now in `_impl-rules.md`), and destructive history operations.
4. **Supply grant (T1 ↔ T2).** `{provider, model, concurrency, until}`. Today implicit: the owner said "grok 额度最多" (09-12), then "开发任务改用 claude-relay Opus(1M) + seed" (09-13), then "codex 额度已恢复" (09-14) — three re-grants in three days, each of which changed every subsequent spawn.

---

## 2. The loop as a state machine

```
                      ┌─────────────────────────── owner / bot ───────────────────────────┐
                      ▼                                                                   │
  INTAKE ──▶ PLAN/SPLIT ──▶ BRIEF ──▶ PLACE ──▶ SPAWN ──▶ RUNNING ──▶ REPORTED ──▶ VERIFY ─┼─▶ LAND ──▶ RETIRE ──▶ DELIVER
               │  ▲            │        │         │         │  │         │(BLOCKED) │(fail)│
               │  │            │        │         │         │  │         ▼          ▼      │
               │  └────────────┴────────┴─ REWORK ◀─────────┴──┴─────────┴──────────┘      │
               │                                    (re-brief, same worktree/branch)       │
               ▼                                                                           │
         DEFERRED (supply down) ─── probe ok ──▶ PLACE                                     │
                                                                                           │
  orthogonal, can fire in any state:  SUPPLY-DOWN · CARRIER-LOST · HOST-DEGRADED ·         │
                                      DISK-PRESSURE · CONFLICT-STORM · WORKER-STUCK ───────┘
```

**States.** `intake → planned → briefed → placed → spawning → running → reported(DONE|BLOCKED) →
verifying → landed | rework → retired → delivered`, plus `deferred` (waiting on supply) and the
six incident states, which are *modifiers*, not a separate lane: a worker in `running` can be in
`SUPPLY-DOWN` and stay in `running` after a model switch.

**Invariants that held all day (and are the product's real spec):**

- **I1 — A `DONE` is a claim, not a gate.** Verification runs on the exact tree about to land, never on the worker's word (`coordinator-guide.md`, and `p-rmergequeue.txt` quotes the rule back as required reading).
- **I2 — One worker = one worktree = one branch = one target dir = one port block.** Violated once in the pre-history (15 agents in one checkout broke main 5+ times) and never again.
- **I3 — Exclusive file ownership, declared before dispatch.** Every brief carries an `OWNERSHIP (exclusive)` list *and* a `Do NOT touch` list naming the other worker who owns each forbidden path.
- **I4 — Landing is serialized even when verification is parallel.** `rgate3.sh` runs two verify lanes but takes `/tmp/coord-land.lock` and re-checks `git merge-base --is-ancestor origin/main $sha` before every push.
- **I5 — Nothing a worker cannot undo.** Workers on the remote have no push credentials at all; the coordinator fetches branches over `ssh://` remotes `sg` / `sg2`.
- **I6 — Every claim about live behaviour needs a redacted evidence file** (`docs/design/evidence/*.md`), paired with a report. Fixture green ≠ live green.

---

## 3. Step by step: inputs, tool calls, scripted vs judged, failure modes

### 3.1 INTAKE

| | |
|---|---|
| **Decision inputs** | owner message (often one line of Chinese intent), current in-flight set, main's sha, open incidents, supply state |
| **Tool calls today** | none — chat + `git -C <repo> log --oneline -3 main` |
| **Scripted** | nothing |
| **Judged by the LLM** | is this a task, a policy change, or an escalation answer? does it invalidate work in flight? (D-031 invalidated an entire merged deploy path and spawned `c-notunnel` retroactively) |
| **Failure modes** | intent arrives while 8 workers are mid-flight and silently changes their constraints — today handled by broadcasting scope corrections (`p-xp1proto-note.txt`, `q.sh`) rather than by re-spawning |

### 3.2 PLAN / SPLIT — the ownership map and the contracts

This is the single most judgement-dense step and the one with the least tooling.

| | |
|---|---|
| **Decision inputs** | the code itself (read with line numbers), the in-flight branches and *their* ownership, the diff each in-flight branch will produce, which files are hot (`styles/ui.module.css` is shared by 18 files — declared a parallel-write hotspot in `workbench-ux-plan.md` §0) |
| **Tool calls today** | `git log/diff --stat main..<branch>`, `git show --stat <sha>`, grep/read of the repo, plus a hand-maintained map in the design doc |
| **Scripted** | nothing |
| **Judged** | *everything*: the partition, the freeze list, the sequencing, and the contracts that must be frozen at merge so downstream batches consume rather than reinvent |
| **Failure modes** | two workers on one file → conflicts at gate time (`c-hookgap` conflicted on 7 files after 4 branches landed under it → `p-chookgap-rebase.txt`); a semantic conflict that no per-branch gate can see (a branch green on old main failed to compile after another branch added a test — the reason the serial gate is kept) |

Two artefacts today encode this step, and both should become first-class product objects:

- **The contested map** (`design.md` §3.0): a table `in-flight → owns → therefore off-limits`, plus the inverse observation ("`crates/remuda-signal/*` was **not** touched by c-hookgap and is in no other brief — that is where this design goes"), plus an explicit **non-tasks table** (`item → owner → why not here`) so two coordinators cannot dispatch the same dependency twice.
- **The batch matrix** (`workbench-ux-plan.md` §0/§1): per batch, `owned files | new tests | evidence doc | size | when it may start`, with the start condition written as a dependency on *another branch landing* ("A 合入后放 D；`r-effort-slider` 合入后放 B、F"), and a global 禁改清单 per in-flight branch.

Two contracts were frozen *at merge* so four later batches could depend on them without touching
each other: the overlay contract (`Sheet`/`useFocusTrap` props) and the notify interface
(`notify({subject, stage, reason?, actions?, severity})`). **Contract-first sequencing is cheaper
than conflict resolution** and the product should be able to express it.

### 3.3 BRIEF

Every one of the 105 briefs has the same skeleton. This is a *format*, not prose, and it should be
generated:

```
identity        "You are r-live-screen (Claude on <remote-host>) = live structured view batch B."
worktree/branch "<remote-agents>/wt/r-live-screen on branch wt/r-live/osc-tier-and-budgets
                 (from origin/main ≥ 3f68061)"          ← base floor, not just 'main'
rules block     process safety · D-031 · build jobs · e2e ports + flock + PW env · cleanup ·
                commit style · secret-scan · push/no-push · no files outside worktree ·
                test roots · which sandbox dirs are allowed · the demo is off limits
read-first      docs + source files WITH LINE NUMBERS (signature.rs:48, map.rs:155, bus.rs:208-222)
measured problem  the evidence that justifies the task ("zero times in a 22,521-byte capture",
                  "0 events across 20.6 s while the TTY pushed 218 frames")
OWNERSHIP       exclusive file list (new files marked "(new)")
DO NOT TOUCH    file list, each annotated with the worker who owns it
TASK            numbered deliverables
ACCEPTANCE      numbered, falsifiable, incl. "the regression test fails on main and passes here"
CHECKS          the exact commands
merge-main-last "Merge origin/main as the LAST step before reporting DONE (main moves several
                 times a day)"
protocol        "last line of your final reply exactly DONE <sha> or BLOCKED <reason>"
```

| | |
|---|---|
| **Decision inputs** | the plan, the target host's constraints (ports, build jobs, missing toolchains), the harness's quirks, what the worker must *not* learn (secrets) |
| **Tool calls today** | write file to `<coord-scratch>/p-<name>.txt`; `scp` to the remote; `herdr agent prompt <name> "$(cat file)"` |
| **Scripted** | delivery only (`spawn*.sh` take a prompt-file argument; `gaps.sh` re-prompts four agents with one shared preamble) |
| **Judged** | the measured problem, the read-first list, the acceptance criteria, and how much the worker is trusted (the rules block is longer for remote Claude than for local codex) |
| **Failure modes** | backticks inside a double-quoted `herdr agent prompt` are eaten by zsh (hit twice → rule: always `"$(cat file)"` or single quotes); placeholders `<sha>`/`<reason>` in the brief match the DONE poller (every poller now greps them out); a brief that omits the ports lets a worker take the gate's e2e ports |

### 3.4 PLACE — local vs remote, harness, model flavor

| | |
|---|---|
| **Decision inputs** | task class (mechanical fix / design-heavy / evidence spike / web-e2e / rebase) · supply (see §4) · host capability (the remote has 64 cores but Ubuntu 20.04: no Chromium install, OpenSSL 1.1.1, no push creds) · current host load and `/tmp` usage · whether the task needs the owner's live demo, the real `claude` binary, or macOS |
| **Tool calls today** | `python3 gateway-ok.py <model>` (1-token `/v1/messages` probe), `ssh <remote-host> uptime/df`, `herdr agent list`, knowledge of what each host has |
| **Scripted** | the *mechanics* of each placement (a flavor arg: `es1 → <gateway-model-A>[1m]`, `seed → <gateway-model-B>[1m]`, `opus → claude-opus-5[1m]`); the deferral loop in `spawn-d028-p1.sh` (probe every 90 s for up to 3 h, then spawn or give up) |
| **Judged** | the mapping task-class → harness+model, and the concurrency ceiling ("keep ≤4 remote workers building at once" was learned, not configured) |
| **Failure modes** | load 126/64 cores with 5 workers + a gate building → e2e timeouts across the board; `/tmp` at 95 % because per-worker `target-<name>` is 5–35 GB and `target-gate` reached 71 GB; placement that ignores a host's missing toolchain (Playwright/Chromium, OpenSSL 3) fails minutes later inside the worker instead of at dispatch |

### 3.5 SPAWN

The scripted core, and the part Remuda can absorb almost entirely.

Local (`spawn-claude.sh`): `git fetch` → branch from `origin/main` → `git worktree add` →
`herdr tab create --workspace w5 --cwd <wt> --no-focus` → parse `result.root_pane.pane_id` out of
JSON → `sleep 2` → `herdr agent start --kind claude --timeout 60000 -- --settings
~/.claude/settings.relay.json --model <flavor> --dangerously-skip-permissions` → **trust-dialog
detection** (`agent read --source visible | grep 'trust this folder'` → `send-keys enter`) →
`agent wait --timeout 60000` → `agent prompt "$(cat brief)"`.

Remote (`remote-spawn.sh`): the same, plus the entire environment passed through
`tab create --env`: `LANG`, `RUSTUP_HOME`, `CARGO_HOME`, `PATH`, `CARGO_TARGET_DIR=<remote-agents>/target-<name>`,
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=16`, `OPENSSL_DIR=<remote-agents>/conda-git`,
`RUSTFLAGS=-C link-args=-Wl,-rpath,...`, and for respawns `PW_CHANNEL=chromium`
`PW_TEST_CONNECT_WS_ENDPOINT=ws://127.0.0.1:3177/`.

| | |
|---|---|
| **Decision inputs** | name, branch, base, label, brief file, flavor, port block, target dir, env |
| **Tool calls today** | `git worktree add`, `herdr tab create/agent start/agent read/send-keys/agent wait/agent prompt`, `ssh`, `scp` |
| **Scripted** | ~95 % — and the registry write: `spawn-now.sh` appends `name\tpane\tsession-id\tflavor` to `relay-workers.tsv` and refuses to double-spawn a name already there |
| **Judged** | almost nothing; this is the strongest candidate for full automation |
| **Failure modes** | `herdr agent get` returns no `agent_session` yet → the supervisor can never resume that worker (`WARN: no session id`); a long `--resume` exceeds `agent start --timeout` and herdr **loses the agent name** — from then on the worker is only reachable by pane id (which is why `relay-supervisor.sh` keys everything off `pane`, not name); `--settings` is not restored by herdr session restore, so a restored tab is not a usable worker |

### 3.6 MONITOR

`relay-supervisor.sh` is the whole of tier-2 observability today; it is 20 lines and it encodes
five distinct policies:

1. **Completion detection.** `herdr pane read --source recent-unwrapped --lines 60`, then
   `grep -E '^\s*(●|•|⏺)?\s*(DONE [0-9a-f]{7,40}|BLOCKED [^<])'` with `grep -v '<sha>\|<reason>'`
   to exclude the brief's own echo. Dedupe by writing the matched line to `reported/<name>`.
2. **Supply health.** `gateway-ok.py claude-opus-5` every cycle; its result gates every recovery
   action (never resume a worker into a dead gateway).
3. **Stall detection + nudge throttling.** If the last 8 lines contain `API Error: 5`, the gateway
   is back, and `agent_status != working`, prompt "the gateway error is over; continue" — **at most
   once per 15 min**, tracked in `nudged-<name>`.
4. **Death detection.** Pane tail containing `Resume this session` → `resume-relay.sh <name> <pane>
   <session-id> <flavor>` → `agent start … --resume <sid>` → re-brief with a *state-loss preamble*,
   falling back to `herdr pane run` when the agent name was lost.
5. **Liveness.** `poll-all.sh` reports `GONE` when `herdr agent get` fails at all.

| | |
|---|---|
| **Decision inputs** | pane text, agent status, gateway probe, elapsed since last progress, disk/load |
| **Tool calls today** | `herdr pane read`, `herdr agent get/read/prompt/send-keys`, `ssh <host> df/uptime`, `git -C <wt> status --short && git log -1` (is it actually committing?) |
| **Scripted** | detection, dedupe, throttle, resume |
| **Judged** | is "no output for 20 min" thinking, a stuck TUI, or a dead loop? is this `BLOCKED` real or a mis-scope? should this worker be replaced rather than nudged? |
| **Failure modes seen today** | `agent read --source recent-unwrapped` **errors while the agent is on the alt screen** → monitors must use `--source visible`; codex at "Queued follow-up inputs · ? 1 question" makes `agent prompt` fail with `agent_blocked` → recipe is `send-keys alt+up`, read visible, then `pane run <answer>`; grok is transiently mis-reported `done/idle` between tool calls; a `pgrep -f 'coord-queue.sh X'` waiter matches *its own* command line and never exits (6 zombie watchers); `ssh host 'pkill -f <pattern>'` kills the ssh shell itself when the pattern matches its own argv |

### 3.7 VERIFY — the gate

Two shapes were in use on 09-14, in this order of evolution:

- **Local**: `remuda merge <branch> --gate --json` (already a product command; `gate-cclaudequeue.log` is a full step trace).
- **Remote, one lane** (`rgate.sh` → `rgate2.sh`): `mkdir /tmp/coord-queue.lock` mutex → `scp remote-gate.sh` → per branch: push the branch to origin if missing → `ssh <remote-host> bash remote-gate.sh <branch> [--web]` → on `REMOTE-GATE-OK main=<sha>`, fetch that sha from the `sg` remote and push it to `origin/main`, with 3 retries → delete the branch locally and on origin.
- **Remote, two lanes** (`rgate3.sh` + `remote-gate2.sh`): lane 1 = `repo`/`target-gate`/ports 58980·58989/remote `sg`; lane 2 = `repo2`/`target-gate2`/ports 58970·58979/remote `sg2`. Verification is parallel; **landing takes `/tmp/coord-land.lock`**, re-fetches, asserts `git merge-base --is-ancestor origin/main <sha>`, and on failure re-verifies the branch on the new main (one retry). A `step preflight failed` on the first attempt is interpreted as "raced a landing on the other lane" and retried.

`remote-gate.sh` itself is a hygiene program wrapped around `remuda merge`:
`git merge --abort; git rebase --abort; git reset --hard; git clean -fdq` → kill stale listeners on
its own e2e ports via `ss -ltnp` → `git checkout -B main origin/main` → print a conflict preflight
from `git merge-tree --write-tree` → **`flock <remote-agents>/e2e.lock`** around
`remuda merge <branch> --gate --json --no-push --repo … --target-dir … [--web-e2e]` → per-branch
stderr log to `gate-logs/<slug>.err` → parse the `steps[]` array → emit `REMOTE-GATE-OK main=<sha>`
or `REMOTE-GATE-FAIL rc=<n>` with the last 8 stderr lines.

| | |
|---|---|
| **Decision inputs** | the branch diff vs the files the brief named; conflict preflight; which gate steps apply (web / e2e); which lane is free; whether main moved |
| **Tool calls today** | `git log --oneline main..<br>`, `git diff --stat main...<br>`, `ssh … remote-gate.sh`, `remuda merge --gate --json`, `git fetch sg <sha>` |
| **Scripted** | the whole mechanical path; conflict preflight; port cleanup; log capture; the e2e lock |
| **Judged** | **the diff review** — "a diff that has grown beyond the files the task named is the most common reason to reject, and no gate catches it" (`coordinator-guide.md`); and the routing of a failure: back to the worker (`p-rp3stream-fix.txt` hands back a *deterministic* e2e failure with the exact repro command, the port block to use, and three ranked root-cause hypotheses), vs. a coordinator-side fix, vs. a rebase round (`p-chookgap-rebase.txt` names every branch that landed under the worker and orders "resolve so BOTH sides survive") |
| **Failure modes** | stale generated client (`web/src/lib/api.generated.ts`) turned CI red twice → fixed *in the product* by adding the `gen-api-current` gate step (`p-cgategen.txt`); `verify-tree` fails when e2e rewrites committed screenshots; a previous aborted e2e leaves a Hub on the port (`already used`, fails in ~3 s); worker e2e runs collide with the gate on the default `58880/58889`; orphaned `fake-herdr server` processes spin at 93 % CPU and make everything time out; gate throughput was **~1 branch per 8–12 min** because verification and landing were the same serialized act (the motivation for `p-rmergequeue.txt`) |

### 3.8 LAND

| | |
|---|---|
| **Decision inputs** | gate result, current `origin/main`, the other lane's state, branch ordering (land the smaller diff first when two agents touched one file) |
| **Tool calls today** | `git fetch sg <sha>`, `git merge-base --is-ancestor`, `git push origin <sha>:refs/heads/main`, `git push origin --delete <br>`, `git branch -D` |
| **Scripted** | CAS, ancestor check, retries, branch cleanup |
| **Judged** | order under contention; whether a `BaseMoved` deserves a re-verify or a hand-back |
| **Failure modes** | the classic: a coordinator building a detached worktree from main while the gate advanced main, then `update-ref main <new> <expected>` **erased the gate's merge from local main** — origin still had it and only the rejected push revealed it. Rule since: `git merge-base --is-ancestor "$exp" "$new" || abort`, or just use `remuda merge --gate` |

### 3.9 RETIRE

| | |
|---|---|
| **Decision inputs** | branch landed or abandoned; worker idle; disk pressure |
| **Tool calls today** | stop the agent (for grok: `ctrl+c` ×2 / `ctrl+d`, `esc` is not enough), `herdr tab close`, `git worktree remove`, **`rm -rf <remote-agents>/target-<name>`**, free the port block, drop the row from `relay-workers.tsv` |
| **Scripted** | partially — this is the least scripted step and the one that caused the most damage |
| **Judged** | when a stuck worker must be *replaced* rather than nudged (rule from 09-13: "same mechanical fix, grok two rounds without landing → switch to codex, and explicitly STOP the original worker so it cannot overwrite the new worker's branch") |
| **Failure modes** | worktree removed but `target-<name>` left behind (5–35 GB each) → `/tmp` at 95 %; a stopped-but-not-killed worker force-pushing over its replacement's branch; the Node-side equivalent inside the product (18 panes / 15 workspaces accumulated in the `remuda-node` herdr session, and `remuda dev` shutdown erroring with "driver shutdown did not settle") — which is exactly the `c-cleanup` task |

### 3.10 DELIVER

| | |
|---|---|
| **Decision inputs** | what landed, what is still blocked, what the owner asked for, whether the live demo binary must be refreshed |
| **Tool calls today** | prose summary in chat; `docs/design/evidence/*.md` links; demo refresh via an atomic binary swap |
| **Scripted** | nothing |
| **Judged** | all of it |
| **Failure modes** | on macOS, `cp` over a *running* binary makes the next start `SIGKILL` (rc=137) — releases must be `cp -p x x.new && codesign -s - --force x.new && mv x.new x`; running a demo straight out of a shared `target/debug` serves stale embedded assets because build-script `OUT_DIR`s collide across worktrees → run from a snapshot (`/tmp/remuda-live/bin`, `--web-root /tmp/remuda-live/web`) |

### 3.11 INCIDENTS — the six that actually fired on 2026-09-14

| Incident | Detection | Response that worked | What the product should own |
|---|---|---|---|
| **Supply 429** — `<gateway-model-A>` returned HTTP 429 `{"error_code":-2001}` to every request for a while; `<gateway-model-B>` kept working | probe + pane text | **live model switch on a running worker**: `send-keys esc` ×2 (break the retry loop) → `agent prompt "/model <gateway-model-B>[1m]"` → `send-keys enter` (accept the "conversation is cached for the current model" confirm) → "continue where you left off" | `remuda profile probe` + `worker switch-model` as a first-class op, with the confirm dance encoded per harness |
| **Supply 503** — the relay returned "no eligible upstream account"; **five Claude sessions exited in the same minute**, each printing `Resume this session with: claude --resume <id>` | pane tail `Resume this session` | `relay-supervisor.sh` + `resume-relay.sh --resume <sid>` + a preamble telling the worker why it died; briefs were changed to demand frequent local commits | supervised sessions with durable session ids and automatic resume; "commit often" as a launch-time policy, not brief prose |
| **Carrier death** — the headless `herdr --session remuda-sg server` **died at ~20:50 with 8 workers attached**; no OOM, no shutdown line in the log; most likely a worker's `pkill herdr`-style cleanup matching the real server | workers all `GONE` at once | `setsid nohup herdr --session remuda-sg server &` (session.json restores workspace + stale tabs → `tab close` each) → `remote-respawn.sh <name> <label> <flavor>` which reopens a tab **in the existing worktree** and runs `claude --continue`, then a message: *"the herdr server hosting your pane died at about 20:50 and your process was lost; uncommitted edits on disk survived, any process you had running did not — rebuild/re-run as needed"* | process-safety as an enforced sandbox rule, not a sentence in 105 briefs; carrier supervision + `--continue` respawn as a product verb |
| **Host degradation** — load 126 on 64 cores; root cause: **dozens of orphaned `fake-herdr server` processes at ~93 % CPU for 12 h**, leaked by tests killed mid-run | `uptime`, e2e timeouts | `pkill -f 'fake-herdr server'`; then a *task* was dispatched to fix it at the root (`p-rfakeherdrleak.txt`: parent-death detection, no idle spin, `kill_on_drop`) | concurrency caps per host; orphan reaping; the "incident becomes a task" pattern is itself the product loop working |
| **Disk pressure** — `/tmp` at 95 %; `target-gate` 71 GB; a 62 GB copy of `target-gate` was started to seed lane 2 and had to be aborted | `df` | lane 2 got its own cold-built `target-gate2`; retirement must `rm -rf target-<name>` | disk budget per host, enforced at placement and reclaimed at retire |
| **Conflict storm** — `c-hookgap` conflicted on 7 files after four branches landed beneath it | gate `CONFLICT` preflight | a rebase brief naming every branch that landed, the intent of each, and "resolve so BOTH sides survive" | branch-age alarms; dependency-aware land ordering; the merge queue of `p-rmergequeue.txt` |

---

## 4. Model profiling: capability the coordinator knows, supply the user must declare

The owner's framing is exactly what the evidence supports: **capability is general knowledge,
supply is local truth and must be declared.** Today supply lives in three places — the owner's chat
messages, `agent-quota-priority.md`, and a `flavor` argument hard-coded in four shell scripts.

### 4.1 The supply declaration (user-provided, per project or global)

```
provider: relay-anthropic          # or: openai/codex, xai/grok, google/gemini, ark/seed, …
  endpoint_probe: <cmd|http>       # today: gateway-ok.py — a 1-token /v1/messages call
  models:
    - id: claude-opus-5[1m]
      alias: opus
      class: [design-heavy, rebase, evidence]
      quota: {unit: tokens|requests|usd, budget: …, window: 5h|day, resets_at: …}
      concurrency_max: N           # today learned the hard way: "≤4 remote workers building"
      rate_limit: …
      cost_per_unit: …
      rank: 2                      # the owner's preference order
      fallback: [<gateway-model-B>[1m]]
```

Facts this schema must be able to express, all of which bit today:
- **Per-model, not per-provider, availability**: `es1_orange_o50` was 429 while `<gateway-model-B>`
  on the same gateway was fine; conversely `seed` probes sometimes 503 while opus is up.
- **Preference order changes by owner decree**, mid-project: `grok > codex > claude-relay > agy`
  (09-12) → "开发任务改用 claude-relay Opus(1M) + seed" (09-13) → "codex 额度已恢复，也可以用"
  (09-14). The scheduler must re-plan without re-briefing.
- **Reserved models**: Fable is coordinator-only — "Subagent 不要用 Fable". A supply entry needs a
  `role: coordinator-only` flag that the dispatcher refuses to spend on workers.
- **Host-bound supply**: the remote's Claude is gateway-configured through its own
  `~/.claude/settings.json`; the mac's uses `--settings ~/.claude/settings.relay.json`. Supply is a
  (host × provider) pair, not a global.

### 4.2 Capability → task class (the coordinator's own knowledge, refutable by measurement)

| Task class | What today's evidence says |
|---|---|
| mechanical, one-line, mechanical-refactor | codex landed in one round what grok failed six times; rule: **two failed rounds on a mechanical fix → swap harness** |
| design-heavy implementation with a 60-line acceptance list | Claude Opus 1M / seed on the remote; these are the `p-r*` briefs |
| evidence spike (drive a real CLI, read its rollout/transcript, write fixtures) | codex locally, because the binary being probed *is* codex/grok/claude and must run on the host that has it |
| rebase / conflict resolution across four landed branches | the strongest available model; the brief must carry the intent of every branch it is merging against |
| read-only research fan-out | Workflow/Opus subagents, never the coordinator model |
| web + Playwright e2e | wherever a browser endpoint exists — on the remote that meant a Docker `playwright run-server` on `:3177` because Playwright ≥1.6x refuses to install Chromium on Ubuntu 20.04 |

### 4.3 The scheduling rule the scripts actually implement

`effective throughput = min(supply, host capacity, lane capacity)` — and today all three bound at
different moments: supply (429/503 twice), host capacity (load 126, `/tmp` 95 %), lane capacity
(1 branch / 8–12 min). A scheduler that only models quota will happily spawn eight workers into a
host that then times out every e2e. **Placement must be a joint decision over (supply, host, lane,
disk), and it must be re-evaluated on incident, not only at dispatch.**

---

## 5. The command surface a coordinator agent needs

Design constraint: a coordinator agent should never write a shell script. Every verb below exists
because a script under `<coord-scratch>/` does it today; the "today" column is the provenance.
Several already exist in `remuda` (`worktree`, `instance`, `fleet`, `merge`, `doctor`, `agents`,
`ssh`) — the gap is the *coordination* layer above them.

### 5.1 Verbs

| Verb | Shape (sketch) | Replaces today | Notes |
|---|---|---|---|
| `remuda project` | `project init/show/set <key> <value>` — workspaces, hosts, lane count, port pools, gate definition, branch naming, policy set | hard-coded paths in 12 scripts | the tier-2 configuration object the product currently has nowhere to put |
| `remuda profile` | `profile show`, `profile probe [model…]`, `profile pick --class design-heavy [--host h]`, `profile spend <task>` | `gateway-ok.py`, the `flavor` case statements, three memory files | returns `{harness, model, host, why}`; `probe` is the 1-token call; keeps a rolling ledger |
| `remuda task` | `task add --intent … --acceptance …`, `task split`, `task show`, `task list --state`, `task depends <a> --on <b>` | prose + the batch matrix in a design doc | the unit tier 1 hands down and tier 2 reports on; carries the ownership map |
| `remuda own` | `own claim <task> <path…>`, `own check <task>`, `own map` | `design.md` §3.0 table, 禁改清单 in `workbench-ux-plan.md` | **the single highest-value new primitive**: a live registry of path → owning task, queryable before dispatch and enforceable at gate time (`own check` = "did this diff touch paths owned by another in-flight task?") |
| `remuda brief` | `brief render <task> --host h --harness k` → text; `brief lint <file>`; `brief send <worker> [--file]` | `p-*.txt` written by hand, `scp`, `agent prompt "$(cat …)"` | render composes project policy + ownership + port block + placement into the fixed skeleton; **lint** catches secrets, placeholder `<sha>` echoes, missing ports, missing DONE protocol |
| `remuda dispatch` | `dispatch <task> [--defer-until supply-ok --max-wait 3h]` → worktree + env + instance + brief + registry row, idempotent by name | `spawn*.sh`, `remote-spawn.sh`, `relay-workers.tsv` | must record the durable session id; must refuse to double-dispatch a name |
| `remuda watch` | `watch [--project p] [--json]` → event stream: `reported{done|blocked}`, `stalled`, `exited`, `gone`, `supply-down`, `disk`, `load` | `relay-supervisor.sh`, `poll-all.sh`, `reported/`, `nudged-` | encodes the DONE regex *with placeholder exclusion*, per-source fallback (`visible` vs `recent-unwrapped`), dedupe and nudge throttling as policy knobs |
| `remuda worker` | `worker nudge/answer/keys/switch-model/resume/replace/stop <name>` | `resume-relay.sh`, `remote-respawn.sh`, the codex `alt+up` recipe, the `/model` switch dance | `resume` carries the state-loss preamble; `replace` stops the old worker *properly* per harness before starting the new one |
| `remuda gate` | `gate <branch…> --onto <ref> --lanes N --host h --json` | `rgate*.sh`, `remote-gate*.sh` | verification only; per-lane target dir + port block + shared browser endpoint + e2e advisory lock, all allocated by the product (this is `p-rmergequeue.txt`'s `--onto`/`--lanes`) |
| `remuda land` | `land <branch> --verified-on <base>` → CAS with ancestor check, delete branch, or exit `BaseMoved` | the lock + ancestor dance in `rgate3.sh` | landing stays globally serialized even when gating is parallel |
| `remuda retire` | `retire <worker> [--keep-branch]` → stop, close pane/tab, remove worktree, **remove target dir**, free ports, unregister | a checklist the coordinator sometimes forgot | must report reclaimed bytes; should be automatic on `land` |
| `remuda report` | `report --since … [--for-owner]` → the T2→T1 roll-up; `report escalate <task> --reason` | prose summaries | the bot renders this; escalation is the only path that interrupts the owner |
| `remuda hostcap` | `hostcap show <host>` → cores, load, `/tmp` free, port pool state, browser endpoint, toolchain inventory, concurrency cap | `ssh host uptime/df`, tribal knowledge | placement input; also the pre-flight that would have caught "Ubuntu 20.04 has no Chromium" at dispatch instead of 40 min later |

### 5.2 Two shapes worth spelling out

**Port and resource blocks.** Every brief today hand-assigns `HUB_E2E_LISTEN`/`HUB_E2E_WEB_PORT`
(58480, 58580, 58780, 56980, 57080, 57780, 58980/58989 for gate lane 1, 58970/58979 for lane 2 …)
and every collision was a coordinator slip. The product should hand out a **resource block**
`{ports: {...}, target_dir, scratch_root, e2e_lock, browser_endpoint}` at dispatch, inject it as
env, render it into the brief, and reclaim it at retire. Reserved blocks (the owner's live demo
`:18080`/`:18787`, `/tmp/remuda-live`) are project policy.

**The event, not the poll.** `watch` should emit events, not require a loop. Today's supervisor
runs every 120 s and the operator was explicitly told off once for a 9.5-minute blocking poll
("检查 agent 状态用短命令…不要长 sleep 循环占着主回合"). A coordinator agent must be able to
subscribe and keep working — the existing `remuda instance wait --until` (hard max 5 min) is not
enough; the loop belongs in the product.

### 5.3 What must stay LLM judgment

Everything in this list was attempted-as-policy at some point today and had to be decided case by
case:

1. **The ownership partition and the freeze list.** Requires reading the code and the in-flight
   diffs. `own claim` can *record* and *enforce* it; only a model can *derive* it.
2. **Brief content**: the measured problem, the read-first list with line numbers, and the
   acceptance criteria. The skeleton is generated; the substance is not.
3. **Sequencing vs parallelizing.** "A, C1, G1 now; D after A; B/F after r-effort-slider; C2 after
   c-hookgap; E after r-p3-stream" is a judgement about contracts, not a dependency graph anyone
   can compute from the repo.
4. **Diff review at the gate** — the one rejection reason no gate catches.
5. **Failure routing**: worker's fault / coordinator's fault / flake / environment. Today's
   discriminator was "the same spec passed in six other gates today and the load is now ~10" — that
   is reasoning over history, not a threshold.
6. **Swap-or-nudge** on a stuck worker, and **is this BLOCKED real**.
7. **Supply judgement under partial information**: which model to fall back to when one is 429 and
   the probe for the other is intermittently 503.
8. **When to interrupt the owner.**

---

## 6. Policy vs preference

**Policy** = the product must enforce it; a coordinator agent must not be able to talk itself out
of it, and a worker must not be able to violate it even by accident.

| Policy | Where it came from | Enforcement point |
|---|---|---|
| **No tunnel tooling (D-031)**; no execution of anything under `deploy/` | owner decree after a host security monitor flagged the `cloudflared` probe | `scripts/ci/no-tunnel-scan.sh` in the gate **and** a dispatch-time command denylist in the worker sandbox; the rule is repeated in every brief today because there is no other enforcement |
| **Never a bare `pkill herdr` / never kill a pid you did not start**; the only allowed pattern is `pkill -f 'fake-herdr serve[r]'` | a worker most likely killed the carrier of eight other workers | sandbox denylist; better, make it unnecessary by owning process lifecycle (the `r-fakeherdrleak` fix: parent-death exit + `kill_on_drop`) |
| **Secrets never in a brief, a prompt, or a commit** | `settings.relay.json` contents, Hub bootstrap/device tokens | `brief lint` + `secret-scan` gate step + runtime resolution (`remuda mcp` resolves Hub URL and token at runtime — `docs/design/remuda-mcp.json` is committed *precisely because* it contains neither) |
| **Workers never push to `main`, never touch files outside their worktree** | 15-agents-in-one-checkout, main red 5+ times | remote workers have no credentials at all; locally, branch protection + gate |
| **`DONE` is a claim; land only through a gate run on the tree being landed** | repeated | `remuda merge --gate` / `remuda land`; no manual `update-ref` path |
| **CAS + ancestor check before advancing main** | a coordinator erased a gate's merge from local main | inside `land` |
| **One worktree / one branch / one `CARGO_TARGET_DIR` / `CARGO_INCREMENTAL=0` per worker** | shared-target artifact collisions, 40+ load, stale embedded assets | `dispatch` allocates it |
| **Port blocks are allocated, never literal**; reserved blocks (owner demo `:18080`/`:18787`) are inviolate; nothing below `:50000` on a shared host | three separate collisions | resource block at dispatch |
| **Disk reclaimed at retire** (`target-<name>` removal is part of retirement, not an afterthought) | `/tmp` at 95 %, 71 GB `target-gate` | `retire`, plus a per-host disk budget checked at placement |
| **Never touch OS settings / drive the system GUI; permission problems are `BLOCKED` to the owner** | `c-deploy` opened System Settings to grant itself Full Disk Access | sandbox + `_impl-rules.md`; escalation path |
| **Evidence is redacted before commit; runtime state stays out of the repo; paid runs record cost and model** | `coordinator-guide.md` | secret-scan + review |
| **Shared-resource serialization**: one advisory lock per shared e2e/browser resource | gate and workers colliding on ports and on one Playwright server | `gate` allocates the lock; briefs stop having to say `flock` |

**Preference** = project or owner configuration; a coordinator may change it without asking.

- Model/harness ranking and the task-class → model mapping (§4) — it changed three times in three days.
- Local vs remote default placement; number of parallel workers; `CARGO_BUILD_JOBS`; lane count.
- Gate composition *beyond* the mandatory scans (which crates, `--affected` vs `--full`, whether web/e2e steps run).
- Naming: `wt/<agent>/<topic>`, `p-<name>.txt`, worker names, labels, workspace ids.
- The completion protocol wording (`DONE <sha>` / `BLOCKED <reason>`) — must be *a* fixed literal, needn't be *this* literal.
- Polling cadence, nudge throttle (15 min today), stall threshold, resume policy.
- Brief style: read-first lists, "merge origin/main last", evidence-doc requirement, per-brief size.
- Whether a demo refresh or a screenshot accompanies delivery.

---

## 7. Gaps — what today's loop does that the product has nowhere to put

1. **The ownership registry.** `design.md` §3.0 and `workbench-ux-plan.md` §0 are hand-maintained
   prose tables that go stale the moment a branch lands. This is the single most valuable thing to
   make executable (`remuda own`).
2. **Dependency-gated dispatch.** `gaps.sh` expresses "start only after main contains the commit
   'merge: wt/x-proto/dogfood-1 into main' — poll every 2 minutes, up to 40 minutes" as *prose
   inside the worker's brief*, which means the worker burns its own context polling git.
   `task depends --on` + deferred dispatch belongs in the coordinator.
3. **Supply-gated dispatch.** `spawn-d028-p1.sh` is a 3-hour probe-then-spawn loop. This is
   `dispatch --defer-until supply-ok` and should not be a shell script holding a terminal.
4. **A durable worker registry.** `relay-workers.tsv` (name, pane, session-id, flavor) is what makes
   resume possible; when herdr loses the agent name, the pane id is the only handle left. The
   product needs instance identity that survives carrier death by construction.
5. **Incident → task.** The best pattern of the day: every incident became a *dispatched fix*
   (`r-fakeherdrleak` from the load-126 incident, `c-gategen` from the CI-red incident, `c-cleanup`
   from the pane-leak incident, `c-notunnel` from the security-monitor incident, `r-mergequeue` from
   the 8–12-min landing latency, `c-resumefix` from a live resume failure the owner hit in the UI).
   A coordinator agent should be able to file one with the incident's evidence attached.
6. **Cross-tier reporting.** There is no report schema; there is a person writing paragraphs.
7. **Cost/ledger.** `coordinator-guide.md` requires recording cost and model for paid runs; nothing
   aggregates it, so §4's supply model has no feedback loop.

---

## 8. One-paragraph summary of the whole loop

A tier-2 coordinator takes an intent, reads the repo and the in-flight branches to produce an
**exclusive ownership partition** and a **sequencing plan** whose edges are "branch X must land
first"; it renders one brief per partition from a fixed skeleton (identity, worktree, rules,
read-first, measured problem, ownership, do-not-touch, task, acceptance, checks, `DONE <sha>`); it
picks a host and a model from declared supply and measured host capacity; it dispatches into an
isolated worktree with an allocated resource block; it watches panes for a `DONE` line while
distinguishing stalls, gateway outages, carrier deaths and stuck TUIs, and it nudges, resumes,
switches models or replaces the worker accordingly; it treats every `DONE` as a claim, re-verifies
on the exact tree it is about to land, reviews the diff for scope creep that no gate can see, lands
through a compare-and-swap with an ancestor check while verification runs in parallel lanes;
it retires the worker *including its build directory*; it turns every incident into a dispatched
fix; and it reports upward in a form a bot can show the owner. Roughly 70 % of that is mechanical
and already half-written in `<coord-scratch>/*.sh`; the other 30 % — the partition, the brief's
substance, the diff review, the failure routing and the decision to interrupt the owner — is the
judgement the product should protect, not replace.
