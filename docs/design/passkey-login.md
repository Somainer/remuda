# Passkey 登录（WebAuthn）

日期：2026-09-14 · 决策：**D-030** · 证据：[evidence/passkey-1.md](./evidence/passkey-1.md)

## 1. 目标与边界

登录不再需要手输访问码：浏览器/系统里保存的 **Passkey（WebAuthn discoverable
credential）** 成为主登录方式；在设置界面可添加 / 重命名 / 删除 Passkey。

硬边界：

- **第一次配对仍然必须用访问码（D-018）**。Passkey 注册接口要求设备已登录——
  只能从一台已配对的设备上添加。没有「纯 Passkey 自助注册」路径，否则访问码
  轮换和 TTL 这道 bootstrap 闸就形同虚设。
- Passkey 登录成功后发放的设备 token / cookie 与 `POST /v1/login`、
  `POST /v1/devices/pair` **完全相同**（同一 Argon2id 哈希、同一
  `remuda_device` HttpOnly cookie、同一 `DeviceSession` JSON）。Passkey 不是
  新的主体：沿用 D-018 的「单一操作者 + 其配对设备」模型，只新增一种兑票方式。
- 访问码路径保留，作为 bootstrap 与兜底入口，在登录页降为次级「使用访问码」。

## 2. 协议

### 2.1 RP ID 与来源（永不硬编码主机名）

WebAuthn 的凭据与 **RP ID**（来源的可注册域后缀）和 **expected origin** 绑定，
二者按请求实时解析，不写死任何主机名：

1. 配置了 `--public-origin` / `REMUDA_PUBLIC_ORIGIN`（如
   `https://remuda.<zone>`）：用它的 host 作为 RP ID，expected origins =
   `public_origin` + `allowed_origins`。
2. 未配置（`remuda dev` / 本地 demo）：来源取已通过 CSRF 校验的请求 `Origin`，
   只接受 host 为 `127.0.0.1` / `localhost` 的回环来源；RP ID 就是回环 host
   本身（`127.0.0.1` 或 `localhost`）。回环来源允许 `http://`（WebAuthn 规范对
   loopback 的安全来源豁免）。
3. 其余情况只接受 `allowed_origins` 精确匹配，且必须是 https。

`remuda dev` 自动把 web/node 的回环来源加进 allowlist（既有逻辑）。

**同一把 Passkey 不能跨来源使用**：内网 `https://remuda.<zone>` 与本地
`http://127.0.0.1:<port>` 是两个独立的 RP，需要分别注册。设置页与登录页都明确
提示这一点，避免「在家注册的 key 在公司用不了」被当成 bug。

实现注记：0.5.5 的安全构造器 `WebauthnBuilder::new` 用 `Url::domain()` 校验
RP ID，而 `domain()` 对 IP 字面量返回 `None`，因此它无法构造
`http://127.0.0.1:<port>` 的 RP；且包装 crate 没有重新导出能绕过该校验的
`WebauthnCore::new_unsafe_experts_only`。Hub 直接依赖
`webauthn-rs-core = "=0.5.5"`（MPL-2.0），逐参数复刻包装 crate 的 passkey
ceremony（attestation `none`、UV required、resident key 偏好见 §2.3、
secure 算法集、与包装一致的扩展集），origin allowlist 在我们自己手里收敛，
不放宽任何校验。

### 2.2 端点

全部在 `/v1/auth/passkeys` 下：

| 方法 路径 | 鉴权 | 作用 |
| --- | --- | --- |
| `POST /register/start` | 设备 | 返回 `CreationChallengeResponse`（透传给 `navigator.credentials.create`） |
| `POST /register/finish` | 设备 | 校验 attestation，落库新凭据 |
| `POST /login/start` | 无 | 返回 `RequestChallengeResponse`；显式按钮走普通 discoverable，输入框 autofill 走 conditional mediation |
| `POST /login/finish` | 无 | 校验 assertion，发放设备 cookie，审计 |
| `GET  /` | 设备 | 列出凭据（名称、创建时间、最近使用、本机提示） |
| `PATCH /{id}` | 设备 | 重命名 |
| `DELETE /{id}` | 设备 | 删除 |

### 2.3 凭据与可发现性

- registration：`attestation: "none"`，`userVerification: required`，
  resident key **required**（discoverable 是主 UX：登录页无需输入任何标识；
  同步型平台认证器（iCloud/Google/1Password）本就存 resident）。
- 单操作者部署：WebAuthn `user.id` 用**每个 RP 固定的 UUID v5**（命名空间 +
  RP ID 派生），`user.name` / displayName 用操作者标签；这样同一 RP 下所有
  凭据指向同一账号，登录后服务端按 credential id 找 key 即可。固定 user.id 也
  让排除列表（excludeCredentials，防止重复注册同一把 key）稳定。
- 登录 start 不带 `allowCredentials`（discoverable / conditional）；finish 从
  assertion 的 userHandle + raw credential id 定位库里的凭据，再交给
  webauthn-rs 校验。

### 2.4 挑战状态

- 进程内 `Mutex<HashMap<token, (kind, state_json, expires, 用途元数据)>>`，
  每次 start 生成随机 128-bit challenge token 作为 key 返回给浏览器（放在
  challenge JSON 之外的信封字段 `challengeId`），finish 按 token 取出并
  **一次性删除**。
- TTL 120 秒；过期或重放一律失败。惰性清理 + 容量上限（默认 4096，与限流表
  一致），超限拒绝新 start。
- 注册挑战绑定发起设备 id；登录挑战无绑定。

### 2.5 存储

新表 `passkeys`（见 store schema）：

