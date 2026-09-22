# Hub 拓扑与状态：持久化、单例锁、备份、恢复与换机迁移

> **状态（2026-09-22，c-hubstate）**：docs-only 权威节点 + 一个本地备份脚本。
> 本文不改动 Rust 行为、store schema、wire 版本；两处 Rust 文件只加 doc comment。
>
> **所有者裁定（2026-09-22）**：
> 1. **本轮不做公网前门。** 笔记本如何被公网设备够到（前门、会合机制、任何公网
>    可达形态）不在本批派工，本文不描述这些机制、也不假设其中任何一种获批。
> 2. **暴露姿态（D-031 重议，计划中称 D1）暂不决。** 未拍板前 D-031 现状持续
>    有效（决策原文 `docs/design/decisions.md:74`）。
>
> 因此本文只收敛**无论暴露姿态如何都成立**的部分：Hub 在笔记本上的失效模型、
> 数据目录（`data_dir`）里到底有什么、单例锁、加密备份、恢复后的副作用抑制、
> 换机迁移不变量。另一份规划文档（任务简报附件）中的候选拓扑、公网前门与
> 七项决策讨论**不是**本文内容；如与本文冲突，以上述所有者裁定为准。

## 0. 依据与边界

协议方向与持久化现状的关键锚点（行号对应当前工作树）：

- **Node 主动出站拨号 Hub**：`HostTransportMode` 只有 `OutboundWss` 与
  `SshTunnel` 两值（`crates/remuda-protocol/src/enums.rs:42-45`）；常驻 Node
  daemon 在握手里上报本机水位（`crates/remuda-node/src/daemon.rs:576-584`），
  Hub 回应后 daemon 按水位追平 journal（`daemon.rs:694`、
  `daemon.rs:783-815`）。Hub 从不主动向 Node 建 TCP。
- **Node 生命周期独立于控制连接（D-019）**：决策原文
  `docs/design/decisions.md:59`；控制器循环在 30 秒 ACK 硬失败时退出重连，
  daemon 与实例进程不退（`crates/remuda-node/src/daemon.rs:659-662`）。
- **SQLite 单表 journal**：每条实例一条独立单调 seq 流，
  `PRIMARY KEY (instance_id, seq)`（`crates/remuda-hub/src/store.rs:4876-4883`）；
  实例行带 `durable_seq`（`store.rs:4844-4860`）。
- **权威表**：hosts（`store.rs:4801-4818`）、objects（`store.rs:4819-4837`）、
  interactions（`store.rs:4925-4937`）、passkeys（`store.rs:4957-4970`）。
- **启动序列**：建目录、解析 bootstrap、`Store::open`、主机全部标离线
  （`crates/remuda-hub/src/lib.rs:608-617`）；push 服务可缺（`lib.rs:618-630`）；
  重启后回收 lost host、过期 requested 与 gate 队列（`lib.rs:652-658`）。
- **推送**：journal 事件经 `alerts::observe` 扇出（`crates/remuda-hub/src/alerts.rs:75-84`），
  badge 取持久 pending 数（`alerts.rs:172-180`），手机口径见 D-049
  （`docs/design/decisions.md:844`）。
- **bootstrap/enroll 拆分（D-018）**：`bootstrap-token` 只是设备配对码
  （`docs/design/decisions.md:58`；落盘 `crates/remuda-hub/src/auth.rs:62-68`），
  Node 持久 host token 走一次性 enroll 通道，hello result 回写
  （`crates/remuda-node/src/daemon.rs:688-690`）。
- **passkey 与 origin 绑定（D-030）**：RP ID 是请求 origin 的完整 host，
  公网 origin 来自 `REMUDA_PUBLIC_ORIGIN`（`crates/remuda-hub/src/passkeys.rs:152-156`、
  `passkeys.rs:170-184`；决策原文 `docs/design/decisions.md:73`）。
- **拒绝语义（D-035）**：`docs/design/decisions.md:232`。
- **Hub 身份密钥（规格，尚未实现）**：`docs/design/protocol.md` §7.7
  （节起 `protocol.md:1613`）；私钥路径与备份归属在 `protocol.md:1654-1670`，
  pin 失败的 fail-loud 形状在 `protocol.md:1712-1718`。
