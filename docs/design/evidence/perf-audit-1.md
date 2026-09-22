# perf-audit-1 · 前端卡顿诊断：埋点与可复现场景的第一组数字

- 任务：c-perfaudit（桌面计划 M1；`briefs/desktop-app.md` §c-perf-audit）
- 范围：**只测量，不修任何性能问题**。本提交新增埋点
  （`web/src/lib/profileFlags.ts`、
  `web/src/features/session/tty/rendererProbe.ts`）、可复现的 Playwright
  场景（`web/tests/perf/scenarios.perf.ts`）与 fake Node 的 perf 触发分支
  （`crates/remuda-hub/examples/hub_e2e.rs`，仅 `HUB_E2E_PERF=1` 生效）。
- 分工：本表 Linux/Chromium 由本任务产出；**macOS 的 Chromium 与 WebKit
  两列留空，由协调员用同一份驱动在所有者机器上补**（§4）。

## 1 测量方法（可复现）

### 1.1 埋点

- 页面 URL 带 `?profile=1` 时，`profileFlags` 才实例化
  `PerformanceObserver({ entryTypes: ["longtask"] })` 并挂
  `window.__remudaPerf`；默认关闭时不构造 observer、不挂 window、
  `profileRegion()` 只是一次直接函数调用（有单测钉死：默认关闭时
  `profilingEnabled === false`、observer 构造器零调用、无 window API）。
- 被计时的三个嫌疑区域（区域计时**无论是否超过 50ms 都汇总**，Long Task
  归因只覆盖跨阈值的任务）：
  - `transcript.assemble` —— `Transcript.tsx` 的 `assembleTranscript` +
    `compactTranscript` 重算（内含协调员指出的
    `assemble.ts:467` `nodes.some(...)` 平方查重）；
  - `transcript.visibleRange` —— 每次 scrollTop 变化的
    `virtualWindow.visibleRange`（`virtualWindow.ts:18-66`，两个长度 n
    数组的线性分配）；
  - `tty.onFrame` / `tty.termWrite` —— 终端每块输出到达时的
    `setPreview`/`setRawTail`（拼接+截 4000 字符、逐字节
    `Array.from(...).join("")`）与 xterm `term.write` 的解析派发。
- Long Task 归因：浏览器不给 Long Task 附带 JS 栈，因此「栈顶 file:line」
  来自该任务时间窗内执行的被埋点区域（捕获点是埋点调用处，来源 URL
  去掉 origin，报告中不含主机名/端口）。覆盖不到任何区域的 Long Task
  **如实记为 `(unattributed)`，不猜**。
  - 注意：行号取自 Vite dev server 实际下发的**转译后模块**（React 插件
    会把多行 `useMemo(() => …, deps)` 压成一行），所以
    `Transcript.tsx:211` 对应源码里 `assembled` 的那个 useMemo（磁盘上
    在约 251-259 行）；功能定位不受影响，协调员在 Mac 上跑同一份 dev
    服务，行号口径一致。
- `rendererProbe` 把终端**实际生效**的 renderer（webgl / canvas / dom）
  与 WebGL context-loss 事件上报到同一通道（选择发生在
  `tty/renderer.ts` attach 完成时；context loss 在
  `WebglAddon.onContextLoss` 回调里）。

### 1.2 场景与驱动

独立目录 `web/tests/perf/`、独立配置 `web/playwright.perf.config.ts`、
独立命令（**不进任何闸门 e2e 全量**）：

```sh
# 一次性安装浏览器（Linux 上只需 chromium；macOS 对比另需 webkit）
pnpm --dir web exec playwright install chromium webkit

# Linux / macOS 通用，引擎由 --project 决定，引擎名取自 Playwright browserName
HUB_E2E_PERF=1 pnpm --dir web exec playwright test -c web/playwright.perf.config.ts --project=chromium
HUB_E2E_PERF=1 pnpm --dir web exec playwright test -c web/playwright.perf.config.ts --project=webkit
# 或直接：HUB_E2E_PERF=1 pnpm --dir web test:perf（默认跑配置内全部 project）
```

