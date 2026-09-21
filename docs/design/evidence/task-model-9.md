# task-model 任务 9（t-annotations）证据：批注为 composer 草稿（两级载体，随发送投递）

- 日期：2026-09-21
- 分支：`wt/c-annotations/b-annotations-md`
- 计划：`briefs/plans/task-model.md`（(C) 任务 9，B.7、D7、M2 第 3 波）
- 设计依据：**D-050 §7**、[task-model.md §1.5 / §8](../task-model.md)、[ui-spec.md §1.5 / §2.9](../ui-spec.md)
- 基线：`origin/main @ f782e922`（任务 5 的 `TaskDetailPanel` 已带 `data-anchor-surface` 锚点面标记）

> 合规：本文只用泛称与公开项目语境，不含任何内部产品名、内部文档引述、内部主机名/用户名/主目录；两张截图全部是 **Remuda 自身渲染**（1440 与 390 宽，假 Node 标签 `e2e-fake-node` 为 e2e 夹具专用占位）。

## 目标一句话

用结构化批注替代「复制粘贴给 agent + 描述问题」：批注钉在**任务卡片**或选中文本的 **① 消息内锚点**上，随该会话**下一次发送**作为结构化前缀投递，发送后清零。**零 wire 字段、零新表、零 schema 改动**；agent 提示词里不注入任何看板协议文本（D7 / design §8）。

## 交付物（严格 = 任务 9 归属文件）

| 文件 | 内容 |
|---|---|
| `web/src/features/tasks/annotations.ts` | 新建。设备本地批注草稿模型（仿 `web/src/lib/drafts.ts`，localStorage 键 `runtime.annotation.<instanceId>`）：`AnnotationDraft {carrier:"card"|"anchor", anchor?:{surface,messageId,quote}, body}`、卡片/锚点草稿创建、`normalizeQuote`（空白折叠 + 160 字截断）、① 编号（creation order，卡片不占号）、`serializeAnnotationPrefix` / `withAnnotationPrefix`（结构化前缀）、`composeWithAnnotations`（发送点折叠）、按实例存取/清除/外部订阅、`readAnnotationSelection`（DOM 选区捕获） |
| `web/src/features/tasks/annotations.test.ts` | 新建。19 条 vitest：锚点创建、徽标计数、发送序列化（两载体、空发送、空白忽略、无协议词）、localStorage 持久/隔离/清除/损坏 JSON 容错、订阅通知、DOM 选区（transcript/task-detail、read-only、无归属会话） |
| `web/src/features/tasks/AnnotationPanel.tsx` | 新建。`AnnotationProvider` + `AnnotationBadge`（「本次发送带 N 条批注」）+ `AnnotationPanel`（**卡片 / 标记 两 tab**）+ `useSessionTask`（`GET /v1/tasks` 只读解析会话→任务，归档判定）。无 Provider 时有 localStorage 降级上下文（SessionPage 单测可独立挂载） |
| `web/src/features/tasks/AnnotationPanel.test.tsx` | 新建。4 条组件测试：徽标出现/计数、卡片增删、标记 tab 的 ① 编号、归档只读 |
| `web/src/features/tasks/annotation.module.css` | 新建。徽标、两 tab 面板、选区气泡的样式（复用设计 token，未碰 `session.module.css`）。**休止态徽标行用零高度 float layer 锚在 dock 顶部**（绝对定位浮在 composer 上方），不占文档流，故 session body 的视口占比测量（m-chrome ≥0.60）与改动前一致；只有打开的面板进流 |
| `web/src/features/session/AnnotationCapture.tsx` | 新建。选区→锚点 affordance：在 `data-anchor-surface` 内选中正文弹出「批注」触发钮与输入气泡，存为归属会话（`data-annotation-instance`）的 ① 草稿；read-only 面不弹；在 `/board` 时保存后给「去工作台查看」入口 |
| `web/src/features/session/Transcript.tsx` | **纯增量标记**：消息 `<section>` 加 `data-anchor-surface="transcript"` + `data-anchor-message={node.id}`，使正文成为 ① 锚点面；未改渲染结构、未碰 ToolCard |
| `web/src/features/tasks/TaskDetailPanel.tsx` | mandate 正文面补 `data-annotation-instance`（主会话 id）与 `data-annotation-readonly`（归档任务=1） |
| `web/src/app/Shell.tsx` | Shell 包一层 `AnnotationProvider` 并挂一个 `AnnotationCapture`（唯一两处 Shell 增量） |
| `web/src/pages/SessionPage.tsx` | 会话页：页面根挂 `data-annotation-instance/readonly`（tty/events 段不挂，即「终端段不提供批注」）；composer dock 上方挂徽标 + ＋加批注 + 面板；`onSend` 经 `composeWithAnnotations` 折叠前缀、**发送成功后才清草稿**（失败保留为待确认）；`onHold` 同样折叠（排队行按序携带） |
| `web/tests/e2e/task-model-annotations.hub.spec.ts` | 新建。hub e2e（无 gated 触发，默认全量跑）：徽标出现、两载体、发送带前缀、发送后清零、终端段无锚点面；证据截图仅在 `REMUDA_EVIDENCE=1` 写 |

