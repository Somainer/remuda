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

## 升级到当前 main 并接入 Node（D1/D2，2026-10-05）

**内网 Hub 升级（9dd7ec7 → 当前 main）与笔记本、devbox Node 接入 · 所有者/运维清单**

适用：D1/D2（2026-10-05）。执行人：所有者或运维。worker 不执行本清单的任何一步，也不执行 `deploy/` 下的任何脚本。不使用任何隧道工具（D-031）。记录与证据只写占位名（`remuda.<zone>`、`<sg-host>`、`<devbox>`、`<laptop>`、`<DATA_DIR>`）；不写真实主机名、地址、用户名、家目录路径、token、访问码或配对码。

### 0. 前提与边界
- 起点是内网 Hub 自己的数据目录（D2）。笔记本 demo Hub 的数据不迁移、不合并，成为历史。这次不是换机，所以 hub-topology §7 的「首次启动前必须轮转 bootstrap」不适用；是否轮转见第 6 步。
- 只升级 Hub 镜像。Caddy、DNS、证书、`deploy_default` 网络、网关站点一律不动，做法与上次升级到 9dd7ec7 相同。如果目标提交改动了 `deploy/intranet/`，先审阅 `git diff 9dd7ec7 <TARGET> -- deploy/intranet/`，只采纳审阅过的改动。凡涉及 Caddy 的改动，另走 deploy-runbook 的批准流程。
- hub.lock 尚未实现，所以任何时刻同一个数据目录只允许一个 Hub 进程。服务运行期间，不要对同一数据目录再起第二个 Hub 进程，包括 `compose run`。
- Node 升级或重启会结束该 Node 上正在运行的会话（`node-epoch-changed`）。只在 Node 空闲时操作。先升级 Hub，确认健康后再升级 Node。
- 本清单要执行两次：
  - 现在一次：升级到当前 main 并接入 Node；
  - 第一阶段代码全部合入后再执行一次：升级到包含第一阶段的提交，然后入座 Main、取证、切换（见第 12 步）。

### 1. 构建（在可信构建机上，用同一个提交）
1. 固定目标提交 `<TARGET>`。确认 `git cat-file -t <TARGET>` 的结果是 commit，并且它在 `git ls-remote origin refs/heads/main` 所指提交的历史上。
2. 在 `web/` 下执行 `pnpm install --frozen-lockfile && pnpm build`。然后执行 `REMUDA_GIT_SHA=<TARGET> cargo zigbuild --locked --release -p remuda --target x86_64-unknown-linux-musl`。Hub 与 devbox Node 用同一个 Linux musl 二进制；笔记本用同一提交的 macOS 原生构建。
3. 用 `deploy/m1/Dockerfile` 打出 `remuda-hub:<TARGET 前 7 位>` 镜像归档。把 musl 二进制和镜像归档的 SHA-256、镜像 ID 写进私有记录；证据里只写哈希。
4. 在本地对该二进制执行 `remuda version --json` 并核对：
   - `git_sha` 等于 `<TARGET>`（即构建时经 `REMUDA_GIT_SHA` 注入的值）；
   - `wire_major` 为 1；
   - 记下 `version` 字段（包版本），第 5 步要和 `/healthz` 对比。

   `schema_major` 写死为 0，只是 CLI 兼容标记，不是数据库迁移依据；它不能证明 schema 兼容。schema 兼容性只以第 4 步的 `hub --migrate` 演练结果为准。

### 2. 只读预检（SG 主机 `<sg-host>`，服务照常运行）
- `docker compose --env-file .env -p remuda-intranet -f compose.hub.yml config`（只读）。
- `docker compose --env-file .env -p remuda-intranet -f compose.hub.yml exec remuda-hub /usr/local/bin/remuda version --json`：记录当前的 `git_sha`（预期为 9dd7ec7）和 `version`。
- 从内网客户端、开启证书校验执行 `curl -fsS https://remuda.<zone>/healthz`，应返回 `{"ok":true,...}`；记录其中的 `version`。
- 记录四项基线：Caddy 容器 ID、Caddy 启动时间、活动 Caddyfile 的 SHA-256、网关健康响应的 SHA-256。升级后逐项对比，必须不变。
- 数据目录 `<DATA_DIR>`：属主为 `HUB_UID:HUB_GID`，模式 0700；可用空间不少于现有库体积的 3 倍（备份加演练副本）。
- 出站探测（只读，不改配置）：
  - Hub 所在主机可以访问 Web Push 服务（所有者已确认，D-057 OA4）；第 7 步的测试推送就是验证。
  - devbox 与笔记本能否执行 `curl -fsS https://remuda.<zone>/healthz`？不能的话报告所有者，不要绕过。

