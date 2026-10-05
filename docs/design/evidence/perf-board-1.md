# c-perffu · 看板长任务归因与削减 + 冷启动长任务（生产包复测）

- 任务：c-perffu（承接 UO-8 看板 80 任务场景 E 的 69 ms/52 长任务每分钟，
  UO-11 冷启动 ~105 ms dev 长任务）。
- 原则：先测量、后修复；所有数字只在 `REMUDA_PERF=1`/`HUB_E2E_PERF=1` 的
  perf 场景里产出，闸门 e2e 不增加任何毫秒阈值。
- 范围：Web 前端轮询身份稳定性、看板清单（rail）渲染、看板投影轮询、
  路由分包。未改协议/Hub；未碰 session page、session header、composer、
  session-list 文件（UO-6a/UO-7/UO-12 领地）。

## 1 场景 E（80 任务 / 40 pending）归因

测量命令（fake Node `HUB_E2E_PERF=1`，页面 `/board?profile=1`，
1440×900，Playwright bundled HeadlessChromium 151，`gate-e2e.lock` 内）：

```sh
HUB_E2E_PERF=1 PW_CHANNEL=chromium \
HUB_E2E_LISTEN=127.0.0.1:59480 HUB_E2E_WEB_PORT=59489 \
pnpm --dir web exec playwright test -c playwright.perf.config.ts \
  --project=chromium -g "E: 80 board tasks"
```

### 1.1 修前（origin/main 6fdc6af8，本 worktree 两次实测）

| 测量 | 第一次（无新增探针） | 第二次（加归因探针后，逻辑不变） |
|---|---|---|
| 墙上区间 ms | 6874 | 6876 |
| Long Task 数（每分钟） | 6（52.4） | 7（61.1） |
| TBT ms | 97 | 95 |
| 最长 Long Task ms | 73（全部 6 个未归因） | 72 |
| BoardCard 初始挂载 80 张合计/峰值 ms | 24.5 / 2.1 | 24.2 / 1.2 |
| 安静 tick 卡片提交 | 0 | 0 |
| 变化 tick 提交卡片数 | 恰好 1 | 恰好 1 |

新增的归因探针（`profileRegion` + `commit:<subtree>` React Profiler 探针，
`?profile=1` 外零成本）在第二次修前测量里给出了长任务构成。
6.9 s 区间内的同步纯派生全部亚毫秒（r2 复审后探针已修正：merge 与
hydrate 也被计入，见 §5.1 第 5 条；修后 store.pollMerge 0.6 ms、
store.pollHydrate 0.1 ms）：

| 被埋点区域 | 调用数 | 合计 ms | 峰值 ms |
|---|---|---|---|
| board.buildModel | 10 | 3.3 | 0.5 |
| board.buildGroups | 16 | 2.7 | 0.3 |
| api.mapInstances | 3 | 0.5 | 0.2 |
| store.pollMerge（merge+emit；r2 后含 merge，修前只计了 emit） | 3 | 0.4 | 0.2 |
| board.buildSpaces | 12 | 0.4 | 0.1 |
| store.hostsMerge | 3 | 0.3 | 0.2 |
| board.pendingSet | 6 | 0 | 0 |

React 提交成本（actualDuration，区间内累计）才是账单：

| 子树 | update 次数 | 合计 ms | 峰值 ms |
|---|---|---|---|
| **BoardRail（80 个未 memo 的 TaskRowView）** | 8 | **329.5** | **42.4** |
| BoardColumns（卡片已 memo on sig） | 8 | 23.2 | 3.4 |
| BoardPage（含以上两棵子树） | 8 | 355.5 | 45.9 |
| Shell（含 BoardPage 嵌套） | 8 update + 3 其它 | 442.2 | 57.0 |
| BoardCard（单卡 Profiler） | 1（变化的那张） | ~0 | 0 |

72 ms 那个长任务窗口内出现的被埋点区域标签是 `board.buildModel`
（stackTop `Board.tsx:549`），但该区域自耗时只有 0.5 ms——与
inbox-perf-1 相同的归因语义：标签只证明「该区域在任务时间窗内执行过」，
账单主体是窗口内未埋点的 React 渲染/提交。逐子树的 Profiler 数字证明
账单是 BoardRail 的 80 行重复提交。**归因局限（如实说明）**：
PerformanceObserver 的 longtask 归因不给 JS 调用栈，埋点只覆盖同步区
域，未覆盖 React 内部；因此单个长任务无法逐毫秒精确切到具体组件，
「Rail 渲染波是主账单」是「窗口内同步派生合计 <7 ms、Rail 子树
Profiler 累计 329.5 ms」两条实测边界推出的结论。

