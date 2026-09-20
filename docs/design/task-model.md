# Task 优先模型：任务聚合、目录绑定、worktree 池与看板投影

- 日期：2026-09-20
- 状态：设计权威（task-model 批次任务 2–9 的实现引用本文；本文为 docs-only，不含任何代码变更）
- 基线：`origin/main` = `1468a2ba`；本文所有 `file:line` 均在该基线上逐条复核（复核表见 [evidence/task-model-1.md](./evidence/task-model-1.md) §4）
- ADR：[D-050](./decisions.md)
- 相关规格：[ui-spec.md](./ui-spec.md) §1.5 / §2.9（task 层与看板/任务列表表面）、[coordinator-hierarchy.md](./coordinator-hierarchy.md) §2.6（台账向人向表面浮现）、[files-view-contract.md](./files-view-contract.md)（文件视图轴与六可用性状态）、D-024 / D-033 / D-035 / D-047 / D-049

## 0. 一句话架构判据

**Task 是 instance 之上的一层聚合投影，不是第二套状态机。**

- Hub 的 Task 台账与其 8 态迁移是权威；看板列、任务分组、人读 key、「与 N 个 task 共用」全部是只读派生，不可写回为状态。
- 每个 task 的工作目录绑定二选一：**`reuse`**（复用既有目录 = 一目录一分支，attach 期间独占，多 task 顺序轮用）或 **`pool`**（app 管理的 worktree 池 = detached-HEAD 停放、租借时按 task 切分支、归还不删除）。
- 目录冲突、池满、分支冲突一律**显式拒绝**（`blocked{reasons[]}` / 典型 429 `SUPPLY_DEFERRED`，`crates/remuda-hub/src/error.rs:154`），绝不静默回退到主 checkout、绝不偷换目录（D-035 / D-047）。
- 「与 N 个 task 共用」指多个 task **顺序轮用**同一目录/分支，不是同时执行；并发隔离靠运行期 attach-lock，不靠 gate 期的 `remuda own check`。

## 1. 实体模型

### 1.1 Task —— 已存在，仅两处增量

`Task` 行已在协议与 Hub 中存在（`crates/remuda-protocol/src/task.rs:194`）：`projectId / parentTaskId / title / mandate / class / state / owns / deps / budget / placement / landedSha / blockedReason`。8 态枚举在 `task.rs:16-23`（pending/placed/running/stalled/done/failed/parked/deferred），合法迁移只由 `can_transition_to` 判定（`task.rs:52-64`），终态只认 done/failed（`is_terminal`，`task.rs:67-69`）。Hub CRUD 路由在 `crates/remuda-hub/src/tasks.rs:185-199`（`/v1/tasks`、`/split`、`/land`、`/own`、`/placements`）。

本模型只加两处存储：

1. **增量可空列 `archived_at`**。归档是正交维度，不进 8 态机、不碰 `is_terminal` 的 done/failed 语义。迁移落在 `tasks.rs` 的 `migrate()`（`tasks.rs:143`，`CREATE TABLE tasks` 在 `:147`），用幂等的 `ALTER TABLE tasks ADD COLUMN archived_at`；**不**放进 `store.rs` 的 `ensure_column` 区块（那处只迁 `instances`/`hosts` 表，`store.rs:4430-4445`，放错会加错表并制造两个模块的迁移耦合）。旧行 `archived_at` 为 NULL，行为与今天字节一致。
2. **`workspaceBinding` 字段**序列化进既有 `doc_json` 列（`tasks.rs:154` 是 `doc_json TEXT NOT NULL`，整行 Task 序列化其中），serde `#[serde(default)]` 保证旧行零迁移、字节安全：

```jsonc
{
  "mode": "reuse",            // "reuse" | "pool"
  "hostId": "hst_1",
  "workspaceId": "wsp_1",
  "worktreeName": null,       // 可空；reuse-to-root 时为 null
  "branch": null,             // 仅作记录；reuse 不切分支，pool 分支由 Node 派生
  "leaseRefIds": []           // 关联的 worktree_leases 行
}
```

