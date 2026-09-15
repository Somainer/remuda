# Workflow 进度信号实测（r-ux-w spike 1）

- 日期：2026-09-14，devbox-sg
- harness：`claude` **2.1.221**（本机安装版），`claude -p --dangerously-skip-permissions`
- 挂载方式：复刻 D-028 shim overlay——per-session settings 注册
  SessionStart / UserPromptSubmit / Stop / StopFailure / SessionEnd / Notification /
  PreToolUse / PostToolUse / PostToolBatch / MessageDisplay / PermissionRequest /
  Elicitation / **SubagentStart / SubagentStop**（后两者在 2.1.221 上同样触发），
  relay 把 envelope 转发到一个本机 unix socket（Python 捕获器，逐行落 JSONL）。
- 跑过的 run（真实模型，小 prompt，throwaway 目录）：
  1. `spike-wf-1`：2 phase × 2 agent 的 `parallel()`，含 Bash/纯文本 agent（成功路径）；
  2. `spike-wf-kill`：2 × `sleep 45`，launch 后立刻由主会话调用 `TaskStop`（用户停止路径）；
  3. 同一脚本另做了一次进程级 SIGINT（杀整个会话，非任务级停止）；
  4. `spike-wf-fail`：agent 内部 Bash `exit 7`（**不是** workflow 失败）；
  5. `spike-wf-throw`：thunk `throw`（被 `parallel()` 隔离成 `null`，run 仍 completed）；
  6. `spike-wf-runfail`：脚本主体在 agent 完成后 `throw`（run 级 failed）。
- 全机旁证：汇总本机所有历史 workflow（`find $HOME/.claude/projects -name journal.jsonl`），
  共 163 条 `started` / 129 条 `result`，**0 条** `launched` / `failed` / 取消 / 暂停记录；
  151 个 `agent-*.meta.json` 全部只有 `{agentType, spawnDepth}` 两个键。

## 1 实测到的信号（2.1.221）

### 1.1 Workflow 启动：`PostToolUse(Workflow)` 的结构化 `tool_response`

主会话的 `PostToolUse` hook 比 transcript 文本更好用——`tool_response` 是**对象**：

```json
{
  "hook_event_name": "PostToolUse",
  "tool_name": "Workflow",
  "tool_use_id": "toolu_…",
  "tool_input": { "script": "export const meta = { name: …, phases: [{title}] }\n…" },
  "tool_response": {
    "status": "async_launched",
    "taskId": "wn1eumvco",
    "taskType": "local_workflow",
    "workflowName": "spike-wf-1",
    "runId": "wf_c3422384-cb1",
    "summary": "r-ux-w two-phase signals spike",
    "transcriptDir": "$HOME/.claude/projects/<enc>/<sid>/subagents/workflows/wf_c3422384-cb1",
    "scriptPath": "$HOME/.claude/projects/<enc>/<sid>/workflows/scripts/spike-wf-1-wf_c3422384-cb1.js"
  }
}
```

- Workflow 在 2.1.221 上**一律后台启动**（无前台形态；tool_result 文本以
  `Workflow launched in background. Task ID: …` 开头，`PostToolBatch` 里 `tool_response` 是同内容的字符串）。
- `PreToolUse(Workflow).tool_input.script` 在启动前就到，包含完整脚本源码；
  meta.phases 与每个 `agent('<prompt>', { label, phase })` 调用点都在里面。
- `tool_use_id` 即时间线里那条 Workflow tool 行的 id——卡片挂载点的关联键。

### 1.2 SubagentStart / SubagentStop：实时、带 agent 身份

```json
{ "hook_event_name": "SubagentStart",
  "agent_id": "aae139d44933cefe2", "agent_type": "workflow-subagent",
  "session_id": "<主会话 sid>",
  "transcript_path": "$HOME/.claude/projects/<enc>/<sid>.jsonl",
  "prompt_id": "2753c367-…", "cwd": "<工作目录>" }
```

