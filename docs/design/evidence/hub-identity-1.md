# Hub 身份密钥与 Node 端 pin：规格要点、攻击链与现状引用

Date: 2026-09-22. Batch: c-hubidentity. Scope: docs-only（不改 Rust 实现、不改 wire
版本号、不改 store schema、不引入新依赖）。

Design references: [protocol.md §7.7](../protocol.md)（本批新增的规格正文）、
[D-035](../decisions.md)（fail-loud 拒绝形状；决策小节起于
`docs/design/decisions.md:232`）、D-018（一次性 enroll token 带外通道）、
[deploy-runbook.md](../deploy-runbook.md) 备份要求。

本文不截图；攻击链以现状代码的 file:line 为证据，所有行号对应当前工作树，可逐条复核。

## 1. 规格要点（详见 protocol.md §7.7）

1. **Hub ed25519 身份密钥**：首次启动在 Hub `data_dir` 下生成
   `hub-identity/identity.ed25519`（`0600`，目录 `0700`）与同名 `.pub`（`0644`），
   `create_new` 原子收敛、已存在绝不重生成。私钥必须随 `bootstrap-token`、master key
   与其他 secret envelope 一起进备份集——恢复丢钥 = 全体 Node pin 失效、必须重新
   enroll。计划性轮转走「新旧并存 + 旧私钥签转签声明」；疑似泄露走无宽限应急轮转。
2. **Node 端 pin**：enroll 时在 `enrollment.json` 增加可选 `hubPin`
   （`alg/keyId/fingerprint`，指纹 = 公钥 DER 的 SHA-256），与 `nodeToken` 同一次
   原子写落盘；TOFU 的信任锚是 D-018 的一次性 enroll token 带外通道。pin 不匹配 =
   fail-loud 拒绝（D-035）：不覆盖、不降级、不自动重 pin。
3. **签名帧**：hello result、`instance.create`、任何 `origin:"human"`/Bot 特权帧、
   `api.egress` 安装与撤销帧必须签名。签名基覆盖 `(nonce, hostId, seq, frame_type)`
   并附 `payloadSha256`：nonce 防跨连接重放、hostId 防跨主机挪用、seq 防会话内重放/
   重排、frame_type 防帧类型掉包、payload 摘要防参数篡改。验签在统一派发之前。
4. **迁移**：推荐零宽限重新 enroll；grace 路径 ≤14 天硬截止、首次 TOFU 必须人工带外
   核对完整指纹、已有 pin 的节点在 grace 内同样强制验签。grace 的残余风险是 TOFU
   首连被冒接，必须在运维文档中复述。
5. **不做**：不实现、不改 wire 版本（`crates/remuda-protocol/src/lib.rs:62`）、不动
   schema、不加依赖、不做公网前门、不签 Node→Hub 方向。

## 2. 攻击链复述（对抗审查结论）

前提：攻击者处在 Node 与所配置 Hub 名字之间的解析/路由位置，并能取得该名字的一张
**合法**证书（公开信任体系内签发即可，不需要 Node 主动装根）。

1. Node 用 outbound WSS 拨号，`connect_async` 只做默认 TLS 校验——证书链合法且名字
   匹配即通过（`crates/remuda-node/src/transport/wss.rs:496-507`）。没有任何
   application-layer 字段让 Node 识别「这是不是我 enroll 的那个 Hub」。
2. Node 在握手时自报家门：hello 参数携带持久化 `hostId` 与
   enrollment/Bearer token（协议结构 `crates/remuda-protocol/src/hubnode.rs:494-497`；
   Node 侧构造 `crates/remuda-node/src/transport/hubnode_codec.rs:888`）。Hub 方向
   的认证只有 Node→Hub 一侧：`node.auth` 比对 Bearer
   （`crates/remuda-hub/src/ws.rs:418-427`），Hub 再从参数取 `hostId` 入账
   （`crates/remuda-hub/src/ws.rs:428-441`）。冒名 Hub 只需应答 hello，不需要任何
   对应私钥。
3. 握手之后，Node 把对端帧全部当作权威 Hub envelope：
   - `wire_origin` 直接读帧参数里的 `origin`，注释自述「Only the Hub envelope is
     authoritative」（`crates/remuda-node/src/origin.rs:39-43`，注释在
     `origin.rs:40-41`）。伪造方可以把任意帧标成 `origin:"human"`，越过人类专属
     审批门；该函数的调用点覆盖 close/create/send/cancel/configure/respond/keys
     （`crates/remuda-node/src/transport/hubnode_codec.rs:270`、
     `hubnode_codec.rs:463`、`hubnode_codec.rs:538`，另见 570/595/671/703）。
   - 伪造方可下发任意 `instance.create`（派发实现 `hubnode_codec.rs:463` 附近的
     create 路径），以人类身份在 Node 上启动 agent。
   - 伪造方可下发 `api.egress`——协议中**唯一**携带网关凭据的帧
     （`crates/remuda-protocol/src/hubnode.rs:127-130`，参数结构
     `hubnode.rs:1025-1040`；其 `Debug` 被专门手写以避免 `auth_token` 入日志）——
     从而直接收取模型 API 凭据，或以伪造 `revoke` 做拒绝服务。
