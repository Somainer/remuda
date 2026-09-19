# mobile-new-session-1 · c-mobilenew 证据

- 日期：2026-09-19
- 分支：`wt/c-mobilenew/b-mobilenew-md` · 任务 c-mobilenew（a phone can always create a session; bulk screen reads must never starve control RPCs）
- Base（round 2 后）：`origin/main` @ b0dd8389
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

把 Hub 三个文件临时还原到修前代码，对 14 个已退出实例并发扇出 40 个 `/screen`
（fake Node 的门控文件让先到的读停在 Node 侧）：

```
OLDCODE_STATUS_HISTOGRAM [[500,40]]
```

40/40 全部 500 INTERNAL，0 个 503 —— 与 owner 现场逐字一致。修后同一 case：
**40/40 全部 503 NODE_BUSY，0 个 500**；并且控制半区被 32 个停住的控制类 RPC 占满时，
真正的 `POST /v1/instances` 直接拿到 503 NODE_BUSY（详见 §3 case 2 与
`crates/remuda-hub/tests/node_busy.rs`）。

## 2. 修复（round 2 含评审修正）

### 2.1 Hub：控制 RPC 预留一半容量（`transport.rs` + `error.rs`）

- 新增 `HubError::NodeBusy { retry_after_ms }` → HTTP **503**、code **NODE_BUSY**，
  带 `retryAfterMs: 2500` 与 `retryable: true`。任何路径都不再把预算拒绝映射成 500 INTERNAL。
- 每条 Node 链路总预算仍为 32；**批量只读子预算 16**（`is_bulk_read`：`tty.screen`、
  `subagent.transcript`、`host.files.list/read/search`）。控制方法（create/cancel/steer/
  写/placement 的 `host.resources`/开流的 `tty.attach` 等）始终保有至少 16 个槽；
  批量读到 16 即 503 NODE_BUSY（帧入队**之前**拒绝，Node 侧零副作用），控制半区也满时
  控制调用同样拿 NODE_BUSY。批量读超时也归为 NODE_BUSY（慢一行是可重试噪声，不是 500）。
- 挂起表为 `std::Mutex<HashMap<String, PendingCall>>`，由 `PendingGuard` 的同步 Drop 保证
  回复/发送失败/超时/future 取消/链路断开每条路径都恰好摘掉自己的槽；`ws.rs` 与
  `ssh_hosts.rs` 在循环结束时 `fail_all_pending()`。
- **round 2 修正（评审 1）**：帧**入队前**的发送失败仍是 `Ok(None)`（=未连接/未写）；
  帧已发出后 waiter 被链路拆除丢弃（`fail_all_pending`）或 oneshot 关闭时，必须是
  `Err(Internal("node rpc dropped"))`，让 `forward_if_online` 走超时同款
  `mark_reconciling`（Node 可能已执行）。单测 `post_send_link_drop_is_lost_reply_error` 与
  `pre_send_failure_is_not_writable` 分别钉死两条路径；`bulk_reads_cannot_starve_control`
  改为停在长超时上的确定性调度（不再和 150ms 真实超时赛跑），另加
  `bulk_timeout_is_node_busy` 覆盖超时映射。

### 2.2 Hub：create 的 NODE_BUSY 不能被吞成 200（`http.rs`；评审 2）

`forward_if_online` 原来把所有 `Err` 吞成 200 reconciling。NODE_BUSY 在构造上是
**帧入队前拒绝**，Node 从未见过该命令，因此：

- 特殊分支：`instance.create`/`instance.resume` 的 NodeBusy 把刚插入的实例行
  `fail_instance` 置 `failed`，其它命令 `reject_command`，然后**返回 Err** → 调用方拿到
  带 `retryAfterMs` 的 503，可安全重试；
- 只有「可能已到达 Node」的错误（超时、链路断开丢回复等）才 `mark_reconciling`，§2.5
  不重发的语义不变。

Rust 集成测试 `crates/remuda-hub/tests/node_busy.rs`：用一个永不回复的 fake Node 占满
32 个控制槽，真实 `POST /v1/instances` 返回 503 NODE_BUSY、`retryable:true`、
`retryAfterMs>0`，且该行 lifecycle 为 `failed` 而非 reconciling。

### 2.3 Web：列表不再饿死创建（`store.ts` / `api.ts` / `SessionList.tsx`）

- `api.screenRead` 识别 503 NODE_BUSY 抛 `ScreenNodeBusyError`；**round 2 修正（评审 6）**：
  `rest()` 解析响应里的 `retryAfterMs`（`HubHttpError` 透传），`ScreenNodeBusyError` 用该值，
  store 的回退严格按 Hub 的提示走，2500ms 只是缺省。
- store 屏幕轮询为有界调度器：**exited/failed 永不读屏**；全局**最多 4 个在途**且每行
  single-flight；SessionList 用 IntersectionObserver 把可视行排到队首；NODE_BUSY 只回退该行
  一个 Hub 提示周期，不打 console、不报错。
- SessionList 轮询 effect 门控在 `location.pathname === "/sessions"`：该组件在
  `/sessions/new` 后面 dimmed 常驻（手机弹层现场），会话页/弹层期间不再为任何会话发
  `/screen`。