### 3. 备份（先停服务，再拷贝）
1. 执行 `docker compose ... stop remuda-hub`（30 秒优雅期，writer 会做 WAL checkpoint），再用 `docker compose ... ps` 确认已退出。
2. 使用审阅过的 `scripts/hub-backup.sh` 副本。它只做本地操作，需要用能读取数据目录的身份执行。
   - 先 dry-run：`scripts/hub-backup.sh --data-dir <DATA_DIR> --output-dir <备份目录> --recipient <age 公钥>`。核对成员：`hub.sqlite`、`secrets/`、`bootstrap-token` 必须为 present；WAL/SHM、`vapid.json`、`push.sqlite*` 要么为 present，要么明确标为 absent。
   - 再正式执行：`REMUDA_BACKUP_YES_I_KNOW=1 scripts/hub-backup.sh --data-dir <DATA_DIR> --output-dir <备份目录> --recipient <age 公钥> --execute`。
   - 备份目录不得位于任何 `deploy/` 目录之下，也不得位于数据目录之内。
3. 核对旁车 `.sha256` 文件。`.age` 与旁车各保存一份到带外位置；age 私钥不放在 SG 主机上。
4. 保留旧镜像 `remuda-hub:9dd7ec7`，回滚时要用。

### 4. schema 兼容性演练（唯一的 schema 证据，不碰活数据）
1. 新建一个空目录（0700，属主为 HUB_UID），用 `age -d` 解密备份并解包进去，再按 manifest 逐个文件核对 SHA-256。
2. 用新镜像只对副本做迁移，不联网：`docker run --rm --network none --user <HUB_UID>:<HUB_GID> -v <演练目录>:/data -e REMUDA_DATA_DIR=/data remuda-hub:<TARGET7> hub --migrate`。退出码必须为 0，日志里不能有迁移错误。
3. 失败就停：不做第 5 步，保持旧 Hub 运行，把输出交给所有者。成功后，安全删除演练目录（里面有密钥）。

### 5. 升级
1. `docker load -i <镜像归档>`，核对镜像 ID 与第 1 步一致。
2. 在私有 `.env` 中只改 `HUB_IMAGE=remuda-hub:<TARGET7>`，其他值不动；用 `docker compose ... config` 复核。
3. 执行 `docker compose ... up -d remuda-hub`，等待健康检查变为 healthy（用 `docker compose ... ps` 查看），并确认日志里没有迁移错误。
4. 验证：
   - `docker compose ... exec remuda-hub /usr/local/bin/remuda version --json`：`git_sha` 等于 `<TARGET>`；
   - `curl -fsS https://remuda.<zone>/healthz` 返回 200，且 `version` 等于上一条输出的 `version` 字段（包版本）。`/healthz` 不带提交号，提交号只看 `git_sha`；
   - `/` 的响应体与该构建 `web/dist/index.html` 的 SHA-256 一致；
   - Caddy 容器 ID、启动时间、Caddyfile 哈希、网关健康哈希与第 2 步的基线一致。
5. 用已配对的笔记本浏览器登录 `https://remuda.<zone>/`，确认设备与历史都还在（数据延续自内网 Hub）。

### 6. bootstrap 轮换（按需）
- 什么时候需要：访问码曾出现在所有者私有文件之外（终端回滚、共享笔记、证据草稿等），或者回滚到了轮换之前的备份。D2 不是换机，所以不是必做项。
- 怎么做：
  1. `docker compose ... stop remuda-hub`；
  2. `docker compose ... run --rm --no-deps remuda-hub hub rotate-bootstrap`。新访问码打印到 stdout，只写入私有的 0600 文件，不进 argv 历史、日志或证据；
  3. `docker compose ... up -d remuda-hub`。
- 已配对设备的 token 不受影响（D-018），只影响以后的新配对。

### 7. 手机与推送
1. 在笔记本浏览器的 Settings 里生成配对码；手机经**获准的内网路由**打开 `https://remuda.<zone>/login?pair`。iOS 需要先添加到主屏幕。
2. 在 `/m/inbox` 的横幅或 Settings → 通知里开启推送。
3. 推送测试：在 devbox Node 上开一个会提问的短会话。在第一阶段代码合入之前，只要有任一设备 follow 该会话，所有设备都会被静音，所以测试期间桌面端不要打开它。确认手机收到「Need your input」后，删除这个会话。
4. 手机在办公网之外也有获准的路由到达内网 Hub（所有者已确认，D-057 OA4）。在办公网之外再做一次：打开 Hub，并收到一条测试推送。

