# 文件 / diff 视图数据契约（工作台 G1）

- 日期：2026-09-14
- 状态：勘察与契约决策（G1，只产文档；不含代码变更）
- 源码基线：`origin/main` = `3da158d`
- 上游需求：[workbench-ux-exploration.md](./workbench-ux-exploration.md) §5 P2-1
- 执行计划：[workbench-ux-plan.md](./workbench-ux-plan.md) §1 批次 G、§4 风险 5
- 证据记录：[workbench-g-files-contract-1.md](./evidence/workbench-g-files-contract-1.md)

本文回答一个问题：会话页 `/s/:instanceId/files` 的文件 / diff 视图，**能由什么数据支撑，每一份数据到底能证明什么**。结论先行：

> 当前代码里**不存在**任何能证明“某次会话产生了某次文件改动”的结构化数据。唯一可诚实交付的一期视图，是在 Node 上**实时只读地**计算**注册工作区当前的 git 工作树状态**，标题恒为「工作区当前变更」，不归因给任何会话。该结论为 **GO**（范围严格收缩，见 §7）；任何基于 journal 的“本次会话改动”列表为 **NO-GO**。

---

## 1. 问题边界与现有 UI 事实

- 路由已存在：`/s/:instanceId/files` 直接渲染 `SessionPage` 的 files 分支（`web/src/app/router.tsx:29`）；视图类型含 `"files"`（`web/src/pages/SessionPage.tsx:32`）。
- 桌面入口按钮已存在（`web/src/pages/SessionPage.tsx:176-186`，`data-testid="files-toggle"`），正文中只有静态占位（`web/src/pages/SessionPage.tsx:326-341`，`files-pane` / `files-back`，占位文案在 `:339-340`），Esc 返回逻辑在 `web/src/pages/SessionPage.tsx:63-67`。
- 手机等价入口当前不存在：入口按钮挂 `.deskOnly`（`web/src/pages/SessionPage.tsx:180`），该类在 `@media (max-width: 767px)` 下隐藏（`web/src/features/session/session.module.css:1789,1809`）。全屏路由本身不依赖桌面布局，G2 需要在手机补入口而不是新路由。
- 上游明确禁止：不能用未经证实的“本次改动”列表替换占位（探索文档 §5 P2-1）；只有工作树状态可用时标题必须是「工作区当前变更」（计划 §4 风险 5）。

数据链路（探索文档 §2.1 的总体结构）：原生 CLI → driver/wire → journal 观测 → Hub → Web；工作区注册与文件系统只存在于 Node。文件视图的数据来源因此分两类：**journal 里已经发生过的观测**，和 **Node 此刻能从工作区读到的状态**。

---

## 2. 候选数据源勘察

### 2.1 Journal 工具调用记录（Claude：stream-json 与 transcript 同一 mapper）

Claude 的 `tool_use` / `tool_result` 被映射为统一观测 `tool_call` / `tool_result`（枚举：`crates/remuda-protocol/src/enums.rs:747-763`，标签在 `:750-751`）。有三条进入 journal 的路径，载荷形状相同：

1. 实时 stream-json 流式 mapper：`crates/remuda-driver/src/claude_print_stream.rs:87-108`；
2. 实时 `user` 帧里的工具结果：`crates/remuda-driver/src/claude_print.rs:893-934`；
3. 磁盘 transcript / 终端提升后的补水 mapper：`crates/remuda-journal/src/claude.rs:761-852`，其存在意义就是让 `shell-pty` 提升的会话用同一解析器补水（`crates/remuda-driver/src/claude_print.rs:1754-1760`）。

载荷事实：

- `ToolCallPayload`（`crates/remuda-protocol/src/observation.rs:279-303`）只保存：工具名、类别、**原样未类型化的 `input: Knowledge<Json>`**（`:294-295`）、`state`。文件路径没有独立字段，只活在 `input.file_path` / `input.command` 等 JSON 成员里；Web 现在也是从 JSON 里现取（`web/src/features/session/ToolCard.tsx:37,81,103,118`）。
- 类别仅按名字推断（`crates/remuda-journal/src/claude.rs:870-881`：`Write/Edit/NotebookEdit → FileWrite`、`Bash → Shell`、`Read → FileRead`），**类别不代表副作用发生**。
- `state` 在所有 Claude 路径上恒为 `Proposed`（`crates/remuda-journal/src/claude.rs:801`、`crates/remuda-driver/src/claude_print_stream.rs:105`）；流式期间 input 还是 `unknown("streaming-input")`（`claude_print_stream.rs:96-104`）。协议映射表本身写明“看到工具调用意图不证明已经执行”（`docs/design/protocol.md:956`）。
- `ToolResultPayload`（`observation.rs:320-338`）里 Claude 路径把三个本可证明事实的字段全部放空：`structured_result: unknown("not-emitted")`、`exit_code: unknown("not-emitted")`、**`changes: Vec::new()`**（`crates/remuda-journal/src/claude.rs:847-849`；实时路径 `crates/remuda-driver/src/claude_print.rs:929-931` 同样空）。结果正文被压成一段自由文本（`crates/remuda-journal/src/claude.rs:824,854-868`），只有 `is_error` 决定 `Succeeded/Failed`（`:820-823,841-845`）。
- 仓库内**没有任何一处**构造过非空的 `FileChange`：`changes: Vec::new()` 是全部构造点。

