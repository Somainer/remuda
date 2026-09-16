# Remuda 三层 Coordinator 与模型供给调度 · 设计

- 日期：2026-09-15
- 状态：设计（待用户拍板后按 §8 派工）
- 基线：`origin/main` = `9df9099`（三份输入报告写于 `3f68061`；本文引用的 `file:line` 已在 `9df9099` 上重新核对）
- 输入：`<coord-scratch>/coordinator/{remuda-surfaces,playbook,prior-art}.md`
- 相关决策：D-002、D-005、D-011、D-013、D-014/D-015、D-017、D-021、D-023、D-024、D-026、D-028、D-031
- 本文不改仓库任何代码；§8 的批次才是可派工单元

---

## 1 目标与非目标

### 1.1 目标

1. **把 2026-09-14/15 手工跑的 coordinator loop 变成产品。** 今天 `<coord-scratch>/*.sh`（spawn / poll / rgate / supervisor）加一个 Claude Code 会话同时兼任 T1 与 T2；目标是同一套判断在 `remuda` 动词上重跑，coordinator agent **永远不写 shell 脚本**。
2. **三层：T1 统领（唯一，对 owner 负责，出口是 bot）→ T2 项目 coordinator（每项目一个，拥有本项目的 workspaces / 主机 / 策略 / 模型画像）→ T3 worker（现有 instance）。**
3. **Model Profiling：capability 由 coordinator 自带知识 + 仓库内置表给出；supply（配额、速率、并发、成本、重置窗口、优先级、workhorse）必须由用户声明**，调度器据此在混合 harness 上求最大有效吞吐。
4. **跨层状态落在 Hub，不落在任何会话的上下文里。** 任一 coordinator 被杀、被换模型、被 resume，都能从 Hub 重建；这是对 prior-art「Clean Slate Problem」的正面回答。
5. **尽量复用既有实体**：`Instance` / `Workspace`（D-023）/ `Host` / `Space`（D-024）/ `ProviderProfile`（D-021）/ `worktree` / `remuda merge --gate`。新实体只加两个半：`Project`、`Task`（含 ownership 与 placement ledger 两张附表）。
6. **第一里程碑（M1）**：人类 coordinator 只用 `remuda` 动词完成一轮 dispatch→watch→gate→land→retire，`<coord-scratch>/*.sh` 全部作废。

### 1.2 非目标

1. **不造 agent loop（D-002）。** T1/T2 仍然是 claude/codex 等 harness 在跑，Remuda 只提供状态、动词与护栏。「coordinator」不是新 `AgentKind`。
2. **本轮不做 Web UI。** `workbench-ux-plan.md` §0.3 明确「不做：独立任务/看板实体、调度引擎」——那条约束限定的是 A–G 这批**前端文件**；本设计把任务与调度放在 Hub + CLI + MCP 层，批次 1–6 只动 `crates/` 与 `skills/`，不碰 `web/src/pages/*`、`web/src/features/*`，因此该约束成立。看板与 Project 设置页留待 ux 批次全部合入后单开。
3. **不做跨主机原生会话迁移**（同 UX plan）。supply fallback = 重新派工（见 §4.2），不是 in-flight 迁移。
4. **不做 HTTP 层代理/计量。** Remuda 是进程主管，不是网关（prior-art §C.1）。token 预算只能是估算 + 准入控制。
5. **不做多用户/成员权限模型。** 边界只有三条：tier、project、instance。
6. **不实现 `remuda-ptyd`/P8、不动 print 退役闸门**——那是 D-028 的轨道。

### 1.3 三份报告的分歧裁决一览

三份输入报告在五处给出不同答案。裁决如下，正文对应小节展开理由。

| # | 议题 | playbook 说 | prior-art 说 | surfaces 说 | 裁决 | 在哪 |
|---|---|---|---|---|---|---|
| 1 | 项目配置的家 | `remuda project init/set`（CLI 为家） | — | Hub `Project` 实体 + 仓库内顾问文件 | **Hub 实体为权威**，`remuda project` 是它的 CLI 外壳；仓库文件只做 brief/gate/worker 规则，永不带权威 | §3.1 |
| 2 | supply 住哪 | 一份独立的 YAML 声明 | 新顶层实体 `SupplyProfile` | 扩 provider model catalog | **位置取 surfaces（扩 `ProviderProfile`），词汇取 prior-art（`RateLimitWindow`）**；并允许**无 secret 的 profile**，让原生登录也有一行 | §4.1 |
| 3 | 计量维度 | `rpm/tpm/quota/rank/fallback` | `windows[]{usedPercent,resetsAt,windowDurationMins}` | — | **观测维度用 window（prior-art 对：Remuda 看不到 HTTP 层）；声明维度保留 rank/concurrency/fallback/coordinator-only（playbook 对：这些确实只有用户知道）**；`rpm/tpm` 降为可选声明 | §4.1–4.2 |
| 4 | 撞限后怎么办 | 在跑的 worker 上活切模型 | 默认 park 到 `resetsAt`；fallback 是迁移不是重试 | — | **默认 park（prior-art）**；活切保留为显式路径，**且只在窗口是 family 级时合法**——账号级窗口跨模型共享，切了也白切 | §4.6 |
| 5 | digest 节奏 | 120 s supervisor 轮询 | 事件驱动，周期 digest 本身是成本 | 现有 `follow_live` 1 s 轮询 | **用户可见层按状态变化发（prior-art）；传输层保留轮询（playbook/现状）** | §5.3 |

另有两处不是分歧但需要点名：`Space` 不升格为控制面实体（surfaces 的基数论证成立，D-024 拒绝跨主机合并），但 `(hostId, workspaceId)` 这个键被 `Project.members[]` 直接复用；`remuda own` 只有 playbook 提出，本设计全盘采纳并把它接到 gate 上。

---

## 2 三层模型

### 2.0 一句话形态

**三层都是 Remuda Instance。** 层级由 instance 上两个新增字段决定：`role ∈ {worker, project-coordinator, top-coordinator}`（缺省 `worker`）与 `projectId`。这是纯增量字段，`InstanceRecord`（`crates/remuda-hub/src/store.rs:287`）已经有 `parent`、`delegation`、`provider_profile_id` 这一类扁平列，加两列与现有 `ensure_column` 迁移同形（`store.rs:3506`）。

**不新建 `AgentKind`**：D-002 说 Remuda 不造 agent loop，coordinator 的「智力」来自它跑的 harness；Remuda 提供的是 (a) 一个 skill（system prompt 层）、(b) 一组被 Hub 按 role 收紧的动词、(c) Hub 里的持久状态。因此 coordinator = `kind: claude`（或 codex/grok）+ `driver: agent-pty` + `role: *-coordinator`。

**座位可由人占。** 一个持有相同 role 的**人类设备**（owner 的 Claude Code 会话）可以直接坐进 T1 或 T2 的座位——这正是今天的运行方式，也是分批迁移的机制：批次 1–4 落地后人类 coordinator 先换用新动词（M1），批次 5–6 之后再把座位交给常驻 agent（M2）。

### 2.1 Tier 1 — 统领 Coordinator

| 维度 | 设计 |
|---|---|
| **身份与持久化** | 每个 Hub **恰好一个**，`role: top-coordinator`，`projectId: null`。它是一个长生命周期 Instance，钉在一台指定 host（默认 owner 的 mac 或 Hub 所在 Node）。存活靠 D-026 `nativeRef` + `POST /v1/instances/{id}/resume`（`crates/remuda-hub/src/instances.rs:21` → `http.rs:674`）；监督者在它退出后**不追加上下文**，而是用 Hub ledger 重新 brief 一遍。其记忆 = Hub 表，不 = 它的 context window。 |
| **持有的状态** | ① 全局 **task ledger**（owner 说过的每件事及其状态）；② 全局 **supply ledger**（跨项目的配额账，§4）；③ **project 名册**与各项目的优先级/预算；④ **escalation 队列**与审计线索。 |
| **工具面** | `remuda task add/list/show/split`、`remuda project list/show`、`remuda profile show/grant`、`remuda report --for-owner`、`remuda report escalate`。**没有** `instance create`、**没有** `merge`/`land`、**没有** worktree 动词、**没有** 任何写仓库的工具（prior-art：CrewAI「tools belong to agents, not the manager」；Claude Code teams 的 "lead starts implementing tasks itself" 是已记录的失败模式）。 |
| **LLM 判断** | 这是任务还是策略变更还是对 escalation 的回答；跨项目抢配额时谁先；什么值得打扰 owner；owner 的一句中文意图落到哪个项目。 |
| **产品强制** | 不可 spawn/merge/push（Hub 按 role 403）；每条 owner 可见消息必须挂一条 ledger 行（可审计）；digest 只在状态变化时发（§5.3）。 |

### 2.2 Tier 2 — 项目 Coordinator