```
id            TEXT PRIMARY KEY          -- psk_<uuid>
credential_id TEXT NOT NULL UNIQUE      -- webauthn credential id（base64url 原样）
public_key    TEXT NOT NULL             -- webauthn-rs Credential 的 JSON
counter       INTEGER NOT NULL DEFAULT 0
transports    TEXT NOT NULL DEFAULT '[]'
name          TEXT NOT NULL
aaguid        TEXT                      -- attestation none 时通常为空
created_by    TEXT NOT NULL             -- 发起注册的设备 id（“本机”提示）
created_at    TEXT NOT NULL
last_used_at  TEXT
```

设备→凭据的配对记录不另加列：登录审计 detail 里带 `passkeyId`，需要时从审计
表回答「这台设备由哪把 key 配进来」。`credential_id` 唯一索引在数据库层兜底
webauthn-rs 文档要求的「credential id 不得注册给两个账号」。

### 2.6 威胁模型与对策

- **挑战重放**：single-use（取出即删）+ 120 s TTL。
- **签名计数器克隆检测**：沿用 webauthn-rs 的
  `require_valid_counter_value = true`；认证成功后用
  `Credential`/`AuthenticationResult` 更新 counter。同步型 passkey 计数器常为
  0，按规范豁免（两边都为 0 时不判克隆）。
- **凭据枚举**：`credential_id` UNIQUE；登录失败统一返回 401，不区分「无此
  key / 签名错 / 挑战过期」；login start 恒返回挑战，不依据任何账号信息。
- **限流**：`login/start` 与 `login/finish`、`register/start` 与
  `register/finish` 并入现有 token-bucket 认证限流（每 IP 独立桶 + 全局限流），
  与 `/v1/login` 同预算参数。
- **CSRF / 跨源**：所有 mutating 路由先过既有 `require_origin`；WebAuthn 自身
  再做 origin 精确匹配 + RP ID 绑定（抗钓鱼的核心）。
- **UV**：注册与认证都要求 user verification；webauthn-rs 对 UV 位做服务端校验。
- **删除最后一把 key**：允许（访问码永远可重新 bootstrap），不产生锁死。
- **审计**：`passkey.register` / `passkey.login` / `passkey.delete` /
  `passkey.rename`，subject 为 passkey id，login 的 device_id 为新发设备。

## 3. Web 客户端

### 3.1 登录页

- 主按钮「使用 Passkey 登录」→ `navigator.credentials.get({mediation:"required"})`。
- 支持时启用 **conditional mediation**：一个隐藏/普通输入框
  `autocomplete="username webauthn"`，页面加载即发起
  `mediation:"conditional"`，浏览器在 autofill 里直接列出 passkey。
- 「使用访问码」折叠出既有的 bootstrap / 配对码表单（testid 全部保留）。
- `PublicKeyCredential` 不存在（http 非回环来源、老浏览器）时，Passkey 区域显示
  明确说明，访问码表单自动展开。
- 400px 宽度可用；全键盘可达；iOS 主屏 PWA（standalone）下 platform
  authenticator 可用（来源必须与注册时一致）。

### 3.2 设置页

新增「Passkeys」分区：

- 「添加 Passkey」：先询问名称（默认取浏览器/OS 提示，如
  `Chrome on macOS` / `Safari on iPhone`，由 UA 客户端给默认值），再走
  create 仪式。
- 列表：名称、创建时间、最近使用、由哪台设备注册（当前设备显示「本机」）。
- 重命名（PATCH）、删除（确认弹窗）。
- 固定提示：内网与本地为不同来源，Passkey 需分别注册。

### 3.3 编码

WebAuthn 的二进制字段用 base64url（无填充）在 JSON 里传输；`web/src/lib/passkeys.ts`
负责 `ArrayBuffer ↔ base64url`、特性探测、`isConditionalMediationAvailable()`、
create/get 信封封装与错误归一化。

## 4. 测试

- Hub：集成测试用内置的最小 ES256 软件认证器（测试模块，0.5.5 不带
  softtoken）完成 register→login 全链路；覆盖：错误 origin 拒绝、重放挑战
  拒绝、重复 credential id 拒绝、counter 回退判克隆、conditional 挑战 mediation
  标记、删除后登录 401、限流覆盖。
- Web：Vitest + jsdom，mock `navigator.credentials`，测特性探测、base64url
  编解码、登录/注册/删除流程与降级 UI。
- E2E（hub-live，Chromium）：CDP `WebAuthn.addVirtualAuthenticator`
  （resident key + user verification）：设置页注册 → 退出 → Passkey 登录；
  负例：删除该虚拟认证器后登录失败。e2e 不改动任何被 git 跟踪的文件。

## 5. 配置与运维

- 新增配置：无新必填项。RP/origin 全部复用 `public_origin` /
  `allowed_origins`。生产部署（`deploy-public.md`）必须设 `--public-origin
  https://<对外域名>`，否则浏览器只在回环来源上暴露 Passkey API。
- 依赖：`webauthn-rs-core = "=0.5.5"`（MPL-2.0，与仓库既有许可兼容；锁文件
  钉死补丁版本）。
- 不持久化挑战状态：Hub 重启使进行中的仪式失效，客户端重新 start 即可。

## 6. Node 侧 device passkey 联署（规格；本批不实施）

日期：2026-09-22 · 状态：**规格已裁定，未实现** ·
证据：[evidence/node-cosign-1.md](./evidence/node-cosign-1.md) ·
相关：D-017、D-018、D-030（本节）、D-035、D-045/D-046、并行的 c-hubidentity

本节是**规格**：只定义目标行为。没有任何对应实现，wire 没有对应字段，
任何代码路径都还不按本节工作。

