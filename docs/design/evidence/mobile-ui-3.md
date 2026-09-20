# 手机优先 UI · 任务 3：`/m` 会话 home（分组 + 下一步 + context 环 + 错误当正文）

2026-09-20 · `wt/c-mhome/b-mhome-md` · mobile-ui 实施计划 §(C) 任务 3（c-mhome）
规格权威：[ui-spec.md](../ui-spec.md) §1.3 / §2.1 / §4.7，[decisions.md](../decisions.md) D-038、D-049；基线 `origin/main` @ 39d5bb73（c-mshell、c-nextstep 已落地），rebase 后基于 927bde54。

截图：`mobile-ui-3-home-390.png`（390×844，合成数据，来自 in-process fake node；无主机路径 / 用户名 / 真实主机名）。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/features/mobile/homeRows.ts` | 新增。纯派生：`buildHomeGroups()` / `homeError()` / `homeBody()` + 本设备排序读写（`HOME_ORDER_KEY`） |
| `web/src/features/mobile/homeRows.test.ts` | 新增。分组、两种排序、blocked 置顶、错误优先、状态待确认不被覆盖、null 环、搜索边界、resume 能力 |
| `web/src/features/mobile/ContextRing.tsx` / `.test.tsx` | 新增。SVG 剩余上下文环；`contextPct == null` 不渲染环也不渲染数字；0 与 100 边界、越界钳制 |
| `web/src/features/mobile/HomeList.tsx` / `home.module.css` | 新增。`/m` 手机首页组件与样式 |
| `web/src/features/mobile/HomeList.test.tsx` | 新增。恢复失败留在原地显错、成功时恢复中态的组件级覆盖（e2e 覆盖成功路径） |
| `web/src/app/router.tsx` | 仅 `/m` index 元素：占位 `SessionsPage` → `HomeList`。桌面路由零改动 |
| `web/tests/e2e/m-home.hub.spec.ts` | 新增。390px hub 规格（分组头/blocked 计数/置顶、context 环、错误当正文、搜索边界、排序记忆、一键恢复） |
| `crates/remuda-hub/examples/hub_e2e.rs` | fake node 新增 `mhome-blocked`（approval + usage=50% + blocked）与 `mhome-exit`（native session id + 带 lastError 的 exited）两条合成场景；通用 `instance.resume` 回复补 journal `ready` |
| `web/tests/e2e/hub-auth.ts` / `m-shell.hub.spec.ts` / `ux-keys.hub.spec.ts` | 共享登录与既存 compact 断言改指 `home-list`（桌面仍断言 `session-list`） |

**只读复用，未改一行**：`features/session/nextStep.ts`（句子原样）、`liveSummary.ts`、`features/spaces/store.ts` 的 `buildSpaces`（分组与 blockedCount 唯一来源）、`features/session/contextUsage.ts`（`UsageRollup.contextPct`）、`features/search/quickFindSearch.ts` 的 `rankQuickFind`（标题/项目/主机搜索边界）、`lib/store.ts` 的 `resume` / `titleOf` / `hostName` / `usageRollupOf` / `summaryOf`、`features/files/filesApi.ts` 的 `fetchChanges`（git 分支）。

未触碰：`SessionList.tsx(.module.css)`、`SessionPage.tsx`、`Composer.tsx`、`PhoneShell.tsx`、`playwright.hub.config.ts`、`sw.src.js`、`deploy/`。

## 2. 行口径（验收 1 / 2 / 3）

行 = **状态点 + 标题 + 一句正文 + context 剩余环**，全部投影已有字段，无新状态机。

- **验收 1（D-038 同口径）**：默认视口不出现 `lifecycle · activity · connectivity` 三元组、`ins_` 短码、driver、model。e2e 不只看 testid——`home-list.innerText` 用正则断言不含 `ins_[a-z0-9]`、`waiting-interaction|connected|disconnected|reconciling`、`*-pty|claude-print`、`e2e/auto|driver|model`，并显式断言不包含两条实例 id 全文。
- **验收 2**：正文句直接调 `nextStep(instance, pending, screen, summary)`，输出**原样**使用（单元测试断言语料逐字相等，如「回合结束、进程仍在 · 可继续发送」）。context 环取 `hubStore.usageRollupOf(id).contextPct`；`null` 时 `ContextRing` 返回 `null`——无环、无 `0%`、无数（vitest + e2e 双覆盖；50% 语料断言 `data-pct="50"` 与可见数字）。
- **验收 3（错误当正文）**：`homeError()` 先取 `instance.lastError`（Hub `store.rs` 已把 native severity=error 的 lifecycle 折进该字段），缺省时回退查本地 journal 里最近一条 native 失败 lifecycle 的 `relatedIds.lastError` / `status.value`。错误文本占据正文位（CSS 单行截断，`title` 给全文，e2e 断言 body `data-error="1"` 与 title 含全文）。**不推断成功**：`projectStatus === "unknown"`（`connectivity ≠ connected` 或 `lifecycle ∈ {unknown,reconciling}`）时正文一律保持 nextStep 的「状态待确认 · 不推断成功或结束」，错误也不能覆盖（vitest 对断连 + reconciling 各一例断言不含错误词与正向词）。
- exited 行：`nextStep` 的 exit 文案 + `canResume`（`capabilities.resume.state === "supported"`，D-026）才渲染「恢复」。

## 3. 分组、排序与置顶（验收 4）

- **分组 = 项目 + git branch**：空间来自 `buildSpaces(hub.workspaces, hub.instances, prefs)`，组头显示项目名 + 实时分支（`fetchChanges()` 的 `workspace.scm.status` 代理，分支 unknown/拒绝/离线时只留项目名）。未注册 workspace 的实例进「其他」并恒排最后（沿用 buildSpaces 的 OTHER_SPACE 顺序）。
- **组头 blocked 计数** = `space.blockedCount` 原值，搜索过滤期间也不重算（搜索只隐藏行，不改计数）；vitest 直接与 `buildSpaces().blockedCount` 比对，e2e 断言组头 `1 待处理`。
- **blocked 置顶**：两种排序下 blocked 行恒在组顶（ui-spec §2.1 待处理置顶、不进折叠组）。
- **两种排序**：时钟 = 组按组内最近 `updatedAt` 倒序、组内按 `updatedAt` 倒序；列表 = 组按项目名、组内按标题。选择存 `localStorage["remuda.mobile.home.order.v1"]`（本设备，不跨设备），e2e 断言切换、reload 后保持、再切回写回。

## 4. 搜索边界（验收 5）

搜索直接复用 `rankQuickFind()`（其固定边界为 title > space > host > id），手机端再**剔除 id 命中**——承诺只搜标题 / 项目 / 主机，opaque id 不泄漏到首页。e2e：

- 标题词命中（`MHOME_FIND_TITLE` → 1 行）；项目名命中（`remuda-e2e` → 2 行）；
- **只出现在交互描述/正文句里的词 `echo`（blocked 行 next-step 句「echo e2e」）必须 0 结果**；
- 只出现在 exited 行错误正文里的 `MHOME_EXIT_SENTINEL` 同样 0 结果（错误当正文≠错误进搜索索引）。

vitest 另补一条：完整实例 id 作为 query 也无结果。

## 5. 一键恢复（验收 6）

已退出行的「恢复」按钮调 `hubStore.resume(id, "structured")`（与 SessionPage 同一路径，无第二个 API）：

- 成功：`resume` 返回新实例 id → `navigate('/s/<newId>')`；e2e 用 fake node 走完整 HTTP resume（父行带 native session id，节点对通用 `instance.resume` 回 ready），断言新 id ≠ 父 id 且落在 `session-page`。
- 失败：`resume` 返回 `null`（store 已 toast Hub 的 409 原因）→ 行留在原地，显示「恢复失败，请重试」（`home-resume-error`，按钮恢复可点）。

## 6. 验证

| 检查 | 命令（锁槽 `flock …/locks/e2e.lock-c`，端口 59210/59219/59211，`PW_CHANNEL=chromium`） | 结果 |
|---|---|---|
| 单测 | `pnpm --dir web test` | 137 文件 / 1307 用例全绿（含新增 25：homeRows 23 + HomeList 2） |
| 类型 | `pnpm --dir web typecheck` | 干净 |
| lint | `pnpm --dir web lint` | 无 error（仅仓库既有 warning） |
| 本规格 ×3 | `playwright test -c playwright.hub.config.ts m-home.hub.spec.ts` | 每次 4/4 通过（约 47s） |
| 全量 hub e2e | 同上不加过滤（chromium 单浏览器 hub 配置） | 143 passed / 18 skipped / 0 failed（22.5m） |
| 密钥扫描 | `bash scripts/ci/secret-scan.sh` | pass（`no-tunnel-scan.sh` 同机亦 pass） |

全量回归备注：首次整串跑在另一 worker 同时占用本机（load ~11）时，`grok-structural.hub.spec.ts` 的实时 PTY 流用例（桌面 1440，驱动**真实** `native_hub_e2e`/fake-harness 二进制，非本任务修改的 in-process fake node；240s 预算、250ms 轮询时序）超时一次；该用例隔离重跑 2/2 通过，且后续整串（161 条）143 passed/0 failed。与本变更无关（桌面路由、真实节点、fixture prompt 不含 `mhome-*` sentinel）。

闸口提醒（计划 §(C) 公共约定 / E4）：本任务新增的是 `web/src/features/mobile/**` 与 `app/router.tsx` 的一处替换，不在 `crates/remuda/src/cmd/merge/web_e2e.rs` 的自动 `--web-e2e` 清单内；`remuda merge` 时必须**显式传 `--web-e2e`**。闸口只跑 hub 配置（chromium）；390px 几何与 DOM 即验收，webkit 不进闸（E3），本任务未声称 iOS 实测。

## 7. 与既有屏幕的关系

- `/m` 现在渲染 HomeList；PhoneShell 顶部仍保留 c-mshell 的 spaces chips / 当前 space tabs 过渡条（其归属文件 PhoneShell.tsx 本任务不得改）。HomeList 自身是全分组首页，不按该选中 space 过滤；该过渡条的收口属后续手机壳任务，不影响本任务任何断言。
- `/m/inbox` 仍是过渡 ApprovalsPage（任务 4 m-inbox 拥有）。
- 桌面 `/sessions`、共享 `/s/:id*` 路由零改动；1440 桌面行为不受影响（全量回归覆盖）。

## 8. 截图

`mobile-ui-3-home-390.png`（REMUDA_EVIDENCE=1 由本规格首条用例截取，reduced-motion）：一个项目组 `remuda-e2e · feat/workbench-g2 · 1 待处理`；置顶 blocked 行（⚠ + 标题 + 「echo e2e」+ 50% 环）；其下 exited 行（■ + 标题 + 红色错误正文 `API Error: MHOME_EXIT_SENTINEL (429)` + 恢复）。底栏收件箱角标 1。全部为合成标题与 sentinel，无真实路径 / 用户名 / 主机名。
