# Remuda deploy runbook：SG 内网 Hub + 现有 Caddy

当前交付状态：**prepared only — awaiting-caddy-restart-approval**。按用户最新
指示，准备变更与回滚脚本，等待明确的 Caddy 重启批准；不得因脚本已准备好就
执行重启或继续 Node 接入验收。此前操作和基线恢复以
[实施证据](./evidence/intranet-hub-1.md) 为准，不把临时探测当作最终启用结果。

公网暴露：待定；禁止隧道工具（D-031）。唯一支持入口是
[deploy/intranet](../../deploy/intranet/README.md) 的内网 Caddy。按 D-020
后续优先级调整，先把 Hub 放到 `<sg-host>`，复用现有 Caddy 的 DNS-01，使用
独立域名 `remuda.<zone>`。历史公网 VPS 方案保留在
[deploy/public](../../deploy/public/README.md)，等待新的公网部署决策。

SG 没有公网 IP；域名 A 记录指向内网地址。DNS-01 可签发 HTTPS 证书，但不会
让该地址变成公网可达。浏览器、手机和 Node 必须有获准的内网路由。普通蜂窝
网络访问不属于此次内网验收。旧公网暴露路径已移除，遵循 D-031。

## 前置证据与构建

操作员先核验 `<sg-host>` 的 OS/架构、Docker/Compose、现有 Caddy 容器、
`deploy_default` 网络、Caddyfile 挂载及 import 方式、DNS 模块和既有凭证变量名。
查看配置时不输出凭证。记录相关状态和已有网关健康响应作为回滚/验收基线；
执行结果集中写入 [intranet-hub-1.md](./evidence/intranet-hub-1.md)，不能由旧 M1
预检记录推定当前可用。

本轮现场检查识别的宿主机是 Debian 10 x86_64；以检查结果为准，不套用公网
包的 Ubuntu 24.04 安装器。现有 Caddy 已把 `/config` 和 `/data` 挂载为命名卷，
主 Caddyfile 是独立 bind mount；拟复用 `/config` 保存持久 include。远端 PATH
检查未发现原生 agent CLI 或 Herdr；未来 Node 注册可以报告缺失工具，但当前
不执行 Node 接入验收，也不据此宣称具备会话执行能力。

