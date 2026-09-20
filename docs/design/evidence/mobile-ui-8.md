# 手机优先 UI · 任务 8：`m-push` — badge 计数、权限入口、关闭态深链

2026-09-20 · `wt/c-mpush/b-mpush-md` · mobile-ui 实施计划 §(C) 任务 8（c-mpush）

规格权威：[ui-spec.md](../ui-spec.md) §4.5（含 D-049 增补段），[decisions.md](../decisions.md) D-049 §(5)(6)；基线 `origin/main` @ 218abd99（含 c-minbox：`/m/inbox` 权限横幅、`?focus=` 深链与 compact 重定向）。

## 0. 范围与延后项（carve-out）

派工明确：**本任务不编辑 `web/sw.src.js`（长期规则）**。计划 B.5 中「service worker 在 `push` 事件里 `setAppBadge(n)` / `clearAppBadge()`」的**关闭态角标更新延后**到 owner 解锁的后续任务；其余任务 8 内容全部在本分支交付。当前树的角标语义因此是：

| 场景 | 角标行为（本任务之后） |
|---|---|
| PWA 打开（任意路由） | 页面订阅 hub store，OS 角标始终等于本设备 pending interaction 数；归零即清；无 Badging API 则什么都不做 |
| PWA 关闭 / 被杀，收到推送 | 通知正常展示、点按深链正常（见 §4）；**SW 不在 push 事件里改角标**（延后项）——打开 PWA 后页面立即把角标同步为当前 pending 数 |
| 载荷没有 `badge` 字段（老 Hub） | 页面/SW 行为与今天逐字节一致；角标只由页面按 pending 数驱动 |

后续 SW 改动落地时不需要动协议：载荷里的 `badge` 整数本任务已经在发（§2），SW 只需在 push 事件读 `payload.badge` 并对支持的平台调 `navigator.setAppBadge()` / `clearAppBadge()`，能力缺失即跳过。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `crates/remuda-push/src/notify.rs` | `Notification` 增加 `#[serde(skip_serializing_if = "Option::is_none")] badge: Option<u64>` 与 `with_badge(n)`；`Notification::new` 签名与旧 JSON 形状不变。3 个单测覆盖序列化、缺省、显式 0 |
| `crates/remuda-hub/src/alerts.rs` | `fanout` 读一次持久 pending interaction 总数（`list_interactions(.., pending_only=true)`），给每条出站通知 `.with_badge(n)`；follow 抑制（有设备 follow 即不推）原样不动 |
| `crates/remuda-hub/tests/push.rs` | 假推送端点记录密文；新增 RFC 8291 解密断言：两条 interaction 后载荷 badge=2、data.url 正确；非 interaction 告警同样带当前 pending 总数 |
| `web/src/lib/push.ts` | `notificationFromPayload` 携带可选整数 `badge`（缺失/错型/负数/小数一律不带，绝不合成）；新增 `syncAppBadge(n)`、`startAppBadgeSync()` / `stopAppBadgeSync()` |
| `web/src/app/Root.tsx` | 挂载时 `startAppBadgeSync()`，卸载即停；与具体路由无关 |
| `web/src/features/mobile/Inbox.tsx` | 横幅「开启」被**拒绝**后持久关闭横幅（per device）；授权成功本就因 `granted` 而隐藏。只接线，无样式改动 |
| `web/src/lib/push.test.ts` | 新增 13 个 vitest（共 16）：payload 解析/缺省/畸形、set/clear/无 API/setter 拒绝、store 订阅跟随 pending、去重、幂等、停止 |
| `web/tests/e2e/m-push.hub.spec.ts` | 新增 hub 规格（390px，fake node；只断言站内可断言部分） |
| `docs/design/evidence/mobile-ui-8.md` | 本文件（无截图：本任务没有视觉变化；真机证据待 owner 按 §6 清单执行） |

未触碰：`web/sw.src.js`、`web/sw-build.ts`、`web/playwright.hub.config.ts`、`crates/remuda-push/src/vapid.rs`、`tag.rs`、`crates/remuda-hub/src/interactions.rs`（HTTP 面）、桌面任何页面与样式。`PushTag` topic 规则、四类告警（interaction / turn error / exited / waiting）与「不新增事件类型」均不变。

## 2. 载荷兼容（验收 1）