| 维度 | 设计 |
|---|---|
| **身份与持久化** | 每个 `Project` 一个，`role: project-coordinator`，`projectId` 必填，钉在 `project.homeHost`。同样是长生命周期 Instance + resume；同样「状态在 Hub」。一个项目同时只允许一个活跃 T2（Hub 唯一性约束），避免两个 coordinator 派同一个依赖（playbook §3.2 的「非任务表」问题）。 |
| **持有的状态** | ① 本项目 `Project` 配置（§3）；② **task ledger** 的项目切片 + 依赖边；③ **ownership map**（path → 持有它的 task，`remuda own`）；④ **worker roster**（name/instance/host/harness/model/branch/port block/target dir/durable session id——即今天的 `relay-workers.tsv`）；⑤ **gate queue**（lane 占用、land 锁、待验证分支序）；⑥ **placement ledger**（每次 dispatch 的 `reasons[]`/`rejected[]`，既是审计又是 bot 卡片）。 |
| **工具面** | `project show/set`、`task`、`own`、`brief render/lint/send`、`dispatch`、`watch`、`worker (nudge/answer/keys/switch-model/resume/replace/stop)`、`gate`、`land`、`retire`、`report`、`hostcap`、`profile probe/pick`，以及既有 `worktree`、`instance *`、`fleet`、`merge --gate`、`doctor`、`agents`。 |
| **LLM 判断**（playbook §5.3，全部保留） | 所有权划分与冻结清单；brief 的实质（measured problem、带行号的 read-first、可证伪的验收）；串行 vs 并行的合同排序；gate 处的 **diff review**（唯一没有 gate 能抓的拒绝理由，`docs/design/coordinator-guide.md`）；失败归因（worker / coordinator / flake / 环境）；卡住的 worker 该 nudge 还是 replace；部分信息下的 supply 取舍；BLOCKED 是否成立。 |
| **产品强制** | 不得跨项目（Hub 403）；不得改 owner 设的 policy（D-031 一类字段只读，改需 escalation）；不得越过 gate 落地；不得花别的项目被授予的 supply；port block / target dir / worktree 由产品分配而非自报。 |

### 2.3 Tier 3 — Worker

维持现状（D-014/D-015 已产品化，surfaces §1.3 全 OK），只加两件：

- `role: worker`（缺省）与可选 `taskId` 绑定，使 `own check` 能在 gate 时把 diff 与 task 的 `owns[]` 对账。
- **worker 的上行只有一行**：`DONE <sha>` / `BLOCKED <reason>`，由 `remuda instance wait --until 'line:(?m)^DONE'`（`crates/remuda/src/cmd/instance.rs:102-117`，含 bullet 容错与 brief 回显抑制 `:718-740`）捕获。prior-art 的 LangGraph `output_mode` 结论：T2↔T3 这条边**永远** `last_message`，PTY transcript 绝不上行。

### 2.4 层间安全边界

| 边界 | 机制 | 落点 |
|---|---|---|
| T2 不能碰别的项目 | `caller()` 解出 device → instance 绑定 → `role` + `projectId`；对 `/v1/projects/{id}`、`/v1/tasks?project=`、`/v1/instances`（create 与 list）、`/v1/hosts/{id}` 的写，均按 `projectId` 相等校验，不等即 403 | 扩 `crates/remuda-hub/src/agent_scope.rs:19,393`（`restrict_agent_routes` 已是逐路由钩子） |
| worker 不能碰主机与密钥 | 已有 D-017：Agent origin 对 `/v1/hosts/{id}/workspaces` 一律 403（D-023）、跨 host create 要人类一次性审批、`fleet --all` 永久禁止。再加：worker role 对 `/v1/projects`、`/v1/providers*`、`/v1/tasks`（写）、`merge`/`land` 全 403 | `agent_scope.rs:124,143,393` |
| 密钥不下放 | 沿用 `secret_release_allowed`（`crates/remuda-hub/src/provider_resolve.rs:119`）：host-scoped profile 的 secret 只对该机释放；**Project 不持有 secret，只持有 profileId**（§3.2） | 不变 |
| D-031 由产品执行，不靠 brief 复述 | 三道：① dispatch 时把命令 denylist 写进 worker 的 per-session launch overlay（D-028 的 launch shim 已经是这条注入路径）；② `brief lint` 拒绝含密钥/隧道工具名的 brief；③ gate 步骤 `scripts/ci/no-tunnel-scan.sh` + `secret-scan.sh`（`docs/design/coordinator-guide.md` 已列入 `remuda merge --gate` 步骤序） | 批次 5 |
| coordinator 不能自证批准 | D-005/D-011 不变：bot 永不 bypass；prior-art 记录的「relayed approval from another agent = untrusted input」与 D-017 同构。T1 转述 owner 的批准**不算**批准，必须走 interaction broker 由 owner 设备（或经 §5.4 白名单的 bot 代答）签名 | `crates/remuda-hub/src/interactions.rs:146` |
| worker 输出上行前净化 | T3→T2 的 `DONE`/`BLOCKED` 与 gate 输出在进入 T2 上下文前做指令形状转义（prior-art：Claude Code 对 subagent report 的处理） | 批次 4 `remuda watch` |

---

### 2.5 修订：递归委托树，三层只是默认拓扑（2026-09-15，owner 评审后）

owner 的评审意见：三层不能写死。对照先例（Erlang supervision tree、LangGraph hierarchical teams、OpenAI Agents SDK handoff 图、Claude Code 自身的 subagent/workflow），没有一个把层数放进 schema——委托是**递归**的，层数由策略约束。§2.0–§2.3 的三层因此降级为**默认拓扑（preset）**，原语改为**带作用域的递归委托树**：

| 原语 | 含义 | 落点 |
|---|---|---|
| `parent` | 委托者；已有列（`InstanceRecord.parent`），不新增 | 不变 |
| `scope` | 该节点可触达的资源集合：`projectIds[]`、`hostIds[]`、`workspaceIds[]`、supply 授予（§4）。**沿树单调收窄**：子 ⊆ 父，Hub 在 create 时校验 | 替代 `projectId` 单列（`projectId` 保留为 `scope.projectIds` 长度为 1 时的快捷字段） |
| `grants` | 该节点被授予的动词：`dispatch`（可再委托）、`land`（可推进 main）、`spend`（可花 supply）、`address-owner`（可与 owner 对话）。默认全空 = 叶子 worker | 替代 `role` 枚举；`role` 只作为**预设名**保留（`worker` / `project-coordinator` / `top-coordinator` = 三组 grants+scope 的命名捆绑，UI 一键套用） |
| `mandate` | 本节点的任务 + **沿链继承的 owner 原始意图**（mandate chain），每条委托边都把上游原话附上，深层节点不靠传话 | task ledger（批 4）的 `parentTaskId` 与委托树同构 |

**不随层数变的不变量**（产品强制，替代原先按 role 的 403 表）：① 作用域沿树单调收窄；② supply 授予沿树下发并逐层记账，子节点不能花父节点没授予的额度；③ 上行只传结构化摘要（每条边 last-message，永不上传 transcript）；④ 落地按项目由 gate 串行，与树形无关；⑤ 深度上限（默认 3）、扇出上限、预算上限是 **策略字段**，写在 Project / Hub 策略里，可改；⑥ 不能向祖先的作用域之外委托，不能形成环（DAG 校验）；⑦ 任一节点都可由人占座。

**由此自然得到的拓扑**：单项目用户 = 2 层（owner 直接对持有 dispatch 的项目节点说话，无人持有 address-owner 也可以——bot 直连该节点）；标准 = 3 层；worker 派 spike = 4 层（受深度上限约束，spike 节点无 `land`）；跨项目发布 coordinator = scope 覆盖多个 project 而无 address-owner 的节点；review agent、gate 失败交回原 worker = 同级 handoff（消息，不是树边）。

**对批次的影响**：批 1（co-project）实现 `scope`/`grants` 两列与预设，唯一性约束改为「每个 Hub 至多一个活跃的 `address-owner` 持有者」+「每个项目默认至多一个活跃 `dispatch` 持有者（策略可放宽）」；批 4 的 task ledger 带 `parentTaskId`；批 5 的 `dispatch` 动词在创建子节点时校验收窄与 DAG。§2.1–§2.4 中所有「T1/T2 不得…」的 403 规则一律读作「不在 scope/grants 内的动作」。

---

## 3 项目配置

### 3.1 住在哪里：两层，一层权威一层顾问

**权威层 = Hub `Project` 实体。** 理由（采纳 surfaces §3.2c，否决 §3.2a/b/e）：

- **否决「把 Space 升成控制面实体」**：Space 定义就是 `(hostId, workspaceId)`（`web/src/features/spaces/store.ts:44` `spaceKey`），D-024 明确拒绝跨主机合并同名目录；而一个项目今天正好横跨 mac 与 `<remote-host>` 两台机。基数不对。
- **否决「扩 Workspace」**：protocol §2.2 把 `hostId` 钉死在 Workspace 上，项目字段会在每台机的 workspace 行里复制且无真理归属。
- **否决「`remuda.toml` 的 `[project.*]` 块」**：改一次要重启 Hub，Web/bot 拿不到，也没有审计。可作第一个项目的 bootstrap，不是归宿。
- **采纳 `Project`**：基数对（1:N workspaces 跨 host）；它要的每个字段都已是现成外键（hostId、workspaceId、providerProfileId、labels）；bot 侧已经为它留好类型（`web/src/features/bots/channels.ts:24` 的 `defaultProject` 是全仓唯一出现「project」作为路由概念的地方）；而且有一个**有先例的注入点**——`merge_host_launch_defaults`（`crates/remuda-hub/src/http.rs:516`）已经在 create 热路径上折叠 host 默认值，旁边加 `merge_project_defaults` 是一个函数的改动。