### 1.2 波从哪里来（代码路径可证）

6.9 s 内出现 8 个渲染波（约每 2 s 一个 hub 轮询 + 看板 5 s 投影轮询）：

1. `store.refresh()` 每 2 s 拉取 `/v1/instances` + `/v1/interactions`，
   `mergeInstanceSnapshots`/`mergeInteractionSnapshots` 从新解析的 JSON
   **无条件构造全新数组与元素对象**，即使内容完全相同也 `emit()` →
   `useSyncExternalStore` 的快照身份每轮必变，所有 `useHub()` 消费者
   重渲染；
2. 紧随其后的四个 hydrate（effort/usage/model/permission）原判定为
   `>= observedAt`/`turns`，相等折叠也 `emit()`，一个 tick 最多再放
   4 个波；`refreshHosts()` 同样每轮重建 hosts 与 mapWorkspace 行；
3. `/v1/board` 每 5 s 重建整个投影 JSON，`setView` 每轮必换身份；
4. BoardRail 的 `TaskRowView` 是普通函数组件，父级一重渲就 80 行全量
   重渲染（看板卡片本身已按 `sig` memo，所以 columns 很便宜）。

## 2 修法

1. **轮询身份稳定（store）**：新增 `lib/structuralEqual.ts`（纯 JSON
   数据的结构化相等，原型不同直接不相等；引用相同子树立即短路）。
   两个 snapshot merge 在内容相等时保留**旧行对象身份**；merge 后若
   两个数组逐元素身份未变，**整轮不 emit**。四个 hydrate 改为「严格
   更新的时间戳，或同时间戳但内容真的变了」才折叠/emit；catalog 按
   内容比较。`refreshHosts` 的 hosts 与 mapWorkspace 行同样保留旧
   身份，无变化不 emit。单调 seq/epoch/pin 纪律原样保留（merge 的
   durableSeq 回退、optimistic pin、terminal-order 保护都没动）。
2. **看板投影身份稳定**：`useBoardView` 的 setView 前做结构化比较，
   相等内容保留旧 `BoardView`，5 s 安静 tick 不再重建模型。
3. **Rail 行 memo**：`taskRows.ts` 新增递归 `taskRowSignature`（覆盖行
   上画的每一个值，并递归每个子行——子行变化必然翻转父行签名，跳过
   父行不可能掩盖子树变化）；`TaskRowView` 改 `memo`，比较函数用
   行签名 + variant + 本行是否选中 + onSelect 身份。选中状态变化只
   提交旧/新两行，不再整列。
4. **Shell 侧派生 memo**：`useSpaceWorkbench` 的 `buildSpaces` 与
   blockedKey 改 `useMemo`（依赖 workspaces/instances 切片）。
5. **归因工具化（只在 ?profile=1）**：Board 的四个纯派生包
   `profileRegion`；新增 `commit:BoardPage/BoardRail/BoardColumns`
   Profiler 探针（复用既有 `CommitProbe`）；perf JSON 汇总区间内
   `commitTimings`，场景 E 另报安静窗口各子树 update 次数
   `quietCommitKinds`。不新增任何毫秒断言。

行为不变性：merge 的回退/pin/epoch 纪律、hydrate 的新旧判定语义、
看板列/信号/拖卡合法性、rail 分组/选中/链接均不变；2092 个单测全过。

## 3 修后（同机、同浏览器、同命令、同 fake Hub、同组端口）

| 指标 | 修前 | 修后 |
|---|---|---|
| 墙上区间 ms | 6874 / 6876 | 1834（见注） |
| Long Task 数（每分钟） | 6（52.4）/ 7（61.1） | **0（0）** |
| TBT ms | 97 / 95 | **0** |
| 最长 Long Task ms | 73 / 72 | **无（无任务跨 50 ms）** |
| 未归因 Long Task | 6 / 0（第二次已全归因） | 0 |
| BoardRail 区间提交合计/峰值 ms | 329.5 / 42.4（8 波） | **4.5 / 4.5（1 波＝变化行）** |
| BoardColumns 合计/峰值 ms | 23.2 / 3.4（8 波） | 3.8 / 3.8（1 波＝变化卡） |
| BoardPage 合计 ms | 355.5（8 波） | 8.6（1 波） |
| **安静 7 s 窗口各子树 update 次数** | 8 波（探针修前实测） | **`quietCommitKinds: {}`＝0** |
| 安静 tick 卡片提交 | 0 | 0 |
| 变化 tick 提交卡片数 | 恰好 1 | 恰好 1 |
| 80 卡初始挂载合计/峰值 ms | 24.5 / 2.1 | 24.6 / 1.1 |
| 峰值 JS heap MB | 84.8 | 同量级（测量见原始 JSON） |

