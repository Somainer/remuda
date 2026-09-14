# 工作台 C1 实跑证据：状态词汇、通知接口、持久错误、可访问播报

- 日期：2026-09-14
- 分支：`wt/ux-c1/status-vocabulary-and-notify`，基线 `origin/main` = `3da158d`
- 对应需求：[workbench-ux-exploration.md](../workbench-ux-exploration.md) §5 P0-3；
  执行计划 [workbench-ux-plan.md](../workbench-ux-plan.md) §2 通知契约、§3 字段映射、§4 风险 4
- 数据来源：单元测试用合成 fixture；e2e 用 `crates/remuda-hub/examples/hub_e2e.rs`
  的 fake node + fake-harness。**全程没有真实模型调用。**

## 0 结论

四件事落地：P0-3 八行状态词汇成为一个纯投影（`lib/commandStatus.ts`）、
通知契约冻结（`lib/notify.ts`）、2.4 秒 toast 换成「短暂确认 + 常驻错误区」、
流式正文与播报区彻底分开。**没有新增状态机**：八行全部由已有字段决定，
未知永远不渲染成成功，任何一行都不提供重发动作。

## 1 §3 映射表 → 代码事实

`projectCommandStatus()` 的判定顺序（`lib/commandStatus.ts`）。规则 4 特意排在
5–8 之前，这正是「未知不显示为成功」从口号变成事实的地方。

| 文档行 | 投影自 | 词汇 | success |
|---|---|---|---|
| 在队列，尚未发送 | `Command.state==="queued"` 且 `dispatch ∈ {not-dispatched, intent-durable}`；或本地 `LocalBubble.state==="queued"` | 等待发送 | false |
| 已受理 / 会话已创建 | `Command.state ∈ {accepted, settled}`；或 `Instance.lifecycle ∈ {requested, preparing, starting}` | 已受理 / 会话已创建 | false |
| 已发送，等待确认 | `dispatch==="transport-written"` 且 `resolution==="clear"` | 已发送，等待确认 | false |
| 状态待确认 | `resolution ∈ {unknown, reconciling}`；`connectivity ∈ {disconnected, reconciling}`；`lifecycle ∈ {unknown, reconciling}`；`lastError==="node-epoch-changed"`；`Interaction.delivery==="unknown"` | 状态待确认 | false |
| 需要你回答 | `projectInteraction()==="pending"` 且 `answerable` | 需要你回答 | false |
| 回答已提交但原生未清除 | `state ∈ {answer-committed, resolved}` 且无 `native-cleared` | 回答已提交，等待处理 | false |
| 本轮已结束 | `ContentStatus ∈ {complete, interrupted}` **且** `completion-native-turn.state==="supported"` **且** `provision==="native"` | 本轮已结束 | **true** |
| Hub 已删、Node 清理未确认 | `nodePurge !== "purged"` | 会话记录已删除，主机数据待清理 | false |

三条降级是这次实现里最值得记录的部分：

1. **`provision` 不是 native 就降级**。`complete` 只证明*内容流*停了。
   `emulated` / `unknown` 时边界是 Remuda 推断的，诚实答案是「状态待确认」——
   不用 terminal idle，也不用 `CommandState::Settled`。
2. **`native-acknowledged` 仍然只是「已受理」**。命令送达 harness 不等于业务成功。
3. **没有服务端 `commandId` 时只有 `queued` 能声称阶段**。今天 `store.ts:521-527`
   先塞一个 `local_…` id，失败路径把 bubble 置 `"unknown"` 但 id 还在；
   `hasServerCommandId: false` 会让投影退回「状态待确认」，禁止拿它查
   `/v1/commands`，也不自动重发。C2 用 `clientRequestId` + `commandId: Id | null`
   替换这个参数。

**动作集合是封闭的，且故意没有 resend 成员**（`CommandStatusAction`）：
刷新只能重新读取。单测 `no row offers a resend action` 对五行断言了这一点。

### 一个被测试反过来纠正的判断

`state: "unknown"` 的交互我一开始写成「状态待确认」。跑测试才发现
`interactionStatus.ts:39` 明确把 `unknown` 当 pending 处理，§3 的未知清单里
列的也是 `delivery` 不是 `state`。这是有意的：一个可能真的正在阻塞 agent 的请求
如果被藏进「状态待确认」，就没人能解开它了——比显示一个可能过期的表单更糟。
最后改的是测试不是代码（`commandStatus.test.ts`，`an unknown-state interaction
stays answerable…`），并且补了 `answerable: false` 时才降级的断言。