**顾问层 = 仓库内 `.remuda/project.toml`**（采纳 surfaces §3.2d）：brief 模板、完成行字面量、build env、per-agent `CARGO_TARGET_DIR` 模式、gate 命令、worker 规则文本、ownership 提示。跟着代码一起过 review，且与 `scripts/ci/gate.sh` 同构——**gate 脚本是从合并后的临时 worktree 里读的**（`docs/design/remuda-cli.md:134-137`），所以「改 gate 的分支用它自己那版 gate 验」这条性质天然适用。

**它永远不带权威**：D-017 说 agent instance 不是受信 principal，而 worker 能写仓库。因此 host 白名单、provider 绑定、权限姿态、密钥、并发上限一律只在 Hub。CLI 读到 repo 文件里出现权威字段时**警告并忽略**。

**分歧裁决（§1.3 分歧 1）**：playbook §5.1 把 `remuda project init/set` 当作配置的家，surfaces 把家放在 Hub 实体。**Hub 赢**（权威与审计），`remuda project` 是它的 CLI 外壳。二者不真冲突，这里明确记为一个东西的两面。

### 3.2 `Project` schema（Hub，thin）

```jsonc
{
  "projectId": "prj_…",
  "name": "remuda",
  "homeHost": "hst_mac",                  // T2 座位所在
  "repoRemote": "git@…/hybrid-harness",   // 可选，只用于校验
  "defaultBaseBranch": "main",
  "branchPattern": "wt/{worker}/{topic}",

  // ——— workspaces：复用 D-023 注册表，Project 只引用 ———
  "members": [
    { "hostId": "hst_mac", "workspaceId": "wsp_a", "role": "primary" },
    { "hostId": "hst_sg",  "workspaceId": "wsp_b", "role": "build"   }
  ],

  // ——— remotes with capacity：Host 记录的项目侧配额视图 ———
  "hosts": [
    { "hostId": "hst_sg",
      "maxInstances": 8,            // 不超过 HostRecord.max_instances（store.rs:205）
      "maxBuilding": 4,             // 今天口口相传的「远端同时 ≤4 个在编」
      "diskBudgetGb": 120,          // /tmp 预算；retire 必须回收
      "portBlocks": ["58400-58999"],
      "requires": ["toolchain=rust", "browser=playwright-ws"],   // 复用 labels（placement.rs:234）
      "latencyClass": "remote" }
  ],

  // ——— placement 默认 ———
  "placement": { "default": "auto", "labels": ["project=remuda"], "hostIds": [] },

  // ——— provider / supply 引用（不含 secret） ———
  "provider": { "delegation": "gateway", "profileId": "prv_relay" },
  "modelRoles": {                      // 项目只认角色，不认具体 model id（§4）
    "workhorse": "workhorse", "frontier": "frontier",
    "reviewer": "frontier", "cheap": "cheap"
  },
  "defaultEffort": "high",
  "permissionPosture": "ask",          // ask | accept-edits | bypass（D-011；Agent 发起时强制非 yolo）

  // ——— gate：消费 r-mergequeue 的 --onto/--lanes ———
  "gate": {
    "command": "scripts/ci/gate.sh",   // 权威在合并后的 worktree 里（remuda-cli.md:134-137）
    "affected": true, "web": "auto",
    "lanes": [
      { "id": "lane1", "hostId": "hst_sg", "repoPath": "<remote-agents>/repo",
        "targetDir": "<remote-agents>/target-gate",  "ports": "58980-58989", "remote": "sg" },
      { "id": "lane2", "hostId": "hst_sg", "repoPath": "<remote-agents>/repo2",
        "targetDir": "<remote-agents>/target-gate2", "ports": "58970-58979", "remote": "sg2" }
    ],
    "landSerialization": "global-cas",  // 验证可并行，落地永远串行（playbook I4）
    "mandatorySteps": ["secret-scan", "no-tunnel-scan", "gen-api-current", "verify-tree"]
  },

  // ——— policies：enforced 与 configurable 分开存，enforced 只有 owner 能改 ———
  "policy": {
    "enforced": { "noDeployScripts": true, "noTunnelTools": true, "workersNeverPush": true,
                  "gateBeforeLand": true, "casAncestorCheck": true, "oneWorktreePerWorker": true,
                  "allocatedPortBlocks": true, "reclaimDiskOnRetire": true,
                  "noOsSettingsChanges": true, "secretsNeverInBriefs": true },
    "configurable": { "maxConcurrentWorkers": 8, "nudgeThrottleMins": 15, "stallThresholdMins": 20,
                      "maxFanOutPerTask": 6, "completionLine": "DONE <sha>",
                      "defaultPlacement": "remote-when-build-heavy" }
  },
  "briefRef": ".remuda/brief.md"
}
```

**Reuse 说明**：`members[]` 的元组 `(hostId, workspaceId)` **就是** Space 的主键（`store.ts:44`）。因此 Space 保持 D-024 原样（本机 localStorage 的展示分组），只是从此每个 Space 能回答「我属于哪个 Project」。零新前端概念，也不违反 D-024 拒绝跨主机合并的规定——跨主机的是 Project，不是 Space。

### 3.3 `.remuda/project.toml`（顾问层，节选）

```toml
[brief]
skeleton = "identity, worktree/branch, rules, read-first, measured-problem, ownership, do-not-touch, task, acceptance, checks, merge-main-last, protocol"
completion = "DONE <sha>"
merge_main_last = true
evidence_dir = "docs/design/evidence"

[env]
CARGO_TARGET_DIR = "<remote-agents>/target-{worker}"
CARGO_INCREMENTAL = "0"
CARGO_BUILD_JOBS  = "16"

[worker_rules]
text = """不得执行 deploy/ 下脚本或探测隧道工具（D-031）。禁止裸 pkill herdr。
不得 push，不得写 worktree 之外的文件。"""

[gate]
command = "./scripts/ci/gate.sh"
```

### 3.4 T2 如何决定 remote vs local

调用 `remuda dispatch` 时 T2 给出 TaskSpec（§4.3），Hub 侧按固定顺序求解，**判断留给 LLM 的只有 task class 与 latency/cost 敏感度的赋值**：

1. **硬需求过滤**：任务需要的 `requires` 标签（toolchain / browser endpoint / os）必须在 host labels 上（`placement.rs:234` 已有精确匹配，`:`→`=` 归一）；该 host 必须有本项目的 member workspace；host-scoped supply（D-021）必须匹配。
   - 这一步就是「Ubuntu 20.04 装不上 Chromium」在 **dispatch 时**被抓住，而不是 40 分钟后在 worker 里炸。
2. **容量排除**：`online`（`placement.rs:180-194`）、`running < maxInstances`（`:200-214`）、`building < project.hosts[].maxBuilding`、`/tmp` 剩余 < `diskBudgetGb`、port block 无空闲、`resources.cpu_pct`/`mem_pct` 越线。
   - `HostRecord.resources`（`store.rs:203`，由 `crates/remuda-node/src/inventory.rs:723` 采集）**今天存了但 `select_hosts` 从不读**——这是最便宜的一项改进，批次 3 把它接进 `load_of`（`placement.rs:162`）与 `consider`。
3. **倾向规则**（可配置，`policy.configurable.defaultPlacement`）：
   - 偏 **remote**：build-heavy（`cargo test --workspace`）、长跑、`latencySensitivity: low`、远端空闲核多。
   - 偏 **local**：任务要**探测本机安装的那个 CLI 二进制**（evidence spike 必须跑在装着 codex/grok/claude 的那台）、需要 macOS、需要 owner 的 live demo、`latencySensitivity: high`、或该 workspace 的 prompt cache 刚在本机热过（affinity）。
4. **排序**：§4.4 的 rank 函数（supply 优先级 → 窗口余量 → affinity → cost → 现有 `load_of`）。
5. **背压**：都不满足不静默降级——任务进 `deferred`，带机器可读的 `reasons[]`/`rejected[]`，bot 如实说「在等什么」。今天 `HubError::Unsatisfiable`（`placement.rs:147-152`）是直接拒绝，caller 手工重试；`deferred` 队列取代它。

---

## 4 Model Profiling

### 4.1 三份数据，三个来源

| 分类 | 谁产生 | 住哪 | 可否被推翻 |
|---|---|---|---|
| **Capability** | coordinator 的通识 + 仓库内置表 | 新文件 `crates/remuda-hub/src/model_catalog.rs`，沿用 `crates/remuda-driver/src/usage/prices.rs:14` 的 `REVISION` + `Published\|Provisional` 纪律 | 可被用户 per-profile 覆盖，也可被实测推翻 |
| **Supply（声明）** | **用户必须声明** | `ProviderProfile.supply`（扩现有实体） | 只有用户能改 |
| **Supply（观测）** | harness 上报 / 探针 / 屏幕文本 | 同一对象的 `windows[].source = observed`，与 declared 分开存 | 观测**不清除**已观测值（照抄 Codex 契约） |

