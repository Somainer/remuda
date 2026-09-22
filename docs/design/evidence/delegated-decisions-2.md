# 委托决策留痕 c-deleg2 实跑证据（ADR D-051 第二步）

日期：2026-09-22。分支 `wt/c-deleg2/b-deleg2-md`。前置 c-deleg1 已合入
main（`47b410f4`）。范围：(1) driver broker 提交 actor 按 caller device
的 origin 如实落（不再写死 Human/票主实例）；(2) Hub 在**每次成功 CAS
之后**无条件写一行 `action='interaction.answered'` 审计，`detail_json`
携带完整委托链。无 SQL 迁移、无 wire/schema 文件变更、无新 Rust module
（链递推就地放在 `interactions.rs`）、无 UI。

设计权威：`briefs/delegated-decisions.md`（D-051 已批准）。c-deleg1 的
三条机械排除与开关语义未动；`agent_approvals.rs`、`agent_scope.rs` 未碰。

## 1. actor 真实化

| 位置 | 改动 |
|---|---|
| `crates/remuda-driver/src/interaction.rs:131` | 新增 `AnswerCaller { device_id, origin: LaunchOrigin, instance_id: Option<InstanceId> }`：上游（Node 读 Hub 戳的帧）从认证 device 解析的应答者身份，broker 只如实盖章。 |
| `crates/remuda-driver/src/interaction.rs:356` `answer_for` | CAS 提交时构造的 `ActorRef`：`type` 由 `LaunchOrigin` 1:1 映射（Human→human / Bot→bot / Agent→agent）；`device_id` = 应答设备；`instance_id` = Agent 的**绑定实例**（委托应答时即应答父，而非票主子），Human/Bot 无绑定则仍锚在票主实例（逐字等同旧行为）。 |
| `crates/remuda-driver/src/interaction.rs:334` `answer` | 保留为薄包装：Human origin + 无绑定实例。两个既有调用方不动——Hub 内存一次性审批（`agent_approvals.rs:273`，按门控只可能被人类应答）与 insert 时 auto-allow（`interaction.rs` 本文件）。 |
| `crates/remuda-node/src/interactions.rs:495` `dispatch_rpc` | `interaction.answer` 帧解析：origin 走既有 chokepoint `crates/remuda-node/src/origin.rs:39` `wire_origin`（与 instance.create 等特权帧同一 Hub 戳字段）；`byInstanceId` 显式解析为 `InstanceId`，两者都不读 answer body。映射为 `AnswerCaller` 传入 broker。 |
| `crates/remuda-node/src/runtime.rs:2181` | Node 本地队列排空的 `RespondInteraction`（终端/local API，无 Hub 帧）显式带 `"origin": "human"`，保持旧写死 actor 逐字不变（缺省走 wire_origin 会变 Agent）。 |
| `crates/remuda-hub/src/interactions.rs:585-586` | Hub→Node `interaction.answer` params 新增 `origin`（`json!(origin(device))`，与 `stamp()` 同一序列化）与 `byInstanceId`（Agent 设备的绑定实例）。均从认证 device 解析，caller 无法自证。 |

混版本备注：新 Node + 旧 Hub（帧里缺 origin）时按 Node 既有 fail-closed
约定落 Agent（权限最低方向，`origin.rs` 文档明示）；旧 Node + 新 Hub 时
多余字段被忽略。未新增 wire 字段：`origin` 是所有 Hub→Node 帧的既有信封
字段，`byInstanceId` 仅为该内部 JSON-RPC 的 ad-hoc 参数。

## 2. 审计无条件化

`append_audit` 原本只在 bot-relay 命中时于 `settle_bot_relay` 内写
`interaction.bot-answer`。现在：