```json
{ "hook_event_name": "SubagentStop",
  "agent_id": "aae139d44933cefe2", "agent_type": "workflow-subagent",
  "agent_transcript_path": "$HOME/.claude/projects/<enc>/<sid>/subagents/workflows/wf_c3422384-cb1/agent-aae139d44933cefe2.jsonl",
  "last_assistant_message": "hello-alpha-1\ndone-alpha-1",
  "effort": { "level": "xhigh" }, "permission_mode": "bypassPermissions",
  "stop_hook_active": false,
  "background_tasks": [
    { "id": "wn1eumvco", "type": "workflow", "status": "running",
      "name": "spike-wf-1", "description": "r-ux-w two-phase signals spike" } ],
  "session_crons": [] }
```

- 成功 run 的事件次序（4 agent）：每个 agent 都是
  `SubagentStart → PreToolUse → PostToolUse → PostToolBatch → SubagentStop`，
  四个 agent 两两并发，与 phase 边界一致（但**载荷里没有 phase/label**）。
- `SubagentStop.background_tasks` 在 run 运行期间恒为 `status:"running"`；
  终态不在这个字段里（完成后的那次主会话 `Stop` 为 `[]`，被 TaskStop 后也是 `[]`）。
- **子 agent 的工具 hook 确实走同一个 socket**：子 agent 的 `PreToolUse` /
  `PostToolUse` / `PostToolBatch` 载荷带 `"agent_id"` / `"agent_type":"workflow-subagent"`。
  `PostToolUse(Bash)` 带 `duration_ms`，`tool_response` 有 stdout/stderr，
  但**任何 hook 载荷都没有 token usage**。

### 1.3 run 目录文件（session 内、append-only、本机可读）

`<transcriptDir>/journal.jsonl`（2.1.221）只有两种记录：

```json
{"type":"started","key":"v2:fdc7…（sha256）","agentId":"aae139d44933cefe2"}
{"type":"result","key":"v2:fdc7…","agentId":"aae139d44933cefe2","result":"PONG"}
```

- 无 `launched`、无 `label`/`phase`（与协调者在 2.1.270 上的记录一致：旧版只有 key+agentId）。
- `key` 是 `(prompt, opts)` 的 memo 键；全机统计中同一 key 出现 2–6 次 `started`
  的有 8 例 → **attempt = 同 key 的 `started` 次数**，重试时不换 agentId 的情形也存在
  （计数按 key 而非 agentId）。
- `agent-<id>.meta.json` 在 2.1.221 上**存在但退化**为 `{"agentType":"workflow-subagent","spawnDepth":1}`：
  没有 description / workflowPhase / model。
- `agent-<id>.jsonl` 是完整子会话：首条 `user` 记录的 message.content 就是
  **脚本里的 prompt 原文**；`assistant` 记录有 `message.model`（如 `claude-opus-4-8`）、
  `message.usage`（input/output/cache_creation/cache_read tokens）、
  `content[].type=="tool_use"` 的 `name`、`timestamp`（毫秒）。
  → 时长 = 首末 timestamp；token = usage 求和；最近工具 = 最后一个 tool_use。
  文件在 SubagentStart 时即已存在（首条 user 记录随 spawn 落盘），运行中持续 append，稳定。
- `workflows/scripts/<name>-<runId>.js` 是脚本副本（0600），`meta.phases` 在其中。

### 1.4 label / phase 在 2.1.221 上怎么还原

journal/meta/hook 三处都没有 label/phase，但脚本源码（1.1，启动时即得）和
agent jsonl 首条 prompt（1.3）给了**确定性连接**：

- 解析脚本里的 `agent('<prompt 原文>', { label: '…', phase: '…' })` 调用点；
- agent spawn 后读其 jsonl 首条 user 文本，与调用点 prompt **精确相等匹配** →
  得到该 agent 的 label 与 phase；
- 动态拼接 prompt / 字面量不匹配时，该 agent 退化为 agentId 短码、不分配 phase
  （整卡是否降级见 §3 的规则）。
- 排队中的 agent 尚不存在 agentId，但静态调用点已知：在「全部调用点都是字面量」
  时可以预渲染 queued 行与总数；否则总数标记为未知，只显示 done/running 计数。

### 1.5 终态：主 transcript 的 `<task-notification>` 注入记录

Workflow 终态不是 hook，而是主 transcript 里的一条注入 `user` 记录（origin=hook-context）：