- fake Node 只有在 `HUB_E2E_PERF=1` 启动时才识别 perf 哨兵；缺触发时
  三个场景 `test.skip`（不是静默降级成普通 echo 路径）。
- 三场景（指标下限来自任务书；数字为**下限保证**，真实渲染负载更高）：
  - **A 长 transcript 流式**：2000 个 journal 事件 / 600 个工具调用，
    10 事件/帧、20 帧/秒（≥20 批/秒）持续推入；跟随页提前挂载走 live
    follow，期间 transcript 滚动条每 120ms 顶↔底往复。
  - **B 终端洪水**：5000 行输出，25 行/帧、30 帧/秒写入
    （`__perf_tty__:<lines>:<perFrame>:<fps>` 走真实 `tty.frame`
    WebSocket 通道），落完后在终端上 3 轮滚轮上下翻页（xterm
    scrollback=4000）。
  - **C interaction 泛滥**：100 张 pending 审批卡
    （`__perf_interactions__:100`），打开审批中心后在其滚动容器内
    4 轮顶↔底滚动，跨过多轮 2s `interaction.list` 轮询。
- 每场景输出 JSON 到 `web/tests/perf/results/perf-<engine>-<ts>.json`
  （该目录 gitignore，数字手抄入本表）：
  Long Task 次数、折算次数/分钟、最长 Long Task（ms + 归因区域 +
  捕获点 `file:line`）、总阻塞时间 TBT（Σ(duration−50ms)）、
  未归因 Long Task 数、区域直方图、区域计时（调用数/总 ms/峰值 ms，
  含未跨阈值的开销）、峰值 JS heap（Chromium `performance.memory`，
  WebKit 无此 API 时如实为 null）、实际终端 renderer、context-loss 次数。

## 2 Linux / Chromium 结果（本任务实测）

### 2.0 测量条件（重要：这是「有负载的相对值」，不是干净基线）

- 硬件/系统：共享 Linux 构建主机（64 核）。
- 测量时刻（2026-09-23 04:22 本地）机器**正被合入闸门占满**：
  `load average 52.55 / 57.38 / 54.58`，另有 **5 个并行 cargo
  build/test 闸门进程**（来自其他 worktree 的在跑任务）；本测量进程被
  调度器调到 **nice 10**（闸门优先）。
- 浏览器：Playwright bundled **HeadlessChromium 151.0.0.0**（Linux，
  UA 自述 HeadlessChrome；软件 GL 环境，WebGL 仍由浏览器内置路径生效，
  见场景 B），视口 1440×900。
- 服务：Vite dev（非生产构建）+ 仓内 fake Hub/fake Node（debug 构建）。
- **解释口径**：这些数字是高负载下的相对基线，绝对 Long Task/TBT
  大概率**偏高**（CPU 被闸门抢占会放大同步任务时长）；但任务的归因标签、
  区域间相对关系（谁的 long task 最多、哪个区域自耗时多大）在同机同载
  下成立。干净的绝对值与引擎对比以 §4 协调员在所有者 Mac 上的复测为准。

### 场景 A · 长 transcript 流式（2000 事件 / 600 工具，20 批/秒 + 滚动）

| 指标 | 值 |
|---|---|
| 墙上时长（markScenario 区间，含命令往返与 1.5s 收尾） | 12.1 s（折算推流 ≈19–20 批/秒） |
| Long Task 数 / 每分钟 | 52 / 257 |
| 总阻塞时间 TBT | 711 ms |
| 最长 Long Task（归因 / file:line） | 96 ms · `transcript.assemble` · `Transcript.tsx:211`（dev 转译模块行号，见 §1.1） |
| 未归因 Long Task | 0 |
| Long Task 归因直方图 | `transcript.assemble` 49 · `transcript.visibleRange` 3 |
| `transcript.assemble` 调用数 / 总 ms / 峰值 ms | 462 / 1237 ms / **7.4 ms** |
| `transcript.visibleRange` 调用数 / 总 ms / 峰值 ms | 960 / 8.8 ms / **0.2 ms** |
| 峰值 JS heap | 128.7 MB |
| 实际终端 renderer | n/a |

