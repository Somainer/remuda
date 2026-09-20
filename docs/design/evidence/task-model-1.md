# Task 优先模型 · 规格与 ADR 修订（D-050）

2026-09-20 · `wt/c-tspec/b-tspec-md` · 任务 c-tspec（task-model 实施计划 §(C) 任务 1 t-spec，docs-only，批次内第一个合）

本任务把 task 优先层、看板投影、目录绑定、池/租借策略、批注草稿、任务空间投影写进设计权威，使后续实施任务（t-pool / t-bind / t-board-api / t-tasklist / t-board-ui / t-taskspace / t-project-switcher / t-annotations）不再与 `ui-spec.md` / `decisions.md` 打架。**不实施任何代码**：`web/` 与 `crates/` 零改动。

派工计划（task-model 计划）本身**不在仓库里**，本文件按名字引用它、不建链接；入库后的机械权威是新增的 [task-model.md](../task-model.md)。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `docs/design/task-model.md` | **新增**：入库设计文档（实体模型、reuse\|pool 绑定规则与 attach-lock、池 lease/return 策略与精确 Node RPC/Hub 路由形状、回收路径 lease-aware、看板投影、grant 门控、任务空间过滤投影、批注草稿、迁移预算、冲突核对、合规护栏） |
| `docs/design/ui-spec.md` | v0.2.3 changelog；§1.1 projects 行 +「项目」段改为 Hub `Project` 实体；§1.2 路由表新增 `/board`；§1.3 映射表补看板 compact 落点；新增 §1.5（Task 层）；新增 §2.9（任务列表与看板 `/board`）；§4.7 `/m` 子树表补任务分组与单列看板；§5.1 vibe-kanban 条补「看板是项目维度表面」限定 |
| `docs/design/decisions.md` | 顶部索引表追加 D-050 一行（D-049 之后）；文末追加 `## D-050` 正文段（11 条决策 + 背景 + 由谁 + 依据，同 D-034 以来各 ADR 形状） |
| `docs/design/coordinator-hierarchy.md` | 抬头相关决策行补 D-050；新增 §2.6 修订（Task/Project 台账向人向表面浮现） |
| `docs/design/evidence/task-model-1.md` | 本文件 |

## 2. 旧 → 新逐条对照

### 2.1 `task-model.md`（新文件）

| | |
|---|---|
| **旧文** | 无。派工计划 §(B) 设计只存在仓库外 |
| **新文** | §0 一句话判据；§1 实体模型（Task 两增量、Instance 无改动、`worktree_leases` 全表 DDL、catalog serde 增量、Annotation 非表非 wire）；§2 reuse/pool 绑定规则 + attach-lock 语义表；§3 Node RPC/Hub 路由/看板路由精确形状与 M4 参数白名单；§4 四条回收路径 lease-aware；§5 8→4 投影表、failed 归位、拖卡多跳、I1；§6 grant 动词门控；§7 任务空间过滤投影；§8 批注；§9 人向表面摘要；§10 迁移预算；§11 冲突核对表；§12 显式不做；§13 合规护栏 |
| **为何需要** | 任务 2–9 需要一份入库的机械权威（表结构、线形状、行号锚点），ADR 钉边界、ui-spec 钉界面，三者分工同 mobile-ui 批次 |

### 2.2 `ui-spec.md`：§1.1 projects 行与「项目」段

| | |
|---|---|
| **旧文** | §1.1 表 projects 行 =「Workspace（cwd / worktree）通讯录，按主机分组」；§1.1 段：「『项目』是 UI 文案，**不是**独立实体、也不是 vibe-kanban 看板。协议实体是 **Workspace**」 |
| **新文** | projects 行 =「Hub `Project` 实体（任务看板、任务列表、项目切换器；`members[]` 引用 Space 键，D-024/D-050）」，落点加 `/board` 与顶栏切换器；段落改为「『项目』对应 Hub **`Project` 实体**（D-050 起 Web 采纳）」，并保留「Workspace 仍是已注册目录实体、Project 引用 Workspace 而不替代它」 |
| **为何这样写** | 不写这处，任务 8（project-switcher）与任务 6（board-ui）直接与规格「项目不是实体」矛盾；同时写明 D-024 不放松，避免被读成放松 Space 键 |