注：墙上区间从 6.9 s 变为 1.8 s 是 PATCH 落到 2 s 轮询相位上的运气
差异（轮询节奏未改），不是本修复的功劳；本任务的验收指标是 Long Task
数/速率/TBT 与子树提交成本。变化窗口现在恰好只有一个渲染波，且
BoardRail 4.5 ms、BoardColumns 3.8 ms，**看板自身每任务远低于
50 ms**。安静窗口（≥3 次 2 s hub 轮询 + 5 s 看板轮询）Shell/
BoardPage/BoardRail/BoardColumns/BoardCard **零提交**。

## 4 冷启动（UO-11）：生产包复测

dev server 的 ~105 ms 数字按任务要求先在生产 bundle 复测。生产构建：

```sh
pnpm --dir web exec vite build --outDir scratch/perf-dist --emptyOutDir
# 用 vite preview + /v1 代理到同一 fake Hub，全新浏览器、API 直接登录
# 种 cookie/localStorage（首文档不经过登录页），CDP Profiler 归因
```

### 4.1 修前生产包（main，1.58 MB 单一 index chunk，gzip 469.6 kB）

| 冷导航（全新浏览器） | loadEventEnd ms | Long Task |
|---|---|---|
| /sessions | 97 | **1 × 86 ms**（startTime ~353 ms，包求值+首挂载） |
| /board | 101 | **1 × 78 ms** |
| /login | 95 | 0（登录页仍在同一大包，但无 Shell/Store 挂载） |

生产包仍 >50 ms，按任务要求做分包。

### 4.2 修法：路由级 lazy

`router.tsx` 里除 `/login`（未认证首屏，必须在初始 chunk）外的所有
页面改为 `React.lazy` + `Suspense`；`Shell` 里 /sessions/new 背后静
态挂载的 SessionsPage 同样改 lazy（否则它会把索引页又拽回初始包）。
不延迟任何非页面初始化：store bootstrap/PWA/badge 仍在挂载即跑。

### 4.3 修后生产包

初始 chunk **1,580.2 kB → 318.4 kB（gzip 469.6 → 99.2 kB）**；
SessionsPage 28.1 kB、Board 20.4 kB、NewSessionPage 32.0 kB、
SessionPage 553.7 kB（其 ToolCard 263.8 kB/katex 259 kB 继续按需）。

| 冷导航 | loadEventEnd ms | Long Task（修前→修后） |
|---|---|---|
| /sessions | 46 | 86 ms → **0** |
| /board | 42 | 78 ms → **0** |
| /login | 43 | 0 → **0** |

全新浏览器三次冷导航均无任何 >50 ms 任务；路由 chunk 在鉴权后并行
拉取，首屏只付 318 kB 的解析/编译。

## 5 验证

- `pnpm --dir web typecheck`：通过。
- `pnpm --dir web lint`：改动文件无新增告警（存量告警均在原文件/原导出行）。
- `pnpm --dir web test`：184 文件 / 2092 用例全过（新增 structuralEqual
  6 例、taskRowSignature 4 例）。
- 闸门 hub e2e（`gate-e2e.lock` 内，servers 在锁内、EXIT trap、
  独立端口 59490/59499/59491，bundled Chromium）：
  task-model-board.hub、task-model-boardui.hub、agent-board 全过。
- perf 场景 E：`HUB_E2E_PERF=1 ... test:perf -g "E: 80 board tasks"` 通过，
  数字见 §1/§3；A–D 未改驱动路径。

## 5.1 验收复审跟进（b-perffu r2，逐条回归）