- **边界**：本轮不读远端、不执行 `deploy/` 下任何东西、不改 store 迁移、
  不调外部 API。备份脚本只做本地文件操作。

## 1. 当前拓扑一句话

一台由所有者持有的**笔记本**运行 `remuda hub`：它是 journal、interactions、
passkey、设备/主机表与 provider secret envelope 的**唯一权威**
（`crates/remuda-hub/src/lib.rs:611-617`；表结构见上节）。一台或多台 **Node**
是常驻 daemon，主动出站 WSS 拨号 Hub（`crates/remuda-protocol/src/enums.rs:42-45`），
断线按指数退避重连（初值 1 秒、上限 30 秒、±25% 抖动，
`crates/remuda-node/src/transport/mod.rs:36-52`）。**手机**是装成 PWA 的浏览器，
经 HTTPS/WSS 读 Hub（follow 通道快照 + 广播，`crates/remuda-hub/src/ws.rs:241`）。
在 D-031 现状下，浏览器、手机与 Node 必须有获准的内网路由才能访问 Hub
（`docs/design/deploy-runbook.md:14-16`）；本文不讨论改变这一可达性的任何方案。

## 2. 失效模型

两张表都按同一组四格写：**什么死了 / 什么还能用 / 丢什么 / 用户看到什么**。
表中只陈述当前代码事实；标注「规格」的是本文件为实现批定的行为。

### 2.1 逐组件

| 组件 | 什么死了 | 什么还能用 | 丢什么 | 用户看到什么 |
| --- | --- | --- | --- | --- |
| **Hub 进程**（笔记本上） | journal/interactions 的唯一写入面、审批与推送扇出（`alerts.rs:75-84`）、follow 广播（`ws.rs:241`）全部不可达 | Node daemon 与实例进程不退（D-019，`decisions.md:59`），实例本地继续跑、journal 继续追加（`daemon.rs:834-852`）；手机可读已缓存历史 | Hub 缺席窗内的在线审批、push、follow 实时帧；持久状态不丢（SQLite WAL 已确认事务为准，`store.rs:4876-4883`） | 手机实时视图停更；Node 控制器循环在 30 秒 ACK 超时后退出重连（`daemon.rs:659-662`），Hub 回来后按 watermark 幂等追平（`daemon.rs:694`） |
| **一台 Node**（daemon + 它管的实例） | 该主机的实例执行面、journal 来源、经由该机的 `via:<hostId>` API 出口（`crates/remuda-hub/src/api_relay.rs:415-453`） | Hub 与其余 Node 不受影响；Hub 持久历史不丢；手机可看其它主机 | 该机离线期间的新观测（待其 daemon 回来后按 watermark 补，`daemon.rs:783-815`）；持久 ack 后的帧不重发 | 主机清单里该机离线（启动即标离线 `lib.rs:612-615`、hello 丢失标离线 `ws.rs:376-382`）；经它代理的实例报出口不可达 |
| **手机**（PWA/浏览器） | 只是 Hub 的一个无状态客户端，死了不影响 Hub/Node/其它设备 | Hub 与全部 Node 照常；其它已登录设备照常 | 无权威数据丢失；仅该设备本地缓存与推送授权状态 | 其它设备无感知；重开后由快照重建视图（`ws.rs:1408-1424`） |
| **Hub ↔ Node 网络** | 在飞帧；新的 journal ACK | 双方进程都活着；Node 侧实例继续跑并本地记账（`daemon.rs:834-852`） | 网络恢复前无新的权威落库；已确认 durable 的不丢 | Node 退避重连（`transport/mod.rs:58-87`），恢复后 hello 带水位、Hub 回 resume 水位并转发追平（`daemon.rs:576-584`、`daemon.rs:694-696`） |
| **Hub ↔ 手机网络** | follow 实时帧、新会话操作 | Hub/Node 全不受影响；手机缓存与历史可读 | 无持久数据 | follow 靠快照 + gap resync 恢复（广播容量满即 gap，`ws.rs:190`；重连后重放快照，`ws.rs:1381-1390`）；错过的审批 push 不再补响铃（badge 是持久 pending 真值，`alerts.rs:172-180`） |

### 2.2 笔记本特有工况（六种）