### 6.1 背景与定位

2026-09 一次对抗评审把一条攻击链分成两半：

1. **通道**：Node 对 Hub 的认证只在连接建立时——WSS upgrade 的 Bearer
   host token（`crates/remuda-node/src/transport/wss.rs:55-80`；持久化在
   `crates/remuda-node/src/enroll.rs:44-46`），ssh-stdio 桥逐帧透传 JSON。
   这是连接级的对称持有秘密，**不是逐请求签名**。
2. **请求**：连上之后，帧里的 `origin` 在 Node 侧直接取自 params
   （`crates/remuda-node/src/origin.rs:39-43`；c-hubidentity 合入后该函数
   上方 25-38 行新增了同指此缺口的 §7.7 doc comment）；Hub 侧所有特权门把它当
   权威：`crates/remuda-hub/src/agent_scope.rs:284`
   （`restrict_permission` 只约束非 Human）、
   `crates/remuda-driver/src/presets.rs:151-166`（Human/Bot 即追加 yolo
   argv）、`crates/remuda-driver/src/launch/skills.rs:137-145`（能力门只挡
   Agent）。Human 路径还允许 `binaryPath` / `args` 自选可执行文件与
   flag（Hub 侧只对 Agent 拒绝，
   `crates/remuda-hub/src/agent_scope.rs:302-314`；字段
   `crates/remuda-node/src/model.rs:79-91`）。

于是任何能以 Hub 身份发帧的一方，发一帧 `instance.create` 带
`origin:"human"`（可再叠 yolo / capabilities / binaryPath），经
`POST /v1/fleet/instances`（`crates/remuda-hub/src/fleet.rs:18`）扇出，
每个 enrolled Node 都物料化并启动它——**全 fleet 远程代码执行**，Node
没有第二道证据可拦。完整链路与逐条引用见
[evidence/node-cosign-1.md](./evidence/node-cosign-1.md) §1。

本节定义第二道证据：**特权请求除通道凭据外，还必须带一把已配对 human
设备上 passkey 对「这一帧的精确字节」的签名（co-signature）**。它与并行的
c-hubidentity（证明通道对端是真 Hub）是两个独立性质：

- 前门解决「谁在发帧」；联署解决「这一帧的特权有没有一个在场的人用设备
  授权过」。Hub 被冒用、host token 泄露、桥上出现冒名者时，前门之外仍有
  这一道。
- 反过来联署不替代前门：Node 不应在未认证通道上接受仅凭断言自证的帧。
  两道门都上线才闭环；本节不定义前门。
- **纯内网下同样成立**：威胁模型假设攻击者已在内网、已拿到 Hub 的网络
  位置；本机制不依赖任何外部服务，只依赖 D-030 已注册的设备 passkey 与
  Node 本地持有的公钥副本。

与 D-030 登录仪式的关系：**复用同一把凭据、同一套注册/存储/审计设施，
但语义不同**。登录证明「这台设备的持有者要建立会话」；联署证明「这台设备
的持有者此刻授权这一个特权操作」。登录断言不能重放成联署（challenge
内容、RP 上下文、审计事件各自独立）。

### 6.2 角色、凭据与信封（术语）

- **co-signer（联署设备）**：一台已配对 human 设备（D-018），其上有按
  D-030 注册的 passkey；WebAuthn 断言必须 `UV=1`。Bot 设备令牌（Hub 为
  进程内 dispatcher 铸的 `kind:"bot"` 凭据，
  `crates/remuda-hub/src/agent_scope.rs:567-581`）**不是** co-signer，
  bot 自己永远签不出特权帧。
- **联署公钥副本**：Node 在本地（node 数据目录，0600，沿用
  `enroll.rs` 的落盘形状）持有允许为它联署的 credential 公钥 allowlist。
  公钥由 Hub 在通道认证通过后同步（同步协议本身随 c-hubidentity 之后的
  wire 批次定义）；Node 对每个 `kid` 首次见到时**钉死**，更新/吊销必须
  显式。allowlist 为空时，所有特权帧一律拒绝（fail closed）。
- **co-sign 信封 E**（未来 wire；在 `HubNodeRequest`
  （`crates/remuda-protocol/src/hubnode.rs:144-159`）上**新增**一个与
  `method`/`params` 平级的可选字段 `cosign`；今天不存在、不解析、不发送）：

  ```jsonc
  {
    "v": 1,
    "kid": "<base64url 的 WebAuthn credential id>",
    "nonce": "<base64url，16 字节随机数，单次有效>",
    "iat": 1758000000,          // 签发时刻（unix 秒）
    "exp": 1758000120,          // 过期时刻；交互式硬上限 iat+120 秒
    "aud": "<目标 Node 的 hostId>",
    "authData": "<base64url authenticator data>",
    "clientData": "<base64url clientDataJSON>",
    "sig": "<base64url 断言签名>",
    // 以下三者（直签 / bundle / coupon）恰好出现一个，见 6.4 与 6.6：
    "bundle": [ {"aud": "...", "method": "...", "params": "<P 的规范编码>"} ],
    "index": 0,
    "coupon": { /* 联署券 C 与其签名，形状见 6.6 */ }
  }
  ```

  `cosign` 是帧的平级信封，**绝不允许嵌进 `params`**：任何在 params 内
  出现的 `cosign` 键都按畸形帧拒绝（防止签名覆盖的对象与执行对象错位）。

### 6.3 哪些请求需要联署（穷举）

判定全部在 Node 侧、对**归一化之后的请求**（归一化见 6.4 第 1 步）独立
进行，不相信 Hub 已替它过滤过。规则用一句话概括：