**分歧裁决（§1.3 分歧 2）— supply 放哪。** prior-art §D.1 提议新建顶层 `SupplyProfile`；surfaces §3.2f 提议扩 provider model catalog。**surfaces 的位置赢，prior-art 的词汇赢。** 理由：`ProviderProfile` 已经是「一个账号」的实体，已经有 `scope: universal | host:<id>`（D-021）、已经有密钥释放守卫（`provider_resolve.rs:119`）、已经有会迁移的可选字段数组 catalog（`crates/remuda-hub/src/provider_models.rs:22`）。再造一个 `SupplyProfile` 会把 scoping 与密钥规则复制一遍。**补一条**：允许**不带 secret 的 profile**——原生登录（`delegation: none` / host `providerBinding: native`）也要有一行，否则 mac 上的 Claude 订阅与远端自带网关都无处声明配额。profile 从此是「Remuda 知道的账号」，密钥可选。

**分歧裁决（§1.3 分歧 3）— 计量维度。** playbook §4.1 用 `rpm/tpm/quota/rank/fallback`；prior-art §D.1 用 `windows[]{usedPercent, resetsAt, windowDurationMins}` 并论证 rpm/tpm 不可观测（§C.1：Remuda 看不到 HTTP 层）。**观测维度用 prior-art 的 window；声明维度保留 playbook 的 rank/concurrency/fallback/coordinator-only。** `rpm`/`tpm` 保留为**可选声明字段**，只在 API-key 型供给上有意义，且只用于准入提示，Remuda 不宣称能精确计量。

### 4.2 Schema

**Capability（内置，coordinator 知识）**

```jsonc
{ "id": "claude-opus-5", "family": "opus",        // family = 限流桶，≠ id
  "aliases": ["opus", "claude-opus-5[1m]"],
  "class": "frontier",                            // frontier | workhorse | cheap
  "contextWindow": 1048576, "maxOutputTokens": 64000,
  "effortLevels": ["low","medium","high","xhigh","max"],   // 对齐 web/src/features/session/effort.ts:44-84
  "supports": { "toolChoiceAny": true, "structuredOutput": true, "vision": true,
                "promptCaching": true, "assistantPrefill": false },
  "suitedFor": ["design-heavy","rebase","review"],
  "price": { "usdPerMTokIn": …, "cacheWrite1h": …, "revision": 1, "status": "Provisional" } }
```

`family` 与 `id` 必须分开：限流按 family 分桶，而 `es1_orange_o50` 与 `<gateway-model-B>` 在同一网关下是两个桶——这正是 09-14 事故的形状。

**Supply（用户声明 + 观测合并），挂在 `ProviderProfile` 上**

```jsonc
{ "profileId": "prv_relay", "scope": "host:hst_mac",
  "account": { "label": "relay personal", "auth": "gateway", "planType": "unknown" },
  "supply": {
    "priority": 20,                     // 用户偏好序：grok > codex > claude-relay > agy（会被改，三天改了三次）
    "reserve": "none",                  // none | coordinator-only（Fable 只做统领，见 subagent-model-policy）
    "concurrency": { "max": 4 },        // 跨 host 的账号级上限 —— 今天完全缺失的那个原语
    "declared": { "rpm": null, "tpm": null,
                  "dailyUsd": 0, "weeklyUsd": 0,
                  "resetWindowMins": 300, "resetsAtHint": null },
    "windows": [                        // ← Codex RateLimitWindow，逐字照抄
      { "id": "primary",  "appliesTo": ["*"],     "windowDurationMins": 300,
        "usedPercent": 41, "resetsAt": 1757900000, "source": "observed", "observedAt": … },
      { "id": "weekly",   "appliesTo": ["*"],     "windowDurationMins": 10080,
        "usedPercent": 62, "resetsAt": null,      "source": "declared" }
    ],
    "credits": { "hasCredits": true, "unlimited": false },
    "spendControl": { "limit": "50.00", "used": "12.40", "remainingPercent": 75 },
    "ordinaryUsageAllowed": true,       // null=未知；禁止从百分比或重置时间推断恢复
    "state": "available",               // available | degraded | cooling | exhausted | unknown
    "cooldownUntil": null,
    "lastError": null
  },
  "models": [                            // 扩 provider_models.rs:22 的 ProviderModel（全部可选字段）
    { "id": "<gateway-model-A>[1m]", "enabled": true, "contextWindow": 1048576,
      "family": "es1", "role": "workhorse", "priority": 20,
      "concurrencyMax": 4, "fallback": ["<gateway-model-B>[1m]"],
      "windows": [ { "id": "model", "appliesTo": ["es1"], "usedPercent": 100,
                     "resetsAt": null, "source": "observed" } ] },
    { "id": "<gateway-model-B>[1m]", "family": "seed", "role": "workhorse", "priority": 18 },
    { "id": "claude-opus-5[1m]", "family": "opus", "role": "frontier", "priority": 15 }
  ] }
```

关键点：`appliesTo` 存在，是因为**账号级窗口切模型逃不掉、family 级窗口切模型能逃掉**（prior-art §B.2 的 Anthropic 文档原文）。没有这一位，调度器会一头撞墙地重试。

**观测填充的三个来源，质量递减**（prior-art §C.2）：
1. **observed-structured** — Codex `account/rateLimits/updated`（`crates/remuda-codex-wire/src/notification.rs:106` 已解出变体，**全仓零消费者**）；Claude `rate_limit_event`（`crates/remuda-claude-wire/src/types.rs:369`，`rate_limit_info` 是 `Value`，在 `crates/remuda-driver/src/claude_print.rs:764` 被丢成一条 diagnostic）。仓库里已经 vendored 了 Codex 的 schema：`docs/research/cli-help/codex-app-server-schema/v2/GetAccountRateLimitsResponse.json`。**Remuda 不发明供给原语，直接采用 Codex 的。**
2. **observed-textual** — 屏幕/journal 上的固定串（`You've hit your session limit` / `weekly limit` / `Opus limit` / `Request rejected (429)` / `Budget limit reached`），匹配方式与 `wait --until 'line:'` 同一套。
3. **declared** — 用户 YAML/CLI，唯一能覆盖「还没开口的供给」的来源。

### 4.3 TaskSpec（dispatch 时提交的东西）

采纳 prior-art §D.3 的裁剪版；四个 brief 字段逐字照抄 Anthropic 的 task description：

```jsonc
{ "taskId": "tsk_…", "projectId": "prj_…", "parent": "tsk_…",
  "objective": "…", "outputFormat": "最后一行 DONE <sha>", 
  "guidance": { "tools": ["cargo","git"], "sources": ["docs/design/usage-adapter.md:3"] },
  "boundaries": { "owns": ["crates/…/**"], "mustNotTouch": ["deploy/**"] },
  "class": "implement",                   // research|implement|review|test|merge-gate|triage|docs
  "minClass": "workhorse", "effort": "high",
  "contextNeed": { "expectedInputTokens": 250000, "needsLongContext": false, "repoScope": "crate" },
  "costSensitivity": "normal", "latencySensitivity": "low",
  "requires": ["toolchain=rust"],
  "budget": { "maxUsd": 5.0, "maxTurns": 40, "maxWallMins": 45 },
  "isolation": { "worktree": "wt/co/usage-tail", "targetDir": "auto", "ports": "auto" },
  "host": "auto", "pin": null,            // pin={harness,model,supplyId} ⇒ 关闭自动选择并如实上报
  "gates": ["secret-scan","no-tunnel-scan","fmt","check","clippy","test","gen-api-current","verify-tree"],
  "scopeCheck": { "diffMustStayWithin": "owns" },
  "escalation": { "stallThreshold": 2, "onGateFail": "return-to-worker",
                  "onRateLimit": "park-until-reset", "onScopeDrift": "ask-owner" } }
```

`scopeCheck.diffMustStayWithin: owns` 就是 `remuda own check`：把 `git diff --stat main...<branch>` 与 ownership map 对账。playbook 称之为「单条最高价值的新原语」，本设计同意——它把「没有 gate 能抓的拒绝理由」变成一条可执行检查（但 diff 的**语义** review 仍是 LLM 判断）。

### 4.4 调度策略

**准入控制重、故障转移轻**（prior-art §C.1 的核心论证：Remuda 的 fallback 是**迁移**——重新装载上下文——不是重试；所以智力要花在 spawn 之前）。

