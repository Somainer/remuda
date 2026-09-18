# Grok Structural 转译改进方案

日期：2026-09-18  
状态：勘察 + 方案（本文不改代码）  
对照二进制：本机 `grok 1.0.34 (3736acbc8658)`，模型 `grok-4.6`  
夹具基线：`crates/remuda-driver/tests/fixtures/grok/` 来自 **grok 1.0.30**，且探测进程带 `--no-subagents --no-plan`  
已读：`grok_adapter.rs`、`grok_session.rs`、`remuda-acp-wire`、`protocol.md` §5.7 grok-acp、`native-pty-first.md` P6/风险 15、`live-structured-view.md`、`harness-parity.md`、`usage-adapter.md`、`files-view-contract.md`、`web/src/features/session/{toolPresenters,toolRegistry,assemble,live/*}`、`remuda-signal/src/live.rs`

本文回答一件事：Remuda 结构视图（Structural）对 **Grok 这一家 harness** 的转译，值不值得改、改什么、按什么顺序改。结论来自两处对照——仓库里已经落地的 P6 文件适配器，以及本会话作为 Grok Build TUI 主循环实际看到的工具面。

---

## 0. 结论先行

**值得改，而且缺口不在「再写一个 grok-acp driver」。**

P6 已经把 Grok 的 `updates.jsonl` / `events.jsonl` 接到 journal：文本块、思考块、工具起止、回合边界、事后权限对账、session 用量都能进结构视图。这比 Claude 的 transcript 文件层更早——Grok 在 **调用当下** 就落 `tool_call`，Claude 往往要等工具跑完才写 `tool_use`。

结构视图今天看起来「不像 Grok」，是因为转译停在「能解析的最小子集」，而消费端（live 条、工具卡、子代理、工作流树）是按 Claude 名字和 `turn.live` 相位做的。三层对不齐：

1. **协议已经写了、adapter 没按协议做**（工具名、title、kind、diff、progress update）。
2. **live 层已经预留、adapter 没往里填**（`thinking` / `tool-output` 相位就是给文件/RPC 适配器留的，见 `remuda-signal/src/live.rs` 文件头）。
3. **真实 Grok 1.0.34 的工具面，夹具从未见过**（探测用了 `--no-subagents --no-plan` + 只有 `run_terminal_command` / `ask_user_question` 的假 Completions 服务）。

不要用第二条 ACP 进程给同一个 PTY 实例做「结构 overlay」。D-028 的统一原则仍然成立：一个 agent session 就是一个 terminal session。Grok TUI 落盘的本来就是 ACP 帧，文件尾就是结构通道；缺的是把这些帧译成结构视图已经认识的形状。

`grok-acp` 只在一件事上仍有独立价值：结构化审批作答（风险 15）。那是控制面，不是结构转译。

---

## 1. Structural 在本项目里指什么

结构视图不是终端的 ANSI 回放，也不是把 JSONL 原样摊开。它是 journal 投影：

| 原生事实 | 协议观测 | 结构视图 |
|---|---|---|
| 用户/助手文本 | `message` open/append/close | 气泡 |
| 思考 | `thought` | 折叠 thinking |
| 工具调用/结果 | `tool_call` / `tool_result` | `ToolCard` |
| 回合相位 | `lifecycle.native` + `related_ids.phase` | LiveStatusStrip / 本地计时 |
| 子代理 | `parent_tool_call_id` + `native_agent_id` | 折叠行 + `/agents/:id` |
| 工作流 | `workflow.run/phase/member` | WorkflowTree |
| 提问/审批 | `interaction` | QuestionForm / ApprovalCard |
| 文件改动 | `tool_result.changes[]` | 卡片 diff / 日后 files 视图 |

权威阶梯不变：`Hook > File > OSC > Screen`。Grok 的结构内容应以 **File（ACP 帧）** 为权威；OSC 标题只许抬 `busy` / 补相位提示，不许当正文。

---

## 2. 今天实际接到了什么

路径：`(grok, shell-pty)` → 影子 `GROK_HOME` → `GrokAdapter` 每 250 ms tail → supervisor stamp → journal → Hub → `assembleTranscript`。

`GrokAdapter` 已映射：

