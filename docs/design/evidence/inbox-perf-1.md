# inbox-perf-1 · 收件箱卡顿修复：2.7 秒 Long Task 的构成与修前/修后对比

- 任务：c-inboxperf（所有者反馈「收件箱有时候会卡死」）。
- 上游测量：`docs/design/evidence/perf-audit-1.md` 场景 C（c-perfaudit）。
- 范围：只动 Web 前端收件箱派生与渲染（`/approvals` 与 `/m/inbox`）；
  不改协议、不改 Hub。

## 1 症状与定位（2.7 秒是谁花的）

perf-audit 场景 C（100 张 pending 审批卡 + 收件箱滚动，Linux
HeadlessChromium）在闸门满负荷（测量时 `uptime` 负载 52.6，5 个并行
闸门构建、本进程 nice 10）时记录到：

- 13 个 Long Task，最长 **2723 ms**，时间窗内的被埋点区域是
  `approvals.deriveRows`（`ApprovalsPage.tsx:48`）；
- 但该区域 **18 次调用累计仅 48.3 ms、单次峰值 46.5 ms**。

即：2.7 秒里约 2.68 s 不是行派生，而是**每次 2 秒 `interaction.list`
轮询后 React 对约 100 张卡片的整表重新渲染/提交**。轮询把 interactions
解析成全新对象（数组与元素身份全部变化），`rows` 的 `useMemo` 依赖整个
`hub` store 对象必然失效，随后每张卡片无 memo，全部重新提交。

同一场景 C 在本任务的常规负载（`uptime` 5.4/9.7/12.4，见 §4）下复测
修前代码，结果更清楚地暴露结构：7 个 Long Task、最长 113 ms、TBT
360 ms，而 `approvals.deriveRows` **14 次调用累计 0.8 ms、峰值 0.2 ms**
——派生几乎免费，账单全部在卡片提交；满闸门负载把同一个同步提交从
113 ms 放大到 2723 ms，正是所有者偶发「卡死」的来源（共享构建主机被
闸门占满时打开/停留在收件箱）。

协调员给的三个线索在 profile 下全部成立，但都不是主账单：

| 线索 | profile 结论 |
|---|---|
| `rows` memo 依赖整个 `hub` | 成立：每次轮询（及无关 store 变化）都整体重算 |
| 每行 `instances.find`/`hosts.find`，O(n×m) | 成立，但 n=100 量级自耗时亚毫秒，非主因 |
| 已结束 interaction 在过滤前参与投影 | 成立，纯浪费；规模放大后才显著 |

## 2 修法

1. **依赖具体切片而非整个 `hub`**：两个收件箱的 `useMemo` 改为依赖
   `hub.interactions / hub.instances / hub.hosts / hub.answering /
   hub.summaries / hub.usageRollup` 与一个显式 workspace Map 等具体引用；
   无关的 store 变化（host 目录刷新、effort 帧）不再重算收件箱。
2. **实例/主机建索引（Map）**：桌面侧新提取纯函数模块
   `web/src/features/approvals/approvalRows.ts`（`deriveApprovalRows`，
   带单测），每行 O(1) 连接；每条 interaction 只投影一次。手机侧原本
   已有 Map，但 blocked-instance 集合对全部 interaction 做了**第二次**
   `projectInteraction`，合并为单次投影。
3. **先滤后投影**：本设备已处理的 interaction（`answer-committed`/
   `resolved` 且 actor 为本设备）在 join/projection 之前丢弃；kind/host
   过滤也在 join 之前。判定顺序敏感（过期/deadline 优先于 committed），
   为此在 `interactionStatus.ts` 导出与 `projectInteraction` 同序的
   `settledOnThisDevice()` 并钉了单测（含「已 resolved 但 deadline 已过
   → 仍进已离队」的边界）。
4. **针对真正大头（整表提交）**：
   - 行组件 `React.memo`，比较函数用每行的内容签名 `sig`
     （JSON 覆盖该卡片读取的全部 store 派生字段，含 `request` 全文——
     轮询重新解析出的相等内容 sig 相同 → 卡片跳过重渲染）。
   - **渐进挂载**：新 hook `useIncrementalLimit`（`web/src/lib/`，带单测）
     把首屏列表按每动画帧 12 张切片提交，100 张卡不再出现在同一个
     任务里；全部行仍在数帧（≈150 ms）内挂载，计数/过滤/深链看到的仍是
     完整列表。深链行若落在后续切片，focus 滚动效果在切片挂载后补跑。
   - 手机侧 `/m/inbox`（所有者最常用的收件箱入口）同样处理：两档分别
     渐进挂载 + 卡片 memo（sig 同时覆盖 2.5 s summary tick 会变化的
     phrase/timeLabel）。

