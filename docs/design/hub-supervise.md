# Hub 可被托管运行（c-hubsupervise）

Date: 2026-09-23。桌面计划 M1（`~/Projects/remuda-agents/briefs/desktop-app.md`
c-hub-supervise 节）。本文是该任务的权威设计；与原 brief 冲突处以下方
§1「对原 brief 的修正」为准。

Scope：把 `remuda hub` 从「终端窗格里的一个进程」变成可被托盘小工具 / 桌面
shell 拉起的常驻子进程——托管父进程消失时自己退出、SIGTERM 3 秒内干净
关停、写 PID 文件、`/healthz` 暴露版本与运行时长。**不新增监听端口、不新增
关停网络接口、不改 wire 版本、不改 store schema。**

## 1. 对原 brief 的修正（所有者裁定）

1. **不新增 `/shutdown` 接口。** 原设计用请求体里的父进程 pid 作鉴权，但 pid
   在本机用 `ps` 即可看到，而 Hub 未来可能被公网到达，这等于开了一个远程关停
   口子。托管方本来就是父进程，**关停走 SIGTERM**，不需要任何网络接口。
2. **不新增独立 `/version` 接口。** 版本信息并入**已有** `/healthz`，新增
   `version` 与 `uptimeSecs` 两个字段；**`ok` 字段不删除、不改名**（现有探针、
   Caddy 健康检查、Web 客户端都依赖它）。
3. **路径修正**：brief 写的 `crates/remuda-cli/src/cmd/hub.rs` 不存在，hub
   子命令实际在 `crates/remuda/src/cmd/hub.rs`。

## 2. CLI 契约

```text
remuda hub --managed <parentPid>
```

- `--managed <u32>`：由托管方（tray helper / 桌面 shell / launchd 包装）传入
  **它自己的 pid**。与 `--healthcheck`、`--migrate` 互斥（clap
  `conflicts_with`），可与 `--with-dispatcher` 同用。
- 参数在绑定监听端口**之前**校验并武装观察器：
  - pid 必须为正、不能是 Hub 自身；
  - 命名的父进程启动时必须存活，否则 Hub 以非零码退出——**不允许「带着死
    托管方」启动**。
- 不带 `--managed` 时行为与本任务之前**逐字节一致**（见 §7 的对照测试）。

## 3. 托管模式关停序列

两类触发走同一条关停路径：

- 收到 SIGTERM/SIGINT（既有 `Shutdown` 信号处理器，`crates/remuda/src/main.rs`）；
- 父进程消失（§4 的观察器完成）。

关停动作与时限（`RunningHub::shutdown_within(3s)`）：

1. 立刻向 Axum server 发 graceful shutdown：**停止接受新连接**，已建立连接
   进入 drain；
2. 最多等待 3 秒 drain，超时则 abort server task（下限 250 ms 收尾）；
3. 向 SQLite writer 发 `Job::Stop`：writer 在停止前执行
   `PRAGMA wal_checkpoint(TRUNCATE)`——**journal flush**
   （`crates/remuda-hub/src/store.rs` 的 writer Stop 分支）；flush 另保
   500 ms 下限，不与连接 drain 抢同一个 3 秒；
4. 进程退出码 0；
5. 删除 `<data_dir>/hub.pid`（drop guard，见 §5）。

非托管模式不改用这条路径：仍是原来的 `drop(running)`——只发 graceful
shutdown 请求、不等 drain（由进程退出和 tokio runtime 的
`shutdown_timeout` 收尾）。

### 3.1 hub.lock 依赖（本任务不实现）

`docs/design/hub-topology.md` §3 规格化了 `<data_dir>/hub.lock` 单例锁，
**代码里尚未实现**。按任务约束本任务**不**实现它；将来的实现批应保证
SIGTERM 关停在释放锁之前完成 checkpoint（同节规格第 4 条）。本任务的 PID
文件**不是**单例锁（见 §5），不承担互斥职责。

## 4. 父进程死亡检测（分平台方案与理由）

### 4.1 macOS：kqueue `EVFILT_PROC` + `NOTE_EXIT`（精确机制）

在专门的 `hub-parent-watch` 线程上建 kqueue，注册
`EV_ADD | EV_CLEAR`、`EVFILT_PROC`、`fflags = NOTE_EXIT`，ident = 托管方
pid，然后阻塞在一次 `kevent(2)` 里（注册与等待同一调用）：

- knote 绑定的是内核 **proc 对象**，不是 pid 数字——pid 被回收复用不会产生
  误判；
- 父退出时内核立刻投递事件，**无轮询延迟**；
- 注册时父已死：kevent 返回的事件带 `EV_ERROR`、data 为 `ESRCH`，立即触发
  关停；
- kqueue 创建/注册因任何其它原因失败：降级到 250 ms `kill(pid, 0)` 轮询。

**Linux 闸门编译不到这个分支**（`#[cfg(target_os = "macos")]`），由协调员
在 Mac 上手工验收（§8）。

### 4.2 Linux：`prctl(PR_SET_PDEATHSIG)` + `/proc` starttime 轮询（双保险）

两条机制同时武装，任一触发即开始同一套优雅关停，互不冲突：

1. **`prctl(PR_SET_PDEATHSIG, SIGTERM)`**：仅当 `--managed` 给的 pid 等于
   `getppid()`（tray → hub 的正常形态）时安装。父死的瞬间内核投递
   SIGTERM，Hub 走与操作员 SIGTERM 完全相同的关停路径。fork/exec 与
   prctl 之间的窗口靠安装后立刻做一次存活检查闭合（prctl(2) 对该竞态的
   经典告诫）。