### 场景 B · 终端洪水（5000 行，30 帧/秒 + 翻页）

| 指标 | 值 |
|---|---|
| 墙上时长 | 13.5 s |
| Long Task 数 / 每分钟 | **0** / 0 |
| 总阻塞时间 TBT | **0 ms** |
| 最长 Long Task | 无 |
| 未归因 Long Task | 0 |
| `tty.onFrame` 调用数 / 总 ms / 峰值 ms | 201 / 27.8 ms / **0.6 ms** |
| `tty.termWrite`（xterm 解析派发）调用数 / 总 ms / 峰值 ms | 194 / 2.5 ms / 0.1 ms |
| 峰值 JS heap | 96.4 MB |
| 实际终端 renderer / context-loss | **webgl** / 0 |

### 场景 C · interaction 泛滥（100 pending + 收件箱滚动）

| 指标 | 值 |
|---|---|
| 墙上时长 | 14.1 s（跨多轮 2s 轮询） |
| Long Task 数 / 每分钟 | 13 / 55 |
| 总阻塞时间 TBT | **9249 ms** |
| 最长 Long Task（归因标签 / file:line） | 2723 ms · `approvals.deriveRows` · `ApprovalsPage.tsx:48` |
| 未归因 Long Task | 4（另有 9 个带 deriveRows 标签） |
| `approvals.deriveRows` 调用数 / 总 ms / 峰值 ms | 18 / 48.3 ms / **46.5 ms** |
| 峰值 JS heap | 104.6 MB |
| 实际终端 renderer | n/a |

> **归因语义（读这张表必须知道）**：Long Task 不附带 JS 栈，标签是
> 「该任务时间窗内执行过的、自耗时最长的被埋点区域」，**不代表整块
> Long Task 都花在该函数**。场景 C 最典型：2723 ms 的任务里只包含
> 46.5 ms 的 `deriveRows` 自耗时，其余约 2.68 s 是未埋点的 **React
> 渲染/提交 100 张卡片**的成本；场景 A 同理，单次 assemble 自耗时
> 峰值仅 7.4 ms，它触发的下游渲染提交把任务撑到 50–96 ms。

## 3 对三个嫌疑点的判断（以 §2 数字为证据）

1. **transcript 每批整体重建（含 `assemble.ts:467` 平方查重）——
   场景 A 的主凶（作为触发器），但结构细节要分两层看**：
   A 的 52 个 Long Task **100% 与 assemble/visibleRange 同帧**（49 + 3），
   「每批事件整体重建」与卡顿一一对应，协调员的主凶判断成立。
   但埋点把自耗时拆出来后：`assembleTranscript+compactTranscript`
   单次峰值只有 **7.4 ms**、12.1s 内累计 1237 ms，其中 `nodes.some`
   平方查重在 n≈2000 节点 / 600 工具时**尚未成为主要自耗时**；把任务
   推过 50ms 的是重建后 React 对虚拟窗口行的重新渲染/提交。
   含义：后续修复的第一优先是**增量组装 + 行级 memo**（消除每批全量
   重渲染的触发）；平方查重是随规模恶化的隐患（n=10k 时自耗时会到
   数十 ms 量级），用 `Set` 去重是顺手的结构性保险，但不是本轮数字下的
   主要账单。
