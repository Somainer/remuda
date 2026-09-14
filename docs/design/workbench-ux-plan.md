# 工作台体验改进 · 执行计划（A–G 派工版）

- 日期：2026-09-14
- 状态：执行计划（配合 [workbench-ux-exploration.md](./workbench-ux-exploration.md)，该文档的需求与验收标准为准）
- 基线：`origin/main` = `3da158d`；事实核对结论：探索文档 §5 引用的全部代码事实在该基线上成立（0 过期、0 错误）

## 0 协调结论

1. **顺序**：A、C1、G1 立刻并行（文件零交集）；A 合入后放 D；`r-effort-slider` 合入后放 B、F；`c-hookgap` 合入后放 C2；`r-p3-stream` 合入后放 E。任一时刻 `Shell.tsx` 只有 C 可写、`SpacesPanel.tsx` 只有 D 可写、`SessionPage.tsx` 只有 G2 可写。
2. **与 D-028 的关系**：P0-3 的状态词汇不新造状态机，直接投影 D-028 已合入的字段（见 §3 映射表）；composer 三态（发送 / 排队 / 打断）、`launchedBy`、`node-epoch-changed` 的展示由 P2 web 已落，C 只统一词汇与持久错误。
3. **不做**：独立任务/看板实体、调度引擎、成员权限模型、跨设备 Space 同步、跨主机原生会话迁移；文件结果视图先契约后代码（G1 只产文档）。
4. **每批一个 worker、一条分支、过统一 gate**（远端 gate 含 Playwright e2e），证据文档用合成 fixture。


基线 `origin/main` = `3da158d`。已复核报告结论并补充三点代码事实：`SessionList.tsx:66` 已用 `useSearchParams`（URL 已是筛选事实来源）、`Shell.tsx:54` 已有 `input/textarea/select/[contenteditable]/.xterm` 键盘护栏、`components/Modal.tsx` 已有 `role="dialog"`+`aria-modal` 但无焦点圈定/Escape；`styles/ui.module.css` 被 18 个文件共用，是并行写入热点。

## 0. 全局约定

- 一批次 = 一 worker = 一分支 `wt/ux-<letter>/<slug>`，经统一 gate 合入：`./scripts/ci/gate.sh --web-only` + `pnpm --dir web lint` + `pnpm --dir web run gen:api` 后 `git diff --exit-code -- web/src/lib/api.generated.ts` + `pnpm --dir web run test:e2e:hub`；触碰 Rust 时追加 `./scripts/ci/gate.sh`。
- **禁改清单（在飞行分支合入前任何 worker 不得写）**：`features/session/EffortSlider.tsx`、`features/session/effort.ts`、`features/session/session.module.css`、`features/session/Composer.tsx`、`pages/NewSessionPage.tsx`、`tests/e2e/{composer-effort,new-session}.spec.ts`（属 `r-effort-slider`）；`lib/store.ts`、`lib/api.generated.ts`、`types/generated.ts`（属 `c-hookgap` / `r-launchopts` / `r-p3-stream`）。
- 新样式一律落新 CSS module，**不得编辑 `styles/ui.module.css`**（F 独占它与 `styles/tokens.css`）。
- e2e 一律用 fake node + `fake-harness`（`crates/remuda-testing/src/bin/fake-harness.rs`，见 `docs/design/testing-fake-harness.md`），禁止真实模型调用。

## 1. 批次矩阵

