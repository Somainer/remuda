# Task track：subagent 行随完成而关闭（c-tasktrack）

- 日期：2026-09-15（复核 2026-09-16），devbox-sg
- 症状（owner，Mac demo，claude 2.1.272）：结构视图 TaskTrack 里 subtask/subagent
  完成后行一直留着；前台、后台 subagent 都不消失。
- 本机 `claude` 为 2.1.221；结论用两条证据交叉验证：
  1. 走 **Node 的真实载体**（claude-pty：transcript pump + SessionStart hook
     watch，`REMUDA_PTY_EMULATOR=1`、`REMUDA_PTY_HOOKS=1`）驱动真实 `claude`，
     一前台一后台 Agent（`remuda-driver` 的 `live_tasktrack` ignored 测试）；
  2. 汇总本机多份真实 transcript（2.1.221 与 2.1.272）做离线 mapper 回放。

## 1 复现：每一个 Agent 都拿到了「假 Final」

真实 live run（2026-09-15，隔离 native home、隔离 herdr session
`remuda-tt-repro`）抓到的 transcript 关键记录：

```text
assistant  tool_use   name=Agent id=toolu_vrtx_01CR…   （run_in_background 缺省/true 都有）
user       tool_result tool_use_id=toolu_vrtx_01CR…
                       toolUseResult={"isAsync":true,"status":"async_launched",
                                     "agentId":"af06deb0…","outputFile":"…/tasks/af….output"}
queue-operation enqueue            （通知排队）
user (promptSource:"system", origin:{kind:"task-notification"})
   <task-notification><task-id>af06deb0…</task-id>
   <tool-use-id>toolu_vrtx_01CR…</tool-use-id>
   <status>completed</status><result>DONE</result></task-notification>
```

两个关键事实：

1. **2.1.221/2.1.272 上所有 `Agent` 工具调用都异步返回**：`tool_result` 在调用后
   立即落盘，`toolUseResult.isAsync=true / status="async_launched"`。前台 subagent
   （主会话必须等它）也是同一形态——真正的完成在之后的通知里。旧 mapper 不看
   sidecar，把这条立即返回的记录一律映射成 `ToolResult{stage:Final,
   outcome:Succeeded}`：所以行其实带了 Final，但**内容是「Async agent launched
   successfully」，不是子任务结果**。
2. **真正的完成是注入的 `<task-notification>` user 记录**（`promptSource:system`、
   `origin.kind:task-notification`），带 `<tool-use-id>` 关联回启动调用。旧两个
   mapper（`remuda-journal::claude` 与 `remuda-driver` 的 `TranscriptMapper`）都把它
   当普通 hook-context 消息，**从不产出 ToolResult**，因此完成事件永远不会折到
   工具节点上；任务行停在「刚启动」语义。

离线回放确认（修复前，`TranscriptMapper` 逐行映射真实 transcript）：

- launch：`ToolResult Final/Succeeded rev1 text="Async agent launched…"`；
- 通知：只有一条 `Message origin=hook-context`，无 ToolResult。

## 2 修复（在 mapper/fold 源头，按 §5.2 词表）

`remuda-protocol` 新增纯解析 `task_notification::TaskNotification`（两处 mapper
共用）：

- `<task-notification>` 解析出 `task_id / tool_use_id / status / summary / result`；
- `status → ToolOutcome`：`completed*→succeeded`、`killed/failed/…→failed`、
  `cancelled/stopped/…→cancelled`；
- `tool_result_is_async_launch(sidecar)` 识别 `isAsync:true /
  async_launched|launched|backgrounded`。

映射规则（journal 文件 mapper 与 driver transcript mapper 一致，hook live 层同源）：

| 事件 | 阶段 | 结果 |
| --- | --- | --- |
| 普通（同步）工具结果 | `Final` | 按 `is_error` failed/succeeded |
| Agent 立即返回 `isAsync` | **`Partial`**（TaskTrack 显示「running in background」） | succeeded（仅表示启动成功） |
| `<task-notification>` 完成（带 `<tool-use-id>`） | **`Final`** | 按 `<status>`：completed→succeeded，killed→failed，… |
| `is_error:true`（如 `InputValidationError: missing description`） | `Final` | failed |

配套收敛：

- 工具节点 id 改为对 `(instance scope, native tool_use_id)` 用 `Id::derive`
  （driver 的 `NativeIds::tool` 原本随机），hook relay（Pre/PostToolUse）与
  transcript 回放因此折到**同一个节点**，hooks 在场（shell-pty / promoted）时不出现
  双行；ToolResult 修订号在每条通道内单调递增，通知的 Final 修订号高于 launch 的
  Partial，web `newerMutation` 正常覆盖。
- hook live 层（`remuda-signal::live`）：`PostToolUse(Agent)` 带
  `isAsync/async_launched/agentId` 时产出 `Partial` 且不把调用标记 finished；
  `SubagentStop` 带匹配 `agent_id` 时在同一节点产出 `Final`。**SubagentStop 依旧
  不是 turn 证据**：它不开 phase、不改 raw lifecycle，关闭一个任务行不是 turn
  transition（map.rs 规则与相关单测保持不变）。
- driver `tool_category` 补 `"Agent" => Agent`（原来只有 `"Task"`，2.1.x 工具名是
  `Agent`；web 侧 `familyFor` 本就有 Agent→Task）。
- TaskTrack 仅加状态词：`partial` 结果显示「running in background /
  `data-task-outcome="background"`」，最终态保持 succeeded/failed/denied。

## 3 验证

- 真实 live：修复后同一份 transcript 经 mapper 产出
  `launch Partial rev3 → notification Final rev4`，两个 Agent 都在通知处 Final；
  web `assembleTranscript + collectTasks` 折叠后两任务均 `final/succeeded rev4`。
- 录制 fixture（脱敏，`claude-transcript-tasktrack.jsonl`，driver + journal 各一份）：
  同步前台 1×Final；后台 Partial→Final(Succeeded)；killed Partial→Final(Failed)。
- 单测：`remuda-driver::tasktrack_fold`（3）、`remuda-journal::tasktrack_fold`（1）、
  `remuda-signal::tasktrack_live`（4，含「SubagentStop 不产生 phase」）、
  web `TaskTrack.test.tsx`（新增 background 状态）。
- Node 集成：`remuda-node::tasktrack_node`，fake-harness 新增
  `tools[].async_agent` 脚本能力（立即 async launch + turn 结束后注入
  task-notification），经真实 Node runtime（hook relay + transcript 双通道）断言：
  前台节点不经 Partial 直接 Final；后台节点 Partial 后在同一节点 Final。

无截图（`REMUDA_EVIDENCE` 未置位）；如需截图以 hub 截图流程补在
`web/test-results/evidence/`。