| 源 | 产出 | 保真度 |
|---|---|---|
| `user_message_chunk` | `message` user，Structured | 文本 only；origin 恒空（渲染成人） |
| `agent_message_chunk` | `message` assistant Streaming + `turn_completed` Close | 按 `promptId` 折叠；非 text content 丢弃 |
| `agent_thought_chunk` | `thought` Streaming；回合结束用 `thought_payload`（Open 语义）再发一次全文 | 思考通道是 Grok 相对 Claude 的优势 |
| `tool_call` | `tool_call` state=`Proposed`，category=`Shell` | 名字优先 `title`，不是协议要求的 `_meta["x.ai/tool"].name` |
| `tool_call_update` **有 status** | `tool_result` Final | progress / 无 status 的 update **直接丢** |
| `turn_completed` | 关闭当前 prompt 的 message/thought | `stop_reason=cancelled` → Interrupted |
| `events.jsonl` `turn_started`/`turn_ended` | lifecycle `turn_started`/`turn_ended` | Node 用来切 Working/Idle |
| `permission_requested`/`resolved` | lifecycle topic=Permission | 事后对账，不可答 |
| `usage.json` + update 内 usage | `UsagePayload` session snapshot | 回合边界才读文件；夹具里几乎是空对象 |

明确没映射（协议表已点名）：`plan`、`available_commands_update`、`session_info_update`、`current_mode_update`、`config_option_update`、`hook_execution`、`tool_call_delta_chunk`、`pending_interaction`、`background_tasks`、`model_changed`、`phase_changed`、`first_token`。content 的 `diff` / `terminal` / 非 text 块也不进 `ContentBlock` / `FileChange`。

Web 侧另外两处断层：

- Live 条只认 `nativeName === "turn.live"` 且 `related_ids.phase` 为九值词汇。Grok 发的是 `turn_started`。`live.rs` 写明 `thinking` / `tool-output` **就是留给文件适配器的**，Grok 没填。
- `toolRegistry` / `presentTool` 只认识 Claude 名：`Bash` / `Read` / `Write` / `Edit` / `Workflow` / `Task`。Grok 的 `run_terminal_command` 落到 Generic 卡，把 `rawInput` 做成键值表。`ask_user_question` 也当 Generic 工具，而不是 Question 卡。

夹具本身的范围（必须写进方案，否则后续会把「没见过」写成「不支持」）：

```
grok --no-alt-screen --no-subagents --no-plan -m spike
```

工具面只有 `run_terminal_command` 和 `ask_user_question`。`spawn_subagent`、`workflow`、plan mode、MCP、搜索/读改文件族、后台任务、图像工具全部不在 1.0.30 夹具里。本机已经是 1.0.34。

---

## 3. 真实 Grok harness 发出什么（对照转译）

以下按 **Grok Build TUI 主循环实际暴露的工具** 分组。ACP 帧上稳定身份在 `_meta["x.ai/tool"]`：

```json
{ "name": "run_terminal_command", "kind": "execute", "namespace": "grok_build", "label": "Run Command", "read_only": false }
```

`title` 是展示句（`Execute \`printf…\``），会随 update 变；`name` 才是稳定工具名。协议 §5.7 已经这样写了，adapter 写反了。

### 3.1 工具身份与类别（应进 `ToolCallPayload`）