| 批 | 独占文件（owned） | 新增测试（unit / hub-live e2e） | 证据文档 | 大小 | 何时开工 |
|---|---|---|---|---|---|
| A | `features/session/SessionList.tsx`、`SessionList.module.css`、`features/session/sessionFilters.ts`(新)、`components/Modal.tsx`、`components/Sheet.tsx`(新)、`components/useFocusTrap.ts`(新)、`components/overlay.module.css`(新)、`lib/keyboardScope.ts`(新) | `sessionFilters.test.ts`（URL↔条件往返、切 Space 丢弃 host/workspace 保留 q/status）、`useFocusTrap.test.tsx`；e2e `ux-filters.spec.ts`（`aria-expanded`、零结果+清除筛选、同名跨主机 workspace、深链/刷新/返回一致） | `docs/design/evidence/workbench-a-filters-1.md` | M | **现在并行** |
| B | `pages/NewSessionPage.tsx`、`NewSessionPage.module.css`、`lib/newSessionDraft.ts`(新)、`NewSessionPage.test.tsx`、`tests/e2e/new-session.spec.ts` | `newSessionDraft.test.ts`（身份 + `hostId+workspaceId` 隔离、无身份仅内存）、`NewSessionPage.test.tsx`（来源返回、Escape 保留草稿、首层无实现词汇）；e2e 复用 `new-session.spec.ts`（纯键盘完成、未知 ACK 不重复创建、390px 软键盘不遮挡主操作） | `docs/design/evidence/workbench-b-newsession-1.md` | M | A 合入 **且** `r-effort-slider` 合入后 |
| C | `lib/commandStatus.ts`(新)、`lib/notify.ts`(新)、`app/Shell.tsx`、`app/shellNotify.module.css`(新)、`lib/interactionStatus.ts` | `commandStatus.test.ts`（§3 八行投影）、`notify.test.ts`（阻断错误不被后续成功 toast 覆盖）、`interactionStatus.test.ts` 增补（已答未清除/未知/并发回答）；e2e `ux-status.spec.ts`（断线、慢确认、拒绝、并发回答、延迟清理；刷新不新增原生动作） | `docs/design/evidence/workbench-c-status-1.md` | M | **现在并行**（C1 不碰 `store.ts`）；C2（`clientRequestId` 落 `store.ts`）待 `c-hookgap` 合入 |
| D | `features/search/`(新目录：`QuickFind.tsx`/`quickFind.ts`/`quickfind.module.css`)、`features/spaces/SpacesPanel.tsx` | `quickFind.ts` 单测（标题/空间/主机优先级、ID 辅助命中、同名由空间+主机区分）；e2e `ux-quickfind.spec.ts`（快捷键打开→方向键→Enter 跳对实例、Escape 还原原焦点且不改变 Space 选择、离线只搜缓存文案、不截获 xterm 输入） | `docs/design/evidence/workbench-d-quickfind-1.md` | M | C1 合入后 |
| E | `features/session/Transcript.tsx`、`assemble.ts`、`TaskTrack.tsx`、`features/session/transcriptSearch.ts`(新)、`tests/e2e/session-virtual.spec.ts` | `transcriptSearch.test.ts` + `assemble.test.ts`（稳定内容身份、修订/追加不跳错节点、TaskTrack 长 prompt 截断）；e2e 2,000-event fixture 命中窗口外已加载节点、退出恢复阅读位置与 follow 态、工具失败可见、搜索不改 journal/原生 session | `docs/design/evidence/workbench-e-transcript-1.md` | M | A + C1 合入 **且** `r-p3-stream` 合入后 |
| F | `pages/SettingsPage.tsx`、`settings.module.css`、`styles/tokens.css`、`styles/ui.module.css`、`features/spaces/spaces.module.css`、`README.md` | `SettingsPage.test.tsx`（锚点分组、深链/返回、保存中/已保存/失败三态、失败回退旧有效值、不新增默认权限）；e2e `ux-settings.spec.ts` + `mobile-qa.spec.ts` 增补（双主题、200% 缩放、390/768/1440、44px 命中区域含 Space chips 与 ActionSheet） | `docs/design/evidence/workbench-f-settings-1.md` | M | A + C1 合入 **且** `r-effort-slider` 合入后 |
| G | `docs/design/files-view-contract.md`(新)；**第一期不写 UI 代码** | 契约期无代码测试；结论为“可实施”时才开 G2，届时接管 `pages/SessionPage.tsx` files 分支与 `session.module.css` files 段 | `docs/design/evidence/workbench-g-files-contract-1.md` | S（勘察）/ L（实现） | **现在并行** |