`boardColumn` **不存储**，由 `state + archived_at + placement` 只读派生（§5）。

### 1.2 Instance（session）—— 无改动

一实例一终端（ui-spec §1.3）。实例经既有 `instances.task_id` 列归属 task（`crates/remuda-hub/src/store.rs:4441`）；placement 经 `TaskPlacementRef.instanceId` 反指会话（`task.rs:173-190`，字段在 `:182`）。不新增实例列、不改派发 wire（§3.4）。

### 1.3 WorktreeLease —— 唯一的新表

```sql
CREATE TABLE worktree_leases (
    id                 TEXT PRIMARY KEY,
    mode               TEXT NOT NULL,          -- 'reuse' | 'pool'
    host_id            TEXT NOT NULL,
    workspace_id      TEXT NOT NULL,
    worktree_name      TEXT,                   -- 可空：reuse-to-root 为 NULL
    dir_key            TEXT NOT NULL,          -- 相对 workspace 根的规范化路径；注册根本身 = '.'
    branch             TEXT,
    project_id         TEXT NOT NULL,
    refcount           INTEGER NOT NULL,
    state              TEXT NOT NULL,          -- 'leased' | 'parked' | 'provisioning'
    holder_instance_id TEXT,                   -- 可空：当前 attach 的会话，attach-lock 持有者
    base_oid           TEXT,
    created_at         TEXT NOT NULL,
    released_at        TEXT,
    UNIQUE (host_id, workspace_id, dir_key)
);
```

三个关键设计：

- **`mode` 列**区分 reuse 与 pool，使 reset/clean/park 语义严格 **pool-only**（§2.2）；reuse lease 归还对目录零操作（§2.1）。
- **身份键是复合键 `(host_id, workspace_id, dir_key)`**，不是 worktree 名。`dir_key` = 目标目录相对 workspace 根的规范化路径；注册根本身为 `'.'`。`worktree_name` 可空：reuse-to-root 时仓库根不是 worktree、没有 name（`resolve_instance_cwd` 明确允许注册根本身，`crates/remuda-node/src/worktree.rs:355-401`）。「与 N 个 task 共用」= 同一复合键行的 `refcount`，对 reuse-to-root 同样成立。同名目录、跨主机目录因此仍然不合并（D-024）。
- **`holder_instance_id` + attach-lock**：一个 lease 在有会话 attach 期间独占（§2.3）；第二个 task 绑同一复合键时 `refcount += 1` 但排队或 `blocked{reason}`，不并发执行。

### 1.4 目录端 catalog 增量 —— 零迁移 JSON

`<git-common-dir>/remuda-worktrees.json` 的 `WorktreeRecord`（`worktree.rs:12-19`）增加两个 serde-default 字段：

```jsonc
{ "name": "s01", "path": "…", "branch": "wt/s01/<slug>", "base": "main",
  "state": "free | leased | parked",
  "leasedBy": ["tsk_…"] }
```

JSON 目录册无 schema 迁移：旧记录缺字段即取默认（`#[serde(default)]`），与 `Catalog` 现有写法同形（`worktree.rs:21-26`）。

### 1.5 Annotation —— 不是表、不是 wire 字段

批注是**设备本地 composer 草稿**（与 `web/src/lib/drafts.ts` 同类）：`{carrier:"card"|"anchor", anchor?:{messageId, textRange}, body}`。发送时序列化进 prompt 前缀，发送后清零（§8）。零表、零 wire、零 schema。

## 2. 目录绑定规则

task 创建时 `workspaceBinding.mode` 二选一。首次绑定强制显式选择、按项目记忆上次选择（force-choose-then-remember-per-project）；不存在静默默认。

### 2.1 `reuse` —— 复用既有目录（顺序轮用，一目录一分支）