2. **250 ms 轮询命名 pid**：`kill(pid, 0)`，`EPERM` 也算存活。轮询针对
   **参数里的命名 pid**（而非 `getppid()`），所以同时覆盖：
   - 托管方不是直接父进程（启动器/包装器形态，prctl 语义不适用）；
   - 裁剪过 prctl 的容器/内核；
   - subreaper 把孤儿收养成非 1 pid 的情形（单纯判断 `getppid()==1` 会漏）。
   为防 pid 回收复用误判，轮询在 arm 时快照 `/proc/<pid>/stat` 第 22 字段
   `starttime`（时钟滴答），之后只有「pid 存在 **且** starttime 不变」才算
   原托管方存活。

选这套而不是「只轮询 getppid」的理由：prctl 在直接父进程形态下给出内核级
即时投递（无 250 ms 延迟、无持续唤醒）；命名 pid + starttime 轮询兜住所有
非典型进程树。复用 `remuda-testing/src/parent_watch.rs` 已在闸门上验证过的
三机制思路，但本模块不做硬退出（Hub 有自己的 3 秒优雅关停，不需要观察器
线程 `exit(0)`）。

### 4.3 非 unix

`watch_parent` 直接报错退出（`--managed` 仅支持 unix；本项目部署目标只有
macOS 与 Linux）。

## 5. PID 文件

- 路径：`<data_dir>/hub.pid`（与 `hub.sqlite`、`listen` 同级），模式 `0600`，
  内容为 Hub 自身 pid + 换行；`fsync` 后才算写好。
- **写入时机在端口绑定成功之后**：陈旧 pid 文件永远不会指向一个启动失败的
  Hub（`listen` 文件也是绑定后才写）。
- 删除：`PidFile` guard 在 drop 时删除，即 server 已停、journal 已 flush
  之后。
- **它不是锁，也不是存活证明**：SIGKILL/断电后文件会残留，这是正常的；
  托管方必须 `kill(pid, 0)` 验活，绝不能靠「文件在不在」判断 Hub 在不在，
  也不应手工删它排障。互斥由未来的 hub.lock（hub-topology §3）负责。

## 6. `/healthz`

既有路由，匿名可访问（在 auth-establishing 名单内），响应由
`{"ok":true}` 扩展为：

```json
{ "ok": true, "version": "0.1.0", "uptimeSecs": 12 }
```

- `ok`：200 时恒为 `true`，语义不变；
- `version`：Hub crate 包版本（workspace 统一版本，等于 `remuda` CLI
  版本）；
- `uptimeSecs`：本 Hub 进程启动以来的整秒数（首秒为 0，单调非降）。进程级
  起点（`OnceLock<Instant>`，首次 `spawn` 时戳记），不是单个 in-process
  `RunningHub` 的年龄。

契约同步更新：`crates/remuda-hub/openapi/openapi.json`、
`web/src/lib/api.generated.ts`（与 `pnpm gen:api` 的输出形态一致）、
`crates/remuda-hub/README.md` HTTP 表。OpenAPI 三字段都列入 required——
`ok` 仍是其中之一，老探针不受影响。

## 7. 测试

集成测试 `crates/remuda/tests/hub_supervise.rs`（`#![cfg(unix)]`），全部驱动
**真实 `remuda` 二进制**（不经过 in-process `spawn`，因为父死亡与信号时序
是进程契约），不开外网：

1. `managed_hub_shuts_down_within_three_seconds_on_sigterm`：直启
   `hub --managed <测试进程pid>` → `/healthz` 就绪、`hub.pid` 内容为本 pid
   且权限 0600 → SIGTERM → 退出码 0、实测耗时 < 3 s、pid 文件删除；
2. `managed_hub_exits_when_parent_is_killed`：经 `sh -c '... & wait'` 包一层
   托管父，**SIGKILL 父 shell**（父没有任何机会转发信号）→ Hub 在预算内自己
   退出并删除 pid 文件。这是 Linux prctl + starttime 轮询路径；
3. `healthz_reports_version_and_monotonic_uptime`：`ok` 为 true、
   `version` == `CARGO_PKG_VERSION`、`uptimeSecs` 为非负整数且 1.1 秒后
   严格增大；
4. `unmanaged_hub_runs_unsupervised_like_before`：不带 `--managed` 时不写
   pid 文件；SIGKILL 父 shell 后 Hub 被收养但**继续服务**（证明非托管行为
   未变），只有自己的 SIGTERM 能停它。

另有既有 `crates/remuda-hub/tests/hub.rs::healthz_ok` 继续证明 `ok` 字段
存在。

## 8. macOS 手工验收（协调员，闸门跑不到 kqueue 分支）

在 Mac 上：

1. `cargo build -p remuda`；
2. 临时目录里 `sh -c 'remuda --data-dir D hub --managed $$ & h=$!; sleep 2;
   kill -9 $$'`（或先 `echo $$` 再另开终端 `kill -9 <pid>`），预期 Hub
   在亚秒内自行退出、`D/hub.pid` 消失；
3. `remuda --data-dir D hub --managed <一个不存在的 pid>` 必须立刻非零退出；
4. `remuda --data-dir D hub --listen 127.0.0.1:PORT` 后 `kill -TERM`，
   3 秒内退出码 0；
5. `curl -s localhost:PORT/healthz` 含 `ok/version/uptimeSecs`。

## 9. 明确不做

- 不做 `/shutdown`、`/version` 等任何新 HTTP 路由；关停只有信号一条路；
- 不实现 hub.lock（hub-topology §3 是依赖规格，留给实现批）；
- 不做 pid 文件验活之外的托管协议（重启策略、日志管道、launchd plist 由
  桌面壳任务负责）；
- 不改非托管模式的关停时序（保持 drop 语义）；
- 不做 Windows 支持。