**能证明的：** 某个实例（instance）的某个工具调用**提到过**一个路径字符串（且 Edit/Write 的新内容、Bash 命令全文在 `input` 里）。
**不能证明的：** 文件被实际改动；改动是否落盘；改动后是否又被改回；路径是否越界；Bash 里重定向/脚本造成的改动；工具是否实际执行（proposed 语义）。

截断与体量：

- 结构化载荷与结果文本**没有截断标记字段**（`observation.rs` / `enums.rs` 中无任何 truncation/elided 字段）；唯一的“完整性”信号是映射保真度 `Completeness`（`enums.rs:441-446`），不是内容长度。
- 传输层只有**硬上限**：Claude NDJSON 单行 8 MiB，超过即读错误（`crates/remuda-claude-wire/src/codec.rs:10,50-63`，错误类型 `LineTooLong`，在 `crates/remuda-driver/src/claude_print.rs:1670` 被归类）；ACP 2 MiB（`crates/remuda-acp-wire/src/codec.rs:12`）；Codex app-server 16 MiB（`crates/remuda-codex-wire/src/rpc.rs:14`）。这是“整行拒收”，不是“截断保留”。
- 原始 JSONL 整行以内容寻址 blob 留存（`crates/remuda-journal/src/envelope.rs:50-60`，`crates/remuda-journal/src/blob.rs:24-85`），blob 层也没有大小上限。图片内联另有 3.5 MiB 上限（`crates/remuda-driver/src/claude_print.rs:1510`），但那管的是外发 prompt 附件，与观测记录无关。

归因：

- 每条观测带 `instance_id` / `journal_id` / `run_id?` / `host_id` / `seq`（envelope 构造见 `crates/remuda-journal/src/claude.rs:996-1034`；`seq` 由 store 在提交时按实例单调分配，`crates/remuda-journal/src/store.rs:419`）。实例归因可靠。
- 原生轮次归因不可靠：`native_turn_id` 在 Claude 路径上恒为 `unknown("not-emitted")`（`claude.rs:1012`；实时 mapper `claude_print.rs:1172` 同义）；轮次边界是投影期推导的，不是记录字段（`crates/remuda-journal/src/projection.rs:186-291`）。
- `native_session_id` 是关联属性而非键轴（`observation.rs` 的 `ObservationSource`，Claude 侧 `claude.rs:1011`）。一次原生会话跨多个实例（resume 链），journal 按实例分别留存——同一文件被 resume 后继实例改动时，无法从观测推出“属于哪一轮”。

### 2.2 协议里已定义但从未生产的“改动”通道

协议本身为文件改动预留了结构化通道，但当前代码一律没有生产：

| 协议字段 / 变体 | 定义 | 当前生产情况 |
| --- | --- | --- |
| `FileChange { path, diff, application }` | `crates/remuda-protocol/src/observation.rs:305-315`；`ChangeApplication = proposed|applied|unknown`（`enums.rs:539-543`）；TS 契约 `docs/design/protocol.md:864` | **零构造点**；`ToolResultPayload.changes` 永远为空（见 §2.1） |
| `artifact` 观测 / `ArtifactType::Diff` | `observation.rs:647-673`、`enums.rs:643-650` | 运行时从不构造（仅 schema 注册引用） |
| `ArtifactLocator.workspace-file`（workspaceId + relativePath + revision + digest） | `observation.rs:599-645`；授权语义见 `docs/design/protocol.md:941`（“可读 workspace-file 路径由 Node 按注册根解析，拒绝 `..`、越界 symlink 和跨 host 路径”） | 仅随 ArtifactPayload 可达，而 ArtifactPayload 无生产方 |
| `Workspace.repository / worktree` 实体字段（含 `head_oid`、`base_oid`、`dirty`） | `crates/remuda-protocol/src/entities.rs:94-145,150-179` | Node 从不填充，见 §2.6 |
| `input_digest` / plan digest | 审批交互上的**输入 JSON** 摘要（`crates/remuda-driver/src/claude_print.rs:1311`） | 是提案输入的 digest，不是文件内容 digest |

### 2.3 Claude 原生 `file-history-snapshot` / `file-history-delta` / `edited_text_file`

