# 工作台 C2：commandId 与本地气泡的可靠对账（2026-09-15）

**分支：** `wt/ux-c2/client-request-id`（起点 `eefcbba`）
**需求：** workbench UX 探索 §5 P0-3 + plan §3「尚无可靠 commandId 的位置」
**数据来源：** 单元/集成测试（fake-harness）、Playwright hub e2e（fake node）
**全程无真实模型调用。**

## 0 结论

结构化 composer 发出的 prompt 之前会显示两次：Remuda 自己的本地气泡
（POST 前乐观渲染）和 hook/transcript 观测到来后组装出的新用户节点。本批次
用**关联而不是压制事件**修复：

1. **Node**：命令投递的 prompt 写入 PTY 时注册
   `(commandId, nodeId, text, t0)`；随后的 `UserPromptSubmit` hook 事件与
   transcript `user` 记录按精确文本（再退化为空白归一化文本）匹配，匹配上限
   10 分钟、FIFO、不同文本绝不匹配。hook 事件在 `relatedIds.commandId` 带上
   命令 id；transcript 记录的 `MessagePayload.commandId` 带上同一 id，并
   **join 到 Node 已合成的排队消息节点**（revision 3 replace）。
2. **协议**：`MessagePayload.command_id: Option<CommandId>` 与 `origin` 一样
   是加性字段；原生键入的 prompt 不序列化该字段，客户端读作「无命令」。
3. **Web**：本地气泡拆成 `clientRequestId`（POST 前生成，`local_` 前缀）与
   `commandId`（仅服务器响应赋值；5xx/离线保持 `null`）。journal 用户节点
   携带相同 commandId 时气泡原地升级为「已接受 → 已结算」，不再渲染第二个
   节点；无 commandId 的节点（终端原生键入）照常独立渲染。
4. **词汇**：气泡与 SessionPage 状态标签统一消费 C1 的
   `projectCommandStatus` —— queued 投影「等待发送」，null commandId 的
   unknown 投影「状态待确认」，绝不显示成功；失败路径不自动重发。

## 1 before / after 字段表

### Web `LocalBubble`（`web/src/lib/store.ts`）

| 字段 | before | after |
| --- | --- | --- |
| `id` | 本地生成 id，但在 POST 前就被当成 commandId 使用 | 删除；拆成下面两个 |
| `clientRequestId` | — | POST 前生成的本地请求身份（`local_…`），**永不**作为 commandId 查 `/v1/commands` |
| `commandId` | POST 前 = 本地 id；失败路径残留本地 id | `Id \| null`；**仅** `instanceSend` 响应赋值；5xx/Node 离线保持 `null` |
| `state` | queued/accepted/…/unknown | 不变；unknown + `commandId === null` 经投影为「状态待确认」 |

### 协议 `MessagePayload`（`remuda-protocol` §5.2）

| 字段 | before | after |
| --- | --- | --- |
| `origin: Option<MessageOrigin>` | 加性，缺省视为 human | 不变 |
| `command_id: Option<CommandId>` | 不存在 | 新增，加性（C2）。Node 合成的排队/投递消息、join 的 transcript 用户记录携带；原生键入缺省（`null`） |

### hook `NativeLifecycle.relatedIds`（`UserPromptSubmit`）

| key | before | after |
| --- | --- | --- |
| `promptId` | fake-harness 的原生 promptId | 不变 |
| `prompt` | 不转发（payload 里有但 mapper 丢弃） | 新增：Node 用它做文本匹配 |
| `commandId` | — | 匹配上命令时新增；原生键入缺省 |

## 2 关联规则（Node `prompt_correlation.rs`）

- 每个待匹配条目可按通道各匹配**一次**：hook turn 事件与 transcript
  user 记录是同一次提交的两条独立证据，必须带同一个 commandId；两条都到
  后条目出队。
- 先精确文本、再 `split_whitespace().join(" ")` 归一化；同文本多命令 FIFO。
- 窗口 [`PROMPT_CORRELATION_WINDOW = 600s`]：Claude 在 prompt 进入**原生**
  队列时触发 `UserPromptSubmit`，steer/queue 场景可能晚一整个 turn，因此窗口
  取大；窗口外条目按过期清理。
