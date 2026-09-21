# task-model 任务 5（t-tasklist）证据：按项目/分支分组的任务列表（桌面 + /m）

- 日期：2026-09-21
- 分支：`wt/c-tasklist/b-tasklist-md`
- 计划：`briefs/plans/task-model.md` §(B.5)/§(B.6)、§(C) 任务 5（验收 1–7）、D12
- 设计依据：**D-050**、`docs/design/task-model.md` §9（人向表面）、`docs/design/ui-spec.md` §1.5/§2.9（任务列表）、§4.7（/m 任务分组层）；D-024（Space 键不放松）、D-049（/m 不造第二份 transcript）
- 范围：消费已落地的 Hub Task 台账（M1），交付桌面 `/board` 任务清单 + 详情正文面板，以及 `/m` home 之上的**任务分组层**。零 wire / 零 schema 改动。

> 合规：本文只用泛称（「参考工作清单产品」），未出现任何内部产品名、内部文档引述、公司主机名/用户名/家目录路径；证据截图均为 Remuda 自身在 1440 / 390 宽的渲染（假 Node 仅产生本地任务/会话数据）。

## 交付物

| 文件 | 内容 |
|---|---|
| `web/src/features/tasks/taskRows.ts`（新） | 纯投影模型 `buildTaskGroups()`：需要你首组、project+Space+branch 分组、`parentTaskId` 父子森林、每项目 `SE-nn` 派生 key、已归档折叠；`taskAwaitsHuman()`、`taskSpaceId()`（含父链目录继承）、`taskNextStep()`。无 React、无 fetch |
| `web/src/features/tasks/taskRows.test.ts`（新） | 16 个 vitest 用例：分组/blocked 计数、父子嵌套（含孙任务、孤儿父引用、归档子任务不跟随）、需要你首组、归档折叠、SE-nn 派生与跨项目序号重置、session 聚合与 placement 首选、父链 Space 继承、搜索、下一步文案 |
| `web/src/features/tasks/TaskList.tsx`（新） | `/board` 桌面页：280px 任务轨 + 详情面板；`useTaskLedger()`（轮询 `GET /v1/tasks` + `/v1/projects` 取名）、`useLiveBranches()`（复用文件视图只读 changes 代理，与手机 home 同一分支水合）、`TaskGroups`/`TaskRowView`（desktop/phone 两变体） |
| `web/src/features/tasks/TaskDetailPanel.tsx`（新） | 只读详情正文：状态徽标、标题、`blockedReason` 警示条、`mandate.chain` 正文（`data-anchor-surface`，任务 9 批注锚点的挂载面）、会话入口「在工作台打开」→ 共享 `/s/:id` |
| `web/src/features/tasks/tasklist.module.css`（新） | 轨/组/行/详情样式，复用既有 tokens；compact 媒体查询 |
| `web/src/features/mobile/homeRows.ts`（增量，只读 import） | 新增 `buildHomeTaskLayer()`，整体委托给 `buildTaskGroups()`；既有 `buildHomeGroups()` 一字未动 |
| `web/src/features/mobile/HomeList.tsx`（增量） | 在既有 project+branch 会话组之上渲染任务层；任务行点击进共享 `/s/:id`；`?project=` 透传（桌面 /board compact 塌缩时查询串原样保留） |
| `web/src/types/instance.ts`、`web/src/lib/api.ts` | 归一化 `Instance` 增 `taskId?: Id|null`，从 M1 已下发的 `InstanceRecord.taskId`（`instances.task_id`）映射；纯增量、未绑定会话为 null |
| `web/src/app/router.tsx`、`web/src/app/Shell.tsx`、`web/src/lib/mobileRoute.ts`(+test) | `/board` 路由（挂在 ViewportGate 内，compact 自动塌缩）、桌面轨「任务」入口（手机不显示，手机走 /m）、`/board → /m` 重定向及用例。主导航入口属本任务：否则新建的 `/board` 任务列表无任何可达路径，验收 1–6 的桌面表面无法进入；只加一枚桌面图标，未改会话主导航结构，看板任务（t-board-ui）复用同一路由 |
| `web/tests/e2e/task-model-list.hub.spec.ts`（新） | hub e2e：1440 桌面 + 390 手机两条，连跑 3 次全绿 |
| `docs/design/evidence/task-model-5-board-1440.png`、`task-model-5-home-390.png`（新） | Remuda 自身渲染证据 |

未触碰：`SessionList.tsx`、`SessionPage.tsx`、`Composer.tsx`、`playwright.hub.config.ts`、`sw.src.js`、`crates/**`、`deploy/**`。

## 验收逐条对照（计划任务 5 的七项）

1. **按 project + git branch 分组，组头 blocked 计数等于 `buildSpaces().blockedCount`。**
   组键是 `(projectId, Space id)`——Space id 即 `(hostId, workspaceId)`，D-024 不放松（同名/跨机目录各成一组）；branch 只是组头展示，由与手机 home 相同的只读 changes 代理水合，未知则不渲染。组头 blocked 数直接取对应 `Space.blockedCount`（搜索过滤时仍报整空间，不报过滤后切片）。e2e 在 `wsp_e2e` 启一个 `mhome-blocked` 会话，断言其项目组头为「1 待处理」；`taskRows.test.ts` 断言与 `buildSpaces()` 的 `blockedCount` 精确相等。