1. **能力过滤**：丢掉 `class < minClass`、缺 `supports.*`、`contextWindow < contextNeed` 的 model。
2. **供给过滤**：丢掉 `state ∈ {cooling, exhausted}`；丢掉适用窗口余量不够跑完 `maxWallMins` 的；丢掉 `resetsAt` 落在预计工期中间的；丢掉 host scope 不符（D-021）；丢掉 `reserve: coordinator-only` 的（除非 caller 自己是 coordinator）；丢掉 `concurrency.inFlight >= max` 的——**这条是今天完全没有的账号级天花板**：`maxInstances` 只管 host（`placement.rs:200`），而真正会饱和的是 provider 账号。
3. **主机过滤**：§3.4 的 1–2 步。
4. **排序**：`priority`（用户声明序**就是**目标函数）→ 剩余声明份额 `1 - max(usedPercent)` → cooldown 新鲜度 → warm-cache affinity（同 workspace 刚跑过的 host；订阅 1h / credits 5m 的 cache TTL 决定这条的价值）→ `costSensitivity: high` 时按 cost → 最后才是现有的 `load_of`（`placement.rs:162`）。
   **明确否决** OpenRouter 的价格反平方加权：订阅制供给在耗尽前边际价格为 0，耗尽后等于无穷，价格加权是错的形状。
5. **预留**：把估算额度从 task budget 与 supply 窗口双双扣掉，写一行 `placement` ledger（含 `reasons[]` / `rejected[]`）。这行同时是审计记录与 bot 的「已派发」卡片。
6. **Fallback chain**：`models[].fallback` 是**有序**的候选，不是自动重试。切换 = 一次新的 §4.4 求解（新 worker / clean slate），只有在 task 还很早、或窗口是 family 级且兄弟模型可用时才走（见 §4.6）。
7. **Cooldown 与退避**：冷却单位是 **(supplyId, windowId)**，不是 endpoint。已知 `resetsAt` 就冷到那一刻；未知就指数退避 60s → 2m → 5m → 15m 封顶。**529/overloaded 一类绝不冷却**——那是对方机队过载，不是你的配额，冷错了会白白烧掉另一份供给。
8. **背压而非静默降级**：无候选 → task 进 `deferred(reason)`，bot 如实播报；除非 TaskSpec 显式写了 `allowDowngrade`，否则**绝不**偷偷换成更弱的模型。
9. **Sticky session**：已 running 的 task 钉死在它的 `(host, supply, model)`；改变它是一次显式决定（park / switch-model / replace），有 ledger 行。
10. **公平**：共享供给按 `project.share` 加权轮转；`reserve: coordinator-only` 的份额 worker 永远拿不到。
11. **硬上限写在代码里，不写在 prompt 里**（prior-art：Anthropic 的「spawning 50 subagents」是 prompt 启发式失效的实例）：`maxConcurrentWorkers`（项目）、`concurrency.max`（供给）、`maxFanOutPerTask`、嵌套深度 3、`maxWallMins`、`maxUsd`。

### 4.5 反馈回路

```
worker（三家 harness 的结构化文件 tail）
   └─> UsageAdapter（crates/remuda-driver/src/usage/，已建已测，未接任何 driver tail —— usage-adapter.md:3）
        └─> ObservationPayload::Usage（crates/remuda-protocol/src/observation.rs:576）
             └─> Hub usage_events 表（新）  ← 今天 grep `usage` 在 crates/remuda-hub/ 零命中
                  ├─> 按 (project, supply, family, window) 聚合 → 回填 windows[].source="inferred"
                  ├─> 按 (task class, harness, model) 聚合 → Devin 式 XS/S/M/L/XL 尺寸桶
                  │     → 修正下次 dispatch 的 expectedInputTokens / effort / 是否该再切分
                  └─> 与 task budget 对账（估算 ±10%，硬 cap 必须带容差：warn 于估算，stop 于估算×1.15）
```

金额一律标「估算」（`usage-adapter.md` 既有要求；Haiku 估算低 7%，Opus-5/Sonnet-5 行为 `Provisional`，实测有一帧偏 1.79×）。

**与在飞工作的关系**：`r-p6-adapters` 正在接三家 driver 的 usage tail。本设计**不碰** `crates/remuda-driver/src/usage/*` 与 adapter 文件，只在 Hub 侧建消费端（批次 3），并在 r-p6-adapters 合入后自动有数据。

### 4.6 09-14 的 es1 429 事故，自动化后长什么样

事实（playbook §3.11）：`<gateway-model-A>` 对每个请求回 HTTP 429 `{"error_code":-2001}`；**同一网关上的 `<gateway-model-B>` 一直好使**；人工处置是对每个在跑 worker 敲 `esc esc` → `/model <gateway-model-B>[1m]` → `enter`（确认「conversation is cached for the current model」）→「continue where you left off」。

| 阶段 | 自动化行为 |
|---|---|
| **Detect** | 三路任一命中即置位：① worker journal 的 `rate_limit_event` diagnostic（今天到 `claude_print.rs:764` 为止就没了）；② 屏幕文本匹配器命中 `Request rejected (429)`；③ `remuda profile probe` 的 1-token 探针（今天是 `gateway-ok.py`）返回 429。Hub 将 `models[es1].windows[model]` 置 `usedPercent=100, source=observed, appliesTo=["es1"]`，supply `state=degraded`，`cooldownUntil = resetsAt ?? now+60s`（指数退避）。**因为窗口是 family 级而非 `appliesTo:["*"]`，`ordinaryUsageAllowed` 不置 false。** |
| **Decide** | 调度器对受影响的 8 个 in-flight task 重跑 §4.4 过滤。判定树：窗口 `appliesTo` 是账号级 → 全部 **park until `resetsAt`**（默认策略，对应 Claude Code 自己的 `autoContinueAtUsageLimit`）；窗口是 family 级且同 profile 存在 `state=available` 的兄弟（`<gateway-model-B>`，`priority` 次高，且在 `models[es1].fallback` 里）→ 对 **running** 的 worker 走 `switch-model`（省下 clean slate），对 **queued/deferred** 的只换候选，**不重发 brief**。 |
| **Act** | `remuda worker switch-model <name> --to <gateway-model-B>[1m]`：按 harness recipe 执行确认舞（claude：`esc`×2 打断重试循环 → prompt `/model …` → `enter` 吃掉缓存确认 → prompt「continue where you left off」）。recipe 落在 `crates/remuda-rules/` 的新文件里，不散在 105 份 brief。若无兄弟可用 → task 进 `parked(until=resetsAt)`，`remuda dispatch --defer-until supply-ok` 的守候由产品持有（今天是 `spawn-d028-p1.sh` 一个占着终端的 3 小时轮询）。 |
| **Audit** | 每个被切换/park 的 worker 追加一行 `placement` ledger（`reasons[]` 含「es1 window 100% observed at T」、`rejected[]` 含被跳过的供给及原因）；`audit_log`（`crates/remuda-hub/src/store.rs:2970`）记 `supply.cooldown` / `worker.switch-model`；bot 发**一张**「供给事故」卡片（不是 8 张）：受影响 worker 数、切前切后、预计恢复时刻、是否需要 owner 决策。 |

**分歧裁决（§1.3 分歧 4）— 撞限后怎么办。** playbook 记录的是「在跑的 worker 上活切模型」；prior-art 主张「默认 park 到 `resetsAt`，fallback 是迁移」。**prior-art 的默认赢**（park 是 default），**playbook 的活切作为显式合法路径保留**，且**只在窗口是 family 级时合法**——因为 Anthropic 文档白纸黑字：session/weekly 限额跨模型共享，`/model` 换不掉；Opus/Sonnet 家族限额才换得掉。09-14 的 es1 事故恰好是 family 级，所以当时的手工处置是对的，而把它写成无条件规则会在 account 级限额下一头撞墙。

---

## 5 Bot 出口

复用既有飞书设计（D-008；`crates/remuda-feishu/`；`docs/design/feishu-session-card.md`）。今天它自述「是一个 channel adapter，不是 agent loop」（`crates/remuda-feishu/src/lib.rs:1-13`）——本节把它变成 T1 的出口，不重写传输。

### 5.1 三个必须先修的阻塞项

| # | 位置 | 现状 | 修法 |
|---|---|---|---|
| 1 | `crates/remuda-hub/src/interactions.rs:154` | `answer_interaction` 要求 `InputOrigin::Human`，bot 设备令牌拿 403（`feishu-session-card.md §1` 有实测） | 放行 `InputOrigin::Bot`，条件是：ticket 绑定存于 **Hub**、答复携带的飞书 `open_id` 在 `dispatcher.owner_open_ids`（`crates/remuda/src/config.rs:122`）内、且该 interaction 属于 bot 绑定的会话。D-005/D-011 的「bot 永不 bypass」不变——bot 只是 owner 的信使，不是自证者；agent 转述的批准仍然无效 |
| 2 | `crates/remuda-feishu/src/hub_api.rs:177` | `map_journal_event` 匹配 `"tool"/"tool_use"/"tool-boundary"` 与 `"result"/"completion"`，而真实 `ObservationKind` 是 `tool_call`/`tool_result`/`lifecycle`（`crates/remuda-protocol/src/enums.rs`）——进度与完成卡片在真实 journal 上**永不触发** | 改成真实 kind，并加一条 journal→卡片的 golden 测试，防回归 |
| 3 | `crates/remuda-feishu/src/tickets.rs:122` | `TicketStore` 纯内存，dispatcher 一重启所有开着的卡片绑定全丢 | 迁到 Hub 表（与 `interactions` 同库同事务），TTL 保持 10–15 分钟 |