这是 Claude 原生记录里**唯一字面包含文件检查点**的来源，但当前被刻意不解析：

- transcript mapper 把 `file-history-snapshot` / `file-history-delta` 映射为 **opaque**（`Completeness::Opaque` + `OpaqueReason::UnmappedFields`），只保留整行 raw blob（`crates/remuda-journal/src/claude.rs:591-596`）；`edited_text_file` 同为 opaque/partial（`:578-583`）。
- 补水 mapper 的注释明示这类记录“map to nothing”（`crates/remuda-driver/src/claude_print.rs:1759-1760`）。
- 协议设计表对它的定性是“文件 checkpoint 不等于当前文件内容或任务通过”（`docs/design/protocol.md:996`）；`edited_text_file` “无确切 tool ID/应用证明时不标 applied”（`docs/design/protocol.md:988`）。

**结论：** 这是最有潜力的二期来源（有历史快照语义），但今天没有结构字段、没有版本化解析、没有与工具调用/工作树的对账，不能作为 G2 依据；raw blob 留底意味着未来可以在不改写旧 journal 的前提下补解析器。

### 2.4 Codex：rollout `custom_tool_call` 与 app-server `fileChange`

- 磁盘 rollout 解析器**能**解析 `custom_tool_call` / `local_shell_call` / 对应 output（`crates/remuda-driver/src/codex_rollout.rs:72-88,226-240`），但文件头自述“This pre-spike is deliberately not connected to a driver”（`crates/remuda-driver/src/codex_rollout.rs:1-5`）。当前唯一消费者是 token 用量提取（`crates/remuda-driver/src/usage/codex.rs:23-64`）与 fake-harness 测试夹具；没有任何代码把它映射成 `ObservationPayload`。
- `remuda-codex-wire` crate 完整建模了 app-server 协议，包括 `turn/diff/updated`、`item/fileChange/patchUpdated` 与 `TypedThreadItem::FileChange`（`crates/remuda-codex-wire/src/types.rs:584-595`，协议映射表 `docs/design/protocol.md:1036,1045-1047`），但**没有任何 crate 依赖它**（workspace 成员之外，`crates/*/Cargo.toml` 中零依赖；workspace 声明在根 `Cargo.toml:6-7`）。
- 实际运行的 Codex 走 `GenericPty` / `ShellPty`（允许配对见 `crates/remuda-node/src/runtime.rs:1525-1550`；`(Codex, GenericPty)` 在 `:1534-1535`），而 `generic_pty` 只发射 lifecycle 观测加原始 TTY 字节（`crates/remuda-driver/src/generic_pty.rs` 内 `ObservationPayload::` 全部为 `Lifecycle`）。

**结论：** Codex 会话今天没有结构化工具/文件观测；即便 wire 层的 `fileChange` 区分 proposed 与 applied（`docs/design/protocol.md:1045-1047`），也没有接线，不能用于 G2。

### 2.5 Grok：ACP `tool_call` / `tool_call_update`

- ACP 帧分类器认识 `tool_call` 与 `tool_call_update`（`crates/remuda-acp-wire/src/types.rs:86-88,113-114`）。
- driver 里的 Grok 解析同样是“deliberately not connected to a driver”的预研（`crates/remuda-driver/src/grok_session.rs:1-6`），且只抽取**关联 ID**，不抽取工具名/输入/结果/路径（`grok_session.rs:58-66,115-119`）；消费者只有测试与 fake-harness。
- 实际 Grok 会话走 `GrokAcp` 枚举或落到 `GenericPty` / `ShellPty`（配对矩阵 `runtime.rs:1531-1532,1536,1547`），运行时没有把 ACP tool_call 映射为 journal 观测的代码。

**结论：** Grok 会话今天无任何文件相关结构化观测。

### 2.6 Node 工作区登记数据与 git 状态