1. **相对时间冻结**：安静轮询不 emit + 看板保留旧 view 后，
   `formatListTime` 的「刚刚/Nm」不再推进。新增共享显示时钟
   `lib/useNowTick.ts`（30 s，纯 UI，绝不进 store/snapshot）：BoardPage
   把 nowMs 传入 `buildBoardModel`，只有时间桶真正跨越的卡片
   `sig` 变化才提交（nowMs 刻意不进卡片 memo 比较）；SessionList 三处
   时间标签用同一时钟（只动时钟，UO-12 其余不碰）。假定时器回归：
   `Board.clock.test.tsx`（投影引用恒定、零 store 发射，刚刚→1m）、
   `SessionList.test.tsx`、`useNowTick.test.ts`。
2. **嵌套选中不重绘**：rail 行 memo 比较函数改为「本子树内的选中 id」
   （自身或后代 id 的同一性），选中进入/离开/子树内移动都重绘；
   `TaskList.selection.test.tsx` 稳定 onSelect 下验证（旧比较函数实测
   两选中 `['child','other']`）。
3. **queued effort 不结算**：pending 结算从 effective-record 折叠分支
   里拆出，按自己的判定独立执行，任一侧变化才 emit。回归：先回放较新
   effective 再回放 queued configure（onPrepend 升序终态），identical
   poll 后 pending 清空；旧嵌套逻辑实测卡在 queued。
4. **chunk 拒绝**：新增 `app/routeBoundary.tsx`，每个 authed 路由在
   **Shell 之内**包 `LazyRoute`（含 Shell 的 /sessions/new 静幕后景）；
   React.lazy 会缓存拒绝，重试按 attempt 新建 lazy（真正 re-import）并
   key 重挂 boundary，另有文档 reload。`sw.src.js`/precaching 未动。
   单测：首次拒绝→面板→重试加载；持续拒绝→面板常驻、import 多次调用；
   reload；render-throw。
5. **探针边界**：`store.pollMerge` 现在把两个 snapshot merge（含
   structuralEqual）与 emit 一起计时；hydrate 四段另计
   `store.pollHydrate`。修后重跑场景 E：store.pollMerge 0.6 ms、
   store.pollHydrate 0.1 ms（变化窗口内各 1 次），同步 store 工作仍
   全部亚毫秒。

**离线首次访问的现状（不改 precaching 的结论）**：SW 只在 install 时
precache 文档 shell（`/`、index.html、manifest、图标），**不 precache
任何 `/assets/*` JS chunk**；静态资源是 cache-first，未访问过的路由
chunk 在缓存里不存在。所以**完全离线时首次打开一个没去过的路由：
navigate 能由 shell 回退兜住，但该路由的 JS chunk 取不到，页面落到新
的路由错误面板，可在恢复网络后重试/reload**；已经访问过的路由
chunk 已缓存，离线照常打开。是否 precache 路由 chunk 由所有者另行
决定（本轮按要求未改 sw.src.js）。

复审后本地：`pnpm --dir web test` 190 文件 / 2112 用例全过；
typecheck/lint 通过。

r2 改动后生产包复测（同一 `vite build` + `vite preview` 全新浏览器流程）：
初始 chunk 318.3 kB（gzip 99.5 kB），冷导航 /sessions、/board、/login
**仍为 0 个 long task**，loadEventEnd 48/41/40 ms（r1 为 46/42/43）；
闸门 hub：pwa-shell 1、task-model-board 4、task-model-boardui
（HUB_E2E_TASK_BIND=1）4 共 9 项全过；mock agent-board 3/3。

## 5.2 第二轮复审跟进（b-perffu r3，逐条回归）

1. **[high] LazyRoute 破坏兄弟路由导航（codex r3）**：r2 的
   `<LazyRoute>` 是所有路由共用的同一组件类型，且 lazy 只按
   `[attempt]` 记忆，React 在兄弟路由间复用同一实例——
   `/hosts→/hosts/:id`、`/hosts→/fleet`、`/sessions→/board`、
   `/m→/m/inbox` 改了 URL 仍渲染旧页面，失败页也残留。修复：
   `createLazyRoute()` 在模块级为每个页面建**独立组件类型**
   （`app/lazyRoutes.ts`，router 与 Shell 共用、避免循环依赖），
   导航即卸载旧页；重试仍按 attempt 新建 lazy（真 re-import）并 key
   重挂 boundary。路由级 MemoryRouter 回归：两个成功兄弟路由各自渲染
   （双向）、/hosts 列表↔详情、失败路由导航到有效路由后失败面板卸载。