```
<task-notification>
  <task-id>wn1eumvco</task-id>
  <tool-use-id>toolu_…</tool-use-id>
  <output-file>$TMPDIR/…/tasks/<id>.output</output-file>
  <status>completed</status>
  <summary>Dynamic workflow "r-ux-w two-phase signals spike" completed</summary>
  <result>{…脚本返回值…}</result>
  <usage><agent_count>4</agent_count><agents_done>4</agents_done><agents_error>0</agents_error>
         <agents_skipped>0</agents_skipped><agents_empty_result>0</agents_empty_result>
         <subagent_tokens>83319</subagent_tokens><tool_uses>3</tool_uses>
         <duration_ms>21572</duration_ms></usage>
```

- run 级失败：`<status>failed</status>`，`<summary>Dynamic workflow "…" failed: Error: …
  at <anonymous> (workflow.js:4:7) …</summary>`，usage 块同样存在。
- agent 内部工具失败（Bash exit 7）**不**产生 failed：agents_error=0，run completed；
  thunk throw 被 `parallel()` 隔离为结果数组里的 `null`，run 仍 completed
  （失败只出现在模型转述的 `<failures>` 里，磁盘无结构化记录）。
- 现有 `claude_print` mapper 已有 `map_task_notification`（stream-json 路径）；
  TTY 路径的 transcript mapper 目前只把它归为 hook-context 文本，**不发 workflow.run**。

### 1.6 用户停止：TaskStop（任务级）与 SIGINT（进程级）

- 任务级：主会话对 Workflow 调 `TaskStop({task_id})`，其 tool_result 为
  `{"message":"Successfully stopped task: <id> (…)", "task_id":"…", "task_type":"local_workflow", …}`。
  - run 目录：journal 停在原处（已完成者有 `result`；被停的 running agent **无** result，
    也**没有**任何 cancelled/killed 记录）；
  - 被停 agent 的 jsonl：拿到一个 is_error tool_result
    `The user doesn't want to proceed with this tool use…` +
    `[Request interrupted by user for tool use]`，随后进程结束（有 SubagentStop，无 result）；
  - 之后**没有** `<task-notification>`；终态的唯一结构化证据是 TaskStop 的 tool_result。
- 进程级 SIGINT：整个 claude 会话死亡——无 SubagentStop、无 SessionEnd、journal 冻结。
  这不是 workflow 卡片的「已终止」来源（属于会话/PTY 生命周期，Node 另有处理）。
- 暂停：本机未观测到 workflow 的 paused 信号。bundle 里存在 `TaskResume` 工具名，
  但无对应运行实例；2.1.221 的 workflow 后台任务没有可验证的 pause 通路。
  → `paused` 状态在协议里保留，当前没有任何生产者；卡片不会在真实 run 上看到它。

### 1.7 其它确认项

- `MessageDisplay` 只承载主会话的可见文本（launch 后那句、完成后的总结），
  不含 agent 级进度。
- `Notification` 在六个 run 中均未因 workflow 完成而触发（完成是靠注入的
  task-notification 恢复主 turn，然后一条 MessageDisplay）。
- `TaskCreated` / `TaskUpdate` 与 workflow 无关（agent-team 任务列表），未在 run 中出现。

## 2 文件稳定性结论（协议增补的前提）

协调者要求：只有证明 run 文件稳定且 session-scoped，才允许 Node 做有界读取。实测：

- 路径在 **PostToolUse 结构化响应**里直接给出（不靠猜），位于该主会话自己的
  projects 目录下；Node 与 harness 同主机。
- `journal.jsonl` / `agent-*.jsonl` 均 append-only：运行中只追加，停止后不再变化；
  读取只取末尾窗口即可得到 model / usage / tool_use / timestamp，不读 prompt/result 正文以外内容
  （prompt 首行是 label/phase 连接所必需，属于元数据用途，不做展示）。
- 无 rename/truncate：六次 run + 全机历史样本路径形态一致。

→ 满足有界读取前提。提议的最小增补见 §4。

## 3 字段 → 来源 → 可用性（卡片对账）