> **未联署帧的最高特权，恰好等于今天一个 Agent-origin 调用方被允许做的
> 形状**（`manual`/`plan`、无 capabilities、无 `binaryPath`/args 覆写、
> 非 danger sandbox）；超出这条基线的每一帧都必须联署。

#### 6.3.1 必须联署的方法与谓词（闭集）

| # | 方法 | 触发谓词（满足其一即需联署） | 为什么在清单里 |
|---|---|---|---|
| C1 | `instance.create`（`crates/remuda-protocol/src/hubnode.rs:43-44`；Node 臂 `crates/remuda-node/src/server.rs:932-953`、`crates/remuda-node/src/runtime_link.rs:38`、`crates/remuda-node/src/transport/wss/runtime_wss.rs:194,210`） | 归一化后 `origin ∈ {"human","bot"}` | origin 是整条特权链的根判别字段（`agent_scope.rs:284`、`presets.rs:157-159`、`skills.rs:139`）；攻击帧伪造的就是它。即便不带任何 flag，human 形 create 仍在主机上启动进程、注入 provider 令牌与 MCP 上下文（`crates/remuda-node/src/model.rs:124-129`），并可被 fleet 扇出。 |
| C2 | `instance.create` / `instance.resume`（resume 走同一个 create 路径，`server.rs:955-986`） | `permissionMode ∈ {"bypassPermissions","bypass-permissions","bypass"}`（Node 等价接受这三种拼写，`crates/remuda-node/src/native.rs:710-716`；请求字段 `model.rs:95-97`） | bypass 使驱动追加各族 yolo argv，跳过全部工具审批：Claude `--dangerously-skip-permissions`、Codex `--dangerously-bypass-approvals-and-sandbox`、agy `--always-approve`、generic `--yolo`（`crates/remuda-driver/src/presets.rs:57,69,81,93,105`，门在 `:151-167`）。即使帧伪造 origin 失败，bypass 本身也必须独立成谓词——Node 不得因为「这种组合 Hub 会先 403」就省略自己的门。 |
| C3 | `instance.create` / `instance.resume` | Codex `sandbox ∈ {"danger-full-access","danger"}`（`crates/remuda-node/src/native.rs:1456`；字段 `model.rs:98-101`；`--dangerously-bypass-approvals-and-sandbox` 同时移除审批与沙箱，见 `crates/remuda-driver/src/materializer.rs:1078-1093`） | 这是 containment 的移除，与 C2 同级；不单列会留下「bypass 被签、沙箱没签」的绕行。 |
| C4 | `instance.create` / `instance.resume` | `capabilities` 非空（字段 `model.rs:152-156`；今天唯一合法值 `"computer-use"`，未知值即拒，`crates/remuda-driver/src/launch/skills.rs:129-135`；Node host 门 `crates/remuda-node/src/computer_use.rs:35-88`） | D-045：全机 `click`/`typeText` 桌面控制，是 Remuda 授予过的最大爆炸半径（D-045 后果段）。D-045 的两道门只证明「来源声称是 Human/Bot + 主机装了软件」，联署补上「设备上的人此刻确实点了」。 |
| C5 | `instance.create` / `instance.resume` | `binaryPath` 非空 **或** `args` 非空（字段 `model.rs:79-91`；Hub 只对 Agent 拒绝此覆写，`agent_scope.rs:302-314`；Node 三个 carrier 都做了字符串校验但都不拒绝 human，`runtime_wss.rs:369-395`、`runtime_link.rs:237-266`） | `binaryPath` 选择运行的代码、`args` 选择 flag；allowlist 若被绕过，这就是直接的任意程序启动。即使 `manual` 模式，自选二进制也是代码执行原语。 |
| C6 | `gate.run`（协议常量 `crates/remuda-protocol/src/gate.rs:16`；Node 入口 `crates/remuda-node/src/gate.rs:552,564-570`） | 无条件，**每一帧**都要 | 参数 `GateRunParams`（`crates/remuda-protocol/src/gate.rs:412-456`）指定 `repoPath`、`branch` 与环境，Node 随后在该 worktree 里执行分支自带的 gate 步骤（子进程 `remuda … merge --gate`，`crates/remuda-node/src/gate.rs:782-825`）。要跑的代码由被验分支选择，帧本身还能切换 repo/branch/注入 env，是主机侧的执行原语。 |
| C7 | `gate.then`（Node `crates/remuda-node/src/gate.rs:590-596`；参数 `GateThenParams`，`crates/remuda-protocol/src/gate.rs:528-542`） | 无条件 | `command` 是帧里直给的字符串，Node 以 shell 执行（字段文档原文「Command line (`bash -lc`)」，`gate.rs:531-532`）。这是清单里最直白的任意命令执行。 |
| C8 | `gate.land`（Node `crates/remuda-node/src/gate.rs:600-606`；参数 `GateLandParams`，`gate.rs:556` 起） | 无条件 | home host 取 lane 的 merge 并对基线分支做 compare-and-swap **push**（`gate.rs:598-599` 注释）；这是对共享远端的写操作，冒充帧可以把任意「已验证」提交推成主干。 |

C2/C3/C4/C5 与 C1 可能同时成立；验证器对各谓词取并集，任一成立即要求
对应信封，不因为另一谓词已满足而跳过。

#### 6.3.2 审阅过但**不**要求联署的方法（同样是闭集，附理由）