## 2 通知契约（§2，合入即冻结）

```ts
notify({ subject, stage, reason?, actions?, severity: "info" | "blocking", diagnostic?, key? })
```

- `info` → `role="status"` + `aria-live="polite"`，去抖 400 ms，2.4 s 后自动消失。
- `blocking` → 常驻错误区，**只有用户显式关闭才会消失**，后续成功永远盖不住。

两者分属两个列表，所以给 info 限流不可能挤掉一条错误；默认折叠键包含
`severity`，所以同一对象同一阶段的成功也不会替换掉那条错误。

**复制诊断按白名单序列化**（`formatDiagnostic` 遍历固定字段表，不遍历对象自身的
键）：`at / statusKey / reasonCode / httpStatus / instanceId / hostId / commandId /
interactionId`，空值整行省略。`NotifyDiagnostic` 上**故意没有** free-form 字段——
逃生舱口正是 prompt、文件内容、token 混进剪贴板的方式。单测
`cannot be made to leak fields outside the allow-list` 塞进 `prompt` / `token` /
`transcript` / `cwd` 四个字段并断言它们都到不了输出。

### 遗留调用方

`hubStore.toast(text)` 的三处调用（`SpacesPanel.tsx:49`、`store.ts:583,619`）在
我不拥有的文件里，用 Shell 里的桥接继续按 `info` 工作。**代价要说清楚**：
`SpacesPanel.tsx:49` 的 `nodePurge !== "purged"` 分支实质是阻断态，
今天仍然会自动消失。TODO 已留在 `notify.ts` 的 `toastAdapter` 与 `Shell.tsx`
桥接处，注明该由 D 批（拥有 SpacesPanel）与 C2（拥有 store）改成
`severity: "blocking"` + `projectDeletion()`。

## 3 风险 4：流式正文不进播报区

- 播报区只承载 `Notification.text`（subject · stage · reason 一行），去抖后写入。
- `Transcript.tsx:136` 根节点加 `aria-live="off"`（单属性改动），任何祖先都无法
  让它开口。
- 可见的 info 条 `aria-hidden="true"`，避免读屏读两遍。
- 常驻错误区是 `role="region"` + `aria-label="需要处理的问题"`，
  即 P0-3 要求的「可发现的错误区域」：读屏用户靠 landmark 直接跳过去，
  而不是用 `role="alert"` 打断。

`Shell.test.tsx` 的 `debounces a burst into a single announcement` 断言 5 条
通知在一个去抖窗口内只播报最后一条；e2e 的 burst 用例在真实浏览器里轮询
播报区文本，断言不同取值 ≤ 2。

## 4 布局：成功不能盖住错误（一次真实的返工）

第一版 CSS 写的是「有错误时把 info 条 `display: none`」。e2e 立刻挂了，
而且挂得对——那等于用「丢失成功反馈」换「保住错误」。第二版改成固定
96px 偏移，e2e 的 `boundingBox` 重叠断言又挂了：写死的偏移量不随错误区高度变化。
最终两个区域收进同一个 flex 栈（`.stack`），确认在上、错误在下，
**布局上不可能重叠**，偏移量这个魔数也随之消失。

e2e `a blocking error stays visible after a later success` 现在直接比较两个
`boundingBox` 断言不相交，而不是只断言可见。

## 5 已答请求不能二次提交

`canSubmitAnswer()`（`lib/interactionStatus.ts`，与 `projectInteraction` 同文件
以免循环依赖，`commandStatus.ts` 再导出）只有 `pending` 投影才返回 true。
覆盖的不可提交场景：本地提交在途、其它设备已答（并发回答）、
`answer-committed` 未被原生清除、`expired` / `invalidated`、deadline 已过、
主机离线或 `connectivity==="disconnected"`、`answerable: false`。

`native-cleared` 是唯一算清除的 resolution reason；另外四个
（`answered` / `native-cancelled` / `generation-ended` / `timed-out`）都不算，
测试对四个逐一断言。

## 6 测试与检查

| 检查 | 结果 |
|---|---|
| `pnpm --dir web test`（全量单测） | 72 files / 498 tests 通过 |
| `commandStatus.test.ts` | 32 通过（八行 + 未知不成功 + 无 resend + 并发/二次提交） |
| `notify.test.ts` | 22 通过（含 cap 淘汰不带走后来条目的计时器安全） |
| `Shell.test.tsx` | 13 通过（含 live region 卫生与 landmark） |
| `interactionStatus.test.ts` | 增补后全通过 |
| `pnpm --dir web lint` | 我的文件 0 warning |
| `pnpm --dir web exec tsc -b` | 0 error |
| `./scripts/ci/secret-scan.sh` | pass |
| `pnpm --dir web run test:e2e:hub` | 见 §7 |

