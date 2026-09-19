# mobile-new-session-1 · c-mobilenew 证据

- 日期：2026-09-19
- 分支：`wt/c-mobilenew/b-mobilenew-md` · 任务 c-mobilenew（a phone can always create a session; bulk screen reads must never starve control RPCs）
- Base：`origin/main` @ 402c3670
- 规格：本任务书 §1–§5；`web/tests/e2e/ux-mobile-new.hub.spec.ts`

## 1. 复现的拒绝（修前必红）

### 1.1 手机现场 → Hub 根因

owner 2026-09-19 iPhone 报告：新建 Claude 会话失败。coordinator 在 headless iPhone 13
profile 对 demo Hub 复现：流程本身通（底栏 新建 → /sessions/new 弹层 → 开始 → /s/<id>），
但打开会话页/列表时，每个列出的会话发一个
`GET /v1/instances/<id>/screen?lines=80`，其中 9 个同时失败：

```
HTTP 500 {"error":"too many in-flight node rpcs","code":"INTERNAL"}
```

根因在 `crates/remuda-hub/src/transport.rs`（修前 100–105 行）：
`WssTransport::call` 对该 Node 上**所有**挂起 RPC 共用 32 的硬上限，超限直接
`Internal("too many in-flight node rpcs")` → HTTP 500 INTERNAL。控制类 RPC
（instance.create / cancel / steer）与批量只读（screen）抢同一个预算；一台 Node 上有很多
已退出实例时，列表的逐行 screen 扇出把预算占满，create 被一个不透明的 INTERNAL 拒掉。

### 1.2 修前失败的自动化证据

e2e 第二个 case（`saturated screen reads still let instance.create through`）通过
hub_e2e 的门控文件把 seeded 行的 `tty.screen` 回复停在 Node 侧，然后用带登录 cookie 的
`page.request` 对 14 个已退出实例并发扇出 40 个 `/screen`，再在弹层里真正走一次创建。

把 Hub 三个文件临时还原到 `origin/main`（example 的新旋钮保留、Web 新客户端在
`page.request` 路径之外），该 case 在修前代码上**必红**，响应直方图：

```
OLDCODE_STATUS_HISTOGRAM [[500,40]]
✘ saturated screen reads still let instance.create through
  Error: excess reads get NODE_BUSY/503 — Expected: > 0, Received: 0
```

40/40 全部 500 INTERNAL，0 个 503 —— 与 owner 现场逐字一致。修后同一 case：
**40/40 全部 503 NODE_BUSY，0 个 500**（门控存在时先到的读占满 16 个只读子槽、在 Node 侧停到
5s 读超时，超时也归 NODE_BUSY；后到的读在帧入队前即被拒。门控存在期间用 UI 真实走的
instance.create 始终成功）。

## 2. 修复

### 2.1 Hub：控制 RPC 预留一半容量（`transport.rs` + `error.rs`）

- 新增 `HubError::NodeBusy { retry_after_ms }` → HTTP **503**、code **NODE_BUSY**，
  带 `retryAfterMs: 2500` 与 `retryable: true`。任何路径都不再把 RPC 预算拒绝映射成
  500 INTERNAL。
- 每条 Node 链路总预算仍为 32（与协议 `max_in_flight_rpc` 对齐）；
  **批量只读子预算 16**（`is_bulk_read`：`tty.screen`、`subagent.transcript`、
  `host.files.list/read/search` —— 会扇出的幂等 pull）。控制方法（create/cancel/steer/
  写/placement 的 `host.resources`/开流的 `tty.attach` 等）始终保有至少 16 个槽。
  批量读到 16 即 503 NODE_BUSY（帧入队**之前**拒绝，Node 侧零副作用）；控制类只有在
  自己的半区也满时才拿 NODE_BUSY。
- 挂起表从 `tokio::Mutex<HashMap>` 换成 `std::Mutex<HashMap<String, PendingCall>>`
  （`PendingCall { tx, bulk }`），并由 `PendingGuard` 的同步 Drop 保证：回复、发送失败、
  超时、future 被取消、链路断开——每条路径都恰好摘掉自己的槽，挂起表不可能留下死 waiter。
  `ws.rs` 与 `ssh_hosts.rs` 在 socket/stdio 循环结束时 `fail_all_pending()`：dropping
  senders 让在途调用立刻返回 `Ok(None)`（not writable），而不是挂到超时。批量读超时也归为
  NODE_BUSY（慢一行是可重试噪声，不是 500）。
- 单元测试（`transport.rs` budget_tests，fake transport + socket pump）：
  `bulk_reads_cannot_starve_control`（40 并发 screen + 1 个 instance.create：create 成功、
  多出的读全部 NODE_BUSY、无 INTERNAL、表归零）、`control_keeps_half_the_slots`、
  `cancelled_call_frees_its_slot`、`link_drop_fails_all_waiters`。

### 2.2 Web：列表不再饿死创建（`store.ts` / `api.ts` / `SessionList.tsx`）

- `api.screenRead` 识别 503 NODE_BUSY，抛 `ScreenNodeBusyError`（带 retryAfterMs）；其它
  失败照旧返回空屏。
- store 的屏幕轮询改为有界调度器：**exited/failed 永不读屏**；全局
  **最多 4 个在途**且每行 single-flight；调用方顺序即列表顺序，
  SessionList 用 IntersectionObserver 把**可视行提到队首**；NODE_BUSY 时只回退该行一个
  轮询周期（retryAfterMs），不打 console、不报错。