| 方法/族 | 代表入口 | 不签的理由 |
|---|---|---|
| `gate.cancel` | `crates/remuda-node/src/gate.rs:573-587` | 只杀死本 job 的进程组，不启动进程、不写远端；而且它是逃生门，故意要求内联立即执行（`gate.rs:43-51`）。加设备在场要求会让「停不下来」成为攻击放大器。 |
| `gate.unpin` | `crates/remuda-node/src/gate.rs:608` 起 | 只删除 lane 主机上为 pin merge 建的**本地**临时 refs；不触远端、不执行命令。 |
| `worker.provision` / `worker.remove` | `server.rs:886-887` | 只做产品分配的 worktree/target dir 的建与收（目录与本地 git 操作），不选可执行文件、不带 bypass；它们只作为一次已授权 dispatch 的内部环节出现。若未来其参数能指向任意路径/二进制，必须重新进入 6.3.1。 |
| `instance.send` / `instance.cancel` / `instance.close` / `instance.configure` / `instance.respond` / `tty.write`(`instance.keys`) / `tty.resize` / `tty.attach` | `server.rs:990-1011`；常量组 `crates/remuda-protocol/src/hubnode.rs:46-66` | 作用域是**一个已存在的实例**，不创建新执行主体；Agent→这些动作的提升（tty 写、按键、shell-driver 目标、越 scope）已有 D-017 的 one-shot Interaction 批准门（`crates/remuda-hub/src/agent_scope.rs:376-407`）。这是刻意取的最小面，**不是**对「这些帧不可冒充」的证明：c-hubidentity 落地后若评审认为通道内仍可注入，本表第一候选追加的就是 `tty.write`/`instance.keys`。 |
| `workspace.*`、`worktree.*`、`workspace.scm.*`、`host.files.*`、`host.doctor`、`host.resources`、`instance.list/get/purge`、`events.*`、`command.get`、`interaction.list`、`subagent.transcript`、`tty.screen/mode/frame`、`object.pull/chunk`、`api.*`、`node.*`/`runtime.hello`/心跳 | 分发表 `server.rs:846-1109`；常量 `hubnode.rs:32-96` | 只读，或会话/主机自身状态维护，或已用 host token 单独授权的对象字节通道；没有一个能选择主机上要执行的程序或把提交写进共享远端。`instance.purge` 是幂等删除（`server.rs:1012-1017`），破坏性限于 Node 本地的既有会话。 |

新增任何 Hub→Node 方法时，默认归类为「需要联署」，直到在本节表格里留下
一行审阅结论；Node 分发层对未在 6.3.2 名单内、又拿不出信封的写方法拒绝。

### 6.4 被签名字节：canonicalization（安全关键）

目标：**两个不同的请求不可能产生相同的被签名内容；同一请求的任意等价
线上拼写只产生同一份内容。** 验证器（Node）与出示方（Hub）必须逐字节
独立重构出同一份文档；任何一步有歧义即拒绝，不猜测。

**第 1 步：处理器同款归一化。** 从收到的 JSON-RPC 帧取出 `method` 与
`params`（丢弃 `jsonrpc`/`id`/`version`，它们是传输外皮，
`hubnode.rs:144-159`）。对 params 施加与处理器**完全相同**的归一化：

- create/resume：复刻 `server.rs:933-951`（runtime 两 carrier 是
  `create_from_params`，`runtime_link.rs` 的等价 lifting 与
  `runtime_wss.rs:330` 起）——`params.spec` 为对象时取它，否则取整个
  params；解析进 `CreateInstanceRequest`（camelCase，
  `crates/remuda-node/src/model.rs:42-169`）；同样地把外层 `instanceId`、
  `initialInput.text`（仅当 prompt 为空）、resume 各字段 lift 进结构体。
- gate 三方法：分别解析进 `GateRunParams` / `GateThenParams` /
  `GateLandParams`（`crates/remuda-protocol/src/gate.rs:412-456`、
  `:528-542`、`:556` 起）。
- **从类型化结构体重新序列化**得到参数对象 `P`，而不是对原始 JSON 做
  签名。这样 `null` 与缺键、字段顺序、camelCase 差异、外层/spec 嵌套
  差异全部收敛成同一个 `P`——签名覆盖的是解析后的语义值，不是某种拼写。
  serde 的 `skip_serializing_if` 与字段顺序（结构体声明顺序）本身就是
  规范的一部分；结构体字段变动 = 规范版本变动（见第 5 步 `v`）。
- **唯一允许不进 `P` 的键**：`agentCredential`。它是传输专用、实例绑定的
  临时令牌，现行代码已声明「Never serialized into a request digest,
  journal, or launch recipe」（`crates/remuda-node/src/origin.rs:70-78`，
  且为 `skip_serializing`，`model.rs:52-54`），Hub 盖戳时也会把它从载荷
  摘除（`agent_scope.rs:56-60`）。将来任何新的「不签名」键都必须先在本节
  登记，否则验证器在归一化时见到无法映射/未登记的键即拒绝（fail closed，
  不静默忽略）。

**第 2 步：JCS 编码。** 对下面第 3/4 步的文档统一采用 RFC 8785 JSON
Canonicalization Scheme（JCS）。把关键规则写死在此，实现不得各取所需：

1. 字节编码 UTF-8，无 BOM；无任何多余空白（成员间、数组间零字节填充）；
   不输出尾随换行。
2. 对象成员按键的 **UTF-16 code unit** 升序排列（JCS §3.2.3 的 surrogate
   对比较规则）；因此本文档写出的键顺序只是给读者看的，权威顺序以排序
   结果为准。数组保持原序。
3. 字符串按 JSON 最小转义：只转义引号、反斜杠与 U+0000–U+001F，小写
   `\u00xx`；正斜杠不转义；其余字符原样输出为 UTF-8（不用 `\uXXXX`）。