所有者让步是「遥控时笔记本开机并联网」，不保证笔记本就在手边、网络不变。
下列工况全部可能在无人物理触达时发生；本表只写当前事实与可见后果，
缓解机制中超出本批范围的（主动保活、接口切换 watcher 等）只点到为止，不落规格。

| 工况 | 什么死了 | 什么还能用 | 丢什么 | 用户看到什么 |
| --- | --- | --- | --- | --- |
| **合盖休眠** | Hub 进程随系统挂起，监听 socket 不应答；休眠期间定时器冻结 | Node 侧实例继续运行、本地 journal 继续追加（`daemon.rs:834-852`）；Node 进程不退（D-019）；手机可看缓存 | 休眠窗内无 push、无审批、无 follow 实时帧 | Node 现有 WSS 代码只**回** Ping 不主动发 Ping（`crates/remuda-node/src/transport/wss.rs:529-532`），半开连接可能挂到系统 TCP keepalive 才断；控制器循环先撞 30 秒 ACK 硬失败并退出重连（`daemon.rs:659-662`），唤醒后按水位追平；手机视图停更后走 gap resync（`ws.rs:1381-1390`） |
| **IP 变更**（换租约/改网段） | 已建立连接的对端地址失效，在飞帧丢失 | Hub 进程不重启，SQLite 权威不丢；Node 实例继续跑 | 换址完成前的在线帧与审批 | Node 拨号报错后走退避重连（`transport/mod.rs:36-52`），hello 重新带水位（`daemon.rs:576-584`）；手机 follow 断开后快照 + resync（`ws.rs:1381-1390`）；追平期间已存在的 journal 行按重放行不重复扇出（`crates/remuda-hub/src/ws.rs:633-642`） |
| **WiFi 切到手机热点** | 若热点不在获准内网，Hub 名字从此网络不可达（D-031 现状无公网入口，`deploy-runbook.md:14-16`）；切换瞬间旧连接同样断 | 笔记本本机一切照跑；留在原内网的设备不受影响 | 离开获准内网期间的远程可达性，无数据丢失 | Node/手机拨不通 Hub 并持续退避；笔记本回到获准内网后自动恢复，不需要重新登录或重新 enroll |
| **强制门户** | 门户放行前出站 TLS 被拦截，WSS/TLS 握手失败 | Hub 本机、同网已认证设备、Node 已在跑的实例 | 门未过期间的远程可达性 | Node 握手失败后退避重试（`transport/mod.rs:58-87`）；手机页面打不开；现有代码没有也不可能有无人值守过门机制，需要人在笔记本旁完成门户认证——这与公网暴露姿态的选择无关，任何姿态下都成立 |
| **VPN 开关** | 路由表切换瞬间经过旧接口的连接全部断开；VPN 接管默认路由时 Hub 监听地址的可达域随之改变 | 切完且新路由仍可达时，Node 与手机按既有退避自动重连（`transport/mod.rs:36-52`） | 在飞帧；切换窗内的在线操作 | 一波集中重连；重连后 journal 幂等追平（主键去重 `store.rs:4882`，重放不重复扇出 `ws.rs:633-642`），用户只需等连接恢复 |
| **Hub 进程重启**（崩溃 / 手动升级） | 进程内存态：follow 跟随者表、blocked 计时器（`alerts.rs:51-72`）、**仅存内存**的 egress 上下文表（`crates/remuda-hub/src/api_relay.rs:99-100`） | SQLite 里的全部持久状态；Node daemon 与实例不退（D-019） | 未落库的内存态；干净停止会先 checkpoint WAL（`store.rs:1496`），异常退出则由 SQLite 在下次打开时恢复 WAL，已确认事务不丢 | 启动先把所有主机标离线（`lib.rs:612-615`）、回收 stale requested 与 gate 队列（`lib.rs:652-658`）；Node 重连后 hello 触发 egress 重装（`ws.rs:332-336`、`api_relay.rs:407-415`）并追平 journal（`daemon.rs:694`）；手机 follow 重新拿快照 |