- SessionList 的轮询 effect 加了 `location.pathname === "/sessions"` 门：该组件在
  `/sessions/new` 后面以 dimmed 状态常驻挂载（手机弹层正是那个扇出的现场），现在弹层和
  `/s/<id>` 都不再为任何会话发 `/screen`；会话页本身从不轮询别的会话（见 §3 case 1：
  在已退出会话页停 3 s、列表页停 6 s，收集到的 /screen 数为 0）。

### 2.3 Web：创建失败就地显示 Hub 错误（`NewSessionPage.tsx` / css）

- 4xx（除 408）仍为「可就地修正的拒绝」；新增 **503 NODE_BUSY 同等待遇** —— 该 503 是
  帧发出前的拒绝，绝不可能已建会话，允许原样再点 开始。错误文案改为
  `CODE · message`（截图：`NODE_BUSY · node busy: too many in-flight node rpcs; retry after 2500 ms`），
  `role="alert"` 就地显示、表单与草稿全保留、开始按钮重新可用；出现时滚入视口，css
  `overflow-wrap: anywhere` 保证 390px 不溢出。既有的「创建结果没有确认」路径（504/断连/
  其它 5xx → 状态待确认、绝不二次创建）原样保留。

### 2.4 iOS / 软键盘（best effort）

- 本机 Ubuntu 20.04 无法安装 Playwright WebKit（`playwright install webkit`：
  “does not support webkit on ubuntu20.04-x64”），**WebKit/iPhone 13 项目未能在本机执行**；
  共享的浏览器 WS 端点（127.0.0.1:3177）是 Chromium CDP，不是 WebKit。
- 几何验证在 390×844 iPhone 13 视口（Chromium，经 PW_TEST_CONNECT_WS_ENDPOINT 连共享
  浏览器）做：focus prompt 后把 `visualViewport.height` 合成收缩到 504 / 400 / 320
  （与既有 new-session.spec 的软键盘模拟同一手法），开始按钮盒模型全部落在收缩后视口内，
  中心点 `elementFromPoint` 命中按钮本身：

  | visualViewport.height | 开始按钮 top | bottom | 在视口内 |
  |---|---|---|---|
  | 504 | 443 | 491 | ✓ |
  | 400 | 339 | 387 | ✓ |
  | 320 | 259 | 307 | ✓ |

  弹层 css 已经从 `--workbench-height`（`useWorkbenchViewport` 实时取
  `window.visualViewport.height` / offsetTop）定尺寸，`Sheet.tsx` 无需改动。
  **未能验证**：真机/Safari 上 visualViewport 事件时序与软键盘动画的最终像素。

## 3. e2e（390px，fake node 新旋钮）

`crates/remuda-hub/examples/hub_e2e.rs` 仅新增旋钮（additive）：
`HUB_E2E_EXITED_INSTANCES=n` 在 Node hello 之后经 in-process store 种 n 个已退出
shell-pty 行（真实 journal lifecycle=exited 路径，Node inventory 仍为空）；
`tty.screen` 对 seeded 行支持 `HUB_E2E_SCREEN_DELAY_MS` 与门控文件
`$TMPDIR/remuda-e2e-screen-gate-<port>`（文件存在即停住回复，删除即放行）。READY 行带
`screenGate`/`seededExitedIds`。默认（无旋钮）行为不变，全量既有套件不受影响。

`web/tests/e2e/ux-mobile-new.hub.spec.ts`，390×844，三例：

1. **14 个已退出行：0 个 /screen、0 个 500，两次创建都打开新会话页** —— 选空间会自动落
   到首个 tab（已退出行），停 3 s 无 /screen；底栏「会话」回列表见「已退出 14」，停 6 s
   /screen 数仍为 0；底栏 新建 → 开始 → `/s/<id>`；在会话页再底栏 新建（dimmed 列表在
   弹层后）→ 第二次创建同样成功。全程收集响应，无 500。
2. **503 NODE_BUSY 创建被拒：弹层就地显示 code+message，保留表单，开始可再点**；
   「状态待确认」路径不受影响（既有 new-session.spec 覆盖）。
3. **读饱和时创建仍成功**：门控停住 16 个入预算的 screen，40 个并发读 → 多出来的读为
   503 NODE_BUSY、**0 个 500**，UI 里走真实创建成功。

验证记录（Chromium，经共享浏览器 WS；本机跑不起 WebKit）：

```
HUB_E2E_LISTEN=127.0.0.1:59150 HUB_E2E_WEB_PORT=59159 \
HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59151 HUB_E2E_EXITED_INSTANCES=14 \
  playwright test --config playwright.hub.config.ts ux-mobile-new.hub.spec.ts
=== RUN 1 === 3 passed (43.9s)
=== RUN 2 === 3 passed (44.1s)
=== RUN 3 === 3 passed (43.9s)
```

## 4. 390px 截图

按本任务规则，截图快照**不入库**；以下三张在 390×844 用
`REMUDA_EVIDENCE=1`（外加上述 HUB_E2E_* 旋钮）重跑本 spec 生成，可随时复现：

- `mobile-new-session-1-list-390.png` — 底栏「会话」后的列表：`已退出 14`、14 个 exited
  卡片、0 个 /screen 请求。
- `mobile-new-session-1-session-390.png` — 创建后打开的新会话页。
- `mobile-new-session-1-node-busy-390.png` — 创建收到 503 NODE_BUSY：弹层底部就地显示
  `NODE_BUSY · …retry after 2500 ms`，开始按钮仍在、可用。
