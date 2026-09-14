# G1 文件 / diff 视图数据契约勘察证据

- 日期：2026-09-14
- 批次：workbench G1（只产文档，无代码变更）
- 产出：[../files-view-contract.md](../files-view-contract.md)
- 源码基线：`origin/main` = `3da158d`（工作树 `wt/ux-g1/files-view-contract`）
- 方法：纯只读源码勘察（Read/Grep/Glob 与两个并行只读探查子任务），**未启动任何服务、未运行 e2e、未调用真实模型**

## 勘察范围与对应结论

| 勘察问题 | 结论 | 关键证据 |
| --- | --- | --- |
| journal 是否记录“文件确实被改” | 否。Claude 工具结果的 `changes: Vec<FileChange>` 在全部构造点恒为空 | `crates/remuda-journal/src/claude.rs:807-852`（`:847-849`）、`crates/remuda-driver/src/claude_print.rs:907-933`（`:931`） |
| 工具调用记录里有什么 | 工具名 + 类别（按名字推断）+ 原样 `input` JSON；状态恒 `proposed`；结果仅成败布尔 + 自由文本 | `crates/remuda-journal/src/claude.rs:761-805`（`:801`）、`:870-881`；`crates/remuda-driver/src/claude_print_stream.rs:89-108`（`:96-105`）；协议 `crates/remuda-protocol/src/observation.rs:279-338` |
| `FileChange`/artifact/workspace-file 协议通道 | 类型齐全但零生产方 | `crates/remuda-protocol/src/observation.rs:305-315,599-645,647-673`；`crates/remuda-protocol/src/enums.rs:539-543,643-650`；全仓 grep `FileChange {` 无协议外构造点 |
| Claude file-history-snapshot | 原生记录存在，被映射为 opaque，raw blob 留底、无结构解析 | `crates/remuda-journal/src/claude.rs:578-596`；`crates/remuda-driver/src/claude_print.rs:1754-1760`；`docs/design/protocol.md:988,996` |
| Codex custom_tool_call / fileChange | rollout 解析器与 app-server wire 类型均存在，但预研自述未接线、wire crate 零依赖；实际 Codex 走 GenericPty/ShellPty，只产 lifecycle + 原始 TTY | `crates/remuda-driver/src/codex_rollout.rs:1-5,72-88,226-240`；`crates/remuda-codex-wire/src/types.rs:584-595`；`crates/*/Cargo.toml` 依赖 grep；`crates/remuda-node/src/runtime.rs:1525-1550`；`crates/remuda-driver/src/generic_pty.rs`（ObservationPayload 全为 Lifecycle） |
| Grok tool_call/update | ACP 分类器认识事件；driver 预研只抽关联 ID 且未接线 | `crates/remuda-acp-wire/src/types.rs:86-88,113-114`；`crates/remuda-driver/src/grok_session.rs:1-6,58-66,115-119` |
| 工作区注册/授权边界 | 注册表 + canonicalize + 允许根 + symlink 拒绝 + 共享 path_guard 已完备，可直接复用 | `crates/remuda-node/src/workspace.rs:100-146,346-432,484-558`；`crates/remuda-protocol/src/path_guard.rs:50,56-67,93-160`；`crates/remuda-node/src/workspace_access.rs:52-71,132-216` |
| Node 是否存 git 基准 | 否。Workspace 实体的 repository/worktree 字段从不填充；上报快照仅 `{workspaceId,hostId,root}` | `crates/remuda-node/src/runtime.rs:1854-1855`；`crates/remuda-node/src/workspace.rs:94-98`；`crates/remuda-hub/src/workspaces.rs:187-194`；`crates/remuda-protocol/src/hubnode.rs:188-197`；`crates/remuda-protocol/src/entities.rs:94-145,150-179` |
| 是否已有文件/diff 读取端点 | 否。git CLI 仅服务 worktree 创建/列举；Node/Hub 路由表无 file/diff/content（图片附件除外） | `crates/remuda-node/src/worktree.rs:129-139,245-336,366-394`；`crates/remuda-node/src/server.rs:94-105,799-846,938-951`；`crates/remuda-node/src/runtime_link.rs:25-156`；`crates/remuda-hub/src/objects.rs:42-43`、`crates/remuda-hub/src/attachments.rs:41-45` |
| Hub→Node 只读代理与离线语义 | `call_node` 代理 + 409 `HOST_OFFLINE` / 422 `PLACEMENT_UNSATISFIABLE` 现成 | `crates/remuda-hub/src/http.rs:896-929,970-996`（`:991-993`）；`crates/remuda-hub/src/error.rs:96,98`；`crates/remuda-hub/src/workspaces.rs:34,77-79` |
| 截断标记 | journal 无软截断字段；传输层为整行硬上限（8 MiB / ACP 2 MiB / codex 16 MiB）；附件接口有结构化 RESOURCE_LIMIT 范式 | `crates/remuda-claude-wire/src/codec.rs:10,50-63`；`crates/remuda-acp-wire/src/codec.rs:12`；`crates/remuda-codex-wire/src/rpc.rs:14`；`crates/remuda-hub/src/attachments.rs:39,113-118` |
| 实例/轮次归因 | 实例与 seq 可靠（提交时分配）；`native_turn_id` 在 Claude 路径恒 unknown；resume 使同原生会话跨实例 | `crates/remuda-journal/src/store.rs:419`；`crates/remuda-journal/src/claude.rs:996-1034`（`:1012`）；`crates/remuda-protocol/src/observation.rs:84-112` |
| Web 占位与手机入口 | files 路由/桌面入口/占位已存在；手机入口被 `.deskOnly` 隐藏 | `web/src/app/router.tsx:29`；`web/src/pages/SessionPage.tsx:32,63-67,176-186,326-341`；`web/src/features/session/session.module.css:1789,1809` |

