# Evidence: Hub 可被托管运行（c-hubsupervise-1）

Date: 2026-09-23。Design: [hub-supervise.md](../hub-supervise.md)。
闸门：Linux devbox（`x86_64-unknown-linux-gnu`，rustc 1.94.1）。
本文不截图；证据是工作树 file:line 锚点与本地可复跑的命令输出。

## 1. 交付物与归属

| 文件 | 性质 |
| --- | --- |
| `crates/remuda-hub/src/supervise.rs` | 新建，独占：父死亡观察器（Linux prctl + starttime 轮询 / macOS kqueue）、`hub.pid` guard、版本与 uptime |
| `crates/remuda-hub/src/lib.rs` | 接线：`pub mod supervise`、`spawn_inner` 里 `mark_started()`、`RunningHub::shutdown_within(budget)` |
| `crates/remuda-hub/src/http.rs` | 只改 `healthz`：保留 `ok`，新增 `version` / `uptimeSecs` |
| `crates/remuda/src/cmd/hub.rs` | `--managed <parentPid>` 参数、武装/关停/ pid 文件接线 |
| `crates/remuda/tests/hub_supervise.rs` | 新建，4 个真实二进制集成测试 |
| `crates/remuda-hub/Cargo.toml`、`crates/remuda/Cargo.toml` | nix 0.28（hub: process/signal/event；remuda dev: signal） |
| `crates/remuda-hub/openapi/openapi.json`、`web/src/lib/api.generated.ts`、`crates/remuda-hub/README.md` | `/healthz` 契约同步 |
| `docs/design/hub-supervise.md`、本文 | 新建 |

未新增任何 HTTP 路由（无 `/shutdown`、无 `/version`）；未改 wire、schema、
`deploy/`；未实现 hub.lock（设计文档 §3.1 注明依赖
`docs/design/hub-topology.md` §3）。

## 2. 设计主张 → 代码锚点

| 主张 | 锚点 |
| --- | --- |
| `--managed <u32>`，与 healthcheck/migrate 互斥 | `crates/remuda/src/cmd/hub.rs:24-25` |
| 绑定端口前武装观察器，死父进程立即拒绝启动 | `crates/remuda/src/cmd/hub.rs:122-128`；`supervise.rs:84-92`、`supervise.rs:154-158` |
| 端口绑定成功后才写 pid 文件，0600，drop 时删除 | `crates/remuda/src/cmd/hub.rs:131-136,165`；`supervise.rs:102-141` |
| SIGTERM 与父死亡汇入同一个 `tokio::select!`，同一关停路径 | `crates/remuda/src/cmd/hub.rs:137-159` |
| 3 秒关停预算：停接受 → drain → WAL checkpoint | `crates/remuda-hub/src/lib.rs:578-606`（writer checkpoint 见 `crates/remuda-hub/src/store.rs:1493-1498`） |
| 非托管仍走旧 `drop(running)` 路径（语义不变） | `crates/remuda/src/cmd/hub.rs:160-164` |
| Linux：直接父才装 `PR_SET_PDEATHSIG(SIGTERM)` | `supervise.rs:231-245` |
| Linux：250 ms 命名 pid 轮询 + `/proc` starttime 防 pid 复用 | `supervise.rs:44`、`supervise.rs:187-227` |
| macOS：kqueue `EVFILT_PROC/NOTE_EXIT`，ESRCH 即已退出，失败降级轮询 | `supervise.rs:289-335` |
| `kill(pid,0)`：EPERM 算存活 | `supervise.rs:174-183` |
| `/healthz`：`ok` 保留 + `version` + `uptimeSecs`（进程级起点） | `crates/remuda-hub/src/http.rs:289-295`；`supervise.rs:48-63` |
| OpenAPI 三字段 required（含 `ok`） | `crates/remuda-hub/openapi/openapi.json` `/healthz` 节 |

## 3. 测试证据

命令（可复跑，不开外网）：

```text
cargo test --locked -p remuda --test hub_supervise -- --test-threads=1 --nocapture
```

结果（devbox，2026-09-23）：