| `_meta.x.ai/tool.name` | 建议 `ToolCategory` | 结构卡应像 | 输入要点 |
|---|---|---|---|
| `run_terminal_command` | Shell | 等同 Claude `Bash`：`$ command`、cwd、exit、是否后台 | `command` / `description` / `is_background` / `timeout` |
| `read_file` | FileRead | 路径 + 行范围 | `target_file` / `offset` / `limit`（不是 `file_path`） |
| `write` | FileWrite | 路径 | `file_path` + `content`（ACP 探针是 opencode namespace） |
| `search_replace` | FileWrite | 路径 + 替换摘要 | `file_path` / `old_string` / `new_string` / `replace_all` |
| `list_dir` | FileRead | 目录 | `target_directory` |
| `grep` | Search | pattern + path/glob | `pattern` / `path` / `glob` |
| `web_search` / `web_fetch` / `open_page` / `open_page_with_find` | Search | 查询/URL | |
| `spawn_subagent` | Agent | 等同 `Task`：description、subagent_type、isolation | `prompt` / `description` / `subagent_type` / `isolation` / `resume_from` |
| `workflow` | Workflow | **不要**走 Claude `export const meta`；Grok 是 Rhai `let meta = #{ name, description }` | `source`（name / script / script_path / resume / pause / stop） |
| `search_tool` / `use_tool` | Mcp | `server/tool`，Grok 限定名是 `server__tool` 而不是 Claude 的 `mcp__server__tool` | `use_tool.tool_name` |
| `ask_user_question` | Other，但观测应升格为 `interaction` | QuestionForm，不是工具卡 | `questions[].question/options`，可 multi_select |
| `todo_write` | Other | 待办列表 | `todos[]` |
| `monitor` / `scheduler_*` / `get_command_or_subagent_output` / `kill_command_or_subagent` | Other | 后台任务快照；对接 `_x.ai` `background_tasks` | |
| `enter_plan_mode` / `exit_plan_mode` | Other + `plan` sessionUpdate | lifecycle plan，**不是** workflow | |
| `image_gen` / `image_edit` / `image_to_video` / `reference_to_video` | Other | 媒体卡；`promptCapabilities.image=false` 管的是 **prompt 能否带图**，不是这些工具不存在 | |
| `x_*` | Search | 查询卡 | |

`kind` 字段已实测：`execute` / `write` / `ask_user` / `edit` / `other`。类别优先查名字表，名字未知再回落 `kind`（execute→Shell，write/edit→FileWrite，其余 Other）。禁止再把所有调用标成 Shell。

### 3.2 工具生命周期（Grok 比 Claude 文件层更完整）

一次真实 `run_terminal_command` 在夹具里是三帧：

1. `tool_call`：`status: Pending`，`title` = 工具名，`rawInput` 已有 command。
2. `tool_call_update` **无 status**：补 `kind: execute`、人类可读 title、`locations`、规范化 `rawInput.variant=Bash`。这就是 **Running**。
3. `tool_call_update` `status: completed`：`rawOutput.exit_code`、`output_for_prompt`、`output_file` 指向 `…/terminal/<callId>.log`。

今天 adapter 吃 1 和 3，丢 2。于是结构视图里工具卡永远是「已提出」，中间十几秒没有 Running、没有 elapsed、没有 stdout 尾巴。

Grok 还把命令 stdout 写到 session 目录的 `terminal/<callId>.log`。Claude 文件层没有等价物。这是结构视图「工具正在跑」的最佳通道，比等 completed 帧早一个数量级。

`ask_user_question` 同构：Pending → 无 status 的 Ask 标题 → `rawOutput.UserAnswered`。这应变成 `interaction.requested`（QuestionInput::SingleSelect / MultiSelect），而不是一张 Generic 工具卡。答案已经在 `tool_result` 里，结构视图应显示选项和用户选择。

### 3.3 思考是一流流

Claude 的 `MessageDisplay` 不含 thinking；结构视图用屏幕短语假装「正在想」。Grok 的 `agent_thought_chunk` 是真增量。adapter 已经 append 这些块，但：

- 回合结束用 `thought_payload`（mutation Open）覆盖，而不是 Close，和 message 的 open/append/close 不一致。
- 没有 `turn.live` phase=`thinking`，LiveStatusStrip / `mountLiveThinking` 看不到 Grok 正在推理——它们要么等 Claude 屏幕短语，要么等 `turn.live`。

Grok 应成为第一家 **native thinking 相位** 的 harness，不要再用屏幕猜。

### 3.4 子代理和工作流不是 Claude 的那一套

`assemble.ts` 把子代理折进 `parentToolCallId` / `nativeAgentId`，下钻读 Claude 的 `agent-<id>.jsonl`。Grok 的 `spawn_subagent`：

- 工具名不是 `Task` / `Agent`。
- 子会话是 Grok session（同样有 `updates.jsonl`），不是 Claude transcript。
- `isolation: worktree` 是一等输入。
- `resume_from` 续跑同一子代理，不是新 Task。

