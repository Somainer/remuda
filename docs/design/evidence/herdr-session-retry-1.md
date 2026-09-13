# herdr session server 重启竞态：实跑证据（herdr-session-retry-1）

工作树 `wt/x-nodeherdr/herdr-session-retry`，herdr 0.9.0，claude 2.1.270。
Hub 61280 / Node 61287，data dir 在 `/tmp/x-nh-*` 下。
下面所有 before/after 都是真实进程的真实输出，不是构造的 fixture。

## 0 结论

`remuda dev` 重启撞上上一个 Node 的 herdr session server 关闭窗口时：

| | before（`0e1e7fc`，修复前） | after（本分支） |
|---|---|---|
| 同一竞态跑 4 次 | **4/4 进程退出** | **4/4 正常 listening** |
| 退出信息 | `Error: driver error: herdr session.snapshot: server_unavailable: server is shutting down` | 无 error，进入 `remuda dev listening` |
| Hub | 一起挂掉，要手工重启 | 一直在 |

根因不是"没重试"，而是**探活用错了方法**：herdr 在整个关闭窗口里
`ping` 照常回 `pong`，只有非 `ping` 方法才回 `server_unavailable`。
`HerdrServer::ensure` 原本用 `ping` 探活 → 把正在退出的 server 判成健康 →
直接挂客户端上去 → 第一个 `session.snapshot` 就撞上 `server_unavailable` →
`reconcile_herdr` 的 `?` 把整个 `remuda dev` 带走。

## 1 herdr 关闭窗口的实测行为（这决定了修法）

对真实 `herdr server` 发 `server.stop`，然后以 30ms 间隔同时打 `ping` 和
`session.snapshot`，只打印状态变化点：

```
$ python3 hammer.py <sock>            # 10 workspaces，各跑一个进程
--- ping ---
  +  0.11s  OK                        ← 关闭期间 ping 一直成功
  +  0.97s  CONN:2                    ← 直到 socket 被 unlink
--- session.snapshot ---
  +  0.21s  ERR:server_unavailable    ← 真正的工作方法立刻被拒
  +  0.94s  CONN:2
```

herdr 自己的日志确认这是它的正常行为：

```
INFO herdr::server::headless::lifecycle: completing server shutdown
INFO herdr::logging: api request completed … outcome="error" method="session.snapshot"
INFO herdr::pane: pane session terminated pane=1 pid=94727 signal=Kill
INFO herdr::logging: herdr exiting event="app.shutdown" … pid=94618
```

`strings $(which herdr)` 里也能看到这个常量：

```
{"id":"","error":{"code":"server_unavailable","message":"server is shutting down"}}
```

**所以：`ping` 不能用来判断 server 可不可用。** 修复后所有探活都改用
`session.snapshot`。

窗口长度随 pane 数和进程是否吃 `SIGTERM` 变化：空 server 约 0.15s，
10 个 pane 约 1s，被卡住的子进程会更久（任务描述里观察到 40–120s）。
所以等待必须是有上界的轮询，不是固定 sleep。

## 2 复现脚本

`/tmp/x-nh-race.sh`：起一个真的 `herdr server` 占住 Node 将要拨的那个 socket，
建 8 个 workspace，然后 `server.stop` 与 `remuda dev` 同时发车——
这就是"`remuda dev` 起来时旧 herdr 还在关"的原样。

```sh
env … HERDR_SOCKET_PATH=$SOCK HERDR_SESSION=remuda-node-race herdr server &
for n in 1..8; do  workspace.create  ; done
python3 rpc.py $SOCK server.stop '{}' &        # 关闭窗口开始
$BIN dev --data-dir $DATA --port 61287 …  &    # 同时启动 Node
```

## 3 before：修复前，进程退出

`0e1e7fc`（本分支的父提交）单独 build 出来的二进制：

```
$ /tmp/x-nh-race.sh /tmp/x-nh-baseline-target/debug/remuda /tmp/x-nh-before …
herdr=55990 node=56659
RESULT: remuda dev EXITED

$ cat /tmp/x-nh-before/dev.log
INFO using Claude binary from PATH path=/Users/…/claude
INFO loaded node identity path=/tmp/x-nh-before/enrollment.json host_id=hst_01a09c16-…
Error: driver error: herdr session.snapshot: server_unavailable: server is shutting down
```

与缺陷报告里的字符串逐字一致。连跑 3 次，3 次都是同一条：