- 绑定目标 = 注册的 Space 根，或其旁已存在的 `remuda-wt/*` 兄弟 worktree。会话 cwd 准入**完全**复用现成的 `resolve_instance_cwd`（`worktree.rs:355-401`）：它允许注册根或 `remuda-wt` 兄弟、越界即拒（受管根集合在 `:382-393`，`path_guard::contain` 调用在 `:388`），无需新代码。
- **不做任何 git 操作**：不 clean、不 reset、不 remove、不切分支。多个 task 绑同一目录 = 绑同一 git 分支。
- **共享 = 顺序轮用，不是同时执行**：reuse 也落 lease 表（`mode='reuse'`）；attach 期间以 `holder_instance_id` 独占，第二个 task 绑同一 `(host, workspace, dir_key)` 时 `refcount += 1` 但排队或返回 `blocked{reason}`，等前一个 detach 才轮到。两个 agent 同时在一个 worktree/分支执行会互相踩坏 `index.lock`/工作树/暂存区，且这先于任何 gate 发生，所以并发隔离必须是运行期 attach-lock。`remuda own check`（`tasks.rs:641-679`）是 gate 期的纯函数路径对账，不是运行锁，不承担并发保护。
- **reuse lease 归还 = 对目录零操作**：refcount 归零时绝不 `git clean`/`reset`/`remove`——绑定目标可能是用户的真实工作目录或注册根，任何清理都是数据丢失。return 只改 lease 行状态，工作树逐字节不变（任务 2 有字节不变断言）。
- gate 期归属仍走 `remuda own check`：`owns[]` 越界在 gate 抓（`tasks.rs:641-679`）；它与 attach-lock 是两件事，不混为一谈。

### 2.2 `pool` —— app 管理的 worktree 池（池化、顺序轮转、按 task 切分支）

每 `(host, repo)` 一个 worktree 池，容量固定 N（默认 4，可配）；只驱逐 clean + idle + `refcount=0` 的 slot。不设上限不是选项（磁盘成本无界）。池层叠在既有 `provision_record`（`worktree.rs:60`）之上，隔离在新模块 `worktree_pool.rs`，不改 `worktree.rs` 的既有创建语义。

- **停在 detached HEAD、租借时才切分支**：slot 空闲时挂在池 base 的 detached HEAD 上、不带分支；`lease` 时在原地切出每 task 独立分支 `wt/<slot>/<task-slug>`（分支 ≠ 目录）。这绕开 git「同一分支不能在两个 worktree 检出」以及 `provision_record` 遇 `ref_exists(branch)` 直接拒绝的路径（`worktree.rs:101-105`）。分支形状仍是 gate/land/`remuda own` 假设的 `wt/<name>/<slug>`（`validate_branch`，`crates/remuda-node/src/worker.rs:58`），`landed_sha` 解锁依赖（§5.4）不受影响。`<task-slug>` 由 Node 从 `taskId` 确定性派生（文件系统安全后缀），分支名不过 wire。
- **lease 三态决策（拒绝而非改道）**：
  1. 有 **parked** slot（detached HEAD 停在池 base）→ 原地切 `wt/<slot>/<task-slug>`、`state=leased`、catalog `leasedBy=[task]`。**warm 命中不 fetch**（避开 `git_fetch` 的无界 fetch 与失败即致命，`worktree.rs:95-99` 与 `:185-205`）。
  2. 否则池未满（`< N`）→ `provision_record` 建新 slot（fetch-first、从 `origin/<base>` 切）。
  3. 否则（池满且无干净 slot）→ **拒绝**：典型 `blocked{reason}` / 429 `SUPPLY_DEFERRED`。绝不静默 reroute、绝不复用脏 slot。
- **return 即重置、不删除（仅 pool）**：`return(task)` → `refcount -= 1`；归零且 task 已归档时 `git clean -fd`（清未跟踪、保留 ignored，使 `node_modules`/target 目录保持暖态）、切回 detached base、`state=parked` 归还池——**return-don't-delete**。这套 reset/clean/park 严格 `mode='pool'` only；reuse 归还零操作（§2.1）。
- **运行加锁 + 脏改拒清**：任一 session attach 期间以 `holder_instance_id` 持锁；`git status --porcelain` 非空时拒绝 return/reset。
- **信任标志保持一致**：`provisioned_worktree_named`（`worktree.rs:335-346`）只按 catalog 中精确记录的路径预信任 agent cwd（`crates/remuda-node/src/native.rs:700-703`，测试 `native.rs:3189`）。池改写 catalog 记录时必须保持 name/branch/path 与记录一致，或显式重标信任，否则丢信任、重触发 onboarding。

