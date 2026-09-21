# task-model 任务 6（t-board-ui）证据：桌面看板（三列 + 已归档过滤 + failed 角标）

- 日期：2026-09-21
- 分支：`wt/c-boardui/b-boardui-md`
- 计划：`briefs/plans/task-model.md` §(B.4)/§(B.5)、§(C) 任务 6（验收 1–7）、D5、D12
- 设计依据：**D-050**、`docs/design/task-model.md` §5（看板投影）/§9（人向表面）、`docs/design/ui-spec.md` §2.9（任务列表与看板）；D-024（Space 键不放松）、D-033/I1（done≠解锁）、D-049（compact 不复制看板）
- 范围：消费任务 4 已落地的 `GET /v1/board` 投影与任务 5 的任务分组/详情面板，交付一个操作员可直接驱动的桌面看板。零 wire / 零 schema 改动。

> 合规：本文只用泛称（「某参考看板产品」），未出现任何内部产品名、内部文档引述、公司主机名/用户名/家目录路径；证据截图为 Remuda 自身在 1440 宽的渲染（假 Node 仅产生本地任务/会话数据）。

## 交付物

| 文件 | 内容 |
|---|---|
| `web/src/features/tasks/boardModel.ts`（新） | 看板纯视图模型：消费 `GET /v1/board`，**严格三列**（待办/进行中/已完成）+ 独立的已归档折叠；每卡聚合其 session（placement 首选、存活优先、时间倒序）、config-reuse 标签、以及按租约身份键 `(hostId, workspaceId, dir_key)` 派生的「与 N 个 task 共用」计数（D-024：跨主机/跨 workspace/不同 worktree 绝不合并）。每卡的三列可拖性由任务 4 的 `columnMoveHops`（`can_transition_to` 上的 BFS）**预计算**：合法拖给出 hop 序列，不可达给出操作员可读原因。无 React、无 fetch |
| `web/src/features/tasks/boardModel.test.ts`（新） | 24 个 vitest 用例：列投影（与 `boardColumn` 逐条一致）、failed 按 placement 落列、归档是过滤器不是第四列、多跳可拖性（pending/deferred→placed→running、stalled→running→done、running/stalled→parked）、终态/同列/跳过进行中列的禁用原因、共享计数（含 reuse-to-root 的 `.` 键、跨 Space 不合并、归档持有者仍计数）、卡内 session 排序与首选、`default`/`none` 基线配置标签、搜索、空视图 |
| `web/src/features/tasks/Board.tsx`（新） | `/board` 桌面看板：左侧复用任务 5 的 280px 任务轨（`TaskGroups` + `useLiveBranches`），中部三列 + 归档折叠，右侧**只读预览**。轮询 `GET /v1/board`（5s）与项目名；卡片列出 session 行（harness 字形 + 相对时间，链接进共享 `/s/:id`）、`SE-nn` key、`default` 配置 chip、共用 footer；failed 红角标（⚠ 形状 + 文案，不只靠颜色）；HTML5 拖卡逐跳 PATCH，归档走 `/archive`；403/409 渲染 Hub 的 grant/状态机拒绝文案；归档折叠是过滤区不是列；项目范围 `?project=` 深链优先、否则读顶栏切换器 |
| `web/src/features/tasks/board.module.css`（新） | 三列网格、卡片、红角标、禁用列（红描边 + 原因条）、归档折叠、预览横幅样式，复用既有 design tokens |
| `web/src/app/router.tsx`（增量） | `/board` 由 `TaskListPage` 改为 `BoardPage`；路由仍在 D-049 `ViewportGate` 内，compact 自动塌缩到 `/m`，不克隆看板 |
| `web/tests/e2e/task-model-boardui.hub.spec.ts`（新） | hub e2e（`HUB_E2E_TASK_BIND=1` 假 Node）：三列各一卡 + 一归档 + 一目录两 task 共享，连跑 3 次全绿 |
| `docs/design/evidence/task-model-6-board-1440.png`（新） | 1440×900 Remuda 自身渲染证据 |

