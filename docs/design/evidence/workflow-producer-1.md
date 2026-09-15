# Workflow run producer 1 — real run drives the timeline card

- 日期：2026-09-15
- harness：本机 `claude` **2.1.221**（`claude -p --dangerously-skip-permissions`，模型 `model_hub/es1_orange_o48[1m]`）
- 范围：`crates/remuda-journal/src/workflow/`（脚本解析 + 运行折叠器）、`crates/remuda-node/src/workflow_producer.rs`（Node 活线接入）、`crates/remuda-signal/src/map.rs`（已存在的字段精选）+ `SubagentStart` 注册、`crates/remuda-driver/src/launch/overlay.rs`
- 前序证据：[workflow-progress-signals-1](workflow-progress-signals-1.md)（信号实测）、[workbench-w-workflow-card-1](workbench-w-workflow-card-1.md)（卡片）

## 1. 要回答的问题

batch W 落地卡片后，真实会话里没有生产者：`WorkflowJournalTailer` 只存在于
`remuda-journal`，没有任何活线驱动，卡片在真实运行中退化成普通行 + note。
本批要证明：**Node 活线收到 PostToolUse(Workflow) / SubagentStart 后，从会话目录
的 journal + agent jsonl + 脚本副本折叠出 `workflow.run/phase/member`，真实数据端到端
把卡片填满**；2.1.221 不产出 label/phase/model 时也能靠脚本调用点与 agent jsonl 还原，
动态/读不到的部分按 decision 6 降级，且绝不伪造。

## 2. 实机方法

两次真实运行，都在 devbox-sg 本机，文件未改动：

1. **2026-09-15 新捕获**（本证据的主运行）：脚本
   `/tmp/wfprod-evidence/proj/wf-2phase.js`，`Plan`/`Check` 两阶段、3 个 agent
   （2 并行 + 1 串行），run id `wf_fcfd9e24-f8f`，task `whk9dywf6`，
   session `1e67087d-…`，结果：
   `PLAN-A ready…` / `PLAN-B: standing by…` / `CHECK: both plan notes … present`。
2. **r-ux-w spike 复核**（2026-09-14 捕获、信号文档同源）：`spike-wf-1`
   两阶段 4 agent，run `wf_c3422384-cb1`，task `wn1eumvco`。

生产代码路径不经任何 mock：用 `remuda-journal` 的证据辅助 example
`workflow_live_dump`（`cargo run -p remuda-journal --example workflow_live_dump …
--scan-transcript`）直接跑真实的 `WorkflowJournalTailer`，把每次折叠产出的完整
payload 落成 JSONL，再由 web 证据页原样喂给**生产组件** `WorkflowTimelineCard`。
原始观测转储：

- `web/test-results/evidence/workflow-live-capture-2026-09-15.jsonl`（新捕获，14 条）
- `web/test-results/evidence/workflow-recorded-221.jsonl`（spike，24 条）

截图（未经 REMUDA_EVIDENCE 标记，按约定放在 test-results 下；Playwright 连本机共享
Chromium server，深色主题）：

- `web/test-results/evidence/workflow-card-real-1440.png`（900px 宽，运行中 + 已完成）
- `web/test-results/evidence/workflow-card-real-390.png`（390px 窄屏，model/tool 列按
  卡片既有规则隐藏）

## 3. 端到端对账

### 3.1 新捕获运行 `wf_fcfd9e24-f8f`

折叠出 8 个 run 快照，终态 3/3 完成。每个成员都从真实文件还原：

| 成员 | phase | 状态迁移 | model | tokens | duration | 最近工具 |
| --- | --- | --- | --- | --- | --- | --- |
| plan:a | Plan | running→completed | claude-opus-4-8 | 22 729 | 4 601 ms | — |
| plan:b | Plan | running→completed | claude-opus-4-8 | 22 729 | 80 296 ms | — |
| check:summary | Check | running→completed | claude-opus-4-8 | 23 442 | 168 144 ms | Read |

- label/phase：本机为 2.1.221，`journal.jsonl` 的 `started` 只有
  `{key, agentId}`，`agent-*.meta.json` 退化成 `{agentType, spawnDepth}`（已在
  信号文档 §1.3 实测）。label/phase 全部由**脚本首条 user 记录精确匹配
  `agent('<prompt>',{label,phase})` 调用点**还原（plan:a / plan:b / check:summary）。
- 脚本来源：本机这次没有写 `workflows/scripts/<name>-<runId>.js`，但 2.1.221 会把完整
  脚本持久化进 `<session>/workflows/<runId>.json` 的 `script` 字段（与 spike 的副本
  内容逐字一致）。生产者按 `hook scriptPath → scripts/ 副本 → <runId>.json#script`
  顺序取脚本，只读脚本字符串、不读该文件的终态 result/progress（终态仍以
  task-notification/journal 为准）。
- token：成员取该 agent **最后一条 usage**（input+output+cache_creation+cache_read；
  末条 usage 已含整段上下文，逐轮替换不累加）。三成员合计 68 897，与
  `<runId>.json` 的 `totalTokens` 68 827、任务通知口径一致（差 70，0.1% 内）。
- 调用数：check:summary `toolCalls=2`（Read），其余 0；run 合计 `2 次调用`，与
  `<runId>.json` 的 `totalToolCalls=2` 完全一致。