### 2.3 attach-lock 语义（两模式共用）

| 情形 | 行为 |
|---|---|
| lease 行无 holder | 准予 attach，写 `holder_instance_id`，`state=leased` |
| 已有 holder，同一 task 的后续 session | 允许（一个 task 的多 session 是 tab 关系；仍受同一目录的串行写约束） |
| 已有 holder，另一个 task 绑同一复合键 | `refcount += 1`；调用方得到排队位或 `blocked{reason:"dir-busy"}`，**不**并发启动 |
| holder 会话退出/detach | 清空 `holder_instance_id`；队列首 task 可 attach；reuse 不动目录，pool 不回收（return 是显式动作） |
| 脏目录上的 pool return | 拒绝 reset/clean，slot 留 `leased` 并带理由，等人工或下一次干净 return |

## 3. wire 形状（全部增量，无破坏性字段）

### 3.1 新 Node JSON-RPC：`worktree.lease` / `worktree.return`（pool 路径）

两个方法都必须进**显式 dispatch**，不能依赖任何兜底：

- 加进 `is_worktree_method` 的匹配（今天只匹配 `worktree.create|worktree.list`，`worktree.rs:207-209`）；
- 加进 Node 侧 `handle_rpc` 的 `match`（`worktree.rs:29-36`，今天只有 list/create 两臂）。

链路上 worktree 方法经显式臂进入 `node.worktree_rpc_capped`（`crates/remuda-node/src/runtime_link.rs:164-166`）；兜底臂在 `runtime_link.rs:178`，它委托的 `dispatch_method` 已把未知方法定义为错误（`crates/remuda-node/src/transport/hubnode_codec.rs:198-200`「an unknown method is an error」），不再是历史上的 `{"ok":true}` 假成功——但**注册义务不变**：不注册就到不了池实现，任务 2 必须断言「未注册时走兜底、注册后返回真实 payload」。

`worktree.lease` params（Hub→Node）：

```jsonc
{ "hostId": "hst_1", "workspaceId": "wsp_1", "name": "s01", "base": "main", "taskId": "tsk_1" }
```

result：

```jsonc
{ "slot": "s01", "worktreeName": "s01", "dirKey": "remuda-wt/s01",
  "branch": "wt/s01/<derived-slug>", "baseOid": "…",
  "state": "leased", "warm": true }
```

`worktree.return` params：`{hostId, workspaceId, name, taskId}`；result：`{state:"parked", refcount:0, cleaned:true}`（脏目录时返回结构化拒绝，slot 保持 `leased`）。

reuse 绑定**不触发这两个 RPC**：无 git 操作、无目录决策；lease 行的获取/归还是 Hub 台账动作（task 创建与派发/回收路径），cwd 准入在 instance create 时经既有 `resolve_instance_cwd` 完成。

### 3.2 新 Hub 路由

紧邻 `create_worktree`（`crates/remuda-hub/src/http.rs:1783-1805`，路由注册在 `:222`），同 `require_origin` + `require_device` 门控（`:1789-1790`）：

- `POST /v1/worktrees/{name}/lease`，body `{workspaceId, base?, taskId}`；
- `POST /v1/worktrees/{name}/return`，body `{taskId}`。

**安全约束（security-review-2 M4）**：与 `create_worktree` 一样（`http.rs:1795-1802` 明写 `path`/`repo` 不转发），转发给 Node 的 params **只含 `{hostId, workspaceId, name, base, taskId}`**，绝不转发原始 `path`/`repo`/分支名——目录由 Node 在自己的 `<repo>/../remuda-wt` 根下挑选（受管根常量 `path_guard.rs:50`，`worktree_root` 在 `:70`）。

