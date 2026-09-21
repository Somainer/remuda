# task-model 任务 8（t-project-switcher）证据：Web 采纳 Project 实体 + 顶栏项目切换器

- 日期：2026-09-21
- 分支：`wt/c-projectswitcher/b-projectswitcher-md`
- 计划：`briefs/plans/task-model.md`（(C) 任务 8，B.5、G9、dispatch 第 3 波 M2）
- 设计依据：**D-050 §9**、[ui-spec.md §1.1 / §2.9](../ui-spec.md)（`projects` 行与「项目切换器」段）、D-024（Space 键不放松）
- 范围：Web 停止渲染 Workspace 通讯录占位页，改为经生成客户端读 Hub `Project` 实体；顶栏 `全局 ▾ project` 切换器按 projectId 过滤任务列表与看板，全局显示全部；`members[]` 经完整 `(hostId, workspaceId)` 对映射到 Space；bot 通道 `defaultProject` 引用保持有效。

> 合规：本文只用泛称（参考看板产品）与公开项目语境，不含任何内部产品名、内部文档引述、内部主机名/用户名/主目录；截图四张全部是 **Remuda 自身渲染**（390 与 1440 宽，假 Node 标签 `e2e-fake-node` / `e2e-project-host-b` 与 `/tmp/remuda-project-{a,b}` 为 e2e 夹具专用占位）。

## 交付物（严格 = 任务 8 归属文件）

| 文件 | 内容 |
|---|---|
| `web/src/pages/ProjectsPage.tsx` | 列表经 `rest<ProjectPage>("GET /v1/projects")` 读 **Project** 实体（成员数、主机数、基线分支；不再列 workspace）；详情 `GET /v1/projects/{id}` 渲染实体字段 + 成员经 `projectMemberRows()` 解析为 Space 行（未注册成员显示裸对，不并入「其他」） |
| `web/src/features/tasks/ProjectSwitcher.tsx` | 新建。`全局 ▾ project` 组件 + 设备本地选择存储（localStorage，跨标签页持久，仿 `spaces/store.ts`，**非 wire 字段**）；纯函数 `filterTasksByProject` / `tasksPath` / `boardPath`（任务列表与看板未来表面共用的过滤与查询拼法）；`resolveProjectMember` / `projectMemberRows` / `projectMemberHostIds`（D-024 完整对映射）；`useProjects` 目录读、`projectLabel`（独立播种引用的显示回退） |
| `web/src/features/tasks/projectSwitcher.test.ts` | 新建。18 条 vitest：过滤 + 全局、跨主机 Space 键不合并、完整对解析、bot 引用回退、存储持久/通知、目录 hook、组件选择/导航 |
| `web/src/app/Shell.tsx` | 顶栏挂载（唯一一处 Shell 增量）：桌面端主区顶部右侧渲染切换器；compact 不渲染（D-049，手机项目分组在 `/m`） |
| `crates/remuda-hub/examples/hub_e2e.rs` | **e2e 夹具（非产品代码）**：新独立触发 `HUB_E2E_PROJECT_SWITCHER=1` 时，主假 Node 多宣告一个 `/tmp/remuda-project-a` 品牌工作区，并 enroll 第二台假主机 `e2e-project-host-b`（宣告 `/tmp/remuda-project-b`，正确应答 `workspace.list` 快照 RPC）；默认运行行为逐字节不变 |
| `web/tests/e2e/task-model-project.hub.spec.ts` | 新建。gated hub e2e（触发缺失时文件级 self-skip，镜像 `task-model-bind.hub.spec.ts`），6 个用例 |

## 验收逐条对照（计划任务 8 的四项）

1. **ProjectsPage 读 Project 实体，不再列 workspace**：
   - 列表/详情都经生成客户端打真实 Hub 路由（`GET /v1/projects` → `ProjectPage{items,nextCursor}`、`GET /v1/projects/{id}` → `Project`）；旧 stub 的「Workspace 通讯录，不是独立项目实体」文案与 `hub.workspaces` 列举全部删除。
   - e2e：项目行显示 `2 个成员 · 2 台主机 · 基线 main` 与两台主机标签；页面正文断言**不含**旧通讯录泄露的 `/tmp/remuda-e2e`；行链接为 `/projects/{projectId}`，不存在指向 `workspaceId` 的链接。
