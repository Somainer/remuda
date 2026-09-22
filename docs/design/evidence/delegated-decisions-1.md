# 委托决策路由 c-deleg1 实跑证据（ADR D-051 第一步）

日期：2026-09-22。分支 `wt/c-deleg1/b-deleg1-md`。范围：两处授权放宽 +
中间件两条字面谓词 + `agent_approvals` 内存审批 origin 门 +
`REMUDA_DELEGATED_DECISIONS` 开关（per-project 覆盖全局）。无 SQL 迁移、
无新 Rust module、无 wire/schema 变更、无 UI 变更（不截图）。

设计权威：`briefs/delegated-decisions.md`（D-051 已批准；冲突以 D-051 为准）。

## 1. 改动清单与 file:line

| 位置 | 改动 |
|---|---|
| `crates/remuda-hub/src/interactions.rs:281` `list_interactions` | 首行 `require_operator` 放宽：Agent-origin 走 D-051 判据；pin 到非 owns 实例 403；开关 off 对 Agent 恒 403（与旧行为逐字一致）。 |
| `crates/remuda-hub/src/interactions.rs:140` `delegated_visible_items` | SQL 行 + `agent_approvals.list` 内存视图 + Node live RPC **合并之后**统一过滤：approval kind、子 bypass、父 bypass、非一跳 owns、开关 off 五类全部不落 Agent 页面。 |
| `crates/remuda-hub/src/interactions.rs:420` `answer_interaction` | 旧的入口处 `if origin == Agent { return Forbidden }` 替换为 `authorize_agent_answer`（`interactions.rs:196`）。 |
| `crates/remuda-hub/src/interactions.rs:196` `authorize_agent_answer` | 机械顺序：approval kind→403；`owns()`（`crates/remuda-hub/src/agent_scope.rs:63-71`）→403；开关→403；子 `permissionMode == bypassPermissions`（`store.rs:3551` `get_instance_spec_json`）→403；父 posture 再读一次同一 accessor，bypass→403。未知 id 也返 403（不泄漏存在性）。 |
| `crates/remuda-hub/src/interactions.rs:38-101` | 开关：`REMUDA_DELEGATED_DECISIONS` 全局（truthy=`1/true/on/yes/enabled`，缺省 off）；`REMUDA_DELEGATED_DECISIONS_FORCE_ON` / `_FORCE_OFF` 逗号分隔 project 列表，per-project 优先；同时出现在两表 fail-closed（OFF 胜）。纯解析函数 `resolve_delegated_decisions`（`interactions.rs:66`）带 4 条单测。 |
| `crates/remuda-hub/src/agent_scope.rs:698` | GET 白名单字面谓词 `path == "/v1/interactions"`。 |
| `crates/remuda-hub/src/agent_scope.rs:715` | POST 白名单字面谓词 `path.starts_with("/v1/interactions/") && path.ends_with("/answer")`。无 GET detail 谓词（该端点不存在），不倚赖任何 read/project/task helper 的隐式匹配。 |
| `crates/remuda-hub/src/agent_approvals.rs:234` `answer` | 签名由 `by_device: Id` 改为 `caller: &Device`（`interactions.rs` 唯一调用点同步）；broker 判定前加门：Agent-origin 且 `caller.id != grant.caller.device_id` → `Forbidden`（`agent_approvals.rs:255`）。与 list 侧合并后 kind 过滤共同关闭内存 grant 的 confused-deputy 旁路。 |

第三条安全排除（Hub 自兜的一次性人类审批，`agent_scope.rs:377-408` 路径）由
(a) approval kind 永不路由 + (b) `agent_approvals` broker origin 门共同实现，
无新增机制。

## 2. 测试

新文件 `crates/remuda-hub/tests/delegated_decisions.rs`（571 行，10 个
hub-level 集成测试，全部脚本化：直插 SQLite + scripted Node transport，
无真实 Node）：

- `agent_lists_own_and_direct_child_pending_but_not_sibling_or_excluded` —
  owns 命中（self/直接子 question/elicitation）可见；sibling、子 approval、
  bypass 子不可见；pin sibling → 403；pin child `kind=approval` → 空页。
- `agent_answer_to_owned_and_self_interaction_reaches_node_cas` — 直接子与
  self 的 question answer 到达 Node CAS（scripted Node 回成功帧），
  `interactions.state` 变 `answer-committed`（仍由 Node 裁决 first-answer-wins）。