2. **父子树嵌套。** 组内按 `Task.parentTaskId` 建森林（`buildForest`），父行显示 `▸ N` 直接子计数，子行 `data-depth` 递增（孙任务 depth=2 有用例）；父任务在其目录分组内，未派发/未绑定的子任务**继承最近的已放置祖先的目录 Space**（cycle-guarded 父链 `taskSpaceId()`），保证一家人在同一 project+branch 组；父引用缺失的孤儿任务和已归档子任务都按顶层处理（归档子任务不跟随父进活跃组）。
3. **「需要你 · N」首组。** `taskAwaitsHuman()` = 任务有 session 挂着 pending Interaction（关联既有 Interaction 收件箱 `/m/inbox`）**或** task 自带 `blockedReason`（待所有者）。该组恒为第一组、跨 project 聚合，且任务**同时**保留在其 project 组（pin，不移动）。它与原始计数的区别有专门用例：一个 session 被 block 但任务无 pending interaction、无 blockedReason 时，**不**进需要你组，而 project 组头仍显示原始 blockedCount=1；已归档任务即使有 blockedReason 也永不进需要你组。
4. **派生 `SE-nn` key。** 复用任务 4 的 `displayKeysByTaskId()`：按 project 内 created_at 最旧优先编号，display-only、无 schema 改动；每行渲染 `SE-nn`，不出现裸 `tsk_…`（e2e 对整个组容器断言不含 `tsk_`）。
5. **详情正文面板。** `TaskDetailPanel` 渲染状态徽标、`title`、`blockedReason`（带 ⚠ 形状+文案，不只靠颜色）、`mandate.chain`（depth 排序的继承链，可选中正文，挂 `data-anchor-surface="task-detail"`），是任务 9 建 `①` 锚点的正文表面；「会话」区提供共享 `/s/:id` 入口，无会话时显示空态。
6. **归档折叠 + session 数/下一步/进工作台。** `archivedAt != null` 的任务全部折进末尾「已归档 · N」组，e2e 逐一断言不出现在任何活跃组（状态不变，归档正交沿用任务 4）；每行显示 session 数（`instances.task_id` 聚合）与一句下一步（`taskNextStep()`：需要你处理 → blockedReason → 待派发/状态短语，done 永不暗示解锁），有会话的行带 `/s/:id`「打开」链接（placement.instanceId 首选，其次存活会话）。
7. **/m 手机按 task 分组、不造第二 transcript。** HomeList 在既有会话组之上渲染同一纯投影（`buildHomeTaskLayer`），390 证据可见需要你首组、嵌套、SE-nn、未绑定目录标注、已归档折叠，且下方既有会话 home 保持不变；任务行整体是到共享 `/s/:id` 的链接，e2e 点击后断言落在 `/s/:instanceId`，没有任何第二份会话实现。桌面 `/board` 在 compact 下经 `resolveLanding` 塌缩到 `/m`（`?project=` 原样保留，新增单测）。

## e2e 方法

`web/tests/e2e/task-model-list.hub.spec.ts` 不引入假 Node fixture（**无新 trigger**，默认全量套件行为不变）：项目/任务/split/state/archive 全走真实 Hub API；一个任务会话经普通派发落在默认 e2e 工作区，prompt 带 `mhome-blocked` 哨兵——假 Node 据此既挂起 approval（Interaction pending = 需要你），又把原生状态置为 blocked（= 组头原始 blockedCount），正好区分两类信号。会话在 `afterAll` 用自己记录的 id 做 `DELETE ?force=1` 清理（不杀任何非本测试启动的进程）。

## 验证记录

- `pnpm --dir web test`：151 文件 / 1498 用例全绿（含新增 16 个 taskRows 用例与 mobileRoute 新用例）。
- `pnpm --dir web typecheck`：通过。
- `pnpm --dir web lint`：退出 0（新文件仅 3 条 fast-refresh/set-state-in-effect 提示级 warning，与仓库既有同型）。
- hub e2e `task-model-list.hub.spec.ts`（锁槽 `flock …/locks/e2e.lock-b`，端口 `HUB_E2E_LISTEN=127.0.0.1:59310 HUB_E2E_WEB_PORT=59319 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59311`，本地无 Google Chrome 故 `PW_CHANNEL=chromium` 用内置 Chromium）：连跑 **3 次**，每次 1440 + 390 两条均通过。
- 全量 web hub e2e 套件在同一锁槽/端口下跑 1 次：**172 passed / 28 skipped / 0 failed**（26.0m，skip 均为预期的 gated 假 Node 用例、真实 Node 用例、证据用例与 mock 用例；本任务的 `task-model-list.hub.spec.ts` 两条在套件内编号 100/101 通过）。
- `bash scripts/ci/secret-scan.sh`：pass；`bash scripts/ci/no-tunnel-scan.sh`：passed。

## 截图

- `task-model-5-board-1440.png`（1440×900，Remuda 自身渲染）：需要你 · 2 首组；项目组 `feat/workbench-g2` 组头「3 1 待处理」，父 SE-01 `▸ 2` 下嵌 SE-02/SE-03；未绑定目录组「0 待处理」含 SE-04 失败/SE-05 进行中；末尾已归档 · 1（SE-06）；右栏详情显示 SE-04 失败徽标、标题、⚠ blockedReason、MANDATE 继承链。
- `task-model-5-home-390.png`（390 宽，Remuda 自身渲染）：`/m?project=…` 任务层叠在既有会话 home 之上，同一投影；任务行点按进共享 `/s/:id`（不造第二 transcript）。