2. **切换器按 projectId 过滤任务列表与看板；全局显示全部**：
   - 选择即写设备本地 `remuda.project-filter.v1`；任务列表/看板表面（M2 并行任务 5/6）读 `useProjectFilter()`，Hub 侧作用域用文档既有的 `GET /v1/tasks?project=` 与 `GET /v1/board?project=`（拼法集中在 `tasksPath`/`boardPath`，全局不带 query）。
   - vitest：全局保留跨项目全部行；选定项目只留该项目行；`tasksPath`/`boardPath` 对 id 做 `encodeURIComponent`。
   - e2e（真实 Hub 投影）：全局 `/v1/tasks` 与 `/v1/board`（`project: null`）含两个项目各一张卡；选定单机项目后 scoped 两个端点只返回该项目卡（跨机项目卡被排除），`/projects` 页自身也只剩 1 行；切回全局后存储键清除、行恢复 2、board `project` 回到 `null`。
3. **members 经 (hostId, workspaceId) 映射到 Space，不放松 Space 键（D-024）**：
   - `resolveProjectMember` 要求**两半都匹配**已注册工作区才解析：同一 `workspaceId` 出现在另一台主机上不解析（不跨主机借名）；解析出的 Space `id === spaceKey(hostId, workspaceId)`；未注册成员仍返回一行（只有裸键，无 Space），绝不折进 `OTHER_SPACE`。
   - vitest：同工作区 id 在两主机上是两个不同键；跨主机项目给出 3 行（含 1 个未注册对）与 `["hst_a","hst_b"]` 主机集合。
   - e2e：跨机项目详情 2 个成员行，`data-space-key` 恰为 `JSON.stringify([hostA,wspA])` 与 `JSON.stringify([hostB,wspB])`，集合大小 2；两行各显示自己的 `/tmp/remuda-project-a|b` 根，无「工作区尚未注册」回退。
4. **BotChannel.defaultProject 仍是有效引用**：
   - `channels.ts` 一字未动（`"sfe-root"`）；`projectLabel(id, [])` 在目录不含该 id 时逐字返回原串（不置空、不改写），BotsPage 通道详情继续渲染 `默认项目 sfe-root`。vitest 遍历 `BOT_CHANNELS` 断言引用非空且空目录下回退为原串；e2e 在 `/bots/feishu` 的 `bot-defaults` 上断言文本可见。

## 顶栏与 IA

- 桌面：`<main>` 顶部右侧一条 scope 条（`项目范围` + 原生 `<select>`，44px 触控命中，复用 `ui.select`/`ui.touchSelect`），在所有 Shell 页面可见；切换只改全局过滤，不强制导航。
- `/projects` 与 `/projects/:id` 页头另有一个同 store 的切换器（`navigateOnSelect`），在 projects 表面选择会导航到 `/projects/{id}`、全局回列表；从其他表面选择不导航（e2e 覆盖两种行为）。
- compact：顶栏切换器不渲染（D-049：手机项目分组挂 `/m`，不复制桌面 IA）；projects 页头切换器仍可用。
- 存储与已注册空间选择器同形（版本化 localStorage、无存储时降级可用、跨标签页 `storage` 事件语义留给既有 store 模式；本批次未加监听，刷新即读新值）。

## e2e 夹具（为什么需要第二台假主机）

Hub 的 `resolve_members`（`crates/remuda-hub/src/projects.rs`）要求成员主机已 enroll 且该品牌 `workspaceId` 已在该主机快照里注册，因此「一个项目跨两台假主机」无法用单台假 Node 表达，也不能靠造数据绕过 D-024。新增触发 **`HUB_E2E_PROJECT_SWITCHER=1`**：

- 主假 Node（`e2e-fake-node`）在既有 8 个工作区之外多宣告 1 个品牌工作区，根 `/tmp/remuda-project-a`（独立根，避免与他规格的 per-root chip 合并）；
- 新 enroll 第二台假 Node `e2e-project-host-b`：hello 宣告 1 个品牌工作区（根 `/tmp/remuda-project-b`），并对 `workspace.list` 返回同形快照（`workspaceRevision:1`）；其余 RPC 答 `{ok:true}`（规格不在该机起会话）；
- 默认（不设触发）：只 enroll 主假 Node、工作区清单与之前完全一致 → 全量默认跑到本规格时文件级 **skipped**，闸口形态不受影响（与 t-bind、api-route 的 gated 约定一致）。

## 测试记录