- 工作区注册表持久化在 Node 的 `workspaces.json`（`crates/remuda-node/src/workspace.rs:15-21,56-59`，0600 原子写 `:167-196`），允许根来自 `workspace_roots` 配置或 `$HOME`（`:43-55`）。
- 注册即做完整规范化：`canonicalize` 解析 symlink，再要求位于允许根之内（`workspace.rs:100-134`，核心判定 `:102`；实现 `:346-373`），并排除注册仓库与其 `remuda-wt` 兄弟目录互相嵌套（`:114-131,377-432`）。共享的低层护栏在 `crates/remuda-protocol/src/path_guard.rs`：词法规范化拒绝相对路径/越界 `..`（`:164-185`）、只对已存在前缀 canonicalize（`:130-160`）、`contain` 沿 symlink 判定包含（`:93-112`，逃逸拒绝的测试 `:241-255`）、`safe_segment`（`:56-67`）、`worktree_root = <repo>/../remuda-wt`（`:69-76`，常量 `:50`）。
- 实例 cwd 准入是“证明请求路径位于注册目录”的现成范式：按 id 选工作区 → 重新校验存在性与 canonical 身份 → `~` 展开、相对根拼合 → 再 canonicalize → 必须 `starts_with` 注册根或其 worktree 边界（`crates/remuda-node/src/workspace.rs:484-558`；配套的可杀子进程 FS 探针在 `crates/remuda-node/src/workspace_access.rs:52-71,132-216`，3 秒超时，含 macOS TCC 拒绝语义）。
- **登记数据不含 git：** Node 构造 `Workspace` 实体时写死 `repository: NotApplicable`、`worktree: None`（`crates/remuda-node/src/runtime.rs:1830-1862`，关键行 `:1854-1855`），实体上的 `head_oid/base_oid/dirty/branch`（`entities.rs:94-145`）从不填充。上报给 Hub 的快照进一步裁剪为 `{workspaceId, hostId, root}`（`workspace.rs:94-98`；Hub 侧只校验这三个字段 `crates/remuda-hub/src/workspaces.rs:187-194`；协议投影 `RegisteredWorkspace` 在 `crates/remuda-protocol/src/hubnode.rs:188-197`；Web 映射 `web/src/features/workspaces/registry.ts:5-12`，类型 `web/src/types/workspace.ts:3-14`）。
- **不存在任何 git diff/status/文件内容读取端点。** Node 现有的 git CLI 使用全部服务于 worktree 创建/列举：`rev-parse --git-common-dir`（`crates/remuda-node/src/worktree.rs:245-253`）、`worktree list --porcelain`（`:308-336`）、`worktree add`（`:129-139`）、`show-ref`（`:286-306`）；git 调用统一走有界 helper（`:366-394`），workspace 依赖中也没有 git2/gix（全程 `Command::new("git")`）。worktree 目录册 `<git-common-dir>/remuda-worktrees.json` 记录的 `base` 只是**创建时的 start-point，默认 `main`**（`:11-30,63-67`），只覆盖经 Remuda 创建的 `remuda-wt` 工作树，不是会话基准。
- Node 现有线面对象：loopback 路由只有实例/命令/journal/follow/tty/rpc（`crates/remuda-node/src/server.rs:94-105`）；JSON-RPC 读方法是 `workspace.list/get`、`worktree.list`、`instance.get/list`、`events.read` 等（`server.rs:799-846,938-951`）；Hub↔Node WSS 派发明列在 `crates/remuda-node/src/runtime_link.rs:25-156`（workspace 方法 `:27`，worktree 方法 `:154`，无任何文件读方法；未知方法回落 `{"ok":true}` 在 `:155`——新方法必须进显式 match，不能依赖兜底）。

**能证明的：** 某个 host 上某个已注册目录此刻的、可授权访问的工作树真实状态（需要新增只读计算）。
**不能证明的：** 变更由谁/哪个实例/哪一轮产生；工作区不是 git 仓库时的基准对比；历史状态（无快照留存）。

### 2.7 Hub 附件 / 对象接口（相关但不可复用为数据源）

- `POST /v1/objects` / `GET /v1/objects/{id}` 只收图片（PNG/JPEG/GIF/WebP）、5 MiB 封顶、24 小时过期、绑定实例（路由 `crates/remuda-hub/src/objects.rs:42-43`）。
- D-028 的会话内附件读 `/v1/attachments`、`/v1/attachments/{objectId}/content` 是给会话内 agent 的，实例凭据被钉死在自己的会话上（`crates/remuda-hub/src/attachments.rs:41-45,145-172`）。
- Node 侧附件是**从 Hub 拉取**并写到工作区之外的 `<data>/instances/<id>/attachments`（`crates/remuda-node/src/attachments.rs:1-10,136-145`），注释明确“transient input, not repository content”。

**结论：** 与工作区文件/diff 无关，不作为来源；但其“作用域校验 + 大小封顶 + RESOURCE_LIMIT 结构化拒绝”的接口形状（`attachments.rs:113-118`）可作为新读接口的范式。

### 2.8 候选源总表