## 可复用基础设施（G2 不需要新造）

1. 路径授权：`path_guard::real_path/contain/contain_strict/safe_segment`（`crates/remuda-protocol/src/path_guard.rs`）+ 工作区注册表校验（`crates/remuda-node/src/workspace.rs`）。
2. 有界子进程：`bounded_workspace_command` / `workspace_access_check`（`crates/remuda-node/src/workspace_access.rs`），现有 git helper（`crates/remuda-node/src/worktree.rs:366-394`）。
3. Hub→Node 代理：`call_node`（`crates/remuda-hub/src/http.rs:970-996`）与 workspaces 路由范式（`crates/remuda-hub/src/workspaces.rs`）。
4. 大小封顶的结构化拒绝：附件 content 路由（`crates/remuda-hub/src/attachments.rs:96-137`）。
5. 实例→host/workspace 定位：`Instance.host_id/workspace_id`（`crates/remuda-protocol/src/entities.rs:216-219`；`web/src/types/instance.ts:26-27`）。
6. 内容摘要算法：journal 已用 `sha256:` 前缀摘要（`crates/remuda-journal/src/util.rs:8-11`），文件 digest 沿用同一表示。
7. 测试夹具范式：临时 git 仓库（`crates/remuda-node/src/worktree.rs:407-412`）、工作区注册测试（`crates/remuda-node/src/workspace.rs:683-743`）、假 Node 已应答 `workspace.list`（`crates/remuda-testing/src/fake_herdr.rs:665-666`）。

## 明确记录的否定结论（避免后续重复勘察）

- 不存在任何“按会话归因的文件改动”结构化来源；工具调用中的路径只能证明**提及**。
- `ToolResultPayload.changes`、`ArtifactPayload`、`ArtifactLocator::WorkspaceFile` 在当前代码中没有任何生产方。
- `Workspace.repository`（含 `head_oid`）与 `Workspace.worktree`（含 `base_oid`/`dirty`）字段虽在协议实体上，但 Node 从不填充，Hub/Web 也只收三字段快照。
- remuda-wt 目录册的 `base`（默认 `main`）只是 worktree 创建起点，且只覆盖经 Remuda 创建的工作树，不是会话基准。
- Codex 与 Grok 当前实际运行路径（GenericPty/ShellPty）不发射结构化工具观测；两个 wire/rollout crate 的能力不能被当作既有数据能力。
- 附件对象是工作区外的临时图片输入，与文件视图无关。

## 勘察的局限

- 本次未运行任何二进制或服务：git 调用行为依据现有 `worktree.rs` 的封装与测试推断，未在真实大仓库上测量 `git status/diff` 时延；契约中的上限数字（条目数/字节数）是提案值，需 G2 用夹具与真机测量后定稿。
- 文件系统权限拒绝（尤其 macOS TCC）的分支依据现有探针错误分类，未在 macOS 上复现。
- 结论均给出 file:line；契约中新增方法名/响应形状是**提案**，当前代码中不存在。

## 自查

- 仅新增本文档与 `docs/design/files-view-contract.md`，无代码变更。
- 文档仅含仓库内相对路径与合成示例，无主机名、个人路径或凭据。
- 已运行 `./scripts/ci/secret-scan.sh`，结果记录于提交说明。