```text
test healthz_reports_version_and_monotonic_uptime ... ok
test managed_hub_exits_when_parent_is_killed ... hub self-exited 77.637363ms after supervisor SIGKILL
test managed_hub_shuts_down_within_three_seconds_on_sigterm ... managed SIGTERM shutdown completed in 17.852998ms
test unmanaged_hub_runs_unsupervised_like_before ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.23s
```

逐条对应验收：

1. **SIGTERM 3 秒内干净关停**：实测 17.9 ms（预算 3 s，余量两个数量级），
   退出码 0，`hub.pid` 在退出后消失；同一用例还断言 pid 文件内容 == Hub
   pid、权限恰为 0600。
2. **父进程被杀后 Hub 自退**：父为 `sh -c '… & wait'`，对父发 **SIGKILL**
   （父无法 trap、无法转发信号），Hub 77.6 ms 内自行消失并删除 pid 文件。
   这是 Linux prctl + starttime 轮询路径。
3. **`/healthz` 新字段**：`ok==true`、`version==CARGO_PKG_VERSION`、
   `uptimeSecs` 为非负整数且间隔 1.1 s 后严格增大。既有
   `remuda-hub/tests/hub.rs::healthz_ok` 继续保证 `ok` 键存在。
4. **不带 `--managed` 逐字节同旧行为**：不写 pid 文件；SIGKILL 父 shell、
   等过观察器轮询周期 4 倍时长（1 s vs 250 ms）后，被收养的 Hub 仍然存活且
   `/healthz` 200，只响应自己的 SIGTERM。

## 4. 闸门清单

```text
cargo fmt --all                                                        # clean
cargo clippy --locked --all-targets -p remuda-hub -p remuda -- -D warnings   # clean
cargo test --locked -p remuda-hub -p remuda                             # 全绿
```

`supervise` 另有 6 个模块内单测（`cargo test -p remuda-hub --lib supervise`）：
非法 pid 拒绝启动、null-signal 验活、`/proc/self/stat` starttime 读取、
**伪造 starttime 时存活 pid 也必须判死（pid 复用守卫）**、pid 文件生命周期
与 0600、uptime 单调增长。

另做了二进制级冒烟：`hub --managed <死pid>` 退出码 1；
`hub --managed 0` 报 positive 校验错；`hub --managed 1 --healthcheck`
被 clap 拒绝（退出码 2）。

未触碰 `docs/design/protocol.md`，无需跑 `wire_golden`。

说明：本闸机共享、高负载，全量并行跑时存在**既有**调度/帧序时序 flake。
为排除本任务引入嫌疑，曾把工作树 `git reset --hard` 到本任务的 base
（`2b4974f2`）跑同一全量并行命令：base 同样出现
`gate_queue::fifo_order_is_preserved_on_one_lane` 与
`gate_queue::land_jobs_are_serialized_and_the_second_lands_after` 失败，
证明 flake 与本任务无关（本任务对 `spawn_inner` 的运行期改动仅一次
`mark_started()`，不碰 gatequeue 调度与 api_relay 帧序；相关用例
`--exact` 单跑均通过）：

- `tests/gate_queue.rs::fifo_order_is_preserved_on_one_lane`：base 全量并行
  复现失败；本分支单测复跑两次通过、整文件 `--test-threads=1` 18/18；
- `tests/api_relay.rs::proxy_reconnect_gets_fresh_api_egress_before_next_open`
  （socket 帧序竞态）：`--exact` 单测复跑通过。

## 5. 未覆盖与手工验收

- **macOS kqueue 分支**在 Linux 闸门不编译（`#[cfg(target_os="macos")]`），
  闸门上也没有 apple target 可交叉 `cargo check`；已对照本机 vendored
  nix 0.28 源码逐形核对（`KEvent::new` 第 4 参为 `FilterFlag` 非 bits、
  方法名为 `Kqueue::kevent(.., Option<timespec>)`、`EVFILT_PROC` 与
  `NOTE_EXIT` 在 apple_targets 可用）。运行期行为仍需协调员在 Mac 上手工
  验，步骤见 `docs/design/hub-supervise.md` §8（SIGKILL 父进程亚秒自退 +
  pid 文件清理、死 pid 启动拒绝、SIGTERM 3 秒、healthz 三字段）。
- hub.lock 未实现（任务约束）；其规格是本设计 §3.1 引用的依赖。