## 7 e2e 实跑（fake node）

`web/tests/e2e/ux-status.spec.ts`，8 个用例，已加进
`playwright.hub.config.ts` 的 `testMatch`（不加就会被静默跳过）。

fake node 自身无法产生的故障——被拒绝的命令、`nodePurge !== "purged"` 的删除——
用 `page.route` 在 HTTP 边界注入，这样被测的仍然是 Hub 契约，
也不需要改 Rust。断线用 `route.abort` 模拟，慢确认用挂起的响应模拟。

「刷新不新增原生动作」是**可测量**的：`recordNativeActions()` 统计所有打到
`/commands`、`/input`、`/interrupt`、`/interactions/*/answer` 的写请求，
断线刷新后断言增量为 0；被拒绝的命令在 2 秒后断言计数未变。

### 两个必须记录的坑

1. **fake node 宣告 `maxInstances: 8`**，而 hub 配置串行跑所有 spec、共用一个
   Hub。本 spec 的用例跑到一半就撞配额（更早的 spec 也占着槽位），
   失败现象是 `POST /v1/instances` 返回 422 `PLACEMENT_UNSATISFIABLE`，
   在 UI 上就是「创建会话没有跳转」——和 host 过载 flake 的表现一模一样。
   两层处理：`afterEach` 用 `DELETE ?force=1` 回收自己建的会话（`force=1` 是
   `u8`，不是 `force=true`，写错会 409）；`beforeEach` 通过
   `PATCH /v1/hosts/{id}` 把**这个 fake fixture** 的 `maxInstances` 临时抬到 24，
   `afterAll` 还原。抬的是测试夹具的容量，不是被测的容量断言。
2. **fake node 每次 `instance.create` 都会挂一个待处理 approval**，使
   `activity: waiting-interaction`、composer 被禁用。要发送的用例必须先用
   `clearPendingApprovals()`（`POST /v1/interactions/{id}/answer`）清掉它，
   否则 `composer.fill` 会在 disabled textarea 上超时 90 s。
   封装成 `createReadySession()`。

### 与已知基线 flake 的关系

`providers-discovery` 在本机同样失败，与本分支无关：它在未修改的
`origin/main` 上也失败，属于共享 devbox 的已知现象（load、端口竞争）。
本批次不触碰 provider 相关代码。

## 8 交付文件

| 文件 | 说明 |
|---|---|
| `web/src/lib/commandStatus.ts` | 八行投影 + 词汇常量 + `projectDeletion` |
| `web/src/lib/commandStatus.test.ts` | 32 用例 |
| `web/src/lib/notify.ts` | 通知契约、store、`formatDiagnostic`、`toastAdapter` |
| `web/src/lib/notify.test.ts` | 22 用例 |
| `web/src/lib/interactionStatus.ts` | `canSubmitAnswer` / `answerPendingNative` / `nativeCleared` |
| `web/src/app/Shell.tsx` | `ShellNotify` 两个区域、遗留 toast 桥接、`__notifyLab` 测试缝 |
| `web/src/app/shellNotify.module.css` | 单一 flex 栈布局 |
| `web/src/app/Shell.test.tsx` | 13 用例（含 live region 卫生与 landmark） |
| `web/src/features/session/Transcript.tsx` | 根节点 `aria-live="off"`（单属性） |
| `web/tests/e2e/ux-status.spec.ts` | 8 个 e2e |
| `web/playwright.hub.config.ts` | `testMatch` 加入 `ux-status` |

## 9 留给后续批次

1. **C2**：`store.ts` 拆 `clientRequestId` / `commandId`，替换
   `hasServerCommandId` 参数；`store.ts:619` 恢复失败改成 blocking。
2. **D 批**：`SpacesPanel.tsx:49` 延迟清理改成 `severity: "blocking"` +
   `projectDeletion()` + 诊断字段。
3. **列表 / Tab / 详情**：统一消费 `COMMAND_STATUS_LABEL` 与
   `PROMPT_MODE_LABEL`（引导 / 排队）、`EMULATED_LABEL`，目前只导出未接入。
4. **E 批**：`aria-live="off"` 已落，Transcript 重构时不要丢。