**明确未触碰**：`Composer.tsx` 的三态/队列结构（D-042）、`ToolCard.tsx`、`session.module.css`、`playwright.hub.config.ts`、`sw.src.js`、任何 `crates/**`（零后端改动）。

## 验收逐条对照（计划任务 9 的三项）

### 1. 两级载体：卡片级 + 消息内锚点 ①

- **卡片级**：composer dock 的「＋ 加批注」开面板卡片 tab，输入正文即存一条挂在当前会话任务上的卡片批注（快照 `taskId/taskTitle`，只用于前缀展示，不回写）。
- **消息内锚点**：在 **任务 5 的 TaskDetailPanel mandate 正文**或 **transcript 消息正文**内选中文本 → 浮出「批注」触发钮 → 输入气泡显示选中引文 → 保存为 ① 锚点草稿（`surface: "task-detail" | "transcript"`，transcript 带 `messageId`，引文空白折叠且截 160 字）。标记 tab 以 ①⑳ 圈码（超出 ⑳ 用 `(n)`）按创建顺序编号。
- 单元测试：`createCardDraft` / `createAnchorDraft`、圈码编号只由锚点草稿占用、`readAnnotationSelection` 对两种面与归属/只读属性的解析；组件测试覆盖气泡保存后标记 tab 显示 ①。
- e2e：真实选区（JS Range 选中消息正文元素，避开 You/assistant 角色抬头）→ capture 气泡 → 保存，徽标变 2，标记 tab 列出 ① 行。

### 2. 徽标 + 发送时结构化前缀，无 wire/表/协议注入

- composer 上方徽标「**批注 N 本次发送带 N 条批注**」（`data-testid=annotation-badge`，计数 `annotation-badge-count`），点击开关两 tab 面板。
- 发送时 `composeWithAnnotations(instanceId, prompt)` 在 SessionPage 的 `onSend` 把草稿序列化为前缀：

  ```
  【批注 ×2】
  1. 卡片批注（SE-02 flake）：rerun with the seed fixed
  2. 文本标记 · 任务详情「mandate says ship friday」：that date slipped

  <原始 prompt>
  ```

  - 无草稿时 `withAnnotationPrefix` 返回值与 prompt **逐字节一致**；只有附件（空 prompt）的发送也能带前缀；空白正文草稿不投递。
  - 折叠发生在既有 `hubStore.send(instanceId, text, …)` 的 **text 参数**上——没有新 wire 字段、没有新表、没有 schema 变化（前后端契约零改动，`crates/**` 本任务一字未改）。
  - 前缀是**反馈措辞**，不含任何看板/协议指令：单元测试断言前缀不匹配 `protocol|board_column|set_task_state`；e2e 断言发送文本不含 `set_task_state|boardColumn|board_column`。
  - 排队（Remuda 代持的 hold）路径同样在 hold 时折叠，保证插队/队首刷新顺序内携带；native 队列经同一 `onSend`/`onHold` 入口。
- e2e：假 harness 把 prompt **逐字回显**（`echo: …`），回显消息含 `【批注 ×2】`、两条批注正文、`文本标记 · 会话记录` 与原 prompt——证明前缀真实到达模型，且走的是原 prompt 通道。

### 3. 草稿设备本地、发送后清零；终端段无批注；归档任务只读预览