> **单实例双进程是本表唯一的「会写坏」情形**：WAL 允许多连接，但 Hub 自己
> 约定单 writer 线程纪律（`crates/remuda-hub/src/store.rs:4730-4731`；writer 线程
> `store.rs:1477-1504`，建库与 `journal_mode=WAL` 在 `store.rs:4789`）。两个 Hub
> 进程开同一个 `data_dir` 不会被 SQLite 文件锁拦住应用层不变量，必须用 §3 的
> 单例锁在启动时拒绝。

## 3. `hub.lock` 单例锁（规格，尚未实现）

**目标**：同一时刻、同一个 `data_dir` 至多一个 Hub 进程。防止两个 Hub 同开一份
库时各自跑 writer 线程、各自做启动回收（`lib.rs:652-658`）并对重连 Node
重复下发 egress（`api_relay.rs:407-415`），把应用层状态写坏。

**规格**（实现批落；本批只在 writer 侧连接打开函数 `try_open_conn` 的
doc comment 留锚，`crates/remuda-hub/src/store.rs:4768-4780`）：

1. **路径**：`<data_dir>/hub.lock`，与 `hub.sqlite` 同级（`data_dir` 现有
   打开点 `crates/remuda-hub/src/store.rs:1468-1470`）。锁文件本身不进备份集（§5.2）。
2. **加锁时机**：`Store::open` 在 `create_dir_all(data_dir)` 成功之后、
   打开任何 SQLite 连接之前，以非阻塞方式取排他 advisory flock
   （`LOCK_EX | LOCK_NB`）。取锁用的 fd 由 `Store` 持有，与 `Store` 同生命周期。
3. **拿不到锁 = 拒绝启动**（fail-closed，D-035 风格的显式拒绝，不等待、不杀对方、
   不尝试删锁文件）：错误信息点名 `<data_dir>/hub.lock` 并提示「已有 Hub 进程
   使用该 data_dir」，进程以非零码退出。
4. **释放**：fd 在 `Store` drop 时关闭，内核随即释放 flock
   （现有 `StoreJoin` drop 会 join writer 线程，`crates/remuda-hub/src/store.rs:138-145`；
   推荐实现批在 writer 的 Stop 路径关闭锁 fd，与 WAL checkpoint 同批收尾，
   见 `store.rs:1496`）。进程被强杀时内核同样释放锁——**锁文件留在磁盘上是
   正常的**，flock 不会被一个已死进程继续持有，运维绝不能靠「删锁文件」排障。
5. **作用域与非目标**：这是**单机/单目录**锁，只挡同一台机器上的第二个进程，
   不挡「两台机器各持一份数据副本同时运行」——后者靠 §7 迁移顺序与
   bootstrap 轮转在程序上杜绝，不靠锁。锁不做死探活、不做租约、不写持有者身份
   到 SQLite。
6. **与备份/迁移的关系**：备份与换机都要求旧 Hub 先收到 SIGTERM 干净退出
   （§5.1、§7），既为了 checkpoint，也为了把锁让出来。

## 4. Hub 持久状态清单（`data_dir` 里有什么）

备份 bundle 的成员清单由此表直接导出。所有路径相对于 `data_dir`。

| 成员 | 内容与锚点 | 是否秘密 | 丢失后果 |
| --- | --- | --- | --- |
| `hub.sqlite`、`hub.sqlite-wal`、`hub.sqlite-shm` | 主库 + WAL/SHM：hosts/instances/journal/interactions/passkeys 等（`crates/remuda-hub/src/store.rs:1470`、`store.rs:4876-4883`、`store.rs:4957-4970`；WAL 模式 `store.rs:4789`） | 含 token hash、mandate/transcript 等，按敏感对待 | 全部权威历史丢失 |
| `secrets/`（`master.key`、`secrets.json`） | ChaCha20-Poly1305 envelope 的 master key 与密文库（Hub 打开点 `crates/remuda-hub/src/lib.rs:616`；文件语义 `crates/remuda-driver/src/secrets.rs:3-6`、`secrets.rs:54-64`，目录 `0700`、文件 `0600`） | **是** | master key 丢失后全部 provider secret envelope 不可解密；只拷 SQLite 不拷此目录的备份不可用 |
| `vapid.json` | Web Push VAPID P-256 私钥，`0600`（`crates/remuda-push/src/vapid.rs:18-21`） | **是** | 推送身份变更，所有手机推送订阅失效、需重新授权订阅 |
| `push.sqlite`（+`-wal`/`-shm`） | push 订阅表（`crates/remuda-push/src/store.rs:31-34`，同样 WAL） | 含推送 endpoint | 订阅记录丢失（重新订阅可恢复，不影响登录）；push 服务在 Hub 里本来就是可缺的（`lib.rs:623-628`） |
| `hub-identity/identity.ed25519`、`hub-identity/identity.ed25519.pub` | Hub ed25519 身份私钥/公钥（**规格 §7.7，尚未实现**；`docs/design/protocol.md:1654-1658`） | 私钥**是** | **恢复后若缺失，Hub 会生成新身份，所有 Node 的 pin 立即全部失效，只能逐台重新 enroll**（`protocol.md:1665-1670`） |
| `bootstrap-token`、`bootstrap-issued-at` | 设备配对 access code 与其签发时间戳（`crates/remuda-hub/src/auth.rs:62-68`、解析逻辑 `auth.rs:88-111`） | 短期秘密 | 换机不轮转即存在新旧机同时可配对的窗口（§7）；时间戳丢失会按 D-018 兼容逻辑补写 |