### 5.2 Intake：聊天 → T1 ledger

今天 `Intent::Prompt` 直接变成**一个 worker instance**（`crates/remuda-feishu/src/dispatcher.rs:720` `handle_prompt`），逐字转发。改为：

```
owner 消息 → SessionKey（feishu:{chat_id}:{thread_id||root_id||main}，inbound.rs:185-196，不变）
           → 按 chat_id / session_key 查 projectId（取代今天唯一一组全局 RouteDefaults，`crates/remuda-feishu/src/dispatcher.rs:246` / 装配于 `crates/remuda/src/cmd/dispatcher.rs:224-228`）
           → POST /v1/tasks  { projectId, intent, urgency, ownerVisible }   ← T1 ledger 行
           → T1 coordinator 的 inbox（一条 instance.send）
```

`ExplicitCommand`（`crates/remuda-feishu/src/inbound.rs:304`）加 `/project <name>`，与既有 `/host` `/agent` `/model` `/status` `/stop` `/yes` `/no` 并列。`RouteDefaults`（`crates/remuda-feishu/src/dispatcher.rs:246`）从「一个全局三元组」变成「projectId → 项目默认值」的查表——这是 surfaces 点名的「最清楚的一个 T2 空洞」。

### 5.3 进度摘要

两份文档 + 三类消息（prior-art §D.5，取自 Magentic-One 的双 ledger）：

- **Task ledger 卡片**（置顶，原地编辑）：目标、已知事实、假设、拆分、每个子任务的归属、将要跑的 gate。
- **Progress ledger**（增量驱动）：每个 task 的 `state ∈ pending|placed|running|stalled|done|failed|parked|deferred`、`placement` 行、耗时、累计估算 USD、上次 gate 结果。
- **消息只有三类**：① **placement**（派发时一张卡，带 `reasons[]`，owner 能看见「为什么是这个 harness/model」）；② **decision**（审批/提问，走 interaction broker）；③ **outcome**（gate 报告 + diff scope 检查 + `DONE <sha>`，或失败与失败步骤的输出）。

**节奏：事件驱动，不是定时。** 传输层仍是既有的 `follow_live` 轮询 `GET /v1/instances/{id}/journal?afterSeq=`（`crates/remuda-feishu/src/dispatcher.rs:630` `follow_live` → `hub_api.rs:135-144`；`follow_interval_ms` 默认 1000，`crates/remuda/src/config.rs:133`）——保留；但**发到聊天窗口的条件是状态变化**。这是 §1.3 分歧 5（playbook 的 120 s supervisor 轮询 vs prior-art 的事件驱动）：**prior-art 在用户可见层赢**（周期性 digest 本身就是一笔成本，也是「agents distracting each other with excessive updates」的实例）；**playbook 的轮询在传输层保留**（它已经跑通了）。

### 5.4 阻塞式提问与审批

沿用 interaction broker（protocol §6，D-005）：worker 的 `can_use_tool` → Hub interaction → T2 判定（在其权限内）或上抛 → T1 → bot 卡片 → **owner 本人**点按 → `POST /v1/interactions/{id}/answer`（修好 §5.1#1 后 bot 可代投，但主体仍是 owner）。`/yes` `/no` 快捷方式（`inbound.rs:330-341`）保留。

### 5.5 Escalation

只有四类事情能打扰 owner（playbook §1.1）：**policy 变更**（D-031 就是这么来的）、**凭据**、**OS 权限**（`c-deploy` 试图给自己开 Full Disk Access 被拦下，规则「权限问题一律 BLOCKED 交用户」）、**破坏性历史操作**。加第五类：**预算/供给耗尽且无 fallback**。`remuda report escalate <task> --reason` 是唯一能主动打断 owner 的动词。

### 5.6 审计线索

每条 owner 可见消息挂一条 ledger 行；`audit_log`（`store.rs:2970`，已有 `subject` 索引）补 `task.*` / `supply.*` / `placement.*` / `land.*` 动作；`placement` ledger 行带 `reasons[]`/`rejected[]`，既是调度解释也是卡片正文，二者永不分叉。

---

## 6 现有能力对照与缺口

| 能力 | 今天在哪 | 状态 | 缺口 / 落点 |
|---|---|---|---|
| Worker 全生命周期 | `crates/remuda-hub/src/instances.rs:13-22`、`crates/remuda/src/cmd/instance.rs:38-131`、MCP `crates/remuda/src/cmd/mcp/instance.rs` | **OK** | 无（D-015 有实证） |
| worktree 隔离 | `crates/remuda-hub/src/http.rs:144`、`crates/remuda/src/cmd/worktree.rs`、`crates/remuda-protocol/src/path_guard.rs` | **OK** | 远端 worktree 根仍是脚本里的常量 → `Project.members[]` |
| gate + CAS 落地 | `crates/remuda/src/cmd/merge.rs:19-44`，权威 `scripts/ci/gate.sh` 从合并后 worktree 读（`docs/design/remuda-cli.md:134-137`） | **OK（最强的 T2 原语）** | 无 lane / 队列序列化 → `r-mergequeue` 在做 `--onto/--lanes`；本设计只消费 |
| 主机注册 / 在线 / 标签 / maxInstances | `crates/remuda-hub/src/store.rs:175-233`、`registry.rs:137-160` | **OK** | 主机是扁平全局车队；项目归属只能靠标签约定 → `Project.hosts[]` |
| Workspace 注册（D-023） | `crates/remuda-hub/src/workspaces.rs:14-19`、`store.rs:226-229` | **OK** | Workspace 固定属于一台 host → 项目跨 host 的 N 需要 `Project.members[]` |
| Space | `web/src/features/spaces/store.ts:44,53`（localStorage `remuda.spaces.v1`） | **UI-only** | 保留原样；`(hostId, workspaceId)` 直接当 `Project.members[]` 的键复用 |
| Placement | `crates/remuda-hub/src/placement.rs:17`（`Host\|Labels\|Any`）、`:133` `select_hosts`、`:162` `load_of`、`:172` `consider` | **PARTIAL** | 单维度最闲主机；加 `Placement::Project`、供给准入、cpu/mem/disk/building 维度 |
| cpu / mem 快照 | 采集 `crates/remuda-node/src/inventory.rs:723`，存 `store.rs:203` | **采了没人读** | `select_hosts` 从不读 → 批次 3 接进 `load_of`/`consider`（最便宜的一项） |
| Host launch 默认值折叠 | `crates/remuda-hub/src/http.rs:516` `merge_host_launch_defaults`（replace 非 concat） | **OK，且是精确的插入点** | 旁边加 `merge_project_defaults`，优先级 显式 > project > host > 全局 |
| Provider 解析瀑布（D-021） | `crates/remuda-hub/src/provider_resolve.rs:156-253`，密钥守卫 `:119` | **PARTIAL** | 瀑布只按 host 键，不按 project 键 → 同机两项目不能用不同网关；`Project.provider` 插在 host binding 之前 |
| Provider model catalog | `crates/remuda-hub/src/provider_models.rs:22`（`{id,enabled,label,contextWindow,tags,surfaces}`，legacy 字符串数组自动迁移） | **OK 的载体** | 无 supply 字段 → §4.2 纯增量扩展 |
| Effort 档位 | `crates/remuda-protocol/src/launch.rs:148-231`，UI 表 `web/src/features/session/effort.ts:44-84` | **旋钮 OK，策略缺** | 无机器可读的相对成本 → capability 表补 |
| Usage / 价目 | `crates/remuda-driver/src/usage/`、`prices.rs:14`；状态「已落地、已测，**尚未接入任何 driver 的 tail**」（`docs/design/usage-adapter.md:3`） | **建了没接** | Hub 侧既不存也不聚合（`crates/remuda-hub/` grep `usage` 零命中）→ 批次 3 建消费端；tail 由 `r-p6-adapters` 接 |
| Claude 限流帧 | 类型 `crates/remuda-claude-wire/src/types.rs:369`，被丢成 diagnostic `crates/remuda-driver/src/claude_print.rs:764` | **可见但惰性** | `rate_limit_info` 是 `Value`，需一次真实帧探针（§7 开放问题） |
| Codex 限流通知 | `crates/remuda-codex-wire/src/notification.rs:106` | **零消费者** | 免费的供给遥测被丢掉；schema 已 vendored 在 `docs/research/cli-help/codex-app-server-schema/v2/` |
| 账号级并发上限 | — | **GAP（最重要）** | `maxInstances` 只管 host（`placement.rs:200`）；真正饱和的是 provider 账号 → `supply.concurrency.max` |
| Task / 目标实体 | — | **GAP** | `Fleet`（`crates/remuda-hub/src/fleet.rs:16-21`）是一个 spec 扇到 N 台机，不是带子任务的 task → 新 `Task` |
| Ownership 注册表 | 手维护的散文表（`workbench-ux-plan.md` §0 禁改清单） | **GAP** | → `remuda own`，gate 时可执行 |
| Bot 出口 | `crates/remuda-feishu/src/lib.rs:1-13` 自述「channel adapter, not an agent loop」 | **PARTIAL** | 三个阻塞项（§5.1）+ intake 改道 |
| Bot 路由默认值 | `crates/remuda-feishu/src/dispatcher.rs:246` `RouteDefaults`（配置来自 `crates/remuda/src/config.rs:128-130`）（**一个全局 host/agent/model 三元组**） | **PARTIAL** | → projectId 查表 |
| Bots 配置页 | `web/src/features/bots/channels.ts:32` 硬编码 `const` | **MOCK** | 其 `BotChannel.defaultProject`（`:24`）是全仓唯一把 project 当路由概念的地方——类型已经留好了 |
| Agent 作为调用方的安全 | D-017：`crates/remuda-hub/src/agent_scope.rs:19,124,143,393` | **OK** | 需按 role/projectId 扩一层（§2.4） |
| 审计 | `crates/remuda-hub/src/store.rs:2970` `audit_log` | **OK** | 补 task/supply/placement/land 动作 |
| 假端测试底座 | `crates/remuda-testing/src/bin/fake-harness.rs`（`docs/design/testing-fake-harness.md`）、`crates/remuda/tests/mcp_hub.rs:42` `enroll_fake_node`、`crates/remuda-feishu/tests/hub_dispatcher.rs:158` | **OK** | 合成 429 与假远端主机可直接在这套上做（§8 每批的测试口径） |