### 2.3 `ui-spec.md`：路由表与 compact 映射

| | |
|---|---|
| **旧文** | §1.2 无 `/board` 路由；§1.3 映射表无看板行；`/projects` 行备注「项目列表（Workspace）」 |
| **新文** | §1.2 新增 `/board`（query `?project=`，compact 由 `/m` 单列分段承载）；`/projects` 行改为 Hub `Project` 实体；§1.3 映射表在「多 pane 分屏 → v1 不做」后加「看板（桌面三列）→ compact 不复制，塌缩为 `/m` 单列分段过滤」 |
| **处理方式** | 沿用 D-049 既有重定向层的形状（桌面形态 compact 落 `/m`，不新增第二套壳）；不新增 `/m/board` 路由——看板 compact 形态是 `/m` 子树内的分段过滤 |

### 2.4 `ui-spec.md`：新增 §1.5 Task 层

| | |
|---|---|
| **旧文** | §1.4 之后直接进 §2；规格没有 task 聚合、看板列、目录绑定、任务空间、批注的任何条文 |
| **新文** | 八条界面口径：①Task 是 instance 之上聚合、非第二状态机；②一 task 多 session 用 tab、不做分屏；③看板列只读投影；④reuse\|pool 绑定与显式 blocked；⑤项目空间 vs 任务空间（过滤投影、空态「还没有文件」）；⑥批注=本地草稿；⑦/m 任务分组与会话不分叉；⑧看板是项目维度表面、会话仍是主导航。机械细节指向 task-model.md |
| **为何这样写** | task 层是横切 IA（左栏、看板、右栏、/m、composer），需要一个 §1.x 总纲；具体屏幕形态放 §2.9，避免在多处重复规则 |

### 2.5 `ui-spec.md`：新增 §2.9 任务列表与看板 `/board`

| | |
|---|---|
| **旧文** | §2 关键屏只有会话/审批/主机/Provider/Bot 等 8 屏；`SessionList` 的 `GROUPS` 是**实例**状态分组（`SessionList.tsx:43`），规格未定义任何 task 卡片屏 |
| **新文** | 桌面看板 ASCII 线框；任务列表分组层次（「需要你·N」首组 → project+branch → 父子嵌套 → 已归档折叠）；`SE-nn` 派生 key；看板三列投影与 failed 角标；卡面字段（session 行、`default` 标签、「与 N 个 task 共用」footer）；拖卡可拖性预计算 + 多跳；grant 门控的 UI 姿态（不假设 agent 一律禁写）；看板详情只读预览 +「在工作台打开」；右栏 详情/文件（项目空间·任务空间）/变更 三 tab；批注两级载体与徽标；项目切换器；compact 映射回 §4.7 |
| **处理方式** | 全部按派工计划 §(B) B.4/B.5/B.6/B.7/B.8 落地为规格条文，但不引述任何仓库外内部材料；理由以代码现状（file:line）与自有表述给出 |

### 2.6 `ui-spec.md`：§4.7 `/m` 子树表与 §5.1

| | |
|---|---|
| **旧文** | §4.7 子树表 `/m` home 行只有「项目 + git branch 组头」；§5.1「vibe-kanban……不要把主 IA 做成看板」无条件句 |
| **新文** | home 行补「叠加 task 分组层（需要你/父子/SE-nn/归档）」；新增「看板的 compact 形态」行（同一 board 投影、一次一段、不复制三列）；§5.1 改为「借看板列投影形态（D-050 的 /board 是项目维度表面），但不要把主 IA 做成看板（会话仍一等导航）」 |
| **处理方式** | 与 D-049「`/m` 只拥有首页级 IA、会话本体不分叉」同向增补；§5.1 旧句保留其判断（主 IA 不是看板），只补新表面的位置 |

### 2.7 `decisions.md`：索引行 + 正文

| | |
|---|---|
| **旧文** | 最新一条为 D-049（手机优先 UI）；无 task 聚合/池/lease/看板投影的任何决策 |
| **新文** | 顶部索引表 D-049 后追加 D-050 一行（摘要含全部硬约束）；文末追加 `## D-050`：日期/状态/相关表 + 背景（带现状 file:line）+ 11 条决策 + 由谁 + 依据。机械细节显式指向 task-model.md，ADR 只钉边界与编号归属 |