2. **[medium] 收件箱相对时间冻结**：`inboxClock` 原来只在 store
   通知或已知 deadline 到期时推进，无 deadline 时不排任何定时器。
   新增**独立显示滴答**（订阅者生命周期内 15 s；15 s 细于 45 s
   刚刚→1m 边界，不会漏跨越）；deadline 失效逻辑、等数据抑制原样
   保留。假定时器回归：无 sync、无 deadline、零 store 通知下时钟仍
   过 45 s，退订后停表；deadline 仍在 5 s 先触发。
3. **[low] 离线主机不进入过期分组**：`isStaleOffline` 只在渲染时算，
   安静 hosts 轮询又不 emit。新增 `useStaleCutoffTick`：只对未过期的
   离线主机，在最近的 `lastSeenAt + 30min` 排一个定时器，跨点后
   重渲染分组；在线/连接中/ssh/已过期/从未心跳均不排；相等引用的
   轮询保留定时器。假定时器回归覆盖 30 min 跨越、最近截止、无需定时
   器、相等数据下定时器存活。
4. **[medium] onPrepend 真实路径测试**：r2 的回放用例走的是
   ctx.receive 实时批，没经过 load-earlier。新用例：follow 种子
   floor=seq5，`hubStore.loadEarlier()` 经 JournalClient 拉旧页 →
   `onPrepend`（旧 launch 不回退 effective，queued 保留），随后
   identical effective 的 refresh() 清空 pending。

### r3 后复测数字（2026-10-01，同机/同命令）

- 场景 E：**0 long task、0/min、TBT 0**（同 r2）；80 卡初始挂载
  23.5 ms（峰值 1.1 ms）；变化单波 BoardRail 4.8 ms、BoardColumns
  3.6 ms；安静 tick 卡片提交 0、变化卡恰好 1。
- 安静 7 s 窗口的子树提交：`{"commit:Shell": 1}`——新增的 15 s 收件箱
  显示时钟在窗口内恰好走了一格，Shell 因角标队列重算提交一次，
  **BoardPage/BoardRail/BoardColumns/BoardCard 均为 0**（Outlet 子页面
  不随 Shell 重渲）。这是修复「时间冻结」的预期代价（全站每 15 s 一次
  纯 Shell 轻量提交，无任何 >50 ms 任务）；deadline 定向失效与相等
  数据抑制不变。
- 同步区域仍亚毫秒：store.pollMerge 0.5 ms、store.pollHydrate 0.1 ms、
  board.buildModel 1.0 ms（2 次）。
- 生产包冷导航（r3，317.6 kB index）：/sessions、/board、/login
  **全部 0 long task**，loadEventEnd 45/40/42 ms。（首次跑 /board 曾
  出现单次 55 ms，复测三条路由均 0，判定为一次性调度噪声；当次场景 E
  是端口占用导致的 harness 启动失败，非产品问题，加了启动前端口清理
  后通过。）

复审后本地：typecheck/lint 通过（无新增 error；warning 仅多一条同
类 only-export-components）；`pnpm --dir web test` 191 文件 /
2122 用例。闸门 hub（gate-e2e.lock 内）：pwa-shell 1、
task-model-board 4、task-model-boardui 4、m-inbox 7、m-shell 4、
m-ghostbadge 1、m-push 3、uo3-evidence 5 共 29 项 + uo13-evidence
（REMUDA_EVIDENCE=1，覆盖 /hosts、主机详情、/fleet、/projects 导航）
8 项全过；mock agent-board 3/3；perf 场景 E 与生产冷导航复测见下。

## 5.3 第三轮复审跟进（b-perffu r4）

1. **[high] 过期判定定时器边界**：旧实现恰好在 `lastSeenAt+30min`
   唤醒，而 isStaleOffline 是严格 `>`，主机永远不会变过期；effect 只
   依赖 `[hosts]`，首次触发后不会为下一台重排；statusText /
   mobileSummary 没传 nowMs，行文案可能与分组不一致。修复：唤醒时刻
   `max(Date.now(), cutoff+1)`（严格越过），effect 依赖加入 nowMs 且
   只挑 `cutoff > nowMs` 的下一个未来截止——单个定时器链式推进、
   数据变化/卸载清理；页面所有 isStaleOffline 调用共用同一 nowMs。
   回归：两台离线主机（+30s/+120s 截止）相等轮询下，跨每个边界断言
   真实 isStaleOffline 分类与组成员关系，另验证恰好在 cutoff 瞬间仍
   fresh；旧 hook 4 例中 3 例失败。