Grok `/workflow` 是 Rhai：`let meta = #{ name, description };` + `agent()` / `parallel()`，有独立的 run 名、budget、pause/resume/stop。协议 `WorkflowEngine` 今天只有 `claude-workflow`。能力矩阵已经写死：Grok 的 ACP `plan` **不准**升格成 workflow，Grok 自己的 workflow **也不能**冒充 Claude engine。

结构视图正确做法：

- `spawn_subagent` → `ToolCategory::Agent` + `parent_tool_call_id` 链；下钻走 Grok session 目录，而不是 `agent-*.jsonl`。
- `workflow` 工具卡用 Rhai meta；进度若有原生 run 观测再开 `WorkflowEngine::GrokWorkflow`（新枚举）。没有观测之前，只做工具卡，不进 Claude WorkflowTree。
- ACP `plan` sessionUpdate → `lifecycle.native` topic 与 plan 相关，保持「执行计划」语义。

未抓到 1.0.34 的 `spawn_subagent` / `workflow` ACP 帧之前，字段映射标 **[U]**，UI 先按名字表渲染输入，不发明 parent 关系。

### 3.5 权限：结构可看，结构化作答仍没有

已验证：

- `PermissionRequest` hook 名被 Grok 静默忽略。
- `PreToolUse` 能 deny/ask，不能替用户选 allow_once。
- `session/request_permission` **不落** `updates.jsonl`。
- `events.jsonl` 的 `permission_requested`/`resolved` 是事后账（`wait_ms:0` ≠ 没弹框）。
- `--always-approve` 下只有 `pending_interaction` → `interaction_resolved`。

结构视图可以显示「正在等许可」（OSC 标题 `⚠ Action Required`、permission 生命周期、屏幕）。作答继续走屏幕按键（emulated），capability 不得标 native。真正的 JSON-RPC 作答应留给独立评估的 `grok-acp` client，不塞进这条 PTY 结构管道。

### 3.6 版本差

| | 夹具 | 本机 |
|---|---|---|
| CLI | 1.0.30 | 1.0.34 |
| 探测开关 | `--no-subagents --no-plan` | 默认可子代理 / plan |
| headless | streaming-json = ACP 帧（`--help` 现已写明） | 同左 |

任何「Grok 不支持 X」的断言，若证据只来自 1.0.30 精简探测，一律视为过期。

---

## 4. 缺口矩阵（按结构视图用户能看见的东西排）

严重度：对结构页「像不像正在跑的 Grok」的影响。

| # | 缺口 | 层 | 严重度 | 证据 |
|---|---|---|---|---|
| G1 | 工具名用 `title` 冒充 `name`；`display_title` 未分开 | adapter vs 协议 §5.7 | 高 | 夹具帧 1 的 title=`run_terminal_command`，帧 2 title 变成 `Execute \`…\`` |
| G2 | `category` 恒 Shell | adapter | 高 | `tool_call_payload()` 写死 |
| G3 | 无 status 的 `tool_call_update` 丢弃 → 永不 `Running` | adapter | 高 | `grok_adapter.rs` 注释「Ignore the progress one」；`live.rs` 正等 Running |
| G4 | `changes: []`，diff content 未译 | adapter / 协议已有 FileChange | 高 | files-view-contract：全仓零构造点；Grok ACP 探针有 `content:[{type:diff}]` |
| G5 | 不发 `turn.live` 相位；思考/工具相位 Web 看不见 | adapter → live UI | 高 | LiveStatusStrip 只认 `turn.live`；`live.rs` 给文件适配器留了 thinking/tool-output |
| G6 | Web presenter / registry 不认 Grok 名 | web | 高 | `toolPresenters.ts` 只分支 Claude 名 |
| G7 | `ask_user_question` 当 Generic 工具 | adapter + web | 中高 | 夹具有完整问答；QuestionForm 已存在 |
| G8 | 不 tail `terminal/<id>.log` | adapter | 中 | `rawOutput.output_file` 已在 completed 帧里 |
| G9 | thought 结束用 Open 不是 Close | adapter | 中 | 与 message 折叠不一致 |
| G10 | `phase_changed` / `first_token` / OSC 标题未进相位 | adapter / screen | 中 | osc-samples：`Thinking` / `Running: <tool>` / `⚠ Action Required` |
| G11 | 子代理不折叠、无 Grok 下钻 | adapter + SubagentView | 中 | 夹具故意 `--no-subagents`；视图写死 Claude jsonl |
| G12 | workflow 不当 Rhai、engine 只有 claude-workflow | 协议 + web | 中 | 能力矩阵已禁止混用 |
| G13 | MCP 名 `use_tool` / `server__tool` 被 `__` 启发式误伤或认不出 | web | 低中 | `familyFor`：`includes("__")` → MCP |
| G14 | 非 text content 丢弃 | adapter | 低中 | `content_text()` |
| G15 | usage 只在 session 快照，少 turn 级 | adapter | 低 | `emit_session_usage` 挂在 `turn_ended` |
| G16 | `available_commands_update` / 技能目录未进结构 | adapter | 低 | 体积大，只作 configuration 生命周期 |
| G17 | 屏幕签名仍几乎是 Claude | remuda-screen | 低（结构次要） | 风险 12；结构应以文件为准 |
| G18 | 夹具/假服务覆盖面过窄 | 测试 | 门禁 | 1.0.30 + 两工具 |