| 源 | 能证明 | 基准 revision | 内容 digest | 截断 | 实例/轮次归因 | G2 可用 |
| --- | --- | --- | --- | --- | --- | --- |
| journal `tool_call.input`（Claude，§2.1） | 路径被**提及**、工具被提议 | 无 | 无 | 8 MiB 整行硬拒收，无软截断 | 实例可靠；轮次不可靠 | 仅可作“提及”标注，禁止归因 |
| journal `tool_result`（Claude，§2.1） | 成败布尔 + 自由文本 | 无 | 无 | 同上 | 同上 | 否（`changes` 恒空） |
| `FileChange` / artifact / workspace-file（§2.2） | 协议上可证明 applied + diff | 字段已预留 | 字段已预留 | 字段可承载 | 字段可承载 | 否（零生产方） |
| Claude file-history 快照（§2.3） | 原生检查点（未来最有潜力） | 原生自有 | 未解析 | 未解析 | 可关联会话文件 | 否（opaque，留待版本化解析） |
| Codex rollout / app-server（§2.4） | 仅用量；wire 层有 fileChange | — | — | — | — | 否（未接线，实际 driver 无结构化观测） |
| Grok ACP（§2.5） | 仅工具调用 ID | — | — | — | — | 否（未接线） |
| Node 工作区实时 git 状态（§2.6） | **工作树此刻确实相对 HEAD 不同** | 可实时得到 HEAD；非 git 不可得 | 可实时计算（SHA-256，与 journal 同算法 `crates/remuda-journal/src/util.rs:8-11`） | 接口层显式封顶 | **不能归因到实例/轮次** | **是（一期唯一来源），标题恒为「工作区当前变更」** |
| Hub/Node 附件（§2.7） | 临时图片输入 | 无 | 有（对象 digest） | 5 MiB | 绑定实例但与仓库无关 | 否 |

---

## 3. 契约决策

### 3.1 数据源：选用与明确拒绝

- **选用（唯一）：** Node 对**注册工作区当前工作树**做实时、只读的 git 计算（status + diff + 受限文件内容读取），经 Hub 新增只读代理接口提供给 Web。与 agent 种类/driver 无关：Claude、Codex、Grok 会话看到的是同一工作区状态，因此该视图在任何 driver 下行为一致。
- **拒绝：**
  1. 拒绝把 journal `tool_call` 路径列表当作改动列表（只能证明提及，§2.1）。
  2. 拒绝依赖 `FileChange` / artifact / `WorkspaceFileLocator`（无生产方，§2.2）。
  3. 拒绝解析 file-history opaque blob 作为一期来源（无版本化解析与对账，§2.3）。
  4. 拒绝使用 Codex/Grok 的未接线预研/wire 类型（§2.4-2.5）。
  5. 拒绝复用附件通道（图片、临时、工作区外，§2.7）。
  6. 拒绝在 Hub 侧缓存/落盘任何工作区文件内容或 diff（Hub 只代理实时响应；离线即不可用，不展示陈旧内容冒充当前状态）。
- **可选的二期增强（不进 G2）：** 把 journal 提及路径做成**非权威角标**（“会话曾提及”，点击仍跳到实时 diff 且实时状态为准）；版本化解析 file-history blob；driver 适配层在未来填充 `changes: FileChange`（届时才可出现“会话改动”措辞，且必须以 `application=applied` 为唯一依据）。

### 3.2 命名与归因规则（硬性）

1. 只要数据来自 §3.1 的实时工作树计算，视图标题**恒为「工作区当前变更」**，副标题展示主机名 + 工作区根路径与采集时间（“采集于 …”），**任何位置**不得出现“本次会话改动 / session changes”。
2. 视图可以从某个实例入口打开（`instanceId` 仅用于定位 `hostId + workspaceId`，字段在实例实体上：`crates/remuda-protocol/src/entities.rs:216-219`；Web `web/src/types/instance.ts:26-27`），但内容不归因于该实例。同工作区存在其他实例（含 resume 后继实例）时，必须展示中性说明，例如“这些变更来自该工作区，可能由本会话或同目录的其他会话产生”。
3. “会话提及”角标是**提及**不是**归因**：提及路径在工作区已不存在/已还原时如实显示为空/已还原，不补造 diff。
4. 轮次级归因在协议补齐 `native_turn_id`（当前恒 unknown，§2.1）之前不承诺。

### 3.3 授权边界