并行窗口：**今天可同时跑 A、C1、G**（三者文件零交集）。A 合入后放 D；`r-effort-slider` 合入后放 B、F；`c-hookgap` 合入后放 C2；`r-p3-stream` 合入后放 E。任一时刻 `Shell.tsx` 只有 C 可写、`SpacesPanel.tsx` 只有 D 可写、`SessionPage.tsx` 只有 G2 可写。

## 2. 两个必须先冻结的契约（A 与 C 的前置产物，合入即冻结）

- **A：overlay 契约**（`components/Sheet.tsx` + `useFocusTrap.ts`）——`{ open, onClose, labelledBy, initialFocusRef, returnFocusRef, variant: "popover" | "sheet" }`，保证 name / `aria-modal` / 初始焦点 / Tab 圈定 / Escape / 触发器焦点返回。B、D、F 直接消费，不得各自再写遮罩。
- **C：通知接口**（`lib/notify.ts`）——`notify({ subject, stage, reason?, actions?, severity: "info" | "blocking" })`；`info` 走 `role="status"` 短暂提示，`blocking` 进常驻错误区且不被后续成功覆盖。A/B 合入前按此签名调用，C1 先落接口与实现，A/B 只依赖类型。

## 3. P0-3 表格 → 现有字段映射（D-028 词汇，`commandStatus.ts` 按此实现，禁止新造状态机）

| 文档行 | 投影自的现有字段 |
|---|---|
| 在队列，尚未发送 | `Command.state==="queued"` 且 `Command.dispatch ∈ {not-dispatched, intent-durable}`（`web/src/types/command.ts`）；本地态 `LocalBubble.state==="queued"`（`lib/store.ts:527`） |
| 已受理 / 会话已创建 | `Command.state==="accepted"`；创建看 `Instance.lifecycle ∈ {requested, preparing, starting}`；**不得**据此显示任务成功 |
| 已发送，等待确认 | `Command.dispatch==="transport-written"` 且 `Command.resolution==="clear"`；交互侧 `Interaction.delivery==="written"`（`types/interaction.ts:85`） |
| 状态待确认 | `Command.resolution ∈ {unknown, reconciling}`；`Instance.connectivity ∈ {disconnected, reconciling}`；`Instance.lifecycle ∈ {unknown, reconciling}`；`Instance.lastError==="node-epoch-changed"`（已被 `SessionPage.tsx:85`、`SessionList.tsx:18` 读取）；`Interaction.delivery==="unknown"` |
| 需要你回答 / 需要批准 | `projectInteraction()==="pending"`（`lib/interactionStatus.ts:33`）+ `InteractionKind ∈ {approval, question, plan-review, elicitation}` |
| 回答已提交但原生未清除 | `Interaction.state==="answer-committed"` 且尚无 `InteractionResolutionReason==="native-cleared"`；UI 取 `projectInteraction` 的 `settled` / `superseded` |
| 本轮已结束 | `ContentStatus ∈ {complete, interrupted}`（`assemble.ts:90` 已按此排序）**且** `capabilities.capabilities.completionNativeTurn.state==="supported"` 且 `provision==="native"`；`provision ∈ {emulated, unknown}` 时降级为“状态待确认”，禁止用 terminal idle 或 `CommandState::Settled` 推断业务成功 |
| Hub 已删、Node 清理未确认 | `DELETE /v1/instances/{id}` 响应的 `nodePurge !== "purged"`（`SpacesPanel.tsx:49` 已有该分支，C 把短 toast 换成常驻提示） |