明确**不**进备份集：`hub.lock`（运行期锁，§3）、`listen`（本机回环发现信息，
非秘密但无迁移价值，`crates/remuda-hub/src/auth.rs:154-164`）、任何临时目录。

## 5. 加密备份（`scripts/hub-backup.sh`）

权威操作定义在脚本与其 `--help` 中；本节是设计口径。脚本**只做本地操作**：
不联网络、不调用任何远端传输工具、绝不写 `deploy/` 目录。

### 5.1 一致性前提：先停后拷

推荐且迁移时**必须**的顺序是先让 Hub 干净退出：CLI 同时安装 SIGTERM/SIGINT
处理（`crates/remuda/src/main.rs:56-83`），退出触发优雅关闭
（`crates/remuda/src/cmd/hub.rs:117-127`），writer 线程在 Stop 时执行
`PRAGMA wal_checkpoint(TRUNCATE)`（`crates/remuda-hub/src/store.rs:1496`）。
脚本对在线库只做**尽力** checkpoint（本机有 `sqlite3` 时尝试
`PRAGMA wal_checkpoint(TRUNCATE)`，忙则告警继续）；连 WAL/SHM 一起拷时，
恢复等价于一次 SQLite 崩溃恢复，WAL 帧校验和会截掉撕裂尾帧，已确认事务不丢。

### 5.2 bundle 结构与清单

加密流：`tar（manifest + 第 4 节全部存在成员） | age`。

- **成员**：第 4 节逐项；目录递归展开为文件级清单。`hub.sqlite`、`secrets/`、
  `bootstrap-token` 是核心成员，缺失即预检失败；WAL/SHM、`push.sqlite*`、
  `vapid.json`、`hub-identity/` 与签发时间戳「存在则必入、不存在则在清单中
  显式标 absent」——§7.7 身份密钥落地后，其缺失应被视为恢复阻断项。
- **内部 manifest（随包加密）**：`format/version`、UTC 时间、每个文件的相对路径、
  字节数、权限位与 SHA-256；数据目录只记录 basename，不记录绝对路径。
- **签名状态如实标注**：manifest 的 `signature` 字段当前为 `null`，
  `signatureStatus` 写明「待 Hub 身份签名落地（protocol.md §7.7）」。
  §7.7 尚未实现，脚本不伪造签名。
- **外部摘要（不落进 bundle）**：加密产物另写一个只含哈希与成员名的
  `<bundle>.sha256` 旁车文件，用于核对传输完整性；它不含任何秘密值。

### 5.3 三道安全闸（测试断言）

1. **age 加密强制**：未提供 `--recipient` 且环境无 `AGE_RECIPIENT` 时，
   任何模式（含 dry-run）都拒绝执行并退出非零。不存在明文 bundle 路径。
2. **默认 dry-run**：不带 `--execute` 只打印计划（成员、present/absent、
   各文件哈希、产物路径、recipient 指纹），不写任何产物。
3. **执行双确认**：`--execute` 还必须带 `REMUDA_BACKUP_YES_I_KNOW=1`，
   否则拒绝；本机没有 `age` 时同样拒绝并给出明确错误。产物权限 `0600`，
   输出路径位于 `deploy/` 之下或位于 `data_dir` 之内时拒绝（防止把备份
   写进自己要拷的集合）。