- **Hub：** 新接口为 operator-only（沿用 `require_operator`，范式见 `crates/remuda-hub/src/workspaces.rs:31,75`），并复用实例读校验的位置语义（`crates/remuda-hub/src/agent_scope.rs:83-93`）。会话内 agent 凭据不得访问该接口（与附件路由的 fail-closed 一致，`attachments.rs:162-172`）。
- **路由定轴：** 请求只接受 `hostId + workspaceId`（或 `instanceId` 解析得到二者），**不接受**绝对路径、repo 路径或 base 参数。参照 worktree.create 显式拒绝 `repo` 入参的先例（`crates/remuda-node/src/worktree.rs:72-80`）。
- **Node 解析：** workspaceId 必须命中 Node 自己的注册表并重新通过 `validate_existing` 的 canonical 身份校验（`workspace.rs:136-146`）；命中失败即 NotFound，绝不回退到“第一个工作区”。
- **路径规范化：** 接口返回的条目路径一律为相对工作区根、字节序稳定的相对路径；任何按路径取内容的请求，在读取前必须用 `path_guard::absolutize` + `path_guard::real_path` + `path_guard::contain` 以「工作区根 + 其 worktree 边界根」（`worktree_boundary`，`workspace.rs:377-432`）为唯一允许根集合做校验（护栏 `crates/remuda-protocol/src/path_guard.rs:93-112,130-160`）。symlink 解到根外即拒（测试范式 `path_guard.rs:241-255`）；不接受绝对路径与含 `..` 的路径（`:164-185`）。
- **执行边界：** 只读。允许的 git 子命令白名单仅为查询类（建议：`rev-parse`、`status --porcelain`、`diff`、`diff --cached`、`ls-files`/`--others` 中的最小集）；参数以 argv 数组传递，禁止 shell；`current_dir("/")`、`LC_ALL=C`、可选锁关闭（`--no-optional-locks`），复用 `bounded_workspace_command` 的可杀子进程与截止时间机制（`workspace_access.rs:132-216`）。未跟踪文件的内容通过普通受限文件读获取，不为此执行任意命令。接口处理一次请求**不得**修改工作区：不创建 index 锁、不写文件、不触发 hook。
- **不扩展为文件浏览器/编辑器：** 不提供目录列举（条目只来自 git status 的结果集）、不接受通配、不提供写接口。

### 3.4 基准 revision

1. 仅支持 git 工作区：Node 以注册根为 `git -C <root> rev-parse --show-toplevel` 判定（git helper 范式 `worktree.rs:366-394`，-C 定根、不经 shell）。非 git 目录 → 整个视图为「不支持」（§3.6 状态 D），不发明合成基准。
2. 对比基准 = **当前 HEAD**：响应必须回传 `headOid` 与分支名（未知时为 `unknown`，遵循项目 Knowledge 约定），并区分**已暂存**（相对 index/HEAD）与**未暂存**（相对 index）两组，使用 `git status --porcelain` 的 XY 码原样透传，不自行发明状态机。
3. Remuda worktree 目录册里的 `base`（默认 `main`，`worktree.rs:63-67`）只可作为“创建起点”附注展示，**不得**静默作为 diff 基准。
4. base/head 在列表与每次取 diff 时各取一次并随响应回传；两端不一致时前端按“内容已变化”处理（§3.7）。

### 3.5 路径、digest、归属与截断字段（响应契约提案）

新增 Node JSON-RPC（仅读，方法名进入显式派发 match，禁止依赖 runtime_link 兜底 `:155`）：

- `workspace.scm.status`，参数 `{workspaceId}`：

  ```json
  {
    "workspaceId": "ws_…", "root": "/canonical/abs/path",
    "scm": "git",
    "headOid": "sha", "branch": {"state": "known", "value": "feat/x"},
    "observedAt": "RFC3339",
    "entries": [
      {"path": "src/a.rs", "xy": " M", "kind": "modified",
       "oldOid": "sha|null", "newOid": "sha|null",
       "sizeBytes": "123", "digest": {"state": "unknown", "reason": "not-collected"}}
    ],
    "limits": {"maxEntries": 5000, "maxDiffBytes": 262144, "maxFileBytes": 1048576},
    "truncated": {"entries": false, "diffBytes": "0" }
  }
  ```

- `workspace.scm.diff`，参数 `{workspaceId, paths: [相对路径…], staged: bool}`：每条目返回 `{path, patch, truncated: bool, bytesAvailable}`，总量受 `maxDiffBytes` 封顶，超出部分给稳定截断标记而不是静默切断。
- `workspace.scm.file`，参数 `{workspaceId, path}`：未跟踪/新文件的当前内容读取，返回 `{path, mediaType:"text/plain"|…, sizeBytes, digest:{state:"known",value:"sha256:…"}, truncated}`；非文本默认不内联（返回 `mediaType` 与 digest，前端自行决定是否展示）。

字段规则：

- **path：** 相对注册根（或其已登记 worktree 边界）的规范化相对路径；读取前再次做 §3.3 的包含校验（状态与内容两次请求之间文件可能被替换为 symlink）。
- **digest：** 对成功读取的当前字节算 `sha256:`（与 journal blob 同一算法，`crates/remuda-journal/src/util.rs:8-11`）；列条目时不算 digest（`unknown("not-collected")`），只在取内容时算，避免大仓库列表请求变成全量读盘。
- **归属：** 响应**不含** instanceId/turn 字段；观测归属属于“从哪个实例打开”这一 UI 上下文，不属于数据。
- **截断：** 三个显式限制（条目数、单请求 diff 字节、单文件字节）必须在响应中回传实际值与各自的布尔标记；截断是结构化状态（参照附件接口超限返回 `RESOURCE_LIMIT` 而非切断响应体的做法，`crates/remuda-hub/src/attachments.rs:113-118`）。journal 来源没有软截断字段、只有 8 MiB 整行硬拒收（§2.1），因此“提及”角标不做部分内容展示，也就不需要截断承诺。
- **体量与超时：** git/读盘走有界子进程（`workspace_access.rs:132-216`），RPC 需要独立超时，建议沿用 worktree RPC 的超时档位（`WORKTREE_RPC_TIMEOUT` 使用点 `crates/remuda-hub/src/http.rs:978`）。

