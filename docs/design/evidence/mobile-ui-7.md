# 手机优先 UI · 任务 7：`m-jumpto` — 分组版 Jump To（项目 + branch + blocked 计数 + 时钟排序）

2026-09-21 · `wt/c-mjumpto/b-mjumpto-md` · mobile-ui 实施计划 §(C) 任务 7（m-jumpto）
规格权威：[ui-spec.md](../ui-spec.md) §1.3 / §2.1 / §4.7，[decisions.md](../decisions.md) D-038 / D-049；基线 `origin/main` @ 5e11f1eb（c-mkeybar 61257447、c-mhome 已落地）。

截图：`mobile-ui-7-jumpto-390.png`（390×844，终端段九键条打开的分组 sheet）、`mobile-ui-7-jumpto-1440.png`（1440×900，⌘K 扁平列表回归）。均来自 in-process fake node 的合成数据；无真实主机名 / 用户名 / 家目录路径。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/features/search/quickFindSearch.ts` | 新增纯函数 `groupQuickFind(hits, spaces, workspaces, order)`（紧邻 `rankQuickFind`，后者排序与作用域一字未动）；新增 `QuickFindOrder`、`QUICKFIND_ORDER_KEY`、`readQuickFindOrder()` / `writeQuickFindOrder()` 与 `QuickFindGroup` / `QuickFindGroupSpace` / `QuickFindBranchWorkspace` 类型 |
| `web/src/features/search/quickFind.test.ts` | 新增 `groupQuickFind` 覆盖：分组键与组头字段、搜索时整组 blocked 计数不重算、空态、时钟（组按最近叶子 + blocked 置顶）、列表（项目名→标题）、Other Space 恒末、排序持久化与存储抛错兜底 |
| `web/src/features/search/QuickFind.tsx` | `openQuickFind({ grouped: true })` 入口参数；compact + grouped 时渲染分组（项目 + branch + blocked 组头 + 时钟/列表切换）；复用 HomeList 同款 `fetchChanges` 分支注水；桌面 ⌘K 与普通触发器仍为扁平 listbox；combobox/listbox/aria-activedescendant 合同不变 |
| `web/src/features/search/QuickFind.test.tsx` | 新增分组组件测试：组头/计数/叶子仍为 option、排序持久化、桌面与普通入口不分组 |
| `web/src/features/search/quickfind.module.css` | 分组组头（sticky、项目名截断、mono branch、blocked 红色/置灰）与时钟/列表切换；手机段切换按钮 44px 热区 |
| `web/src/features/session/tty/PhoneKeyBar.tsx` | 唯一的新调用点：跳转键改调 `openQuickFind({ grouped: true })`（先点 `spaces-drawer-open` 挂载面板的既有机制不变） |
| `web/tests/e2e/m-jumpto.hub.spec.ts`（新） | hub 规格：390 从九键条打开分组 sheet（组头/branch/blocked 计数、叶子仍是会话、正文词无结果、时钟/列表切换并持久化、Enter 跳转）；1440 ⌘K 扁平列表回归 |

**只读复用，未改一行**：`features/spaces/store.ts` 的 `buildSpaces()`（Space 与 `blockedCount` 唯一来源；既有导出签名未动）、`types/workspace.ts` 的 `Workspace.branch`、`features/files/filesApi.ts` 的 `fetchChanges()`（实时 git 分支）。

**homeRows 规则的处理**：分组排序规则（blocked 置顶、时钟=最近 `updatedAt`、列表=项目名→标题、Other Space 恒末）与 `homeRows.ts` 的 `compareRows` / `compareGroups` **镜像一致但未抽公共函数**——两处的行形状不同（`HomeRow` 带 body/contextPct/canResume 等一堆首页字段），抽 helper 必须改 `homeRows` 的入参形状或引入新泛型层，超出「不改 homeRows 行为」的约束。`groupQuickFind` 的 doc comment 明确写了「keep the two in step」。

未触碰：`SessionList.tsx`、`SessionPage.tsx`、`Composer.ts`、`spaces/store.ts`、`playwright.hub.config.ts`、`sw.src.js`、`deploy/`。`PhoneKeyBar.tsx` 只改跳转键一处实参与注释。

## 2. 分组与计数（验收 1 / 5）

- **分组键 = Space = (host, workspace)**：`groupQuickFind` 不重建空间模型，只是把 `rankQuickFind()` 已经产出的 `QuickFindHit[]`（hit 上带 `spaceId`）按 `spaceId` 归桶；组头项目名取 hit 的 `spaceName`（即 `buildSpaces()` 的 Space 名），host 取 hit 的 `hostName`。没有任何 pane 层级——组的叶子**就是会话 option**，仍是同一个 `role="listbox"` 里的 `role="option"`（验收 5，ui-spec §1.3「v1 不做多 pane 分屏」）。
- **blocked 计数恒等于 `buildSpaces()` 的 `blockedCount`**：计数从 `spaces` 参数按 space id 读，**不从当前可见 hit 现算**。所以搜索把某个 blocked 会话过滤掉时，组头仍报该 Space 的完整待处理数（与 homeRows 同一规则；vitest「keeps the whole-Space blocked count while a query hides leaves」覆盖）。e2e 用脚本化 `mhome-blocked` approval gate 造出唯一 blocked 会话，断言 remuda-e2e 组头 `1 待处理` 与 `data-blocked="1"`，remuda-e2e-second 组 `0 待处理`。
- **branch**：优先用 Hub 注册 workspace 自带的 `Workspace.branch`（最小 Node 注册回复里没有），组件打开分组 sheet 时再用与 HomeList 完全相同的 `fetchChanges()` 只读 SCM 代理按 Space id 注水，known 才上组头，unknown/denied/失败只留项目名。fake node 对两个 workspace 的 `workspace.scm.status` 都回 `feat/workbench-g2`（默认臂），e2e 对两组都断言该分支。
- **Other Space**：未注册 workspace 的实例归入 `OTHER_SPACE`，时钟与列表两种排序都恒排最后（vitest 双断言）。

## 3. 两种排序与持久化（验收 2）

- **时钟（默认）**：组按组内最近状态变化时间倒序，组内叶子同样按最近状态变化倒序；时间取 `instance.updatedAt`——web `Instance` 类型没有独立的 `lastEventAt` 字段，`updatedAt` 就是 rankQuickFind 一直使用的状态变化时间戳（排序键与扁平列表的 recency 完全一致）。
- **列表**：组按项目名 `localeCompare`（id 兜底），组内按标题（id 兜底）。
- **blocked 置顶**：ui-spec §2.1 待处理置顶在两种排序下都成立，且优先级高于时钟——被阻塞的叶子即使最旧也在组顶（vitest 时钟/列表各一例）。
- **持久化**：`localStorage["remuda.mobile.quickfind.order.v1"]`，与手机首页的 `remuda.mobile.home.order.v1` 是**两个独立键**（桌面 ⌘K 用户不会被手机上的切换污染，反之亦然）；存储不可用/抛错时回落时钟且不炸（vitest 覆盖）。e2e 断言切换写键、reload + 重开后保持、切回时钟写回。

## 4. 搜索作用域与 a11y 不回归（验收 3 / 4）

- **搜索仍只走 `rankQuickFind`**：分组是对 `rankQuickFind()` 返回 hits 的纯展示变换，匹配字段仍是 标题 > 空间 > 主机 > id，正文从不参与。e2e 用只存在于 blocked 会话 transcript/下一步句里的词 `echo` 搜索断言 0 结果 + empty 态，再用项目名 `remuda-e2e` 断言有结果；`cacheOnly` 提示分支未动（组件测试既有覆盖）。
- **桌面 ⌘K 零行为变化**：是否分组 = `open && openQuickFind({grouped}) 的请求 && mobile`。桌面视图下即使调用方传 `{grouped:true}` 也强制扁平（组件测试 + 1440 e2e 双覆盖）；⌘K 全局快捷键本身不传 grouped。
- **a11y 合同保留**：分组后 DOM 仍是一个 `role="listbox"`，叶子仍是带 `id="quickfind-option-<n>"` 的 `role="option"`，input 的 `role="combobox"` / `aria-controls` / `aria-activedescendant` 与扁平模式同一套；键盘 `ArrowDown/Up` 跨组循环（index 取全局 `hits` 的下标，组内 option 的 `data-index`/id 与之对应），Enter 打开全局 active 命中。既有 QuickFind.test.tsx 全部用例（含 ⌘K、xterm 不抢键、Escape 还焦点、zero-match 恢复）保持绿色。
- **手机入口**：九键条第 5 键「跳转」沿用 c-mkeybar 的既有机制——先程序化点击 `spaces-drawer-open` 把内联（不 portal）的 QuickFind 面板挂载进抽屉，再调**导出的** `openQuickFind({ grouped: true })`；状态与 UI 没有第二份拷贝。

## 5. 测试与验证

- vitest：`quickFind.test.ts`（ranker 既有用例不动 + grouper 新增）、`QuickFind.test.tsx`（既有用例不动 + 3 个分组用例）；全量 `pnpm --dir web test` 149 文件 1465 用例全绿、`typecheck` 无错误、`lint` 退出 0（仅全库既有的 fast-refresh/refs warning，无新增）。
- hub e2e `m-jumpto.hub.spec.ts`：390（hasTouch、宽度驱动 compact，同 m-keybar 口径）4 条 + 1440 回归 1 条。本机无 Google Chrome，按 hub 配置的既有开关用 `PW_CHANNEL=chromium` 走 Playwright 自带 Chromium（与 CI 同一浏览器分支）。
- 本规格连续运行 3 次结果：**3 × 5 passed**（59.8s / 59.2s / 1.1m，2026-09-21）。调试期曾出现一次 1440 用例的轮询时序失败（⌘K 打开瞬间 store 还没加载完第二个实例，光标在单条上回绕），修复为打开后显式等待 `quickfind-result` 计数到 2 再断言 ArrowDown，之后连续 3 次全绿。
- 回归：`m-keybar.hub.spec.ts`、`ux-keys.hub.spec.ts` 合并 1 次 **9 passed**；全量 web hub e2e 套件 1 次 **170 passed / 20 skipped / 0 failed / 0 flaky**（26.2m；skipped 为既有的环境/通道门控用例，与本改动无关）（锁槽 `flock …/locks/e2e.lock-b`，端口 59250/59259/59251）。
- `bash scripts/ci/secret-scan.sh`：**pass**（退出 0）。

## 6. 显式不做（ui-spec §4.7 / 报告 §10-23 / §11.2）

- 不做第二套空间模型：不新增 store、不新增路由、不复制 SpacesPanel；分组只是 ranker 输出的一层视图。
- 不做 pane 层级：一实例一终端，组的叶子就是会话。
- 不搜索正文/历史消息：作用域与 rankQuickFind 逐字一致。
- 不改桌面 ⌘K 的扁平形态与任何既有 testid。