4. 现状没有「未知通知静默容忍」之外的第二道关：统一派发表是 `dispatch_frame`
   （`crates/remuda-node/src/transport/hubnode_codec.rs:214`），任何过了握手的帧在
   进入它之前没有身份校验；而 `unhandled`（`hubnode_codec.rs:324`）对未知通知的容忍
   不能被解读为对缺签名帧的容忍——后者是 §7.7 明确禁止的。

纯内网拓扑中此路径被网络边界挡着；但它不依赖公网暴露，且任何未来可达性方案都会先撞上
它，所以 owner 裁定本批只补这条前置（规格），不做公网前门。

## 3. 现状代码与规格落点的对应

| 规格条目 | 现状锚点（file:line） | 现状行为 |
| --- | --- | --- |
| Hub 身份密钥文件位置 | `crates/remuda-hub/src/store.rs:1468-1470` | `Store::open` 在 `data_dir` 建/开 `hub.sqlite`；规格中的密钥目录与之同级，尚无代码创建 |
| 私钥落盘写法可照抄 | `crates/remuda-hub/src/auth.rs:57-67` | `bootstrap-token` 以私有文件辅助 `0600` + sync 写入 `data_dir` |
| `create_new` 收敛写法可照抄 | `crates/remuda-node/src/identity.rs:11-34` | Node host-id 首次生成、并发启动收敛、坏内容 fail-closed |
| Node pin 落点 | `crates/remuda-node/src/enroll.rs:38-47`（结构）、`enroll.rs:11`（文件名） | 现仅 `hostId` + 可选 `nodeToken`，无 `hubPin` |
| pin 的原子写 | `crates/remuda-node/src/enroll.rs:82-107` | 临时文件 + `0600` + rename + dir sync，规格要求 pin 与 token 同一次原子写 |
| TOFU 带外锚 | `crates/remuda-node/src/enroll.rs:13-36` | 一次性 `REMUDA_ENROLL_TOKEN`（D-018），旧 bootstrap 环境变量只发警告 |
| Node 自报 hostId | `crates/remuda-protocol/src/hubnode.rs:494-497`；`crates/remuda-node/src/transport/hubnode_codec.rs:888` | `NodeHelloParams.host_id` 可选；stdio/hello 构造直接填入 |
| Hub 侧只做 Node→Hub 认证 | `crates/remuda-hub/src/ws.rs:418-427`、`ws.rs:428-441` | `node.auth` 比 Bearer；hello 从参数取 `hostId`，无 Hub 签名 |
| 传输层仅默认 TLS | `crates/remuda-node/src/transport/wss.rs:496-507` | `connect_async`，合法证书 + 名字匹配即通过 |
| origin 信任单点 | `crates/remuda-node/src/origin.rs:39-43` | 直接读 `params.origin`，无对端身份校验 |
| origin 调用点 | `crates/remuda-node/src/transport/hubnode_codec.rs:270`、`463`、`538`、`570`、`595`、`671`、`703` | close/create/send/configure/cancel/keys/respond 全部经 `wire_origin` |
| 验签应插入的位置 | `crates/remuda-node/src/transport/hubnode_codec.rs:214` | `dispatch_frame` 是所有 carrier 的唯一派发表 |
| 凭据帧 | `crates/remuda-protocol/src/hubnode.rs:127-130`、`hubnode.rs:1025-1040` | `api.egress` 常量与 `ApiEgressParams`（含内存态 `auth_token`、手写 redacted `Debug`） |
| seq 不可复用既有水位 | `crates/remuda-node/src/transport/hubnode_codec.rs:17-27` | `SeqWatermark` 是每实例 journal durableSeq，规格签名 seq 是每连接传输计数 |
| wire 版本不改 | `crates/remuda-protocol/src/lib.rs:62` | major 1 / minor 0，本批不变 |
| 备份归属 | `docs/design/deploy-runbook.md:128-130` | 升级前一致性备份 SQLite，并保留匹配的 bootstrap、master key 与其他 secret envelope |

三个代码点本批只加 doc comment、零行为改动：`crates/remuda-hub/src/store.rs:1468`
（密钥目录的指定位置）、`crates/remuda-node/src/transport/hubnode_codec.rs:888`
（握手单向认证缺口）、`crates/remuda-node/src/origin.rs:39`（envelope 权威但无 Hub
身份）。

## 4. 验收对照

- 每条必签帧的签名基写明覆盖 `(nonce, hostId, seq, frame_type)` 并逐项解释，
  `payloadSha256` 同列必签——protocol.md §7.7.4。
- 指纹不匹配 = fail-loud 拒绝，显式引用 D-035——protocol.md §7.7.3。
- `git diff --stat` 仅含 `.md` 与三个 `.rs` 文件的 doc comment 新增；无 wire 版本号、
  schema、依赖变化。
- 本文与 §7.7 的每条 file:line 由 `scripts/tests/test_topology_doc.py` 静态校验存在性，
  并人工对照过当前工作树。