### 3.6 可用性状态（穷举，前端不得合并成“加载失败”）

| 状态 | 触发事实 | 数据来源/先例 |
| --- | --- | --- |
| 无变化 | git 工作区干净（含无未跟踪文件，忽略规则与原生 git 一致并在响应中说明） | 新 status 计算空集 |
| 尚未采集 | 请求尚未返回 / 视图首次打开且无响应；Hub 不缓存内容，刷新即重新采集 | UI 本地态，不得拿 journal 旧数据冒充 |
| 不支持 | 注册目录不是 git 仓库；或条目为二进制且请求了文本内联（条目级不支持，不影响整视图） | `rev-parse` 失败的结构化分类 |
| 权限不足 | operator 鉴权失败（HTTP 403，范式 `attachments.rs:162-172`）；或 Node 文件系统探针被拒/超时（macOS TCC、`workspace_access.rs:52-71,116-128`） | 403 / Node 错误结构化映射 |
| 离线 | host 无活动 Node 连接 | Hub 现有语义：`HostOffline` → HTTP 409 `HOST_OFFLINE`（`crates/remuda-hub/src/error.rs:98`，触发点如 `http.rs:690-694`、`workspaces.rs:77-79`）；`call_node` 返回 `Ok(None)` → HTTP 422 `PLACEMENT_UNSATISFIABLE`（`http.rs:991-993`、`error.rs:96`）。Web 已有 host `online/offline` 状态（`web/src/types/generated.ts:1076`、`web/src/types/instance.ts:86`、`web/src/lib/status.ts:5`） |
| （附加）工作区不存在 | workspaceId 未登记/根已迁移 | Node `NotFound`（`server.rs:806-816`）/ `validate_existing` 拒绝（`workspace.rs:136-146`） |

### 3.7 并发一致性（“内容已变化”）

列表快照与取内容是两次请求，期间文件可能变化：`workspace.scm.file` 必须回传当前 `headOid + digest + sizeBytes`；前端发现与列表时的 oid/大小不一致，或取 diff 返回的 oid 已变，显示“内容在采集后变化，请刷新”的显式状态并提供一次手动刷新，不自动轮询、不静默替换。这沿用项目“缺 ACK/调和中不猜测”的既有原则（探索文档 §5 P0-3）。

---

## 4. API / 协议承载提案（纯增量）

1. **Node 侧**：新增 `workspace.scm.status` / `workspace.scm.diff` / `workspace.scm.file` 三个只读 JSON-RPC：
   - loopback 派发表加显式分支（`crates/remuda-node/src/server.rs:799-818` 一带，紧挨 `workspace.get`）；
   - WSS 派发表加显式分支（`crates/remuda-node/src/runtime_link.rs:27,154` 一带，与 workspace/worktree 方法同级），stdio 派发表同步（`crates/remuda-node/src/stdio.rs:381` 一带有 worktree 同类入口）；
   - 实现落新模块（建议 `crates/remuda-node/src/workspace_scm.rs`），复用 `path_guard`、`workspace_access::bounded_workspace_command`、`worktree::git` 风格 helper；不改动注册表与 journal。
2. **Hub 侧**：新增 operator-only REST，建议挂在已有工作区资源下：`GET /v1/hosts/{hostId}/workspaces/{workspaceId}/changes`、`/changes/diff`、`/changes/file`（路由表范式 `crates/remuda-hub/src/workspaces.rs:14-18`；代理用 `call_node`，`http.rs:970-996`；离线错误免费获得，§3.6）。Web 已持有 hostId/workspaceId，无需额外实体；`gen:api` 后类型进 `web/src/lib/api.generated.ts`（G2 需重新生成并通过冻结检查）。
3. **协议侧**：不新增 observation kind、不修改 journal。若 G2 需要把 SCM 快照做成可回放观测，那是独立版本化提案（应走 `artifact/diff` 或新 lifecycle，不在本批）。
4. **Web 侧**：只替换 `files-pane` 占位（`web/src/pages/SessionPage.tsx:326-341`）；宽屏按需面板、手机全屏路由（路由已具备，补手机入口，§1），返回保持正文位置（占位已有 back 行为）。不碰 Shell/SpacesPanel。
5. **夹具**：Rust 单测用临时 git 仓库（既有范式：`crates/remuda-node/src/worktree.rs:407-412` 初始化仓库、`workspace.rs` 注册测试）；hub-live e2e 的假 Node 需新增三个只读方法的夹具应答（现有假 Node 已应答 `workspace.list`，`crates/remuda-testing/src/fake_herdr.rs:665-666`）；全程 fake node + fake-harness，不用真实模型（计划 §0）。