`harness-parity.md` 里「Grok ingested today: No」已经过期（P6 已接线）。过期的是 **live 相位和工具呈现**，不是 parser。

---

## 5. 约束（改的时候不许踩）

1. **不给 PTY 实例再挂一条 grok-acp stdio。** 双 writer、双 session id、Terminal 与 Structural 分叉。结构继续吃 TUI 落盘的 ACP 帧。
2. **initialize 不声明 fs/terminal。** 声明了会把写盘和 shell 反代到 Remuda；遥控 Node 时工具必须落在远端。
3. **ACP `plan` ≠ workflow；Grok Rhai ≠ `claude-workflow`。**
4. **文件层权限不可答。** 结构可显示 blocked；作答保持屏幕 emulated，或另开 grok-acp 评估。
5. **不把 OSC / 屏幕当正文。** OSC 只可抬 busy 和相位提示。
6. **未知 `sessionUpdate` 保持 opaque。** 不靠缺席证据宣称不支持。
7. **`--always-approve` 探针不能当审批协议已通。**
8. **金额继续标估算。** `usage.json` 文件体仍标偏差，直到 1.0.34 实文件复核。

---

## 6. 目标转译（adapter 应当发出的形状）

对每一条 `updates.jsonl` 帧，稳定键是 `params._meta.eventId`（已有去重）+ `toolCallId` / `promptId`。

### 6.1 工具

```
tool_call
  → ToolCall { name: meta.tool.name,
               display_title: title,
               category: table(name) ?? kind,
               input: rawInput,
               state: Proposed }

tool_call_update 无 status
  → 同 node：Replace/Append
       state: Running
       display_title 更新
       input 若 rawInput 变了则更新（不含字段保持旧值，协议已写）
       若 content[].type==content 且有 text → ResultStage::Partial（不关节点）

tool_call_update status=completed|failed|denied|cancelled
  → ToolResult Final
       outcome 沿用现有 tool_outcome()
       blocks: content 文本 / terminal 引用说明
       changes: content[].type==diff → FileChange{path, diff, application:Applied 仅当 status=completed 且无 error}
       structured_result: 整帧 update（已做）
       exit_code: rawOutput.exit_code
```

`terminal/<id>.log`：在 Running 期间按 250 ms 已有节奏读增量，发 Partial `tool_result` 文本块。文件尚未出现不算错误。completed 帧到达后停 tail。这是 Grok 独有的 live stdout，Claude 没有对等物，不要为了「三家对称」丢掉。

### 6.2 相位（接到已有 Web，不新造 payload）

在现有 lifecycle 上打 `turn.live` 标签，或另发一条 `nativeName="turn.live"`（Claude hook 层选了「标签骑在原事件上」以免双观测。Grok 文件层没有这个约束，**建议另发 `turn.live`**，保留 `turn_started`/`turn_ended` 给 Node activity。Web 已经按名字过滤。）

