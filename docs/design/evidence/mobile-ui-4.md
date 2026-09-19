# 手机优先 UI · 任务 4：`/m/inbox` 两档收件箱

2026-09-20 · `wt/c-minbox/b-minbox-md` · mobile-ui 实施计划 §(C) 任务 4（c-minbox）
规格权威：[ui-spec.md](../ui-spec.md) §1.2 / §2.5 / §4.5 / §4.7，[decisions.md](../decisions.md) D-049；基线 `origin/main` @ 39d5bb73（含 c-mshell fd4dceba：`/m` 路由树、PhoneShell、重定向层；落地前 `/m/inbox` 在 PhoneShell 内渲染桌面 `ApprovalsPage` 占位）。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/features/mobile/inboxRows.ts` | 新增。纯派生：两档投影、副标题（错误优先、原文不改写）、context 剩余环百分比、`?kind=` / `?focus=`、权限横幅状态 |
| `web/src/features/mobile/inboxRows.test.ts` | 新增。22 个 vitest：分档、排序、错误优先、paused、focus、kind、null 环、横幅状态机 |
| `web/src/features/mobile/Inbox.tsx`、`inbox.module.css` | 新增。手机收件箱屏（顶栏标题 + kind 横滚分段 + 权限横幅 + 两档） |
| `web/src/app/router.tsx` | 唯一路由改动：`/m/inbox` 元素由 `<ApprovalsPage />` 换为 `<Inbox />`；重定向层、其他路由不动 |
| `web/tests/e2e/m-inbox.hub.spec.ts` | 新增 hub 规格（390px，fake node） |
| `docs/design/evidence/mobile-ui-4.md` + `mobile-ui-4-inbox-390.png` | 本文件与证据截图（合成数据，fake node） |

复用未改：`projectInteraction` / `INTERACTION_LABEL` 语义（`web/src/lib/interactionStatus.ts`，与桌面同一档队列判定）、`hubStore.respond`（按钮禁用直到 journal 回执）、`readPushStatus` / `subscribePush` / `needsHomeScreenForNotifications`（`web/src/lib/push.ts`）、`liveSummary` 投影（经 `hubStore.hydrateRowSummaries`，与 `SessionList` 同一取词路径）、`projectStatus`、`StateDot`、`UsageRollup.contextPct`、`formatListTime`。

未触碰：`ApprovalsPage.tsx` / `ApprovalsPage.module.css`、`ApprovalCard.tsx`、`QuestionForm.tsx`、`ElicitationCard.tsx`、`store.ts`、`push.ts`、`PhoneShell.tsx`、`SessionPage.tsx`、`Composer.tsx`、`playwright.hub.config.ts`、`sw.src.js`。桌面审批中心零改动。

## 2. 两档结构（验收 1）

只有两档，没有第三档（计划 §11.3 实测口径；桌面 `ApprovalsPage` 的「已离队」expired/superseded 分组**不进**手机屏）：

- **待你处理**：`hub.interactions` 经共享的 `projectInteraction()` 投影为 `pending` / `answering` / `paused` 的全部交互（settled / expired / superseded 不落档），按 `createdAt` 倒序。
- **进行中 · 最近**：`hub.instances` 经 `projectStatus()` 投影为 `working` / `idle` / `exited` 的实例，按 `updatedAt` 倒序；被任一未决交互（pending/answering/paused）占住的实例**不进此档**——即使该交互正被 `?kind=` 过滤隐藏（单测钉住：kind 过滤只影响渲染，不影响「blocked 绝不冒充 working」）。

vitest：分档（含 settled/expired/invalidated 全部排除）、blocked 不双档、两档各自时钟倒序。

## 3. kind 分段（验收 2）

`?kind=approval|question|plan-review|elicitation`，与 `/approvals` 同名同义；缺省/非法值 = `all`（URL 上删除参数）。分段条 40px 高可横滚，按钮 testid `m-inbox-kind-*`，选中态走 `aria-selected`。kind 只过滤「待你处理」交互档；「进行中 · 最近」是实例投影，不参与交互类型过滤（与桌面语义一致——桌面 host/workspace 筛选同样不改变会话存在性）。e2e：question/approval 互滤、分段按钮改写 URL、全部回到无 query 的 `/m/inbox`。

## 4. 行形态（验收 3）

```
⚠ Bash                              [context 环]
echo e2e                                      ← 副标题：最近事件原文
e2e-fake-node / remuda-e2e · claude · 9/12    ← 主机 / 项目 / harness · 相对时间
[允许一次] [拒绝] [打开会话]
```

- **标题**：审批 = `request.title`（工具名 Bash/Write），提问 = `AskUserQuestion`（native-tty = 终端提问），计划 = 计划，表单 = 表单标题（与桌面 `kindLabel` 同一函数语义）。
- **副标题 = 最近事件原文，错误优先，绝不改写**（`latestEventText` 纯函数）：
  1. `instance.lastError` 非空 → 逐字打印（含 `API Error: Request rejected (429)` 这类原文），永远优先，哪怕已有 journal 短语；
  2. 交互行其次打印**交互请求原文**（审批 description、native-tty 提问的逐行原文、`问你 N 题 · AskUserQuestion`、计划/表单标题）。被阻塞的回合里，这条请求就是最新的可操作事件，更早的 journal 行（如 fake node 在挂起审批后写入的 `echo: …`）不能盖掉要批的文本——这是 e2e 第一轮暴露的真实问题，已修正并加单测；
  3. 实例行（进行中 · 最近）才回退到 journal 尾投影的 live phrase（`liveSummary`，与首页/会话列表同一原文）。
- **context 环**：右侧 SVG 环 + 百分比，取 `UsageRollup.contextPct`（store 缓存或实例自带）。`null` 时**环与数字都不渲染**（ui-spec §3.3，单测钉住 78% 与 null 两态）。
- **一键**：approval / plan-review 直接渲染请求自带的 options（允许一次 / 拒绝 / 允许会话等，label 逐字来自请求）；所有交互行带「打开会话」`Link → /s/:id`。
- **question 行不在列表里填题**：无论 carrier 都只给「去回答」→ `/s/:instanceId`（ui-spec §2.5；`QuestionForm` 继续只在会话页使用）。native-tty 的逐字副标题与 harness-hook 的来源注记保留。

## 5. `?focus=` 深链（验收 4）

`?focus=<interactionId>` 时对应行加 `data-focus="true"` + dust 描边（`m-inbox-row-focus` 样式），挂载/数据到达后 `scrollIntoView({ block: "center" })`（ref 回调注册表，effect 按 focus id 查找）。query 由 c-mshell 的重定向层从 `/approvals?focus=` 原样带到 `/m/inbox?focus=`，本任务未动重定向层。e2e：造 4 条 pending，深链最旧一条，`boundingBox()` 断言整行落在 390×844 视口内（x/y 双侧）；无 focus 时无任何 `data-focus="true"`。

## 6. 提交语义与暂停（验收 5）

- **禁用权威在 store**：点击后 `hubStore.respond()` 先置 `hub.answering[id]`，行投影立即变 `answering`（选项按钮替换为「已提交」spinner，无任何可再次提交的控件）；store 在 POST + refresh + catchup 后，仅当 journal 未出现 `interaction.answered` 且交互仍 pending 时才清该标记——**journal 回执到达前按钮一直禁用**，与桌面页同一路径，手机侧不另造本地锁（ui-spec §2.5「不要本地抢锁」）。回执后交互离队，行消失（多设备第一台点的算数，由 Hub + 同一投影保证）。
- **主机离线（paused）**：`projectInteraction` 在 host 非 online/enrolled 或 `instance.connectivity !== "connected"` 时投影 `paused`；行显示 `主机离线，交互暂停`（testid `m-inbox-paused`），允许一次/拒绝全部 `disabled`，「打开会话」仍可达。e2e 在 REST 边界把 `/v1/hosts` 响应改为 offline（`ux-files.hub.spec.ts` 同款做法，共享 fake node 不杀进程）。
- 解答后的实例转 idle，进入「进行中 · 最近」（e2e 断言实例在该档可见、无「已离队」第三档）。

## 7. 权限横幅（验收 6，D-049 §6 / ui-spec §4.5）

- 纯状态机 `derivePushBanner(status)`（单测四态）：`granted` → 不显示；状态未读出（null）→ 不显示；非 iOS 的 `unsupported` → 不显示（点了也没用，不画假按钮）；`default` / `denied` → 显示「开启」；iOS 且未 standalone（`needsHomeScreenForNotifications()`，即 `push.ts:19-21`）→ 显示「**先加到主屏幕**」并展开分享菜单指引。
- **只在挂载时 `readPushStatus()` 读状态，绝不在加载时请求权限**；唯一的权限申请路径是「开启」按钮的 onClick → `subscribePush()`（内部 `Notification.requestPermission()` + 订阅），另一处是既有设置页。「先加到主屏幕」按钮不调权限请求（iOS Safari 调了也静默无效），只展开指引。
- 横幅可关闭（×，dismissal 存 localStorage），不影响任何数据面。

## 8. 截图

[mobile-ui-4-inbox-390.png](./mobile-ui-4-inbox-390.png)：390×844，Night 主题，`prefers-reduced-motion`，合成数据全部来自 in-process fake node（主机标签 `e2e-fake-node`、工作区 `remuda-e2e` 夹具、提示词为 `m inbox evidence …` 合成串）；无个人路径、用户名或真实主机名。画面包含：顶栏「收件箱 · 待处理 1」、横滚 kind 分段（全部/审批/提问/计划/表单）、权限横幅（开启 / ×）、待你处理档 1 条（Bash · echo e2e · 允许一次/拒绝/打开会话）、进行中 · 最近档 1 条（已答转 idle 的同批会话）、底栏收件箱 badge 1。

## 9. 测试与验证

- `pnpm --dir web test`：vitest 全绿（134 文件 / 1297 用例，其中本任务新增 22 个）。
- `pnpm --dir web typecheck`、`pnpm --dir web lint`：通过（lint 仅有既有文件的 react-compiler warning，本任务文件零 warning/error）。
- 新 hub 规格 `m-inbox.hub.spec.ts` 连跑 **3 次全绿**（每次 4 passed / 1 evidence skipped；派工指定端口与锁槽；该机无 Google Chrome，按 hub 配置的 CI 回退用 bundled chromium `PW_CHANNEL=chromium`）：

```bash
flock "$E2E_LOCK" bash -c '
  export HUB_E2E_LISTEN=127.0.0.1:59200 HUB_E2E_WEB_PORT=59209 \
         HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59201 PW_CHANNEL=chromium
  pnpm --dir web run test:e2e:hub m-inbox.hub.spec'