---

## 7 风险与缓解

| # | 风险 | 具体形态（有据） | 缓解 |
|---|---|---|---|
| 1 | **失控扇出** | Anthropic 记录过「简单问题 spawn 50 个 subagent」；09-14 远端 8 个 worker 同时在编把 64 核打到 load 126 | 硬上限**写在代码里不写在 prompt 里**：`maxFanOutPerTask`、`maxConcurrentWorkers`、`supply.concurrency.max`、嵌套深度 3。超限 → `deferred` + bot 播报，不是静默排队 |
| 2 | **跨层丢上下文** | Claude Code 的 "Clean Slate Problem"；09-14 relay 503 让 5 个会话同分钟退出，每个只留一行 `claude --resume <id>` | 层的状态放 **Hub 表**而非 context：task ledger / ownership map / roster / placement ledger 可随时重放。coordinator 退出后**不续写上下文**，而是重新 brief；worker 用 D-026 `nativeRef` resume 并带一段 state-loss 前言。T2↔T3 边永远只传 `last_message` |
| 3 | **范围漂移** | `coordinator-guide.md` 原话：「diff 长到超出任务点名的文件是最常见的拒绝理由，而且没有 gate 抓得住」 | `remuda own claim/check`：dispatch 时登记 `owns[]`，gate 时对账 `git diff --stat main...<br>`；越界即 `onScopeDrift: ask-owner`。**语义** review 仍由 LLM 做——`own check` 只抓路径 |
| 4 | **成本爆炸** | multi-agent 约为单聊的 15×；agent teams 在 plan mode 约 7×；公开的 16-Claude C 编译器实验 $20,000 | 每 task `budget.maxUsd/maxTurns/maxWallMins`，子任务花费计入父预算；因为金额是**估算**（±10%），硬 cap 带容差：估算处告警、估算×1.15 处停。超 `maxWallMins` 的 worker 被停并上报，不默默跑着 |
| 5 | **画像过期** | 价目表 `PRICE_TABLE_UPDATED = "2026-09-14"`；Opus-5/Sonnet-5 行是 `Provisional`，实测有一帧偏 1.79×；网关会改名/悄悄替换模型 | capability 表带 `revision` + `Published\|Provisional`，UI/卡片一律标「估算」；supply 的 `source` 逐字段记录，observed **不清除**已观测值；`aliases`/`family` 处理网关改名；`remuda profile probe` 是一次 1-token 真值校准 |
| 6 | **多用户/共享主机** | 09-14：几十个孤儿 `fake-herdr server` 以 93% CPU 跑了 12 小时；`/tmp` 95%，`target-gate` 71 GB；一个 worker 的清理极可能杀掉了承载另外 8 个 worker 的 carrier | ① 资源块（ports/targetDir/scratch/e2e lock/browser endpoint）由产品**分配**并在 retire 回收（含 `rm -rf target-<name>`，且必须报告回收字节数）；② `hostcap` 作为 dispatch 前置；③ 进程安全变沙箱 denylist（禁裸 `pkill herdr`）而不是 105 份 brief 里的一句话；④ 共享资源一把 advisory lock 由 `gate` 分配 |
| 7 | **T2 自己动手改代码** | Claude Code teams 已记录「lead 开始自己实现」 | CrewAI 原则：coordinator 只持有 dispatch/gate 工具，不持有编辑工具（§2.1/§2.2 的工具面按 role 在 Hub 侧收紧，不靠自律） |
| 8 | **DONE 谎报 / 状态滞后** | Claude Code teams：「teammate 有时不标完成，阻塞依赖」 | 不变量 I1：**DONE 是声明不是 gate**。依赖解锁看的是 `land` 的 sha，不是 worker 的状态位 |
| 9 | **供给判断在部分信息下失误** | es1 429 与 seed 503 交替，探针本身间歇失败 | `ordinaryUsageAllowed` 不从百分比/重置时间推断（Codex 契约原话）；529 类不冷却；无候选时 `deferred` 而非降级；三次以上反复 → escalate |
| 10 | **CAS 滑落** | 09-14：coordinator 用 `update-ref` 推进本地 main，**抹掉了 gate 已合的一次 merge**，只有被拒的 push 暴露出来 | `land` 内置 `git merge-base --is-ancestor` + CAS，退出码 `BaseMoved`；不存在手工 `update-ref` 路径 |
| 11 | **协议/热文件冲突** | `native-pty-first.md` §13 的冲突规避规则：协议变更由单一 owner 一次性纯增量落地 | 批次 1 独占全部 protocol/store/openapi 增量，其余批次只 rebase 一次；每个文件唯一 owner（§8 矩阵） |

---

## 8 分阶段计划

体例同 `docs/design/native-pty-first.md` §13 与 `docs/design/workbench-ux-plan.md` §1：一批 = 一 worker = 一分支 `wt/<name>/<slug>`，过统一 gate 合入。

### 8.0 与在飞工作的边界（禁改清单）

| 在飞分支 | 它独占 | 本计划的约束 |
|---|---|---|
| `r-mergequeue` | `crates/remuda/src/cmd/merge.rs`、`crates/remuda/src/cmd/merge/` | 批次 1–5 **不得写**；批次 6 在它合入后**消费** `--onto/--lanes`，不改其文件 |
| `r-p2-createlag` | `crates/remuda-node/src/runtime.rs`、`runtime_wss.rs` | 全程不碰 `crates/remuda-node/src/runtime*.rs` |
| `ux-f` | `web/src/pages/SettingsPage.tsx`、`settings.module.css`、`styles/{tokens.css,ui.module.css}`、`features/spaces/spaces.module.css` | 全程**不写 `web/src/`**（`api.generated.ts` 除外，见下） |
| `r-p6-adapters` | `crates/remuda-driver/src/{codex_adapter,grok_adapter}.rs` + usage tail 接线 | 不碰 `crates/remuda-driver/src/usage/*` 与两个 adapter 文件；只在 Hub 侧建消费端 |
| `r-effortsync` | effort 同步相关 | 不碰 `crates/remuda-protocol/src/launch.rs` 的 effort 段 |
| `r-live-signal` / `r-live-screen` | `crates/remuda-signal/*`、`crates/remuda-screen/*` | 全程不碰这两个 crate |

**`web/src/lib/api.generated.ts`**：`gen-api-current` 是强制 gate 步骤（`coordinator-guide.md`），任何 OpenAPI 变更都会改它。规则：**只有批次 1 与批次 6 在自己的 gate 里跑 `pnpm --dir web run gen:api` 并提交该文件**，其余批次不产生 OpenAPI 变更；与 ux 批次的顺序冲突用「先落 ux 批、本批 rebase 一次」解决。

### 8.1 批次矩阵