mode 分流在 Hub：lease 路由服务 pool；reuse 的台账获取随 task 创建/派发完成。return 路由按 lease 行 `mode` 分流——`reuse` 只改台账、**不向 Node 发 RPC**；`pool` 且 refcount 归零时转发 `worktree.return`。

### 3.3 看板与归档路由

- `GET /v1/board?project=<projectId>`：独立只读端点，返回每个 task 的派生列（§5），与池层完全解耦。
- `POST /v1/tasks/{id}/archive`，body `{archived: boolean}`：只置位/清空 `archived_at`，不改 state。
- 拖卡迁移复用既有 `PATCH /v1/tasks/{id}`（`set_task_state`，`tasks.rs:327`），可多跳（§5.3）；land 仍只走 `POST /v1/tasks/{id}/land`（`tasks.rs:442`）。

### 3.4 派发 wire 无新增

`CreateInstanceBody` 已有 `taskId` / `projectId` / `worktree` / `taskSpec` 字段（`crates/remuda-hub/src/http.rs:44`，task 绑定字段在 `:120-129` 一带）；task 绑定经这些既有字段折进派发，不新增派发字段。

## 4. 既有回收路径必须 lease-aware

`refcount > 0` 时拒绝物理删除的护栏不能只加在新 `/return` 上——实际删除主路径是按 worker **name** 的无条件强删，今天不查任何 lease：

```
retire_worker（crates/remuda-hub/src/workers.rs:1284）
  / delete_instance（http.rs:473；store.rs:2093）
→ worker.remove_worker（crates/remuda-node/src/worker.rs:217，typed 入口 :226）
→ remove_record（worktree.rs:153；其内 git worktree remove --force 在 :170）
```

`remove_record` 的 `--force` 与 `:168-169` 注释记录的是**单 worker retire 语义**（worker 遗留未跟踪文件时强删是既定 retire 行为）；它不是多 task 共享安全的证据。共享 slot（refcount>1）被这条路径触发会误删别的 task 的载体。

规则：上述四条路径在物理删 pool slot 前必须先查 `worktree_leases.refcount`：

- `refcount > 0` → 不物理删目录，改为 return/park（reuse 绑定下本就零操作）；
- `refcount = 0` 且 task 已归档 → 既有 force-remove / 回收字节数语义不变；
- 任务 2 的回归断言：对被多 task 租借的 slot 发普通实例 `worker.remove`，目录不得被物理删除。

## 5. 看板投影

### 5.1 8 态 → 4 列（只读，纯派生）

独立读端点 `GET /v1/board` 返回派生 `boardColumn`。8 态机仍是唯一权威（迁移只由 `can_transition_to` 强制，`task.rs:52-64`）；`boardColumn` 永远不是可写的第二状态，也不存储。

| 列 | 由哪些 `TaskState` 投影 |
|---|---|
| 待办 | `pending` / `placed` / `deferred` / `parked` / **`failed` 且 `placement == None`** |
| 进行中 | `running` / `stalled` / **`failed` 且 `placement != None`** |
| 已完成 | **只有 `done`** |
| 已归档 | `archived_at != null`（正交维度；任意列的 task 都可归档，UI 作过滤器/分组，不作第五列） |

### 5.2 `failed` 的确定归属：按 `placement` 派生 + 角标

`TaskState` 没有历史字段（无 `prev_state`），且 failed 可从 pending/placed/running/stalled/parked/deferred 任一态进入（`task.rs:52-64`），无法反推失败前所在的列。因此**不引入 pre-fail 列存储**（那会突破 §10 迁移预算），改用从既有存储可派生的固定规则：

- `placement.is_some()`（`Task.placement`，字段在 `task.rs:225`；`TaskPlacementRef` 在 `:173`）→ failed 落**进行中**；
- `placement == None` → failed 落**待办**；
- 卡片在该列内叠加红色 **failed 角标**，机读理由携 `blockedReason`（字段 `task.rs:232`）。