### 2.8 `coordinator-hierarchy.md`：§2.6 修订

| | |
|---|---|
| **旧文** | §1.2 非目标 2 写「本轮不做 Web UI」（限定 §8 coordinator 批次）；Task 在 §6 缺口表标 GAP、Space 标 UI-only、Projects 页标 MOCK |
| **新文** | 新增 §2.6（2026-09-20 修订，同 §2.5 的修订体例）：task-model 批次把 Task/Project 台账浮现给人；看板列是只读投影、台账权威不变；门控按 §2.5 的 grant/scope 读法（Dispatch/Land，非 human-only）；目录共用建模为 task 层 refcount 不合并 Space；§6 三行缺口由任务 4–8 接续关闭、不回改原表；抬头相关决策行补 D-050 |

## 3. 与 D-024 / D-033 / D-035 / D-047 / D-049 的逐条无冲突对照

（计划任务 1 验收第 4 条；同表亦在 D-050 第 10 条与 task-model.md §11）

| ADR | 既有决定（一句话） | 本批新增 | 关系判定 |
|---|---|---|---|
| **D-024** | Space = `(hostId, workspaceId)`；同名/跨主机目录不合并；Space→Tabs 不引入 panes | 「与 N 个 task 共用」建模为 `worktree_leases` 同复合键 `(host_id,workspace_id,dir_key)` 的 refcount + attach-lock；`Project.members[]` 仍是 Space 键元组；一 task 多 session 用 tab（ui-spec §1.5/§2.9） | **遵守，不放松**。共用是 task 层顺序轮用计数，Space 键一字未改；pane 禁令重申 |
| **D-033**（I1） | done 是声明不是 gate；依赖只由 `landed_sha` 解锁 | 已完成列只投影 `done`；拖卡入列只发 state PATCH、不写 sha；UI 不暴露 land；failed 角标不折进已完成；task-model §5.4 + ui-spec §2.9 明文 | **引用，不改写**。状态机权威与解锁规则原样 |
| **D-035** | 显式 carrier；拒绝不静默替换；记录实跑值 | 池三决策的第三态是显式 `blocked{reason}`/429 `SUPPLY_DEFERRED`；目录忙排队/blocked；不 reroute、不换目录、不复活主 checkout；carrier 选择次序未触碰 | **同向复用同一拒绝形状**。无静默降级路径新增 |
| **D-047** | 模型 API 交付可选 via；refuse-never-reroute；D-031 例外条款 | lease/池决策与 API 路由子模式完全无交集；拒绝复用 `SUPPLY_DEFERRED`/blocked 形状；池只跑本地 git，不开端口、不起 relay | **无交集 + 同形拒绝**。D-047 的中继/隧道边界不受影响 |
| **D-049** | 受限 `/m` 树；会话本体共享 `/s/:id` 不分叉；重定向层 | 看板 compact 塌缩为 `/m` 单列分段过滤（同一 board 投影）；任务分组叠在 `/m` home 既有 project/branch 组上；任务空间走共享 `/s/:id/files` 内 tab；批注手机只读 | **受限增补**。不新增第二份看板/transcript/文件实现 |

另核对（计划要求同列遵守）：**ui-spec §1.3** 一实例一终端/不分屏——§1.5/§2.9 明确 tab 化；**D-031**——池无端口/隧道；**D-002**——无新 harness/agent loop，批注零 wire；**files-view-contract**——任务空间是其只读 SCM 视图的客户端过滤，轴与六可用性状态不变；**security-review-2 M4**——lease/return 参数白名单 `{hostId,workspaceId,name,base,taskId}`。

结论：五条 ADR 均无需修订原文；本批是在其边界**之上**加 task 聚合层。

## 4. 引用 file:line 复核（基线 `1468a2ba`，逐条重验）

规格正文与 ADR 出现的每条代码锚点都在本工作树（`1468a2ba`）上用 `grep -n`/`sed` 逐条复核后才引用；本分支不改代码，行号不会在本批内漂移。派工计划给的行号有少量漂移，**入库文档一律引用右列实测行号**。