- 终态：主 transcript 的 `<task-notification>`（task `whk9dywf6`，
  `<status>completed</status>`，summary
  `Dynamic workflow "…" completed`）把 run 收口为 completed，并带活线 summary。

### 3.2 spike `wf_c3422384-cb1`（2026-09-14）

24 条观测：launch 1 + 10 个 run 快照（running×9 → completed×1）、4 个 phase 各
queued→running→completed、8 条 member（4 成员 running+completed 各一）。

- 4 个 agent 全部匹配到 alpha:one/two、beta:one/two，phase 分别 Alpha/Beta；
- 成员 token 合计 **83 365**，任务通知的权威 `subagent_tokens` 为 **83 319**
  （差 46，0.06%）；
- 4/4 done、0 running、`3 次调用`（3 个 agent 各 1 次工具，PONG 代理 0 次）；
- 活线行 `当前 Alpha: alpha:one` 来自最近启动的运行中成员；终态行 summary 来自
  task-notification；**全流没有任何 note**（真实数据存在就永不降级）。

## 4. 预算与「无轮询噪音」

- 首个 `workflow.run`：在 PostToolUse(Workflow) 的 hook 折叠里**同步**产出（launch
  快照与脚本解析都在事件处理栈上完成，不等待 250 ms tick）；
  `crates/remuda-node/tests/workflow_producer.rs::launch_member_terminal_sequence_and_budgets`
  实测 ≤ 250 ms。
- 首个 member 观测：SubagentStart（2.1.221 不带 transcript 路径，生产者按
  `agent-<id>.jsonl` 是否已落盘归属 run）同步折叠，首条 user 记录在 spawn 时已在盘上，
  所以开门观测就带 label/phase/model；同测试实测 ≤ 250 ms。
- 文件增长按 250 ms tick（`WORKFLOW_POLL`）drain：journal 每行是一次原生状态迁移，
  逐行折叠并各自出快照；agent jsonl 的 token/calls 增长与 elapsed 移动**不单独触发**
  观测（run 签名只含状态/计数/live agent），它们搭下一次真实迁移的快照；
  `idle_polls_emit_nothing` / 集成测试空闲 poll 断言均为空。
- journal 一次性追上多行时，phase 的 running 修订也不丢：每行后重算 phase
  （queued→running→completed 三个修订都在转储中可见），这是真实迁移而非轮询噪音。

## 5. 降级（decision 6）与终态

- 动态 prompt（模板插值、字符串拼接、`for/while/map` 包 agent 调用）：脚本解析器不把它
  当字面量，成员 label 退化为 agent id 前 8 位短码、不分配 phase、`totalKnown=false`，
  单测 `dynamic_prompts_degrade_without_a_phase` / 集成夹具
  `dynamic_prompt_agents_degrade_to_short_code_with_no_note` 锁定；
- 2.1.270 形状优先：`started` 的原生 label/phase 与 meta 的
  `description/workflowPhase/model` 直接采用（`a_270_shaped_run_…` 测试，用真实 agent
  jsonl + 合成 270 journal/meta）；
- run 目录连续 8 个 tick（约 2 s）不可读才挂 note；一旦出现 journal/member/脚本立即清除；
- cancelled：TaskStop（hook 或主 transcript 的 TaskStop tool_result）把 run 置
  cancelled，仍在 running/queued 的成员置 cancelled（即「已终止」），
  `cancelled_run_marks_live_members_killed` 锁定；run 级 `failed` 同理收口。

## 6. 测试

- `cargo test -p remuda-journal`：脚本解析 11、既有 journal 7、新 tailer 6（真实 run 目
  录 + 270 合成 + 动态降级 + task-notification + 空闲无噪音）全过；
- `cargo test -p remuda-node`：库 173、`tests/workflow_producer.rs` 2
  （真实 SignalBus hook + 真实 store append + 合成 run 目录，断言序列与 250 ms 预算）；
- `cargo test -p remuda-signal`、`-p remuda-driver --lib` 全过
  （`tests/adapters_parity` 在改动前的干净树上同样失败，是本机环境问题）；
- web：`tsc -b` 干净，workflow 相关 vitest 53 个全过，
  `ux-workflow-card.hub.spec.ts` 6/6（共享 Playwright server，flock + 非默认端口）；
- `cargo fmt --all`、`cargo clippy -p remuda-journal -p remuda-node -p remuda-signal
  -p remuda-driver --all-targets` 无告警。

## 7. 实现要点

- 脚本解析（`workflow/script.rs`）是一个引号/注释/模板敏感的小扫描器，不引 JS 引擎；
  只认静态形状，`agent('<literal>', { label, phase })` 的 prompt 必须是「整参数字面量」，
  拼接/插值返回 `None`；重复字面量按源序各占一个调用点。
- agent jsonl 读取有界（>8 MiB 跳过，成员退化为 journal-only），FileTail 增量游标，
  主 transcript 活线从**当前 EOF** 起读（新增 `FileTail::at_end`），证据重放才从 0 起。
- 生产者在 Node observation pump 内：hook 观测提交后同步
  `producer.on_observation(..)`（launch/member 预算），250 ms `tokio::interval` 上
  `producer.poll()`；不改 `shell_pty.rs` / `Transcript.tsx` / `assemble.ts`，协议零改动
  （全部是 additive）。
- `SubagentStart` 加入驱动 overlay 的 HOOK_EVENTS 与 signal REGISTERED_EVENTS（2.1.221
  实测该事件已触发，只是此前未注册）。
