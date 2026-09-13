# Passkey 登录（WebAuthn）

日期：2026-09-14 · 决策：**D-029** · 证据：[evidence/passkey-1.md](./evidence/passkey-1.md)

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