| 位置 | 改动 |
|---|---|
| `crates/remuda-hub/src/interactions.rs:800` `append_answered_audit` | 新统一写入点：`action='interaction.answered'`、`subject=interactionId`；`detail_json` = `chain[]` / `answeredByLevel` / `byDevice` / `byInstanceId` / `byOrigin` / `interactionKind`。best-effort（CAS 已裁决，审计失败只 `tracing::error!`，不把已落定应答变失败——与 `settle_bot_relay` 同姿态）。 |
| `crates/remuda-hub/src/interactions.rs:619` | 持久化 Node CAS 成功（`record_interaction_answer` 之后）无条件写一行。 |
| `crates/remuda-hub/src/interactions.rs:542` | Hub 内存一次性审批 broker CAS 成功同样写一行（`interactionKind="approval"`，链起点=被 gate 的 agent 实例；CAS 前从 pending grant 视图取实例 id，该视图在 `list` 内已做 caller 过滤）。仅在无持久化 interaction 行时才 peek 视图，常见持久化路径零额外开销。 |
| `crates/remuda-hub/src/interactions.rs:707` `ChainHop` / `:730` `chain_hops` / `:759` `answered_by_level` / `:772` `delegation_chain` | 链递推就地实现，无新 module。每跳序列化为 `{instanceId, parentInstanceId}`，审计行本身即可重构每跳，不依赖回 join `instances` 表。纯逻辑核心 `chain_hops` 三态查表（行存在有父 / 行存在 NULL 父根 / 行缺失），带环防御；生产 async walker 走既有 `Store::get_instance`，同一边序。 |

### 2.1 链与 level 语义

- `chain[]`：从票主实例 C 起按不可变 `parent_instance_id` 顺序上溯；
  末元素 = 第一个 `parent_instance_id IS NULL` 的祖先。
- **中间父已删除**：上溯在最后一个可达祖先处终止；该跳仍保留指向已删
  实例的悬空边（`parentInstanceId` 照实记录），不伪造不可达的根。
- `answeredByLevel`：Agent 设备 = 其绑定实例在 chain 中的下标（D-051
  一跳门控下只可能 0=self / 1=直接父）；Human/Bot 设备无绑定实例，锚在
  `chain.length-1`（根 operator）。

### 2.2 bot-relay 行叠加保留

`interaction.bot-answer`（`settle_bot_relay`，含 `actingOpenId` /
`ticketId` / `relayedBy` 与票翻转）原样保留；relay 应答现在产生**两行**：
`interaction.bot-answer` + `interaction.answered`，同一 subject。集成测试
`bot_relayed_answer_keeps_bot_answer_row_in_addition_to_answered_row` 钉住。

### 2.3 事后重构

```sql
SELECT action, detail_json FROM audit_log WHERE subject = ? ORDER BY id ASC;
```

同一 interaction 的全部应答事件按 id 升序返回；`chain[].parentInstanceId`
边即每一跳（测试 `audit_rows_for_one_subject_reconstruct_every_hop_in_id_order`）。

## 3. 测试

### 3.1 新文件 `crates/remuda-hub/tests/delegated_decisions_2.rs`（9 个
hub-level 集成测试，全部脚本化：直插 SQLite + 自研 recording Node
transport，无真实 Node）

- `audit_answered_by_level_zero_when_self` — Agent 答 self question：
  level 0；chain=[root, NULL 边]；`byOrigin=agent`、`byInstanceId=root`、
  `byDevice`=该 agent 设备 id、`interactionKind=question`。
- `audit_answered_by_level_one_when_direct_parent` — 根实例凭证答直接子
  question：level 1；chain=[child→root]；byInstanceId=根（应答父，非
  票主子）。
- `audit_answered_by_level_last_when_root_operator` — 三层树 leaf 上的
  question 由人类 operator 答：level 2 = chain.length-1；
  byInstanceId=null、byOrigin=human。
- `plain_human_answer_writes_one_answered_row_and_no_bot_relay_row` —
  恰好一行 `interaction.answered`，无 bot 行。
