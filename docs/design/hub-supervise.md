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

## 4. 父进程死亡检测（方案与理由）

### 4.0 跨平台基线：250 ms `kill(pid, 0)` 轮询

**所有 unix 平台（Linux、macOS、其它）都运行同一段轮询代码**：专门的
`hub-parent-watch` 线程每 250 ms 对 `--managed` 命名的 pid 发一次空信号，
`EPERM`（进程属于别的用户）也算存活；查不到即判定托管方已退出，通知 run
loop 开始优雅关停。

这段代码 `#[cfg(unix)]`、**不绑定任何具体 OS**，因此在 Linux 合入闸门上
必然被编译、被集成测试实际跑到（§7）。父退出后最迟约 250 ms 被察觉——对
一个本地托管 Hub 完全可接受。

### 4.1 Linux：额外武装 `prctl(PR_SET_PDEATHSIG)`（加速，非唯一机制）

除轮询外，当命名 pid 恰为 `getppid()`（tray → hub 的正常形态）时，Linux
另装 `prctl(PR_SET_PDEATHSIG, SIGTERM)`：父死的**瞬间**内核投递 SIGTERM，
Hub 走与操作员 SIGTERM 完全相同的关停路径，不必等下一个 250 ms 轮询点。
fork/exec 与 prctl 之间的窗口靠安装后立刻做一次存活检查闭合（prctl(2)
对该竞态的经典告诫）。

轮询并不因此可有可无，它针对**参数里的命名 pid**（而非 `getppid()`），
同时覆盖 prctl 兜不住的形态：

- 托管方不是直接父进程（启动器/包装器形态，prctl 语义只认 fork 者）；
- 裁剪过 prctl 的容器/内核；
- subreaper 把孤儿收养成非 1 pid 的情形（单纯判断 `getppid()==1` 会漏）。

为防 pid 回收复用误判，Linux 轮询在 arm 时快照 `/proc/<pid>/stat` 第 22
字段 `starttime`（时钟滴答），之后只有「pid 存在 **且** starttime 不变」
才算原托管方存活。

### 4.2 macOS 与其它非 Linux unix：只用轮询（明确不做 kqueue 分支）

macOS 本来可以用 kqueue `EVFILT_PROC`/`NOTE_EXIT` 得到内核即时通知，
**本任务明确不做**，理由是工程性的而非机制性的：

- 该分支只能 `#[cfg(target_os = "macos")]` 编译，**Linux 合入闸门既编译
  不到也测不到**；本任务的第一版正是因此带着两处 macOS 编译错误上了
  Mac（`KEvent::data()` 在 macos 目标是 `isize` 不满足 `From<isize> for
  i64`；`&oneshot::Sender` 上调用会 move 的 `send`）。
- 在一条闸门永远走不到的路径上继续修补，只会一轮轮往返。
- 选择**合入闸门能编译、能测到的代码路径**优先；代价是 macOS 上父退出后
  最多约 250 ms 才察觉，托管 Hub 可接受。

macOS 路径与 Linux 轮询是**同一份代码**，闸门已经覆盖其编译与逻辑；协调员
在 Mac 上的验收（§8）只是真实系统上的运行期冒烟。

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
   退出并删除 pid 文件。跑的是跨平台轮询（外加 Linux 的 prctl 加速）；
3. `healthz_reports_version_and_monotonic_uptime`：`ok` 为 true、
   `version` == `CARGO_PKG_VERSION`、`uptimeSecs` 为非负整数且 1.1 秒后
   严格增大；
4. `unmanaged_hub_runs_unsupervised_like_before`：不带 `--managed` 时不写
   pid 文件；SIGKILL 父 shell 后 Hub 被收养但**继续服务**（证明非托管行为
   未变），只有自己的 SIGTERM 能停它。

另有既有 `crates/remuda-hub/tests/hub.rs::healthz_ok` 继续证明 `ok` 字段
存在。

## 8. macOS 运行期验收（协调员，合入前）

macOS 与 Linux 跑的是**同一份轮询代码**，闸门已覆盖其编译与逻辑；Mac 上的
验收是真实系统的运行期冒烟，重点看轮询在 macOS 的进程/信号语义下端到端
成立：

1. `cargo build -p remuda --locked` 必须直接编过（本任务不允许有闸门编不到
   的 macOS 专用代码）；
2. 临时目录里 `sh -c 'remuda --data-dir D hub --listen 127.0.0.1:0
   --managed $$ & sleep 2; kill -9 $$'`，预期父 shell 死后 Hub 在约 250 ms
   轮询周期内自行退出、`D/hub.pid` 消失；
3. `remuda --data-dir D hub --managed <一个不存在的 pid>` 必须立刻非零退出；
4. `remuda --data-dir D hub --listen 127.0.0.1:PORT` 后 `kill -TERM`，
   3 秒内退出码 0；
5. `curl -s localhost:PORT/healthz` 含 `ok/version/uptimeSecs`。

## 9. 明确不做

- 不做 `/shutdown`、`/version` 等任何新 HTTP 路由；关停只有信号一条路；
- **不做 macOS 专用 kqueue 分支**：只在 macOS 编译的路径闸门测不到，第一版
  即因此带编译错误；宁要 ~250 ms 检测延迟的同一份轮询代码（§4.2）；
- 不实现 hub.lock（hub-topology §3 是依赖规格，留给实现批）；
- 不做 pid 文件验活之外的托管协议（重启策略、日志管道、launchd plist 由
  桌面壳任务负责）；
- 不改非托管模式的关停时序（保持 drop 语义）；
- 不做 Windows 支持。