- **设备本地**：草稿只在 localStorage（`runtime.annotation.<instanceId>`），与 `lib/drafts.ts` 同形；按实例隔离；损坏 JSON 安全返回空。
- **发送后清零**：`onSend` 仅在 `hubStore.send` 返回非 false 时 `clearAnnotations`（POST 失败保留草稿，与「状态待确认」语义一致）；e2e 发送后徽标消失、`localStorage` 键为 null，下一条普通发送不再含任何前缀块。
- **终端段不提供批注**：tty/events 段不挂 `data-annotation-instance`（选区无法归属）且不挂 dock；e2e 对 `/s/:id/events` 断言 `[data-annotation-instance]` 计数为 0（该会话在假 Node 上无 pty，选 events 段作为终端式段的稳定表达）。
- **归档任务只读预览**：`useSessionTask` 从 `GET /v1/tasks` 解析会话所属任务（先 `instances.taskId` 归属、再 `placement.instanceId`），任务有 `archivedAt` 时：徽标 `disabled`/`data-disabled=1`、面板显示「只读预览：归档任务的会话不能批注」、不渲染输入表单、页面根 `data-annotation-readonly=1` 使 TaskDetailPanel 与 transcript 的选区 affordance 不弹出。组件测试覆盖只读徽标与面板。
- 手机（compact，D-049）：不另造表面，批注建在桌面优先；390 证据截图显示同一 dock/面板随 composer 收敛、同一设备本地草稿可见。

## 截图（仅 Remuda 自身渲染）

| 文件 | 宽 | 内容 |
|---|---|---|
| `task-model-9-composer-1440.png` | 1440 | 会话工作台：标记 tab 打开，① 锚点行（引文=transcript 正文，不含角色抬头）+ 卡片计数，徽标「批注 2 本次发送带 2 条批注」 |
| `task-model-9-composer-390.png` | 390 | 手机宽度：同一设备本地草稿随 composer 收敛，徽标/面板仍可读 |

截图只在 `REMUDA_EVIDENCE=1` 时写入（默认运行写 `web/tests/test-results/evidence/`，仓库树保持干净；已按任务要求在**不带标志**的默认跑后确认 `git status` 无 PNG 变更）。

## 测试记录

| 检查 | 命令 | 结果 |
|---|---|---|
| web 单测 | `pnpm --dir web test` | **155 files / 1573 passed**（本任务新增 23 条：annotations 19 + AnnotationPanel 4） |
| typecheck | `pnpm --dir web typecheck` | PASS |
| lint | `pnpm --dir web lint`（oxlint） | exit 0（新文件无 error；仅 fast-refresh/set-state-in-effect 既有 warning 级） |
| 本 spec ×3 | `HUB_E2E_LISTEN=127.0.0.1:59350 HUB_E2E_WEB_PORT=59359 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59351 PW_CHANNEL=chromium` + `flock e2e.lock-c`，`playwright -c playwright.hub.config.ts task-model-annotations` | 默认跑 3 轮全绿（每轮 2 passed + 1 skipped（证据用例））；另 `REMUDA_EVIDENCE=1` 轮 3 passed（截 1440/390） |
| 全量 hub e2e | 同一锁槽与端口，全 `*.hub.spec.ts` | **178 passed / 38 skipped / 1 failed（flake，见下）/ 27.4m**；唯一失败 `ux-files.hub.spec.ts` row9（FilesView SCM 变更检测等待，未触碰本任务代码，非指针拦截），单独重跑 **8/8 passed**，判定为既有时序 flake。开发中首跑还暴露并已修复两处本任务引入的布局问题：① 入流式 dock 永久压低 session body（m-chrome 0.577 < 0.60）→ 改零高度 float layer；② 全宽浮动条拦截 composer 档位/模型 chip（effort-sync/ux-modelpick/ux-modelsync click timeout）→ 浮动条收缩为靠右内容宽、零高度层 `pointer-events:none`，三 spec 单独重跑 **13/13 passed** |
| 默认跑脏树检查 | 不带任何标志跑本 spec 后 `git status` | 无新增/修改 PNG（证据仅 `REMUDA_EVIDENCE=1`） |
| secret-scan | `bash scripts/ci/secret-scan.sh` | 见下节 |
| no-tunnel-scan | `bash scripts/ci/no-tunnel-scan.sh` | 见下节 |

## 设计边界自检

- **零 wire / 零 schema / 零表**：批注只经 composer 既有 prompt text 通道；无任何 `crates/**`、`api.generated.ts`、protocol 变更。
- **协议文本不进 agent 提示词**：前缀是结构化反馈（载体 + 引文 + 批注正文），没有让 agent 自管看板动作的指令；前缀措辞断言无协议词。
- **不动 composer 三态/队列（D-042）**：折叠在 SessionPage 的既有回调边界外完成，Composer 内部状态机、ToolCard、session.module.css 均未改。
- **一实例一终端 / D-049**：无分屏、无第二 transcript；手机复用同一 `/s/:id`，批注只读式收敛（建锚点桌面优先，ui-spec §2.9）。
- **终端段 / 只读预览**：如上，两处均不提供创建入口。