| 触发 | phase |
|---|---|
| `turn_started` 或首条 `user_message_chunk` | `prompt-accepted` |
| 首条 `agent_thought_chunk`（尚无 tool/text） | `thinking` |
| `first_token` 或首条 `agent_message_chunk` | `text-streaming` |
| `tool_call` | `tool-started` |
| 无 status 的 `tool_call_update` 或 terminal log 字节 | `tool-output` |
| 终态 `tool_call_update` | `tool-finished` |
| `permission_requested` 或 OSC `⚠ Action Required` | `blocked` |
| `turn_ended` outcome=completed | `turn-ended` |
| `turn_ended` outcome=cancelled | `interrupted`（可同时保留 related_ids.trigger） |

`phase_changed` 的原生字符串进 related_ids 作诊断，不直接当九值相位（词汇未钉死）。

### 6.3 提问

`ask_user_question` 的 Pending 帧 → `interaction.requested`，`QuestionInput` 按 `multiSelect` 选 single/multi。completed 的 `UserAnswered` → `interaction.resolved`，并保留配对的 `tool_result` 方便 transcript 回放。结构页优先渲染 Question 卡。

未验证的 `session/request_permission` 仍然不可从文件重建。不要伪造 optionId。

### 6.4 Web 呈现

`familyFor` / `presentTool` 按 **native 名** 分发，不要先把 Grok 改名成 Bash。Grok 卡与 Claude 卡可以共享布局函数（`presentShell(command)`），但 registry 键是 `run_terminal_command`。

Rhai meta 解析与 Claude `export const meta` 分开；解析失败则退回 `source` 类型 + 截断脚本，永不 blank 卡。

`familyFor` 的 `includes("__")` 只用于 **MCP 限定名本身**（`use_tool` 的 `tool_name`），不要把任意带 `__` 的 Grok 工具标成 MCP。

### 6.5 子代理 / 工作流（有帧再升级）

第一期：名字表 + 卡片。`parent_tool_call_id` 仅在帧里出现稳定 parent/child id 时填写。

第二期（需 1.0.34 实帧）：

- 子代理 session 目录发现（dashboard 说「every session, top-level and subagents」）。
- SubagentView 增加 Grok 投影（同一 `GrokAdapter` bind 到子目录），路由仍用 native id。
- `WorkflowEngine` 增加 `grok-workflow`；没有 run 级观测就不往树上挂。

---

## 7. 明确不做（本方案范围外）

- 实现完整 `Driver` for `grok-acp`（M4-03）。保留 wire crate；需要结构化审批时单独立项。
- 把 Grok 结构通道改成 headless `-p --output-format streaming-json` 作为默认载体。headless 是 ACP 帧，但没有 Terminal 同行，违反 D-028。
- 用屏幕签名冒充工具行。Grok 结构以文件为准。
- 为图像/X/computer-use 各做一套协议类型。先走 Generic/Other 卡 + 诚实 presenter。
- 改 Claude/Codex adapter 来「对齐」Grok。不对称是真实的：Grok 有 live thinking 和 terminal log，Claude 没有。

---

## 8. 分批落地

原则：每一批独立可审、可测；先修「协议已写但译错」和「live 层已留洞」；再扩工具面；最后才碰协议枚举和子代理下钻。

### PR 1 — 工具身份与 Running（adapter，零协议变更）

**标题：** `fix(driver): grok tool identity, category, and running updates`

**文件：** `grok_adapter.rs`、`adapters/mod.rs` 的 `tool_call_payload`（不要让 Codex 误伤：Grok 专用 builder 或加参数）、`grok_adapter/tests.rs`、夹具断言。

**内容：**

- `tool_name` ← `_meta["x.ai/tool"].name`；缺则 Unknown，**不用 title 填**。
- `display_title` ← `title`。
- `category` 按 §3.1 表。
- 无 status 的 `tool_call_update` → 同 node `state=Running`，更新 title/input。
- 终态帧继续发 `tool_result`；`results` 去重保留。

**验收：** 用现有 `tui-updates.jsonl`：`run_terminal_command` 的 name 稳定、中间帧 Running、completed 仍 Succeeded + exit 0。`ask_user_question` 类别 Other。

### PR 2 — diff / content 块（adapter，零协议变更）

**标题：** `feat(driver): map grok tool content/diff into FileChange`

**文件：** `grok_adapter.rs`、tests；必要时小测 `ContentBlock` 构造。