2. **终端每块输出两次 React 状态更新 + 逐字节分配——本机本引擎下
   不是主凶，但不能据此反驳所有者的 WebKit 卡死**：
   5000 行 / 30fps 下 **0 个 Long Task、TBT=0**，`tty.onFrame` 峰值
   0.6ms（含两次 setState 拼接截断 + `Array.from` 逐字节分配），
   `term.write` 峰值 0.1ms；WebGL renderer 全程未降级、无 context loss。
   双 setState 是真实存在的频次浪费（每帧两次 React 提交），在这台
   Chromium 上从未跨阈值；所有者感受到的终端卡死是否是 WebKit 引擎特有
   问题（xterm.js 在 WebKit 上的已知坑，见 desktop-app.md §B.b），
   **必须由 §4 macOS WebKit 列回答**，本组数字不预设结论。
3. **滚动时 `visibleRange` 的线性分配——真，但影响最小，与预判一致**：
   960 次调用、单次峰值 **0.2ms**、累计 8.8ms；只有 3 个 Long Task
   与它同帧且都同时包含 assemble。线性 GC 抖动存在，不构成主线程阻塞，
   修复优先级最低。

**额外发现（任务书三个嫌疑之外）**：场景 C 是三场景里 TBT 最高的
（9.3s，单任务 2.7s）。审批中心在每次 2s `interaction.list` 轮询后
对 100 张卡片做全量渲染提交，行数据派生本身只占 48ms，账单几乎全在
React 提交。这不是本轮修复范围（本任务只测量），但应作为后续任务的
候选，优先级凭 §4 复测再定。

跨场景补充：Linux headless/软件 GL 下 WebGL 保持生效、零降级，没有
观测到「WebGL 上下文丢失后掉到 DOM」的隐藏卡顿；该探针的价值要在
macOS（尤其 WebKit）列兑现。

## 4 引擎对比表（协调员在所有者 Mac 上填写）

用 §1.2 同一份驱动、同一份场景（事件/行/卡片数与速率都在 fake Node
里写死），在 macOS 上分别跑 `--project=chromium` 与
`--project=webkit`，把各自
`web/tests/perf/results/perf-<engine>-*.json` 的数字抄入：

| 场景 | 指标 | Linux Chromium（本任务，有负载¹） | macOS Chromium | macOS WebKit（Safari 引擎） |
|---|---|---|---|---|
| A | Long Task 数/分钟 | 257（52 个 / 12.1s） | _（待填）_ | _（待填）_ |
| A | 最长 Long Task ms（归因 file:line） | 96 · `transcript.assemble` · `Transcript.tsx:211` | _（待填）_ | _（待填）_ |
| A | TBT ms | 711 | _（待填）_ | _（待填）_ |
| A | 峰值 heap | 128.7 MB | _（待填）_ | _（待填，WebKit 可能为 null）_ |
| B | Long Task 数/分钟 | 0（0 个 / 13.5s） | _（待填）_ | _（待填）_ |
| B | 最长 Long Task ms（归因 file:line） | 无 | _（待填）_ | _（待填）_ |
| B | TBT ms | 0 | _（待填）_ | _（待填）_ |
| B | renderer / context-loss | webgl / 0 | _（待填）_ | _（待填）_ |
| C | Long Task 数/分钟 | 55（13 个 / 14.1s） | _（待填）_ | _（待填）_ |
| C | 最长 Long Task ms（归因 file:line） | 2723 · `approvals.deriveRows` · `ApprovalsPage.tsx:48`（其中仅 46.5ms 为派生自耗时，余为卡片渲染提交，见 §2 归因语义） | _（待填）_ | _（待填）_ |
| C | TBT ms | 9249 | _（待填）_ | _（待填）_ |

¹ 该列在共享 Linux 构建主机负载 52.6（1min）/5 个并行闸门构建/本进程 nice 10
下测得，绝对值偏高，见 §2.0；macOS 复测请在所有者常规使用状态（不要
刻意空载，也不要刻意加压）记录同样的负载信息以便对照。