| 引用（入库文档使用） | 验证到的内容（`1468a2ba`） | 计划行号 |
|---|---|---|
| `crates/remuda-protocol/src/task.rs:16-23` | 8 态 `wire_enum!`（Pending `:16` … Deferred `:23`） | 16-23 ✓ |
| `task.rs:52-64` | `pub fn can_transition_to`（`:52`）到 match 结束 | 计划 :41-63，已漂移 |
| `task.rs:49-51` | I1 注释「done is a claim, not a gate」 | 计划 :47-51，微调 |
| `task.rs:67-69` | `is_terminal` 只认 Done/Failed | 计划 :66-70，微调 |
| `task.rs:173-190` / `:182` | `struct TaskPlacementRef`（`:173`），`instance_id`（`:182`） | 计划 :180 |
| `task.rs:194` | `pub struct Task` | 194 ✓ |
| `task.rs:203` | `parent_task_id` | 计划 :198 |
| `task.rs:225` | `placement: Option<TaskPlacementRef>` | 计划 :213-215 |
| `task.rs:232` | `blocked_reason` | 计划 :230-233 |
| `task.rs:329-335` | `unmet_deps` 过滤；`landed_sha.is_none()` 在 `:334` | 计划 :331-341 |
| `crates/remuda-hub/src/tasks.rs:143` / `:147` / `:154` | `migrate()` / `CREATE TABLE IF NOT EXISTS tasks` / `doc_json TEXT NOT NULL` | 143/147/154 ✓ |
| `tasks.rs:185-199` | task 路由表（`/v1/tasks`、`/split`、`/land`、`/own`、`/placements`、`/own/check`） | 计划 :186-198 |
| `tasks.rs:203-208` | `require_dispatch` 内 `require_grant(..Dispatch)`（授权调用 `:208`） | 208 ✓（函数起点 :203） |
| `tasks.rs:256` / `:261` | `create_task` handler / 门控行 | 计划 :946 为内层 store 方法（`:946` 另在） |
| `tasks.rs:327` / `:333` | `set_task_state` / 门控行 | 327 ✓ |
| `tasks.rs:442-465` / `:449` | `land_task`；`require_grant(..Land)` 在 `:449` | 449 ✓ |
| `tasks.rs:641-679` | `/v1/own/check` 文档注释（`:641`）到 `check_own` 响应结束；纯函数路径对账 | 641-679 ✓ |
| `crates/remuda-hub/src/agent_scope.rs:104-127` | `require_grant`：非 Agent 放行；Agent 须持有逐字 grant | 计划 :104-129 |
| `agent_scope.rs:680` | 注释「a launched coordinator is always agent」（另 `:168` 同义） | 680 ✓ |
| `crates/remuda-protocol/src/enums.rs:106-107` | `wire_enum!(GrantVerb …)`，Dispatch=`"dispatch"`（Land/Spend 紧随） | — |
| `crates/remuda-node/src/worktree.rs:12-19` | `struct WorktreeRecord`（name/path/branch/base） | 计划 :11-23 |
| `worktree.rs:21-26` | `Catalog` serde default 写法（catalog 增量照此） | 计划 :26-29 |
| `worktree.rs:29-36` | `handle_rpc` 的 `match`（今天仅 list/create 两臂） | 计划 :34-36 |
| `worktree.rs:60` | `provision_record` | 60 ✓ |
| `worktree.rs:73-86` / `:81` | 幂等复用块；条件 `existing.branch == branch`（`:81`） | 计划 :74-83 |
| `worktree.rs:95-99` | fetch-first 注释与 `git_fetch(&repo_root)?` 调用（`:99`） | 计划 :97-99 |
| `worktree.rs:101-105` | `ref_exists(branch)` 同名分支拒绝 | 计划 :100-104 |
| `worktree.rs:153-181` | `remove_record`（doc 注释始于 `:150`）；`--force` 在 `:170` | 计划 :152-186 |
| `worktree.rs:168-169` | `--force` 的单 worker retire 语义注释（非多用户安全证据） | 计划 :169-176 |
| `worktree.rs:185-205` | `git_fetch` 文档注释（`:185-186`）与函数（`:187`） | 185-205 ✓ |
| `worktree.rs:207-209` | `is_worktree_method` 只匹配 create/list | 207-208 ✓ |
| `worktree.rs:335-346` | `provisioned_worktree_named` 只按精确记录路径认名 | 计划 :335-352 |
| `worktree.rs:355-401` | `resolve_instance_cwd`；受管根集合 `:382-393`、`contain` 调用 `:388` | 计划 :355-410 |
| `crates/remuda-node/src/worker.rs:58` | `validate_branch`（`wt/<name>/<slug>` 形状） | 58 ✓ |
| `worker.rs:217` / `:226` / `:293` | `remove_worker` / typed 入口 / `remove_record` 调用点 | 217/293 ✓ |
| `crates/remuda-hub/src/workers.rs:1284` | `retire_worker` | 1284 ✓ |
| `crates/remuda-hub/src/http.rs:44` / `:120-129` | `CreateInstanceBody`；taskId/taskSpec 绑定字段区 | — |
| `http.rs:222` | `/v1/worktrees` 路由注册 | — |
| `http.rs:1783-1805` | `create_worktree`（doc `:1783`/fn `:1784`）；门控 `:1789-1790`；path/repo 不转发注释 `:1795-1796`、params `:1797-1802` | 计划 :1783/1788-1789/1797-1800 |
| `http.rs:473` | `delete_instance` | 473 ✓ |
| `crates/remuda-hub/src/store.rs:2093` | `Store::delete_instance` | 2093 ✓ |
| `store.rs:4430-4445` / `:4441` | instances/hosts 的 `ensure_column` 迁移区；`task_id` 列在 `:4441`（`archived_at` 不放这） | 计划 :4433/4441 |
| `crates/remuda-node/src/runtime_link.rs:164-166` | worktree 显式臂 `is_worktree_method → worktree_rpc_capped` | **漂移**：计划 :76-84 |
| `runtime_link.rs:169-172` | SCM 显式臂（`is_scm_method`） | — |
| `runtime_link.rs:178` | 兜底臂 `_ => dispatch_method`；注释（`:174-177`）说明未知方法今为 request-style 错误而非历史 `{"ok":true}` | **漂移**：计划 :76-84 |
| `crates/remuda-node/src/transport/hubnode_codec.rs:198-200` | `dispatch_method` 文档：「an unknown method is an error」 | 行为已硬化，计划描述的假成功是历史行为 |
| `crates/remuda-node/src/native.rs:700-703` | `trust_containment` 调 `provisioned_worktree_named` | 计划 :699-703 |
| `native.rs:3189` | trust-flag 测试 `a_provisioned_worktree_cwd_gets_the_trust_flag_and_bypass_answer` | 3189 ✓ |
| `crates/remuda-node/src/workspace_scm.rs:37-53` | `is_scm_method`（37-42）+ `handle_rpc` status/diff/file 三臂（51-53） | 37-42/55 ✓ |
| `workspace_scm.rs:170-176` | `ok/unsupported/denied` 可用性赋值 | 计划 :157/170/175 |
| `crates/remuda-node/src/runtime.rs:3055-3056` | `Workspace` 实体 `repository: NotApplicable` / `worktree: None` | **漂移**：计划 :1830-1862 |
| `crates/remuda-protocol/src/path_guard.rs:50` / `:70` | `WORKTREE_DIR="remuda-wt"` / `worktree_root` | 50/70 ✓ |
| `crates/remuda-protocol/src/project.rs:483` / `:503` | `pub struct Project` / `members: Vec<ProjectMember>` | 483 ✓；members 计划 :502 |
| `crates/remuda-hub/src/projects.rs:46` / `:167` | `/v1/projects` 路由 / `list_projects` | 计划 :44-53/167 |
| `crates/remuda-hub/src/error.rs:154` | `SupplyDeferred => "SUPPLY_DEFERRED"` | — |
| `web/src/pages/ProjectsPage.tsx:10` | 「Workspace 通讯录，不是独立项目实体」stub 文案 | 10 ✓ |
| `web/src/features/files/filesViewModel.ts:75-83` | `unsupported`/`denied` 可用性消费 | 75-83 ✓ |
| `web/src/features/files/filesApi.ts:43` | 取 `workspace.scm.status` | 43 ✓ |
| `web/src/features/spaces/store.ts:53` / `:73-74` / `:93-108` | `buildSpaces` / blockedCount、liveCount / `visibleTabs`(`:93`)、`spaceSessions`(`:102`) | 53/73-75/92-108，微调 |
| `web/src/features/mobile/homeRows.ts:171` / `:230` | `buildHomeGroups` / 组头 blockedCount | 171/229-230 ✓ |
| `web/src/features/session/sessionFilters.ts`、`web/src/lib/drafts.ts` | 既有搜索过滤机制、设备本地草稿先例（文件存在） | — |
| `web/src/app/router.tsx:62` / `:79-80` | `/s/:id/files` 路由 / `/m/inbox` 路由 | 62/79 ✓ |