角标是列内视觉叠加：不是第五列、不折进已完成。任何 failed task 都有确定列可放。

### 5.3 拖卡 = 列→列映射到目标 state（可多跳）

列与 state 是多对多，列边界与 `can_transition_to` 不对齐（例如 `pending` 只能 → `placed/deferred/failed`，不能直接 → `running`；而同在待办列的 `parked` 可直接 → `running`，`deferred` 需先回 `placed`）。规则：

- 每张卡的**可拖性由当前 state 决定**：UI 用 `can_transition_to` 预计算该卡合法可达的目标列；不可达列在拖拽中禁用并提示，不是静默拒绝。
- **待办 → 进行中**目标 state = `running`：对 `pending`/`deferred` 卡走多跳序列（`pending→placed→running` 或 `deferred→placed→running`），对 `placed`/`parked` 卡单跳 → `running`。每跳都是合法 `PATCH /v1/tasks/{id}`；任一跳非法则整体拒绝并复位。
- **进行中 → 已完成**目标 state = `done`（仅 `running`/`stalled` 可达）。done 是声明；land 另走 gate。
- **回拖（进行中 → 待办）**按 `can_transition_to` 选最近合法态（如 `running→parked`）。
- 归档不是列迁移，只走 `POST /v1/tasks/{id}/archive`，不改 state。

### 5.4 硬规则 I1：已完成列永不解锁依赖

不变量 I1（`task.rs:49-51` 注释）：`done` 是声明不是 gate；依赖边只由 `landed_sha` 解锁。`unmet_deps` 只看依赖 task 的 `landed_sha.is_none()`（`task.rs:329-335`）。看板把卡拖进已完成列不解锁任何下游；UI 不暴露 land 动作，land 只走 gate/`POST /land`（`tasks.rs:442-465`）。

## 6. 门控：grant 动词，不是「agent 一律 403」

- task 创建与状态迁移门控 **`GrantVerb::Dispatch`**：`create_task`（handler `tasks.rs:256`，门控在 `:261`）与 `set_task_state`（`tasks.rs:327`，门控在 `:333`）都经 `require_dispatch`（`tasks.rs:203-208`），其核心是 `require_grant(state, device, GrantVerb::Dispatch)`（`tasks.rs:208`）。
- land 门控 **`GrantVerb::Land`**：`land_task`（`tasks.rs:442`）内 `require_grant(.., GrantVerb::Land)`（`tasks.rs:449`）。
- `require_grant` 的语义（`crates/remuda-hub/src/agent_scope.rs:104-127`）：非 Agent 来源直接放行；Agent 来源必须在其 instance 持有的 grants 中逐字持有该动词，否则 403。枚举定义在 `crates/remuda-protocol/src/enums.rs:106`。
- 一个**被派发的项目协调员 agent 本身也是 Agent 来源**（`agent_scope.rs:680` 注释「a launched coordinator is always agent」，另见 `:168`）——持 dispatch/land grant 且在 project scope 内，它就能例行创建/迁移/落地 task；无 grant 或越 scope 才 403。
- 「Human-only、agent 403」的措辞**只保留给 D-017 真正限制的动词**：跨实例控制、跨 host create、agent keys。task 写操作不在其列。
- 非法状态迁移由 Hub 按 `can_transition_to` 拒绝，UI 预先禁用不可达列；这是状态机权威，不是身份门。

## 7. 任务空间 vs 项目空间：过滤投影，无新端点

文件视图契约（[files-view-contract.md](./files-view-contract.md)）的只读 SCM RPC **已经实现**：`workspace.scm.status/diff/file`（`crates/remuda-node/src/workspace_scm.rs:37-53`，返回 `ok/unsupported/denied` 可用性在 `:170-176`），经显式 dispatch 臂（`runtime_link.rs:169-172`），Web 已消费（`web/src/features/files/filesViewModel.ts:75-83`、`filesApi.ts:43`）。只有 `Workspace` 实体本身仍写死 `repository: NotApplicable`（`crates/remuda-node/src/runtime.rs:3055-3056`），但文件视图走独立 SCM RPC，不经过那个实体字段。