填写时请一并记录：Chrome/Safari 版本、Mac 机型与年份、是否外接显示器
（WebGL 硬件加速路径相关），以及测量时刻的 `uptime` 负载（本机这组数字
对应的引擎是 HeadlessChromium 151.0.0.0，负载条件见 §2.0）。这张表替代桌面 brief §B 里引用的第三方
vendor-friendly 数字，作为 c-shell-decision 的 owner-machine 证据。

共享机构器上若默认端口（Hub 127.0.0.1:58880 / web 58889）被其他 e2e
占用，可用 `HUB_E2E_LISTEN=127.0.0.1:<port> HUB_E2E_WEB_PORT=<port>`
覆盖（本任务 Linux 复测即使用 58882/58891）。

## 5 本任务刻意不做

- 不修任何性能问题（修复是后续 c-transcript-virt / c-raf-budget 等任务，
  凭本表数字排优先级）。
- 不录火焰图截图/不录屏：埋点 JSON 已含归因与 file:line；如后续补截图，
  遵守 `REMUDA_EVIDENCE`，只截 Remuda 渲染，宽度 390 或 1440。
- perf 场景不进闸门；`tests/perf/results/` 已 gitignore。

## 6 合入闸门失败复核（2026-09-23）：既有负载抖动，非本任务引入

闸门 hub e2e 阶段在高负载下报 `ux-question.hub.spec.ts:246` failed（重试一次
仍失败）+ 同文件 `:233` flaky。复核结论：**与本任务改动无关**。

- **失败签名不在被测逻辑**：`:246` 两次都死在 `createSession()`
  （spec:98）——等待 20s 让 fake node 出现在新建会话的主机选择框
  （`new-session-host` 含 `e2e-fake-node`），终端自答的测试体根本没开始。
  闸门日志同窗口有 14 个 EPIPE 与多条 `node did not durably accept command`
  告警（node 实际回了 `{"ok":true}`，是 Hub 转发持久化窗口在饱和下未满足）。
- **flaky 签名是共享登录助手的冷挂载竞态**：`:233` 首次失败在
  `tests/e2e/hub-auth.ts:30`——`/login` 5s 内没出现登录表单（页面以已认证
  shell 挂载）。本任务在自己的 perf 驱动里已用 `ensureLogin()`（容忍该竞态
  的两侧）绕开，但**没有改动闸门共享的 `hub-auth.ts`**（按复核要求不为过闸
  改既有测试/助手）。
- **改动隔离性（静态）**：`hub_e2e.rs` 的 269 行新增全部位于
  `HUB_E2E_PERF=1` 条件之后（3 个哨兵分支 + 4 个新函数），无既有共享辅助
  函数被修改；闸门 hub 配置不设置该变量（仓内全量 grep 仅 perf 配置/源码
  引用）；`ux-question.hub.spec.ts` 与 base `fe392d6a` 逐字节相同；
  `ApprovalsPage.tsx` 仅 `profileRegion` 纯包装。
- **对照实验（同一锁槽 `/tmp/remuda-perf-question.lock`、同一端口
  58884/58893、`PW_CHANNEL=chromium`、`HUB_E2E_PERF` 未设置）**：

  | 版本 | 运行 | 1min 负载区间 | 结果（每文件 3 用例） |
  |---|---|---|---|
  | 本分支 `a6f6e7cb` | 3 次 | 12.97–18.94 | 9/9 passed（48.0/49.5/55.1s） |
  | base `fe392d6a`（独立 target 冷编译） | 3 次 | 11.31–13.85 | 9/9 passed（48.2/49.4/94.0s） |

  合计 18/18，两修订零失败、无差异。闸门当时整机负载约 52（5 个并行
  cargo 闸门），对照实验负载为 11–19；失败只在饱和窗口出现，记为
  **既有负载型抖动**（主机选择框 20s 超时 + 登录 5s 冷挂载超时均为
  setup 阶段的固定超时，不随本任务变化）。后续若要治理，应放宽/重试
  这两个 setup 等待，属独立任务，不在 c-perfaudit 范围。