复用而非重写：列投影与多跳拖拽路径来自任务 4 的 `boardColumns.ts`（`WORK_COLUMNS`/`columnMoveHops`/`boardColumn`），人读 key 与状态文案来自任务 4/5（`SE-nn` 已由 Hub 在 BoardItem 上下发，客户端不重算），任务轨/分支水合/详情面板来自任务 5（`TaskList.tsx`/`taskRows.ts`/`TaskDetailPanel.tsx`），共用文案复用任务 3 的 `sharedWithTasksLabel()`。未触碰：`SessionList.tsx`、`SessionPage.tsx`、`playwright.hub.config.ts`、`sw.src.js`、`crates/**`、`deploy/**`。

## 验收逐条对照（计划任务 6 的七项）

1. **三列消费 `GET /v1/board`；已归档是过滤器不是第四/五列。** 看板恒渲染待办/进行中/已完成三列，卡片的列**直接采用** Hub 下发的 `boardColumn`（客户端不二次投影，避免漂移）。`archivedAt != null` 的卡只出现在独立的「已归档」折叠里，由工具栏开关控制显示，归档不改 state（e2e 断言归档卡不在任何工作列，且 `GET /v1/tasks/{id}` 仍是原状态）。
2. **卡列 session 行 + 共用 footer + `default` 标签 + `SE-nn` key。** 每张卡渲染 `instances.task_id` 归属的 session 行（harness 字形 + `formatListTime` 相对时间，placement.instanceId 首选，行链接进共享 `/s/:id`）；footer 左侧 `default` 配置 chip（B.8 config-reuse：未配置与基线 native profile `none`（D-012）都显示 `default`，具名 profile 才显示其 id），右侧「与 N 个 task 共用」（同租约身份键 refcount，独占显示「独占目录」）；卡头显示 `SE-nn`，不出现裸 `tsk_`（e2e 对卡片断言）。
3. **failed 红角标 + blockedReason，按 placement 落列，绝不折进 done。** failed 卡沿用 Hub 的 placement 派生列（有 placement→进行中、无→待办），卡上叠红色 ⚠ 角标并可见 `blockedReason`；e2e 造一张 dispatch placement 后 failed 的卡，断言它在进行中列、带角标与原因、且不在 done 列。**卡进已完成列不解锁任何依赖**：UI 不暴露 land 动作（没有任何 land 控件），done 列的卡是终态、不可拖，land 只走 gate/`POST /land`（I1）。
4. **拖卡可拖性预计算 + 多跳。** 每张卡的 `drops` 由当前 state 经 `columnMoveHops` 预计算：合法目标给出 hop 序列（待办→进行中对 pending/deferred 走 `pending→placed→running` 多跳；进行中→已完成对 running 单跳、对 stalled 走 `stalled→running→done`；进行中→待办落 `parked`）；不可达列在拖拽期间 `data-drop="disabled"` 并显示原因条（终态、已归档、同列、跳过进行中列各有明确中文原因），**不松手后 4xx 才报错**。终态卡（done/failed）`draggable=false`，根本不可拖。e2e 用真实拖放序列把 pending 卡移到进行中（随后断言任务状态确为 running）、再到 done；并断言待办卡拖向 done 时目标列 disabled 且带原因。
5. **门控按 grant 动词，UI 不假设「agent 一律禁写」。** 拖卡对每跳发 `PATCH /v1/tasks/{id}`、归档发 `POST /v1/tasks/{id}/archive`，Hub 按 `GrantVerb::Dispatch` 门控；看板对所有非终态卡照常开放移动。e2e 拦截 PATCH 返回 403，断言看板内联渲染「没有 Dispatch 授权…」拒绝条且卡片留在原列——这是「照常发起、Hub 授权、拒绝可见」，不是按调用方身份预先禁写。
6. **看板详情 = 只读预览。** 点卡片打开右侧面板，顶部固定「**预览模式**」横幅与「在工作台打开以完整操作」链接（→ 共享 `/s/:id`，无会话时给「启动会话后…」静默态）；正文复用任务 5 的 `TaskDetailPanel`（mandate/title/blockedReason）。看板表面**没有 composer**（无任何输入/发送控件），完整操作只在工作台。e2e 断言横幅、链接 href 与标题。
7. **compact 重定向到 `/m`，不复制看板。** 路由挂在任务 5 已落地的 D-049 `ViewportGate` 内，`resolveLanding` 对 compact 的 `/board` 返回 `/m`（`?project=` 原样保留），手机由任务 5 的任务分组层承载，不另造单列看板或第二份 transcript。该重定向由 `mobileRoute` 单测覆盖（任务 5 已含 `/board → /m` 用例），本任务未改其逻辑。