4. 数字：整数以最短十进制整数输出；归一化后的文档里允许出现的数字只有
   布尔/整数（u64 范围，如 `iat`/`exp`/`timeoutSecs`/`gateTimeoutSecs`）
   与字符串、对象、数组、null。**任意位置出现浮点数、或超出 u64::MAX/
   i64::MIN 的整数、或 NaN/Infinity，一律拒绝该帧**，不尝试规范化（消灭
   JCS 双精度序列化的歧义面）。注意自由 JSON 字段
   （`providerOverlay`，`model.rs:124-126`）也递归适用本规则。
5. 解析阶段拒绝重复键的对象（JCS 对重复键无语义；类型化重序列化后重复
   键不可能存活，因此入口直接拒）。

**第 3 步：直签挑战文档。** 交互式联署时，被签名的挑战文档 `J` 恰为 JCS
编码的：

```
{
  "v": 1,                         // 联署规范版本；归一化/结构变动即 bump
  "kid": "<credential id，base64url 原样>",
  "nonce": "<16 随机字节 base64url>",
  "iat": <整数秒>,
  "exp": <整数秒，恒有 exp-iat == 120，不接受更长窗口>,
  "aud": "<本 Node hostId；与节点实际 id 不符即拒>",
  "method": "<方法名原文>",
  "params": <第 1 步的 P>
}
```

WebAuthn `clientDataJSON` 的 `challenge` 字段 =
`base64url(SHA-256(utf8(J)))`（32 字节摘要，base64url 无填充）；
`type="webauthn.get"`；`origin` 与 RP ID 沿用 D-030 §2.1 对同一请求实时
解析的结果（内网来源/回环来源是不同 RP 的限制对联署同样适用）；
`crossOrigin` 不为 true。Node 用信封 E 里的 nonce/iat/exp/aud/kid/method
与**自己归一化出的 P** 重构 J，逐字节比对 challenge，并校验：

- `authData` 的 RP ID hash 匹配、flags 位 `UV=1`、（如存在）签名计数器
  相对该 kid 已见值严格增长；两边都为 0 的同步型 passkey 按规范豁免，规则
  同 D-030 §2.6；
- 签名算法只接受 ES256（COSE alg -7）；
- `kid` 在本机联署公钥 allowlist 中，断言签名对该公钥验证通过；
- `nonce` 未被使用过（见 6.5 重放缓存），时钟：仅接受
  `iat-60 ≤ now ≤ exp+60`。

**第 4 步：fleet bundle。** 一次 fleet 派发对应**一次**设备仪式，不为每台
主机各敲一次。把 J 替换为 bundle 文档 `Jb`：

```
{
  "v": 1, "kid": "...", "nonce": "...", "iat": ..., "exp": ...,
  "members": [
    {"aud": "<hostId A>", "method": "instance.create", "params": <P_A>},
    ...
  ]
}
```

`members` 先按 `aud`、再按 `method` 的 UTF-16 序排列（排序是签名内容的
一部分）。每个成员帧的信封携带同一个 `Jb` 的 E 与自己的 `index`；Node
校验 `members[index].aud` 等于自己的 hostId、其 `method`/`params` 与本帧
归一化结果逐字节一致，且 index 不重复使用。增删任一成员、调换顺序、改动
一个参数都会使摘要变化。

**第 5 步：版本协商。** `v` 不匹配的信封一律拒绝（不降级到旧版、不退回
无签名）。Node 软件版本与支持的联署版本在 hello 能力位里声明（随未来 wire
批次）；未声明支持联署的旧 Node 收不到特权帧——Hub 必须以「目标 Node 不
支持联署」拒绝操作，而不是发无签帧。

### 6.5 验证路径、失败处理与 D-035

**位置。** 验证发生在 carrier 边界、分发进任何处理器**之前**。所有能到达
这些方法的 carrier 必须共用同一个验证器入口，不允许各臂自行解释：

- WSS / stdio 主分发：`crates/remuda-node/src/server.rs:838-846`
  （`dispatch_hub_rpc` → `dispatch_rpc`，create 臂在 `:932`、gate 族在
  `:888-890`）；
- runtime 链接：`crates/remuda-node/src/runtime_link.rs:29-38`
  （`dispatch` → create 臂；外层 `attach_runtime` 在 `:10`）；
- 出站 WSS runtime：`crates/remuda-node/src/transport/wss/runtime_wss.rs`
  的方法 match（gate 族 `:306-312`，create 经 `create_from_params`
  `:194,210,330`；其余帧落到
  `crates/remuda-node/src/transport/hubnode::dispatch_frame`，
  见 `runtime_wss.rs:321`）。

顺序固定为：取帧 → 归一化（6.4 第 1 步）→ 按 6.3 谓词判断是否需要信封 →
需要则校验信封（allowlist → J 重构 → WebAuthn 各项 → nonce/时钟/aud）→
通过才进入既有处理器。判断与校验全部基于归一化值，永不回看原始拼写。

**失败形状。** 缺信封、kid 未知、签名错、RP/origin 不符、UV=0、版本不
匹配、过期、时钟超偏、nonce 重放、bundle 成员不符、coupon 谓词不符
（6.6）——全部以**同一个 JSON-RPC 拒绝**返回（同一 code、同一文案，不区分
「没有」与「错了」，对齐 D-030 §2.6 的凭据枚举口径）；服务端审计里各自
留独立 reason。拒绝必须：

- 发生在请求被持久接受、实例入册、任何子进程 spawn、任何物料化文件落盘
  **之前**——与 D-035 决策 (2) 的 Node 侧拒绝同形（「不降级；拒绝发生在
  命令被持久接受之前」，见 [decisions.md](./decisions.md) D-035）；