旧 JSON：`{"title","body","tag","data":{"url"}}`。新增字段为 `Option<u64>` + `skip_serializing_if`：

- `Notification::new(..)` 构造的载荷**不含 badge 键**（不是 `null`），老客户端逐字节无感；
- 老 Hub 发的载荷没有该键，新客户端 `notificationFromPayload` 返回 `badge === undefined`，通知展示路径不变；
- `with_badge(0)` 显式序列化为 `0`（「已知为零，去清角标」语义，不能塌缩回缺省）。

`cargo test -p remuda-push -p remuda-hub` 全绿；解密级断言在 hub 集成测试里（假端点只收 RFC 8291 密文，`ece::decrypt` 后比对 JSON，确保字段真的上线而不是只过序列化单测）。

## 3. 角标口径（验收 2）

**唯一数据源是 pending interaction 数**，与站内角标（`Shell.tsx` / `PhoneShell.tsx` 都用 `hub.interactions.filter(i => i.state === "pending")`）同一口径：

- Hub 侧：`interactions` 表 `state='pending'` 的**持久**总数。这是全局队列——每个订阅设备收到同一个数；实时 RPC / Bot 审批是瞬态面，不计入（推送本身是异步 fanout，且被 follow 的实例根本不推）。
- 页面侧：直接用 store 现有投影，不重新发明判定。

诚实降级：`navigator.setAppBadge` 与 `clearAppBadge` 都不存在时 `syncAppBadge` 静默返回；`clearAppBadge` 缺失但有 setter 时退化为 `setAppBadge(0)`；Promise reject（权限/平台拒绝）吞掉——角标是装饰性的。**任何路径都不用通知条数冒充角标数。**

同步器模块级单订阅（StrictMode 双挂载只订一次），仅在计数变化时调 API，`stopAppBadgeSync()` 解绑并重置，供测试与卸载清理。

## 4. 权限入口与关闭态深链（验收 3、4）

- 权限申请只发生在用户手势里：`/m/inbox` 横幅「开启」与设置 → 通知（既有）。Inbox 挂载 effect 只 `readPushStatus()`（读），从不 `requestPermission()`。
- 横幅消失条件：`granted`（`derivePushBanner` 既有）或本次手势被**拒绝**（本任务补：写 `runtime.m-inbox-push-banner-dismissed` 并置本地态），或用户点 ×。dismissal 仍按设备持久化。
- 关闭态：SW 被系统唤醒 → `showNotification`（`sw.src.js` 既有逻辑，未改）→ 点按 `notificationclick` 打开 `data.url`；interaction 告警的 url 是 `/approvals?focus=<id>`，compact 重定向层（c-mshell / c-minbox 已交付）原样带 query 落到 `/m/inbox?focus=<id>`，行高亮且滚入视口。**本任务只断言落点，不重新实现重定向。**

## 5. 自动化测试

- `cargo test -p remuda-push -p remuda-hub`：全绿（remuda-push 22 + 3 新单测；hub 全部套件含改后的 tests/push.rs）。
- `pnpm --dir web test`：vitest 全绿（143 文件 / 1390 用例；push.test.ts 16 个，其中 13 个新增）。
- `pnpm --dir web typecheck`、`pnpm --dir web lint`：通过（lint 0 error；输出中 react-compiler warning 全部来自既有文件，本任务文件零 warning）。
- hub e2e `m-push.hub.spec.ts`（390×844，合成数据全部来自 in-process fake node，无个人路径/用户名/真实主机名），3 用例：
  1. 默认权限横幅出现（`data-mode=enable`）→ × 关闭 → 重新打开仍隐藏（per-device 持久）；
  2. context 级授予 notifications 后横幅从不出现（页面只读状态）；
  3. `/approvals?focus=<id>` → URL 为 `/m/inbox?focus=<id>`，目标行 `data-focus=true` 且整体在 390×844 视口内。

  **真实 VAPID 推送与真实 OS 角标不在 e2e 内**（需要真机与浏览器推送服务），由 §6 人工清单覆盖。

  运行注记：本机无 Google Chrome（`/opt/google/chrome/chrome` 缺失）。hub 配置在 `PW_CHANNEL=chromium` 时实际下发的是 **chrome-headless-shell**，该构建把 granted notifications 只反映到 Permissions API、不反映到 `Notification.permission`（已实测：permissions.query=granted 而 Notification.permission=denied），故本规格在**文件级** `test.use({ channel: "chromium" })` 钉住完整 Chromium 构建（browser 级选项不能放 describe 内）；配置文件未改动，其余规格继续走默认构建，CI 的 `channel:"chrome"` 也被该文件级覆盖兼容。

  ```bash
  # E2E_LOCK = 本派工锁槽 locks/e2e.lock-c；端口 59240/59249/59241
  flock "$E2E_LOCK" bash -c '
    export HUB_E2E_LISTEN=127.0.0.1:59240 HUB_E2E_WEB_PORT=59249 \
           HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59241 PW_CHANNEL=chromium
    cd web && ./node_modules/.bin/playwright test -c playwright.hub.config.ts m-push.hub.spec.ts'
  ```

  连跑记录：**3 次全绿（3 passed）**（派工要求「your spec three times」；运行时间 2026-09-20）。