- `agent_answer_to_unowned_interaction_is_forbidden` — owns 不命中 403，行仍 pending。
- `agent_answer_to_approval_kind_is_forbidden_even_when_owned` — approval kind
  对直接子和 self 一律 403（exclusion (a)）。
- `bypass_posture_child_interaction_is_refused_and_hidden` — 子 bypass：answer 403 + list 过滤（exclusion (b) 前半）。
- `bypass_posture_parent_cannot_answer_or_see_child_pending` — 父 bypass：
  answer 403，且连 manual 子的 pending 也看不到（exclusion (b) 后半，
  防父拿自己的 bypass mandate 替子答）。
- `in_memory_approval_grant_is_filtered_from_agent_page_after_merge` — 真实
  409 HUMAN_APPROVAL_REQUIRED 流程在内存 broker 铸 grant；Agent 合并页
  （unpinned 与 pin self 两种）无 approval 行；operator 页仍可见。
- `agent_interactions_are_operator_only_when_switch_is_off` — off 时
  unpinned/pin child/pin self list 均 403、answer 403、行仍 pending。
- `middleware_admits_only_the_literal_interaction_predicates` — 分层测：
  白名单未命中的形状对 Agent 是**中间件 403**（同一 GET 对 operator 是 SPA
  200，证明该路径无 handler）；白名单命中但 id 畸形 → **handler 400**
  （证明请求穿过了中间件、到达 handler）；off 时命中谓词 → handler 403。
- `operators_still_see_and_pin_every_pending_with_switch_on` — 开关 on 不
  收窄 operator 页（approval/sibling/bypass 行全部仍可见）。

库内单测：`interactions::tests` 4 条开关解析（全局缺省 off、FORCE_ON、
FORCE_OFF、两表同列 fail-closed）；`agent_approvals::tests` 新增
`agent_approvals_answer_refuses_agent_origin_from_non_grant_caller`
（非 grant caller 的 Agent 设备 broker 边界 403，grant 存活；grant 自身
caller 可过此门），既有两条 broker 测试同步到新签名。

### 2.1 test log 摘要

`cargo test --locked -p remuda-hub`（本机，全 38 个测试二进制全绿；
下面只列与本任务相关的，既有套件数字一并摘录证明无回归）：

```
lib (remuda-hub 单元): 172 passed; 0 failed
  interactions::tests::switch_defaults_to_the_global_when_no_project_or_list_matches ... ok
  interactions::tests::force_on_admits_one_project_while_the_global_stays_off ... ok
  interactions::tests::force_off_refuses_one_project_even_when_the_global_is_on ... ok
  interactions::tests::appearing_on_both_lists_fails_closed ... ok
  agent_approvals::tests::agent_approvals_answer_refuses_agent_origin_from_non_grant_caller ... ok
tests/delegated_decisions.rs: 10 passed; 0 failed; finished in 8.98s
tests/cross_host_messaging.rs:   1 passed; 0 failed
tests/durability.rs:            2 passed; 0 failed
tests/lifecycle.rs:             4 passed; 0 failed
tests/hub.rs:                  28 passed; 0 failed
tests/projects.rs:              12 passed; 0 failed
tests/tty_relay.rs:             19 passed; 0 failed
tests/openapi.rs:               4 passed; 0 failed
（其余 30 个二进制全部 0 failed；1 个既存 trigger-gated 测试 ignored。）
```

`cargo clippy --locked -p remuda-hub --all-targets`：0 warning。

## 3. 与「逐字节等同今天」的对应（开关 off）

- list：入口对 Agent-origin 一律 403（pin/unpin 同），等同旧首行
  `require_operator`（旧 `interactions.rs:60-64`）。
- answer：`authorize_agent_answer` 在开关判据上 403，等同旧
  `if origin == Agent { return Forbidden }`（旧 `interactions.rs:158-168`）。
- 中间件两条谓词无条件加入，但 handler 判据恒 false 时结果仍为 403——
  D-051 设计明确允许白名单无条件加。
- operator 分支与 bot-relay 分支未改；既有 operator/bot 测试全绿即回归证据。

## 4. 备注

- 父/子 posture 是同一函数内两次独立 `get_instance_spec_json`
  （`store.rs:3551`）读取，失败均返 `Forbidden`，不静默 route-around。
- Agent answer 成功仍走既有 Node `interaction.answer` RPC 与
  `record_interaction_answer` 镜像，Hub 不引入第二个裁决者。
- 本文件由 c-deleg1 独占创建；c-2 后续只 append。