```
--- before run 1 ---  Error: driver error: herdr session.snapshot: server_unavailable: server is shutting down
--- before run 2 ---  Error: driver error: herdr session.snapshot: server_unavailable: server is shutting down
--- before run 3 ---  Error: driver error: herdr session.snapshot: server_unavailable: server is shutting down
```

注意它是在 `loaded node identity` 之后、`remuda dev listening` 之前死的——
Hub 根本没起来。

## 4 after：修复后，等待并正常启动

同一个脚本、同一个竞态，换成本分支的二进制：

```
$ /tmp/x-nh-race.sh …/target-x-nodeherdr/debug/remuda /tmp/x-nh-after2 …
herdr=89437 node=90197
RESULT: remuda dev ALIVE

$ cat /tmp/x-nh-after2/dev.log
INFO using Claude binary from PATH path=/Users/…/claude
INFO loaded node identity  path=/tmp/x-nh-after2/enrollment.json host_id=hst_01a09c1b-…
INFO persisted node identity …
INFO remuda dev listening hub=127.0.0.1:61280 node=127.0.0.1:61287
INFO development access code file file=/tmp/x-nh-after2/dev-hub/bootstrap-token
INFO shutdown requested                      ← 脚本自己收尾，不是崩
```

连跑 3 次，3 次都 ALIVE、`errors=0`：

```
run 1 | before: EXITED (errors=1) | after: ALIVE (errors=0)
run 2 | before: EXITED (errors=1) | after: ALIVE (errors=0)
run 3 | before: EXITED (errors=1) | after: ALIVE (errors=0)
```

旧 server 的 pane 已经随它一起没了，所以这些 Instance 被标成
`exited / carrier-shutdown`，而不是假装还活着——重启拿回来的是**载体**，
不是 pane。

## 5 运行中掉载体（第 2 项）

Node 正常跑起来、起了一个 `claude-pty` Instance 之后，
对它的 herdr server 发 **SIGKILL**（连关闭流程都没有）：

```
$ kill -KILL 12898          # carrier pid
$ curl -X POST …/commands -d '{"operation":"send","prompt":"hello"}'
send http=200
RESULT: remuda dev ALIVE
```

dev.log：

```
INFO starting herdr server session=remuda-node-7ef594946f2a1984d6c1fc27 socket=…/herdr.sock binary=herdr
WARN herdr carrier lost mid-session; attempting bounded recovery
     error=driver operation failed: carrier unavailable: herdr unreachable (…/herdr.sock)
INFO starting herdr server session=remuda-node-7ef594946f2a1984d6c1fc27 socket=…/herdr.sock binary=herdr
```

对应 Instance 的 journal（`/v1/instances/{id}/journal`）：

```
seq= 13 pty-prompt-error         sev=error  driver operation failed: carrier unavailable: herdr unreachable (…)
seq= 15 herdr-carrier-lost       sev=error  herdr session server was lost and restarted; this pane did not survive
seq= 16 herdr-carrier-restarted  sev=info   a fresh herdr session server is available for new instances
seq= 17 lifecycle ready -> failed  reason=herdr-carrier-lost
```

即：Node 不死，载体自动拉起来，受影响的 Instance 用 journal 诊断说清楚
"是载体没了，不是 agent 自己错了"。**不会**偷偷重开 agent——要不要续，
留给人决定。

## 6 修复内容

`crates/remuda-herdr`

* `retry.rs`：`RetryPolicy`，指数退避（50ms→1s 封顶），总预算默认 120s。
  不看时钟，`elapsed` 由调用方喂进来，所以单测是确定性的。
* `server.rs`：`endpoint_state()` 用 `session.snapshot` 探活，区分
  `Absent / Ready / ShuttingDown`。`ensure` 撞到 `ShuttingDown` 就有界等待；
  预算耗尽则换一个带后缀的 session 名启动并 `WARN`，**绝不**把错误抛给 Node。
  后缀目录取**兄弟目录**（`herdr-<suffix>`）而不是子目录——子目录属于还活着的
  那个进程，而且路径会超过 macOS `sun_path` 的 ~104 字节上限（实测踩到）。
* `error.rs`：新增 `Unreachable`（连都没连上，请求没发出去，可以重试）
  与原有 `Disconnected`（写完才断，结果未知，**不重试**）分家；
  `is_server_unavailable()` / `is_transient_carrier()`。

`crates/remuda-node`

* `carrier_recovery.rs`（新）：运行中丢载体的有界恢复，journal 诊断，
  `CarrierSupervisor` 让 Instance worker 用弱引用触发恢复而不阻塞命令结算。