配套词汇：`PromptMode ∈ {new-turn, steer, queue}` → `LocalBubble.promptMode`（`store.ts:529`），列表/Tab/详情统一显示“引导 / 排队”；`CapabilityProvision ∈ {native, emulated, unknown}` → `Instance.capabilities.capabilities[name].provision`，`emulated` 必须在 UI 标注；`launchedBy` → `Instance.launchedBy`（缺失时用 Hub 的 `derive_launched_by`），只做来源标注、绝不当能力等级。

**“尚无可靠 commandId”今天的位置**：`lib/store.ts:521-527` 先 `const localId = id("local_")` 并直接 `commandId: localId`；成功路径 `:536-538` 才用服务端 `result.command.commandId` 覆盖；失败路径 `:545-547` 把 bubble 置 `"unknown"` 但 `commandId` 仍是 `local_…`。C2 必须拆成 `clientRequestId`（本地）与 `commandId?: Id | null`（仅服务端赋值），为 `null` 时一律显示“状态待确认”，禁止拿它查 `/v1/commands`，禁止自动重发。

## 4. 最高风险项与必须实现的缓解

1. **焦点圈定 × xterm（A，连累 B/D/E）**：焦点陷阱一旦全局挂 keydown 会吞掉终端原生 Escape/Tab。缓解：`useFocusTrap` 仅在 overlay 打开时把 keydown 挂在 **overlay 容器**上，且首行 `if (e.target.closest('.xterm')) return;`；把 `Shell.tsx:54` 的 selector 抽成 `lib/keyboardScope.ts` 常量由 A 导出供 D 复用；e2e 必须断言 attach 终端后按 Escape 不关闭任何面板，且 ESC 字节确实到达 `fake-harness`。
2. **URL 事实来源 × Space store（A、D）**：`useSearchParams` 与 active space 双写易成回环，且 `q` 每次按键都进历史。缓解：单向推导——切 Space 时只写一次 `setParams(next, { replace: true })` 丢弃 host/workspace 条件、保留 `q`/`status` 并在 UI 显示；搜索输入用 `replace: true`，显式条件变更用 push；禁止在 effect 中由 params 反写 store。
3. **无可靠身份的草稿隔离（B）**：`lib/drafts.ts` 只有 `runtime.draft.<instanceId>`，新建页没有 instanceId，跨账号/跨主机串用风险直接落在 localStorage。缓解：使用独立版本键 `runtime.draft.new.v1.<authSubject>.<hostId>.<workspaceId>`；**拿不到可靠 auth subject 时只用 `useRef` 内存、不落盘**；只存正文与非敏感选项，不存认证值/临时文件内容/附件二进制；不改 Composer 在用的 `runtime.draft.` 命名空间，回退时不破坏原数据。
4. **live region × 流式正文（C，连累 E）**：把 Transcript 放进 `aria-live` 会在流式输出时形成高频播报。缓解：`role="status"` 容器只承载 `notify()` 短文本并 `aria-live="polite"` + 去抖；Transcript 根节点显式 `aria-live="off"`；阻断错误用可发现的常驻错误区而非 `role="alert"` 轰炸；e2e 断言流式期间 live region 文本变更次数有上限。
5. **文件视图契约（G）**：工具输出中的路径与当前 worktree diff 都不能证明“本次会话产生了改动”。缓解：G1 只交契约文档，必须写明数据来源、基准 revision、路径规范化与注册目录边界（`crates/remuda-node/src/workspace.rs`）、内容 digest、所属 instance/run、截断标记与不可用态（无变化 / 尚未采集 / 不支持 / 权限不足 / 离线）；若无可靠来源，就以准确的不可用说明收尾，**不得**用虚构数据验收，也不得把占位换成“本次改动”；只有工作树状态时标题必须是“工作区当前变更”；公开截图只用合成 fixture。

次级风险（记录，不阻塞）：`SessionPage.tsx:178-186` 的 files 入口在 `@media (max-width: 767px)`（`session.module.css:1789`）下被 `.deskOnly` 隐藏，G2 之前手机无等价入口；F 不要顺手补这个入口，以免与 G2 争抢文件。