# Node 侧 device passkey 联署 · 规格要点与现状证据（c-nodecosign）

2026-09-22 · `wt/c-nodecosign/b-nodecosign-md` · 任务 c-nodecosign（docs-only：
只写规格，不改实现、不改 wire）。规格正文在
[../passkey-login.md](../passkey-login.md) §6「Node 侧 device passkey 联署」。

## 1. 攻击链（评审后半段）现状引用

同一次对抗评审把链路分成两半：前半「有人能冒充 Hub」由并行任务
c-hubidentity 负责（通道身份）；本任务负责后半：**一旦 Hub 可被冒充，
Node 没有第二道绑定到人与设备的证据，于是全 fleet 远程代码执行**。

1. Node 今天对 Hub 的认证只在连接建立时：WSS upgrade 的 Bearer host token
   （`crates/remuda-node/src/transport/wss.rs:55-80`，token 来自
   `crates/remuda-node/src/enroll.rs:44-46` 持久化的 `host-token`，0600），
   ssh-stdio 桥逐帧转发 JSON（`crates/remuda-node/src/stdio.rs`）。这是一个
   **连接级对称秘密**，不是逐请求签名；连上之后的每一帧都不再证明「这一帧
   出自一个在场的人」。
2. 帧里的 `origin` 字段在 Node 侧**直接取自 params**，没有任何复核：
   `crates/remuda-node/src/origin.rs:39-43`（c-hubidentity 合入前为
   25-32；其 §7.7 doc comment 在 `:25-38` 描述了同一缺口）——
   `wire_origin` 读 `params["origin"]`，`command_origin` 把 `Human` 映射成
   `CommandOrigin::Ui`（现位于 `:45-51`）。
3. Hub 侧所有特权门都把这个字段当权威。`restrict_permission` 只对
   非 Human 来源补 `manual`、只拒绝 Agent 的非 manual/plan：
   `crates/remuda-hub/src/agent_scope.rs:284`。`stamp` 从已认证设备盖 origin
   （`crates/remuda-hub/src/agent_scope.rs:51-61`）——这个盖戳在「Hub 自己没被
   冒充」时成立，Node 没有任何手段独立验证它。
4. yolo 双门（D-011/D-017）同样只认 origin：
   `crates/remuda-driver/src/presets.rs:151-166` 中
   `merge_yolo_argv` 对 `LaunchOrigin::Human | Bot` 即追加各族 yolo 参数
   （Claude `--dangerously-skip-permissions`，`presets.rs:57`；Codex
   `--dangerously-bypass-approvals-and-sandbox`，`presets.rs:69`；其余两族
   `presets.rs:81,93,105`）。Node 侧 bypass 的三种拼写在
   `crates/remuda-node/src/native.rs:710-716` 被等价接受。
5. D-045 computer-use 能力门只挡 Agent 来源：
   `crates/remuda-driver/src/launch/skills.rs:137-145`；能力数组来自 create
   请求体 `crates/remuda-node/src/model.rs:152-156`，Node host 门在
   `crates/remuda-node/src/computer_use.rs:35-44`。
6. Human 路径还允许调用方自带 `args` / `binaryPath`——即**选择在主机上执行
   哪个可执行文件、带哪些 flag**；Hub 的 `restrict_launch_overrides` 只约束
   Agent（`crates/remuda-hub/src/agent_scope.rs:302-314`），Node 侧字段在
   `crates/remuda-node/src/model.rs:79-91`。
7. `POST /v1/fleet/instances`（路由 `crates/remuda-hub/src/fleet.rs:18`）
   把同一个 create 扇出到全部在线 Node。

把 2–7 连起来：任何能以 Hub 身份发出 Hub→Node 帧的一方，发一帧
`instance.create` 带 `origin:"human"`（再随意叠加 bypass / capabilities /
binaryPath），每个 enrolled Node 都会物料化并启动它——**全 fleet
RCE**。Node 没有第二道签名可以拦。这正是评审结论「Node 联署是任何可达性
方案的硬前置」的所指；它在纯内网同样有价值，因为威胁模型假设的就是内网
里的 Hub 冒用，不依赖任何外部服务。

## 2. 规格要点（详见 passkey-login.md §6）

- **联署面（穷举）**：`instance.create` / `instance.resume` 中任何超出
  「Agent 调用方今天被允许的形状」的特权（origin human/bot、bypass 三种
  拼写、codex `danger-full-access`、非空 capabilities、binaryPath/args），
  以及 `gate.run` / `gate.then` / `gate.land`；`gate.cancel` /
  `gate.unpin` 与全部只读/会话内方法经逐一审阅后排除，清单与理由都在
  §6.3，不用「等等」。
- **canonicalization**：签名只覆盖「按 Node 处理器同款归一化后、从类型化
  结构重新序列化的 method+params」，再包一层带 `aud`（hostId）、单次
  nonce、iat/exp、kid 的挑战文档，统一按 RFC 8785 JCS 规则编码；字段
  顺序、编码、数字规则、唯一允许排除的键（transport-only
  `agentCredential`）在 §6.4 逐条写死，使两个不同请求不可能产生同一份被
  签名内容。
- **验证路径**：三个 carrier 入口（`server.rs` 的 `dispatch_rpc`、
  `runtime_link.rs` 的 dispatch、`transport/wss/runtime_wss.rs`）必须在
  分发进处理器**之前**共用同一个验证器；缺签/坏签/过期/重放/谓词不符
  全部以同一形状 **refuse**，不降级、不剥离特权后继续、不在持久接受之后
  发生——形状照抄 D-035 的拒绝（[../decisions.md](../decisions.md) D-035
  决策 (2)）。
- **无人值守**：以「联署券 coupon」（passkey 预先签名、短时、有界次数、
  谓词精确）承接 gate verify 等既有无人值守流程；**不**为 bypass、
  computer-use、gate land/push 发券——这等于明确移除「任意无人值守
  yolo / 无人值守落地」能力，冲突在 §6.7 写明，不回避。
- **明确不做**：不实现、不改 wire、不改 D-045 既有条目（decisions.md
  只追加附记）、不做前门（c-hubidentity 的范围）、不扩到 tty 等会话内
  动作。见 §6.9。

## 3. 本批改动面

| 文件 | 变化 |
|---|---|
| `docs/design/passkey-login.md` | 新增 §6 规格全文 |
| `docs/design/decisions.md` | D-045 节末尾**追加**一段 2026-09-22 附记；既有条目一字不改 |
| `crates/remuda-node/src/origin.rs` | 仅新增 doc comment（锚在测试模块前，不移动 `:25-32`） |
| `crates/remuda-hub/src/agent_scope.rs` | 仅改写既有 doc comment 行数（不增删行，不移动 `:284`） |
| `scripts/tests/test_topology_doc.py` | 新增纯静态测试：file:line 锚点 + 关键词覆盖 |
| 本文件 | 证据 |

没有任何 `.rs` 可执行行改动；`git diff --stat` 只含 `.md`、`.py` 与两处
doc comment。