- `bot_relayed_answer_keeps_bot_answer_row_in_addition_to_answered_row` —
  bot allowlist + 未过期 open card ticket 的 relay 应答：两行并存，票翻
  answered，answered 行 byOrigin=bot。
- `node_rpc_frame_carries_truthful_origin_and_bound_instance` — recording
  transport 实抓 Hub 发出的帧：agent 帧 `origin=agent`、
  `byInstanceId=父`；human 帧 `origin=human`、`byInstanceId=null`。
- `in_memory_one_shot_approval_answer_leaves_an_answered_audit_row` —
  真实 409 HUMAN_APPROVAL_REQUIRED 流程，人类答内存 grant：
  answered 行存在、kind=approval、链起点=被 gate 的 agent、无 bot 行。
- `chain_walk_tolerates_a_deleted_middle_ancestor` — DELETE 中间实例行后
  应答仍成功审计：chain 止于 leaf 且保留指向已删中间父的边。
- `audit_rows_for_one_subject_reconstruct_every_hop_in_id_order` — 直接
  跑重构 SQL，逐跳边断言。

### 3.2 库内单测（`interactions::tests`，新增 9 条）

链纯逻辑：无父 root 单跳、三层上溯到 NULL 根、**中间父已死**（末跳保留
悬空边、不伪造根）、起点行缺失=空链、环防御终止；level：0/1/last
（human 锚根与根实例自身同下标）、单跳链根=0。

driver 库内新增 3 条（`interaction::tests`）：Agent caller 的 actor 落
Agent 且 instance_id 是**应答父**而非票主子；Human/Bot caller 保持票主
锚点（含 device_id 逐字）；legacy `answer()` 包装仍是 Human/票主形状。

### 3.3 test log 摘要（Linux 构建主机，`--locked`）

```
remuda-hub 单测 lib: 181 passed; 0 failed（c-deleg1 时 172，+9）
tests/delegated_decisions_2.rs: 9 passed; 0 failed
tests/delegated_decisions.rs:  10 passed; 0 failed（c-deleg1 套件无回归）
remuda-hub 整包: 全部测试二进制 0 failed（1 个既存 trigger-gated ignored）
remuda-driver lib: 451 passed; 0 failed；整包全部二进制 0 failed
remuda-node tests/interactions.rs: 2 passed; 0 failed（hub→node 应答 e2e）
remuda-node 整包: 34 个测试二进制全部 0 failed（lib 360 passed, 3 ignored）
cargo clippy --locked -p remuda-driver -p remuda-node -p remuda-hub
  --all-targets: 0 warning
```

备注：`remuda-driver --test adapters_parity` 的
`codex_and_grok_adapter_dumps_are_journal_diff_parity` 在干净 target 下会
命中既有 bin-locator fallback（`cargo build -p remuda-testing --bin
remuda`，而 `remuda` 属 remuda 包）——与本改动无关；共享 target 内有
remuda 二进制时该测试通过（已验证 4/4 ok）。

## 4. 未碰清单核对

- `agent_approvals.rs`、`agent_scope.rs`：未改（c-deleg1 文件）。
- protocol/wire 文件：未改（`ActorRef`/`ActorType`/`InputOrigin` 均复用）。
- SQL schema：未改（仅 INSERT 既存 `audit_log`，读既存
  `instances.parent_instance_id`；测试夹具直插既存表）。
- 链递推函数就地位于 `interactions.rs`，未新建 module；新测试支持仅一处
  additive 的 `#[doc(hidden)] test_set_node_transport`（`lib.rs:456`）。

## 5. 备注

- 唯一不审计的边界：interaction 在 Hub 无任何持久化行（纯 live Node
  RPC）时链起点未知；Hub 不臆造，跳过该行（D-051 agent 应答必经
  `authorize_agent_answer` 的持久化读取，故所有委托应答都有行）。
- 审计写入失败不影响已裁决应答，错误进 `tracing::error!`，与
  bot-relay 既有姿态一致。
- 本文件由 c-deleg2 独占创建；后续任务只 append。