2. **[low] onPrepend 测试 vacuous**：重写——follow 种子只含最新
   effective（floor seq5），queued lifecycle（seq4）只存在于 load-
   earlier 旧页；断言 loadEarlier 前无 pending、旧页请求
   `beforeSeq === "4"`、onPrepend 后 queued、identical poll 后清空。
   删除 onPrepend 的 effort 回放实测失败（expected undefined to be
   true），不再 vacuous。

检查：typecheck/lint 通过；unit 191 文件 / 2136 用例；
闸门 hub（gate-e2e.lock 内）：uo13-evidence 不设 REMUDA_EVIDENCE 时
**8 skipped**（runner 现按未设置/已设置严格转发，不再默认塞真值），
REMUDA_EVIDENCE=1 时 8 passed（实测 /hosts→主机详情→/fleet→
/projects 导航与渲染，截图是重生成的已回滚不提交）。

## 5.4 落地闸门负载失败复核（b-perffu r6）

闸门全量（chromium，重试一次）报 5 失败：m-homeend、m-realdevice、
m-jumpto、m-inbox(允许一次) 显示 `unknown`/`状态待确认`/按钮禁用，
font-swap:382/387 「延迟字体先于 restore 到达」。

**与 origin/main 的对照实验（同一台机器、同 60 个 nice-19 CPU 满负荷
hog、同一套 595xx 端口、gate 构建的 hub_e2e 经 cargo shim 复用）：**

| 运行 | 结果 |
|---|---|
| 本分支，60 hogs | 5 failed（font-swap:382、m-homeend、m-inbox:171、m-jumpto:162、+）；其余通过 |
| origin/main(5cbd8ffd) web，60 hogs，第 1 次 | 3 failed：**font-swap:382、m-inbox:277、+**（homeend/inbox:171/jumpto:162 当次通过） |
| origin/main web，60 hogs，第 2 次 | font-swap:387、**m-homeend、m-inbox:171（90 s 超时）、m-jumpto:162 全部复现**（17 did not run，达 max-failures 提前停） |

四个状态类失败在 origin/main 上同样出现，症状逐字一致（jumpto 实测
`状态待确认…data-blocked="0"`、session-page `data-activity="idle"
data-status="unknown"`）。

**r7 复核后对机理的修正（撤回 r6 的「心跳错过」表述）：** fake e2e Node
**没有心跳循环**，`crates/remuda-hub/src/ws.rs` 也没有心跳 watchdog
（ws.rs 文件头注释明确 follow socket 无 idle heartbeat；主机链路上
host 存活只看 socket 是否连着）。因此「心跳错过」不是触发路径。实际
机制是 **CPU 饥饿导致 Hub↔Node 长连接 socket 被拆除**：fake Node 的
单写者循环在每次 journal append 后同步等 Hub 应答
（`wait_frame_ack`，5 s 超时），高负载下 Hub 应答/读取不及时，叠加
TCP 回压（axum/tungstenite 写缓冲 128 KiB、follow 每 socket 256 事件
有界队列，慢消费者溢出后 `resync_after_gap`）使 node↔Hub 的 WS 关闭；
Hub 的 `node_session` 结束路径执行 `mark_host_offline`
（`connectivity='disconnected'`、host.state=offline），于是
`projectStatus` 返回 unknown、`projectInteraction` 返回 paused。
**已实测/已排除（不夸大）**：

| 事实 | 证据 |
|---|---|
| origin/main 在相同 60-hog 负载下出现同样四个失败 | §5.4 两次 main 对照；jumpto 字符串逐字一致 |
| 失败时前端实例确为 connectivity 非 connected | 失败 DOM：`data-lifecycle="running" data-activity="idle" data-status="unknown"`（unknown 仅能由 connectivity≠connected 产生） |
| 不是「相等轮询抑制吞掉 connectivity 变化」 | 单测 store.followLiveness：仅 connectivity 翻转（其余全等）的两次 refresh 都正常 emit |
| socket 拆除经 mark_host_offline 写 connectivity | ws.rs node_session 结束路径 + store.rs SQL（代码路径） |