## 6. 恢复与恢复后的副作用抑制

### 6.1 恢复步骤

1. 在目标机器上空的、`0700` 的 `data_dir` 上 `age -d` 解密；
2. 先校验外部摘要，再解出内部 manifest，逐文件核对 SHA-256 与权限位；
3. 解包到该空目录（确认不含 `hub.lock` 等运行期文件）；
4. **首启前**轮转 bootstrap（`remuda hub rotate-bootstrap` 已存在：
   `crates/remuda/src/cmd/hub.rs:52-59` 调 `remuda_hub::rotate_bootstrap`，
   `crates/remuda-hub/src/auth.rs:117-121`；已配对设备的 device token 不受影响，
   D-018 原文 `decisions.md:58`）；
5. 再启动 Hub。身份私钥随包恢复时 Node pin 全部保留；缺失则按 §7.7 兜底：
   逐台重新 enroll，没有自动铸新身份的口子。

### 6.2 为什么旧备份会「重放」副作用

恢复一份旧备份后，设备份点每实例水位为 R0。Node 在 Hub 缺席期间继续记账，
重连时 hello 上报自己的水位（`crates/remuda-node/src/daemon.rs:576-584`），
随后把 (R0, wm] 段的 journal 转发上来（`daemon.rs:834-852`）。

现状代码有一层去重：journal 行已存在时标记 `replayed = true`
（`crates/remuda-hub/src/store.rs:8137-8148`），ws 层对重放行只落库、
**不**发 bus、不触发告警/usage/supply（`crates/remuda-hub/src/ws.rs:633-642`）。
但备份点之后、Node 在缺口窗内追加的行在恢复库里**不存在**，会走全新插入
（`store.rs:8169-8192`，`replayed = false` 在 `store.rs:8190`），于是今天会
被当成「刚刚发生」重新扇出副作用。这就是本节要堵的缺口。

### 6.3 判据：每实例 `restore_watermark`（规格，尚未实现）

- **定义**：恢复后第一次 hello 中，该 Node 上报的每个实例 `durableSeq`
  （字段形状 `daemon.rs:576-584`，解析 `daemon.rs:783-815`）记为该实例的
  `restore_watermark`；恢复过程写入一个仅内存/标记文件的「恢复未结清」状态，
  每实例追平到自己的水位（或上报水位本就不大于当前 durable）后标记结清，
  全部结清才删除标记。本规格不新增 store 表（本批不动 schema）。
- **规则**：追平窗内 `seq <= restore_watermark` 的事件只做**状态投影**，
  不触发任何外向副作用；seq 越过水位后的新事件恢复正常。
- **外向副作用穷举（只有这三类，抑制判据统一在 Hub 侧外向派发层）**：
  1. **interaction 唤醒**：`alerts::observe` → `AlertKind::Interaction`
     （`crates/remuda-hub/src/alerts.rs:75-84`、`alerts.rs:119-124`、
     分类 `alerts.rs:217-229`）——历史审批不得重新叮人；
  2. **push 推送（含 blocked 等待提醒与 badge 推送）**：
     `fanout`（`alerts.rs:148-185`；等待提醒 `alerts.rs:95-113`；
     badge `alerts.rs:172-180`；「正在 follow 则不推」是另一条独立抑制，
     `alerts.rs:159-162`，不被本条替代）；
  3. **`api.egress` 重装**：hello 后按持久路由把网关凭据重新发给代理 Node
     （触发点 `crates/remuda-hub/src/ws.rs:332-336`；
     `reinstall_egress_on_connect` 读的是库里持久化路由
     `crates/remuda-hub/src/api_relay.rs:407-453`；装帧本身
     `api_relay.rs:294-355`）——旧备份可能复活备份之后已撤销/改道的出口，
     必须等该实例结清后才允许重装；结清前需要出口的实例按既有
     `via-host-offline` 拒绝形状处理（`api_relay.rs:349-353`），不静默改道。