| 检查 | 命令 | 结果 |
|---|---|---|
| web 单测 | `pnpm --dir web test` | 151 files / 1501 passed（含新增 18 条） |
| typecheck | `pnpm --dir web typecheck` | PASS |
| lint | `pnpm --dir web lint`（oxlint；新文件仅 fast-refresh 既有 warning 级，无 error） | exit 0 |
| 夹具编译 | `cargo check -p remuda-hub --example hub_e2e` | PASS |
| hub e2e（本规格 ×3） | `pnpm playwright test -c playwright.hub.config.ts task-model-project`（锁 `locks/e2e.lock`，端口 59330/59331/59339，`HUB_E2E_PROJECT_SWITCHER=1`，PW_CHANNEL=chromium） | 6 passed × 3（47.0s / 46.1s / 49.2s） |
| hub e2e（全量 ×1，默认不设触发） | 同一锁槽 `pnpm playwright test -c playwright.hub.config.ts` | **170 passed / 34 skipped / 0 failed**（25.4m；34 skipped 含本规格文件级 self-skip 的 6 个用例，已单独复跑确认 6 skipped） |
| 密钥/隧道扫描 | `bash scripts/ci/secret-scan.sh` / `bash scripts/ci/no-tunnel-scan.sh` | `secret-scan: pass`（exit 0）/ `no-tunnel-scan: passed`（exit 0） |

### 本规格三次连跑输出（末尾摘要）

```text
Running 6 tests using 1 worker
  ✓  the projects page reads Project entities and no longer lists workspaces
  ✓  members map to Spaces through the full (hostId, workspaceId) pair — two hosts stay two keys (D-024)
  ✓  the switcher scopes the board and the task list by project id; global shows everything
  ✓  the bot channel defaultProject reference stays a valid display reference
  ✓  evidence render: projects directory and the cross-host project at 390px
  ✓  evidence render: projects directory and the cross-host project at 1440px
  6 passed (47.0s / 46.1s / 49.2s)
```

（证据截图用例仅在显式 `REMUDA_EVIDENCE=1` 时写入 `docs/design/evidence/`，普通运行落到 `test-results/evidence`。）

> rebase 复核：推送前 `origin/main` 前进到 effortflake 合并（`fd37d714`，触及 `web/src/lib/store.ts`），rebase 无冲突；rebase 后重跑 `pnpm --dir web typecheck`（PASS）、lint（exit 0）、`pnpm --dir web test`（**151 files / 1505 passed**，新增 4 条为 effortflake 用例）、`cargo check -p remuda-hub --example hub_e2e`（PASS），并在同一锁槽重跑本规格：4 passed / 2 skipped（2 个 skipped 为未设 `REMUDA_EVIDENCE` 的截图用例）。

### 端点 transcript（live Hub，id 已脱敏）

```jsonc
// POST /v1/projects —— 成员直接跨两台已 enroll 的主机
{ "name": "跨机项目 …",
  "members": [
    { "hostId": "hst_A…", "workspaceId": "wsp_projectA…", "role": "primary" },
    { "hostId": "hst_B…", "workspaceId": "wsp_projectB…", "role": "build" } ] }
// 200 → Project{ id:"prj_A…", members:[…两对…], defaultBaseBranch:"main", … }

// GET /v1/hosts/{hst_B}/workspaces → {workspaceRevision:1, workspaces:[{workspaceId:"wsp_projectB…", root:"/tmp/remuda-project-b"}]}
// GET /v1/board                     → { "project": null, columns:{ …两个项目的卡… } }
// GET /v1/board?project=prj_other   → { "project": "prj_other…", columns:{ …只含单机项目卡… } }
// 顶栏 select prj_other → localStorage["remuda.project-filter.v1"] === "prj_other…"
// 顶栏 select 全局      → 该键删除；GET /v1/board 重新为 "project": null
```

## 截图（仅 Remuda 渲染，390 / 1440）

| 文件 | 画面 |
|---|---|
| `task-model-8-projects-1440.png` / `-390.png` | `/projects`：Project 实体行（成员/主机数、基线），顶栏 + 页头两个 `全局` 切换器 |
| `task-model-8-members-1440.png` / `-390.png` | 跨机项目详情：两台假主机各一个成员工作区，根不同、Space 键不合并（D-024），两个切换器同步显示所选项目 |

## 故意不做（守住任务 8 边界）

- 不建任务列表/看板 UI（并行任务 5/6 的归属文件）；过滤只提供它们消费的 store + 纯函数与查询拼法。
- 不新增任何 wire/字段/路由：切换作用域复用 M1 已落地的 `?project=` 查询；选择只存设备本地。
- 不改 Space 键、不合并跨主机/同名目录（D-024）；未注册成员显式显示裸对而非隐藏或并入「其他」。
- 不碰 `playwright.hub.config.ts`、`sw.src.js`、`deploy/`；夹具改动只在 `examples/hub_e2e.rs` 且由独立触发门控。