---

## 5. 显式不做（Out of scope）

- 不做“按会话/按轮次归因的改动列表”（无可靠来源，§2）。
- 不做目录浏览、文件编辑、写回、提交、分支操作、命令执行。
- 不解析 Claude file-history opaque blob、不接入 Codex/Grok 未接线的工具流（二期候选）。
- 不在 Hub 缓存/持久化任何文件内容或 diff，不做跨主机聚合。
- 不做非 git 目录的“按时间/按工具提及”伪 diff；非 git 只给「不支持」。
- 不改变 worktree 创建语义、注册表结构与 `Workspace` 实体（`repository/worktree` 字段的填充是独立工作）。

---

## 6. 安全与隐私

- 工作区文件内容可能含密钥与私人信息；接口受 operator + 注册根双重约束（§3.3），响应不进 Hub 持久层。
- 任何对外截图/证据只用合成 fixture（探索文档 §1、§5 P2-1 既定要求）。
- git 调用面收敛到只读白名单 + argv 数组 + 有界子进程（§3.3），防止参数注入与挂死；symlink/`..` 逃逸在读取前由 `path_guard` 拒绝。
- 二进制/超大文件默认不内联；任何单文件/单请求都有显式字节上限（§3.5）。

---

## 7. G2 建议：GO（最小只读范围）

**建议开工 G2，但范围严格限定为：实时只读的「工作区当前变更」视图。** 勘察确认授权、路径规范化、有界 git 调用、离线错误、Web 入口与路由这五块基础设施全部存在可复用先例（§3、§4），不需要新的归因数据，也不需要改 journal/协议观测。工作量与计划估计一致：实现为 L（跨 Node + Hub + Web，纯增量）。

G2 最小范围：

1. Node 三个只读 SCM RPC（status/diff/file）+ Hub 三个 GET 代理路由；
2. Web 替换 files 占位：变更条目列表（XY 码 + 路径 + 大小）、条目 unified diff、未跟踪文件受限文本预览；标题恒为「工作区当前变更」+ 采集时间；
3. §3.6 六态齐全；§3.7 的内容变化态；§3.5 的三类截断标记；
4. 手机补全屏入口（路由复用）；
5. 不实现“提及角标”、不实现 file-history 解析（留待 G3 候选，须另行评审）。

验收清单（探索文档 §5 P2-1 七项展开为可测用例）：

| # | 场景 | 期望 |
| --- | --- | --- |
| 1 | 合法文件（已跟踪被修改） | 条目出现，diff 与夹具实际内容一致，显示 headOid |
| 2 | 合法未跟踪文件 | 条目出现（`??`），可只读预览内容，含 sha256 digest |
| 3 | 越界路径（`../outside`、指向根外的 symlink；直接构造 `workspace.scm.file` 请求） | 请求被结构化拒绝（4xx），不返回字节；Node 日志不含越界读取（`path_guard` 测试范式 `path_guard.rs:218-255`） |
| 4 | 无变化 | 干净仓库显示「无变化」空态，不显示“0 个会话改动”之类归因措辞 |
| 5 | 不支持 | 非 git 注册目录显示「不支持」并说明原因；二进制条目不内联 |
| 6 | 离线 | 断开 Node 后打开视图显示「离线」（409/422 映射），不展示陈旧内容 |
| 7 | 权限不足 | 不可读目录（探针拒绝/超时夹具）显示「权限不足」，错误可定位 |
| 8 | 截断 | 超过 maxFileBytes 的文件、超过 maxDiffBytes 的 diff、超过 maxEntries 的列表分别出现对应截断标记与已封顶事实，不静默切断 |
| 9 | 内容变化 | 列表后修改文件再取 diff/file，出现“采集后已变化”，手动刷新后恢复一致 |
| 10 | 只读性（关键安全验收） | 连续打开视图 N 次：夹具文件 mtime/内容不变、`git status` 前后一致、无 index 锁残留；单测断言 git 调用 argv 全部在白名单内、无 shell 调用；视图操作不产生任何实例命令/journal 事件 |
| 11 | 命名与归因 | UI 文案在任何状态下均无“本次会话”；resume 后继实例同工作区时显示中性归属说明 |
| 12 | 手机 | 390px 下可经全屏路由进入、返回保持正文位置（探索文档 §5 P2-1 既定） |

若 G2 实施中发现 git 只读白名单无法在不触达工作区写锁的前提下完成（例如某 git 版本强制加锁），按「准确的不可用说明」收尾并回退到状态说明，不得改为执行写操作或用 journal 提及数据填充（探索文档 §5 P2-1 的进入条件原文要求）。