**内容：** `content[]` 按 type 分支：`content`→文本块，`diff`→`FileChange`（path 从 locations 或 diff 对象取），`terminal`→说明块 + 保留 terminal id（不是 tty-attach 承诺）。application：completed 无 error 才 `applied`，否则 `unknown`。

**验收：** 给一条合成 `type:diff` 帧；`changes` 非空。files 视图仍按 G1 契约不声称「本会话改动」，但 ToolCard 能展示 diff。

### PR 3 — `turn.live` 相位（adapter → 现有 Web）

**标题：** `feat(driver): emit turn.live phases from grok file adapter`

**文件：** `grok_adapter.rs`（可抽 `grok_live.rs`）、`live.rs` 相位枚举保持不动、web 测试若缺 grok fixture 则补一条。

**内容：** 按 §6.2 另发 `turn.live`。thinking 用 thought chunk，tool-output 用 Running 更新。`turn_started`/`turn_ended` 行为不变（Node activity）。

**验收：** 把真实夹具喂给 adapter，投影 `livePhase` 走过 `prompt-accepted → thinking|tool-started → turn-ended`。取消回合落到 `interrupted`。Web LiveStatusStrip 在 grok journal 回放里出现 elapsed（本地 1 Hz，不增 wire）。

### PR 4 — Web Grok presenters

**标题：** `feat(web): grok-native tool cards`

**文件：** `toolRegistry.ts`、`toolPresenters.ts` + tests、`ToolCard.tsx` 仅当需要 category 图标。

**内容：** 注册 §3.1 名字。`run_terminal_command` 复用 shell 布局（读 `command` 不是 Claude 的 `Bash` 名）。`read_file` 认 `target_file`。`search_replace` / `write` 认 `file_path`。`spawn_subagent` 用 Task 布局。`workflow` 解析 Rhai `let meta = #{...}`。收紧 MCP 启发式。

**验收：** presenter 单测覆盖夹具里的 `run_terminal_command` 与 `ask_user_question` 输入；`run_terminal_command` 卡片标题不再是生 `title` JSON。

### PR 5 — 提问升格为 interaction

**标题：** `feat(driver): grok ask_user_question as interaction`

**文件：** `grok_adapter.rs`、protocol 若现有 interaction builder 可复用则不改 schema、web 结构页已能渲染 interaction。

**内容：** Pending → requested；completed UserAnswered → resolved。仍保留 tool_call/result 以便回放。不可答的 permission 生命周期不动。

**验收：** 夹具 QUESTION 回合在结构页出现选项 Alpha/Beta 且选中 Alpha；不出现「未验证的审批 RPC」。

### PR 6 — terminal log 增量（Grok 独有 live stdout）

**标题：** `feat(driver): tail grok session terminal logs while tools run`

**文件：** `grok_adapter.rs` 或 `grok_terminal.rs`、fake-harness 写 log 文件。

**内容：** Running 期间 tail `output_file` 或惯例路径 `sessionDir/terminal/<toolCallId>.log`。Partial result，truncated 标记若 rawOutput 将来带。completed 后停止。

**验收：** fake-harness 在 completed 前写入多行 log，journal 出现 Partial 再 Final；Final 文本不因 Partial 双算（投影以 Final 为准，Partial 只供 live）。

### PR 7 — 夹具重采（门禁，先于子代理/工作流承诺）

**标题：** `test(driver): recapture grok 1.0.34 structural fixtures`

**内容：** 对 1.0.34 **不要** `--no-subagents --no-plan`。至少录：

- 文本 + thinking + `run_terminal_command`
- `read_file` / `grep` / `search_replace`
- `ask_user_question`
- 一次 `spawn_subagent`（isolation none 即可）
- 一次最小 Rhai `workflow`（若默认工具仍在）
- 一次取消（双 Ctrl+C）
- 真实 `usage.json` 体

影子配置继续本地 spike 或受限 live，密钥不进仓。更新 `fixtures/grok/README.md` 的 scrub 规则。

**验收：** PR 1–6 的测试改打新夹具；旧 1.0.30 夹具可留作回归直到替换。

### PR 8 — 子代理下钻与 Grok workflow engine（依赖 PR 7）

**标题：** `feat: grok subagent fold and grok-workflow engine`