### 8. Node 接入

**devbox（常驻；Main、worker、gate lanes、项目 home host 都在这里）**
1. 以 devbox 运行用户的身份，把同一提交的 Linux 二进制放进该用户的私有 bin 目录，用 `remuda version --json` 核对 `git_sha`。
2. 由已登录设备生成一次性 enroll token（在主机页操作，或调用已认证的 `POST /v1/hosts/enroll-token`）。
3. 执行 `remuda --data-dir <稳定的私有目录，不在 /tmp> node install --systemd-user --hub 'https://remuda.<zone>' --enroll-token '<one-shot>'`，然后从 shell 历史里清除 token。数据目录要与任何既有 Node 或 demo 的数据目录分开（Herdr 会话名按数据目录派生，互不干扰）。
4. 如果要在用户注销后继续运行，需要 systemd linger，由所有者或管理员按策略决定。
5. 核对：
   - `remuda --data-dir <目录> node status` 正常；
   - Hub 主机页显示该主机 online，CLI 清单列出 claude、codex、herdr 及版本；
   - **主机记录必须登记 herdr**：用 Human 设备读取 `GET /v1/hosts/<devbox-host-id>`（或打开主机详情），`herdr` 字段必须非空。只在 CLI 清单里出现 herdr 不够；
   - 记录该 Node 是否报告原生 `shell-pty` 可启动（`capabilities.driverInventory`）；
   - 在 Hub 上为它登记持久工作区（项目检出所在的家目录卷，不是 /tmp）。
6. 说明：如果 Node 报告原生 shell-pty 可启动，不带 `--carrier` 的 Claude dispatch 会选 shell-pty。shell-pty 属于 shell 驱动，Agent 来源要做一次性人审（D-017）。免审的同 host 路径是显式 `--carrier herdr`，或 codex/grok worker（generic-pty，同样依赖 herdr）。**主机记录登记 herdr 之前，不要入座 Main。**
7. 重启一次该用户服务，确认 Hub 上仍是同一个 host id，没有出现重复主机。
8. 不要用 pkill/killall 按模式杀进程；不要动现有 demo 或脚本化 gate 的进程。

**笔记本（普通 Node，会休眠；保留已接入的身份，D-057 OA5）**
1. 沿用内网验收时安装的 Mac Node 数据目录与 host id，不重新 enroll：换成同一提交的 macOS 二进制，重启它的 launchd 用户代理，确认 Hub 主机页上 host id 不变。
2. 只有当该数据目录已经不在、Hub 主机页也不再列出该主机时，才报告所有者并重新接入：`remuda --data-dir <稳定的私有目录> node install --launchd --hub 'https://remuda.<zone>' --enroll-token '<one-shot>'`。安装器拒绝覆盖属于其他数据目录的单元；需要时，先对旧数据目录执行 `node uninstall`（保留 journal 与身份）。
3. 笔记本上的 demo Hub（本地 `remuda dev`）成为历史：确认上面没有所有者的会话后停掉它，不迁移它的数据。
4. 合盖后，Hub 应显示该主机 offline；唤醒后自动恢复 online，host id 不变。

### 9. 项目、lanes 与 provider（所有者在 Hub 上配置）
- 在内网 Hub 上登记自开发项目（D2：不从 demo 迁移）。成员是 devbox 工作区（笔记本工作区可选），home host 是 devbox，gate lanes 在 devbox 上。需要时调整项目策略，例如 `coordinatorFanOut`（默认 8）。
- provider 在各 Node 本地按主机配置。凭据不写进 Hub 的 `.env`，也不复制到别处；Hub 只保存 profile id。
- 脚本化 landing gate 与 Hub 的 gate lanes 共用 devbox 的端口和 e2e 锁。在切换（D4）之前，两者不要同时运行。
- 委托决策（D-051）对自开发项目开启（D-057 OA3）：在 Hub 进程环境里设置 `REMUDA_DELEGATED_DECISIONS_FORCE_ON=<project-id>`（项目登记后才有 id）。当前 `deploy/intranet/compose.hub.yml` 不透传这个变量，由所有者或运维以审阅过的方式加入 Hub 服务环境，用 `docker compose ... config` 复核后执行 `up -d remuda-hub`。这一步只改 Hub 环境，不涉及 Caddy；入座 Main 之前必须生效。