### 2.4 Web：创建失败就地显示 Hub 错误（`NewSessionPage.tsx` / css）

4xx（除 408）与 **503 NODE_BUSY** 都是「可就地修正/安全重试的拒绝」（503 拒绝发生在帧发出
前，绝不可能已建会话）；错误文案为 `CODE · message`，`role="alert"` 就地显示、表单与草稿
全保留、开始按钮重新可用、出现即滚入视口，css `overflow-wrap: anywhere` 保证 390px 不溢出。
「创建结果没有确认」路径（504/断连/其它 5xx → 状态待确认、绝不二次创建）原样保留。

### 2.5 fake Node：运行时门控，无环境变量（`hub_e2e.rs`；评审 3/4/7）

- 删除 round 1 的 `HUB_E2E_EXITED_INSTANCES` / `HUB_E2E_SCREEN_DELAY_MS` / READY 附加字段。
- 唯一开关是运行时门控文件 `$TMPDIR/remuda-e2e-rpc-gate`：存在时 fake Node 把
  **tty.screen（只读半区）和 worktree.list（控制半区）**的帧停在帧队列里（已占 Hub
  pending 槽、不回复），删除后原帧重新入队、走各自常规分支。门控默认不存在，且文件在启动和
  Ctrl-C 时都会被清掉；**不启用门控时 fake Node 的每一个回复与 base 完全一致**——
  特别地，`tty.screen` 现在只有**一个** match arm：
  `if cooked-buffer（c-nextstep 的屏幕哨兵行；本分支留空钩子）→ 屏幕行`
  `else → send_rpc_ok({ ok: true })`（base 原回复），c-nextstep 落地时把哨兵集合折进同一个
  arm，不会产生重复字面 arm。

## 3. e2e：标准配置直接跑，无旋钮（390px）

`web/tests/e2e/ux-mobile-new.hub.spec.ts` 在**未改动的 `playwright.hub.config.ts`**、无任何
额外环境变量下运行。已退出行由 spec 自己通过 `page.request` 真实创建 claude-pty 实例再
`instance.close`（fake Node 会写真实 exited lifecycle 事件）得到；断言只统计本 run 创建的
id 的 `/screen`，卡片数用「不少于」，落点显式校验属于本 run（评审 5）。三例：

1. **14 个已退出行：0 个 /screen、0 个 500，两次创建都打开新会话页**（列表一次、会话页
   dimmed 弹层一次）。
2. **真实 Hub 控制半区饱和**：门控停住 32 个控制类调用（20 worktree.list + 占用其余槽的
   screen），UI 真实点 开始 → POST 真正返回 **503 NODE_BUSY + retryAfterMs**，弹层就地显示
   `NODE_BUSY · …`、表单保留、开始可再点；不再使用 `page.route` 桩（评审 2）。
3. **读饱和时创建仍成功**：门控停住 16 个 screen，40 个并发读 → 多出来的为 503 NODE_BUSY、
   0 个 500；UI 真实创建成功。

```
# 标准 hub config（仅给本 worker 分配端口），无 HUB_E2E_EXITED_*：
HUB_E2E_LISTEN=127.0.0.1:59150 HUB_E2E_WEB_PORT=59159 \
HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59151 \
  playwright test --config playwright.hub.config.ts ux-mobile-new.hub.spec.ts
= 1 = 3 passed
= 2 = 3 passed
= 3 = 3 passed
```

全量 hub e2e 套件（`flock e2e.lock-c`，同一套端口、未改 config、无旋钮）整跑通过：
**120 passed, 19 skipped, 0 failed**——证明 §2.5 的无门控零行为改动（已有 spec 的列表摘要
等回复不变）。

## 4. 390px 截图

按任务规则截图快照**不入库**；以下三张在 390×844 用 `REMUDA_EVIDENCE=1` 重跑本 spec 生成：

- `mobile-new-session-1-list-390.png` — 「已退出」卡片、0 个 /screen 请求。
- `mobile-new-session-1-session-390.png` — 创建后打开的新会话页。
- `mobile-new-session-1-node-busy-390.png` — 真实 503 NODE_BUSY：弹层底部就地显示
  `NODE_BUSY · …retry after 2500 ms`，开始按钮仍在、可用。

## 5. iOS / 软键盘（best effort）

本机 Ubuntu 20.04 无法安装 Playwright WebKit（`playwright install webkit`：
“does not support webkit on ubuntu20.04-x64”），共享浏览器 WS 端点是 Chromium CDP，
**WebKit/iPhone 13 项目未能在本机执行**。几何验证在 390×844 iPhone 13 视口（Chromium）做：
focus prompt 后合成收缩 `visualViewport.height` 到 504/400/320，开始按钮盒模型均落在收缩后
视口内、中心点 `elementFromPoint` 命中按钮自身；弹层 css 已从
`--workbench-height`（实时取 `window.visualViewport`）定尺寸，`Sheet.tsx` 未改。未能验证：
真机/Safari 的 visualViewport 事件时序与软键盘动画最终像素。