两处相对计划的实质漂移已按本树写进入库文档：(a) 兜底臂不再回 `{"ok":true}` 而是未知方法错误（注册义务不变，任务 2 的断言形状仍成立：未注册到不了实现、注册后返回真实 payload）；(b) `Workspace` 实体写死行迁移到 `runtime.rs:3055-3056`（结论不变：文件视图走独立 SCM RPC，不经过该字段）。

## 5. 计划任务 1 四条验收对账

| # | 验收要求 | 落点 |
|---|---|---|
| 1 | D-050 明文记录：Task 聚合；workspaceBinding；lease/return/refcount + detached-HEAD + return-don't-delete（reset/clean/park 仅 pool、reuse 零操作）；lease 表 mode/复合键/可空 name；四条回收路径 lease-aware；failed 按 placement 派生 + 角标不存 pre-fail；8→4 投影；拖卡多跳；`archived_at`；唯一新表；批注草稿；任务空间过滤投影 | D-050 第 1-5、7 条；task-model.md §1-2、§4-5、§7-8；ui-spec §1.5/§2.9 |
| 2 | 门控 grant-verb 表述（Dispatch create/set-state、Land land；逐条锚点 tasks.rs:208/449、agent_scope.rs:104-127/680；持 grant 协调员授权；human-only 仅限 D-017 动词）+ 遵守 D-024/ui-spec §1.3/D-035/D-047/I1/D-049/D-031/合规规则 + M4 参数白名单 | D-050 第 6、9、10、11 条；task-model.md §6、§11、§3.2 |
| 3 | 迁移预算护栏：一列 `archived_at`（tasks.rs migrate）+ 一表 `worktree_leases`；binding 落 doc_json；catalog serde default；failed 派生不增存储 | D-050 第 8 条；task-model.md §10 |
| 4 | 与 D-024/D-033/D-035/D-047/D-049 无矛盾的冲突核对表 + 参考产品名合规护栏 | 本文件 §3；D-050 第 10、11 条；task-model.md §11、§13 |