| 卡片字段 | 来源（2.1.221 实测） | 结论 |
|---|---|---|
| 挂载点（Workflow tool 行） | `tool_call(Workflow)` ↔ PostToolUse 的 `tool_use_id` | 可用（现有 transcript pump） |
| name / description / phases 顺序 | `tool_input.script` 的 `meta`（Pre/PostToolUse 即得） | 可用，扩展 `parseWorkflowMeta` 到 `phases` |
| runId / taskId / run 目录 | PostToolUse(Workflow).tool_response 结构化字段 | **可用（新增：hook 关系字段 curated）** |
| agent 身份 + 实时 running | SubagentStart（agent_id, workflow-subagent） | 可用 |
| agent 完成/失败 | SubagentStop + journal `result`；agent jsonl 末尾 | 可用（有界读文件） |
| agent label / phase | 脚本调用点字面量 ↔ agent jsonl 首条 prompt 精确匹配；2.1.270+ 原生 meta | 2.1.221 需 Node 连接；匹配失败则该行降级 |
| 模型短名 | agent jsonl `message.model` | 可用（有界读文件） |
| 最近工具 / 调用次数 | 子 agent PostToolUse（带 agent_id）实时；jsonl tool_use 兜底 | 可用 |
| token | agent jsonl `message.usage` 求和（运行中为部分值）；终态 notification 有总量 | 可用（有界读文件；hook 无 usage） |
| 时长 | agent jsonl 首末 timestamp；run 级 notification duration_ms | 可用 |
| attempt（×n） | journal 同 key 的 `started` 次数 | 可用（有界读 journal） |
| queued 行 / 总数 | 脚本静态调用点（全字面量时）；否则未知 | 条件可用，未知时不显示分母 |
| run completed | task-notification `status=completed` + usage | **需 Node 工作（TTY mapper 未发 workflow.run）** |
| run failed | task-notification `status=failed` + summary | 同上 |
| run killed（已终止） | TaskStop tool_result（task_type=local_workflow） | **需 Node 工作（mapper 识别）** |
| run paused | 无可观测信号 | 不可用：协议保留，无生产者 |
| 「当前 phase: agent」实时行 | 最近一个 running 连接成功的 agent（label/phase 来自 §1.4） | 可用（连接失败时只显示 agent 短码） |
| 旧 daemon / 无明细兜底 | 无 PostToolUse 结构化字段、无 run 目录、脚本无 meta.phases | 前端按 decision 6 降级 |

## 4 提议的最小协议/Node 增补（全部 additive）

1. **新观察类型 `workflow.progress`（快照，按 revision 替换）**，一个 payload 自带卡片所需全部字段：
   `{ workflowId, toolCallId?, nativeRunId, nativeTaskId, name, description, state,
      phases: [{ phaseId, title, totalKnown, agents: [{ agentId, key?, label, state:
      queued|running|done|failed|killed, model?, latestTool?, tokens, calls, durationMs?,
      attempt }] }],
      totals: { totalKnown, agents, done, failed, killed, queued, tokens, calls, elapsedMs },
      livePhase?, liveAgent?, note?, revision }`。
   事件由 Node 发，web 只做纯投影；旧消费者忽略未知类型，安全。
2. **remuda-signal `map.rs` 扩 curated 关系字段**（不新增事件）：
   子 agent 事件加 `agentId`/`agentType`；PostToolUse(Workflow) 加
   `runId`/`taskId`/`transcriptDir`/`scriptPath`/`workflowName`；
   子 agent PostToolUse 加 `durationMs`；SubagentStop 加 `agentTranscriptPath`
   与 background_tasks 摘要。纯函数、用真实捕获样本做 fixture 单测。
3. **remuda-driver TTY transcript mapper**：把 `<task-notification>` 映射为终态
   `workflow.run`（对齐已有 `claude_print::map_task_notification`）；
   TaskStop tool_result（task_type=local_workflow）映射为 `cancelled` 终态。
4. **remuda-node 新增 workflow 进度折叠器**（observation pump 内，文件只读、有界）：
   hook 给实时身份/工具，journal/agent jsonl 给 label 连接/model/token/时长/attempt，
   终态 workflow.run 收口；每个状态变化发一条 `workflow.progress` 快照。
   连接不上的 agent 不臆造：按 decision 6 让卡片显示降级说明。
5. **fake-harness**：scenario 增加 workflow turn，落合成 run 目录（脚本副本、
   journal、agent jsonl）并按真实次序发 hook，使 hub-live e2e 走完整条链路。