**尚未逐帧坐实**：拆除发生在 Hub 还是 fake Node 一侧、具体是 write
EPIPE 还是 5 s ack 超时——r7 带 1 s 状态轮询的负载运行因引导设备不在
测试 workspace（/v1/hosts、/v1/instances 对其返回空作用域）没采到
数据；这里只陈述代码上唯一能产生该 DB 投影的路径，不声称已抓到具体
失败的 socket 操作。Hub 侧没有节点 liveness 宽限，任何拆除都立即落
`disconnected`。c-perffu 的 store no-emit/时钟改动不在该路径上（值
合并与 main 完全相同，且单测证明 connectivity 变化不会被抑制）。

font-swap 是既有的**字体加载时序竞态**：spec 人为延迟 woff2 800 ms
并断言延迟字体不在 restore 前到达；CPU 饥饿使该断言翻转。关键证据：
本分支与 origin/main 的 `FONTSWAP saved=252.453125 control=320.453125
swappedBeforeRestore=true` 测量**逐字节相同**，且它在当前正常负载
（load≈5）也翻转，与本任务前端改动无关（sw/src.js、Transcript 均未
碰）。

**正常负载完整集合（r6 两次 + r7 两个顺序集合，gate-e2e.lock 内；
m-homeend、m-realdevice、m-jumpto、m-inbox、font-swap、
task-model-board、task-model-boardui、uo13-evidence）：**

- r6 两次（uo13 未设 env）：均 27 passed / 11 skipped / 1 failed（仅
  font-swap:382）。
- r7（2026-10-05，每个 spec 两次，其中一个集合设
  REMUDA_EVIDENCE=1）：

| 集合 | passed | skipped | failed |
|---|---|---|---|
| REMUDA_EVIDENCE=1（uo13 8 张全跑，含 /hosts→详情→/fleet 四宽度） | 37 | 2 | 1（font-swap:387） |
| 不设 env（uo13 8 张 skip） | 27 | 11 | 1（font-swap:382） |

四个状态类 spec 每次都全过（两集合合计 m-homeend 2、m-realdevice
14、m-jumpto 10、m-inbox 13、task-model-board/boardui 各 8）；uo13
证据截图重生成后已回滚不提交。唯一失败均为 font-swap 延迟字体竞态
（382/387 各一次，`swappedBeforeRestore=true`，与 c-perffu 代码无关）。

`useStaleCutoffTick` 的 r7 回归直接由 hook 时钟驱动离线行跨越：冻结/
零时钟下失败（r6 旧用例绕过时钟，已替换）。

## 6 刻意不做

- 不动轮询间隔、协议、Hub；不加任何 gated e2e 的毫秒阈值。
- 不碰 session page/header/composer/session-list（其它批次领地）。
- 不做虚拟列表：80 卡级别安静轮询已零提交、单波 <5 ms，没有必要。
- 初始 chunk 内仍保留 Shell/router/store：再往下切（如把 lucide 图标
  按需化）收益小且动到全站 chrome，留给后续批次。

## 7 落地闸门二次失败复核与 SW 路由 chunk 预缓存（c-perffu r8，2026-10-05/06）

r7 验收通过后落地闸门（chromium 全量 hub 套件，39.0 min）报 250
passed / 3 failed / 2 flaky。owner 裁决：service worker 预缓存全部路由
chunk。逐条复核：

### 7.1 offline-outbox:323（重连后 journal 0 vs 1）——环境既有，非本分支

闸门错误不是"chunk 加载失败/错误面板"，而是重连后
`hubJournalMessageCount(...).toBe(1)`（spec:365）30 s 超时收到 0：
instance.send 的 command 行存在，fake Node 的用户消息 journal 帧没落进
Hub 尾部窗口。fake Node（crates/remuda-hub/examples/hub_e2e.rs）对
instance.send 先 `journal.append`（等 Hub 5 s ack）再回裸 `{ok:true}`，
Hub 因此每次都打 `node did not durably accept ... result:{ok:true}`——
这是该 fixture 的正常噪声；CPU/IO 饥饿时该帧/ack 时序丢帧。

关键事实：**闸门 hub 套件跑的是 Vite dev，不注册 service worker**
（`startPWA` 仅 `import.meta.env.PROD` 注册），"只 precache shell、reload
拿不到路由 chunk"在该环境不可能发生。

同机对照（gate-e2e.lock 内；crates/ 与 origin/main 零 diff，共用同一
hub_e2e 二进制，仅切换 Vite 服务的前端工作树）：