从已验证的同一提交先构建 web，再构建 Linux musl 二进制与
`deploy/m1/Dockerfile` 镜像。详细构建顺序见
[镜像构建](../../deploy/public/README.md#build-the-image)。确认镜像架构匹配
SG x86_64，并用明确 tag 或摘要标识实际部署构建。

## 批准前：准备材料

1. 核验 `remuda.<zone>` 的 A 记录为目标内网地址；复用 Caddy 已有的 Cloudflare
   DNS-01 凭证。DNS token 仅供 Caddy 使用，Hub 不获取它。
2. 准备 [intranet Compose](../../deploy/intranet/compose.hub.yml) 的环境值：
   `HUB_IMAGE`、`HUB_DOMAIN=remuda.<zone>`、`DATA_DIR`、实际数据目录 owner 的
   `HUB_UID/HUB_GID`，以及只包含 Caddy 实际网络 IP 的 `REMUDA_TRUSTED_PROXIES` JSON。
   数据目录 `/data00/remuda/hub` 模式为 0700，由实际 Compose 运行用户拥有；
   已有数据和 bootstrap 文件必须保留。Hub 不发布端口。
3. 核验独立 `remuda-intranet` Compose 项目的准备状态和既有外部网络
   `deploy_default`。用 `docker compose config` 检查镜像、数据目录和只读设置；
   已启动服务及内部健康检查的实际情况以证据为准，不改动 Caddy/网关项目。
   拟使用 Caddy 上游专用别名 `remuda-intranet-hub`，避免共享网络中的服务名冲突。
4. 将 [Caddy 片段](../../deploy/intranet/Caddyfile.snippet) 实例化为单独的 Remuda
   include，使用已验证的 DNS-01 配置。保留操作员源文件
   `~/astergate/deploy/Caddyfile.d/remuda.caddy`。批准前只准备未激活的片段与
   脚本，不在活动 Caddyfile 添加 import。准备阶段可复制到现有容器内
   `/config/remuda/Caddyfile.d/remuda.caddy`，位于已挂载的 `/config` 命名卷；
   保留此卷即可跨容器重建保留 include。
5. 准备并审阅 [caddy-change.py](../../deploy/intranet/caddy-change.py) 的
   `prepare`、`apply` 与 `rollback` 路径，
   包括原配置备份、仅限 Remuda 的 diff、验证、重启及健康检查；等待批准。
   首次登录 access code 保存于 `/data00/remuda/secrets/access-code`，模式 0600，
   不进入 argv、URL、日志或证据文档。

主机上的脚本名为 `~/astergate/deploy/remuda-caddy-change.py`。私有设置文件
`.remuda-caddy-change.json` 使用 0600，包含 `gateway_health_url`、`hub_health_url`、
`baseline_caddy_sha256`、`baseline_started_at`、`include_sha256` 和
`gateway_health_sha256`，均由本轮已审阅基线产生；不包含 DNS token。

```bash
cd "$HOME/astergate/deploy"
python3 remuda-caddy-change.py prepare --settings .remuda-caddy-change.json
```

`prepare` 检查配置哈希、容器启动时间、片段哈希与网关健康响应；生成
`Caddyfile.remuda-prepared`，把候选配置和未激活片段复制到已有 `/config` 卷，
执行 Caddy validate，再复查网关健康与活动文件未改变。它不改活动 Caddyfile、
不发 reload 信号、不重启。准备成功也不解除审批等待状态。

## 获得明确批准后：一次性 Caddy 变更

以下是待批准脚本的行为约定，当前不得执行。`apply` 应备份原 Caddyfile，
核验准备好的 include 哈希，写入专用 Remuda include，在主文件只添加缺少的
`import /config/remuda/Caddyfile.d/*.caddy`，并把全局 `admin off` 改为
`admin localhost:2019`。管理端点仅在 Caddy 容器内回环地址监听，不发布宿主机
2019 端口。保留既有网关站点及其 `/v1` 路由。

完成配置验证后，脚本才运行用户待批准的 `docker restart deploy-caddy-1`，
随后检查原网关健康响应哈希以及 Remuda HTTPS `/healthz` 的 `{"ok": true}`，
HTTPS 请求保持证书验证。完整登录页面另列为后续验收。重启可能暂时
中断该 Caddy 承载的连接，正是本次需要等待明确批准的动作。原 API reload 失败
与 SIGUSR1 探测经过只保存在实施证据中，不作为本次绕过批准的替代执行入口。

只在批准后，从同一主机目录执行：

```bash
python3 remuda-caddy-change.py apply --settings .remuda-caddy-change.json
```

`apply` 会重复基线检查，保存 `Caddyfile.pre-remuda-approved` 和恢复状态，再
修改活动文件。若变更后的重启或健康检查失败，脚本自动恢复原文件并再次重启
Caddy 以恢复网关。批准 `apply` 包含这次失败恢复重启，不在故障时额外等待批准。

## 配套回滚（同样包含重启）

`rollback` 恢复备份中的原全局 admin 设置（本轮基线为 `admin off`）和原 import
状态，通过恢复 import 停用新增 Remuda 片段；保留未激活的准备文件供审阅。
验证恢复后的配置，再
`docker restart deploy-caddy-1`，复核原网关健康。回滚脚本当前也只准备，不能在
未获批准时执行其中的重启。保留 Hub 数据、Caddy `/config` 与证书卷、
`deploy_default` 网络，不重建网关项目。

以下命令留作批准后的显式恢复使用，当前不执行：

```bash
python3 remuda-caddy-change.py rollback --settings .remuda-caddy-change.json
```

## 批准并启用后的验收（当前未执行）

从具备内网路由的客户端验证 `https://remuda.<zone>/login`，私下读取 access
code 首次登录；已登录设备在 Settings 生成手机配对码，手机使用 `/login?pair`。
按 [Node onboarding](../../deploy/public/NODES.md) 接入 Node，再核验
`/v1/follow`、`/node/v1/connect`、主机清单与断线重连。当前 carrier 参数为
`--hub-url` / `--host-token-file`；c-daemon 与 D-018 合入后使用安装器及一次性
enroll token。内网 provider 由各 Node 直接访问，凭证和 profile 按主机配置。
这些步骤是后续验收清单，不是当前完成声明。

## 后续维护

升级前对 SQLite 做一致性备份，并保留匹配的 bootstrap、master key 与其他
secret envelope；数据库迁移失败时保持 Hub 停止。旧镜像必须兼容当前 schema
才可直接回滚；否则在新目录恢复备份和对应配置，保留故障目录并验证后切换。
公网包的安装/升级脚本面向独立 VPS 栈，不直接作用于已有 SG Caddy 项目。

公开 VPS CI smoke 只验证包的内部证书容器路径。此次内网部署、DNS-01 和
真实客户端结果以 [实施证据](./evidence/intranet-hub-1.md) 为准。

## Hub 换机迁移（不重新 enroll、不重新登录）

把 Hub 从一台机器搬到另一台，权威设计见
[hub-topology.md](./hub-topology.md) §5–§7；本节是操作顺序。全程只动
`<data_dir>` 与两台机器上的 Hub 进程，不碰 Node 的 enroll、不碰手机登录。

### 不变量（违反任何一条都会退化成重新 enroll/重新登录）

- **RP ID 与 origin 不变**：新机继续使用同一个 `REMUDA_PUBLIC_ORIGIN` 与
  同一个访问名字；RP ID 是请求 origin 的完整 host，换名字即全部 passkey
  失效（`crates/remuda-hub/src/passkeys.rs:152-156`、
  `passkeys.rs:170-184`；D-030 `decisions.md:73`）。
- **passkey 不变**：passkey 行在主库内，随备份迁移
  （`crates/remuda-hub/src/store.rs:4957-4970`），origin 不变则浏览器侧
  凭据原样有效。
- **host token 不变**：hosts 表随库迁移（`crates/remuda-hub/src/store.rs:4801-4818`）；
  Node 持久 node token 来自 hello result 回写（`crates/remuda-node/src/daemon.rs:688-690`），
  与机器无关，Node 不需要重新 enroll。
- **Hub 身份不变**：`hub-identity/identity.ed25519` 必须随备份恢复；该文件
  丢失则全体 Node pin 立即失效，只能逐台重新 enroll
  （`docs/design/protocol.md:1665-1670`）。
- **二进制版本兼容**：新机使用版本不低于旧机、且兼容当前 schema 的二进制；
  schema 只前滚（迁移入口 `crates/remuda/src/cmd/hub.rs:111-113`）。

### 必须变化

- **bootstrap-token 必须轮转**：新机首启前执行
  `remuda hub rotate-bootstrap`（`crates/remuda/src/cmd/hub.rs:52-59`
  调 `crates/remuda-hub/src/auth.rs:117-121`）。已配对设备的 device token
  不受影响（D-018 `decisions.md:58`），旧配对码立即失效，杜绝新旧机
  同时能配对新设备的窗口。

### 顺序

1. **新机准备**：安装版本兼容的二进制；准备空的、权限 `0700` 的
   `<data_dir>`；确认新机上的 `REMUDA_PUBLIC_ORIGIN` 等配置与旧机逐字一致。
2. **旧机先 SIGTERM 并确认进程退出**：CLI 同时处理 SIGTERM/SIGINT
   （`crates/remuda/src/main.rs:56-83`），退出走优雅关闭
   （`crates/remuda/src/cmd/hub.rs:117-127`），writer 线程在 Stop 时
   `PRAGMA wal_checkpoint(TRUNCATE)`（`crates/remuda-hub/src/store.rs:1496`）。
   规格中的 `<data_dir>/hub.lock` 单例锁（hub-topology.md §3）落地后，
   这一步同时让出锁；锁文件留在磁盘上是正常的，不要手工删除。
3. **旧机做加密备份**：`scripts/hub-backup.sh --data-dir <旧 data_dir>
   --recipient <age-recipient> --execute`（另需 `REMUDA_BACKUP_YES_I_KNOW=1`；
   先不带 `--execute` 跑一次 dry-run 核对成员）。bundle 含 SQLite 与
   WAL/SHM、`secrets/`、VAPID 私钥、`hub-identity/`、`bootstrap-token`
   （清单理由 hub-topology.md §4）。脚本只做本地操作，不联网络、不写
   `deploy/`。
4. **带外搬运并校验**：由操作员自选带外手段把 `.age` 与旁车 `.sha256`
   送到新机（脚本本身不提供任何远端传输）；新机 `age -d` 解密，核对旁车
   摘要与 bundle 内 manifest 的逐文件 SHA-256，解包到空 `<data_dir>`。
5. **首启前轮转 bootstrap**：见上「必须变化」。
6. **新机首启**：Node 重连后按 watermark 追平 journal
   （`crates/remuda-node/src/daemon.rs:576-584`、`daemon.rs:694`）；追平窗内
   的历史事件不得重放 push/交互唤醒/`api.egress` 重装，判据与结清条件见
   hub-topology.md §6.3。手机用同一 origin 打开即恢复，不重新登录。
7. **旧机处置**：确认新机健康、Node 全部追平后，再擦除旧机 `<data_dir>`。
   任何时候不得让两台机器各持一份数据副本同时充当 Hub；hub.lock 只防
   同机双进程（hub-topology.md §3），跨机双活靠本顺序与 bootstrap 轮转杜绝。

### 回滚

新机首启失败且尚未擦除旧机时：保持新机停止、在旧机原目录启动同一二进制
即恢复（不要轮转第二次 bootstrap；旧配对码仍有效）。一旦在新机执行过
bootstrap 轮转并首启成功，旧机配对码已失效，回滚方向只能是修新机，
不能简单启旧机继续对外。