- **项目空间** = 既有文件视图，轴 `hostId+workspaceId` 不变（契约 §3.3），即 worktree 全树。
- **任务空间** = 对同一视图的**客户端过滤投影**：按 task 的 session 集合（`instances.task_id`）加 `owns[]` glob 过滤 status/diff 条目。空投影显示「还没有文件」，不合成条目、不新增 Node/Hub 端点、不新建草稿文件系统。
- 非 git / 不可用 / 离线 / 权限不足等落到契约的六个可用性状态（契约 §3.6），原样透传，不伪造基准。

## 8. 批注：两级载体，设备本地草稿

- **两级载体**：卡片级（挂 task 卡）或消息内锚点（选中详情/transcript 正文生成 `①`）。
- **随发送投递**：composer 上方显示「本次发送带 N 条批注」徽标；发送时批注作为结构化前缀拼进下一条 prompt（brief-as-file 风格，沿用 D-032 把 brief 作为附件/结构化文本投递的先例），**不新增协议事件流、不新增表/字段**，也不把看板协议文本塞进 agent 的系统提示词。
- 草稿设备本地、发送后清零；终端段不提供批注；从看板进入的只读预览不可批注（ui-spec §2.9）。

## 9. 人向表面（UI 细节以 ui-spec 为准）

- **任务列表**（桌面与 `/m`）：首组「需要你·N」（待人类 Interaction / blocked 待所有者，关联既有 `/m/inbox` 与 `blockedCount`），其余按 project + git branch 分组，组内按 `parentTaskId`（`task.rs:203`）嵌套父子任务；每行显示派生的每项目人读 key（display-only，形如 `SE-nn`，由 project 内 task 顺序派生，**无 schema 改动**）；归档折进「已归档·N」。
- **看板卡片**：消费 §5 投影；卡内 session 行（模型字形 + 相对时间）、`SE-nn` key、当前 profile 的 `default` 标签（config-reuse 子集，无新存储）、footer「与 N 个 task 共用」（lease 行 refcount）、failed 红角标 + `blockedReason`。
- **工作台**：一个 task 的多 session 用 tab/卡片（复用 `visibleTabs`/`spaceSessions`，`web/src/features/spaces/store.ts:93-108`），点开进共享 `/s/:id`；**不做并排多 pane 分屏**（ui-spec §1.3）。
- **看板详情 = 只读预览**：composer 禁用，标「预览模式·在工作台打开以完整操作」，完整控制在 `/s/:id`。
- **手机（D-049）**：`/m` home 在既有 project+branch 分组上叠 task 分组；看板塌缩为单列分段过滤（同一投影，一次一列）；会话本体与文件视图仍走共享 `/s/:id`、`/s/:id/files`，不造第二份 transcript。
- **项目切换器**：Web 采纳 Hub `Project` 实体（`crates/remuda-protocol/src/project.rs:483`，`members[]` 在 `:503`；`list_projects` 在 `crates/remuda-hub/src/projects.rs:167`），顶栏 全局▸project 按 projectId 过滤列表/看板；`members[]` 的 `(hostId, workspaceId)` 直接是 Space 键，不放松 D-024。
- **搜索**：task/board 搜索复用既有会话过滤机制（`web/src/features/session/sessionFilters.ts`），不新造搜索栈；跨 session 语义搜索不做（§12）。

## 10. 迁移预算（硬护栏）

存储增量严格封顶为：

1. **一个增量可空列** `archived_at`（迁移落 `tasks.rs:143` 的 `migrate()`，非 `store.rs`）；
2. **一张新表** `worktree_leases`（含 `mode` / 复合键 / 可空 `worktree_name` / `holder_instance_id`，仍是一张表）。