行为不变性：tier 划分、过滤、focus、已离队、回答提交流程与原来一致；
既有 approvals 与 m-inbox 单测/e2e 保持通过（见 §5）。

## 3 测量方法（可复现）

同 perf-audit §1：fake Node 以 `HUB_E2E_PERF=1` 启动，场景 C 哨兵
`__perf_interactions__:100`，页面 `/approvals?profile=1`，1440×900，
Playwright bundled HeadlessChromium 151；命令：

```sh
HUB_E2E_PERF=1 PW_CHANNEL=chromium \
pnpm --dir web exec playwright test -c playwright.perf.config.ts \
  --project=chromium -g "C: 100 pending"
```

默认 Hub/web 端口被占用时用 `HUB_E2E_LISTEN` / `HUB_E2E_WEB_PORT`
覆盖（取值均为本地回环地址与端口）。

修前/修后两次测量使用同一 worktree、同一浏览器版本、同一 fake
Hub/Node、同一命令与同一组端口，两次运行前后都记录了
`uptime`。原始 JSON 在 `web/tests/perf/results/`（gitignore），下表为
手抄；wall 为 `markScenario` 区间（含 2.5 s 收尾等待），滚动四轮
顶↔底且跨过至少一次 2 s 轮询，与 perf-audit 场景 C 完全相同。

## 4 修前 / 修后结果（场景 C，Linux HeadlessChromium）

| 指标 | 修前（常规负载） | 修前（满闸门，perf-audit 记录） | 修后（常规负载） |
|---|---|---|---|
| 测量时 `uptime` 负载（1/5/15 min） | 5.4 / 9.7 / 12.4 | 52.6 / 57 / 55 | 6.9 / 7.7 / 8.2 |
| 墙上时长 s（scenario 区间） | 7.35 | 14.1 | 7.86 |
| Long Task 数（每分钟） | 7（57.1） | 13（55） | **0（0）** |
| TBT ms | 360 | 9249 | **0** |
| 最长 Long Task ms（归因 / file:line） | 113 · `approvals.deriveRows` · `ApprovalsPage.tsx:48` | 2723 · 同左 | **无** |
| 未归因 Long Task | 0 | 4 | 0 |
| `approvals.deriveRows` 调用数/总 ms/峰值 ms | 14 / 0.8 / 0.2 | 18 / 48.3 / 46.5 | 14 / 4.8 / 0.6 |
| **> 50 ms 的 Long Task** | **7（最长 113 ms）** | **13（最长 2723 ms）** | **0（最长任务 < 50 ms，未跨阈值）** |
| 峰值 JS heap MB | 79.5 | 104.6 | 61.8 |

注：修前/修后均由同一份场景 C 驱动，唯一代码差异为本任务提交。两次
常规负载测量负载相当（1 min 5.4 vs 6.9）；满闸门列直接引用
perf-audit-1 的实测——修后提交被切成每帧 12 张卡片（单帧派生+提交远
低于 50 ms），轮询时相等内容的卡片 memo 命中不重渲染，因此闸门抢占只
会整体放慢帧节奏，不再产生一个跨秒的长任务。`deriveRows` 总耗时从
0.8 ms 升到 4.8 ms 是行签名 `sig`（每行一次 JSON.stringify）的成本，
峰值仍仅 0.6 ms，换来整表提交消失。

## 5 验证清单

- `pnpm --dir web test`：✅ 161 文件 / 1639 用例全过（新增
  `approvalRows`、`useIncrementalLimit`、`settledOnThisDevice` 与 sig
  相关用例）。
- `pnpm --dir web typecheck`：✅ 通过。
- `pnpm --dir web lint`：✅ 改动文件无告警（仓内既有的 SessionPage
  warning 与本改动无关）。
- Playwright e2e（Linux bundled Chromium，HUB_E2E_PERF 无关的常规
  fake Hub）：
  - mock 后端 `approvals.spec.ts`：✅ 2/2（过滤/focus/允许一次/各 UI 态）。
  - fake-node hub：`m-inbox.hub.spec.ts` ✅ 4 过 + 1 跳过
    （evidence 截图用例，需 `REMUDA_EVIDENCE=1`）；
    `grok-structural.hub.spec.ts` approvals 队列用例 ✅；
    `m-shell`（?focus 深链/390 重定向）、`m-push`、`ux-question`
    （/approvals 提问卡与 390 回答跳转）、`ux-nextstep`：✅。

## 6 刻意不做

- 不做虚拟列表（行高不固定、含表单；渐进挂载 + memo 已把单任务压到
  50 ms 以下，虚拟列表留待更大的洪水量级）。
- 不改轮询节奏、协议或 Hub；不碰 c-mfix 正在改的 PhoneShell /
  TerminalView / OpaqueRow。