### 10. 验收命令汇总
- `docker compose --env-file .env -p remuda-intranet -f compose.hub.yml exec remuda-hub /usr/local/bin/remuda version --json`：`git_sha` 等于 `<TARGET>`。
- `curl -fsS https://remuda.<zone>/healthz`（开启证书校验）：`ok` 为 true，`version` 等于上一条输出的 `version` 字段。
- `/` 响应体哈希与构建产物一致。
- Caddy 的四项基线与升级前一致。
- schema：第 4 步的 `hub --migrate` 演练退出码为 0（不以 `schema_major` 为证据）。
- 主机页：devbox 与笔记本都显示 online；断开再重连不会产生重复主机；笔记本的 host id 与升级前相同。
- devbox：`remuda --data-dir <目录> node status` 正常；`GET /v1/hosts/<devbox-host-id>` 的 `herdr` 非空。
- 手机：配对成功，并收到测试推送，其中一次在办公网之外。
- D-051 开关：`docker compose ... config` 显示 Hub 环境含 `REMUDA_DELEGATED_DECISIONS_FORCE_ON=<project-id>`。
- 第二次执行时（第一阶段代码合入后）额外确认：各 Node 的 hello 报告了 fence 能力（看主机详情或 Hub 日志），见 main-agent.md §7.6。

### 11. 回滚
- **新 Hub 起不来，或迁移失败**：
  1. `docker compose ... stop remuda-hub`；
  2. 保留失败的数据目录，供诊断；
  3. 在一个**新的空目录**里，按第 4 步的方式恢复升级前的备份（迁移只前滚，旧二进制不保证能读迁移后的库）；
  4. 在 `.env` 里改 `DATA_DIR=<新目录>`、`HUB_IMAGE=remuda-hub:9dd7ec7`；
  5. `up -d`；
  6. 按第 5.4 步核对，此时 `git_sha` 应为 9dd7ec7。
- **Node**：Hub 确认健康之前，不升级 Node。如果 Node 已经升级而 Hub 又回滚了，把 Node 二进制也换回原版本，再重启它的服务。
- **Caddy**：本流程不改 Caddy，无需回滚。如果基线对比发现 Caddy 变了，立即停止，按 deploy-runbook 的回滚流程处理（需所有者批准）。
- **访问码**：回滚到轮换之前的备份后，旧访问码会重新有效，需要按第 6 步再轮换一次。
- 回滚会丢失升级后新产生的数据（新会话、新配对）。回滚前先告知所有者。

### 12. 第一阶段代码合入后：第二次执行、入座、取证与切换
1. 按第 1–5 步把 Hub 升级到包含第一阶段的提交，再在空闲时依次升级 devbox 与笔记本 Node（第 8 步）。确认每个 Node 都报告了 fence 能力，确认 devbox 主机记录的 `herdr` 非空，并确认 D-051 开关已对自开发项目生效（第 9 步）。未升级的 Node 会把 Hub 生成的重启通知错记成 Agent 来源，所以全部 Node 升级完成之前不入座。
2. 所有者用 Human 设备入座 Main（以下全部为占位）：
   `remuda instance create --host <devbox-host-id> --workspace-id <workspace-id> --kind claude --driver claude-sdk --name main --title Main --grant address-owner [--grant <grant> …] --scope-project <project-id> --scope-host <devbox-host-id> --permission-mode <mode> --model <model> --restart process-loss:3 --prompt-file <启动说明>`

   Main 由 `address-owner` 定义；其他 grants（包括 `land`）与权限档位和任何实例一样，是入座时选定的配置，本清单不作建议（D-057 OA2）。`process-loss:3` 是建议的重启上限，每小时 3 次（OA5）。
3. 取证窗口内，人工 coordinator 与脚本化 gate 保持空闲。按 main-agent.md §14.1 的 E1–E9 取证，写入 `docs/design/evidence/main-agent-1.md`（脱敏）。E2 中的 worker 用 `--carrier herdr` 或 codex/grok。E3 要杀掉 Main 进程，由所有者或 coordinator 在 devbox 上执行，不由 worker 执行。
4. 全部通过后，一次性切换（D4）：
   - 人工 coordinator 停止派工、gate、land；
   - 脚本化 gate、会话级 monitor 与 cron 退出循环路径；
   - 所有者的 Human 设备仍可读、可回答。这是流程约定，Remuda 没有只读的 Human token。

   任何一项不通过：从手机暂停 Main，恢复人工循环，把缺口记为任务。