- **绝不**降级：不剥离 `origin`/capabilities 后按 Agent 形继续，不改发
  manual，不换 carrier，不「先跑再补签」。refuse-never-reroute 在本机制
  上逐字适用（同 D-047 对该规则的援引口径）；
- 计入限流与审计（审计事件建议 `cosign.refused`，带 method、hostId、
  reason，不带任何令牌内容）。

**重放缓存。** Node 为每个 kid 维护「已用 nonce → exp」有界缓存
（TTL = 该 nonce 的 exp，容量上限沿用 D-030 挑战表的 4096 量级，超限拒绝
新帧而不是淘汰未过期条目）。缓存只在内存：Node 重启后、未过期窗口内的
nonce 状态丢失，安全方向是把重放窗口内的再来帧拒掉（操作者重敲一次
即可），不得为了可用性放行。

**公钥轮换与吊销。** kid 钉死后，公钥更新/删除必须是显式同步操作；删除
最后一把联署公钥不产生锁死——D-018 访问码 bootstrap 永远能配进新设备、
重新走 D-030 注册与公钥同步；但在新公钥到达前，该 Node 上所有特权操作
不可用（fail closed），普通 Agent 形派发不受影响（见 6.7）。

### 6.6 无人值守：联署券（coupon）

设备在场是默认值；但既有产品里有两类流程设计上无人值守（lane gate 队列
自动 verify；人类协调员以 cron/调度方式跑批量**普通**派发）。为此允许
人用 passkey **预先**签一张有界授权券。券不是宽松口，是把「设备在场」
换成「设备曾在明确的边界内授权」，边界在券面写死、Node 机械执行。

券文档 `C`（JCS，字段与顺序以 JCS 排序为准）：

```
{
  "v": 1,
  "typ": "node-cosign-coupon",
  "kid": "<credential id>",
  "jti": "<16 字节 base64url，券的全局唯一 id>",
  "iat": <秒>, "exp": <秒>,          // exp-iat ≤ 24 小时（硬上限；建议 ≤ 1 小时）
  "audiences": ["<hostId>", ...],   // 不允许 "*"；逐台列举
  "nMax": <正整数>,                 // 最大兑现次数
  "scopes": [ { "method": "...", "match": { ... } } ]
}
```

`C` 在浏览器里以与 6.4 相同的 WebAuthn get 仪式签名
（`challenge = b64url(SHA256(JCS(C)))`、UV=1），由 Hub 保管，在兑现在
帧的 `cosign.coupon` 里出示 `C`、其 `authData/clientData/sig` 与本次
nonce。Node 首次见到 `jti` 时验券签名并持久化 `(jti, kid, exp, used=0)`；
之后每次兑现：nonce 单次有效（6.5 缓存）、`used < nMax` 且原子自增、
`now ≤ exp`、本帧 `aud ∈ audiences`，并且**归一化后的 P 必须满足对应
scope 的 match**。

`match` 是唯一允许的约束语法，键路径与操作符在此**穷尽**，实现不接受
未列出的路径：

- 操作符只有 `eq`（JSON 相等，比较前双方过 JCS）、`prefix`（仅字符串）、
  `in`（标量枚举）。
- 允许的键路径（对 6.4 第 1 步归一化后的 P）：
  - gate.run：`/mode`、`/push`、`/pushFrom`、`/branch`、`/repoPath`、
    `/targetDir`。券若包含 gate.run scope，**必须** pin
    `{"/mode":{"eq":"verify"}, "/push":{"eq":false},
    "/pushFrom":{"eq":"home"}}`（即只验不推；形状照 D-034/D-036 里 lane
    主机不持推送凭据的 verify-only 角色）；`prefix` 只可用于 branch/
    repoPath/targetDir，且前缀必须以 `/` 或路径分隔边界结尾，不允许空
    前缀。
  - gate.then：`/command` 与 `/cwd`，只允许 `eq`——操作者签字时看到并
    逐字固定那一条命令与目录；不允许 prefix。
  - instance.create/resume：`/kind`（`in` 白名单）、`/permissionMode`
    （`in`，且只可列 `"manual"`/`"plan"`）、`/args`（必须 `eq []`）、
    `/binaryPath`（必须 `eq null`）、`/capabilities`（必须 `eq []`）、
    `/sandbox`（若出现必须不等于 danger 值，以 `in`
    `["read-only","workspace-write"]` 表达）。
- 其余一切键路径出现在 match 里 = 券无效。

**券永远不能覆盖的特权（v1 硬禁止，mint 与 redeem 两端都拒）**：

- C2 任何 bypass 拼写、C3 `danger-full-access`/`danger`；
- C4 任何非空 capabilities（computer-use 只接受逐次交互式联署，设备在
  create 当时必须在场——与 D-045「桌面控制 + 跳过审批是唯一没有回收路径
  的组合」同级对待）；
- C5 非空 args / 非空 binaryPath；
- C8 `gate.land`，以及 gate.run 的 `mode:"land"` / `push:true` /
  `pushFrom` 非 `home`；
- 通配 audience、嵌套券（券里签券）、超过 24 小时的寿命。

### 6.7 可用性后果与冲突（如实记账）

联署的代价就是「某些操作必须有设备在场」。逐条写清，不粉饰。