## 6. 合规护栏对账（人工审查门）

- 五个入库文件对参考产品**只用泛称**（「某参考看板产品」），具名出现的仅为开源/既有调研对象（herdr、paseo、vibe-kanban、Codex——均为仓库既有文档已在使用的名字）；内部产品名零出现（`git grep -i` 复核为 0）。
- 无内部文档引述/转述；工程理由全部以本仓代码锚点与自有表述给出。
- 无公司主机名/用户名/主目录路径；JSON 示例占位为 `hst_1`/`wsp_1`/`<repo>`；线框中的 SE/task 文案为产品 UI 拟稿。
- 本任务无截图（docs-only，无产品渲染可截）；后续任务证据截图只提交 390/1440 的 Remuda 渲染，已写进 task-model.md §13 与 D-050 第 11 条。

## 7. 验证

| 检查 | 结果 |
|---|---|
| `bash scripts/ci/secret-scan.sh` | 输出 `secret-scan: pass`（无其他输出） |
| `bash scripts/ci/no-tunnel-scan.sh` | 输出 `no-tunnel-scan: passed`（无其他输出） |
| Markdown 链接 | 五文件互引（task-model.md ↔ decisions.md ↔ ui-spec.md ← coordinator-hierarchy.md ← 本文件）及既有文件链接均为同目录相对路径，按文件所在目录解析存在；§4 表内代码反引号路径非链接 |
| 代码锚点 | §4 表全部在 `1468a2ba` 实测；本分支零代码改动 |
| 与 D-024/D-033/D-035/D-047/D-049 冲突 | 见 §3：无一需修订原文 |
| vitest / e2e / cargo | 不适用（docs-only，`web/`、`crates/` 零改动；无端口分配） |
