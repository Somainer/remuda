# Remuda deploy runbook：SG 内网 Hub + 现有 Caddy

当前交付状态：**prepared only — awaiting-caddy-restart-approval**。按用户最新
指示，准备变更与回滚脚本，等待明确的 Caddy 重启批准；不得因脚本已准备好就
执行重启或继续 Node 接入验收。此前操作和基线恢复以
[实施证据](./evidence/intranet-hub-1.md) 为准，不把临时探测当作最终启用结果。

当前支持入口是 [deploy/intranet](../../deploy/intranet/README.md)。按 D-020
后续优先级调整，先把 Hub 放到 `<sg-host>`，复用现有 Caddy 的 DNS-01，使用
独立域名 `remuda.<zone>`。未来公网 VPS 方案保留在
[deploy/public](../../deploy/public/README.md)，本轮不部署公网 VPS。

SG 没有公网 IP；域名 A 记录指向内网地址。DNS-01 可签发 HTTPS 证书，但不会
让该地址变成公网可达。浏览器、手机和 Node 必须有获准的内网路由。普通蜂窝
网络访问不属于此次内网验收。cloudflared、frp、ngrok、长期 `ssh -R` 等隧道/
内网穿透均不适用；旧 [cloudflared 文档](../../deploy/cloudflared.md) 仅作历史记录。

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