* `reclaim.rs`：`reconcile_herdr` 同样有界等待；等不到就把那批 pane 按
  `carrier-shutdown` 退役，而不是让 Node 起不来。`agent_list` 紧跟在
  `session.snapshot` 后面，也会撞上同一个窗口，一并处理。

`crates/remuda-driver`：`DriverError::CarrierUnavailable`，与
"agent 没准备好"的 `ControlUnavailable` 分开。

## 7 两个实现过程中发现并修掉的问题

1. **只读调用的 `Disconnected`**。第一版 after 跑出来变成
   `Error: driver error: herdr disconnected (…/herdr.sock)`——窗口末尾 socket
   是在读的过程中断的，被我按"结果未知"挡在重试之外。`session.snapshot`
   是只读的，断了就等于"对端没了"，没有不确定性，所以在 reconcile 这条路上
   与 `Unreachable` 同等对待。prompt 这类会改状态的调用仍然不重试。
2. **恢复路径没用配置里的 herdr 二进制**。测试跑完机器上多出 4 个真的
   `herdr server` 常驻进程，才发现 `recover_herdr_carrier` 直接走了 PATH 上的
   `herdr`，把测试特意指定的 fake 绕过去了——也就是说那版测试是"过得不对"。
   已改为走 `config.herdr_binary`，并在测试里加了断言：出现 herdr 才会写的
   `sessions/` 目录就算失败。

## 8 检查

```
cargo fmt --all                                              ok
cargo clippy --workspace --all-targets --locked -- -D warnings   ok（无 warning）
cargo test --workspace --locked                              见下（本改动相关的全过）
./scripts/ci/secret-scan.sh                                  secret-scan: pass
```

`cargo test --workspace` 在本机当前负载下有 **1 个与本改动无关的既有不稳定用例**
会偶发失败，在 `remuda-node --lib` 的这两个之间跳：

```
native::tests::registry_constructs_all_three_native_claude_drivers
  … Claude startup configuration /Users/…/.claude could not be read:
    workspace … is inaccessible: access probe timed out
stdio::tests::composed_stdio_dispatches_create_and_streams_journal
  … panicked at stdio.rs:697: hello deadline: Elapsed(())
```

判定为环境问题，不是本改动引入的，依据三条：

1. **同样的用例在未改动的 `0e1e7fc` 上也失败。** 把父提交 checkout 到
   `$HOME` 下（避开 `workspace_roots` 干扰）跑 `-p remuda-node --lib`，
   失败的就是上面**同一对**用例。
2. **不确定性**：同一棵树连跑多次，失败的用例在这两个之间漂移，
   `--test-threads=1` 也照样失败——是 deadline 输给机器负载，不是逻辑错。
3. **成因明确**：`registry_…` 会用 3s 预算（`PROBE_TIMEOUT`）去探真实的
   `~/.claude`，而这台机器上它有 **5.6 GB / 16639 个文件**；
   跑测试时 `load average ≈ 26`，同机还有别的 worktree 在跑
   `cargo test --workspace` 和 `remuda dev`。

本改动自己新增和触及的用例（`remuda-herdr` 全部、`remuda-node`
`carrier_recovery` / `reclaim` / `runtime::pty_queue`）在每一次运行中都是通过的。

新增测试：

* `remuda-herdr` 单测 `retry::tests` ×4：退避增长与饱和、预算耗尽、
  最后一步不超预算、自定义边界不倒挂。
* `remuda-herdr` 集成 `tests/session_retry.rs` ×4：假端点先
  `server_unavailable` 后 ok；**`ping` 骗人**那条单独立一个用例；
  `ensure` 等待后正常启动；预留不退时走后缀 session。
* `remuda-node` `carrier_recovery::tests` ×4：启动期 reconcile 撞关闭窗口、
  运行中丢载体后恢复 + journal 诊断、载体健康时恢复是 no-op、
  载体丢失与普通 driver 错误的判别。

反向验证（确认测试不是白过的）：把 `reclaim.rs` 里的等待改回原来的直接
`session_snapshot()`，`startup_reconcile_survives_a_shutting_down_predecessor`
立刻失败，且失败信息就是线上那条：

```
reconciliation must not fail the Node:
  Driver("herdr session.snapshot: server_unavailable: server is shutting down")
```

## 9 没做的事

* 没有做 pane 级别的续跑。server 一死，pane 里的 agent 进程就没了，
  重启载体救不回来；硬续只会造出一个假装还在的会话。
* 后缀 session 的兜底不会去清理卡住的旧 server，故意留给人工恢复。
* `web/` 未改动。