其余全部走零迁移载体：`workspaceBinding` 进既有 `doc_json`（serde default）；catalog 的 `state`/`leasedBy` 进 JSON 目录册（serde default）；`boardColumn` 与 `SE-nn` 只读派生、不存储；failed 列按 `placement` 派生、不加 pre-fail 列；批注零存储。任何超出此预算的存储改动需单开决策。

## 11. 与既有决策/不变量的核对

| 依据 | 既有约束 | 本模型的遵守方式 |
|---|---|---|
| D-024 | Space = `(hostId, workspaceId)`，同名/跨主机目录不合并 | 「共用」用 task 层 lease 行的 refcount（复合键 `host_id,workspace_id,dir_key`）建模；Space 键一字不改，`Project.members[]` 仍引用该键 |
| D-033（I1） | done 是声明；依赖只由 landed sha 解锁（`task.rs:49-51`、`:329-335`） | 已完成列只投影 `done`；拖卡入列不写 sha、不解锁；land 只走 gate/`/land` |
| D-035 | 显式 carrier、拒绝不静默替换、记录实跑值 | 池满/脏 slot/分支冲突显式 `blocked`/429 `SUPPLY_DEFERRED`；不静默 reroute、不换目录、不复活主 checkout |
| D-047 | refuse-never-reroute 的供给语义 | lease 拒绝复用同一 `blocked{reasons[]}` / `SUPPLY_DEFERRED` 形状；池决策不触碰 API 路由子模式 |
| D-049 | `/m` 只拥有首页级 IA；会话本体不分叉 | 看板在 compact 塌缩为单列分段过滤；task 分组叠在 `/m` home 上；会话与任务空间都走共享 `/s/:id`、`/s/:id/files` |
| ui-spec §1.3 | 一实例一终端，v1 不做多 pane 分屏 | task 的多 session 是 tab/卡片；参考产品的并排多面板不抄 |
| D-031 | 不装不探隧道工具、不监听非 loopback | 池只跑本地 git worktree 与目录册读写，不开端口、不做端口探测 |
| security-review-2 M4 | path/repo 不过 wire | lease/return 转发参数白名单 `{hostId,workspaceId,name,base,taskId}`（§3.2） |
| files-view-contract | 文件视图轴与六可用性状态 | 任务空间是该视图的客户端过滤投影，不新增端点（§7） |

## 12. 显式不做

- 不做 side-by-side 多 pane 分屏工作台；task 多 session 用 tab。
- 不新增 agent loop、不造新 harness（D-002）；不新增批注 wire/表；批注走设备本地草稿。
- 不做常驻可调用 agent 身份（与 D-017 冲突）；本轮只采 config-reuse 子集（ProviderProfile + composer 三元组），卡片 `default` 标签是其可见面。
- 不做看板优先级过滤、标签过滤（需要新排序/管理维度；后续可落 `doc_json`）；不做可枚举模板库/模板过滤。
- 不做跨 session 语义搜索/知识问答式搜索；不做持久稳定 task 序号存储（`SE-nn` 仅 display-only 派生）。
- 不把 done 当依赖解锁；不放松 Space 键；不静默降级或改道。
- 不开端口、不做隧道（D-031）；不超 §10 迁移预算。

## 13. 合规护栏（人工审查门）

脚本扫描（`secret-scan.sh` / `no-tunnel-scan.sh`）不覆盖产品名与文档引述，因此以下是人工审查项，适用于本批次每个入库文件（设计文档、ADR、ui-spec、证据）：

- 只用泛称描述参考产品（如「某参考看板产品」）；开源项目（如 herdr、paseo、vibe-kanban、Codex）可具名引用，**内部产品名一律不写**。
- 不粘贴、引用或转述任何内部文档（内部文章、内部 wiki）；工程理由以本仓代码与本文件的自有表述给出。
- 不写公司主机名（既有约定的 `devbox`/`devbox-sg` 占位除外）、用户名、主目录路径；JSON 示例用 `hst_1`/`wsp_1`/`<repo>` 一类占位。
- 证据截图只提交 Remuda 自身渲染（390 或 1440 宽），绝不提交参考产品截图。