| 批 | 名 | 独占文件（owned） | 新增测试 | 证据文档 | 大小 | 何时开工 |
|---|---|---|---|---|---|---|
| **1** | `co-project` | `crates/remuda-protocol/src/project.rs`(新) + `entities.rs`/`enums.rs` 纯增量；`crates/remuda-hub/src/projects.rs`(新)；`store.rs`（`projects` 表 + instances 的 `role`/`project_id`/`task_id` 三列，走 `ensure_column`，`store.rs:3506`）；`placement.rs`（`Placement::Project`）；`http.rs`（`merge_project_defaults`，紧邻 `:516`）；`agent_scope.rs`（role/project 作用域）；`provider_resolve.rs`（瀑布加 project 层）；`crates/remuda/src/cmd/project.rs`(新)；`web/src/lib/api.generated.ts`（仅重生成） | `crates/remuda-hub/tests/projects.rs`(新)：CRUD、成员跨两台 fake host、默认值折叠优先级（显式>project>host>全局）、`Placement::Project` 解析与解析结果、绑定项目 A 的 agent 令牌访问项目 B 得 403；`placement.rs` 单测；`crates/remuda/tests/project_cli.rs`(新) | `docs/design/evidence/coord-project-1.md` | L | **今天并行** |
| **2** | `co-botfix` | `crates/remuda-feishu/src/{hub_api.rs,tickets.rs,inbound.rs}`；`crates/remuda-hub/src/interactions.rs`；`crates/remuda-hub/src/store.rs` 的 `card_tickets` 表（**与批 1 同文件**→ 批 1 先落表结构骨架，本批只 `ensure_column` 追加；若排期冲突则本批延后到批 1 合入后） | `crates/remuda-feishu/tests/hub_dispatcher.rs` 增补：真实 `ObservationKind`（`tool_call`/`tool_result`/`lifecycle`）驱动出进度卡与完成卡的 golden；bot 令牌答 interaction 由 403→200（owner open_id 在白名单内）且非白名单仍 403；dispatcher 重启后 ticket 绑定仍在 | `docs/design/evidence/coord-bot-1.md` | M | **今天并行**（文件与批 1 零交集，`store.rs` 例外见左） |
| **3** | `co-supply` | `crates/remuda-hub/src/supply.rs`(新)；`model_catalog.rs`(新，capability 表)；`provider_models.rs`（supply 字段，纯增量）；`providers.rs`（REST 增量）；`placement.rs`（供给准入 + rank + cpu/mem/disk 维度）；`crates/remuda-hub/src/usage_store.rs`(新，Hub 侧 usage 消费端)；`crates/remuda/src/cmd/profile.rs`(新) | `crates/remuda-hub/tests/supply.rs`(新)：**合成 429** —— fake node + `fake-harness` 脚本吐 `Request rejected (429)` 与一帧 Codex `account/rateLimits/updated`，断言窗口置位、`state=cooling`、`cooldownUntil=resetsAt`、**529 不冷却**；family 级窗口允许兄弟模型、account 级窗口全 park；`concurrency.max` 跨两台 **fake 远端主机**生效（host 都没满也要拒）；rank 排序单测（priority > 余量 > affinity > cost > load）；`remuda profile show/probe --dry-run` | `docs/design/evidence/coord-supply-1.md` | L | 批 1 合入后（共用 `placement.rs`/`store.rs`） |
| **4** | `co-task` | `crates/remuda-protocol/src/task.rs`(新)；`crates/remuda-hub/src/tasks.rs`(新)；`store.rs` 的 `tasks`/`task_paths`/`placements` 三表；`crates/remuda/src/cmd/{task.rs,own.rs}`(新) | `crates/remuda-hub/tests/tasks.rs`(新)：状态机合法迁移与非法拒绝、依赖边解锁只认 `land` 的 sha 不认 worker 状态位、`own claim` 冲突检测、`own check` 对合成 diff 判越界、placement ledger 行的 `reasons[]`/`rejected[]` 往返；`crates/remuda/tests/own_cli.rs`(新) | `docs/design/evidence/coord-task-1.md` | L | 批 1 合入后（与批 3 并行；`store.rs` 由批 1 先落表骨架，二者各自 `ensure_column`，排期上批 3 先） |
| **5** | `co-loop` | `crates/remuda/src/cmd/{dispatch.rs,watch.rs,worker.rs,retire.rs,brief.rs}`(新)；`crates/remuda/src/cmd/mcp/coordinator.rs`(新)；`crates/remuda-rules/src/supply.rs`(新，switch-model/resume 的 per-harness 确认舞)；`skills/remuda-coordinator/`(新目录)；`skills/remuda/sections/10-coordinator.md` | `crates/remuda/tests/coordinator_loop.rs`(新)，全程 fake node + `fake-harness`：`dispatch` 幂等（同名不二次派发）、资源块分配与 `retire` 回收（断言 target dir 被删且报告字节数）、`watch` 事件（`reported{done,blocked}`/`stalled`/`gone`/`supply-down`）含 **brief 回显抑制**（`<sha>`/`<reason>` 占位不得命中）与 nudge 节流；`worker switch-model` 的按键序列 golden；`brief lint` 拒绝含密钥/隧道工具名/缺完成协议的 brief；`dispatch --defer-until supply-ok` 在合成 429 下不阻塞主回合 | `docs/design/evidence/coord-loop-1.md` | L | 批 3 **与** 批 4 合入后 → **M1** |
| **6** | `co-lanes` ✅ 2026-09-17（gate/land 经 Node 落地；见 D-034） | `crates/remuda-protocol/src/gate.rs`(新：GateJob/Step/枚举 + gate.* 线类型)；`crates/remuda-node/src/gate.rs`(新：lane runner `gate.run`/`gate.cancel`/`gate.then`，stdio+WSS 双 carrier event pump)；`crates/remuda-hub/src/gatequeue.rs`(新：gate_jobs 表、`/v1/projects/{id}/gate*` 路由、FIFO 调度器、CAS reverify、cancel、audit journal)；`crates/remuda/src/cmd/gate.rs`(新：`remuda gate`/`land`/`gate list`/`gate cancel`)；`crates/remuda/src/cmd/{watch.rs,report.rs,dispatch.rs,project.rs}`（gate 行、carrier 偏好、lane 配置）；`ProjectGateLane` 纯增量 env/lockPath/pwEndpoint/toolchainPath；`merge.rs`/`merge/` 零改动；`web/src/lib/api.generated.ts`（重生成） | `remuda-hub/tests/gate_queue.rs`(8 例：两 lane 并行 verify、land 串行、CAS 失而复得 re-verify、FIFO、queued/running cancel、输入校验)；`remuda-node` gate runner 7 例（假 gate 脚本 + 真 git fixture：step stream、stale-tip 拒绝、lane-busy、cancel killpg、整跑超时、land argv）；`crates/remuda/tests/gate_cli.rs`(5 例：实时 step 表、JSON shape、land sha、list、help) | `docs/design/evidence/gate-lane-1.md` | L | `r-mergequeue` 合入 **且** 批 5 合入后 → **M2**（飞书卡片仍属后续批次） |

**并行窗口**：今天可同时跑 **批 1 与批 2**（除 `store.rs` 的表骨架外零交集，见批 2 备注）。批 1 合入后放批 3；批 3 合入后放批 4（或二者并行，代价是 `store.rs` 一次 rebase）。批 5 等 3+4。批 6 等 `r-mergequeue` 与批 5。

### 8.2 里程碑与验收

- **M1（批 5 合入）——「人类 coordinator 只用 remuda 动词，不写 shell 脚本」。** 验收：用 `remuda project/task/own/brief/dispatch/watch/worker/retire` 完成一轮**真实**派工（≥3 个 worker，至少一个远端），过程中 `<coord-scratch>/*.sh` 一次都不调用；证据文档记录每一步的命令与输出，并逐条对照 playbook §3 的 11 个步骤指出哪些已由产品承担、哪些仍是 LLM 判断。
- **M2（批 6 合入）——「bot 是出口」。** 验收：owner 在飞书发一句中文意图 → T1 ledger 出现一行 → T2 派发并回一张带 `reasons[]` 的 placement 卡 → gate 结果卡 → owner 在卡片上批准一次 interaction（不再 403）→ `land` 的 sha 出现在 outcome 卡里。dispatcher 中途重启一次，ticket 绑定不丢。
- **每批共同验收**：`remuda merge <branch> --gate --json` 全绿（含 `secret-scan`、`no-tunnel-scan`、`gen-api-current`、`verify-tree`）；证据文档用**合成夹具**，不含真实凭据；新增公共 API 有 OpenAPI 与生成客户端同步。

### 8.3 开放问题（不阻塞批 1，阻塞批 3 的 observed-structured 路径）

1. **Claude `rate_limit_info` 的真实形状** —— `crates/remuda-claude-wire/src/types.rs:371` 里是 `Value`，`crates/remuda-driver/src/claude_print.rs:764` 丢掉。需要一次真实帧探针；拿不到就只走 observed-textual + declared。
2. **本机安装的 `codex app-server` 是否真的暴露 `account/rateLimits/read`** —— schema 已 vendored，变体已解（`crates/remuda-codex-wire/src/notification.rs:106`），零消费者。若暴露，这是今天就白送的供给遥测。
3. **grok / agy 的供给信号** —— `usage.json` 的 body 从未被观测到（`usage-adapter.md` 已记为已知偏差）；在此之前 grok 供给只能 `declared`。
4. **网关供给 vs 原生供给** —— 网关 profile（D-007/D-012）根本没有 plan 窗口，其供给是网关自己的配额，Remuda 不可见。建议建模为 `windows: []` + `state: unknown`，只在被显式提高优先级、或所有带窗口的供给都耗尽时才用。
5. **估算误差与硬预算** —— 成本估算只有一个锚点、±10%；硬 `maxUsd` 必须带容差带，且每个数字在卡片上标「估算」。
