# Workflow 子会话分组与钻取（c-wfdrill）

- Date: 2026-09-17
- Harness: hub e2e fake Node，`workflow card drill` 场景（`crates/remuda-hub/examples/hub_e2e.rs` 的 `append_workflow_scenario("drill")`）
- Spec: `web/tests/e2e/ux-wfdrill.hub.spec.ts`
- Captures: `workflow-drill-grouping-1440-night.png`, `workflow-drill-grouping-390-night.png`, `workflow-drill-in-view-1440-night.png`, `workflow-drill-in-view-390-night.png`

## 问题

Workflow 成员和 Agent/Task 子代理都是**同一个 Remuda 会话内的 Claude 子会话**，不是独立实例。截图里成员行显示「completed · 无 childInstanceId，不可点进 · claude-opus-5」，而子代理自己的 `PreToolUse/PostToolUse` 产生的工具节点被平铺在主结构化会话里。

## 改动

### 身份贯通（hook 通道）

子代理的工具 hook 载荷带 `agent_id`（2.1.221/2.1.273 一致，见 `workflow-progress-signals-1.md` §1.2）。此前 `remuda-signal` 的派生 `ToolCall/ToolResult` 把它丢弃：`executor` 恒为 unknown，`source.native_agent_id` 恒为 not-applicable。

- `crates/remuda-signal/src/bus.rs`：`emit_derived` 透传 `agent_id`，盖到派生观测的 `source.nativeAgentId`；主会话 hook 不带该字段，保持 not-applicable。
- `crates/remuda-signal/src/live.rs`：子代理自己的 `tool_call` 额外盖 `parent_tool_call_id`——后台 Agent 在 launch 结果的 `agentId` 命中 `bg_agents`；前台 Agent 在 `SubagentStart` 绑定尚未关闭的 Task launch（`fg_agents`/`fg_open`）；workflow 成员（`agent_type=workflow-subagent`）不走 Task 绑定，由 journal 成员身份连接。

### 分组（web 结构化投影）

`web/src/features/session/assemble.ts`：

- `ToolNode` 记录 `agentId`；新增 `groupSubagentTools`，在 `supersedeStreamed` 之后运行。
- 凡顶层 tool 节点的 native agent 命中成员（`workflow.member.nativeAgentId` → 挂载的 Workflow 工具行）或 Task 父行（`parentToolCallId`），就从顶层移除，挂到父行的 `subagents`（`workflow-member`）或 `subagents`（`agent`）桶里。主会话只保留主代理回合 + 父行；无证据匹配的节点留在顶层，绝不猜测父行。
- 父行显示紧凑 live 摘要「n tool calls · last tool」，可就地展开子代理自己的工具行；搜索（`transcriptSearch.ts`）和定位（`locateNode`）会下钻这些子行。

### 钻取（按需、有限、不加假实例）

子会话完整记录只存在于 sidechain 文件（主 transcript mapper 按文档化不变量丢弃 `isSidechain:true`），不进 live 流：

- `crates/remuda-journal/src/subagent.rs`：一次性读取 `<session>/subagents/workflows/wf_*/agent-<id>.jsonl`（及非 workflow 的 `subagents/agent-<id>.jsonl`），8 MiB 上限，agent id 字符白名单防穿越，经**同一个** `map_claude_line` 管线映射，同时折出 prompt/model/tokens/calls/起止时间/最终文本。
- Node RPC `subagent.transcript`（`crates/remuda-node/src/subagent.rs`，挂在 `server.rs` 与 ssh-stdio `hubnode_codec.rs` 两张分发表）；Hub 路由 `GET /v1/instances/{id}/subagents/{agentId}/transcript`（`instances.rs`/`http.rs`，openapi 已登记）。无 transcript 返回 `available:false`（UI 显示「启动中」），不是 404。
- Web：`web/src/features/session/subagent/`（`subagentApi.ts` 仿 `filesApi` 的无缓存按需读取；`SubagentView.tsx` 路由 `/s/:instanceId/agents/:agentId`，复用 `assembleTranscript`）；成员行（`WorkflowTimelineCard.AgentRow`、`WorkflowTree.MemberRow`）与 TaskTrack 的 Task 行都改为该路由钻取，删除了只在 mock 出现的 `childInstanceId` 死路。

## 2.1.221/2.1.273 sidechain 字段核对

- 2.1.273 实测（本会话自己的 `agent-<id>.jsonl`）：sidechain 记录带顶层 `agentId` + `isSidechain:true`，无 `parentToolUseId`。
- 2.1.221 录制（`crates/remuda-journal/tests/fixtures/workflow/runs-221/` 与 `claude-workflow-221.jsonl`）：同样的顶层 `agent_id` hook 载荷；journal reader 单测直接跑这些真实文件。
- 旧版主 transcript 里嵌入的 sidechain 记录可能只有 `parentUuid`（`claude-transcript.jsonl` 里的孤例）：这类没有 agent id 的节点按「无证据不猜测」留在顶层。

## 截图

- 1440：卡片中成员行下的 live 工具折叠；钻取页（prompt、model/tokens、工具行、最终文本）。
- 390：同一两处的窄屏布局。