- 回归：`m-inbox.hub.spec.ts` 单跑 **1 次全绿**（派工要求）；完整 web hub e2e 套件在最终提交树跑 **1 次**，结果见 §7。
- `bash scripts/ci/secret-scan.sh`：通过（见 §7）。
- 闸口注意（计划 E4）：改动含 `web/src/lib/push.ts`、`web/src/app/**`、`web/src/features/mobile/**`，不在 `remuda merge` 自动 `--web-e2e` 清单内，合并时必须显式传 `--web-e2e`。

## 6. 真机清单（B.5 七步，owner 执行；本环境无法运行）

以下七步需要一台 iPhone（iOS 16.4+，Web Push 只在已加主屏幕的 standalone PWA 里可用）与另一台能产生审批的机器（Mac/PC + Remuda node，或同机第二个浏览器会话亦可，但推送抑制要求该实例**没有设备在 follow**——先关掉桌面上对应会话的页面）。我无法在本机执行：没有 iOS 设备、没有可用的浏览器推送服务链路，e2e 只覆盖站内 DOM/路由。请 owner 逐项执行并在右侧记录；截图请按 c-minbox 口径脱敏（合成会话名，不出现真实主机名/路径/用户名）。

| # | 步骤 | 本任务树的预期 | 结果 / 截图 |
|---|---|---|---|
| 1 | iPhone Safari 打开 Hub（HTTPS），分享菜单 →「添加到主屏幕」 | 主屏出现 Remuda 图标；**不要在 Safari 标签页里继续** | |
| 2 | 从主屏图标启动 PWA → 进「收件箱」(`/m/inbox`)，点顶部横幅「开启」 | 系统弹窗授权 → 横幅消失；底部/再次进入均不再出现；设置 → 通知显示已订阅 | |
| 3 | 上划**彻底杀掉** PWA（不是只切后台），锁屏等 10 秒 | —（确认推送走的是关闭态 SW 唤醒路径） | |
| 4 | 另一台机器新起一个会话并让它停在一次 approval / AskUserQuestion（或 Bash 审批）；保持该会话页面**关闭**（否则 follow 抑制不推） | Hub 日志出现一次 push 201，无 "push suppressed" | |
| 5 | iPhone 收到通知 | 标题/正文为 Need your input 类文案；通知中心里按 `interaction:<id>` 折叠去重。**角标预期（当前树）：SW 不改角标（延后项）；解锁/点开 PWA 后**下一步立即成立 | |
| 6 | 点按通知 | PWA 打开 `/approvals?focus=<id>` → compact 立刻重定向到 `/m/inbox?focus=<id>`，对应行高亮并位于视口中部；kind 分段为「全部」时可见 | |
| 7 | 核对角标与清零 | 打开 PWA 后主屏图标角标 = 待处理总数（多设备/多条 pending 全数计入）；在收件箱「允许一次/拒绝」处理完最后一条 → 角标**清零**（页面驱动）。不支持 Badging 的平台（如未安装的 Safari/部分 Android 皮肤）全程无角标且无报错 | |

步骤 5/7 的「推送到达瞬间角标即为 N、不打开 PWA 也正确」属于延后的 SW 部分；owner 解锁后续任务后，该清单在步骤 5 增加一条预期：锁屏时图标角标已显示 pending 总数；处理完（在另一台设备）后下一条推送带 `badge=0` 并由 SW 清零。