**现状标注（2026-09-22 第 1 轮复核补）**：今天的 Remuda **没有「无人
值守」这个概念**——没有任何 spec 标志声明一次运行是 unattended，没有
presence 判定，`launchedBy` 按
[protocol.md](./protocol.md) 实例字段表（第 222 行）只记出身
（`remuda|user`）、**不表示能力等级**，也没有「人在场」位。本节反复
提到的 `unattended-timeout` 是一份**尚未拍板的设计**（所有者未批）。
因此下文第 3–5 条描述的是联署与那份**预期中**设计之间的冲突，不是与
现有产品行为的冲突；今天实际存在的只有「调度触发的派发与自动 gate
队列」这组事实流程，联署对它们的影响按这三条的措辞理解为规划性后果。

1. **Web 上的特权 create**：多一次系统 passkey 确认（UV）。fleet 派发按
   6.4 bundle 只敲一次，不按主机数倍增。
2. **CLI 上的特权 create**：终端本身不承载 WebAuthn。CLI 提交后进入
   「等待联署」态（形状类似今天的 approval 等待），在 Web/已登录设备上出
   一张联署卡，人在手机或平台同步 passkey 设备上确认；CLI 带 TTL（120 s）
   轮询，超时即操作失败、不留下「运行中」行。**纯 SSH headless、身边没有
   任何已登录设备时，特权操作不可用**——这是设计，不是待修 bug。
3. **无人值守 gate verify**：靠 6.6 的 lane 券存活：lane 接入/换班时人签
   一张 audience 钉死 lane 主机、scope 钉死 verify-only、寿命 ≤24h、次数
   有界的券；队列照常自动跑。
4. **无人值守 land（冲突，写明）**：今天 gate 队列可以自动 land 并 push
   主干（D-034；D-036 M1 的 home-host CAS 推送同构）。联署后
   `gate.land`/`push:true` 不发券，**闭环会停在「verified，等待设备签
   署」**，必须有人在 120 s 内完成一次交互式联署才落地。「全自动跑到主干
   更新」与「落地必须设备在场」在 v1 不可兼得；本规格选择后者。若所有者
   以后要恢复无人值守落地，需要一条新的显式决策（比 6.6 更强的审计与
   回滚前提），不是实现时可以顺手放开的开关。
5. **无人值守 bypass / computer-use（冲突，写明）**：任意调度的 yolo
   派发、无人值守桌面控制在 v1 被**移除**：券不可覆盖。需要这类跑批时，
   只能降为 manual/plan 普通 agent（其 Agent 形派发今天就不需要人在场，
   D-036 的 dispatch→watch→gate 闭环不依赖 yolo），或逐次留人在场。
6. **coordinator / bot 自动化**：bot 与 coordinator agent 以 Agent 来源
   在自己 scope 内派发 manual/plan 子实例的既有行为**完全不变**（本来就
   在 6.3 基线内、且本来就不能自铸 yolo，`presets.rs:146-150`）；变化的
   只是「替人类调出特权」这一层——现在必须附上人签的帧或券。
7. **凭据损坏/设备丢失**：访问码 bootstrap（D-018）仍能配进新设备并重新
   注册 passkey（D-030 §2.6 允许删掉最后一把 key），但 bootstrap 只恢复
   「管理通道」，**不能代替任何一次操作联署**；恢复完成前特权面停摆，
   基线派发不停。
8. **Node 重启 / 时钟 / 新主机**：nonce 缓存重启使窗口内重试多敲一次
   （6.5）；参与各方时钟偏差需在 ±60 s 内；新 enrolled Node 在公钥同步
   完成前收不到特权帧（Hub 直接以「Node 不支持联署」拒绝，不发无签
   帧）。内网与回环是不同 RP、passkey 需分别注册的 D-030 限制，原样适用
   于联署仪式。

### 6.8 与既有决策的关系

- **D-035（refuse-never-reroute）**：6.5 的失败形状直接援引；联署验证是
  D-035「拒绝发生在持久接受之前、不静默替换/降级」规则的新适用点，不修
  改 D-035 本身。
- **D-045（computer-use）**：联署在 D-045 两道门（来源 + 主机回报）之外
  加第三道「设备上的人对这一帧的签名」，且与 D-045 的 bypass 互斥拒绝
  一致地把 computer-use 排除在 coupon 之外。D-045 既有条目不改动；
  decisions.md 仅在 D-045 节末追加一段 2026-09-22 附记指向本节。
- **D-017/D-018**：one-shot Interaction 批准是「agent 请人批准提升」的
  既有通道，联署是「人（或人的券）直接给 Node 的帧级授权」；两者并存，
  不互相替代（6.3.2 已列边界）。配对/访问码体系原样作为设备入场前提。
- **c-hubidentity（并行）**：前门（通道认证）与联署（请求意图）独立上线、
  独立失效；任一缺失，特权面都不完整开放。

### 6.9 明确不做（本批与 v1 的边界）

- **不实现**：本节没有对应代码、配置、端点或定时任务；`cosign` 字段不
  存在于 wire；Node 不解析任何联署信封。本批交付物（docs-only）只有本文
  档、一段 decisions 附记、两处 doc comment、一个纯静态测试与证据文档。
- **不改 wire、不改协议版本协商**：上述信封/hello 能力位/公钥同步全部留
  给后续实施批次；实施批次必须先 bump 6.4 的 `v` 协商设计。
- **不改 D-045、D-035 等既有决策条目**（仅允许追加附记/引用）。
- **不做前门**：Hub 身份认证是 c-hubidentity 的范围；本节不依赖其实现
  细节，也不替代它。
- **不把联署扩到会话内动作**（tty 写键、send/respond 等；6.3.2 记录了复
  核条件），不覆盖 passkey 以外的签名器（SSH key、静态 API HMAC、机器身
  份证书等留待以后），不做通配 audience，不做嵌套/长寿命券。
- **不引入任何对外网依赖**：验证只使用 Node 本地公钥副本与标准加密。