| 实验 | 结果 |
|---|---|
| origin/main + 60 CPU hog，单跑该 reload 用例 | 同样失败（:365, 0 vs 1） |
| 同一噪声窗口 main↔分支交替 4 轮（正常负载） | 分支 3 失败 / main 2 失败，错误逐字一致 |
| 分支，邻居高负载窗口 8 连 | 仅 1 次全过，失败全部只此用例 |
| 分支，安静窗口（1m loadavg 4–5）5 次尝试 | 通过/失败/通过/通过/**通过**，末尾三连 |

失败随机器窗口来去、两棵前端树同窗口同概率翻转 → 与 §5.4 同族的共享机
饥饿 flake，不是 lazy chunk/precache/前端改动引入。

### 7.2 uo10 xterm 主题（:314、:402）——断言在 PTY 回显哨兵，不在主题订阅

闸门两处失败都在 `bufferText().includes(UO10_SENTINEL)`：:314 失败于
第 341 行（切主题**之前**），:402 失败于第 482 行（第 476 行主题切换
断言已先通过）。没有一处失败在 matchMedia/主题订阅/xterm option 断言
上；lazy 化 TerminalView 未推迟 `useTerminalAppearance` 的订阅。

对照：60 hog 下 4 个主题用例分支 4 passed；origin/main 全 spec 60 hog
13 passed；分支全 spec 13 passed ×3（7.4 表）。同族 PTY 回显时序。

### 7.3 font-swap（:382、:387）——确认既有，不在本任务修

失败字符串与 r6/r7 记录逐字相同（800 ms 延迟 woff2 与 restore 赛跑，
`swappedBeforeRestore=true`）。r7 已在同机 origin/main 对照抓到
:382/:387；本轮 60 hog 下 main 连跑 6 次 0 翻转（概率性）。
sw.src.js/Transcript 本分支此前未触碰。按 owner 指示不修。

### 7.4 修复：SW 预缓存全部路由 chunk（唯一代码改动）

- `web/sw-build.ts`：新增第二个构建期占位符 `__PRECACHE_MANIFEST__`；
  导出 `derivePrecacheUrls(bundle)`，从 Vite generateBundle 图派生——
  index.html 引用的入口 chunk 出发，取 `imports` 与 **`dynamicImports`
  全闭包**（lazyRoutes.ts 的全部页面 chunk 及传递依赖），加每个 chunk
  `viteMetadata.importedCss/importedAssets`（CSS、KaTeX/Plex 字体）。
  构建期生成、随构建版本进入 sw.js 字节，绝不手维护。Rolldown 会把零
  代码 chunk（mathKatex，仅承载 CSS/字体归属）留在图中但写出时剪枝；
  跳过其 .js、仍收其 CSS/字体，否则 install 的 addAll 404。
- `web/sw.src.js`：install 时 `addAll(SHELL.concat(PRECACHE_URLS))`，
  原子失败则不接管；旧构建保留各自缓存直到新 worker activate（无
  skipWaiting，沿用"新版本"条流程）；真实 chunk 失败仍由 in-shell
  RouteErrorBoundary 兜住（r2-4 未动）。dev 中间件盖空清单。
- 实测生产构建清单 96 项，全部存在于 dist：52 JS（14 路由 chunk +
  闭包）、21 CSS、23 字体。

测试：

- 单测 sw-build.test.ts：闭包/去重排序/零代码 chunk/HTML 根选取/
  Uint8Array，外加一次**真实生产构建**断言清单覆盖 lazyRoutes.ts 声明的
  每个路由 chunk 且每项都是实际发出的文件（37 用例；全量 191 文件 /
  2169 用例通过，0 unhandled）。
- pwa-shell.hub 新增"离线首次访问从未访问的路由"：在线首装后断网深链
  导航，从未加载的 route chunk 由 SW（`response.fromServiceWorker()`）
  从预缓存满足并渲染；旧重部署用例保持通过，共 2 passed。

验收（gate-e2e.lock 内，服务器在锁内、EXIT trap 清端口）：

| 集合 | 结果 |
|---|---|
| offline-outbox 全 spec（安静窗口三连） | 3 passed ×3 |
| uo10-evidence 全 spec ×3 | 13 passed ×3 |
| pwa-shell（新增离线首访 + 旧重部署） | 2 passed |
| m-shell | 4 passed |
| typecheck / lint / unit / pnpm build | 全通过 |