**文件：** protocol `WorkflowEngine` 增加 `grok-workflow`（这是本方案唯一建议的协议增量）、journal 投影、`SubagentView` 双路径、adapter parent 关联。

**内容：** 仅在 PR 7 帧给出稳定 id 时实现。没有帧就停在 PR 4 的卡片，不猜。

**验收：** 一条 spawn_subagent 在主结构页折叠；下钻看到子 session 的工具，而不是空的 Claude jsonl。

### 可选后续（单独立项，不是结构转译的前置）

- OSC 标题 → busy / `blocked` 相位（只升 busy）。
- Grok 屏幕签名（风险 12），服务 Terminal，不服务结构正文。
- `grok-acp` 审批 client（风险 15）。
- per-turn usage 观测。
- `available_commands_update` 作技能目录。

---

## 9. Key Decisions

1. **结构通道保持 file-tail of TUI ACP 帧，不叠加 grok-acp 进程。** TUI 已经在写 ACP；第二条 stdio 会分裂 Terminal/Structural，且与 D-028 冲突。
2. **工具身份以 `_meta["x.ai/tool"].name` 为准，title 只做 display。** 协议已规定；今日实现写反，是 bug 不是产品选择。
3. **无 status 的 `tool_call_update` 是 Running，不是噪声。** 丢掉它等于丢掉 Grok 相对 Claude 文件层最大的 live 优势。
4. **Grok adapter 必须发 `turn.live`。** Web live 条已经按这套九值相位工作；`thinking`/`tool-output` 是专门给 Grok/Codex 留的。
5. **Grok workflow 与 Claude workflow 分引擎；ACP plan 不升格。** 延续能力矩阵，避免 WorkflowTree 把 Rhai run 画成 Claude phase。
6. **`ask_user_question` 进 interaction，许可仍 emulated。** 前者文件里有完整问答；后者 RPC 不落盘。
7. **先重采 1.0.34 夹具，再承诺子代理/工作流字段。** 1.0.30 `--no-subagents --no-plan` 不能当「Grok 没有这些工具」。

---

## 10. Open Questions

1. **`turn.live` 另发一条 vs 打在 `turn_started` 上？** 建议另发（Web 按 `nativeName` 过滤；Node activity 继续用 `turn_started`）。若希望少观测，需改 Web 同时认两套名字——不建议。
2. **`spawn_subagent` 的子 session 目录布局在 1.0.34 是什么？** dashboard 声称能列 subagents；需 PR 7 实采。在此之前不做下钻。
3. **Grok workflow 的进度通道是 `updates.jsonl` 扩展、独立文件，还是只有工具结果？** 未采到之前只做工具卡。
4. **`write` 的 namespace `opencode` vs `grok_build`：** 结构层只认 `name`，namespace 进 related/opaque。是否要在卡上标注 namespace，等实帧再看有没有用户可感差异。
5. **是否把 `grok-acp` 审批 client 排进本方案？** 默认否。它不改善结构转译，只改善作答。需要的话另开控制面设计。

---

## PR Plan

| 顺序 | PR | 依赖 | 主要文件 |
|---|---|---|---|
| 1 | 工具身份 / category / Running | 无 | `crates/remuda-driver/src/adapters/grok_adapter.rs` |
| 2 | diff/content → FileChange | 1 | 同上 |
| 3 | 发 `turn.live` 相位 | 1 | `grok_adapter.rs`；web live 回放测试 |
| 4 | Grok tool presenters | 1（可与 3 并行） | `web/src/features/session/tool{Registry,Presenters}.*` |
| 5 | `ask_user_question` → interaction | 1 | adapter + 现有 Question 卡 |
| 6 | tail `terminal/*.log` | 1 | adapter + fake-harness |
| 7 | 1.0.34 夹具重采 | 1–6 可先用旧夹具，7 之后收紧 | `crates/remuda-driver/tests/fixtures/grok/` |
| 8 | 子代理下钻 + `grok-workflow` engine | 7 | protocol enums、journal、SubagentView、adapter |

1–6 不改 wire schema。8 是唯一协议增量（`WorkflowEngine::GrokWorkflow`），且可在证据不足时取消而不回滚 1–6。