- **明确不抑制的**：所有**持久投影照常**——journal 落库
  （`store.rs:8169-8173`）、interactions 表投影（`store.rs:8177`）、
  usage/supply 观察（`ws.rs:637-638`）、给在线跟随者的 bus 发布
  （`ws.rs:635`，历史追平本就该可见，且帧带原始 `observed_at`）。
  badge 数字本身来自持久 pending 列表（`alerts.rs:172-177`），是恢复后的
  当前真值，不属于重放。
- **信封要求**：恢复/重连场景的 resume 水位信封必须纳入 §7.7 的 Hub 身份
  签名形状（`docs/design/protocol.md:1737-1749`），且 Node 对 `durableSeq`
  下调拒绝，除非信封显式声明权威重置；§7.7 当前必签帧最小集
  （`protocol.md:1722-1735`）尚未包含该信封，实现身份批次时必须一并补上，
  不得用未签名帧指挥 Node 重放或抑制。

## 7. 换机迁移（不重新 enroll、不重新登录）

操作步骤在 [`deploy-runbook.md`](deploy-runbook.md) 的「Hub 换机迁移」一节；
此处只定不变量与顺序，二者必须一致。

**保持不变（所以不用重登/重 enroll）**：

- **RP ID 与 origin 不变**：`REMUDA_PUBLIC_ORIGIN` 与访问名字原样带到新机；
  RP ID 是 origin 的完整 host（`crates/remuda-hub/src/passkeys.rs:152-156`、
  `passkeys.rs:170-184`，D-030 `decisions.md:73`）。
- **passkey 不变**：passkey 行在主库内随备份一起恢复
  （`crates/remuda-hub/src/store.rs:4957-4970`），RP ID 不变即浏览器侧凭据继续有效。
- **host token 不变**：hosts 表（含 token hash）随库恢复
  （`store.rs:4801-4818`）；Node 的持久 node token 是 hello result 回写的
  （`crates/remuda-node/src/daemon.rs:688-690`），不依赖机器本身。
- **Hub 身份不变**：`hub-identity/` 随包恢复（§4），Node pin 全部保留。
- **版本兼容**：新机二进制版本与 schema 必须兼容；schema 迁移只前滚
  （CLI 的迁移入口 `crates/remuda/src/cmd/hub.rs:111-113`）。

**必须变化**：

- **bootstrap-token 必须轮转**：旧机数据离开旧机前/新机首启前用
  `remuda hub rotate-bootstrap` 换新（`crates/remuda-hub/src/auth.rs:117-121`），
  旧配对码立即失效，封死「新旧机同时可配对新设备」的窗口（D-018
  `decisions.md:58`）。

**顺序硬约束**：

1. **旧机先 SIGTERM**：等待优雅退出完成（信号处理
   `crates/remuda/src/main.rs:56-83`、优雅关闭 `crates/remuda/src/cmd/hub.rs:117-127`、
   WAL checkpoint `crates/remuda-hub/src/store.rs:1496`），并确认进程已不在——
   §3 的 hub.lock 实现后这一步同时让出单例锁；
2. 再做加密备份（§5）、带外传至新机、校验解密（§6.1 步骤 1–3）；
3. 轮转 bootstrap（§6.1 步骤 4）；
4. 新机首启，观察 Node 追平与副作用抑制结清（§6.3）；
5. 确认新机健康后再擦除旧机 `data_dir`。**任何时候不得让两台机器各持一份
   副本同时对外当 Hub**；单机锁（§3）管不到跨机双活，顺序与 bootstrap 轮转
   就是跨机那道保险。

## 8. 明确不做

- 不做高可用/双活/共识：单人系统任一时刻只有一个活 Hub；不做「两台机器
  同时打开同一数据副本」的任何支持。
- 不写公网前门、公网可达性或设备如何在公网会合到笔记本的任何机制——所有者
  已裁定本批不做、姿态暂不决（见文首裁定；D-031 原文 `decisions.md:74`）。
- 本批不改 store schema、不改 wire 版本、不实现 hub.lock、restore 抑制与
  Hub 身份签名；本文与两处 doc comment 是给实现批的规格与验收依据。
- 不做「换 Hub 自动铸新 host token」：D-018 的一次性 enroll 是唯一合法补票路径。
- 备份脚本不联任何网络、不调远端传输工具、不写 `deploy/`；远程搬运由操作员
  另行选择带外手段，不属于脚本职责。