失败重试约定：推送没收到先查 ① 该实例是否有设备正 follow（抑制是设计行为，换个无 follow 的实例重试，见 §8）；② iOS 是否真的从主屏图标启动（Safari 内无推送）；③ 站点是否 HTTPS/证书有效；④ 设置 → 通知里该 PWA 的系统开关。重试请重新从步骤 3 开始，避免把旧通知当新结果。

## 7. 最终验证记录

- `cargo test -p remuda-push -p remuda-hub`：全绿（含 3 个 badge 序列化单测与 tests/push.rs 解密断言）。
- `cargo clippy --workspace --all-targets -- -D warnings`：0 warning/0 error（过程中修掉一处本任务引入的 `len() >= 1`，改 `!is_empty()`）。
- `cargo fmt --all --check`：clean（toolchain 1.94.1，style edition 2024）。
- `pnpm --dir web test`：143 文件 / **1390 用例全绿**；`pnpm --dir web typecheck`、`pnpm --dir web lint`：通过（本任务文件零 warning/error）。
- `m-push.hub.spec.ts`：**连跑 3 次，每次 3 passed**（派工端口 59240/59249/59241，锁槽 `e2e.lock-c`，2026-09-20）。
- `m-inbox.hub.spec.ts`：回归 1 次，**4 passed / 1 evidence skipped**。
- `bash scripts/ci/secret-scan.sh`、`bash scripts/ci/no-tunnel-scan.sh`：pass。

完整 web hub e2e 套件（`playwright test -c playwright.hub.config.ts`，1 worker）运行 **2 次**（全部在派工端口 59240/59249/59241 + 锁槽 `e2e.lock-c`，bundled full Chromium；本任务的 3 个用例在整跑中编号 174–176，均 ✓）：

1. 第一次（约 25.6 分钟）：169 用例 **156 passed / 1 failed / 12 skipped**（skipped 均为未置 `REMUDA_EVIDENCE` 的证据用例）。唯一失败 `grok-structural.hub.spec.ts:507`（真实 fake-harness PTY，1.4 分钟工具卡 settle，"structured tool card never settled"）。该用例运行窗口内 vite 日志出现 `hmr update /src/app/Root.tsx`、`/src/features/mobile/Inbox.tsx`——协调者指示的 main 历史改写 rebase 在套件运行中重写了工作树（同 mobile-ui-4 §10 记录的 HMR 事故类），但见第 2 次结果。
2. 第二次在**最终提交树、工作树完全静止后**整跑（约 27.1 分钟）：169 用例 **155 passed / 2 failed / 12 skipped**。两条失败均为**协调者确认的既有负载型 flaky 用例（plain main 同样失败，已立 follow-up），与本任务无关**：
   - `web/tests/e2e/grok-structural.hub.spec.ts:507` — 「named running tool: … file-decided end」，`grok-structural.hub.spec.ts:434` 抛 "structured tool card never settled"（真实 PTY 1.4 分钟长用例）；
   - `web/tests/e2e/ux-nextstep.hub.spec.ts:159` — 「row shows the approval summary…」，`ux-nextstep.hub.spec.ts:249` `locator.evaluate` 90s 超时（真实 PTY 1.5 分钟长用例）。

   隔离复跑这两个规格（同锁槽/端口）：3 用例 2 passed / 1 failed——ux-nextstep 隔离通过，grok-structural:507 仍失败，符合「负载/环境时序」而非代码回归的定性；按协调者指示不再追跑全量。本任务相关的 `m-push.hub.spec.ts`（3×3 全绿）与 `m-inbox.hub.spec.ts`（4 passed / 1 evidence skipped）不受影响。

合并闸口：计划 E4——改动含 `web/src/lib/push.ts`、`web/src/app/**`、`web/src/features/mobile/**`，不在 `remuda merge` 自动 `--web-e2e` 清单，合并时显式：`./scripts/ci/gate.sh --web --web-e2e`。

## 8. 已知边界（计划 E10）

Hub 的 follow 抑制是实例级而非设备级：**任意**设备正 follow 某实例时，所有设备都收不到该实例推送（`alerts.rs` follow 抑制，本任务按派工要求未改）。多设备同看一场时手机可能因此不收推送；后续如需「按设备 follow」细化是 Hub 侧独立决策，不在本批。badge 计数同理是全局持久 pending 总数，不按设备裁剪——这与站内收件箱口径一致。