```

覆盖：① 允许一次后行随 journal 回执消失、无第二个可提交控件、计数归零、实例进入「进行中 · 最近」；② 离线主机按钮禁用 + 暂停文案；③ `?focus=` 标记与视口内 bounding-box；④ `?kind=` 过滤与 question「去回答」href=/s/:id；⑤ 证据截图（REMUDA_EVIDENCE=1 时）。

- **完整 web hub e2e 套件跑 1 次**（见 §10）。计划 (C) 公共约定 E4：`web/src/features/mobile/**` 不在自动开闸清单，合并时必须显式 `--web-e2e`：`./scripts/ci/gate.sh --web --web-e2e`。
- `bash scripts/ci/secret-scan.sh`、`bash scripts/ci/no-tunnel-scan.sh`：通过。
- 闸口断言全部在 chromium hub 规格（390×844，hasTouch/isMobile）；iPhone webkit 项目不进闸（E3），未声称真机/iOS 验证；iOS 文案分支由纯函数单测覆盖。

## 10. 运行记录

（完整 hub 套件结果于本次运行后回填。）

## 11. 兼容性说明

- 桌面 `/approvals`、其 CSS 与三个共享卡组件零改动；桌面宽度访问 `/m/inbox` 仍由 c-mshell 的 `ViewportGate` 弹回 `/sessions`。
- 其余 compact hub 规格操作的审批卡都在共享 `/s/:id` 路由或桌面 `/approvals` 上；本 diff 只替换 `/m` 树内一个路由元素，不影响这些路径（完整套件回归见 §10）。
- `m-shell.hub.spec.ts` 原有「`/m/inbox` 渲染 `approvals-page`」的临时 IA 断言：该断言针对的占位已被本任务按计划替换为新屏，属计划内交接。