## e2e 方法

`web/tests/e2e/task-model-boardui.hub.spec.ts` 由假 Node 的门控 fixture 驱动：spec 文件顶部 `test.skip(HUB_E2E_TASK_BIND !== "1")`，默认全量套件（无该变量）**整文件跳过**，与 `task-model-bind.hub.spec.ts` / `api-route.hub.spec.ts` 同一 idiom。fixture 用一个品牌 UUID workspace 建项目并加成员；经真实 Hub API 造：待办/进行中/已完成各一卡、一张 dispatch placement 后 failed 的卡（进行中红角标）、一张归档卡、以及两张复用同一 `agent-one` 兄弟目录（refcount 2）的卡；待办卡再经普通派发在默认 e2e workspace 起一个会话（卡内 session 行 + `default` 基线标签）。拖放触发浏览器真实的 `dragstart/dragenter/dragover/drop/dragend` 事件序列（共享一个 `DataTransfer`），跑的是应用真实 React 处理器与逐跳 PATCH——CDP 合成鼠标在 Chromium 进入原生拖拽态后会挂起，故用等价的真实事件序列而非 `mouse.move`/`dragTo`。会话在 `afterAll` 用记录本 spec 启动的 id 做 `DELETE ?force=1` 清理（不杀任何非本测试进程），并恢复假 Node 的实例上限。

证据截图：每次 capture 都在 `REMUDA_EVIDENCE === "1"` 守卫内，默认运行把路径指向被 gitignore 的 `web/test-results/evidence` 且函数直接 return，**默认运行不写任何 PNG**；已实际无标志跑一遍确认 `git status` 无 PNG。

## 验证记录

- `pnpm --dir web test`：154 文件 / **1574 用例全绿**（含新增 24 个 boardModel 用例；任务目录套件 123 用例）。
- `pnpm --dir web typecheck`：通过。
- `pnpm --dir web lint`：退出 0（新文件仅 set-state-in-effect 等与 `TaskList.tsx` 同型的提示级 warning）。
- hub e2e `task-model-boardui.hub.spec.ts`（锁槽 `flock …/locks/e2e.lock-b`，端口 `HUB_E2E_LISTEN=127.0.0.1:59340 HUB_E2E_WEB_PORT=59349 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59341`，`PW_CHANNEL=chromium` 内置 Chromium，`HUB_E2E_TASK_BIND=1`）：连跑 **3 次**均通过（各约 12s）；无 trigger 跑 **1 次整文件 skip**、无 PNG 落库。
- 回归：`task-model-list.hub.spec.ts`（任务 5，复用 `/board`）1440 + 390 两条在新看板页上全绿（最终代码上复跑通过）。
- 全量 web hub e2e 套件在同一锁槽/端口下跑 1 次（无 trigger，与 gate 一致）：**177 passed / 38 skipped / 0 failed**（26.0m）；本任务 spec 在套件内编号 100，默认运行按预期 skip（38 个 skip 均为门控假 Node/真 Node/证据/mock 用例）。
- `bash scripts/ci/secret-scan.sh`：pass（退出 0）；`bash scripts/ci/no-tunnel-scan.sh`：passed（退出 0）。

## 截图

- `task-model-6-board-1440.png`（1440×900，Remuda 自身渲染）：左轨「需要你 · 2」置顶、项目/分支分组与「已归档 · 1」折叠；中部三列——待办含 SE-06/SE-07（均「与 2 个 task 共用」），进行中含 SE-02 与 SE-04 红色「⚠ 失败 · worker exited 42」角标，已完成含 SE-01/SE-03（终态、不可拖）；底部已归档折叠显示 SE-05 并注明「过滤器，不是看板列」；右侧只读预览「预览模式 · 在工作台打开以完整操作」+ mandate 正文；顶栏「没有 Dispatch 授权，Hub 拒绝了拖卡」即验收 5 的 grant 拒绝内联呈现。
