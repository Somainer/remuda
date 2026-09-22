# Hub 状态持久化、单例锁、备份、恢复与迁移：现状锚点与验收记录

Date: 2026-09-22. Batch: c-hubstate（合并 c-topo-spec 中姿态无关部分、
c-hub-lock、c-hub-backup、c-restore-suppress、c-migrate-runbook）。
Scope: 新文档 + 一个本地脚本 + 一个本地测试 + 两处 doc comment；不改 Rust
行为、不改 store schema、不改 wire 版本、不引入依赖、不动 `deploy/`、不联网络。

Design references: [hub-topology.md](../hub-topology.md)（本批权威节点）、
[protocol.md §7.7](../protocol.md)（Hub 身份密钥规格，节起 `protocol.md:1613`）、
[deploy-runbook.md](../deploy-runbook.md)（换机迁移节）、
[decisions.md](../decisions.md)（D-018/D-019/D-030/D-031/D-035/D-049）。

本文不截图；证据是当前工作树的 file:line 锚点与本地可复跑的脚本测试。

## 1. 所有者裁定与范围

1. 本轮**不做公网前门**，暴露姿态（D-031 重议）暂不决；本批只交付无论姿态
   如何都成立的部分。新文档不描述任何公网可达/会合机制，也不假设其获批。
2. 交付物：
   - `docs/design/hub-topology.md`（新）：失效模型（组件 + 笔记本六种工况）、
     `hub.lock` 规格、`data_dir` 清单、加密备份口径、恢复副作用抑制、迁移不变量；
   - `scripts/hub-backup.sh`（新，可执行）：age 强制、默认 dry-run、
     `--execute` 双确认、纯本地、不写 `deploy/`；
   - `scripts/tests/test_hub_backup_dryrun.sh`（新，可执行）：纯本地 32 条断言；
   - `docs/design/deploy-runbook.md`：追加「Hub 换机迁移」节；
   - `crates/remuda-hub/src/store.rs`、`crates/remuda-hub/src/alerts.rs`：
     仅 doc comment（`git diff --numstat` 显示 13 + 14 = 27 行纯新增、0 删除）。
     为不移动其它任务文档引用的行号，store.rs 注释插在最高外部引用
     （`store.rs:4441`）之后的 `try_open_conn` 上（`store.rs:4768-4780`），
     alerts.rs 注释插在外部引用 `alerts.rs:159-160` 之后的 `classify` 上
     （`alerts.rs:187-200`）。

## 2. 现状锚点（逐条可复核）

| 主张 | 锚点 | 现状 |
| --- | --- | --- |
| Node 主动出站，传输模式仅两种 | `crates/remuda-protocol/src/enums.rs:42-45` | `OutboundWss`/`SshTunnel` 枚举 |
| Node 握手上报每实例水位 | `crates/remuda-node/src/daemon.rs:576-584` | hello 参数 `instanceWatermarks` |
| Hub 回应后按水位追平 | `daemon.rs:694`、`daemon.rs:783-815` | `sent = resume_watermarks(node, result)` |
| 30 秒 ACK 硬失败只退控制器 | `daemon.rs:659-662` | 超时报 `journal acknowledgement timed out; reconnect required` |
| journal 主键幂等 | `crates/remuda-hub/src/store.rs:4876-4883` | `PRIMARY KEY (instance_id, seq)` |
| 已存在行标 `replayed=true` | `store.rs:8137-8148` | PK 命中走重放分支，durable 不动 |
| 缺口新插入行 `replayed=false` | `store.rs:8169-8192` | INSERT + 全部投影后返回新鲜行 |
| 重放行不扇出副作用 | `crates/remuda-hub/src/ws.rs:633-642` | `if !appended.replayed` 才 publish/observe/usage/supply |
| interaction 唤醒/push 扇出点 | `crates/remuda-hub/src/alerts.rs:75-84`、`alerts.rs:119-124`、`alerts.rs:148-185` | `observe`→`dispatch`→`fanout`；badge `alerts.rs:180` |
| egress 上下文仅存内存 | `crates/remuda-hub/src/api_relay.rs:99-100` | `egress_contexts` 进程内表 |
| 重连即按持久路由重装凭据 | `api_relay.rs:407-453`；触发 `crates/remuda-hub/src/ws.rs:332-336` | hello 回复入队后 spawn reinstall |
| 安装帧携带网关密钥 | `api_relay.rs:294-355` | `EgressSnapshot` + `METHOD_API_EGRESS` |
| Hub WAL 单 writer 纪律 | `store.rs:4730-4731`、`store.rs:4789` | 注释自述单 writer；`journal_mode=WAL` |
| `Store::open` 现有打开点 | `store.rs:1468-1470` | 建目录、拼 `hub.sqlite`；锁在实现批于此处之前获取 |
| hub.lock doc comment 锚点 | `store.rs:4768-4780` | `try_open_conn` 注释，位于全部外部行号引用之后，插入不移动任何既有锚点 |
| 恢复抑制 doc comment 锚点 | `crates/remuda-hub/src/alerts.rs:187-200` | `classify` 注释，位于外部引用的 `alerts.rs:159-160` 之后 |
| writer 线程 Stop 时 checkpoint | `store.rs:1496`、`store.rs:138-145` | `PRAGMA wal_checkpoint(TRUNCATE)`；drop join writer |
| 主库存权威表 | `store.rs:4801-4818`（hosts）、`store.rs:4844-4860`（instances）、`store.rs:4925-4937`（interactions）、`store.rs:4957-4970`（passkeys） | 全部随 `hub.sqlite` 备份 |
| secret envelope 文件 | `crates/remuda-hub/src/lib.rs:616`；`crates/remuda-driver/src/secrets.rs:54-64` | `secrets/master.key` + `secrets/secrets.json`，`0700`/`0600` |
| VAPID 私钥 | `crates/remuda-push/src/vapid.rs:18-21` | `vapid.json`，缺失自动生成新 P-256 |
| push 订阅库 | `crates/remuda-push/src/store.rs:31-34` | `push.sqlite`，WAL |
| bootstrap 落盘/轮转 | `crates/remuda-hub/src/auth.rs:62-68`、`auth.rs:117-121` | `0600` + sync；`rotate_bootstrap` 已存在 |
| 轮转 CLI | `crates/remuda/src/cmd/hub.rs:52-59` | `remuda hub rotate-bootstrap` |
| passkey RP ID 派生 | `crates/remuda-hub/src/passkeys.rs:152-156`、`passkeys.rs:170-184` | origin 完整 host 即 RP ID |
| SIGTERM/SIGINT 处理 | `crates/remuda/src/main.rs:56-83`；`crates/remuda/src/cmd/hub.rs:117-127` | 两信号都装；退出触发优雅关闭 |
| Node 持久 host token 回写 | `crates/remuda-node/src/daemon.rs:688-690` | hello result `nodeToken` 落 enrollment |
| Node 重连退避 | `crates/remuda-node/src/transport/mod.rs:36-52` | 1s→30s、±25% |
| Node 只回 Ping 不主动发 | `crates/remuda-node/src/transport/wss.rs:529-532` | 休眠半开判定依据 |
| follow 快照/gap resync | `crates/remuda-hub/src/ws.rs:190`、`ws.rs:241`、`ws.rs:1381-1390`、`ws.rs:1408-1424` | 广播容量满即 gap，重连快照重建 |
| 启动回收动作 | `crates/remuda-hub/src/lib.rs:608-617`、`lib.rs:652-658` | 标离线、过期 requested、gate 队列对账 |
| 身份私钥路径与丢钥后果 | `docs/design/protocol.md:1654-1670` | 恢复缺钥 = 全体 Node pin 失效 |
| 签名信封形状（恢复信封复用） | `docs/design/protocol.md:1737-1749`；必签帧现状 `protocol.md:1722-1735` | §7.7 尚未含 resume 信封，文档已点名实现批补 |