- 取消/未投递/发送失败立即 `cancel(command_id)`，避免后续同文本被误归属。
- 关联状态**仅在内存**：重启后 transcript pump 重放历史时没有待匹配条目，
  旧记录正确保持 `commandId: null`；Web 的乐观气泡同样是内存态，生命周期一致。

## 3 测试

### Rust

- `remuda-node` 单元：`prompt_correlation.rs` 5 例（hook+transcript 共命令后
  出队、精确文本优先于归一化候选、同文本 FIFO、不同文本绝不匹配、无待匹
  配即原生）。
- **fake-harness 集成** `crates/remuda-node/tests/prompt_correlation.rs`（3
  例，真 PTY + 真 `fake-harness` + 生产 mapper 重放）：
  1. 命令投递的 prompt：hook+transcript 重放后**恰好一个**用户节点，带同一
     commandId，transcript 记录 join 排队节点 id；
  2. PTY 原生键入的 prompt：hook 与 transcript 均无 commandId；
  3. 相同文本各一条（一条命令、一条原生）：两个不同节点，归属正确。
- `remuda-signal`：`map_event` 转发 `UserPromptSubmit.prompt`（既有测试全绿）。
- `cargo fmt --check`、workspace 全量 `cargo check --all-targets`、
  `clippy --all-targets -D warnings` 干净。

### Web 单元（vitest，610 passed / 79 files）

- `store.localBubble.test.ts`（新，5 例）：POST 前只有本地 id 且投影
  「等待发送」；仅服务器响应赋值 commandId；5xx 保持 null 且投影
  「状态待确认」；并发发送 clientRequestId 互异；journal 节点带同一
  commandId 时结算（相同文本也能区分两条），重放不产生重复。
  「等待发送」；仅服务器响应赋值 commandId；5xx 保持 null 且投影
  「状态待确认」；并发发送 clientRequestId 互异；journal 节点带同一
  commandId 时结算（相同文本也能区分两条），重放不产生重复。
- `store.follow.test.ts`：follow/regroup 之外新增 commandId 对账 1 例（
  同文本两条气泡只被各自的 commandId 节点结算，无 id 的原生节点不误结算）。
- `assemble.test.ts`：新增 C2 分组 5 例（join 后隐藏乐观气泡、无匹配不
  隐藏、有服务器 id 时不靠文本去重、queued 期间命令节点到达仍只一条、
  旧文本规则仅用于无服务器 id 的气泡）。

### Hub e2e（fake node + Playwright，4/4 通过）

- fake node（`hub_e2e.rs`）扩展：
  - `instance.create` / `instance.send` 的用户 journal 节点改为全形状并携带
    params 里的 `commandId`；
  - 新增 `tty.attach` 与 `tty.write`：每实例一个**稳定的** stream id（dev
    StrictMode 会 attach 两次，重挂的 socket 必须沿用同一 stream）；浏览器在
    附加终端里的键入在 CR 时生成 **无 commandId** 的用户节点 + 一行 echo，
    用于验证原生路径。
- `web/tests/e2e/ux-command-id.hub.spec.ts`（新，4 例，本地 45s 全绿）：
  1. composer 提交：hook+transcript 往返后恰好一个用户气泡，且带
     `data-command-id^=cmd_`；
  2. 附加终端原生键入：恰好一个无 `data-command-id` 的用户气泡；
  3. POST 延迟：等待发送期间一条气泡、只一次 POST，响应后原地合并不重复，
     GET catchup 不被路由拦截；
  4. POST 502：气泡与标签显示「状态待确认」，仅一次 POST 尝试、1.5s 内无
     重发；reload 不产生额外原生动作、乐观气泡消失，直接查
     `/journal` 确认失败的 POST 从未被 Node 记录。

> 文件按协调器约定命名为 `*.hub.spec.ts`，未改
> `playwright.hub.config.ts`（待 wt/ux-g2 的 `\.hub\.spec\.ts$` 匹配落地）。

## 4 留给后续

- `hasServerCommandId: false → 状态待确认` 的 C1 形参被 `clientRequestId`
  取代是自然演进（类型不变，语义更明确），后续批次可重命名。
- e2e fake node 的 tty 路径是最小实现（每实例单流、行缓冲），只服务 C2
  的键入断言；终端交互的完整模拟仍由 `remuda dev` 外部 Node 路径覆盖。