## 3. 脚本安全闸的测试证据

`bash scripts/tests/test_hub_backup_dryrun.sh`：32 passed, 0 failed。
纯本地、无网络、无 age 依赖（execute 路径用一个刻意不含 age 的最小 PATH
断言 fail-closed）。覆盖：

1. 无 `--recipient`/`AGE_RECIPIENT`：任何模式退出码 2，错误点名
   `AGE_RECIPIENT`；
2. 默认 dry-run：退出 0，输出含 `DRY-RUN` 标记与全部 9 个关键成员
   （含 `hub-identity/identity.ed25519`）、丢钥后果说明；recipient 只以
   指纹出现；不写任何 `.tar`/`.age`；
3. `AGE_RECIPIENT` 环境变量等价于 `--recipient`；
4. `--execute` 缺 `REMUDA_BACKUP_YES_I_KNOW=1`：退出码 2 且无产物；
5. 确认变量齐备但本机无 `age`：退出码 4、错误明确、无产物；
6. 核心成员缺失：dry-run 也退出码 3 并点名缺失成员；
7. 输出目录位于 `deploy/` 下：退出码 5；
8. 脚本与测试自身不含远端传输命令、隧道工具名、IP 字面量、家目录路径。

## 4. 关键设计取舍记录

- **身份私钥命名以 §7.7 为准**：任务简报使用 `hub_identity.key` 的简称，
  已落地的 main 规格定义为 `hub-identity/identity.ed25519(.pub)`；脚本与
  文档按 §7.7 路径收录整个 `hub-identity/` 目录，dry-run 清单显式打印私钥
  文件相对路径。
- **push 文件位置按代码而非旧规划**：规划文档曾写 `<data_dir>/push/`，
  现状代码把 `vapid.json` 与 `push.sqlite` 直接放在 `data_dir` 根
  （`vapid.rs:21`、`push/store.rs:31`），脚本按现状收录。
- **manifest 签名如实标 pending**：§7.7 未实现，脚本不伪造签名，内部
  manifest 写 `signature: null` 与原因；外部只落一个不含秘密值的
  `.sha256` 旁车。
- **抑制层只放外向副作用**：journal/interactions/usage/supply 等持久投影
  照常应用（`ws.rs:635-638`、`store.rs:8174-8178`），被抑制的穷举为
  interaction 唤醒、push、`api.egress` 重装三类（hub-topology.md §6.3）。
- **单例锁只管同机**：跨机双活不靠锁，靠迁移顺序（先 SIGTERM）+
  bootstrap 必轮转（deploy-runbook.md 换机迁移节）。

## 5. 扫描与编译复核

- `scripts/ci/no-tunnel-scan.sh`：新增/修改文件不含 D-031 禁用清单任何
  token（新文档只以「D-031 禁止清单上的隧道类工具」指代，不列工具名）。
- 新增文档与脚本不含 IP 字面量、家目录绝对路径、域名、token 值；
  数据目录在运行时输出里只显示 basename，bundle manifest 不录绝对路径。
- `cargo check -p remuda-hub --locked`：通过（仅注释新增）。
- `python3 -B -m unittest discover -s scripts/tests -p 'test_*.py'`：
  既有 docs 静态测试（含 protocol.md §7.7 锚点）仍通过；本批未改其文件。
