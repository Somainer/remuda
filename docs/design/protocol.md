# Unified Remote Agent Runtime 协议规格

版本：`0.1-implementation-draft`（尚未发布 wire v1）。日期：2026-09-12。项目名 Remuda；统一 wire major：`1`、minor：`0`；Observation schema：`1`。实体、Serde 编码、binary header 与 schema/TS 生成已在 `remuda-protocol` 实现；进程、存储、网络和原生驱动验收分别见 §12 的实施表。本文的“必须”“禁止”仍约束后续执行实现；已有 `runtime.*` 方法名保持不变。

## 0. 定位、证据与对 proposal 的修正

本文只链接随仓库提交的文件或公开来源。历史本地调研名仅记录设计依据，运行能力以 §12 的实施状态和后续原生验收为准。

沿用 [proposal.md](proposal.md) 的 `Clients → Hub → Node Agent → native CLI` 分层。Web/PWA 是人的主界面；本文的 CLI/MCP 是主 agent 的控制工具和运维入口。Node Agent 托管进程、转发原生输入、记录观察、承接交互，绝不根据 Observation 拼模型历史、执行自己的推理循环或重写 Claude Workflow。任务背景 已确认：网关任意模型名可以用于 Claude dynamic Workflow 的 `agent(prompt, {model})`，不能用于普通 Agent/Task 的 `model` 参数；旧调研中对此标为“推测”的段落已被这条用户事实覆盖。

本文采用三个权威：原生 session 是继续执行的权威；Node 的 Observation journal 是已观测事实的权威；Hub 的持久命令入口与 Node 的持久执行账本共同记录命令投递，其中 **Node 是原生命令派发与 Interaction 决定的唯一裁决者**。Hub 的索引、搜索与 UI projection 都是可重建副本。前两项来自 DSH 报告 §3.4、§8.3–8.4，第三项是为多设备、断线与进程重启增加的设计。

| 对 proposal v0.1 的修改 | 本规格决定 | 理由与证据 |
| --- | --- | --- |
| `Instance.status` 混用 working/done/exited | 分开 `lifecycle`、`activity`、`connectivity`；成功属于 Run | Claude background 的 `state=done` 时进程仍活着，control-plane §1.6 |
| 首个 print `result` 代表任务结束 | `Run.completionScope` 区分 `native-turn` 和 `task`；每条 result 先作为一个原生回合结束 | 同一 Workflow 实测有两条 result；见 control-plane §4 |
| `--settings` / 临时 HOME 被视为完全隔离 | overlay 按 launch 保存；原生 home 持久；共享 daemon 单列 | `--settings` 共享 daemon，空 `CLAUDE_CONFIG_DIR` 没登录态，control-plane §3 |
| 失效后换 key 重启并继续 | 只有确定原生进程已停、native resume 可用、未确认命令已对账时才允许换 generation；禁止重放原 prompt | DSH §8.4、gateway-auth §5 |
| Herdr 状态等于任务结果 / Socket 等于原始 PTY 流 | Herdr 是 Phase 0 PTY 的固定 carrier；状态是 observation，`pane.read` 是屏幕快照；原始字节能力另外协商 | Herdr API、落盘 schema |
| PermissionRequest hook 可以直接套用 Codex 示例 | Claude、Codex 各自编码；Claude hook 没有可假定的 `tool_use_id`，需要本地 invocation ID | hooks 调研、[Claude 官方 PermissionRequest](https://code.claude.com/docs/en/hooks#permissionrequest) |

已读输入：_context、design-protocol task、agent-protocols、DSH、herdr-api-index、env-inventory、[proposal](proposal.md)。专项证据采用 hooks-integrations、claude-control-plane、codex-appserver-driver、grok-acp-driver、gateway-auth 的 CLI 配置节，并纳入写作期间完成的 Claude 交互探针、Claude stream 协议参考、PTY spike。Claude 交互探针已验证的 stdio flag、allow/deny 和单选问答，覆盖较早协议参考中对应的待验证项；多选、plan-review、elicitation 等仍各自保留验证状态。

补充证据包括 agy 原始样例、Codex 本机生成 schema、Codex 源码 README、本机 Grok 随包文档 `<workspace>`。这些来源中的“实测”由原调研执行；本任务只读材料、起草规格，没有启动 agent、花费模型配额、改凭据或修改参考仓库。

另核对了 Anthropic 官方 SDK 源码 [query.py](https://github.com/anthropics/claude-agent-sdk-python/blob/3379406f18fcea64617d25663d811dfdde8cd171/src/claude_agent_sdk/_internal/query.py) 与 [types.py](https://github.com/anthropics/claude-agent-sdk-python/blob/3379406f18fcea64617d25663d811dfdde8cd171/src/claude_agent_sdk/types.py)，读取时 main 为 `3379406f18fcea64617d25663d811dfdde8cd171`。这是 wire 设计参考，不是本机 Claude 2.1.268 与该 SDK 组合已通过测试的证明。其 task ledger 注释明确承认：后台任务先结束、父回合 result 后到时，仍可能有待执行的 continuation；空 ledger 不能证明整个任务结束。

下文“原生已知”表示上述材料支持；“规格决定”表示新协议要求；`unknown` 表示没有足够证据。环境报告中的 PATH、版本和在线状态是历史快照：尤其 Codex 存在 `0.145.0` 与 `0.154.0` 多个安装，必须按目标 binary 的绝对路径和 digest 选择 adapter，不能按 `codex` 名称猜版本。gateway-auth §0

## 1. Wire 基础类型、ID 与原生身份

### 1.1 编码规则

所有控制消息使用 UTF-8 JSON；字段名为 camelCase。省略字段只表示该 variant 不适用或可选项未提供；已查询却不知道的值必须显式使用 `Knowledge<T>`。`null` 仅用于“没有关联实体”，例如后台通知没有 `runId`，不能代替 `unknown`。未知枚举值不得转换成默认成功。

~~~typescript
type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type U64 = string; // 十进制，0 或非零开头的数字；不经 JavaScript Number。
type Timestamp = string; // UTC RFC3339，毫秒精度，例如 2026-09-12T10:00:00.000Z。
type Id = string; // 下述带实体前缀的 UUIDv7；SDK 为各实体生成不同 brand。
type Digest = string; // sha256: 加 64 个小写十六进制字符。
type Knowledge<T> =
  | { state: "known"; value: T }
  | { state: "unknown"; reason: string; evidenceEventIds: Id[] }
  | { state: "not-applicable" };
type EntityMeta = {
  id: Id; revision: U64; createdAt: Timestamp; updatedAt: Timestamp;
};
type ActorRef = {
  principalId: Id;
  type: "human" | "bot" | "agent" | "system";
  deviceId: Id | null;
  instanceId: Id | null;
};
~~~

`revision` 从 `1` 开始，实体每次持久变更加一，用于 CAS；与 Observation `seq` 无关。数字时间、usage、价格等不得有 NaN/Infinity。JSON 输入拒绝重复 key、非法 UTF-8、非整数的计数；结构化扩展只放 `extensions`，不能覆盖标准字段。日志顺序使用 seq，不比较跨主机墙钟决定先后。

### 1.2 全局 ID 与唯一约束

| 对象 | ID 前缀 | 创建者与稳定性 |
| --- | --- | --- |
| Host / Workspace / worktree | `hst_` / `wsp_` / `wkt_` | Hub 注册分配；worktree 可由持有授权的 Node 预分配；主机别名、路径、branch 都不是 ID |
| Instance / Run | `ins_` / `run_` | Hub 在排队前分配；原生自行产生的 Run 由 Node 分配；重发同一 Command 不重分配 |
| Command / Interaction / Observation | `cmd_` / `int_` / `evt_` | Command 可由客户端离线生成；Interaction、Observation 由 owner Node 生成 |
| 其它引用 | `dev_`、`prn_`、`pvp_`、`cred_`、`obj_`、`sub_`、`tty_`、`launch_`、`epoch_` | 同样的 UUIDv7 随机规则；不携带 credential 值或可读用户名 |

统一格式：`<prefix><lowercase UUIDv7>`，例如 `ins_01993ab0-0000-7000-8000-000000000001`。UUID 时间位只辅助排序；随机位按库实现生成，数据库唯一键冲突重试只发生在新对象创建前。恢复磁盘备份保留 ID；克隆成另一台执行节点必须重新 enrollment、生成新 hostId 和密钥，不能复制 host 身份并同时在线。

未单列前缀的 journal、native store、repository、message、tool、workflow、usage、lease、capability snapshot、binary 和 content 对象统一用 `obj_`；SDK 仍按实体种类 brand，接收端核对对象的实际类型，不能因为前缀相同就相互替代。

强制唯一键：`(principalId, commandId)`；`(hostId, journalId, seq)`；`eventId`；活 owner 的 `(hostId, nativeStoreId, nativeSessionId)`；Interaction 的 `(instanceId, processGeneration, connectionEpoch, nativeRequestKey, requestVersion)`。`nativeStoreId` 是已登记的原生数据根 ID；同一 UUID 出现在两个不同原生 home 时不可自动合并。

### 1.3 原生映射

~~~typescript
type HerdrServer = {
  binaryPath: string; version: string; digest: Digest; protocolVersion: string;
  serverIdentity: Id; serverEpoch: Id; representation: "rendered-ansi";
};
type SignalTier = "hook" | "file" | "osc" | "screen" | "none";
type RuntimeCapability = {
  name: CapabilityName; state: "supported" | "unsupported" | "unknown";
  provision?: CapabilityProvision; tier: SignalTier; reasonCode: string;
};
type NativeRef = {
  hostId: Id;
  nativeStoreId: Id;
  kind: "claude" | "codex" | "grok" | "agy" | "generic";
  sessionId: Knowledge<string>;
  transcript: Knowledge<{ objectId: Id; sourcePath: string }>;
  signalTier?: SignalTier;          // D-028 §4.3；缺省表示"无人上报"，不等于 "none"。
  capabilities?: RuntimeCapability[]; // 运行时上报，逐项盖过静态矩阵。
  codex?: { threadId: string };
  acp?: { sessionId: string; protocolVersion: number };
  claude?: { sessionId: string };
  claudeBg?: { jobId: string }; // job 可先于 session UUID 被观测；不是同一种身份。
  agy?: { conversationId: string };
  herdr?: HerdrServer & { session: string; paneId: string };
};
type ProcessRef = {
  processGeneration: U64;
  processIdentity: Knowledge<{ pid: number; birthId: string; supervisorId: Id }>;
  connectionEpoch: Id;
};
type NativeRequestKey =
  | { type: "rpc"; valueType: "string" | "number"; value: string }
  | { type: "hook"; invocationId: Id }
  | { type: "none" };
~~~

**运行时能力上报（D-028 §4.3）。** `signalTier` 是这个 session **实际拿到**的最高信号层级（`Hook > File > OSC > Screen`），`capabilities` 是它逐项上报的能力。二者都可缺省，缺省的含义是「没有人上报」——与上报了 `none` 不是一回事：前者让 §3.3 的静态矩阵继续生效，后者是一句「我什么信号都没有」的断言。有运行时值时**逐个 name 覆盖**静态矩阵，且**两个方向都能覆盖**：实测发现某能力不可用的 session 必须能把矩阵里的 `supported` 改回去，否则「运行时上报」就只能往乐观的方向修正。tier 本身也是证据：`hook` 能阻塞 agent 并拿回裁决（这正是 `interactive-approval` 的含义），`file` 只能证明身份与回合边界，`osc` / `screen` 两者都不能证明——这是它们排在最底层的原因。这条通道兑现统一原则第 5 条：能力取决于**本 session 拿到的信号层级**，而不是谁启动的、也不是 `DriverKind` 这一列。

`NativeRef.claudeBg` 只记录 native store 中的完整 job ID，即使统一 sessionId 为 unknown 也可保留；它不能替代 Claude UUID。未发布草案的 `claude.backgroundJobId` 移到独立 claudeBg 分支，不保留两个可分叉的 job ID 字段。Herdr 的 session 是明确选择的 server session 名，paneId 与 serverIdentity/serverEpoch 一起才标识原生 pane；server epoch 改变使旧 pane 控制失效。`NativeRef` 的 kind 与对应分支必须一致；分支中的 claude.sessionId、codex.threadId、acp.sessionId、agy.conversationId 必须等于已知的统一 sessionId，缺少该身份时不生成分支。未知 session 时 `sessionId.state=unknown`，不得虚构 Claude UUID / Codex threadId。`NativeRequestKey` 将 RPC 数字原样编码成十进制字符串并保留原类型，因此 JSON-RPC 的 `1` 与 `"1"` 不碰撞。`sourcePath` 只在授权给执行主机的视图返回，公网 UI 使用 objectId；它不是允许浏览器任意读绝对路径的授权。

| 原生身份 | 统一映射 | 禁止的替代 |
| --- | --- | --- |
| Claude session UUID | Instance 的 NativeRef；`--resume` 延续同一引用 | PID、后台 shortId、目录编码字符串都不能代替完整 session UUID |
| Claude message UUID / tool_use_id / agent_id / task_id / wf run ID | Observation 的 source；分别映射 messageId/toolCallId/memberId/workflowId 的显式关联表 | Task ID 与 workflow `wf_*` 不相等；不可按 tool_result 文本中的“成功”决定终态 |
| Codex threadId / turnId / itemId / RPC requestId | thread→Instance；turn→Run 的 native turn；item→UI node；request→Interaction | 一个 item 可有多个 approvalId/RPC callbacks；不能按 itemId 合并审批 |
| ACP sessionId / session/prompt RPC id / toolCallId | session→Instance；prompt 请求→Run；toolCallId→工具；反向 request RPC id→Interaction | ACP sessionId 不保证跨进程可 resume；以协商的 `loadSession` 为准 |
| agy conversation_id / step_index | conversation→Instance；同一 input 的 step→node，key 含 generation 与 conversation | `step_index` 不能全局唯一；不能把 agy 的 `event` 当 Claude 的 `type` |
| Herdr paneId，例如 `w5:p1` | transport carrier，必须附 serverIdentity/serverEpoch | pane ID 不是 agent session；pane 被复用后旧输入不能落到新程序 |

以上原生字段来自 Claude control-plane、Codex schema、Grok 协议调研、agy 样例、Herdr API。`processGeneration` 每次替换原生进程或加载新的原生执行环境加一；网络连接重建只换 `connectionEpoch`。仅 Hub↔Node 重连且 Node→CLI 未断时，两者均不变。`runGeneration` 是 Run 的原生执行世代，v1 固定从 `1` 开始且不提供重试同 Run 的操作；字段保留用于拒绝旧控制。native resume 恢复 conversation，不自动宣称恢复了旧 Run；新输入或可识别的 native continuation 创建新 Run。旧 Run 由历史证据补齐或保持 unknown；未来同 Run 跨 generation 执行须新增明确协商的方法。

## 2. 核心实体与生命周期

所有实体含 `EntityMeta`。表中字段没有 `?` 即为必填；读取必须包含其明确的空值或 Knowledge。Host 记录进入 Hub 所写的 host registry journal；Workspace/worktree 进入 Node 所写的 workspace registry journal，两者是不同 journalId。Instance/Run/Interaction 进入 Node 的 instance journal。Command 先进入 Hub inbox，交接后由 Node 写入目标 instance journal；workspace 命令写入 Node workspace registry journal。每条 journal 只有一个 owner，交接规则见 §7.2。

### 2.1 Host

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `label` / `ownerPrincipalId` | `string` / `Id` | 显示名与所属账号，label 可变 |
| `state` | `enrolled \| online \| offline \| reconciling \| retired` | Hub 所见连接状态，不代表 host 上的进程已经退出 |
| `nodeVersion` / `platform` | `Knowledge<string>` / `Knowledge<{os, arch, pathStyle}>` | pathStyle 为 `posix` 或 `windows` |
| `identityKeyId` / `nodeEpoch` | `Id` / `Knowledge<Id>` | enrollment 身份密钥引用；每次 Node 重启生成 epoch |
| `transport` | `{mode: outbound-wss \| ssh-tunnel, endpointRef: Id}` | 已登记的网络配置，不含 token |
| `lastSeenAt` / `leaseExpiresAt` | `Knowledge<Timestamp>` | Hub 的活性观测与写入租约截止时间 |
| `driverInventory` | `DriverDescriptor[]` | binary 路径、版本、digest、能力快照；见 §3 |
| `journalId` / `durableSeq` | `Id` / `U64` | Hub 所有的 host registry journal 与已持久 seq；Node 的 host.report 只是输入 |

~~~mermaid
stateDiagram-v2
    [*] --> enrolled
    enrolled --> reconciling: 身份验证
    reconciling --> online: 快照与账本对齐
    online --> offline: 连接或租约失效
    offline --> reconciling: 重新连接
    online --> retired: 注销
    offline --> retired: 注销
    enrolled --> retired: 撤销注册
    retired --> [*]
~~~

Host 被 retired 后禁止新命令，不因此自动杀进程。重新启用须新 enrollment；历史保留。失联时 UI 标注“主机离线，任务状态未知”，不把全部 Instance 改成 exited。

### 2.2 Workspace 与 worktree

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `hostId` / `label` | `Id` / `string` | Workspace 固定属于一个 Host |
| `rootPath` / `canonicalRoot` | `string` / `Knowledge<string>` | 用户请求的主机路径与 Node 验证后的真实路径 |
| `state` | `registering \| ready \| unavailable \| archived \| failed` | 可访问性和登记状态 |
| `repository` | `Knowledge<{repositoryId: Id, gitCommonDir: string, headOid: string}>` | 非 Git 项目为 not-applicable；工作目录不能冒充 repo ID |
| `worktree` | `WorktreeRecord \| null` | 当前 Workspace 若对应 worktree 则记录它 |
| `writePolicy` | `exclusive \| isolated-worktree \| shared-explicit` | 默认 exclusive；并行修改优先独立 worktree |
| `writerLeases` / `accessPolicyRevision` | `{instanceId: Id, fence: U64}[]` / `U64` | Node 执行的写入归属与授权版本 |
| `journalId` / `durableSeq` | `Id` / `U64` | Node 所有的 workspace registry journal 与水位 |

`WorktreeRecord` 字段：`id: Id`、`hostId: Id`、`repositoryId: Id`、`parentWorkspaceId: Id`、`path: string`、`branch: Knowledge<string>`、`baseOid: Knowledge<string>`、`headOid: Knowledge<string>`、`managedBy: runtime|native|external`、`state: creating|ready|unavailable|removed|failed`、`dirty: Knowledge<boolean>`、`createdByCommandId: Id|null`。分支名不是永久身份；checkout、reset、native worktree 行为作为新 observation 更新 head，不偷偷改同一 Run 的启动 cwd。

**路径约束（`security-review-2.md` M4/G5）。** worktree 名必须是单个安全段 `[a-z][a-z0-9_-]{0,31}`——不含 `/`、`\`、`.`，因此永远不是 `..` 或穿越路径。worktree 目录固定落在 `<repo>/../remuda-wt/<name>`：调用方给出的 `path` 只有解析后仍在该根内才接受，`repo` 不再从线上接受（Node 自己的 workspace root 就是仓库）。`CreateInstanceRequest.cwd` 同样受限，必须解析到已登记的 workspace root 或其旁边的某个 worktree 之内；`cwd` 省略时回落到 workspace root。两处共用 `remuda-protocol::path_guard`：先按字面消解 `..`，再对已存在的前缀 `canonicalize` 后做 `starts_with`，所以符号链接祖先不能用来离开该根。绝对路径、`..` 穿越、以及解析到根之外的符号链接一律拒绝，且拒绝发生在 `git worktree add` 建目录之前。MCP 的 `remuda_worktree_create` 不暴露 `path`/`repo`。

~~~mermaid
stateDiagram-v2
    [*] --> registering
    registering --> ready: Node 验证路径与仓库
    registering --> failed: 注册失败
    ready --> unavailable: 路径丢失或权限变化
    unavailable --> ready: 重新验证
    ready --> archived: 无新任务的归档
    unavailable --> archived: 归档
    archived --> registering: 显式重新启用
~~~

Instance close 不删除 worktree。移除 worktree 是独立命令，要求没有运行 owner、dirty 已知为 false；否则 `WORKTREE_BUSY` / `WORKTREE_DIRTY` / `STATE_UNKNOWN`。本规格不提供默认 force 删除。Herdr 的 worktree API 可以承载创建/打开，但清理责任与 runtime writer lease 仍由 Node 管理。Herdr worktree 方法

~~~mermaid
stateDiagram-v2
    [*] --> creating
    creating --> ready: 创建或现有 worktree 登记完成
    creating --> failed: 确认创建失败
    ready --> unavailable: 路径或仓库丢失
    unavailable --> ready: 原路径和身份重新验证
    ready --> removed: 独立 remove 命令确认成功
    unavailable --> removed: 核对原生登记已移除
    removed --> [*]
    failed --> [*]
~~~

### 2.3 Instance

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `hostId` / `workspaceId` | `Id` / `Id` | 创建后不变；跨主机迁移创建新 Instance 并记录关系 |
| `kind` / `driver` | `claude\|codex\|grok\|agy\|generic\|terminal` / `DriverKind` | kind 是原生产品，driver 是控制方式；`terminal` + `shell-pty` 是无 agent 的 login shell |
| `lifecycle` | `requested\|preparing\|starting\|ready\|closing\|exited\|failed\|unknown\|reconciling` | 进程/会话状态 |
| `activity` | `Knowledge<idle\|working\|waiting-interaction\|draining>` | 当前活动；附带 activityEvidenceEventIds |
| `connectivity` | `connected\|disconnected\|reconciling` | Node 到原生 carrier 的连通性，与 Host 状态独立 |
| `ownership` | `managed\|adopted-control\|observed-only` | 是否有创建/停止/写 stdin 权利 |
| `nativeRef` / `processRef` | `NativeRef` / `ProcessRef` | 保留每个 generation 的历史绑定 |
| `specRevision` / `launchId` | `U64` / `Knowledge<Id>` | 输入规格版本与物化后的 launch 清单 |
| `capabilities` | `CapabilitySnapshot` | 本 generation、settings、auth 下的有效能力 |
| `ownerFence` / `activeRunIds` | `U64` / `Id[]` | 单写 owner fencing token；默认一个 foreground Run |
| `parent` | `{instanceId: Id, runId: Id, commandId: Id}\|null` | runtime 调起的 child；与 native Workflow member 区分 |
| `journalId` / `durableSeq` | `Id` / `U64` | instance journal；换进程不重置 seq |
| `exit` | `Knowledge<{code: number\|null, signal: string\|null, observedAt: Timestamp}>` | 只能由 owner/supervisor 的退出证据赋值 |
| `lastError?` | `string` | 可省略的 driver/原生诊断文本，通常随 `lifecycle=failed` 写入；只是展示用摘要，不是状态、不是终态证据，也不能替代 `exit` 或 Run 的 `terminalEvidence` |
| `mode?` / `promotedAt?` | `native\|promoted` / `Timestamp` | 该 Instance 如何到达当前 `kind`。`promoted` 表示 `terminal` + `shell-pty` 实例的 PTY 前台被已知 agent CLI 接管（D-025）：`kind` 改为该 agent，`driver` 仍是 `shell-pty`，`promotedAt` 记录检测时刻。降级回 `terminal` 时 `mode` 复位为 `native` 且清空 `promotedAt`。只有 driver 的 `agent_promoted` / `agent_demoted` native lifecycle 能改这两个字段，同名 `agent_detected` diagnostic 只进 journal |
| `launchedBy?` | `remuda\|user` | **谁敲的这条命令**（D-028 §1.0 规则 4）。只记录出身，**不表示能力等级**：user 启动的 session 与 remuda 启动的享有完全相同的信号与能力。D-028 之前写入的行没有这个字段，读取时由 `mode` / `promotedAt` 推导——`promoted` 意味着一个 login shell 的前台被 agent CLI 接管，那条命令只可能是人敲的，因此推导为 `user`，其余为 `remuda`。它与 `mode` 并存而不取代它：`mode` 回答「kind 是不是提升来的」，`launchedBy` 回答「谁启动的」 |

~~~mermaid
stateDiagram-v2
    [*] --> requested
    requested --> preparing: 规格与资源接纳
    preparing --> starting: 物化成功并持久化派发意图
    preparing --> failed: 确定未启动的错误
    preparing --> closing: 未 spawn 的准备态显式 close
    starting --> ready: 原生初始化完成
    starting --> unknown: spawn 或握手结果不明
    starting --> failed: 确认启动失败
    ready --> closing: close 命令
    ready --> exited: 观测到退出
    closing --> exited: 进程树退出确认
    ready --> unknown: 控制连接或 owner 丢失
    closing --> unknown: 无法确认退出
    unknown --> reconciling: 查询 supervisor 与原生存储
    reconciling --> ready: 同一活进程重新接管
    reconciling --> exited: 确认进程已退出
    reconciling --> unknown: 证据不足
    exited --> preparing: 显式 native resume 新 generation
~~~

**`exited` / `failed` / `interrupted` 是三件不同的事（D-028 §5.3、§5.5）。** 混淆它们会让 UI 谎报：

| | 含义 | 判据 | 层级 |
| --- | --- | --- | --- |
| `exited` | 托管进程正常结束 | `child.wait()` 拿到退出码 0，或 PTY master EOF 先到且随后确认退出码为 0 | Instance `lifecycle` |
| `failed` | 确定的失败 | 非零退出码或被信号杀死；或确认的启动失败。**断线不升级为 failed** | Instance `lifecycle` |
| `interrupted` | 一条消息在发出前被取消 | 队列中的 user message 被 cancel/close 掉 | Message `status`（§5.2），**不是** lifecycle |

退出检测要**双证据**：一个 waiter task 等 `child.wait()` 拿退出码/信号，PTY master EOF 作为第二证据（`wait()` 竞态时可能先到）。任一先到即发 `native-exit`，并写明来源——今天 `read_pty` 在 EOF 只 `break`，结果是崩掉的 agent 在 UI 上永远显示 ready。

promoted 实例还要再分一层：前台 agent 消失是 **demote**（D-025，shell 还活着，实例不终结），只有 shell 自己退出才是实例 `exited`。停进程若无法确认进程组已消失，记 `stop-incomplete` 诊断，**不谎报 exited**。

一个 Instance 表示一个原生 session 及其进程托管关系，允许同一原生 session 在不同时刻使用替换进程；不允许两个活进程同时写它。`ready` 不保证空闲。`failed` 是确定启动失败，不把断线升级成失败。原生已在执行但无法 attach 时保持 unknown 或 observed-only；不能悄悄启动第二份。

### 2.4 Run

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `instanceId` / `hostId` | `Id` / `Id` | 一个 Run 不跨主机或 Instance |
| `cause` | `{type: command\|native-continuation\|external-input\|import, commandId: Id\|null, sourceEventId: Id\|null}` | 明确区分 UI 输入与 native 自行续跑 |
| `parentRunId` / `rootRunId` / `parentage` | `Id\|null` / `Id` / `known-root\|linked\|unknown` | linked 必须有 parent；另两种为 null。rootRunId 是当前已知关联图的根，unknown 时指自身，不声称不存在原生祖先 |
| `completionScope` | `native-turn\|task` | native-turn 是一次原生回合；task 要有包含续跑的任务终止证据 |
| `state` | `queued\|running\|waiting-interaction\|draining\|succeeded\|failed\|cancelled\|unknown\|reconciling` | succeeded 只意味着原生选定范围正常结束，不证明用户验收条件满足 |
| `processGeneration` / `runGeneration` | `U64` / `U64` | 第一次发送前固定；v1 不复用旧 Run 表示 resume 后的新执行 |
| `nativeTurns` | `{id: string, source: string, resultIndex: Knowledge<U64>}[]` | Claude 无 turn ID 时使用 adapter 的本地关联 ID并标 source=adapter，不冒充 native ID |
| `inputRef` / `inputDigest` | `Id\|null` / `Knowledge<Digest>` | 受控内容对象与摘要，不把 prompt 塞进日志行/进程标签 |
| `providerSelection` / `capabilitySnapshotId` | `ProviderSelection` / `Id` | 本次启动绑定，不随 registry 事后变化 |
| `startedAt` / `endedAt` | `Knowledge<Timestamp>` | 未开始/未结束用 unknown reason；无证据不编时间 |
| `terminalEvidence` | `Knowledge<{eventIds: Id[], ruleId: string, nativeOutcome: string}>` | adapter 版本化结算规则 |
| `stateConfidence` | `confirmed\|unknown` | 只评价当前 state 的证据充分性；冲突时 unknown，即使历史 state 是终态也不能继续报告确定成功 |
| `result` | `Knowledge<{messageIds: Id[], artifactIds: Id[], outputRef: Id\|null}>` | 空回复可能是合法成功，不能一概要求非空 |
| `outstandingWork` | `Knowledge<{workflowIds: Id[], childRunIds: Id[], detachedTaskIds: string[]}>` | native-turn 完成时仍可能有后台工作 |

~~~mermaid
stateDiagram-v2
    [*] --> queued
    queued --> running: 已确认原生接纳
    queued --> failed: 确认未执行且被拒
    queued --> cancelled: 派发前撤销
    running --> waiting_interaction: blocking 交互
    waiting_interaction --> running: 原生继续
    running --> draining: 回合结果已到但任务尚未结算
    draining --> running: 原生 continuation
    running --> succeeded: 对应范围的成功终态
    draining --> succeeded: task 终止证据
    running --> failed: 原生失败终态
    waiting_interaction --> cancelled: 原生取消确认
    running --> cancelled: 原生取消确认
    running --> unknown: 失去控制或缺失终态
    draining --> unknown: 终止语义不足
    unknown --> reconciling: 查询原生状态
    reconciling --> running: 找回活运行
    reconciling --> succeeded: 找回成功终态
    reconciling --> failed: 找回失败终态
    reconciling --> cancelled: 找回取消终态
    reconciling --> unknown: 仍不能确定
~~~

图中的 `waiting_interaction` 是 Mermaid 标识，对应 wire `waiting-interaction`。任意非终态在无法确认执行状态时可进入 unknown；图省略重复箭头。terminal state 不回退，后到的 usage、artifact、子活动可以补充记录，不因此重开 Run。冲突终态触发 journal 的 reconciliation 事件并令 stateConfidence=unknown，不覆盖旧证据；run.wait(terminal) 此时返回 reason=unknown，不把旧 succeeded 当确定结果。

### 2.5 Command：三态与投递账本

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `commandId` | `Id` | 等于 EntityMeta.id；客户端重试必须保持相同 ID |
| `actor` / `origin` | `ActorRef` / `ui\|bot\|mcp\|cli\|system` | 身份来自认证，不能信任请求自报 actor |
| `operation` | `instance.create\|instance.attach\|instance.resume\|instance.send\|instance.cancel\|instance.close\|instance.fork\|instance.configure\|interaction.respond\|tty.write\|workspace.register\|worktree.create\|worktree.remove` | 不认识的 operation 拒绝 |
| `target` | `{hostId: Id, instanceId: Id\|null, runId: Id\|null}` | create 也预分配 instanceId |
| `payloadRef` / `payloadDigest` | `Id` / `Digest` | 包含目标、generation、内容和配置引用的规范化摘要；secret 只哈希引用与版本，不哈希明文 |
| `expected` | `{instanceRevision?: U64, processGeneration?: U64, runGeneration?: U64, ownerFence?: U64, interactionVersion?: U64}` | 变更操作要求其适用的 CAS 字段 |
| `state` | `queued\|accepted\|settled` | 仅这三种业务进度状态 |
| `authority` | `hub-inbox\|node-ledger` | 当前 canonical revision 的唯一写入者 |
| `forwardIntent` | `Knowledge<{hostId:Id,nodeEpoch:Id,hubRevision:U64,createdAt:Timestamp}>` | Hub 首次转发前持久化；存在即不能在未询问 Node 时自行过期或撤销 |
| `dispatch` | `not-dispatched\|intent-durable\|transport-written\|native-acknowledged` | 不把写 pipe 成功称为原生接纳 |
| `resolution` | `clear\|unknown\|reconciling` | 正交于 state；例如 queued + transport-written + unknown |
| `acceptance` | `Knowledge<{scope: native-input\|native-control\|runtime-resource\|tty-bytes, eventIds: Id[]}>` | tty-bytes 只说明 PTY 接收字节，不代表 agent 理解或开始任务 |
| `settlement` | `Knowledge<{outcome: completed\|rejected\|cancelled\|expired, resultRef: Id\|null, error: RuntimeError\|null}>` | send 的 completed 与相应 Run 范围绑定；approve 的 completed 与 broker native resolution 绑定 |
| `queuedAt` / `acceptedAt` / `settledAt` / `expiresAt` | `Timestamp` / `Knowledge<Timestamp>` / `Knowledge<Timestamp>` / `Timestamp\|null` | expiresAt 只约束尚未派发的命令，不是自动杀任务的 deadline |
| `nodeReceipt` | `Knowledge<{nodeEpoch: Id, ledgerRevision: U64}>` | Hub 持久排队与 Node 接收分开 |

~~~mermaid
stateDiagram-v2
    [*] --> queued
    queued --> accepted: 可引用的接纳证据
    queued --> settled: 派发前拒绝或过期或撤销
    accepted --> settled: 对应操作结算
    settled --> [*]
~~~

相同 `(principalId, commandId)`、相同 digest 返回原记录；不同 digest 返回 `COMMAND_ID_CONFLICT`。RPC id 只配对本次连接上的 response，不能替代 commandId。观察到最终原生结果而先前 ACK 丢失时，可以在一次持久事务中记录 accepted、settled 两个有序变更，不能声称收到从未收到的 ACK。

派发顺序固定为：Hub durable inbox → Node durable command receipt → Node 写 `intent-durable` 并 fsync → 唯一 driver owner 发一次原生操作 → 记录 transport-written → 原生证据推进 accepted → 业务证据推进 settled。进程恰好在发出与落账之间崩溃时，intent 已存在即视为“可能发送”；恢复先查原生历史/请求状态，没有原生幂等保证则保持 unknown，**绝不因为找不到 ACK 而重发**。无法证明没执行时也不能标 rejected；超时查询返回 unknown 的现状。

### Origin

D-017 明确：agent instance 不是受信任 principal。请求来源和被操作实例的创建来源分别记录；Human 创建的实例在随后调用 MCP 时仍是 Agent，Human 向 Agent 创建的实例发送输入仍是 Human。

Hub 从已认证设备记录的 `kind`（`human` / `bot` / `agent`）和绑定的 `instanceId` 解析来源。设备名、MCP tool arguments、请求体的 `origin` / `actor` / `parentInstanceId` 都不能提升权限。历史配对设备沿用 Human，bootstrap 登录可显式声明 `deviceKind: "bot"`；Agent 凭据只能由 Hub 绑定到已有实例后签发。`GET /v1/caller` 返回已验证的 `origin`、`instanceId`、`hostId` 与直接 `children`。`x-remuda-instance-id` 只能收窄权限，不能更换已绑定实例。

Hub 在 create 与 command 的载荷外层覆盖 `origin: "human" | "bot" | "agent"`，同时覆盖输入来源。经 WSS / SSH stdio 解码后，Node 在 create 的 driver options、每次 `PromptInput` 和 Command 中保留来源，materializer 使用独立的 `LaunchOrigin::{Human,Bot,Agent}`。缺失或未知来源、未声明来源的 driver options 默认 Agent；`CommandOrigin::Mcp` / `System` 不能隐式转换为 Human。现有 Command 的 `origin` wire enum 不扩展：Human 对应 `ui`，Bot 对应 `bot`，Agent 对应 `mcp`，`actor.actorType` 同时区分三类。

每次 Hub 转发 create 时，为新实例签发仅绑定该实例的设备 token，经已认证 carrier 单独交给 Node。Node 把 `REMUDA_INSTANCE_ID`、`REMUDA_HOST_ID` 和该 `REMUDA_TOKEN` 注入实例子进程；WSS 使用其已配置的 Hub 地址作为 `REMUDA_HUB`，SSH stdio 部署须提供该地址。token 不进入持久化 command、HTTP create 返回体、request digest 或 launch recipe。实例内 CLI 不回退到 bootstrap 环境变量或仓库 token 文件。Human 可用 `POST /v1/instances/{id}/mcp-token` 为该实例配置外置 MCP，返回的仍是 Agent token。人类设备直接运行、没有实例绑定的 coordinator `claude -p --mcp-config …` 保持 Human。

| 动作 | Agent 来源 | Human / Bot 来源 |
| --- | --- | --- |
| send / stop / rm 自己或直接创建的子实例 | 可执行；不扩展到兄弟、祖先或孙实例 | 可执行 |
| 同 host create | 可执行，Hub 在 create 时持久记录 `parentInstanceId`；省略 MCP host 时使用 caller host | 按 placement 执行 |
| 跨实例 send / stop / rm、跨 host create | 人类批准精确动作后执行一次 | 可执行 |
| `remuda_instance_keys` / `tty.write` | 即使目标为自己或子实例也必须审批；新 `shell-pty`（含 `shell` / `terminal` alias）create 和 send 也经此门控；无原始 follow/tty 写通道 | 可执行 |
| `remuda_fleet_send {all:true}` / `remuda_fleet_keys {all:true}` / `fleet send --all` / `fleet keys --all` | 明确拒绝，`confirm` 或审批都不能豁免 | 必须显式 `confirm:true` / `--confirm` |
| filters 选择的 fleet send / keys | Hub 先检查整个目标集合；send 只含自己/直接子实例时可执行，跨实例或 keys 必须审批后才排队 | 可执行 |
| 本地 MCP merge / worktree create | 无实例范围的执行目标，拒绝；使用已有 workspace 创建子实例 | coordinator 可执行 |

`POST /v1/fleet/broadcast` 与 MCP/CLI 共用全量发送约束；`all:true` 必须另带 `confirm:true`，Agent 不接受此豁免。

MCP `call_tool` 在副作用前决定范围，并把需审批动作送到 Hub broker gate；Hub 重新依据认证与真实目标检查后才排队或转发。需要审批时 HTTP 返回 `409 HUMAN_APPROVAL_REQUIRED` 和 `interactionId`，MCP 将其作为明确的工具错误返回。票据显示在 `GET /v1/interactions`，只能由 Human 设备通过既有 `/v1/interactions/{id}/answer` 提交 `allow-once` / `deny` 及匹配的 `inputDigest`。随后用 MCP `approvalId`（HTTP `x-remuda-approval-id`）重试完全相同的动作。grant 绑定 caller device、caller instance/host、操作、目标 host/instance 和完整有效载荷；更改文本/目标、其他设备使用、重复消费、拒绝、过期均不能执行。复用 `InteractionBroker` 的 first-answer-wins，TTL 为 15 分钟；Hub 重启丢弃未消费 grant，必须重新申请审批。超时或 ACK 丢失不自动重放已批准的动作。

Agent create 的 generic-pty 在 materialize 前把 bypass/dontAsk 降为 asking mode，因而得到非 yolo argv。preset bypass flags 仅在 `permissionMode=bypassPermissions` 且 Human/Bot 时合并；默认模式绝不追加。D-011 的 Bot materializer 拒绝 bypass/dontAsk 规则继续存在，因此此条件不授予 Bot 额外豁免。Agent 的结构化 Claude create 显式请求 bypass/dontAsk 会被拒绝，省略权限模式则采用 manual。

此范围约束覆盖 Hub 与 MCP 控制面。Node stdio 的认证依赖其 SSH carrier，本地开发 Node HTTP 的 access code 仍是操作员凭据；同 UID 文件、原生 CLI 与 herdr 的进程隔离和凭据清理是独立边界（security-review-2 tasks 9/12/13），不能把 parent-child 范围检查描述为 OS sandbox。

### 2.6 Interaction

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `instanceId` / `runId` / `hostId` | `Id` / `Id\|null` / `Id` | 独立 MCP elicitation 可能不属于 Run |
| `kind` | `approval\|question\|plan-review\|elicitation` | UI 展示与 answer 验证按 kind 分支 |
| `requestKey` | `{native: NativeRequestKey, processGeneration: U64, runGeneration: U64\|null, connectionEpoch: Id}` | 无 native request ID 的 hook 用本地 invocationId |
| `requestVersion` | `U64` | 同一个待决请求的可回答内容变更时加一；状态刷新只改 revision |
| `state` | `pending\|answer-committed\|resolved\|expired\|invalidated\|unknown\|reconciling` | answer-committed 只表示决定落盘，不等于原生采纳 |
| `blocking` / `answerable` | `boolean` / `boolean` | 屏幕提示通常不可通过 broker 回答；可切到原生终端 |
| `carrier` | `claude-control\|claude-hook\|codex-rpc\|acp-rpc\|native-tty\|unsupported` | 一个原生请求只能有一个响应 carrier |
| `request` | `InteractionRequest` | §5.4 的各类 schema |
| `deadline` | `Knowledge<Timestamp>` | 未给原生 deadline 时未知；runtime 可单独设置 answer deadline 并说明后果 |
| `deadlineSource` | `native\|runtime-policy\|none\|unknown` | native/runtime-policy 对应已知 deadline；none 表示明确无截止时间，不能与未获取到期限混淆 |
| `answer` | `Knowledge<{commandId: Id, actor: ActorRef, value: InteractionAnswer, committedAt: Timestamp}>` | Node CAS 决出的唯一答案 |
| `delivery` | `not-sent\|intent-durable\|written\|confirmed\|rejected\|unknown` | 写回不等于 confirmed |
| `resolution` | `Knowledge<{reason: answered\|native-cleared\|native-cancelled\|generation-ended\|timed-out, eventIds: Id[]}>` | resolved/expired/invalidated 必须说明证据 |

~~~mermaid
stateDiagram-v2
    [*] --> pending
    pending --> answer_committed: Node CAS 成功
    pending --> expired: 明确 deadline
    pending --> invalidated: 原生撤销或 generation 结束
    answer_committed --> resolved: 原生请求已解决的证据
    answer_committed --> unknown: 写回结果不明
    pending --> unknown: 响应通道丢失
    unknown --> reconciling: 核对原生 pending 请求
    reconciling --> pending: 同一请求仍有效且从未提交答案
    reconciling --> answer_committed: 已有答案且可证明未发送
    reconciling --> resolved: 原生已解决
    reconciling --> invalidated: 旧请求已不存在
    reconciling --> unknown: 无法判定
~~~

图中的 `answer_committed` 对应 wire `answer-committed`。同一请求重放不产生第二个卡片；有答案的请求永不回到可回答 pending。`resolved` 只表示原生不再等待，不保证选择了 allow，也不保证工具已执行成功；必须读 resolution、delivery 与 application 字段。

### 2.7 工作区当前变更（只读 SCM 快照）

工作台「工作区当前变更」视图只由 Node 对**注册工作区当前工作树**的实时只读计算支撑（[files-view-contract.md](./files-view-contract.md) §3）。这是三个 Hub→Node JSON-RPC 与三个 operator-only Hub REST 代理；不新增 observation kind、不写 journal、Hub 不缓存任何文件内容或 diff。方法名进入 Node 的显式 `match`，不能依赖 WSS 派发的 `{"ok":true}` 兜底。

**只读与授权边界。**

- 定轴只接受 `workspaceId`（`workspace.scm.file`/`.diff` 另加相对 `path`），不接受绝对路径、`repo`、`base`。Node 必须命中自己的注册表并重新通过 canonical 身份校验；未注册或根已迁移 → `NotFound`（视图「工作区不存在」）。
- 内容路径在读取前用 `path_guard::absolutize/real_path/contain` 以注册根为唯一允许根重新校验，拒绝绝对路径、`..` 穿越、glob/`:(` pathspec magic，以及解析到根外的符号链接；最终叶子以 `O_NOFOLLOW` 打开，闭合校验与读取之间的 symlink 替换窗口。
- git 以 argv 数组调用（无 shell）：`--no-optional-locks`、`current_dir("/")`、`LC_ALL=C`、`GIT_OPTIONAL_LOCKS=0`、`GIT_TERMINAL_PROMPT=0`，且每个子命令必须命中固定白名单模板（`rev-parse --show-toplevel` / `rev-parse HEAD` / `symbolic-ref --quiet --short HEAD` / `status --porcelain=v2 -z --untracked-files=all` / `diff --no-color --no-ext-diff [--cached] -- <path>`）；路径只能出现在字面 `--` 之后。每次调用走有界、可杀进程组、有 5 秒截止与输出字节上限的 helper。

| JSON-RPC（Hub→Node，只读） | 参数 | 结果要点 |
| --- | --- | --- |
| `workspace.scm.status` | `{workspaceId}` | `availability: ok|unsupported|denied`、`headOid`、`branch`、`observedAt`、`entries[]`（相对 `path`、经典两字符 `xy`、`kind`、`sizeBytes`、`oldOid/newOid`，列表期 digest 为 `unknown("not-collected")`；重命名条目额外带 `origPath`）、`limits`、`truncated` |
| `workspace.scm.diff` | `{workspaceId, paths:[…], staged:bool}` | 每路径 `{path, patch|null, binary, truncated, bytesAvailable}`，总量受 `maxDiffBytes` 封顶并回传 `truncated.diffBytes/bytesOmitted` |
| `workspace.scm.file` | `{workspaceId, path}` | 未跟踪/新文件当前字节：`mediaType`、`binary`、`sizeBytes`、`digest:{known,"sha256:…"}`、`content|null`、`truncated`；超 `maxFileBytes` 不返回内容，二进制不内联 |

`status` 的可用性穷举为视图六态（无变化 / 尚未采集 / 不支持 / 权限不足 / 离线 / 工作区不存在），前端不得合并成「加载失败」。非 git 目录的判定是 `rev-parse --show-toplevel` 结果**等于注册根本身**，避免祖先目录恰好是仓库时误判为受支持；探针被拒或超时映射为 `availability:"denied"`。

| Hub REST（operator-only，纯代理、无缓存） | Node 方法 | 离线语义 |
| --- | --- | --- |
| `GET /v1/hosts/{id}/workspaces/{workspaceId}/changes` | `workspace.scm.status` | host 不在线 → 409 `HOST_OFFLINE`；会话不可写 → 422 `PLACEMENT_UNSATISFIABLE` |
| `GET …/changes/diff?path=&staged=` | `workspace.scm.diff` | 同上 |
| `GET …/changes/file?path=` | `workspace.scm.file` | 同上 |

会话内 agent 凭据对三个 REST 一律 403 fail-closed。响应不含 `instanceId`/`turn` 字段，UI 标题恒为「工作区当前变更」；列表与内容是两次独立采集，`headOid`/`sizeBytes`/digest 不一致时前端只显示「内容在采集后变化，请刷新」并提供手动刷新，不自动轮询、不静默替换。

## 3. Driver 接口与能力声明

### 3.1 接口

~~~typescript
type DriverKind = "claude-print" | "claude-pty" | "claude-bg" | "codex-appserver"
  | "grok-acp" | "agy-print" | "generic-pty" | "shell-pty";
// D-035: `claude-print` 是**显式专用**的诊断 carrier，不是任何默认或回落值——
// 它跑完一轮即结束（需要手工 resume），作为 worker carrier 无用。默认选择顺序：
// 宿主 driverInventory 报 shell-pty launchable -> shell-pty；否则 herdr 已广播
// -> claude-pty（codex/grok 为 generic-pty）；两者皆无 -> 以理由拒绝（不回落 print）。
// Node 侧请求的 carrier 构造失败时一律 `instance.create` 带 reason code 拒绝，不降级。
type CallContext = {
  commandId: Id; instanceId: Id; runId: Id | null;
  ownerFence: U64; processGeneration: U64; runGeneration: U64 | null;
};
type DriverAck = {
  dispatch: "not-dispatched" | "transport-written" | "native-acknowledged";
  nativeIds: Record<string, string>;
  evidenceRawIds: Id[];
};
type DriverInput =
  | { type: "prompt"; mode: "new-turn" | "steer" | "queue"; blocks: ContentBlock[];
      origin: "human" | "bot" | "agent"; nativeClientMessageId: string }
  | { type: "steer"; expectedNativeTurnId: string; blocks: ContentBlock[];
      nativeClientMessageId: string }
  | { type: "model-switch"; modelId: string; effective: "next-turn" };
type AttachRef = {
  nativeRef: NativeRef; processRef: ProcessRef;
  mode: "observe" | "control"; allowWake: false;
};
type ResumeRef = {
  nativeRef: NativeRef; previousProcessGeneration: U64;
  launch: MaterializedLaunch; mode: "resume";
};
interface Driver {
  capabilities(): Promise<CapabilitySnapshot>;
  start(spec: MaterializedLaunch, context: CallContext): Promise<DriverAck>;
  attach(ref: AttachRef, context: CallContext): Promise<DriverAck>;
  send(input: DriverInput, context: CallContext): Promise<DriverAck>;
  cancel(context: CallContext): Promise<DriverAck>;
  respondInteraction(id: Id, answer: InteractionAnswer,
    context: CallContext): Promise<DriverAck>;
  close(context: CallContext): Promise<DriverAck>;
  resume(nativeRef: ResumeRef, context: CallContext): Promise<DriverAck>;
  observations(): AsyncIterable<DriverRecord>;
}
type DriverRecord =
  | { type: "raw"; source: ObservationSource; bytes: Uint8Array }
  | { type: "native-exit"; processRef: ProcessRef; code: number|null; signal: string|null }
  | { type: "transport-state"; state: "connected"|"disconnected"; connectionEpoch: Id };
~~~

**三种输入模式（D-028 §6）。** `PromptMode` 从只有 `new-turn` 扩为三个：

| mode | 含义 | 与 `type: "steer"` 分支的关系 |
| --- | --- | --- |
| `new-turn` | 开启新回合。实例空闲时的默认，语义不变 | 无关 |
| `steer` | 当前回合**正在跑**时插入文本，期望被本回合吸收 | 这是「没有 expectedNativeTurnId 可用」时的表达。`type: "steer"` 分支要求调用方指定确切的 native turn id，因而只有结构化 driver 能用；PTY 载体上根本观测不到 turn id，只能表达「现在发进去」这个意图。两者并存不重复：一个是**定向到某回合**，一个是**定向到当下** |
| `queue` | 明确排队，等当前回合结束再投递 | 无关。对应 D-022 的 `pty_queue` 与 claude transcript 里的 `queue-operation` 账本 |

**能力必须诚实。** `steer` / `queue` / `interrupt` 三项 capability 在实测前一律 `unknown`（§14 风险 6、7：claude「回合中打字+Enter 到底是排队还是 steer」尚无结论，grok / agy 的键位全未验证）。调用 `unknown` 的能力返回 `CAPABILITY_UNKNOWN`，UI 显示「尚未验证」——不是灰按钮假装不支持。已排队但在发出前被取消的消息按 §5.2 更新为 `status: "interrupted"`，不留在 `queued` 里骗人。本轮只开协议面，不改任何 driver 行为。

这里的接口是 Node 进程内契约；wire 传输不直接序列化 Uint8Array。start/attach/resume 只建立连接和原生身份，不顺带发送 prompt；`instance.create` 可包含 initialInput，Node 将其作为独立 send 子命令并在 create 返回中提供两个 commandId，避免把“已启动”当“任务完成”。DriverAck 的 transport-written 不推进 native-input accepted；adapter 解析后追加的 observation 才可能提供接纳证据。

每个 Instance 仅一个 driver handle 写控制流；读线程必须持续排空 stdout/stderr，与等待 Interaction 的线程分离。cancel 只请求取消指定 Run，close 终止这个 Instance 的托管进程并保留 native session；取消后仍等待对应终态。UI 关页、Hub WS 断开、`events.unsubscribe` 都不调用 close。observe/control attach 只能认领已存在的 transport；运行 `claude attach` 创建 pane 始终是有副作用的 `instance.open_terminal`，即使目前 job 仍存活也不能由只读 attach 代发。它要求显式人类动作与 `allowWake:true`；没有此授权返回 `ATTACH_WOULD_WAKE`。control-plane §1.5–1.6

resume 只接受明确 NativeRef，禁止 `--continue` / “最近会话”选择器；原生数据不存在返回 `NATIVE_SESSION_NOT_FOUND`，不自动 start 新 session。不可逆的 process exit 确认之前不 resume。resume 命令只结算 native conversation 的恢复：原生若自行续跑，Node 记录 cause=native-continuation 的新 Run，并只在有明确源 ID 时关联旧 Run；原 prompt 不重投，旧命令不因 resume 成功而 settled。fork 作为可选扩展 `fork(ref, boundary:{type:"latest-terminal"}, newInstanceSpec)`，返回新 Instance 与新 native session，要求来源 Instance 无活 foreground Run、最新 native 回合已结算，且在 owner 内串行检查 expected instance revision。v1 不承诺任意历史 turn 切点；不支持时返回 `CAPABILITY_UNSUPPORTED`，不能复制 UI 文本冒充 fork。


**会话续接（D-026）。** 上面这段说的是 driver 层的 resume；面向用户的「继续这段对话」再加一层：每个 Claude 实例把驱动实际上报的 session id 写进 `nativeRef`（claude-print 来自 stream-json `system/init` 映射的 `session` lifecycle，claude-pty 来自 SessionStart hook，并带 `transcriptPath`），create 时按 Instance id 造的占位值只在首次上报前有效。续接**不复活**退出的进程，而是在同一 host / workspace 上新建 Instance：沿用父实例的 provider / permission / model / cwd，由 materializer 发 `--resume <sessionId>`，子实例 `parent` 指向父实例，父子各 journal 一条 lifecycle，旧实例保持 exited。driver kind 可以与父实例不同——`claude-pty` 续一个原本 structured-only 的 session，就是「让这段对话回到终端」。Hub 侧是 `POST /v1/instances/{id}/resume`（Human/Bot；Agent 403），没有上报过 session id 或退出过久都返回 409 并说明原因，而不是开出一个看起来连续、实际为空的新对话。

### 3.2 能力对象

~~~typescript
type CapabilityName = "resume" | "steer" | "queue" | "interrupt" | "model-switch"
  | "fork" | "structured-workflow" | "artifact" | "tty-attach" | "hooks"
  | "interactive-approval" | "question" | "plan-review" | "elicitation"
  | "live-attach" | "completion-native-turn" | "completion-task";
type CapabilityProvision = "native" | "emulated" | "unknown";
type Capability = {
  state: "supported" | "unsupported" | "unknown";
  provision?: CapabilityProvision; // D-028 §6；缺省读作 unknown。
  scope: string[];
  reasonCode: string;
  prerequisites: string[];
  evidence: { type: "fixture" | "native-negotiation" | "source" | "help";
    ref: string; digest: Knowledge<Digest> }[];
};
type CapabilitySnapshot = {
  id: Id; driverKind: DriverKind; adapterVersion: string;
  adapterTransport: "native-rust-wire"|"claude-sdk-sidecar"|"claude-pty-herdr"
    |"claude-bg-herdr-attach"|"codex-appserver-spawn"|"codex-embedded"
    |"grok-acp"|"agy-native"|"generic-herdr";
  binaryVersion: string; binaryDigest: Digest;
  nativeProtocolVersion: Knowledge<string>;
  settingsRevision: U64; providerProfileRevision: U64;
  capabilities: Record<CapabilityName, Capability>;
};
type DriverDescriptor = {
  kind: DriverKind; adapterVersion: string; binaryPath: string;
  binaryVersion: string; binaryDigest: Digest;
  launchable: boolean; reasonCode: string;
  capabilities: CapabilitySnapshot;
};
~~~

`provision` 与 `state` 正交：`state` 回答「这件事能不能做」，`provision` 回答「**谁**来做」——`native` 是 harness 自己的语义，`emulated` 是 Remuda 代劳。这个区别必须对用户可见（D-028 §6）：emulated 的队列存在 Remuda 自己的账本里、chip 可增删改，native 的队列在 harness 内部、Remuda 改不了；把后者包装成前者会让「移除排队项」这个按钮静默失效。缺省读作 `unknown`——旧 peer 没说过谁提供，替它断言 `native` 就是凭空造证据。三项的逐 harness 实测结论见 [native-pty-first §6](./native-pty-first.md#6-steer--排队--打断)；本规格只定义字段，不替 driver 填值。

`queue` / `interrupt` 是 D-028 §6 新增的名字。旧 peer 序列化的 snapshot 里没有它们，**缺失读作 `unknown`**：「对方没提」不是「对方不支持」的证据。这与 `NativeRef.signalTier`（§1.3）是同一条原则的两面——运行时上报缺省时静态矩阵继续生效，静态矩阵里没有的名字缺省时读作未验证。

CapabilitySnapshot 的 adapterTransport 必填，Rust print 与 SDK sidecar、spawned Codex 与 embedded Codex 分开保存证据；替换 transport 必须创建新 Instance，不能沿用另一 transport 的 supported。native 可用、adapter 已实现、配置允许、所选 build 的验证通过，这四项同时成立才返回 supported。源码/help 只证明候选能力存在，不自动打开生产功能。unknown 不当成 false，也不当成可调用；调用返回 `CAPABILITY_UNKNOWN`，UI 显示“尚未验证”与原生通道。snapshot 必须随每个 generation 保存；native 动态能力变化生成新 snapshot，通过 lifecycle 事件宣布，只影响后续操作。

### 3.3 每个 driver 的候选能力矩阵

`S*` 表示原生有证据，仍需所列条件和 adapter 验收；`N` 是本 driver v1 明确不提供；`U` 是证据不足，默认 unknown。矩阵不是“runtime 已实现”的声明。

| driver / 对照版本 | resume | steer | model-switch | fork | structured-workflow | artifact | tty-attach | hooks |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `claude-print` / 2.1.268 | S* `--resume`，同 store，重申 settings/model | U；持续输入不等于针对当前 turn 的 steer | S* SDK `set_model`，只在确定 idle 时 | S* `--resume` + `--fork-session`，返回新 ID | S* Workflow tool + task events + journal；完整 phase schema 未验证 | N 当前实测 init 无 Artifact；未来逐 profile 验证 | N，print 没有可 attach 的原生 TUI | S* 保留原生 hooks；stream hook lifecycle 与 hook 输入分开 |
| `claude-pty` / 2.1.268 | S* 明确 `--resume` | N 语义 steer；只可人控 TUI 输入 | S* 仅原生 TUI `/model`，结构化 RPC 切换 U | S* 重新启动原生 fork；不复用活 session writer | S* 原生 Workflow 完整执行；结构化观察 partial，原生 `/workflows` 保留 | S* TUI + native entitlement；统一 artifact API U | S* carrier 真正支持 terminal attach 时 | S* settings 保留；runtime probe 需注册成功 |
| `claude-bg` / 2.1.268，carrier 待运行验收 | U；不能以 attach wake 冒充 resume | N 语义 steer | U；只保留原生 attach TUI 的候选能力 | U | U；需独立 journal fixture | U；需原生登录/entitlement 与 attach 验收 | S* 显式 open_terminal 建 pane 后才可订阅 | U；blocking hook 的审批所有权独立验收 |
| `codex-appserver` / 明确路径 0.154.0 | S* `thread/resume` | S* `turn/steer` + expectedTurnId | S* 后续 `turn/start.model`，不换当前 turn | S* `thread/fork` | N Claude Workflow；Codex 自身 collab/plan 另行观察 | S* 文件/图片/MCP resource 的已知 item；N Claude Artifact | N 此 driver 不提供 Codex TUI；另开 PTY driver 才可 | S* native hooks 与 hook notifications；信任规则须有效 |
| `grok-acp` / 1.0.25 | S* `session/load` 同/新进程已验证；`session/resume` 仅同进程实测 | N v1；有扩展但未验证不冒充 steer | U，configOptions 有 model，但文档 set_config_option 输入实测 -32602 | U `_x.ai/session/fork` 仅字符串证据，v1 不启用 | N Claude Workflow；ACP plan 不升格 workflow | S* 已知 tool content/diff；应用专用 Artifact U | N，stdio ACP 不含 TUI | S* native hooks 与 hook 通知；完整控制覆盖 U |
| `agy-print` / 1.2.1 | S* `--conversation`，store 可用 | N v1；持续 stdin 的语义 U | N live；新进程 `--model` 与 resume 兼容性 U | U，help 未建立 | N Claude Workflow；agy task/subagent schema U | U，工具存在不证明输出协议 | N | U，不能把 Gemini hooks 表当 agy 已验证协议 |
| `generic-pty` / binary pin | N 默认；产品专用恢复由新 driver 声明 | N | N 语义 API；允许人控键盘 | N | N | N 语义 artifact；普通文件浏览另计 | S* 自有 PTY 或已验证 carrier | N 默认；自定义探针须另建版本化 adapter |
| `shell-pty` / login `$SHELL` | N | N | N | N | N | N | S* portable-pty；kind=`terminal`；无法识别的 agent CLI 的 fallback | N |

本表是**静态 fallback**，不再是唯一真相（D-028 §4.3）。表格按 `DriverKind` 取值，因此 promoted 的 `shell-pty` session 无法用它表达自己实际拿到的能力；`NativeRef.signalTier` / `NativeRef.capabilities` 的运行时上报**逐项盖过**本表，缺省时本表生效。

`steer` / `queue` / `interrupt` 三列在 D-028 本轮对**所有** driver 都是 `U`。不是遗漏：codex 的 `Tab` 排队与 `Esc` 打断虽已实测，但 Remuda 尚未实现对应键位，而 claude 的 queue-vs-steer 语义与 grok / agy 的键位根本没有结论（§14 风险 6、7）。写 `N` 会是一句没有依据的断言。

`shell-pty` 另有 **terminal → agent promotion**（D-025）：轮询 PTY 前台进程组，已知 agent CLI 接管时把实例 promote 成该 kind（`driver` 不变，见 §2.3 `mode`），Claude 另按 SessionStart 同款 transcript 路径水合结构化消息。promote 本身不改变本表这一行——但 promote 之后拿到的 `signalTier` 会：一个把 hook socket 接起来的 session 上报 `resume` / `hooks` / `interactive-approval`，而这三项在本表的 `shell-pty` 行里都是 `N`。这正是运行时上报存在的理由。

`shell-pty` 同时是 D-028 §5.1 的 **agent-in-native-pty** 载体：`(claude|codex|grok|agy, shell-pty)` 组合现在合法，与 promoted 的终端跑在同一条路径上。载体相同、能力就相同，这是统一原则要求的结果。

依据：agent-protocols §2–5、§9、Claude control-plane、Codex schema、Grok 专项实测、hooks-integrations。Grok 的 plan、Codex collab 与 runtime child 不能用 `engine=claude-workflow`。claude-pty 无法自动回答的提示继续留在原生 TUI，不以降级 print 代替。

额外能力：codex-appserver 的 approval/question/elicitation 有 schema 证据但实审批尚未触发；grok-acp 的 approval 也只到协议候选，现有 always-approve 探针没有任何 `session/request_permission`，非 yolo 回填仍须验证。claude-print 的 interactive-approval allow/deny 与 AskUserQuestion 单选已有 2.1.268 实测，均可作为 S* 的 adapter fixture；多选/自由输入/plan-review/elicitation 各自 unknown，不能扩大单选证据。Claude 交互探针 agy-print/generic-pty 的统一 broker 回答默认 N。`completion-native-turn` 对结构化 Claude/Codex/Grok/已识别 agy result 可验证为 S*；`completion-task` 必须单独通过 §5.8 的终态规则，Claude 持续输入含后台续跑的整体 task 当前为 U，不能借用 native-turn 能力。

## 4. InstanceSpec 与 Launch materializer

### 4.1 输入与物化结果

~~~typescript
type EnvBinding =
  | { source: "literal"; value: string; visibility: "private" }
  | { source: "credential"; credentialRef: Id; version: U64 }
  | { source: "host-env"; name: string };
type PermissionMode =
  | { kind: "claude"; mode: "manual"|"auto"|"acceptEdits"|"dontAsk"|"plan"|"bypassPermissions";
      interaction: "host"|"native-tty" }
  | { kind: "codex"; approvalPolicy: "untrusted"|"on-request"|"never";
      approvalsReviewer: "user";
      execution: {sandbox: "read-only"|"workspace-write"|"danger-full-access"}
        | {permissions: string} }
  | { kind: "grok"; mode: "native-prompt"|"auto"|"always-approve" }
  | { kind: "agy"; mode: "native"|"accept-edits"|"plan"|"always-proceed" }
  | { kind: "generic"; mode: "native" };
type InstanceSpec = {
  schemaVersion: 1;
  host: Id; workspaceId: Id;
  kind: "claude"|"codex"|"grok"|"agy"|"generic"|"terminal";
  driver: DriverKind;
  binaryRef: Id;
  cwd: string;
  worktree?: {mode: "existing"; worktreeId: Id}
    | {mode: "create"; worktreeId: Id; baseOid: string; branch: string};
  providerProfile: {id: Id; revision: U64};
  modelId: string|null; // null 请求 owning profile 的显式默认解析。
  effort?: {name: "low"|"medium"|"high"|"xhigh"|"max"; ultracode: boolean};
  permissionMode: PermissionMode;
  env: Record<string, EnvBinding>;
  args: string[];
  settingsOverlay: {format: "claude-json"|"codex-toml"|"grok-toml"|"agy-json"|"none";
    objectRef: Id|null; revision: U64};
  nativeHome: {mode: "registered"; storeId: Id};
  carrier: {type: "stdio"}
    | {type: "pty"; backend: "herdr"; server: HerdrServer; session: string}
    | {type: "claude-bg"; inputDelivery: "deferred-argv"; argvInputPolicy: "explicit-non-secret"};
  requiredCapabilities: CapabilityName[];
  completionScope: "native-turn"|"task";
  parent: {instanceId: Id; runId: Id; commandId: Id}|null;
};
type ProviderSelection = {
  profileId: Id; profileRevision: U64; endpointId: Id;
  ingress: "anthropic-messages"|"openai-responses"|"openai-chat"|"gemini-native"|"native-login";
  credentialRef: Id|null; credentialVersion: U64|null;
  modelRequested: string; modelResolved: Knowledge<string>;
  selectionReason: "pinned"|"weighted-healthy"|"explicit-recovery";
};
type MaterializedLaunch = {
  launchId: Id; instanceId: Id; specRevision: U64;
  binaryPath: string; binaryVersion: string; binaryDigest: Digest;
  argv: string[]; cwd: string;
  inputDelivery: "stdio"|"tty"|"deferred-argv";
  env: Record<string, string>; // Node 内存专用，禁止进入通用日志和 Hub response。
  files: {path: string; role: "settings"|"mcp-config"; contentDigest: Digest;
    permission: "0600"; lifetime: "launch"|"native-store"}[];
  nativeStoreId: Id; providerSelection: ProviderSelection;
  audit: {envNames: string[]; credentialRefs: Id[]; redactedArgv: string[];
    settingsDigest: Digest; inheritedConfigDigests: Digest[];
    approvalAuthority: "runtime-host"|"runtime-hook"|"native-tty"|"unknown";
    prohibitedOptionsChecked: true};
};
~~~

**Claude effort（D-028 §9.1）。** 五档 `low` / `medium` / `high` / `xhigh` / `max`，默认 `high`（成本基准 1.0）。`ultracode` **不是第六档**，是正交 boolean（等价 `xhigh` + dynamic workflow），session-only、永不持久化为档名。落地为 argv 上的**一个** flag：`--effort <档名>`，置了 ultracode 时为 `--effort ultracode`（原生 flag 只收一个值）。`effort` 缺省表示「交给 harness 决定」，materializer **不发** `--effort`——这与请求默认档不是一回事。

历史档名按**名字**归一，绝不按 index：`default→low`、`think→high`、`think-hard→xhigh`、`ultracode→xhigh + ultracode:true`，无法识别的名字落到 `high`（一个过期的 UI 字符串不该让启动失败）。按 index 归一会出错，因为旧表是 per-harness 且长度不同——claude 的 index 3 是 `ultracode`，codex 的 index 3 是 `ultra`。旧客户端发来的 `{index, name}` 仍然接受，`index` 在读取时被忽略、也不写回。

**Codex effort（codex-cli 0.154.0）。** 六档按原生 picker 顺序展示，默认 `medium`。`max` 和 `ultra` 原样持久化并传入 `-c model_reasoning_effort="<v>"`；历史输入 `minimal` 归一为 `low`，未知名字回落到默认档。`ultra` 是 Codex 的真实档位，Claude/agy/Grok 拒绝它；`ultracode` 仍是 Claude-only workflow flag。跨 harness 历史名字只按名字读取，不按旧 index 推测。

| wire value | picker name | subtitle / tooltip |
| --- | --- | --- |
| `low` | Low | Fast responses with lighter reasoning |
| `medium` | Medium | Balances speed and reasoning depth for everyday tasks |
| `high` | High | Greater reasoning depth for complex problems |
| `xhigh` | Extra high | Extra high reasoning depth for complex problems |
| `max` | Max | For difficult problems when quality matters more than speed · higher usage |
| `ultra` | Ultra | For demanding work using multiple agents · highest usage |

Max 使用与 Claude max 相同的静态强调；Ultra 使用最强的 ember 效果，与 Claude ultracode 共用视觉效果但不设置其 flag。验证边界及 390/1440 截图见 [effort-codex-tiers-1](evidence/effort-codex-tiers-1.md)。

**绝不使用 `CLAUDE_CODE_EFFORT_LEVEL`**：它的优先级高于会话内 `/effort`，会把 PTY 的实时改档钉死。反向地，`child_env` 必须从子进程环境中**剥离**宿主继承的该变量，否则外部环境静默覆盖一切。

`MaterializedLaunch` 是进程内结构，含 env 值的部分不准序列化到 RPC、Observation 或错误。持久 LaunchManifest 只存 audit、配置对象引用、binary/cwd/nativeStore/ProviderSelection；重启从 secret store 重新解析相同 credential version，不从日志恢复 secret。私有 settingsOverlay 的明文也不返回 UI；可审查的界面展示 key 名、非敏感 provider/model 字段和凭据引用。`args` 是原生 argv 数组，不是 shell 字符串；materializer 用 allowlist 解析器拒绝与保留 flag 冲突、重复 flag、未知危险启动模式和不兼容 driver 的 flag，不能只搜索子字符串。allowlist 按 driver 分表：claude 的 EXTRA 不等于 codex/grok/agy 的 EXTRA，一个 flag 进表的依据是对该 binary 的实际验证，不是「看起来无害」。

**binaryPath / binarySha256（自定义可执行文件）。** `binaryPath` 是 host 上的绝对路径，覆盖该 driver 默认解析到的命令；缺省时 Node 按「host 默认 → `REMUDA_CLAUDE_BIN` → `PATH`」解析，与以往一致。Hub 只存字符串——它 stat 不到 Node 的文件系统——**Node 是唯一权威**：拒绝相对路径与含 `.`/`..` 的路径（落盘前就拒，不先 canonicalize），拒绝空白与 shell 元字符（该值还会被写进 launch shim 脚本），`canonicalize` 后必须是常规文件且可执行，必须不落在 instance 目录、`<instance>/launch/`、workspace/worktree cwd 或 `TMPDIR` 之内（否则一个能写自己 cwd 的 agent 就自我提权成任意执行），且不能 group/other 可写、不能不属于 Node uid。校验通过后走既有 `pin_binary` 记录 version 与 sha256；若请求带了 `binarySha256` 而 pin 不相等，返回 `INVALID_LAUNCH_SPEC`。任何一条不过都**失败关闭**，绝不静默回落到 `PATH` 上的 `claude`。bot/agent origin 既不能带 `args` 也不能带 `binaryPath`，与 bypass 的拒绝同形（D-011）。override 记在 `LaunchAudit` 上，连同 pin digest 一起进日志。

物化顺序：验证 spec/tag/capability → Node 验证 host-local cwd、worktree 和 writer lease → 选择已健康且 ingress 匹配的 provider/profile → 固定 binary digest → 读取注册的持久 native home → 合并私有 overlay → 校验权限/模型/环境 → 原子写 launch 文件 → 持久 manifest 与命令 intent → spawn。失败时只清理本 launch 创建的临时文件，不改用户现有配置；生成文件夹 `0700`、文件 `0600`，Windows 使用等效 ACL。原生 home、sessions、登录存储不是临时文件，close 和 resume 后都保留。

**子进程环境（`security-review-2.md` S1/S2）。** agent 子进程不继承 Node 的环境：spawn 前先 `env_clear()`，再只注入两部分——一个封闭 allowlist 从本进程继承的名字（`PATH`、`HOME`、`LANG`/`LC_*`、`TERM`、`TMPDIR`、`SHELL`、`USER`、`XDG_*`），以及 materializer 解析出的 `env_allowlist`（driver 提供的 provider 变量，是算出来的值而非继承来的值）。名字不在 allowlist 上就不进入子进程。denylist 与之正交，无论来源（spec、profile、`extra_env`、Node 环境）一律拒绝：`LD_*`、`DYLD_*`、`*_PROXY`（大小写不敏感）、`REMUDA_*`，以及 `NODE_OPTIONS`、`BASH_ENV`、`GIT_SSH_COMMAND`、`SSL_CERT_FILE`/`NODE_EXTRA_CA_CERTS` 等加载器与 TLS 覆盖项。`host-env` binding 的源名也要过同一 denylist，否则 `FOO: host-env(REMUDA_BOOTSTRAP_TOKEN)` 可以绕开 key 检查。规则集中在 `remuda-driver::child_env`，materializer 与各 spawn 点共用。

### 4.2 Claude 硬约束与配置继承

Claude 三个 driver 必须使用经审核且明确列出的 `--setting-sources`；自动化 print 默认排除未经审核的 user decision hook，再显式物化所需 MCP、skills 和 hooks。不得由继承环境决定 responder；有效配置存在第二个 decision hook 时 remote approval 保持 unknown。三者必须保留 native persistence，永不传 `--bare`、`--safe-mode`、`--no-session-persistence`。同义环境 `CLAUDE_CODE_SIMPLE=1`、`CLAUDE_CODE_SAFE_MODE=1` 同样拒绝；环境或上层 flags 若将原生功能关闭，返回 `NATIVE_FEATURE_DISABLED`，不悄悄继续。默认不加 `--restricted`、`--strict-mcp-config`、`--disallowedTools`、`--tools` 缩减清单、空 skills/agents 清单或替换系统提示；不从 DSH wrapper 继承 `persistSession:false` 或自动禁用提问的做法。Claude help、DSH Claude provider

“不传 bare”不足以抵抗未来原生默认改变。每个 binary/profile 的启动验收必须确认 settings sources、Workflow、MCP/skills/hooks 发现和交互 carrier 的实际情况；若未来 print 默认精简且没有已验证的恢复选项，该 build 的 claude-print 不可作为 full-native profile 启动，返回 `NATIVE_FEATURE_DISABLED` 并保留 claude-pty 路径。Artifact 的 native entitlement 另外判断：当前 print 在订阅登录下仍未暴露 Artifact，要求 `artifact` 的 spec 不能被 print 偷偷接纳。control-plane §5

Claude native home 默认选择已登记、已配置的持久 store。可以登记现有 home 以保留已有 plugins/hooks/MCP/skills，也可以显式准备独立 `CLAUDE_CONFIG_DIR`；独立目录必须提供其自己的配置与合法授权，不能以“目录创建成功”声称保留用户原有能力，更不能自动复制 OAuth 文件。两个独立实例只是 endpoint/key 不同时，优先共用已登记的 native 配置基座并使用各自 launch overlay；若 gateway discovery cache/用户设置写入可能相互影响，选择分别准备的持久 store。共享 home 不授予并发写同一个 session 的权限。`CLAUDE_CONFIG_DIR` 在 spawn env 中设绝对路径，不写在 settings.env 中企图延迟搬家。control-plane §3

runtime hook 与 MCP 集成以私有 overlay/已注册扩展追加，保留原有 hook 命令及其顺序语义，不替换整张 hooks 表。合并器必须按目标版本 native 配置规则测试；不把深合并的一般经验当成所有数组的合并语义。运行时探针分“只观察”与“唯一审批 responder”，后者必须满足 §6 的冲突检测；已有 Flux/Orca/herdr 的存在不证明 runtime 也能正确回答。hooks 配置继承调研

### 4.3 各 driver 的实际启动配方

下列是 materializer 输出模板，**本任务没有执行**。`<…>` 是已验证的实体/私有文件引用；实现直接 spawn argv，不经 `sh -c`。cwd 是 spawn 属性，不向 Claude 传不存在的 `--cwd`。

**claude-print：**

~~~text
binary: <absolute claude binary>
argv: [-p, --input-format, stream-json, --output-format, stream-json,
       --verbose, --include-partial-messages, --include-hook-events,
       --forward-subagent-text, --replay-user-messages, --permission-mode, <native mode>,
       --permission-prompts, host, --permission-prompt-tool, stdio,
       --setting-sources, "user,project,local",
       --settings, <launch/settings.json>, --model, <requested native model>,
       --session-id, <new UUID>]
env: CLAUDE_CONFIG_DIR=<registered persistent directory>, plus selected bindings
~~~

2.1.268 的 host driver **必须**带 `--permission-prompt-tool stdio` 并先完成 control initialize；仅有 `--permission-prompts host` 的对照探针没有 can_use_tool，而是直接 permission_denied。SDK 0.3.268 也在配置 canUseTool 后加入此 flag。启动模式 manual 映射 SDK/CLI 的 `default`，不把 native 别名当另一种权限策略。正常运行保持 stdin 可写，所有 control_response 与 user 输入由同一个 writer 串行发送；`--replay-user-messages` 请求 echo，仍只在实际 ID 匹配时推进 accepted。Claude 交互探针 §1

resume 配方用 `--resume <exact session UUID>` 替换新建 `--session-id`，同时重新给相同 overlay 与明确 `--model`，避免 saved custom model 被原生回退。gateway-auth §5.1 不继承探针为排除 Flux 而使用的 `--setting-sources ""`、`--permission-prompts none` 或 disableAllHooks；生产保留原生能力，实际 responder 冲突按 §6.3 处理。

无 secret 的 settings 示例：

~~~json
{
  "model": "passthrough/example-model",
  "env": {
    "ANTHROPIC_BASE_URL": "https://gateway.example",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
  }
}
~~~

这里只示范 provider overlay，已有 hooks/MCP/skills 从 native 配置加载，runtime 自己的已审查扩展另行合入。选静态 credential 时只注入 `ANTHROPIC_AUTH_TOKEN` 或 `ANTHROPIC_API_KEY` 之一；选择 `apiKeyHelper` 时不让静态凭据抢占它。helper 是明确受信的绝对命令，只向 CLI 输出 secret，不向 Hub 回传。模型 requested、CLI 回显 model、gateway wire/resolved model 分开记录，无法观测 wire 时保持 unknown。

**claude-pty：**同一 Claude 配方移除 `-p`、输入/输出 stream flags 和 host permission flags；保留 settings sources、settings、model、session ID 与 native permission mode，由 carrier 分配终端。runtime 默认使用 `manual` + native TUI，已选 auto 的 profile 可原样保留；不自动 bypass。采用 Herdr 时先取得专用 pane，通过已验证的 `agent.start`/受控 launcher 执行 argv；Herdr args 若只能接受 shell 命令文本，使用只含固定 executable 与 launchId 的本地 launcher，让它读取私有 manifest 后 exec，禁止拼 prompt/env/token 到 shell。

Herdr 负责 pane 生存，Node 负责 Instance/native session 对应；runtime 使用 hook 的 `SessionStart.transcript_path` 定位文件，不自己将 cwd 简单替换 `/` 来猜完整目录编码。`--bg` 是未来单独的 carrier variant，不与 print 混用；本规格不把它作为六个 driver 的隐式实现。若以后接入，必须处理它忽略 `--session-id`、attach 会 wake、多个 settings 共享 daemon、logs 是 PTY 转储的行为，不杀共享 supervisor。control-plane §1–3

**agent-in-native-pty（`shell-pty` + agent kind，D-028 §5.1）：**Remuda 自持 PTY 里跑 agent CLI，与 D-025 promote 的终端**同一条路径**。argv 来自 per-kind recipe，不是把 `spec.args` 原样交给 `CommandBuilder`；`flags.rs` 的 BANNED / RESERVED / EXTRA allowlist 在这条路径上同样生效（此前 shell-pty 完全绕过它）。

| kind | argv 模板 | 配置注入 | yolo argv（仅 Human/Bot 且显式 bypass） |
| --- | --- | --- | --- |
| `claude` | `--setting-sources user,project,local` [`--settings <overlay>`] [`--effort <v>`] | argv 上的 settings overlay | `--dangerously-skip-permissions` |
| `codex` | [`-c model_reasoning_effort="<v>"`] | env `CODEX_HOME` 指向影子目录 | `--dangerously-bypass-approvals-and-sandbox` |
| `grok` | [`--effort <v>`] | env `GROK_HOME` 指向影子目录 | `--always-approve` |
| `agy` | [`--effort <v>`] | 无 | `--yolo` |

新建 session **不**钉 `--session-id`：id 由 harness 回报（SessionStart hook / `session_index.jsonl` / `active_sessions.json`），自己造一个只会多出一个要对账的身份。resume 走 `--resume <sid>`，与 §5.6 同路径。`LaunchRecipe` 必须真填 `settings_digest`、`env_allowlist`、provider kind——旧的 shell-pty stub 发的是空白名单加硬编码 provider，D-028 §5.1 步骤 4 把它们列为审计要求。**D-011 / D-017 的授权规则随表一起搬家、一字不改**：Agent origin 一律使用非 yolo preset，bot dispatcher 不自动 bypass。

**codex-appserver：**

~~~text
binary: <absolute pinned codex binary>
argv: [app-server, --listen, stdio://]
env: CODEX_HOME=<registered persistent runtime-owned directory>, RUNTIME_PROVIDER_TOKEN=<resolved secret>
cwd: <verified workspace/worktree directory>
~~~

`$CODEX_HOME/config.toml` 由 materializer 从注册基座与 profile 写出；若同 home 供并发实例使用，配置保持不变，以版本化命名配置或独立 store 处理不同 provider，禁止两个 launch 并发覆盖该文件。默认一 Instance 一 app-server 进程。schema 中 `thread/start.sandbox` 请求用 `workspace-write` 等 kebab-case，响应对象是另一种表示；`permissions` 与 `sandbox` 互斥。initialize 之后，thread/start/resume 显式传 cwd/model/provider/权限，`ephemeral:false`；不继承研究探针的 `--disable hooks`、`approvalPolicy:never` 或删插件做法。`thread/start` 可能把项目 trust 写入 config，因此不得默认使用未登记的用户共享 `~/.codex`。Codex 专项实测 §3、§8–9

~~~toml
model_provider = "runtime_gateway"
model = "gpt-example"

[model_providers.runtime_gateway]
name = "Runtime gateway"
base_url = "https://gateway.example/v1"
wire_api = "responses"
env_key = "RUNTIME_PROVIDER_TOKEN"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
~~~

这里关闭的是 CLI 到模型网关的 Responses WS/重试，和 Hub WSS、app-server 本地传输是三件事；profile 可在相应验收后修改，runtime 自己始终不重投不明 prompt。订阅登录使用注册 home 的原生登录，不写上述 API-key provider，也不从其它账号 home 复制 auth。原生 stdio 是省略 `jsonrpc` 字段的 JSONL；若以后选择 app-server `unix://`，它是 **WebSocket over UDS**，需 HTTP Upgrade，不能把 JSONL 写给 socket。Codex 专项实测 §1、§9

**grok-acp：**

~~~text
binary: <absolute grok binary>
argv: [agent, --no-leader, --model, <catalog model>, stdio]
env: GROK_HOME=<registered persistent store>, GROK_DISABLE_AUTOUPDATER=1, RUNTIME_PROVIDER_TOKEN=<resolved secret>
cwd: <verified directory>
~~~

新建使用 `initialize → session/new`，恢复使用协商后的 `session/load`；不使用隐式共享 leader，不给交互默认加 `--always-approve`。native-prompt 模式不加自动同意；auto 模式只在 `_meta.autoMode` 被该 build 支持时传；always-approve 只有 spec 明确请求才在 `agent` 与 `stdio` 之间加 flag。Responses profile 的持久 Grok config 可为：

~~~toml
[models]
default = "grok-4.6"

[model.grok-4.6]
base_url = "https://gateway.example/v1"
api_backend = "responses"
env_key = "RUNTIME_PROVIDER_TOKEN"
~~~

随包 `05-configuration.md` / `26-config-reference.md` 明确区分 `GROK_CONFIG_PATH` 的软配置 overlay 与真正 endpoint/auth 配置：overlay 的 allowlist 会丢弃 endpoint/auth 等表，不能靠临时 overlay 偷改 provider。provider 进入注册的持久 `GROK_HOME/config.toml`；软设置才可写 `<launch>/grok-overlay.toml` 并设 `GROK_CONFIG_PATH`。runtime 必须先自己校验文件，原生“警告后忽略 malformed overlay”不满足本协议的 fail-loud 要求。真实 endpoint E2E 能力按 profile 验收，不将字段存在当成已接通。gateway-auth §5.3

ACP initialize 默认 `{protocolVersion:1,clientCapabilities:{},clientInfo:{name:"runtime",version:<adapterVersion>}}`，**不声明 fs/terminal**；已验证声明它们会把写盘和 shell 反向交给 client，缺省时 Grok 在执行主机内执行。`session/new` 发送 `{cwd:<absolute>,mcpServers:<registered list>}`；只有明确 always-approve 才加 `_meta:{yoloMode:true}`，列表为空不等于禁用原生用户配置。不要传 `grok agent --no-auto-update`，该位置实测不接受，使用上面的 env。共享 leader 会收到其它 client 的会话更新，不能满足默认单 writer 归因，因此 v1 使用 `--no-leader`。Grok 实测 §2–3、§9、§12

未来复用 Grok serve 可连接 loopback `/ws`，握手用 `Authorization: Bearer <secret>`；已验证 query `server-key` 也可用，但会把 secret 放入 URL，runtime 不采用。不得把 `X-Server-Key` 当可用鉴权头。此模式和 stdio 使用同一 ACP，而与 Hub WSS 是独立连接；启用前仍须单 owner、断线和恢复验收。Grok serve 实测 §10

**agy-print：**

~~~text
binary: <absolute agy binary>
argv: ["-p=<prompt>", --output-format, stream-json, --model, <catalog model>]
cwd: <verified directory>
native settings: <registered OS profile home>/.gemini/antigravity-cli/settings.json
~~~

实测 `-p` 会吞下紧随的 flag，单次 print 用 `-p=<prompt>` 作为一个 argv 元素；manifest 的 redactedArgv 将它替换成 `<inputRef>`。后续 resume 增加 `--conversation <exact conversation ID>`，不能 `--continue`。双向版可用 `--input-format stream-json` 配对 output flag，但 input schema、实时审批和多 turn 关联未验收时不得启用 live send。agy help、control-plane §6

agy 的单次 argv 模式是接口的显式例外：`inputDelivery=deferred-argv`，start/resume 只准备私有 launch template，返回 dispatch=not-dispatched，Instance 留在 preparing；create 可按 scope=runtime-resource 结算并返回 `prepared:true`，不能宣布 native ready。第一条 send 在自己的 dispatch intent 落盘后，将 inputRef 解码为单个 `-p=…` 参数并 spawn，进入 starting→ready；消息只能含该 profile 已支持的文本，超出 argv 长度限制拒绝而不截断。process exit 后再 send 必须先显式 resume；close 未 spawn 的准备态只释放资源，以 no-process 证据结束 Instance。CLI 的 input 只能在这一刻解析，持久 manifest 从不保存明文 argv prompt。其它 driver 的 start/resume 不带 prompt。

agy 没有在现有 help 中建立 `--settings`/专属 config-dir 参数；不伪造 `AGY_HOME`。v1 只在已注册 OS 用户配置中运行，overlay 必须与该 home 的已登记 settings 一致；需要另一份设置时用独立执行用户/容器文件系统，是否保留登录态与原生能力须另验收。只确认 BYOK 的 `modelProvider:"gemini"` + `GEMINI_API_KEY` + 可选 `GOOGLE_GEMINI_BASE_URL`，后者需要 Gemini-native ingress，不能把它指向只有 Messages/Responses 的 AsterGate。未完成独立 profile 准备时返回 `SETTINGS_ISOLATION_UNAVAILABLE`，不改用户现用 settings。`--dangerously-skip-permissions` 只映射明确的 always-proceed；`--mode plan|accept-edits` 保留原生含义。gateway-auth §5.4

**generic-pty：**只运行登记的 executable 与 argv，分配 PTY、继承经过策略选择的 env；不存在通用 provider 配置编码器，profile 必须显式声明 `native-login` 或已注册的 env 模板。它不接受 Claude/Codex settingsOverlay，不提供 semantic resume；屏幕派生的人工 approval/question 回答按 §6 的 PTY 规则处理。未知 binary 不通过尝试不同旗标猜协议。

### 4.4 环境审计、provider 轮换与配置一致性

每个 launch 记录**所有实际注入、继承和主动移除的 env 名字及来源类别，不记值**；下表是必须显式审查的名字，不是允许把当前 shell 全量转发的清单。

| 类别 | env 名称 |
| --- | --- |
| Claude | `CLAUDE_CONFIG_DIR`、`ANTHROPIC_BASE_URL`、`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_API_KEY`、`ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_OPUS_MODEL`、`ANTHROPIC_DEFAULT_SONNET_MODEL`、`ANTHROPIC_DEFAULT_HAIKU_MODEL`、`CLAUDE_CODE_SUBAGENT_MODEL`、`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`、`CLAUDE_CODE_MAX_CONTEXT_TOKENS`、`CLAUDE_CODE_DISABLE_1M_CONTEXT`、`CLAUDE_CODE_API_KEY_HELPER_TTL_MS`、`CLAUDE_CODE_SIMPLE`、`CLAUDE_CODE_SAFE_MODE` |
| Codex / Grok / agy | `CODEX_HOME`、profile 实际声明的 `env_key`/`env_http_headers` 名、`GROK_HOME`、`GROK_CONFIG`、`GROK_CONFIG_PATH`、`GROK_DISABLE_AUTOUPDATER`、`XAI_API_KEY`、`GROK_MODELS_BASE_URL`、`GROK_MODELS_LIST_URL`、`GEMINI_API_KEY`、`GOOGLE_GEMINI_BASE_URL` |
| runtime 身份与 IPC | `RUNTIME_INSTANCE_ID`、`RUNTIME_RUN_ID`、`RUNTIME_PROCESS_GENERATION`、`RUNTIME_LAUNCH_ID`、`RUNTIME_CONTROL_SOCKET`、`RUNTIME_CAPABILITY_FILE`；均为新设计名字 |
| carrier 与一般环境 | `HERDR_ENV`、`HERDR_SOCKET_PATH`、`HERDR_PANE_ID`、`PATH`、`HOME`、`USERPROFILE`、`XDG_CONFIG_HOME`、`TMPDIR`、`TERM`、`COLORTERM`、代理变量以及 profile 允许的其它名字 |

`HOME`/`USERPROFILE` 不作为临时脚本变量或普通 env override 改写；需要不同 OS profile 由 executor 显式切换用户/容器。父 Claude 的临时身份、控制 socket、审批 token 不原样传给 child；每个 child 单独发放 runtime capability，保留所选 native 配置中的工具能力。决定剔除的 native nesting env 必须写入对应版本规则，不能通过大范围删除 `CLAUDE_*`/`CODEX_*` 实现隔离。

ProviderProfile v1 至少有 `id, revision, ingress, endpointCandidates[], models[], credentialRefs[], rotationOwner: gateway|runtime, selectionPolicy: pinned|weighted-healthy, nativeHomeRef, supportedDrivers[]`。endpointCandidate 包含 `id, baseUrl, priority, weight, health: healthy|cooldown|disabled|unknown, cooldownUntil`；同优先级健康候选按权重选择，unknown 默认不自动接流量。运行中固定 ProviderSelection；记录 requested/resolved model，未知 resolved 不回填 requested。AsterGate 作为账号池时 runtime 只轮换 endpoint/profile，不再自行轮换其背后的 upstream accounts。gateway-auth

Hub 操作面（M1）先落地一个薄的可配置子集：`id, name, kind: gateway|direct, baseUrl, models[], defaultModel, headers, defaultGateway, secret: {present, last4, fingerprint}`。auth token 只在 create/rotate 提交，经 SecretBroker 信封加密落在 Hub data dir，GET 永不返回。Claude `delegation=gateway` 的 launch overlay 见 [providers.md](./providers.md)。

换 endpoint/key 分三类：尚未 native 派发的新命令可重选；原生支持且已验收的 credential helper 可在其原生机制内刷新；其它运行中的请求先进入 reconciliation。恢复必须获得原生 session 单 owner lease、确认旧执行已终止、绑定新 generation、显式 native resume，不重投已可能执行的 prompt。失败尝试的 observation 保留，不能通过“换 provider 再跑一次”把已产生的文件修改或 tool effects 隐去。profile 的 ingress 不兼容返回 `PROVIDER_PROTOCOL_MISMATCH`，不临时搭一个有损协议转换器。

#### 4.4.1 模型 API 交付方式与路由（D-047，2026-09-19）

设计全文见 [api-routing.md](./api-routing.md)。

网关凭据是主机绑定的，所以今天「只在一台机器的 relay 上可达的模型」无法交给
另一台机器上的 worker：把凭据发过去既违背 D-021，也未必有用——那个 origin
从 worker 主机可能根本不可路由。D-047 加了**一个可选参数**来关闭这个缺口。

~~~typescript
type ProviderDeliveryMode = "direct" | "via";
type ApiRouteMode = "auto" | "hub-relay" | "direct-net";
type ApiRouteKind = "direct-net" | "hub-relay";   // 已决议的路由；没有 auto

type ProviderDelivery = {
  mode: ProviderDeliveryMode;
  viaHostId?: HostId | null;   // via 时必填（缺则反序列化报错）；direct 时无意义
  route: ApiRouteMode;         // 缺省 auto
};

type RequestedApiRoute = {     // 请求意图，随实例 spec 下发
  mode: ProviderDeliveryMode;
  viaHostId?: HostId | null;
  route: ApiRouteMode;
};

type ApiRoute = {              // 观测：Node 回执里实际跑的路由
  mode: ProviderDeliveryMode;
  route?: ApiRouteKind | null;
  viaHostId?: HostId | null;
  viaHostLabel?: string | null;
};

type HostRelayBind = { addr: string; allowFrom: string[] };  // Host.relayBind，可选
type ApiViaOverride = string;  // "<hostId>" | "self" | "none"
type ApiViaRefusal = "api-via-unknown-host" | "api-via-host-offline"
                   | "api-via-unsupported" | "api-via-unreachable";
~~~

* `ProviderProfile.delivery` 缺省是 `{mode: direct, route: auto}`，也就是今天
  的行为；`InstanceSpec.apiRoute` 缺省是 absent（= 不代理），`Host.relayBind`
  缺省 absent（listener 只绑 loopback）。三者都是**加性**字段，缺省的旧
  payload 照常解析。
* 瀑布：请求 `apiVia` > 项目 > profile `delivery` > `direct`，在**放置之后**
  解析；`via:<H>` 在 `H == W` 时收敛为 `direct`，且收敛只发生在决议时。
* `route: auto` 是**请求**值，不是观测值——`ApiRouteKind` 里没有 `auto`，
  记录里出现 `auto` 就等于在报告「要了什么」而不是「跑了什么」（D-035）。
* 每个拒绝码的 HTTP 状态由协议层固定：`api-via-unknown-host` 是 400，其余
  三个冲突是 409。**没有**任何「回落到 direct」的码，因为不存在这样的路径。
* CLI 的人话拼写是 `provider set --delivery direct|via:<host>` 与
  `dispatch --api-via <hostId|self|none> [--api-route auto|hub-relay|direct-net]`；
  线格式是上面的嵌套对象。逐次覆盖 `apiVia` 之所以保持**字符串**，是因为它
  三个值里有两个是关键字、只有一个带 id。
* 帧的分层、信用、限额、合并与超时见 §7.6。

## 5. Observation envelope、payload 与原生映射

### 5.1 Envelope 与存储顺序

~~~typescript
type ObservationKind = "message" | "thought" | "tool_call" | "tool_result"
  | "interaction.requested" | "interaction.answered" | "interaction.expired"
  | "workflow.run" | "workflow.phase" | "workflow.member"
  | "lifecycle" | "usage" | "artifact" | "raw_tty" | "opaque";
type Completeness = "structured" | "partial" | "screen-derived" | "opaque";
type SourceCursor =
  | {type: "stream"; connectionEpoch: Id; frame: U64}
  | {type: "file"; fileIdentity: Id; fileGeneration: U64; offset: U64; length: U64; digest: Digest}
  | {type: "hook"; invocationId: Id; frame: U64}
  | {type: "tty"; streamId: Id; offset: U64; length: U64}
  | {type: "runtime"; ledgerRevision: U64};
type ObservationSource = {
  driverKind: DriverKind; driverVersion: string; adapterVersion: string;
  channel: "stdout"|"stderr"|"transcript"|"workflow-journal"|"hook"|"rpc"|"pty"|"herdr"|"runtime"
    |"file"|"osc"|"screen";
  delivery: "live"|"replay"|"unknown";
  nativeSessionId: Knowledge<string>; nativeTurnId: Knowledge<string>;
  nativeAgentId: Knowledge<string>;
  nativeItemId: Knowledge<string>; nativeEventId: Knowledge<string>;
  nativeRequestId: NativeRequestKey;
  sourceCursor: SourceCursor;
};
type RawRef = {
  objectId: Id; offset: U64; length: U64; digest: Digest;
  mediaType: string; redaction: "none"|"derived-redacted"|"unavailable";
};
type Observation = {
  schemaVersion: 1; eventId: Id; journalId: Id;
  instanceId: Id; runId: Id|null; hostId: Id;
  processGeneration: U64; runGeneration: U64|null;
  seq: U64; observedAt: Timestamp; nativeAt: Knowledge<Timestamp>;
  source: ObservationSource; kind: ObservationKind;
  completeness: Completeness;
  rawRef: RawRef|null;
  evidenceEventIds: Id[];
  payload: ObservationPayload;
};
~~~

这是 §8.3 草案的正式化：旧 `sessionId` 改名 `instanceId`，native session 只在 source/NativeRef；不同时输出两个可能分歧的别名。seq 是 **一个 instance journal 内持久、连续、单调的 U64**，从 1 开始，process restart 不重置。Node 是唯一分配者；Hub 原样镜像，不重新编号。Raw bytes 先落受保护的 object，再把其引用、source cursor、normalized event、实体变更一起持久提交，提交后才 broadcast/ACK；失败不前移 durableSeq。runtime 自产的生命周期事实 rawRef 可以为 null，必须带 ledger/source evidence；native 原始记录不能无理由丢 rawRef。

**信号分层（D-028 §4.3）。** `channel` 新增 `file` / `osc` / `screen`，与既有的 `hook` 一起构成优先级阶梯 `Hook > File > OSC > Screen`。每条 Observation 因此能解释「凭什么说它 blocked」：`hook` 是 agent 自己报的，`file` 是 tail 它的 transcript / rollout / `updates.jsonl` 读到的，`osc` 是终端控制序列，`screen` 是屏幕签名反推。既有的 `transcript` 保留给已建立的 Claude transcript 路径，`pty` 保留给原始字节；新变体不取代它们。分层与 `Completeness` 正交：前者说**证据来自哪里**，后者说**这条 payload 有多完整**。`unknown` 永不塌缩成 `idle`——没有规则命中是诚实的「不知道」，不是「已完成」的证据。

`structured` 仅表示该 payload 经过明确 schema 映射，**不表示整个 Run 已完整或可信执行**；`partial` 是缺字段、流中增量或可识别但不完整的 native 内容；`screen-derived` 仅来自屏幕/启发式；`opaque` 完全未解释。被截断的 stderr 不拼成可验证 tool_result。raw storage 可含用户输入与工具输出，默认只授予同账号、相应 host/workspace 的 read-raw 权限，禁止把 env/auth frame 写入普通日志；对外 redacted 派生对象与原始 object 使用不同 digest，不覆盖原证据。

file tail 只提交完整 JSONL 行；末尾未完成行等待后续 append。保存 file identity、generation、offset 和字节 digest；truncate/替换生成新 fileGeneration，检测到原前缀被改写进入 reconciliation。Claude transcript、stdout、hooks 同时出现同一工具时，依据 session/tool_use_id/message ID 等稳定键归并成一个 node；不能用文本相等去重，也不能猜同名工具就是同一次调用。没有稳定关联就保留两个来源的 partial observation，并把关联设 unknown。

### 5.2 消息、思考与工具

以下 types 和字段表合在一起定义 `ObservationPayload`；kind 与 payload 类型一一对应，解析器必须使用判别联合。ContentBlock 的二进制内容走 objectRef，不放 base64 图片进每条消息。

~~~typescript
type ContentBlock =
  | {type: "text"; text: string}
  | {type: "image"|"audio"|"file"; objectId: Id; mediaType: string; name: string|null}
  | {type: "resource"; uri: string; mediaType: Knowledge<string>; objectId: Id|null}
  | {type: "opaque"; rawRef: RawRef; nativeType: string};
type NodeMutation = {
  nodeId: Id; revision: U64;
  operation: "open"|"append"|"replace"|"close";
  baseRevision: U64|null;
};
type MessagePayload = NodeMutation & {
  messageId: Id; role: "user"|"assistant"|"system";
  phase: "input"|"commentary"|"final"|"unknown";
  blocks: ContentBlock[];
  targetBlock: number|null;
  parentToolCallId: Id|null;
  nativeOrigin: Knowledge<string>;
  origin: "human"|"injected-skill"|"injected-command-output"
        |"hook-context"|"tool-result"|"compaction"|"unknown";
  commandId?: CommandId;
  // c-steer: how a Remuda-queued prompt reaches the agent. Set on the queued
  // user node and kept through its revisions; absent on natively-observed
  // records. "steer" interrupted the running turn and jumped the PTY queue.
  promptMode?: "new-turn"|"steer"|"queue";
  status: "queued"|"streaming"|"complete"|"interrupted"|"unknown";
};
type ThoughtPayload = NodeMutation & {
  thoughtId: Id; representation: "summary"|"text"|"redacted";
  text: string|null; partIndex: number;
  status: "queued"|"streaming"|"complete"|"interrupted"|"unknown";
};
type ToolCallPayload = NodeMutation & {
  toolCallId: Id; parentToolCallId: Id|null;
  toolName: Knowledge<string>; displayTitle: Knowledge<string>;
  category: "shell"|"file-read"|"file-write"|"search"|"mcp"|"workflow"|"agent"|"other";
  input: Knowledge<Json>; inputTextDelta: string|null;
  state: "proposed"|"running"|"unknown";
  executor: Knowledge<{hostId: Id; workspaceId: Id|null; nativeAgentId: string|null}>;
};
type ToolResultPayload = NodeMutation & {
  toolCallId: Id; stage: "partial"|"final";
  outcome: "succeeded"|"failed"|"denied"|"cancelled"|"unknown";
  blocks: ContentBlock[]; structuredResult: Knowledge<Json>;
  exitCode: Knowledge<number>;
  changes: {path: string; diff: string; application: "proposed"|"applied"|"unknown"}[];
};
~~~

open 的 revision 为 1、baseRevision 为 null；append/replace/close 的 revision 必须是当前 node revision+1，baseRevision 必须匹配。append 的 blocks 只包含新增内容，targetBlock 指向已有文本块；完整原生 message/item 到达时用 replace，再 close，不能把完整文本再 append 一遍。replace 给出完整当前值，空数组表示确实为空。Claude 对同一 message.id 分块发送的 assistant records 由专用 adapter 合并 content block ID/index，不覆盖此前已经观察到的另一 tool_use。某些源没有可确定 delta 语义时只做完整快照 replace 或 opaque。

PTY 输入因原生交互或 control 尚未就绪而等待时，Node 以 user message 的 `status: "queued"` 记录待发文本；确认写入后用相同 node/message ID 的 replace 更新为 `complete`。`PromptMode: "queue"`（§3.1）走的是同一套账本，区别只在于排队是调用方**明确要求**的而不是 ready 判定推出来的；两者都不新增 status 值。这里的 complete 仅证明输入已发送，不证明 assistant turn 完成。排队期间 `instance.create` 可结算 accepted，Instance 保持 ready，原生阻塞交互仍可回答；未发送即取消/关闭的消息更新为 interrupted。`queued` 不用于 thought。

同一 node 的 source 优先级由字段定义：结构化 native item/message 负责内容和最终 tool 结果；hook 负责它自身的前后事件和交互请求；screen 只负责展示提示。低信息来源不能把高信息值改成空值。usage 不随 message replace 被累加第二次。thought 只显示原生实际输出的文本/summary，redacted thinking 保存 redaction 标记，不尝试恢复隐藏内容。

**`origin`（D-028 P3 增量字段）**：`role` 说的是这条记录被记在谁名下，`origin` 说的是**它到底是谁写的**。Claude transcript 把 skill 正文、slash 命令展开（`<command-message>` / `<command-name>`）、`<local-command-stdout>`、hook additionalContext、`<task-notification>`、tool result、compact 摘要**一律记成 `user` 记录**——把它们都画成用户气泡，既重复又让人误以为是自己发的。因此分类必须落在 mapper 上，由**证据**判定而不是猜文本：`isCompactSummary` → `compaction`；`sourceToolUseID` 或含 `tool_result` block → `tool-result`；`isMeta` → `injected-skill`；`<command-*>` 开头 → `injected-skill`（命令调用）；`<local-command-*>` 开头 → `injected-command-output`；`<system-reminder>` / `<task-notification>` 开头 → `hook-context`；其余为 `human`。Claude 自报的 `origin.kind`（`human` / `task-notification` / …）与 `promptSource`（`typed` / `sdk` / `system`）**优先于**上述启发式，因为那是 harness 的直接陈述。

**不得丢弃**：注入项仍要进 journal（否则无法解释 agent 为什么那样答），只是 UI 默认折叠成一行。`unknown` 按 `human` 渲染——漏判要表现为多显示一条，不能表现为静默吞掉用户的话。同一条人类 prompt 可能既有入队记录又有投递记录（共享 `promptId`），按 `promptId` + 文本去重**保留第一条**。

### 5.3 Workflow

| kind | 必填 payload 字段 | 规则 |
| --- | --- | --- |
| `workflow.run` | `workflowId:Id, engine:claude-workflow, nativeRunId:Knowledge<string>, nativeTaskId:Knowledge<string>, toolCallId:Id\|null, state:queued\|running\|completed\|failed\|cancelled\|unknown, revision:U64, title:Knowledge<string>, resultRef:Id\|null` | workflowId 是 runtime 映射键；有 taskId 而无 wf ID 时明确 unknown，随后关联不能新增第二份执行 |
| `workflow.phase` | `workflowId:Id, phaseId:Id, nativePhaseId:Knowledge<string>, label:Knowledge<string>, state:queued\|running\|completed\|failed\|cancelled\|unknown, revision:U64, parentPhaseId:Id\|null` | 只有真实 phase 数据才创建；member label、数组位置不能假造 phase |
| `workflow.member` | `workflowId:Id, memberId:Id, nativeAgentId:Knowledge<string>, nativeKey:Knowledge<string>, attempt:Knowledge<U64>, phaseId:Id\|null, label:Knowledge<string>, state:queued\|running\|completed\|failed\|cancelled\|unknown, modelRequested:Knowledge<string>, modelResolved:Knowledge<string>, resultRef:Id\|null, revision:U64` | key、agentId、attempt 共同区分重试；一个 member 完成不等于 workflow 完成 |

这里仅定义 Claude dynamic Workflow 的已观察树。runtime 调起另一 Instance 通过 Instance.parent/Run.parentRunId 展示；Codex collab/Grok plan 进入 native lifecycle/tool 数据。未来若引入其它真实 workflow engine，新增受版本协商的 engine variant，而不是将它们强改成 Claude Workflow。script 本身仍在原生 CLI 执行，runtime 不解释 `agent()/parallel()` 或改变它的调度。DSH workflow 对比、原生 workflow 实测

### 5.4 Interaction 请求与回答 schema

~~~typescript
type DecisionOption = {
  id: string; label: string;
  effect: "allow-once"|"allow-session"|"deny"|"cancel"|"native-specific";
  nativeValueRef: Id; // Node 保存的原生值，浏览器不得自行构造。
};
type QuestionField = {
  id: string; title: string; description: string|null;
  input: "text"|"single-select"|"multi-select";
  required: boolean; options: {id: string; label: string}[];
  allowFreeText: boolean; sensitive: boolean;
};
type InteractionRequest =
  | {kind: "approval"; title: string; description: string;
      toolCallId: Id|null; actionRef: Id; options: DecisionOption[];
      requestedPermissionsRef: Id|null; inputDigest: Digest}
  | {kind: "question"; title: string; fields: QuestionField[]}
  | {kind: "plan-review"; title: string; planRef: Id; planRevision: U64;
      planDigest: Digest; options: DecisionOption[]; allowFeedback: boolean}
  | {kind: "elicitation"; title: string; mode: "form"|"url"|"native-extension";
      schemaRef: Id|null; schemaDialect: string|null; url: string|null;
      nativeExtension: string|null; allowedActions: ("accept"|"decline"|"cancel")[]};
type InteractionAnswer =
  | {kind: "approval"; optionId: string; inputDigest: Digest}
  | {kind: "question"; answers: Record<string, {optionIds: string[]; text: string|null}>}
  | {kind: "plan-review"; optionId: string; planRevision: U64;
      planDigest: Digest; feedback: string|null}
  | {kind: "elicitation"; action: "accept"|"decline"|"cancel"; content: Json|null};
~~~

`approval` 默认只公开该请求实际允许的选项，不能全局添加“总是同意”；scope 与原生权限范围必须一致。v1 不支持通过 approval answer 编辑命令输入或添加自定义权限规则；有需求须新的 requestVersion/明确 encoder，不把 answer.text 注入 shell。question 字段 ID、optionId、必填性、单/多选限制逐项校验。plan-review 必须有 **真正可回复的原生暂停请求**；仅看到 `plan` 文本或 `ExitPlanMode` 工具调用时不能创建可批准的执行授权。

| kind | payload |
| --- | --- |
| `interaction.requested` | `{interaction: InteractionEntity}`，完整 §2.6 实体；无 response carrier 的提示为 answerable:false |
| `interaction.answered` | `{interactionId, requestVersion, answerCommandId, actor, answerRef, delivery:not-sent\|intent-durable\|written\|confirmed\|rejected\|unknown}`；在 Node 唯一答案提交时发一次 |
| `interaction.expired` | `{interactionId, requestVersion, reason:deadline\|native-cancelled\|generation-ended\|replaced\|channel-lost, evidenceEventIds:Id[]}`；reason=channel-lost 只有确认该旧请求不可再答时使用，否则先 unknown |

回答后 delivery 的进展发 lifecycle/interaction 实体快照，不重复发 answered。答案对象保存在受权限控制的 answerRef；敏感表单值不扩散到通用摘要。URL elicitation 只提供原生 URL 让用户完成原生流程，不把点击链接当完成证据。

### 5.5 Lifecycle、Usage、Artifact、TTY 与 Opaque

| kind / variant | payload schema |
| --- | --- |
| `lifecycle`, `type=entity` | `{type:"entity", entityType:host\|workspace\|instance\|run\|command\|interaction, entityId:Id, revision:U64, previousState:string\|null, state:string, reasonCode:string, evidenceEventIds:Id[], entity:对应完整实体}`；state 必须落在对应实体枚举，entity.state/lifecycle 一致 |
| `lifecycle`, `type=native` | `{type:"native", topic:session\|turn\|hook\|subagent\|task\|plan\|configuration\|permission\|diagnostic\|reconciliation, nativeName:string, nativeId:Knowledge<string>, status:Knowledge<string>, relatedIds:Record<string,string>, dataRef:Id\|null, severity:info\|warning\|error, affectsCompletion:boolean}`；这类事件本身不强制终结 Run |
| `usage` | `{usageId:Id, scope:message\|turn\|session\|workflow-member, scopeId:string, mode:snapshot\|delta, metricRevision:U64, inputTokens:Knowledge<U64>, inputAccounting:total-including-cache\|uncached\|provider-specific\|unknown, outputTokens:Knowledge<U64>, reasoningTokens:Knowledge<U64>, cacheReadTokens:Knowledge<U64>, cacheWriteTokens:Knowledge<U64>, totalTokens:Knowledge<U64>, cost:Knowledge<{amount:string,currency:string}>, accounting:reported\|estimated, nativeFieldsRef:Id\|null}` |
| `artifact` | `{artifactId:Id, revision:U64, action:declared\|available\|updated\|removed, type:file\|image\|html\|url\|native\|diff, title:Knowledge<string>, mediaType:Knowledge<string>, sizeBytes:Knowledge<U64>, locator:ArtifactLocator, producerToolCallId:Id\|null, verification:declared\|read-verified\|native-confirmed}` |
| `raw_tty`, `direction=output` | `{streamId:Id, streamEpoch:Id, direction:"output", representation:pty-bytes\|rendered-ansi, offset:U64, byteLength:U64, dataRef:RawRef, nativeFrame:Knowledge<{seq:U64,width:number,height:number,full:boolean}>}`；rendered-ansi 只表示原生 carrier 重绘的终端画面，不是原进程 stdout/PTY 原字节 |
| `raw_tty`, `direction=input` | `{streamId:Id, streamEpoch:Id, direction:"input", inputId:Id, byteLength:U64, dataRef:RawRef\|null, actor:ActorRef, delivery:written\|unknown}`；交互输入可只记长度/身份，原文是否保存由明确 retention policy 决定 |
| `raw_tty`, `direction=resize` | `{streamId:Id, streamEpoch:Id, direction:"resize", cols:number, rows:number, resizeRevision:U64}`；正整数，按服务器公布上限校验 |
| `opaque` | `{nativeType:string, reason:unknown-type\|unknown-version\|malformed\|unsupported-extension\|unmapped-fields\|screen-snapshot, rawRef:RawRef, affects:(presentation\|control\|terminal)[], summary:string\|null}`；不含自行猜出的 outcome |

`ObservationPayload` 是 §5.2 的 Message/Thought/ToolCall/ToolResult、§5.3 三种 workflow、§5.4 三种 interaction 和本表 variants 的联合。`InteractionEntity` 即 §2.6 全字段加 EntityMeta。host/workspace lifecycle 使用相同 payload，但 envelope 放在独立 `RegistryEvent`：`{schemaVersion:1,eventId,hostId,journalId,seq,observedAt,kind:"lifecycle",payload}`，不伪造 instanceId。Hub Command 入口事件另有自己的 journal，Node 镜像不能让同一实体出现两个权威 revision。

`ArtifactLocator` 为 `{type:"blob",objectId:Id,digest:Digest}`、`{type:"workspace-file",workspaceId:Id,relativePath:string,revision:U64,digest:Knowledge<Digest>}`、`{type:"native",nativeUri:string}` 或 `{type:"url",url:string}`。可读 workspace-file 路径由 Node 按注册根解析，拒绝 `..`、越界 symlink 和跨 host 路径；下载校验 revision/digest 防止读错版本。native URI/URL 不自动抓取或执行。HTML 预览使用独立 origin 与 sandbox；TTY 的 clipboard/URL/escape sequences 不在 Hub 或普通 DOM 执行。Artifact 工具的调用只产生 declared，文件/API 的读回证据才产生 available；普通文件产物不能声称复现了 Claude 原生 Artifact 功能。

usage 每个 scope 用 metricRevision 更新；snapshot 覆盖旧 snapshot，不累加。只有原生明示 delta 才累加。Claude 多条 result 可能是累计 cost，Codex resume 会重发 thread 累计量，agy total 未必包含 cache/reasoning；保持原生统计口径，不用字段相加“修正”上游总量。没有 usage 时所有相关值 unknown，不是 0。Claude Workflow 样例、Codex usage/resume、agy sample

Grok ACP `_meta.usage.inputTokens` 已知包含 cache，故 inputAccounting=total-including-cache；其 headless `end.usage.input_tokens` 是 uncached，不能直接跨表相加或相减假定等价。response_completed、turn_completed、prompt result 可能重复报告同一范围；没有稳定 response/turn ID 时保留 nativeFieldsRef 并将聚合关系标 unknown，不用邻接顺序去重。Grok usage 对照

### 5.6 Claude 原生事件逐项映射

这些映射按版本实现；表内有已知字段才填值。未列出的 type/subtype 原样落 raw + opaque；如果触及控制/终态，不继续自动回答或结算。

| Claude stream-json 原生记录 | 统一事件与字段来源 | 关联、完整性与限制 |
| --- | --- | --- |
| `type:system, subtype:init` | lifecycle/native session/init；保存 session_id、tools、model、cwd、permissionMode、mcp_servers 原始快照 | 校验 session UUID，更新能力候选；init 不证明认证或完整任务成功 |
| `type:assistant`, `message.content[].text` | message replace/append，assistant role；message.id / uuid 经关联表定位 node | 父子用 parent_tool_use_id；完整 content 不是 delta，不能重复 append |
| assistant 的 `thinking` / redacted thinking block | thought，representation 按原生 block | 不补全红acted内容；转发子思考必须保留原生父引用 |
| assistant 的 `tool_use` | tool_call，ID=block.id，name/input 原样 schema 化 | 通常先 proposed；看到工具调用意图不证明已经执行 |
| `type:user`, message 中 text | message/user；uuid 与提交的 nativeClientMessageId 可核对时推进 send accepted | 没回显 ID 时不能凭相同 prompt 文本消除投递歧义 |
| user 的 `tool_result` | tool_result，关联 tool_use_id，content/is_error 映射 | 用户角色不意味着人发言；必须按 content block 判别 |
| `type:stream_event` 的 message_start/content_block_start | message/thought/tool_call open，记录 native message ID、block index | completeness=partial |
| stream_event 的 text_delta / thinking_delta / input_json_delta | 对应 node append；input JSON 未完整前存 inputTextDelta | 不能把半个 JSON 当工具的可执行参数 |
| stream_event 的 content_block_stop/message_delta/message_stop | 关闭 block、更新 stop_reason/usage 候选 | message_stop 是模型消息结束，不是 Run 成功；等完整 assistant/result |
| `type:result` | lifecycle/native turn/result；message final 若仅 result 有文本且未与完整 assistant 重复；usage snapshot | `subtype`, `is_error`, `terminal_reason`, `result_index`, `origin` 保留；只按 §5.8 推进 Run |
| `system/hook_started` | lifecycle/native hook/started，hook_id/name/event | 它表示 hook 命令启动，不等于相应工具已经开始 |
| `system/hook_response` / 版本支持的 hook_progress | lifecycle/native hook/response/progress，exit_code/outcome/output 引用 | hook 输出与原生 tool_result 分开；不是 approval answer ACK |
| `system/permission_denied` | lifecycle/native permission/denied，tool_use_id、decision reason | 原生自动拒绝提示；没有待答 request，不生成批准按钮 |
| `system/status` | lifecycle/native session/status，更新 activity 提示 | requesting/idle 等仅作状态观测，不替代 result |
| `system/session_state_changed` | lifecycle/native session/activity，idle/running/requires_action | idle 可说明当前 native 状态，不证明整个任务无后续续跑 |
| `system/api_retry` | lifecycle/native diagnostic，attempt/max_retries/retry_delay_ms | 这是 CLI 自己的重试，runtime 不据此再投 prompt |
| `system/informational` / `system/local_command_output` | native diagnostic 或原生 local-command 文本 | prevent_continuation 保留，不能套用正常 result |
| `system/elicitation_complete` | 对已关联 elicitation 更新 native resolution | 必须有原请求关联；无关联仅保留 lifecycle |
| `system/control_request_progress` / `tool_progress` | 相应控制/工具进展，partial | 心跳/进度不结算 Command 或工具成功 |
| `keep_alive` | 仅 transport 活性；需要审计时 native diagnostic | 不创建消息、Run、交互，也不作为任务进展 ACK |
| `system/background_tasks_changed` | lifecycle/native task/background-snapshot，tasks 原始引用 | 对它自己的列表整表替换；不覆盖 task_started ledger，也不覆盖全部 foreground/observer work，空数组不证明 quiescence |
| `system/task_started`, task_type=local_workflow | workflow.run running；nativeTaskId=task_id；tool_use_id 链接 | 若没有 wf ID，nativeRunId=unknown；非 workflow task 走 native task 生命周期 |
| `system/task_progress`, workflow_progress[] | workflow.member upsert；agentId/key/attempt/model/state 来自每个 entry | start→running，done→completed；无 schema 的 phase 不造节点；preview 不是完整 result |
| `system/task_updated` | workflow.run 状态 patch（确为同一 workflow 时） | 使用 patch.status 的已知值，未知值 opaque；一条 member done 不代表整 run completed |
| `system/task_notification` | workflow.run/或 native task terminal，保留 task_id/status/output_file | 可触发父原生 continuation，不能立即把父 task-scope Run 成功结算 |
| `control_request` / `control_response` / `control_cancel_request` | §6 的 broker 或 command ACK；必要的状态变化进入 lifecycle/interaction | 不显示为 assistant 文本；control ID 按 connectionEpoch 区分 |
| 其它 system / 无法识别 record | opaque + native diagnostic | 例如未验证的新 settings/Artifact 协议；保留原始数据 |

依据：本机 init fixture、control-plane §4、SDK 0.3.268 类型整理与逐帧参考。本次另只读 `/tmp/hh-probe-ctl/logs/wf.jsonl` 的事件类型和终态元数据，确认 task_* 实际是 `type:system` 的 subtype；两条 result 都曾有 `terminal_reason=completed`、`queued_turn_count=0`，第二条 origin 为 task-notification。因此这两个字段的组合也不能单独证明整体 task 已结束。原生 Workflow 的预算 canary 还观察到父 result 后后台 task 被 budget limit 停止；它提供混合模型的调用/成员记录证据，不是“所有成员成功完成”的证明。交互探针 §4

| Claude transcript JSONL type / attachment 子类 | 统一映射 | 规则 |
| --- | --- | --- |
| `assistant` / `user` | 同上 message/thought/tool_call/tool_result | 用 uuid、message.id、tool_use_id 与 stdout 关联；无 run 关联时 runId=null，不能挂到“最新 run” |
| `system` | 已识别子类型映射 lifecycle，其余 opaque | 不把 system 记录都当模型可见消息 |
| `attachment:hook_success` | lifecycle/native hook 的审计记录 | 不能证明审批允许或 tool success |
| `attachment:edited_text_file` | tool/result 或 artifact 的补充候选，默认 partial | 无确切 tool ID/应用证明时不标 applied |
| `attachment:model` / `mode` / `permission-mode` | lifecycle/native configuration | 记录实际配置；不推断之前所有调用都使用它 |
| `attachment:environment,prompt_snapshot,instructions,nested_memory,session_context,date` | opaque/unmapped-fields 或已知上下文记录 | 不重建模型请求；prompt_snapshot 可截断 |
| `attachment:agent_listing_delta,skill_listing` | lifecycle/native configuration 发现快照 | 存在清单不证明工具调用成功 |
| `attachment:remote_session_change` / `bridge-session` / `frame-link` | opaque，已知 session 关联另行版本化 | 不从私有桥记录推导控制权 |
| `last-prompt` / `queue-operation` | native diagnostic/queue 观察，默认 partial | 不是原生命令幂等 receipt，不能凭它自动重发 |
| `ai-title` | lifecycle/native session/title 展示 | title 不是身份 |
| `atis-latch` | opaque/unknown-type | 不猜语义 |
| `file-history-snapshot` / `file-history-delta` | opaque 或版本化 artifact/diff 补充 | 文件 checkpoint 不等于当前文件内容或任务通过 |
| `artifact-autoreact-ledger` / `artifact-comment-monitor` | opaque + artifact native 引用（有明确 ID 时） | 不伪造 Artifact 状态/批准/可访问 URL |
| 未知 type / 新 attachment | opaque，完整 rawRef | 不丢行、不猜终态 |

transcript 类型依据 agent-protocols §2.4。Workflow 的单独 `journal.jsonl`：`launched`→workflow.run/running；`started{key,agentId,label}`→workflow.member/running；`result{key,agentId,result}`→对应 member/completed 和 resultRef；`workflows/<wfId>.json` 的已知 `status/result`→workflow.run snapshot。其它 journal type（包括尚无 fixture 的 phase/error）先 opaque；agent-*.jsonl 可产生明确 member 下的消息。member result 不能替代 workflow 整体状态。control-plane §4

| Claude hook 输入 `hook_event_name` | 统一事件 | 是否可控制 / 限制 |
| --- | --- | --- |
| `SessionStart` | lifecycle/native session/start，native session + transcript_path + source | 只绑定身份；过滤/标注 agent_id，不能让 child 覆盖 root |
| `UserPromptSubmit` | message/user 或 native input-received 候选 | hook 可发生在后续校验拒绝之前；只有可靠关联时作为“收到输入”，不证明开始执行 |
| `PreToolUse` | tool_call proposed；tool_use_id/tool_name/tool_input | 不生成 approval，预调用不代表执行 |
| `PostToolUse` | tool_result final 或结果补充；tool_response | 与 tool_use_id 关联，原生事件本身不足以证明任意外部副作用完成 |
| `PostToolUseFailure` | tool_result failed，保存 error | 不把“hook 自己失败”混成工具失败 |
| `PermissionRequest` | interaction.requested/approval，carrier=claude-hook | 唯一阻塞 hook invocation 可答；没有 native requestId/tool_use_id 时用 synthetic invocation ID，工具关联 unknown |
| `Notification` | lifecycle/native diagnostic/notification；permission_prompt 只提示等待 | 没请求关联与 live responder 就不可回答；不按通知正文自动发 y/Enter |
| `Stop` | lifecycle/native turn/stop-observed | Stop hook 可被其它 hook 阻止后续停机，也可有后台工作；不是整个 task 终态 |
| `StopFailure` | lifecycle/native diagnostic/error | 精确 native error 有时可结算对应回合；无 version rule 就保持 unknown |
| `SubagentStart` | lifecycle/native subagent/started；agent_id | 只有显式 workflow 关联才额外更新 workflow.member |
| `SubagentStop` | lifecycle/native subagent/stopped | 不是 root Stop；没有 outcome 字段不猜 succeeded |
| `PreCompact` / `PostCompact` | lifecycle/native session/compaction | 不改 runtime 已有 observation，不从 compact 后历史复制出新消息 |
| `SessionEnd` | lifecycle/native session/end-observed | 与 supervisor exit、reason 对账；不等于 task succeeded |
| `TeammateIdle` | lifecycle/native subagent/idle | 不代表所有团队工作结束 |

Hook 名称与本机集成见 hooks-integrations。本次没有实测 runtime 的 hook 安装或审批回写；新 hook payload 仍按 §6 的版本化 encoder 验收。源记录中存在的 user instruction、tool output、hook message 都是数据，不得变成 runtime 的管理命令。

### 5.7 Codex、Grok、agy 与 PTY 映射

**codex-appserver：**必须先区分 `{id,method}` server request、`{method}` notification、`{id,result|error}` response。stdio adapter 按原生省略 `jsonrpc` 的 JSONL 编码，runtime 的 Hub 协议仍是完整 JSON-RPC 2.0。当前实测的正常回复、steer、interrupt、resume 见 codex-appserver-driver；工具、审批、elicitation 完整 variants 按 ServerRequest / ServerNotification 实现，并另外做验收。

| Codex native method / item.type | 统一事件 | 关联和终态规则 |
| --- | --- | --- |
| initialize response + initialized | lifecycle/native session/initialized | 每个连接单独握手；不表示认证成功或已有 thread |
| thread/start / thread/resume / thread/fork response | lifecycle/instance ready、nativeRef | 保存 thread.id/path；resumed 不等于重放 prompt；fork 返回新 Instance |
| `thread/started` | lifecycle/native session/started | 与 response 的同 thread 去重/补字段 |
| `thread/status/changed` | lifecycle/native session/status | idle/active/notLoaded/systemError 是原生状态；idle 本身不结算 Run |
| `thread/name/updated` / settings/updated | lifecycle/native session 或 configuration | 保存明确字段；不是模型调用完成 |
| `thread/archived` / unarchived / deleted / closed / reverted | lifecycle/native session 相应变化 | archived 不等于进程 exited；deleted 使 resume 能力失效；reverted 不删 runtime 审计 |
| turn/start response | Command native-input accepted，记录 turn.id | 随后的 turn/started 才证明开始；两者顺序可交换 |
| `turn/started` | lifecycle/run running | threadId+turn.id 对应确切 Run，不取全局“当前 turn” |
| `turn/completed` | lifecycle/run native-turn terminal | completed→succeeded，interrupted→cancelled，failed→failed；只对相同 thread+turn；itemsView=summary 不是完整历史 |
| `turn/diff/updated` | artifact/diff snapshot | 完整替换最新聚合 diff，不从展示 diff 再执行修改 |
| `turn/plan/updated` | lifecycle/native plan/updated | plan[].status 为计划状态，不是审批请求或 Claude workflow phase |
| item started/completed `userMessage` | message/user | clientId 与 clientUserMessageId 关联；不能把重复历史当新输入 |
| item started/completed `agentMessage` | message assistant open/replace/close | phase=final_answer 仍不终结 turn；delivery=async 保留，questions 为非阻塞提示时不假造 RPC 请求 |
| `item/agentMessage/delta` | message append | delta 是 string，按 itemId 与 revision 追加 |
| item `reasoning` + summaryTextDelta/summaryPartAdded/textDelta | thought summary/text | 分别用 summaryIndex/contentIndex，不混为最终回答 |
| item `plan` + `item/plan/delta` | message/system 或 artifact/plan 文本展示 | 只有真正 server request 才出现可执行 plan-review |
| item `commandExecution` | tool_call(shell) + tool_result | inProgress→running；completed/failed/declined 映射结果；exitCode 可未知；显示 command 不当可重新执行 argv |
| `item/commandExecution/outputDelta` / terminalInteraction | tool_result partial / native tool 交互审计 | terminalInteraction 不是可自动代答权限；不重发 stdin |
| item `fileChange` | tool_call(file-write) + tool_result + artifact/diff | proposed 与 completed/applied 分开；failed/declined 不显示已修改 |
| `item/fileChange/patchUpdated` | tool_call 输入/拟议 diff 更新，partial | patch 在实际执行前流出，不能标 applied |
| `item/fileChange/outputDelta` | opaque 或 legacy tool output | 该版本已弃用/不一定发送，不能作为唯一观察源 |
| item `mcpToolCall` + `item/mcpToolCall/progress` | tool_call/tool_result/progress | server/tool/arguments/result/error 保留，readOnlyHint 不等于实际 outcome |
| item `collabToolCall` / `subAgentActivity` | tool_call(agent)、lifecycle/native subagent | 关联 sender/receiver/agentThreadId；子完成可晚于父 turn/completed，不重开父 native-turn Run |
| item `webSearch` / `imageGeneration` / `imageView` | tool_call/result；明确产物再 artifact | 图片 savedPath 要经过 workspace/object 授权；status 未知不造图 |
| item `functionCallOutput` | tool_result，toolCallId 由独立 native item 映射，outcome=unknown | 没有 call_id 不与附近同名调用强配；保留 standalone 标记于 raw |
| item `sleep` / enteredReviewMode / exitedReviewMode / contextCompaction | lifecycle/native 相应活动 | 不把等待、review 或 compaction 当整个用户任务结束 |
| `thread/tokenUsage/updated` | usage/session snapshot | resume 重发仍是同一累计值；不与逐 turn usage 相加 |
| `error` notification | lifecycle/native diagnostic；附 willRetry 与原生错误 | willRetry=true 时仍等待对应 turn 终态；错误文本不触发 runtime 重投 |
| RPC error response | 对应 Command 的 rejection 或 reconciliation | 只有可证明 native 操作未执行才 rejected；generic -32603 不能一概推断没效果 |
| `serverRequest/resolved` | Interaction resolved/invalidated 的证据 | 包含 threadId/requestId，但可能只是生命周期清理；不单独证明我们的 allow 被采用 |
| warning/configWarning/deprecationNotice/guardianWarning | lifecycle/native diagnostic | 默认非 terminal；严重性保留原生含义 |
| hook/started / hook/completed | lifecycle/native hook | 只记录 hook 的运行，不冒充工具状态 |
| model/rerouted、authRecoveryStarted/Completed、account/rateLimits/updated | lifecycle/native configuration/diagnostic；modelResolved/额度按已知字段更新 | reroute 来自 native，不是 runtime 偷换；额度不同于 usage |
| 未启用的 realtime、process/fs 扩展、其它通知 | opaque | 不声明未实现的扩展 capability；native process/output 也不等于主 TUI 字节 |

| Codex server request | Interaction 类型 / 回写 | 必须保留的区别 |
| --- | --- | --- |
| `item/commandExecution/requestApproval` | approval；`{id,result:{decision:<selected native decision>}}` | `availableDecisions` 有则严格使用；requestId、approvalId、kind(command/writeStdin) 都保留；不能仅按 itemId 去重 |
| `item/fileChange/requestApproval` | approval；decision=accept/acceptForSession/decline/cancel | grantRoot 不自动扩权限；永久规则不作为默认按钮 |
| `item/permissions/requestApproval` | approval；`{permissions:<allowed subset>,scope:turn\|session}` | 返回权限子集而非通用布尔；v1 仅提供精确批准请求集或空 grant，禁止扩大 |
| `item/tool/requestUserInput` | question；`{answers:{<questionId>:{answers:[...]}}}` | isBlocking 原样保留；option label/value 由 encoder 匹配；不把旧 autoResolutionMs 当自动作答授权 |
| `mcpServer/elicitation/request` | elicitation；`{action:accept\|decline\|cancel,content?}` | 按 mode/form schema；未知扩展不自动 accept；turnId 可为空 |
| `execCommandApproval` / `applyPatchApproval` | legacy approval adapter，按独立 schema | 旧 approved/approved_for_session/denied/abort 与 v2 accept/decline 不混用；v1 runtime 默认不声明 legacy 支持 |
| `item/tool/call` | 已登记 dynamic tool 执行；否则 native error | 不是审批；不允许模型创建任意 host RPC handler |
| `account/chatgptAuthTokens/refresh` | 私有 credential service 回应或原生 auth error | token 不进入 Observation 和 Interaction 普通表单 |
| `attestation/generate` / `currentTime/read` | 仅真实实现并声明该 capability 时响应 | 不伪造 attestation；未实现返回方法不支持 |

**grok-acp：**入站标准 ACP 对象有 `jsonrpc:"2.0"`，stdio 一行一帧，无 Content-Length。`session/update.params.sessionId` 与 `params.update.sessionUpdate` 为主要分流字段；headless `streaming-json` 的 `{type:"text"|"end"}` 是另一套入口，不能直接套在 ACP driver。native request_permission 由 Node 这个 ACP client 回答，浏览器不直连 Grok。以下实测范围取自 Grok 专项报告；标准中存在但探针没触发的 variant 仍要独立 fixture。

| ACP 原生方法 / `sessionUpdate` | 统一事件 | 规则 |
| --- | --- | --- |
| initialize response | lifecycle/native session/initialized，保存 protocolVersion/agentCapabilities/authMethods/_meta | 当前协议 1；版本在 _meta.agentVersion，没有 agentInfo；image/audio=false，不能因 ContentBlock 可表达就接纳图片/音频 prompt |
| session/new / session/load response | Instance.nativeRef + lifecycle/ready | session/load 同进程和新进程已验证；加载期间 history 的 source.delivery=replay，不创建新 Run；_meta.isReplay 作为补充标记 |
| session/resume / session/close / session/list response | native session 生命周期/索引 | resume 仅同进程无 replay 实测；close 的 _meta["x.ai/closeOutcome"] 原样保存，不等于监督进程已退出 |
| `agent_message_chunk` | message/assistant append，ContentBlock 映射 | ACP 无 message ID 的区段用 session+prompt RPC+连续 block 序号的 adapter key；runId 明确才能关联 |
| `user_message_chunk` | message/user，partial | 历史回放或 live echo 必须区分；不由文本推断 send ACK |
| `agent_thought_chunk` | thought append | 缺失 thought 不补空块 |
| `tool_call` | tool_call，toolCallId/rawInput；toolName 来自 _meta["x.ai/tool"].name，title 单独展示 | live 可无 kind/status；缺少 name 则 unknown，不用 title 冒充稳定工具名；pending/in_progress 不等于成功 |
| `tool_call_update` | tool_call 更新或 tool_result；content/rawOutput/status | completed→succeeded，failed→failed；不含字段保持旧值，不置空；未知 status 留 opaque |
| tool content 的 `content` / `diff` / `terminal` | ContentBlock / 拟议或已知 diff / 原生 terminal 引用 | terminal ID 不是 tty-attach 承诺，diff 是否 applied 需 native 工具状态 |
| `plan` | lifecycle/native plan/updated | 是执行计划，不是 workflow，也不是 plan-review approval |
| `available_commands_update` | lifecycle/native configuration/commands | 仅更新可用命令目录 |
| `current_mode_update` / `config_option_update` | lifecycle/native configuration | option list 是完整快照；model 只在返回/更新确认后标 resolved |
| `session_info_update` / 版本支持的 usage update | lifecycle/native session metadata / usage | title 有实测；usage 依 §5.5 的 ACP 口径，未映射字段保留 raw |
| `session/request_permission` request | interaction.requested/approval | 原 RPC id+toolCall.toolCallId+sessionId；保留所有 optionId/kind |
| request_permission response | `{outcome:{outcome:"selected",optionId:<native id>}}` 或 `{outcome:{outcome:"cancelled"}}` | 选中的 native option 可为 allow_once/allow_always/reject_once/reject_always；不能任意生成 optionId |
| `session/prompt` response | 对应 Run native-turn terminal | `stopReason=end_turn` 可正常结束；cancelled→cancelled；max_tokens/max_turn_requests/refusal 等保留为非成功限制/拒绝结果；未知值不成功 |
| `session/cancel` notification | Command transport-written，等待 prompt response 的 stopReason | cancel 没独立 RPC ACK；不能在发送 notification 时结算 cancelled |
| `session/set_config_option` | v1 configure 返回 CAPABILITY_UNKNOWN，不发送猜测的输入 | 文档 value:{value:"low"} 实测 -32602；有 configOptions 不证明 encoder 可用 |
| `_x.ai/session_notification` / `_x.ai/session/update` | 按下表的已知内层类型；其余 opaque | wire 有前导下划线；文档 x.ai/fs/list 实测 -32601，不能自动去掉下划线 |
| `_x.ai/*` 其它扩展 | opaque；只有已注册的方法可以控制 | 文件/git/terminal/auth 扩展不透传给网页的通用调用器 |
| 反向 `fs/read_text_file` / write_text_file / terminal 等 client 请求 | 默认不声明、不提供；若未来声明须由 Node 受限 executor 服务 | 实测声明能力会改变工具执行位置；未实现不谎报支持，也不把文件写请求当审批通知 |

Grok `session/prompt` 的 JSON-RPC response 是完成而非早期 admission。没有 native user echo/started 的强证据时，Command 可能停留 queued+transport-written；只有能明确归属该 prompt 的模型/工具 update 才可证明进入执行，之后推进 accepted。available_commands_update、自动标题、settings 通知和 load replay 都不能当此证据。不能为追求与 Codex 一致而伪造一个早期 ACK。实际支持的 stopReason/extension 枚举必须由目标 Grok 协商/fixture 决定，ACP 标准存在不代表该 build 发送过所有变体。

| Grok `_x.ai/session_notification` / `_x.ai/session/update` 内层 sessionUpdate | 统一映射与限制 |
| --- | --- |
| `hook_execution` / `hook_run_started` | lifecycle/native hook；失败保持 diagnostic，不自动禁用 native hooks |
| `session_summary_generated` / `last_turn_summary` | native session/turn 摘要，不能把摘要文本当 prompt ACK 或终态 |
| `response_completed` | usage snapshot 候选与 native response 生命周期，不独立结束 prompt Run |
| `turn_completed` | native turn terminal 的补充 usage/elapsed_ms；结算仍由对应 session/prompt response 提供 |
| `tool_call_delta_chunk` | 已知工具 ID 的 partial 更新，未知 delta schema 保留 opaque |
| `pending_interaction` | native permission/activity 提示；**notification 无 RPC id 时不可答**，不据此阻塞调度或建立可批准卡片 |
| `interaction_resolved` | 已关联原生 interaction 的 cleared 证据；不证明 runtime 答案被采纳 |
| `background_tasks` | native task 快照；不把空表当 prompt 之外任务全部完成 |
| `model_changed` | lifecycle/native configuration；保留实际字段并更新未来选择观察，不回填过去的 model |

always-approve 探针只出现 pending_interaction→interaction_resolved，零条 session/request_permission；因此上述标准 permission encoder 是待验收候选，不是已完成的审批往返。Grok 实测 §5 `_x.ai/session/prompt_complete` 也只作辅助终态观察；`_x.ai/models/update`、`settings/update`、`mcp/servers_updated`、`mcp_initialized`、`announcements/update`、`queue/changed`、`sessions/changed` 分别进入 native configuration/diagnostic 快照，未知 payload 留 opaque。两个扩展通道若同发一件事，必须有稳定原生 ID 才归并。

**agy-print：**当前落盘 fixture 只有以下已验证记录；失败、工具、提问、input stream 更完整 schema 尚未建立，不借用 Claude 或 Gemini 的字段填空。

| agy 原生记录 | 统一事件 | 规则 |
| --- | --- | --- |
| `event:init` | lifecycle/native session/init，conversation_id；tools/cwd/permission_mode | 无 model 字段，不填“已解析模型”；state=DONE 以外枚举未由此样例建立 |
| `event:step_update`, step_type=user_input | message/user 或输入观察 | 样例只有 conversation_id/step_index/state，无原 prompt 回显，不能据此拿文本做 command 去重 |
| step_type=agent_response | message/assistant partial，text_delta；usage 快照 | key 包含 conversation_id、input 关联、step_index；重复更新的 delta/replacement 语义未验收时保留 partial，不双算文本 |
| `event:result`, status=SUCCESS | message final response 的 replace、usage、Run native-turn 成功候选 | 核对 conversation ID 与唯一 input；单次进程还需正常退出；非 SUCCESS 枚举先保留 raw 并明确无成功结论 |
| 未知 step_type / event / status | opaque | 不把 ask_permission 工具名的存在当已接 broker；不把 agy SQLite 当 JSONL tail |
| EOF / 非零退出 / timeout | lifecycle/process 或 diagnostic | 无匹配 result 时 unknown；非零退出可证明进程失败但不证明任务未产生效果 |

具体字段依据 agy-stream-json-sample、control-plane §6。

**claude-pty / generic-pty 的 carrier：**Claude 的语义内容来自 §5.6 的 transcript/hooks/workflow journal；generic 仅有下面的 carrier observation。

| carrier 事件/方法 | 统一映射 | 限制 |
| --- | --- | --- |
| 自有 PTY output bytes | raw_tty/output，按 byte offset 保存 | 完整字节不等于结构化语义 |
| PTY resize / input 写入 | raw_tty/resize 或 input | input ACK scope=tty-bytes；不能证明 agent 接纳 prompt |
| supervisor process exit | Instance exited，记录 exit code/signal | shell/CLI 退出不自动让 native Run succeeded |
| Herdr `pane_created` / `pane_exited` / `pane_closed` | lifecycle/native carrier | pane_exited 可能是 shell/main process，不是完整 agent task 终态 |
| `pane_agent_detected` / `pane_agent_status_changed` / subscription `pane.agent_status_changed` | activity 提示，screen-derived 或明确 hook 来源的 partial | done/idle 是状态检测，不作为成功证据 |
| `pane_output_changed` + `pane.read` | opaque/screen-snapshot，存 content revision | 这是屏幕/scrollback 快照，禁止冒充连续 PTY byte stream |
| `herdr terminal session observe` / `control` 的 `terminal.frame` | raw_tty/output，representation=rendered-ansi，completeness=screen-derived | base64 解码 bytes；保留 per-client seq、width、height、full。它是服务端重绘/增量 ANSI，可给终端渲染器，不是源 PTY 原始输出 |
| `terminal.closed` / observe EOF | lifecycle/native carrier/closed | 只确认此 terminal transport 关闭，未必 agent process 已退出 |
| `agent.start` / `agent.prompt` / `agent.wait` response | carrier ACK / observation | wait 匹配 detector 状态不结算 runtime Run；没有 native receipt 的 prompt 保持接纳未知 |
| `pane.report_agent_session` 的 session 信息 | NativeRef 候选，经 host/pane/process 核对后绑定 | 探针元数据不是操作者身份；不得覆盖不同 generation 的 session |

Herdr 的 ANSI bridge 已有明确源码入口：terminal_sessions.rs 将 `terminal.frame` 输出成 JSONL，control 接受 `terminal.input`（text 或 base64 bytes 二选一）、`terminal.resize`、`terminal.scroll`、`terminal.release`。本次进一步核对 Herdr HEAD `1a7c691559bb6ea8ad366bce68f87f8c3f6db098` 的 TerminalFrame、render_stream.rs、render_ansi.rs：bytes 由 FrameData/BlitEncoder 生成，seq 是 per-client，full 表示重绘。因此 PTY spike 所称“原始 ANSI 流”在此精确记作 **rendered-ansi**，不宣称保留源 PTY 字节、原生滚动历史或跨 attach 的稳定 offset。

规格决定：已装 Herdr 的 Node 优先用该 bridge 承载终端；backend 仍可替换。Node 为每次 Herdr terminal 连接创建 streamEpoch，持久保存自己收到的 rendered-ansi bytes 与 offset。Herdr 重新连接必须换 streamEpoch，并从 full frame 重建显示；Hub 断线而 Node bridge 未断则可按 Node offset 补页。`pane.read` 的普通文本仍不能替代这个 bridge。检测器只用于 activity 提示；spike 已见瞬时 idle 闪烁，禁止据此自动发送下一 prompt。PTY spike §1、§4

### 5.8 成功、取消与“不知道”的结算规则

Run 的成功规则以 `{driverKind,binaryDigest,adapterVersion,completionScope,ruleId}` 固定，不由模型回答“done”决定。`native-turn` 与 `task` 的 UI 文案分别是“本轮结束”和“任务结束”；完成当前轮时始终返回 outstandingWork。业务要求如测试通过、文件发布成功只属于工具/产物的验收证据，不能从 Run.succeeded 推出。

| driver | native-turn 终态 | task 终态与不完整情况 |
| --- | --- | --- |
| Claude print | 匹配 session、明确 input/result 关联的 result；检查 subtype、is_error、error/terminal_reason，不只看 subtype=success | 持续 stdin 没有已验证的整体任务终止信号时 `completion-task=unknown`。多个 result、task notification、peer 输入各自保留回合关系。空 task ledger、queued_turn_count=0、Stop、idle 都不能自行关闭 stdin并结算 task |
| Claude 单次有限输入，确实自然退出 | 可从匹配 result 结算本轮 | 仅在已验证的单次 carrier 正常结束、完整输出已 drain、最终 native 结果覆盖所声明 task 范围时结算。为了制造 EOF 提前关 control stdin 或杀进程不属于自然退出；无该证明返回 unknown |
| Claude PTY | 仅版本化的真实原生回合证据可以结算；单 Stop hook 为 candidate | 无原生 task end 时保持 partial/unknown。Workflow 自己的终态可以显示，但不证明 root 再无工作；原生 TUI继续可用 |
| Codex app-server | 相同 threadId/turnId 的 turn/completed.status | v1 默认 native-turn；不自动包含可晚到的 subAgentActivity。task scope 只有原生任务定义与全部成员终态被可靠枚举时开启 |
| Grok ACP | 原 session/prompt request 的 response.stopReason | 覆盖的是该 prompt；原生长驻/后台扩展是否包含在内须独立能力验证 |
| agy print | 已识别 SUCCESS result，匹配 conversation/input；单次 carrier 等正常退出 | 目前只对已验证单次无后台扩展样例成立，复杂 task scope 为 unknown |
| generic PTY | 不支持语义 native-turn | 只有 process ended；无 semantic succeeded |

Claude print v1 默认使用 `completionScope=native-turn`，长期保留 control stdin。task notification 引发的续跑由 Node 建 `cause=native-continuation` 的 Run；有 task/tool ID 证据才连 parent，否则 parentage=unknown。UI把同一 Instance 的原生回合、Workflow、runtime children并列呈现。创建 Run 的 send（以及含 initialInput 的 create）自动要求对应 completion-native-turn 或 completion-task 能力，不能通过省略 requiredCapabilities 绕过；当前 completion-task 不满足就拒绝创建该任务。不带 initialInput 的 create 只建立 Instance，completionScope 是未来输入默认值，不因此拒绝 generic-pty 或 claude-pty 的原生终端；它们可用 tty.write，交付范围仅为 tty-bytes。调用者可明确选择 native-turn 并观察 Workflow 终态，不能假装整体任务已完成。恢复/导入或原生外部输入的 Run 若缺少终止证据则保留 unknown，wait 如实返回。主 agent 与 Workflow 的原生执行因此保留，未解决的是 runtime 对“全部后台续跑完成”的证明。

cancel 的 RPC response 仅是请求被接收：Codex 等到 turn/completed=interrupted；Grok 等到原 prompt stopReason=cancelled；Claude 等到匹配中断证据。若 cancel 与自然完成竞态，记录实际原生 outcome，cancel command 的结果注明 `alreadyTerminal:true`，不能把成功结果改成 cancelled。进程被 owner 明确强制终止时可记录 `cancelled` + `ruleId=owner-forced-stop`，同时 outstandingWork/effects 保持未知；不得声称原生优雅撤销或回滚工具副作用。

禁止据以推断完成的证据：stdout EOF 单独出现、HTTP/WS 200、命令接纳、最后一条 assistant 文本、`phase=final_answer`、无输出 N 秒、pane done、空 hook 队列、文件存在、一个 workflow member 返回、所有**已知**任务变空、取消写入成功、host 离线。已失去必要证据或终止语义不足时，wait 返回 reason=unknown 及 Run 的 unknown/reconciling 状态，不返回成功。已知仍在运行的任务正常等待条件，超出本次等待时间返回 reason=timeout；不能仅因尚未结束就立刻把它改成 unknown。

## 6. Interaction broker

### 6.1 单次决定、原生回写与断线

Node 保存待答请求与 native channel 的 waiter；Hub 只路由。请求到达时先持久化原始记录与 Interaction，再向设备推送。`interaction.respond` 的参数必须含 `{commandId,interactionId,requestVersion,processGeneration,runGeneration,connectionEpoch,answer}`。浏览器/Bot 不能提供不同的 nativeRequestId 来改变路由；Node 用 Interaction 的已存 requestKey。

裁决事务核对：actor 权限、Instance ownerFence、原生 generation/connection、requestVersion、pending 状态、deadline、输入/计划 digest、answer schema。CAS 成功后同时写入唯一 answer、Command、outbox intent，才响应“答案已记录”。同 commandId 重试返回相同结果；另一设备的另一 commandId 返回 `INTERACTION_ALREADY_ANSWERED`，同时附最新可读实体。native 输出 writer 只接收到这一份被提交的答案，不能让每个设备直接连接 CLI。

Interaction `answer-committed → resolved` 与工具执行分离：UI 可以显示“已选择，等待原生确认”；delivery=written 仍不得显示“已执行”。Codex 的 serverRequest/resolved 也可能由 cleanup 触发，须结合已发送答案与工具后续状态说明结果；Claude/ACP 没有通用 ACK 时，只有明确对应的结果/请求结束证据才 confirmed。若只能确认请求已消失，reason=native-cleared，answer effect 保持 unknown。

~~~mermaid
sequenceDiagram
    participant Native as Native CLI
    participant Node as Node broker
    participant Hub as Hub
    participant Phone as Phone
    participant Mac as Mac
    Native->>Node: native request R
    Node->>Node: persist Interaction version 1
    Node->>Hub: interaction.requested seq N
    Hub->>Phone: pending R v1
    Hub->>Mac: pending R v1
    Phone->>Hub: respond command A
    Mac->>Hub: respond command B
    Hub->>Node: A and B
    Node->>Node: CAS A wins, durable answer/outbox
    Node-->>Hub: A answer-committed, B already-answered
    Node->>Native: one native response
    Native->>Node: native resolution or tool outcome
    Node->>Hub: lifecycle delivery/resolution seq N+K
~~~

| 故障/竞态 | Node、Hub 与设备行为 |
| --- | --- |
| 手机或 Mac 断线 | Node waiter 继续；重连 snapshot 重现同一 Interaction。关页不拒绝、不批准、不取消 |
| Hub 断线但 Node/native 都活着 | Node 保留 waiter；Hub 的缓存标 stale，不能离线裁决 approval。native deadline 到达按已设策略拒绝/取消或留原生 TUI，不能自动 allow |
| Node 重启，原生 stdio owner 也死亡 | 旧 connectionEpoch 所有未答请求 invalidated；已答未确认保持 delivery unknown。resume 后的新请求有新 ID/version，不重用旧答案 |
| Node 重启，Herdr/native 进程仍活着 | Instance reconciling；核对 PID birth/native session/pane、broker invocation。只有证明原请求仍等待且未被回答时重新发布；没有原生 replay/query 就不能恢复自动回答 |
| 答案写出后、落账前崩溃 | outbox 已有 intent，视为可能发送；不能盲目 replay response。查询 native pending/resolution，不可判定则 unknown |
| 原生重放同 requestId | 同 generation+epoch+payload digest 视为同一请求；若内容不同，version 加一且旧请求失效。RPC id 重用但属于新 epoch，不与旧请求合并 |
| timeout 与用户答案同时到 | Node 单个事务时钟与 state CAS 决定；deadline 已过则拒绝。不能因 UI 卡片还显示而复活请求 |
| 原生 TUI 本地先回答 | 待答卡片失效/原生已解决；随后远程答案返回 stale/已解决。无法观察清理时 broker 不宣称有独占回答权 |

hook invocation 自身可使用小型本地持久 spool，把 invocationId、启动 generation 与完整 payload 交给 broker；观察 hook 快速返回原生要求的中性结果。**审批 hook 不能像普通观察 hook 一样先输出 `{}` 再后台等待**；其 stdout 是真实决策通道。helper socket 只允许同 OS 用户/受限凭据访问，绑定 launchId，任意别的进程不能凭一个 sessionId 创建审批请求。

### 6.2 Claude 的三个承载方式

| 方式 | 可行性、选择 | 精确限制 |
| --- | --- | --- |
| `--permission-prompts host` + `--permission-prompt-tool stdio` + stream-json control | claude-print 首选；2.1.268 已验证 allow/deny 和 AskUserQuestion 单选 | 必须先 initialize，持续读流并回写；单独 host flag 已验证不足，多选/plan/elicitation 仍逐 schema 验收 |
| `PermissionRequest` hook | 已验证 print 下阻塞 allow/deny；claude-pty 仅在 runtime 是唯一等待 responder 时启用 | 没有 tool_use_id/requestId；hook invocation 本身是 callback 身份；不覆盖所有 question、sandbox prompt 或 TUI 交互 |
| PTY 原生 TUI | generic-pty / claude-pty 保留原生终端；herdr blocked + 有界屏幕生成 approval/question | 按下文的显式人工回答规则发送按键；无原生语义 ACK，不自动同意、不按坐标点击 |

**PTY blocked adapter（2026-09-13，本次任务明确替换上述屏幕不可答限制）：**

- `generic-pty`（codex/grok/agy 等）和 `claude-pty` 只在 `agent.get.agent_status=blocked` 时建卡；读取和回复绑定精确 pane ID，agent alias 每次启动唯一。取当前 visible pane 最多 32 行、4096 UTF-8 bytes 的 excerpt。重复状态/屏幕不重建卡；截断、空屏幕或重复编号的歧义菜单不可答。
- `carrier=native-tty`、`completeness=screen-derived`、`nativeRequestKey=none`，每次阻塞/屏幕变化生成新的 Interaction ID 和 epoch。`[y/n]` 暴露原生 y/n；编号菜单保留编号/标签，光标标记明确时编码相对 up/down + Enter，否则编号字符 + Enter；明确的 `Enter to continue` 只发送 Enter。其余提示提供单行文本，最多 1024 bytes，禁止控制字符。
- 人工回答统一走 Hub answer → Node first-answer-wins broker → driver，与 `tty.write` 共用 `agent.send_keys` 传输。发送前再次检查 blocked、状态序号和屏幕；旧卡/变屏拒绝。保存已消费 ticket 后才写入，任何可能写过的回答不重放；原生终端的外部写者仍可能在检查后改变状态，不能声称有跨 herdr/本地人的原子输入租约。
- 按键写入只证明 `delivery=written`。观察到 idle/working/done 或旧屏幕被替换，发 Interaction entity lifecycle `resolved`、`resolution.reason=native-cleared`；答案对工具执行的效果仍未证明。进程关闭使未完成 ticket invalidated。Node 重启没有原生 waiter 时旧 pending 行不可答，已消费/已结算行不重新出现在待答列表。
- 协议的 Instance lifecycle 仍为 running，activity 是 waiting-interaction；Hub/CLI 显示 blocked。`instance wait --until blocked` 使用当前状态，历史 interaction 事件不会让已 idle 的实例再次命中。CLI `instance respond <id>` 列出待答项，`--option`、`--text` 或 `--answer` 回答；MCP 为 `remuda_instance_respond`。Feishu 卡片保留 PTY 候选项 ID 与屏幕摘要，回调走相同 Hub answer endpoint。

以下是 SDK 定义与 2.1.268 交互探针 支持的**线协议示例**，ID、工具和路径为示意，初始化 response 省略已知可选目录。adapter 必须锁定 CLI/SDK 版本并回放真实 fixture；未知 subtype 使用已知 control error 机制报告并将交互标不可答/unknown，不自动同意。`request_user_dialog` 只在 initialize 的 supportedDialogKinds 中声明已完整实现的种类；未声明种类不会因可以画表单就自动获得回写能力。SDK 类型参考

~~~json
{"type":"control_request","request_id":"runtime-init-1","request":{"subtype":"initialize"}}
{"type":"control_response","response":{"subtype":"success","request_id":"runtime-init-1","response":{}}}
{"type":"user","uuid":"01993ab0-0000-7000-8000-000000000002","session_id":"01993ab0-0000-7000-8000-000000000003","message":{"role":"user","content":[{"type":"text","text":"Read the project README."}]}}
{"type":"control_request","request_id":"native-perm-1","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"/work/project/README.md"},"tool_use_id":"toolu_example"}}
{"type":"control_response","response":{"subtype":"success","request_id":"native-perm-1","response":{"behavior":"allow","updatedInput":{"file_path":"/work/project/README.md"}}}}
~~~

输入 origin 只有真实用户输入才可标 human；bot/agent 输入不伪装 human 以触发 ultracode。实际支持的 origin enum 按 native schema 编码，不认识的 runtime origin保存在自己的 Command，不瞎造 native 值。`updatedInput` 是完整原输入；v1 允许决定不修改输入，不能发空对象误删参数。拒绝编码为 `behavior:"deny",message:<reason>`；需要中断时只能使用该版本支持的 interrupt 字段。control_cancel_request 撤销指定 request 的 waiter，不再回旧答案；control_response 是对宿主发起控制的响应，不能当普通业务消息。

AskUserQuestion 的已验证请求同样为 `can_use_tool`，`tool_name:"AskUserQuestion"`，`input.questions[]` 有 `question/header/options[{label,description}]/multiSelect`，并可能有 `requires_user_interaction:true`。它必须显示 question UI，不能作为普通 Approve 按钮。Node 给每个 question/option 分配 UI ID，保留到原 question 文本与 option label 的映射；单选回答编码为完整原 input 加 answers，outer control response 继续 echo **request_id**：

~~~json
{
  "behavior": "allow",
  "updatedInput": {
    "questions": [{"question":"Tea or coffee?","header":"Beverage","options":[{"label":"Tea","description":"Tea"},{"label":"Coffee","description":"Coffee"}],"multiSelect":false}],
    "answers": {"Tea or coffee?":"Tea"}
  }
}
~~~

answers 的 key 是原 question 全文，值为选择的原 label；所有非 answers 的原 input 字段都保留。相同 question 文本或重复 option label 若使该 encoder 无法无歧义表达，返回 `INTERACTION_SCHEMA_UNSUPPORTED`，不靠索引写入错误答案。多选和自由输入虽能由统一 schema 表达，当前探针只验证单选；须取得相应 fixture 才开放。ExitPlanMode 必须有原生暂停请求、可见 plan 与准确 encoder；批准只按已展示选项作用，不隐式追加 setMode=bypassPermissions。单独 elicitation/request_user_dialog subtype 各自注册，不能靠 can_use_tool encoder 声称全覆盖。AskUserQuestion 往返

### 6.3 Claude hook 的输入、输出与冲突

Claude `PermissionRequest` 的输出必须是该事件专属的 hook JSON，不能复用 VibeBuddy/Codex 的布尔或 decision 名。普通拒绝的最小示意如下；无论超时还是用户拒绝，都不输出 allow。[Claude 官方 hook 决策](https://code.claude.com/docs/en/hooks#permissionrequest-decision-control)

~~~json
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": {
      "behavior": "deny",
      "message": "The request was not approved."
    }
  }
}
~~~

该 hook 输入缺少 tool_use_id 时，broker 仅能确定“这次阻塞 invocation 的请求”，不能借 PreToolUse 的最新工具 ID 填上；两个并发同名工具必须分成两个 invocation。原生 deny/ask 规则仍可能否决 hook allow；hook allow 不是绕过 native policy 的授权。只用退出码 2、空输出或 Notification 不等价于明确 PermissionRequest deny。sandbox 网络提示等并非一定有此 hook，仍需 TUI 通道。[官方 PermissionRequest 输入和行为](https://code.claude.com/docs/en/hooks#permissionrequest)

同一原生请求禁止同时挂 runtime blocking PermissionRequest 与 runtime host can_use_tool 两个 responder；按 driver 选择一个，另一路只做不阻塞观察。已有第三方 hook 若也会返回 allow/deny，materializer 记录 `approvalAuthority=unknown` 并禁用统一 approval 按钮；声明必须有 interactive-approval 的 print spec 返回 `CONTROL_UNAVAILABLE`，有原生 TUI 的 profile 继续提供 TUI。不能认为“有多个 hooks 就先到先得”，也不能暗中删除第三方 hook。

本机 Flux PermissionRequest 配置 timeout=86400；交互探针通过空 setting-sources 排除了它，证明了新 broker 的编码，但未证明保留全部现用 hooks 时仍无竞争。因此生产必须选择已经准备好的无冲突持久 native store，或逐项登记现有 responder 的协作方式；此配置准备不在本任务中执行。缺省 native command hook timeout 文档为 600 秒，探针用 30 秒验证 allow/deny；没有实等 600 秒的超时证据。`PermissionRequest` 的输入/输出详见 交互探针 §3，并保留 hooks-integrations 的原配置来源。

### 6.4 超时策略与权限默认值

MVP 不把 bot 等同于无人审批：默认 Claude human/bot 都采用已配置的 native manual/auto 决策与可工作的审批 carrier；auto 由 Claude 自己评估，runtime不再自动 approve 所有 server request。bypass/always-proceed/never 等保持原生不同含义，只有 spec 明确选择才使用，不能做跨 driver 的无损布尔映射。

`interactionDeadlineMs`、`hookDeadlineMs`、`controlWriteTimeoutMs` 是 Node 配置，记录在 LaunchManifest；hook helper 的 deadline 必须早于原生 hook timeout，留出写出合法 deny 的时间。原生不支持外部暂停时，在超时后使用已验证的 native deny/cancel encoder，或保持 native TUI等待；不支持时记录 `CONTROL_UNAVAILABLE`，禁止把超时当 consent。由 deadline 导致的拒绝是系统策略事件，actor=system，与人类选择分开。Node→Hub 断线不延长已给出的 native deadline。

Claude can_use_tool 没有可假定的 native park deadline；配置没有 runtime deadline 时保持 pending，不能编一个“CLI 已拒绝”的时间。设置 runtime deadline 时须在请求中明确其来源，并把超时决定作为 Node 的系统 answer/outbox 处理；只有 native 已取消/清理的证据才记原生 resolved。`defer + resume` 不属于 v1 已验收的交互恢复机制，不能用它保证长时间手机审批一定可恢复。交互探针 §1、§7

## 7. Hub ↔ Node Agent 协议

### 7.1 网络、版本和身份

默认 Node 主动连接 Hub `wss://<hub>/node/v1/connect`；经 SSH 隧道承载时仍使用同一应用协议。人使用 Web/PWA 的 HTTPS/WSS client endpoint，Bot 经 Hub dispatcher，不能直接访问 native app-server、ACP、Herdr socket 或模型网关管理面。长连接断开只影响控制与观察同步，不杀已托管进程。拓扑沿 [proposal §4](proposal.md#4-架构分层)；环境已有 SSH、不要求先安装 Tailscale 的依据是 env-inventory §6。

Hub wire 使用 JSON-RPC 2.0；请求 `{jsonrpc:"2.0",id,method,params}`，通知不带 id，response 必须且只能有 result/error 之一。RPC id 是每个方向、每个 connectionEpoch 内唯一的 string；不能跨连接重用它来判定 Command 是否执行。第一条应用请求为 `runtime.hello`，成功前除关闭连接外不接受其它操作。

~~~json
{
  "jsonrpc": "2.0",
  "id": "hello-1",
  "method": "runtime.hello",
  "params": {
    "hostId": "hst_01993ab0-0000-7000-8000-000000000004",
    "nodeEpoch": "epoch_01993ab0-0000-7000-8000-000000000005",
    "nodeVersion": "0.1.0",
    "protocol": {"major": 1, "minMinor": 0, "maxMinor": 0},
    "observationSchemaMajors": [1],
    "features": ["snapshot-follow-v1", "tty-binary-v1"],
    "resumeCursors": []
  }
}
~~~

hello result：`{protocol:{major,minor},connectionId:Id,serverEpoch:Id,observationSchemaMajor:1,features:string[],limits:TransportLimits,lease:{leaseId:Id,fence:U64,expiresAt:Timestamp},reconcileRequired:boolean}`。选择共同 major/minor 和 feature 交集；不存在共同 major 返回 `PROTOCOL_VERSION_UNSUPPORTED` 并关闭。native CLI 的 SDK/ACP/Herdr version 独立于本协议。minor 只增加可忽略字段；新的控制方法/enum 必须通过 capability 与 feature 声明，不依赖老客户端“忽略未知值”实现安全兼容。

`params.instances`（可选，additive）：Node 本次进程拥有的 Instance 清单，每项为 §2.3 的 Instance 对象（Hub 读其 `id` 与 `lifecycle`）。两种 carrier 都发送它——`ssh-stdio` 与 outbound WSS 同等——因为 Hub 只依据它做重启对账：`nodeEpoch` 变化时，Hub 记录中仍 live 而清单里不再 live 的行会被投影为 `exited` 并写入 `lastError: "node-epoch-changed"`（Hub 侧同样以该字符串作为 worker 的 blocked reason），同时释放其 placement slot。判定依据是条目的 `lifecycle` 而非其是否存在：Node 自报 `exited`/`failed` 的条目是**已承认的丢失**，不得因其出现而保护 Hub 的旧行。缺失该 key 表示“无法枚举”而不是“没有实例”——Hub 据此保持现状不动任何行，因此老 Node 的沉默不会被当作否认。

**空清单的额外要求**：`[]` 是唯一会产生破坏性后果的取值（“我一个都没有”会让 Hub 结算该 host 上每一行），因此只有 `params.instanceStoreFound: true` 与它同行时 Hub 才采信。该字段表示 Node 确实在预期位置找到了 instance store：`--data-dir` 指错、磁盘被清空、或使用内存 store 时都会枚举出零行，而这不代表任何东西，此时 Node 必须**省略** `instances`（而非发送 `[]`）——Hub 对两种沉默一视同仁。`requested` 行不在对账范围内：它是 Hub 单方面、尚未被 Node 确认的意图，重启窗口内的缺席不能证明它已丢失，由 `expire_stale_requested` 按时间单独回收。

身份层：Node 优先使用每 Host 单独的 mTLS client certificate，证书身份绑定 enrollment hostId；简化部署可用 TLS + 每 Node 独立的 256-bit 随机 token，Hub 只保存校验材料，支持轮换/撤销。设备 token 与 Node token 是不同身份，不能互换；Web 登录后使用同源安全 cookie 与 CSRF 保护，WS 首次鉴权使用短时会话凭据/同源校验，不把长期 token 放 URL。SSH 登录只负责隧道，不代替应用内 actor/scope 检查。MCP agent capability 与 Bot token 也是受限身份，不能获取 Hub 管理 credential。

Hub 生成 ownerFence，Node 在本地 durable store 单调保存；旧 fence 的命令返回 `OWNER_FENCED`。一个 Node 的新连接取代旧连接，新连接完成 reconciliation 前不派发变更。Node 必须使用 OS 单实例锁和每 native session 的本地 writer lock；没有锁实现的 host 不能把 lease 宣称为防止重复进程的充分保证。网络分区时旧运行可继续原生执行，但 runtime 不在另一 Host 启动同 Instance，也不在租约失效后接受新的跨主机控制任务。

### 7.2 方法列表与命令生命周期

原生执行、资源生命周期和 Interaction 决定使用 §2.5 的 Command wrapper：`{commandId,payload,expected?,expiresAt?}`；Node 和 Hub 都校验 schema/身份。表中的 payload 是该 method 的业务参数；§6/§7.4 的完整参数列表若含 commandId，该字段外置到 wrapper，不在 payload 再保留一份。公开 response 为 `{command:Command,relatedCommandIds:Id[]}`，除特别写明外不等待模型任务结束。连接握手、订阅、lease、幂等 object 上传和 resize 使用各自明确的 ID/revision，不伪装为模型任务。只读方法不启动/恢复 native process。

下表是本协议的**目标方法全集**（46 个，与 `MethodName` 一一对应），不是“当前 Hub↔Node 链路已可调用”的清单。M1 阶段 Hub 与 Node 实际互通的是 [`hubnode.rs`](../../crates/remuda-protocol/src/hubnode.rs) 中更小的运维子集：`node.auth`、`node.hello`、`node.heartbeat`、`instance.create/send/cancel/respond`、`interaction.respond`、`journal.append`、`tty.frame`、`tty.write`、`instance.keys` 以及 `runtime.hello`/`runtime.heartbeat` 别名。该子集是分阶段实施的落地面，本表的语义与约束对它同样有效；两者不是两套相互竞争的语义。逐方法的实现覆盖见 [protocol-audit-1.md](protocol-audit-1.md) §7。

| 方法 | 方向 | payload / result |
| --- | --- | --- |
| `runtime.hello` | Node→Hub | §7.1；完成版本、身份与重连协商 |
| `runtime.heartbeat` | 双向 | `{connectionId,registryWatermarks:[{journalId,durableSeq}],instanceWatermarks:[{journalId,durableSeq}],leaseId}` → `{serverTime,leaseExpiresAt}`；各 registry journal 分别记水位，时间仅用于 lease 展示/校验 |
| `host.report` | Node→Hub | `{hostId,nodeEpoch,platform,driverInventory,workspaceIds,instanceIds}` → `{hostRevision,registrySeq}`；不接受自报另一 host |
| `host.get` / `host.list` | Client/Hub | get `{hostId}` → Host；list `{cursor?,limit}` → `{items,nextCursor}` |
| `driver.list` / `driver.capabilities` | Hub→Node | `{hostId}` / `{driverKind,binaryRef,profileRef}` → descriptor[] / CapabilitySnapshot；不执行模型探针 |
| `workspace.register` | Hub→Node | `{workspaceId,rootPath,label,writePolicy}` → Command；canonicalRoot 由 Node 解析 |
| `workspace.get` / `workspace.list` | Hub→Node | `{workspaceId}` / `{hostId,cursor?,limit}` → Workspace / page |
| `worktree.create` | Hub→Node | `{worktreeId,parentWorkspaceId,baseOid,branch,path?}` → Command；新 Workspace ID 在接纳前预分配 |
| `worktree.remove` | Hub→Node | `{worktreeId,expectedHeadOid,expectedDirty:false}` → Command；没有 force 隐式回退 |
| `instance.create` | Hub→Node | `{instanceId,spec,initialInput?:DriverInput}` → Command + `{instanceId,prepared:boolean,sendCommandId:Id\|null,runId:Id\|null}`；initialInput 只能 prompt；agy deferred-argv 可 prepared=true 而尚未启动 |
| `instance.attach` | Hub→Node | `{instanceId,ref:AttachRef}` → Command；不得 wake/resume |
| `instance.open_terminal` | Hub→Node | `{instanceId,backgroundJobId,allowWake:true,carrier:{backend:"herdr",server:HerdrServer,session}}` → Command；只允许经认证的人类显式动作，可能 wake native job。使用同一 Command wrapper、digest、expected generation/fence 与持久 dispatch intent；不创建 Run、不含 prompt |
| `instance.resume` | Hub→Node | `{instanceId,nativeRef,providerProfileRevision,expectedPreviousGeneration}` → Command；不含 prompt，后续输入另发 |
| `instance.send` | Hub→Node | `{instanceId,runId,input:prompt\|steer,completionScope}` → Command；input 使用 DriverInput 对应分支，model-switch 只能经 configure；steer 必须指定既有 runId，不创建新 Run。prompt 分支的 `mode` 三选一（§3.1）：`new-turn` 开新回合、`steer` 插入当前回合、`queue` 明确排队。三者能力分别由 capability `steer` / `queue` 上报，未实测时为 `unknown`，调用返回 `CAPABILITY_UNKNOWN` 而不是假装不支持 |
| `instance.configure` | Hub→Node | `{instanceId,modelId,effective:"next-turn",effort?}` → Command；仅能力支持且无活 foreground Run 时，转 Driver.send(model-switch)。`effort` 用 §4.1 的 `{name, ultracode}`；历史档名按名字归一后再存，Hub 不保存两种拼法 |
| `instance.fork` | Hub→Node | `{sourceInstanceId,newInstanceId,nativeBoundary:{type:"latest-terminal"},newSpec}` → Command；expected 校验 source instance revision；原生 fork 返回新 native ID，不支持任意历史切点 |
| `instance.cancel` | Hub→Node | `{instanceId,runId}` → Command；expected 中必须含 processGeneration/runGeneration |
| `instance.close` | Hub→Node | `{instanceId,mode:"terminate",retainNativeSession:true}` → Command；不接受清除 transcript 的隐含请求 |
| `instance.get` / `instance.list` | Hub→Node | `{instanceId}` / `{workspaceId?,cursor?,limit}` → Instance / page |
| `command.get` / `command.list` | Client/Hub | `{commandId}` / `{instanceId?,resolution?,cursor?,limit}` → Command / page；先查这两个方法处理超时 |
| `run.get` / `run.list` | Client/Hub | `{runId}` / `{instanceId,cursor?,limit}` → Run / page |
| `run.wait` | Client/Hub | `{runId,condition:"terminal"\|"interaction"\|"observed-update",afterSeq?,timeoutMs}` → `{reason:condition-met\|timeout\|unknown,run,pendingInteractionIds,asOfSeq}`；超时不取消 |
| `workflow.wait` | Client/Hub | `{instanceId,workflowId,afterSeq?,timeoutMs}` → `{reason,workflow,asOfSeq}`；只结算该 workflow，不宣布父 task 完成 |
| `interaction.list` / `interaction.get` | Client/Hub | `{instanceId,state?}` / `{interactionId}` → Interaction[] / Interaction，答案按 actor 权限过滤 |
| `interaction.respond` | Hub→Node | §6.1 除 commandId 外字段，包括 answer；→ Command 与最新 Interaction |
| `events.subscribe` | Hub→Node；Client→Hub | §7.3；建立 snapshot + follow |
| `events.read` | 同上 | `{journalId,afterSeq?,beforeSeq?,limit,cursor?}` → `{events,nextCursor,floorSeq,durableSeq}`；after/before 不同时传 |
| `events.ack` | 同上 | `{subscriptionId,journalId,throughSeq}` → `{acknowledgedSeq}`；只能 ACK 连续已持久应用的前缀 |
| `events.unsubscribe` | 同上 | `{subscriptionId}` → `{}`；不停止 Instance |
| `reconcile.instance` | Hub→Node | `{instanceId,expectedGeneration,commandIds:Id[]}` → `{state,evidenceEventIds,unresolvedCommandIds}`；只读原生状态，不自动重发 |
| `tty.attach` / `tty.detach` | Hub→Node | §7.4；控制读取与写 lease，不等于 Driver.attach/resume |
| `tty.write` / `tty.resize` | Hub→Node | §7.4；write 是 Command，resize 是有 revision 的幂等设置 |
| `object.stat` / `object.read` | Client/Hub→Node | `{objectId}` / `{objectId,offset,length,expectedDigest}` → metadata / 受限 chunk stream；不接受任意绝对路径 |
| `object.prepare` | Client/Hub→Node | `{objectId,hostId,workspaceId,mediaType,sizeBytes:U64,digest:Digest,purpose:input\|settings\|answer}` → `{uploadId:Id,nextOffset:U64,expiresAt,maxChunkBytes}`；同 id+metadata 幂等，不触发执行 |
| `object.write` / `object.commit` | Client/Hub→Node | write `{uploadId,offset:U64,dataBase64,chunkDigest}` → `{nextOffset}`；commit `{uploadId,digest}` → `{objectId,sizeBytes,digest}`；只接连续字节，重复 offset+相同 digest 返回原结果，冲突拒绝；完整 size/digest 校验后才可被命令引用 |

Host、provider registry 和 Hub inbox 由 Hub 写；Workspace 与已接管的 Instance/Run/Interaction/Command 执行账本由 Node 写。Command 在 Hub 入队时标 `authority=hub-inbox`；Hub 首次转发前持久化 `forwardIntent`，并冻结此次 hubRevision 及 canonical 内容。Node 按 commandId+digest 幂等接收，以 hubRevision+1 原子接管为 `authority=node-ledger`，此后只由 Node 增加 revision。Hub 只能镜像，不擅自终结 node-ledger command；转发结果不明时 Hub 也不能仅凭本地过期直接标 expired，须让 Node 判定有没有派发。Hub 可显示单独的传输 stale 状态，但不能并行修改已冻结 Command。这是状态的单写交接，不是两个服务器同时修改同一 revision。

object 上传授权绑定 actor、host/workspace 和用途；uploadId 使用 obj_。暂存对象默认不可读/不可执行，过期后按配置 GC。超额返回 RESOURCE_LIMIT，offset/digest 冲突返回 OBJECT_REVISION_MISMATCH，未 commit 引用返回 OBJECT_NOT_FOUND。settings/answer 内容不以普通聊天附件权限暴露；object.commit 只证明字节已完整保存，不代表应用配置或提交 Interaction。

### 7.3 Snapshot + seq follow 与重连补页

订阅输入：`{journalId,afterSeq:U64|null,snapshot:"required"|"if-needed"|"none",projectionVersion:string,batchLimit:number}`。result：`{subscriptionId,journalId,floorSeq,durableSeq,snapshot:Snapshot|null,replayFromSeq,nextCursor,connectionId}`。`Snapshot` 包含 `{projectionVersion,projectionEpoch:Id,asOfSeq,instance,runs,commands,pendingInteractions,nodes,history:{earliestRetainedSeq,complete:boolean}}`；registry snapshot 使用其对应实体集合，不能给它虚构 Instance。

Node 在同一可重复读快照中取得 asOfSeq 和投影，同时先注册 follow 缓冲；返回 snapshot 后按 seq 发出所有大于 asOfSeq 的事件。数据写入、snapshot watermark 与 pending Interaction 必须来自同一已持久版本。不能先读第一页、随后才订阅，从而丢掉两者间的更新。历史旧页以 beforeSeq/cursor 读取，向前 prepend 不能改变 node 稳定 ID。借鉴 DSH 的 snapshot/follow

事件 notification：

~~~typescript
type EventsBatch = {
  jsonrpc: "2.0";
  method: "events.batch";
  params: {
    subscriptionId: Id; journalId: Id;
    fromSeq: U64; toSeq: U64;
    events: [Observation | RegistryEvent, ...(Observation | RegistryEvent)[]];
    durableSeq: U64;
  };
};
~~~

batch 的非空事件数组必须完整覆盖 fromSeq..toSeq，首尾 seq 精确匹配且 toSeq≤durableSeq；心跳另发，不用空 batch 伪造进度。v1 每个 journal 订阅不做 server-side kind 过滤，避免消费者误把过滤掉的 seq 当丢失；UI 自行筛选。所有事件均带稳定 eventId；重连可重复送达，consumer 按 `(journalId,seq,eventId)` 去重，同 seq 不同 eventId/digest 视为 `JOURNAL_DIVERGED`，不取后到者覆盖。

客户端只 ACK 已连续写入本地 projection 的最高 seq。收到 41 后直接收到 43 时缓存 43、用 events.read 补 42，不能把 asOfSeq 前推到 43；RPC response 先后与 notification 顺序无关，以 seq 为准。Node→Hub 的 ACK 只有 Hub 持久镜像之后发出，不因浏览器已显示而删除 Node 数据。Hub→Client 的 ACK 是设备消费进度，不能代替 Hub 的磁盘持久化确认。

每次断线保存每个 journal 的 cursor 向量，不存在跨主机单一 native 时间轴。重连先 hello，再查 command.get/reconcile unresolved intents，接着以最后 durable seq 补页；这一过程不重发 instance.send/interaction.respond 到 native。继续持有同一 Node/native 连接时不重置 Interaction 的 requestVersion。Hub 离线时积累的 Node events 原样镜像，observedAt 保持原值。

cursor 是服务器签名的 opaque token，绑定 principal、journal、方向和数据版本，客户端不可当文件偏移。cursor 低于保留 floor 时返回 `CURSOR_EXPIRED` 和 floorSeq，客户端请求新 snapshot、更换 projectionEpoch 并显示历史缺口；不能悄悄从最新处续接假装没有丢失。`events.read` 的 limit 默认 128，上限由 hello 给出；最大等待/buffer 是配置，不是睡眠轮询。缓冲满时终止该订阅并给出 lastCommittedSeq，让消费者补页，不能 silent drop。

原始 journal append-only。adapter 升级不重写历史 eventId/seq，不将同一 raw bytes 再追加成“刚刚发生”的重复事实；新增 parser 的历史重算属于新的 projectionVersion/projectionEpoch，保留旧 parser version 与 rawRef。未知 native observation 可以继续展示 raw，但受影响的控制/终态能力降为 unknown；未知 runtime schema major 则拒绝读取，不能降级解析状态机。

### 7.4 多路复用与 TTY

一个 Hub↔Node WSS 连接同时承载：JSON-RPC 控制帧、按 subscriptionId 分流的多个 instance journal、多个 tty/object binary streams。TTY 输出只路由给被授权的 stream subscriber；stdio/app-server/ACP 内容绝不写到 TTY channel。控制与审批优先级高于事件，再高于大对象/TTY replay；各 channel 有独立有界队列和窗口，避免一台设备的慢终端阻塞全部审批。

协商 `tty-binary-v1` 后，二进制帧采用 32-byte header：byte 0 为 framingVersion=1；byte 1 为 channelType（1=TTY output，2=object chunk，3=TTY input）；bytes 2–3 保留且必须为 0；bytes 4–19 为 stream UUID 的 16 bytes（不含前缀）；bytes 20–27 为大端 uint64 byte offset；bytes 28–31 为大端 uint32 payloadLength；之后恰有 payloadLength 原始 bytes。streamId 对应 tty.attach/object.read 返回的 ID；kind/type/length/offset 不合法即拒绝该流，不能把未注册 stream 当另一个 Instance 的输入。channel 3 的 payload 是 xterm.js（或等价客户端）发出的原字节，含键盘与鼠标转义，服务端不得过滤。Web follow 合同见 [remote-terminal.md](remote-terminal.md)。

二进制 header 本身不携带 instanceId 或 hostId，因此接收端**必须**用已注册的 stream→instance 映射决定归属，不能用发送方的 host 身份代替 instance 身份。Hub 侧唯一的注册点是通过 host 绑定校验的 `tty.frame` 控制帧（`ws.rs` `handle_node_method`）。注册表 `TtyRelay` 是进程级的，所以每个 node socket 另外只认自己绑定过的 stream UUID：来自 A 主机的二进制帧即使命中 B 主机注册的 stream 也一律丢弃，未注册 stream 同样丢弃，都不发布到任何 follow 订阅。per-socket 集合有条目上限，超过即整体回收，避免一个 Node 无限增长 Hub 内存。

`tty.attach` 只订阅已经存在的 bridge，不能创建 `claude attach` pane；payload 中不存在 allowWake、job ID 或 launch 参数，额外字段拒绝。刷新、重连与 reconciliation 禁止推导 open_terminal。浏览器 `GET /v1/follow?instanceId=&tty=1` 触发 Hub→Node `tty.attach`。`tty.attach` 参数 `{instanceId,processGeneration,mode:read|write,previousStreamId:Id|null,afterOffset:U64|null}`；返回 `{streamId,streamEpoch,representation:pty-bytes|rendered-ansi,nextOffset,availableFrom,screenSnapshotRef:Id|null,snapshotAtOffset:Knowledge<U64>,writerLease:{leaseId:Id,expiresAt:Timestamp,inputNextSeq:U64}|null,snapshotBase64?:string}`。`snapshotBase64` 是 attach 时刻的全屏 ANSI 或最近 ≤256 KiB 回放，Hub 必须先把它写成 channel-1 二进制帧发给该 follower，再转发 live `tty.frame`。streamId 与 epoch 必须同时关联，每次重建原生 terminal bridge 都分配新 streamId/streamEpoch，不复用旧 offset 空间；只有原 bridge 未断时 previousStreamId/afterOffset 可恢复。read attach 没有输入权；write lease 同一 Instance 同时只有一个，设备失联后到期释放。其他设备继续读。显式接管产生审计事件，并使旧 lease 的写入返回 `TTY_LEASE_LOST`。关闭终端面板只 detach，不 close 原生进程。写权限限于可对该 Instance `POST …/commands` 的设备；单帧输入不超过 `maxTtyInputBytes`。

`tty.write` 参数 `{commandId,instanceId,processGeneration,streamId,streamEpoch,writerLeaseId,inputSeq:U64,dataBase64:string,keys?:string[]}`；Node 把 `dataBase64` 原字节写入 PTY，或把逻辑键（`enter`=`\\r`、`esc`=`\\x1b`、`ctrl+c`=`\\x03` 等）映射成字节后写入，返回 scope=tty-bytes。Web 终端优先走 channel-3 二进制，不经 keys。inputSeq 在该 lease 下严格单调。相同 commandId/inputSeq 的重试只查记录；已可能写入但 ACK 丢失时返回 unknown，不重复送 Enter、Ctrl+C 或粘贴。响应超时不自动重传按键。`tty.resize` 参数 `{instanceId,streamId,writerLeaseId,resizeRevision,cols,rows}` 是状态设置；follow 套接字上的简写是 `{type:"tty.resize",cols,rows}`。较旧 revision 忽略并返回实际尺寸，新设置可安全幂等重发。

输出 offset 按字节而非字符计数，UTF-8/ANSI 可以跨帧，终端客户端保留增量解析状态。断线从 afterOffset 补该 stream 实际保存的 bytes；新终端需要从有效终端状态快照的 snapshotAtOffset 开始，或从 stream 开头重放。前缀已 GC 且无可恢复终端快照时明确 `TTY_HISTORY_GAP`，不把普通屏幕截图当 xterm serialize state。

representation=rendered-ansi 时，Node 把 Herdr terminal.frame 的 base64 解码后经上述 binary framing 发送，并在 raw_tty 记录 native frame seq/full/尺寸；消费者以该 stream 的字节 offset 为水位，不以 Herdr per-client seq 作为跨重连游标。新 Herdr 连接先等 full frame，建立新 stream，清空渲染器后恢复当前画面；若无法得到 full frame，返回 TTY_HISTORY_GAP。该恢复不承诺先前 scrollback/源 PTY 模式恢复。只实现 pane.read 的 backend 仍只能提供 screen-snapshot UI，不能冒充此 ANSI bridge。Herdr terminal transport 源码

TransportLimits v1 默认建议：`maxJsonFrameBytes=1048576`、`maxBinaryChunkBytes=65536`、`maxTtyInputBytes=4096`、`maxInFlightRpc=128`、`maxEventsPerBatch=256`、`maxSubscriptionBufferEvents=4096`、`heartbeatIntervalMs=15000`、`leaseTtlMs=45000`、`maxWaitMs=60000`。这些是可校验配置并在 hello 返回；客户端必须按实际值工作。大 prompt/file 先写受限 object，再在控制消息中引用；不截断成另一条有效命令。磁盘/日志不可写时停止接纳新的 native 变更，返回 `JOURNAL_UNAVAILABLE`，现有运行状态显式 unknown/受配置控制地停止；不能为了保持 UI 流畅而丢终态或授权记录。

**object.pull（Node→Hub，2026-09-17 补充 D-027a 保留情形）。** 附件字节的主路径仍是 Node 用 host token `GET /v1/objects/{id}`；但经 ssh-stdio 注册的主机只有桥接到 `/v1/node` 的那一条通道，通常无法直连 Hub HTTP，因此同一 host token 也可在 node 套接字（WSS 或 stdio NDJSON，bridge 原样转发）上发起 `object.pull`：请求 `{objectId, instanceId}`。授权 = 套接字已认证的 host **且** `objects.instance_id == params.instanceId` 且该实例属于该 host；跨主机或谎报实例一律 `Forbidden`，未知/过期对象 `NotFound`，并按 Hub `attachmentMaxBytes` 做字节上限检查（r-files 默认 25 MiB）。≤768 KiB 的对象（base64 后 ≤ `maxJsonFrameBytes` 并预留信封余量）在最终回复中一次性返回 `{objectId, mime, size, sha256, dataBase64}`；更大对象先按 seq=0..=last 发送若干 `object.chunk` 通知 `{objectId, seq, dataBase64, last}`（每块原始 256 KiB，约 345 KiB base64），再发只含元数据的最终回复；所有 chunk 与回复走同一 FIFO 出站队列且块间 yield，避免大对象饿死 tty/控制帧。Node 端必须校验 size、重组成连续 `0..=last` 区间并复核 sha256，单对象同一时刻只允许一个在途 pull，带超时并对传输类失败重试一次。**为什么不用 channel 2 `BinaryChannel::ObjectChunk`**：ssh-stdio 桥（`remuda ssh node` 的 stdin/stdout NDJSON，见 `remuda-ssh/src/enroll.rs bridge_until_close`）只能逐帧转发 JSON 文本，不能承载 32 字节二进制信封；为桥另外引入二进制隧道会改变 ssh-stdio 的全部安全边界，而 JSON chunk 在两端都复用现有 JSON-RPC 路径、同样的 1 MiB 帧上限和同样的 sha256 完整性校验。channel 2 保留给未来允许二进制的单一 WSS 部署作优化，届时为加性升级，materialize 接口不变。

### 7.5 最小故障恢复算法

1. Node 启动取得单实例锁，新建 nodeEpoch，打开 durable command/interaction/journal。记录所有有 dispatch intent 但未结算的命令，不立刻执行它们。
2. 向 supervisor 查询 PID birth、进程树/PTY owner；读取明确的 native session/thread/status，不使用 PID 数字或“名字一样”认领进程。
3. 原生仍活且可控：重新 attach，取得新的 native connectionEpoch；按原生可验证的 pending/request replay 对账。无法恢复 stdio 控制的活进程只可观察，不能创建替身并重复工作。
4. 原生已停：读取原生 history/terminal evidence。能确定结果则补充 accepted/settled；不能确定则保持 unknown。需要恢复执行时由新的明确 resume Command 增加 generation，旧 Interaction 全部失效。
5. 对已经写过/可能写过的 prompt、approval、TTY input 只查询，禁止 replay。即使同一个原生 requestId 再出现，也只有 native 协议明示幂等且同 request 仍待处理时才能重送同一已提交答案；v1 无该证明默认不重送。
6. Node 与 Hub 按 journal seq 补齐副本；每个实例单独标 recovered/reconciling。一个不可恢复的 optional driver 不阻断其它 Instance 的事件与 Claude 主路径。

### 7.6 `api.*` 带内模型 API 代理流（D-047 / D-048，2026-09-19）

设计全文见 [api-routing.md](./api-routing.md)。

`delivery = via:<H>` 的会话把模型 API 请求交给 H 出去（D-047）。W 的 Node 在
每实例 loopback 监听器上收到请求后，把它变成这里的七个帧之一，走**既有**
Hub↔Node 链路——与 `object.pull` 同一条路径、同一套授权、同一个 1 MiB 帧上限，
理由见 §7.4（ssh-stdio 桥只转发整帧 JSON）。

**全部七个帧都是 notification**，没有 `id`，**永不进入** §7.4 那个 32 槽的
在途 RPC 上限：每条链路维护自己的 stream 注册表，否则几条热流就会卡住
`instance.create` 和 `tty.write`。carrier 必须按 `HubNodeMethod::is_api()`
把它们送进 stream 注册表，而不是 JSON-RPC 派发表。

| 帧 | 方向 | params |
| --- | --- | --- |
| `api.open` | W Node→Hub，Hub→H Node | `{instanceId, streamId, method, path, query, headers:[{name,value}], bodyBase64?, bodyChunked, deadlineMs}` |
| `api.body` | W Node→Hub→H Node | `{streamId, seq, dataBase64, last}` |
| `api.head` | H→Hub→W Node | `{streamId, status, headers:[{name,value}]}` |
| `api.chunk` | H→Hub→W Node | `{streamId, seq, dataBase64, last}` |
| `api.end` | 双向 | `{streamId, error?:{code,message}, bytesUp, bytesDown, ms}` |
| `api.cancel` | 双向 | `{streamId, reason}` |
| `api.credit` | 消费者→生产者 | `{streamId, chunks}` |

`headers` 是**列表**而不是 map：HTTP 允许同名重复（`x-stainless-*` 就会），
折叠成一个值会改变网关看到的请求。`query` 原样转发、从不重新编码；
`path` 是 profile base path **之下**的路径，不含查询串。

**授权是两重的。** W 的 Node 先在本地校验（loopback 对端、常量时间比较 bearer、
实例存活、方法属 `{POST, GET}`、路径在 base path 下），Hub 再对着实例与该会话
已决议的路由校验一遍——和 `object.pull` 一样，Node 的说法不足以放行，而凭据
只在 H 侧加载。目标 origin 固定为 `profile.baseUrl` 的 origin，请求/响应头
各有白名单（`set-cookie` 丢弃）。**拒绝码**：`api-via-unknown-host`（400）、
`api-via-host-offline`（409）、`api-via-unsupported`（409）、
`api-via-unreachable`（409）；`api.end.error.code` 用稳定小写码
（`via-host-offline`、`hub-link-lost`、`upstream-timeout`、`upstream-failed`、`instance-gone`、
`destination-refused`、`cancelled`），监听器把每个映射成 Anthropic 形状的
HTTP 错误，让 harness 渲染出真正的模型 API 失败而不是传输故障。
`upstream-timeout`（504）表示网关不可达或超时阶梯被突破；
`upstream-failed`（502）表示请求已到达网络但因不可归类的上游故障失败
（连接后重置、上游 body 失败），两条中继腿（带内与直连）使用同一映射。

**信用与限额。** Hub→Node 出站队列容量 32 且与 tty 帧共享，因此生产者每流最多
4 个未确认 chunk，消费者边排空边发 `api.credit`，块间 `yield_now()`。
`TransportLimits` 新增 `maxApiStreams`（默认 8/链路、2/实例）与 `apiChunkBytes`
（默认 64 KiB 原始 ≈ 87 KiB base64）。两者在读侧都有默认值，所以 D-048 之前的
`hello.limits`（九键）仍然解析。

**合并与超时。** 每个 token 一帧在 NDJSON/ssh-stdio 上是病态的，所以 H 在
**≥16 KiB 或 ≥50 ms 或流结束** 时合并成一块，严格保序——body 是不透明字节，
不按 SSE 解析。超时阶梯：连接 10 s、首字节 60 s、块间空闲 120 s、硬上限
30 min；越界即向上游 `api.cancel`、向下游 `api.end{error}`，监听器答 `504`。

**为什么不用 channel 2 `BinaryChannel::ObjectChunk`**：与 §7.4 中 `object.pull`
的理由完全相同——ssh-stdio 桥只逐帧转发 JSON 文本，引入二进制隧道会改变桥的
全部安全边界。

**这不是隧道（D-031）。** 不装不探隧道二进制、不用 `ssh -L/-R/-D`、默认不在
非 loopback 接口开端口（只有操作员显式配置 `Host.relayBind` 才会）、到不了
任意主机。被转发的是一个单一 origin、白名单、实例作用域的应用请求，字节走
已经授权、已经审计的链路。


## 8. 主 agent 的 MCP / CLI 控制面

### 8.1 工具定义

Node 提供本机受控 MCP server `runtime`，由 Claude 的原生 MCP 配置加载；MCP transport 使用 stdio 或本机受权限保护的 socket adapter。它与主 Claude 的生命周期独立，不给模型 Hub root token。每个主 Instance 获得绑定 `principalId,instanceId,allowedHosts,allowedWorkspaces,allowedProfiles,allowedDrivers,maxChildren,maxDepth,expiresAt` 的 capability。child 的 parent 关系、actor 与授权从这份 capability 推导，不信任模型传来的 parent/owner。

| MCP tool | 输入 schema（额外字段拒绝） | 返回 |
| --- | --- | --- |
| `runtime_instance_create` | `{commandId,hostId,workspaceId,driver,providerProfileId,providerProfileRevision,modelId?,worktreeMode:inherit\|isolated,permissionPresetId,completionScope:native-turn\|task,initialInput?:ContentBlock[]}` | `{instanceId,createCommandId,sendCommandId:null\|Id,runId:null\|Id,commandState,capabilities,asOfSeq}` |
| `runtime_instance_send` | `{commandId,instanceId,expectedGeneration,runId?,mode:new-turn\|steer,input:ContentBlock[],completionScope:native-turn\|task}` | `{commandId,runId,state,resolution,asOfSeq}`；steer 要求 runId 与已知 native turn |
| `runtime_instance_wait` | `{instanceId,runId?,workflowId?,condition:run-terminal\|workflow-terminal\|interaction\|observed-update,afterSeq?,timeoutMs}` | `{reason:condition-met\|timeout\|unknown,run:null\|Run,workflow:null\|Json,pendingInteractions,asOfSeq,outstandingWork}`；run/workflow selector 按 condition 必填且互斥 |
| `runtime_instance_read` | `{instanceId,runId?,afterSeq?,beforeSeq?,limit?,include:messages\|tools\|workflow\|summary}` | `{observations,result,completeness,asOfSeq,nextCursor,truncated}`；不给模型未授权的 raw/secret |
| `runtime_instance_stop` | `{commandId,instanceId,expectedGeneration,scope:run\|instance,runId?}` | scope=run 调 cancel，scope=instance 调 close；返回 Command，不伪装完成 |

MCP 输出是 runtime 的已观测事实，原始 agent 文本标 source 和 completeness；不把 child 的文本当命令执行。`instance_create` 只能引用已登记的 profile/permission preset，不能让模型传任意 executable、env secret、hooks 脚本或解除权限。默认 child 不拥有批准自己/同级原生审批的权限；需要特定自动化批准时必须是单独明确授权的机器 policy，不将“主 agent 已决定”混成人类批准。

对应非交互 CLI 是同一 API 的薄客户端，不提供另一份队列或 loop：

~~~text
<runtime> instance create --spec <private-spec-file> --command-id <cmd-id> --json
<runtime> instance send <instance-id> --input-file <file> --command-id <cmd-id> --completion-scope native-turn --json
<runtime> instance wait <instance-id> --run-id <run-id> --condition run-terminal --timeout-ms 30000 --json
<runtime> instance read <instance-id> --after-seq 40 --limit 100 --json
<runtime> instance stop <instance-id> --scope run --run-id <run-id> --command-id <cmd-id> --json
~~~

CLI exit 0 只表示 API 请求成功返回；返回对象的 commandState/resolution/run.state 才是工作进度。非零错误也不意味着命令一定未发送。stderr 只放非敏感诊断，stdout 固定单个 JSON；大对象以 ref/cursor 返回。`--wait` 类工具默认一次最多 30 秒，超过上限拒绝/按已公布上限返回，不通过重新发送任务来等待。HTTP/WS 断线后先 command.get，以原 commandId 取结果。

主 agent 的 tool invocation→commandId 关系要持久保存：同一次 MCP 请求重试返回原 commandId；跨模型回合再次调用 create/send 是一个新的明确请求，不能自行猜它是重试。如果 SDK/MCP transport 没有可复用的调用 ID，要求工具输入 commandId，并在工具说明中要求保留；不宣称跨不同 tool invocation 自动 exactly-once。

### 8.2 与 Workflow / Agent(Task) 的分工

| 主 agent 的目的 | 执行路径 | runtime 的职责 |
| --- | --- | --- |
| 同 settings / 同 endpoint 下混用模型 | Claude 原生 Workflow `agent(prompt,{model})` | 观察 task/journal/member，显示请求与解析模型；不替换为其它 agent loop |
| 小型原生 subagent / Task 功能 | Claude Agent/Task 原生工具，遵守其 model 参数限制 | 观察 parent_tool_use_id/agent_id；不把任意 gateway model 注入 Task |
| 另一 endpoint/key/settings、独立权限或生命周期 | `runtime_instance_create` 启动另一 Claude Instance | 单独 materialize profile、native session、权限、writer lease；返回 child IDs |
| 需要 Codex/Grok/agy 特有工具 | 相应可选 driver 的 runtime Instance | 其能力失败不换主 Claude loop；不伪装 Claude Workflow |
| 人要看 ultracode/TUI `/workflows`/Artifact | claude-pty 原生通道 + 结构化 side panel | 保存真实原生体验；print 的缺口不以同名 UI 组件冒充补齐 |

主 Claude 调 child 的完整顺序是 create（只证明进程/会话接纳）→ send（持久 commandId）→ wait/read（取 Run 与 Workflow 的真实状态）→ 将返回内容作为该 MCP 工具结果交回**原生 Claude**→ 显式 stop 或按已设保留策略闲置。runtime 不直接写 parent transcript，不把 child output 合成新的 assistant 消息，也不自动要求 parent 再跑一轮推理。需要父模型继续思考由原生 MCP tool 返回/原生通知机制驱动。

同 native store/session 的复用不是 child isolation；需要并行编辑时默认独立 worktree。深度/并发限制在 create 接纳前执行；达到限制返回 `RESOURCE_LIMIT`，不偷偷改用父工作目录串起隐藏任务。每个主/子 Instance 可有不同 ProviderSelection，gateway model display alias、wire model、native capability 三者分别记录。任务已确认的 Workflow/Agent 区分、[proposal §4.2](proposal.md#42-claude-调-claude--混合模型的实现路径)

### 8.3 Bot dispatcher 对同一协议的使用

Bot 入口以 platform delivery/message ID 在 Hub durable inbox 去重，身份验证后才生成 commandId；项目路由输出具体 host/workspace/profile/driver，不把聊天平台文本直接变成任意 shell command。Bot 的“已接收”对应 queued，“原生已接纳”对应 accepted，“本轮/任务结束”对应所选 Run scope。审批卡片携带 interactionId/version/generation，回调仍由 Node CAS；消息重复投递、卡片重复点击或 Hub 重启都不会绕开同一账本。推送/邮件等外部发送只由明确配置的 dispatcher 通道执行，native output 中出现“通知某人”不会自动成为 runtime 的发送指令。

## 9. 错误码、未知值与兼容性

### 9.1 错误对象

~~~typescript
type RuntimeError = {
  code: string;
  rpcCode: number;
  message: string;
  retry: "never"|"read-only"|"same-command-query"|"after-reconciliation"|"new-command";
  execution: "not-dispatched"|"possibly-dispatched"|"accepted"|"settled"|"not-applicable";
  details: {
    commandId?: Id; instanceId?: Id; interactionId?: Id;
    expectedGeneration?: U64; actualGeneration?: U64;
    evidenceEventIds?: Id[]; nativeErrorRef?: Id; retryAfterMs?: number;
  };
};
~~~

JSON-RPC response.error 使用 `{code:rpcCode,message,data:{code,retry,execution,details}}`，消息文本不能作为业务判别键。错误中的 stdout、provider body、env、Authorization header 必须经过敏感字段过滤，只可用 nativeErrorRef 引用受控原始数据。原生错误保留其 namespace/code，不能把 Codex -32600 当成本 runtime 所有“请求未执行”的证明。

| code | rpcCode | 默认处理 |
| --- | --- | --- |
| `UNAUTHENTICATED` | -32000 | never；重新认证前不执行 |
| `SCOPE_DENIED` | -32001 | never；保留拒绝审计 |
| `HOST_OFFLINE` | -32002 | same-command-query；可显示 Hub queued，不声称 Node 已接收 |
| `OWNER_FENCED` | -32003 | after-reconciliation；旧 owner 禁止写 |
| `PROTOCOL_VERSION_UNSUPPORTED` | -32004 | never；协商失败关闭连接 |
| `SCHEMA_VERSION_UNSUPPORTED` | -32005 | never；未知 runtime journal major 不降级读取 |
| `CAPABILITY_UNSUPPORTED` | -32006 | never；该 driver 不提供 |
| `CAPABILITY_UNKNOWN` | -32007 | never；需要版本/profile 验证，不能自动尝试 |
| `NATIVE_FEATURE_DISABLED` | -32008 | never；违反 Claude 原生功能保留要求 |
| `BINARY_CHANGED` | -32009 | never；digest 与声明不符，重新审查/配置 |
| `INVALID_LAUNCH_SPEC` | -32010 | new-command；字段/args/权限 variant 不合法，未派发 |
| `SETTINGS_ISOLATION_UNAVAILABLE` | -32011 | never；没有可用的独立持久配置入口 |
| `PROVIDER_PROTOCOL_MISMATCH` | -32012 | new-command；ingress 与 driver 不兼容 |
| `PROVIDER_UNAVAILABLE` | -32013 | same-command-query 或 new-command；仅未派发时可重选 |
| `CREDENTIAL_UNAVAILABLE` | -32014 | never；引用/版本不可解析，不输出值 |
| `WORKSPACE_NOT_FOUND` | -32015 | new-command；主机路径未登记或不存在 |
| `WORKSPACE_BUSY` | -32016 | new-command；writer lease 冲突 |
| `WORKTREE_BUSY` | -32017 | never；不能清理活 worktree |
| `WORKTREE_DIRTY` | -32018 | never；不能自动删除修改 |
| `NATIVE_SESSION_NOT_FOUND` | -32019 | never；不回退新建 |
| `NATIVE_SESSION_OWNED` | -32020 | after-reconciliation；已有活 writer，不抢占 |
| `NATIVE_GENERATION_MISMATCH` | -32021 | same-command-query；拒绝旧实例输入/答案 |
| `ATTACH_WOULD_WAKE` | -32022 | never；attach 不执行 resume |
| `CONTROL_UNAVAILABLE` | -32023 | after-reconciliation；可能只可观察/原生 TUI |
| `COMMAND_ID_CONFLICT` | -32024 | never；同 ID 不同 payload |
| `COMMAND_EXPIRED` | -32025 | new-command；仅确定未派发才 settled expired |
| `COMMAND_OUTCOME_UNKNOWN` | -32026 | same-command-query；不重投 native |
| `RUN_NOT_ACTIVE` | -32027 | same-command-query；steer/cancel 查询实际终态 |
| `INTERACTION_ALREADY_ANSWERED` | -32028 | same-command-query；返回已提交答案的可读状态 |
| `INTERACTION_STALE` | -32029 | read-only；刷新 requestVersion，不把旧答案应用新请求 |
| `INTERACTION_EXPIRED` | -32030 | never；不复活 |
| `INTERACTION_SCHEMA_UNSUPPORTED` | -32031 | never；保留原生提示/拒绝，不能编答案 |
| `INTERACTION_NOT_ANSWERABLE` | -32032 | never；需要原生 TUI 或无支持的 responder |
| `INVALID_ANSWER` | -32033 | new-command；内容/digest/options 不匹配，未写原生 |
| `NATIVE_RESPONSE_UNKNOWN` | -32034 | after-reconciliation；已可能写回，禁止再次写 |
| `CURSOR_EXPIRED` | -32035 | read-only；新 snapshot + 明示 gap |
| `JOURNAL_GAP` | -32036 | read-only；补页后再前推水位 |
| `JOURNAL_DIVERGED` | -32037 | never；同 seq 冲突，停止投影/控制结算 |
| `JOURNAL_UNAVAILABLE` | -32038 | after-reconciliation；停止接纳新副作用 |
| `STATE_UNKNOWN` | -32039 | read-only；不填默认值 |
| `RESOURCE_LIMIT` | -32040 | new-command；返回已配置 limit，不静默缩减任务 |
| `TTY_LEASE_LOST` | -32041 | never；旧 writer 不能继续发键 |
| `TTY_HISTORY_GAP` | -32042 | read-only；恢复有效 snapshot 或明确重置 |
| `OBJECT_NOT_FOUND` | -32043 | read-only；可能已按策略 GC，不能返回另一文件 |
| `OBJECT_REVISION_MISMATCH` | -32044 | read-only；重新读取当前 metadata |
| `NATIVE_PROTOCOL_ERROR` | -32045 | after-reconciliation；未知控制/终态，不自动 success |
| `WAIT_TIMEOUT` | -32046 | read-only；超时本身不取消、不重试任务；run.wait 通常用 reason=timeout 正常返回 |

普通 JSON-RPC parse/invalid request/method not found/invalid params/internal error 分别使用 -32700/-32600/-32601/-32602/-32603。内部错误只说明本次 RPC 失败，execution 必须如实填 possibly-dispatched 等，不能默认 not-dispatched。`retry` 描述的是下一步客户端动作，不是许可 native retry；same-command-query 表示先读取/重取同一命令结果。

### 9.2 Unknown 与 reconciliation 的实现要求

Unknown reason 使用稳定小写代码，例如 `not-emitted`、`unsupported-source-version`、`native-ack-missing`、`control-channel-lost`、`native-session-missing`、`incomplete-transcript`、`ambiguous-run-correlation`、`unverified-task-boundary`、`source-conflict`。这些 reason 可扩展展示，但不能当新的成功状态。null 与 unknown 的区别在所有 SDK 保持；UI 显示未知，不渲染成空文本、0 token、0 cost 或“未运行”。

reconciliation 是有输入输出的只读对账动作：输入 instance generation + unresolved command IDs；读取 native session/thread/process/pending 请求；输出逐个命令的证据和仍未知项。它不包含自动 resume、自动 prompt、自动 approve、自动轮 provider、自动删除文件。需要继续执行时是下一条有 commandId 的显式命令，并记录它与前一未知命令的关系。若尚不确定原生命令是否运行，禁止以新的 commandId 规避去重再试一次。

进程出现新的未知 schema/control type 时，adapter 保留 raw，受影响能力降为 unknown、发 lifecycle diagnostic；无关的已知消息仍可显示。必须确认的控制消息不能被当 ignorable；只有已声明 purely observational 的新字段可忽略。恢复后生成新的 capability snapshot，与旧 Run 当时的快照共存，不能事后修改旧快照来声称当时已经支持。

## 10. 待决策与未验证项目

以下问题不在终端询问，也不阻塞本文交付；每项有当前默认行为与进入实现前的验收条件。它们不表示已执行了探针或已获准修改当前用户配置。

| 项目 | 本规格默认决定 | 待验证/决策与通过条件 |
| --- | --- | --- |
| 项目名与后端语言 | wire 使用 runtime，语言无关的 JSON/字段表；沿 proposal 的 Go Hub/Node 倾向 | 命名不进入 native session ID；选 Go/TS 不改变协议 |
| Herdr vs 自有 PTY | 已安装 Herdr 的 Node 优先用 terminal observe/control；其流标 rendered-ansi，carrier 可替换 | bridge 源码已确认，仍须针对目标版本验收 input、UTF-8/ANSI、尺寸、detach、server 重启、full redraw 和 history gap；不要承诺源 PTY 原字节 |
| Claude 持续 stdin 的整体任务结束 | 只默认支持 native-turn；task scope unknown | 原生明确 run-boundary 或足够强的官方 protocol；必须覆盖“任务先完成、首 result 后到、随后还有 continuation”的 fixture，空 ledger 不能过关 |
| Claude control 协议与完整交互 | host+stdio prompt tool+initialize 固定；allow/deny/单选已有 CLI 2.1.268 证据 | 将已有 fixture 转成 adapter 验收，再补两次并发审批、多选/自由输入、ExitPlanMode、elicitation/dialog、control_cancel_request、不中断 Workflow 的持续 stdin；探针空 sources 不证明全配置下无竞争 |
| Claude gateway + Artifact | full-native 人机通道保留 claude-pty；print 当前不能满足 artifact 要求 | 在实际 gateway profile 与 native-login profile 分别验证 Artifact，记录工具资格和事件；失败不可拿普通 HTML viewer 冒充 |
| Claude settings/home 继承 | 固定 sources + 持久 native store，独立 overlay | 两个不同 endpoint 同时运行，hooks/MCP/skills/Workflow 保留，cache 不串路由；不自动迁移/复制 OAuth |
| Claude native hooks 的远程批准 | 只在唯一 responder 和真实输出 schema 验收后启用 | 同时存在 Flux/Orca/herdr 时，证明无重复批准；hook 超时、Node 断线、无 tool_use_id 的并发调用都不能错答 |
| Codex 版本、服务进程与配置 | 明确 binary path/digest；每实例 stdio；独立 CODEX_HOME；保留 native hooks | 0.154.0 的实审批尚需验证，源 schema 不能当实测；不依赖 standalone daemon；如改 UDS 要 WS upgrade 和连接权限验证 |
| Grok ACP 细节 | loadSession/cancel 已有实测；默认不声明 fs/terminal，不共享 leader；扩展固定 `_x.ai/` | 补非 yolo permission 往返、set_config_option 正确编码、stream/replay 去重、resume 新进程行为；fork/steer 不先行开放 |
| agy 设置隔离与完整输入协议 | 单次 print、已登记 OS profile；无未知 env/flag | 证明独立 config/auth/home、stream-json input、工具/失败/审批/resume；不能由 4 行 OK 样例推断全功能 |
| Provider 轮换 | gateway 管其账号池；runtime 管 profile/endpoint，活动 Run 固定 selection | 验证选定 ingress/model/capability、401/helper 刷新、可恢复范围；有副作用的失败不重投 prompt |
| 原始数据 retention | journal/command tombstone/native session 分开配置；原始敏感内容仅授权读取 | 选择大小/时间/加密备份策略；GC 后 floor/gap 明确；保留 command 去重 tombstone 至 Instance 归档且超过重试窗口 |
| 跨主机迁移 | Host/Instance 不原地迁移；新 Instance 显式引用原生导出 | 只有 native 官方 import/resume 方法加完整 store/schema 校验才可复制恢复；observation projection 不能替代原生数据 |
| 本地人与远程多人同时输入 | 多读者、一个 runtime TTY writer；已有本地输入另记 external-input | 若 carrier 无法对所有写入方执行 lease，semantic send/approval 保持不具独占保证，不自动归因输入 |

## 11. 可直接安排的实现与验收顺序

1. **先实现协议与 durable 账本。** 实体/ID/CAS/错误 enum、Command 单写交接、Observation append 与 snapshot-follow 用同一组 fixture 验证。接受路径必须证明：同 commandId 同 payload 不重复、不同 payload 拒绝、dispatch intent 后崩溃不会重投、同 seq 冲突会停止处理、未知控制消息不能变成成功。
2. **实现 Claude 两条原生通道和 materializer。** 保留固定 sources、持久 home、原生 Workflow/MCP/skills/hooks；对三个禁用 flag 与两个等效 env 做拒绝用例。用不同 settings 的两个 Claude 实例证明不串 credential/profile/native session；分别验证 Artifact 和 print 的能力差别，不宣称互换。
3. **接通双设备观察与 Interaction。** Mac/手机同时回答同一请求，仅一个 CAS 成功；Node 在写回前后崩溃均不重答；native generation 更换后旧卡片不可用；Hub 掉线时原生任务存活，恢复只补事件。PTY输入 ACK 只显示字节交付，不能变成 task success。
4. **完成 Workflow 与原生回合的回放验收。** 用已有两条 result 的 Claude fixture、新的“后台先结束”fixture、多个 workflow member/重试/opaque phase fixture 验证；run.wait(native-turn) 可以结束，未支持的 task scope 在派发前拒绝，恢复/导入的 task Run 缺证据时返回 unknown。完整 replay、逐条 append、历史 prepend、重连重复 batch 得到同一投影。
5. **再接 Codex/Grok/agy。** Codex 用匹配的 schema/stdio fixture，分别验收 turn+item+approval/elicitation、steer/interrupt、resume usage 去重；Grok 做 ACP 能力与 permissions/cancel/resume；agy 对未知 type 明确保留 opaque。未完成的 optional driver 不降低 Claude 主路径能力。
6. **最后接主 agent MCP 与 Bot。** create/send/wait/read/stop 都只访问上述 API；证实主 Claude 能启动另一套已登记 settings 的 Claude并取回结果，原生 Workflow 仍可直接混合网关模型。Bot queued/accepted/settled 分开通知，重复 delivery/card callback 不产生第二份任务或答案。

上述顺序中的原生执行、存储与网络验收仍属于对应实现任务；protocol crate 当前的类型、schema、golden 与 binary 编解码实施状态见 §12。每个标 unknown 的能力继续返回明确错误/状态，直到对应 native 版本的验收补齐。

## 12. 实现反馈与 M0-02 状态（2026-09-12）

逐方法、逐实体、逐字段的文档↔实现核对见 [protocol-audit-1.md](protocol-audit-1.md)（2026-09-13）：wire 类型层与本文一致（46 方法、47 error code、15 类 Observation、各实体字段逐项对齐，生成产物为最新），差异集中在 Node/Hub/driver 的实现覆盖，该文末尾列出具体任务。

[协议 crate](../../crates/remuda-protocol/src/lib.rs) 是 wire 类型的单一来源；[JSON Schema](../../crates/remuda-protocol/schema/protocol.schema.json) 与 [TypeScript](../../web/src/types/generated.ts) 由 [生成器](../../crates/remuda-protocol/examples/gen_types.rs) 同次生成。`just gen-types` 只改这两个产物；`cargo run -p remuda-protocol --example gen_types -- --check` 比较字节且不改文件。现有 CI 的 `cargo test --workspace` 会运行 generated freshness 测试，Rust 类型变化而未生成、或手改产物，均使测试失败；不新增依赖 Node 的 CI 流程。计划旧表中的 `scripts/ci/check-generated.sh` 入口由此 crate 内命令实现，避免扩大本任务文件范围。

生成器使用固定版本的 [schemars](https://docs.rs/schemars/1.2.2/schemars/) 派生 Serde schema，以序列化规则保留 required nullable 与可省略字段的区别；自定义 U64/ID/时间/digest/字面量 schema 与入口解析配对测试。TypeScript 从同份 schema 输出命名实体、判别 union 与已实例化的泛型定义；未知 schema 关键字使生成失败。TypeScript 不是运行时验证器：数值范围、字符串格式、对象关系、权限与状态转移仍需 schema/Node 检查。`from_json_slice` 拒绝重复 key、非法 UTF-8 与多余 JSON 文档，网络层在解码前限制 frame 大小。

| 范围 | 状态 | 已落实内容 / 后续责任 |
| --- | --- | --- |
| 核心实体、ID、Knowledge、三维 Instance 状态 | 已实现 | Rust Serde + JSON Schema + TS；U64 是十进制字符串，实体 ID 使用不同 Rust brand；未知枚举拒绝 |
| Driver/NativeRef/carrier 增量 | 已实现 | claude-bg、独立 claudeBg.jobId、Herdr binary/server/session/pane 身份；PTY 固定 Herdr/rendered-ansi；bg first input 声明 deferred-argv 与 explicit-non-secret |
| Command/RPC/Observation | 已实现类型 | Command 三态、15 类 Observation、46 个方法、47 个 error code；open_terminal 使用显式 allowWake:true；只读 tty.attach 拒绝 launch 字段 |
| Wire golden | 已实现 | 本文每个 JSON 示例单独保存；NDJSON 按 frame 拆成独立 JSON；来源与类型分流记录在 golden 目录。原生 control/settings/hook 示例保持原始 JSON，不冒充 Remuda RpcRequest |
| TTY/object 32-byte header | 已实现 | 纯函数检查版本、channel、保留位、UUIDv7、精确长度、配置上限、offset 溢出；跨帧 UTF-8/ANSI 不在 header 层解析 |
| Schema/TS freshness | 已实现 | 生成器、只读 check 与现有 cargo CI 中的字节比较；golden 同时检验 schema 与 Serde；TS 类型编译验证属于本机检查 |
| Native process、bg argv/attach 与 Herdr bridge | 未在本任务实现 | driver/Node 必须核对人类授权、job/store/host、binary pin 和 owner fence，再持久 intent；bg 首次输入只接受显式非敏感单个 text block，bot 禁止。Herdr 重连换 streamId/streamEpoch 并等待 full frame，不能用 pane.read 补增量 |
| Journal、Command/Interaction CAS 与恢复 | 未在本任务实现 | Node/journal 核对 parent ID、重复 lifecycle 字段、digest、generation、租约与状态转移；serde/schema 可解析不等于可派发。ACK 丢失、崩溃或 unknown 不自动重放 |
| 远程审批与 bot 权限 | 未在本任务实现 | D-005 的 host broker 是目标；M0 临时 dontAsk 需记录权限债。bot 永不 bypass；CLI/模型返回文本不构成授权 |
| MaterializedLaunch 与 secrets | 非 wire 类型 | env 明文只留 Node 进程内；不加入 schema/TS。持久 recipe 只保存受控配置/credential 引用与 digest。实现的 `LaunchRecipe` 只存 env **名字与来源**、不存值，强于本文的 `Record<string,string>` 描述 |
| Driver 实现覆盖 | 部分实现 | 仅 claude-print / claude-pty / claude-bg / generic-pty 有 `Driver` impl；codex-appserver、grok-acp、agy-print 只有 materializer argv 配方，经 generic PTY 运行，§5.7 的结构化映射尚不可达。trait 以 `RunHandle` 取代 `observations()`，未实现 `DriverRecord`/`AttachRef`/`ResumeRef` |
| Capability 快照 | 部分实现 | 15 个 CapabilityName 均有取值，但来自 §3.3 矩阵的静态转写：`S*` 直接成为 supported，evidence 固定为指向本文的 `source`，nativeProtocolVersion 恒为 unknown，settings/provider revision 硬编码为 1。§3.2 的四条件门与证据分级尚未执行 |
| MCP / CLI 控制面 | 部分实现 | 工具以 `remuda_*` 而非 §8.1 的 `runtime_*` 命名，并多出 list/keys/rm/worktree/fleet 六个工具；§8.1 的 `--spec` 文件式 CLI 与 capability 对象（allowedHosts/maxChildren/maxDepth/expiresAt）未实现，§8.2 的深度与并发 `RESOURCE_LIMIT` 因此不可执行 |

### 12.1 已收口的字段与仍需运行校验的关系

- hello.resumeCursors 使用 `{journalId,afterSeq:U64}`；driver.capabilities.profileRef 使用 `{id,revision}`，不跟随变化的 registry 默认值。
- Snapshot 使用 `scope:instance|registry`。registry variant 包含 `projectionVersion,projectionEpoch,asOfSeq,hosts,workspaces,commands,history`；instance variant 包含 §7.3 的 instance/runs/commands/pendingInteractions/nodes/history，不以缺少 instance 猜类型。
- tty.detach 使用 `{instanceId,streamId,writerLeaseId?}`，只释放订阅/对应 lease；object.stat 为 `{objectId,sizeBytes,digest,mediaType}`，object.read 为 `{streamId,objectId,offset,length,digest}`，不接受任意文件路径。
- lifecycle 的 entityType 与具体 entity 在 Rust union 绑定；重复的 entityId/state/revision 与内部实体字段仍由 journal 提交入口核对。JournalEvent 的 schema 与解析均禁止带 instanceId 的坏事件退回 registry 分支。
- `instance.open_terminal` 的 Command.operation 也为 `instance.open_terminal`。请求归属于已有 claude-bg Instance，backgroundJobId 必须匹配其 host/store/job；人类 actor、fence、generation 与权限在派发前校验。Command accepted 只证明相应 native-control 接纳，不能证明 Run 成功；重复 commandId 只查原记录。

### 12.2 M0 unknown / reconciliation 与最小错误词汇

| 位置 | 固定的 wire 表达 | 必须保留的语义 |
| --- | --- | --- |
| Command.state | queued / accepted / settled | 不增设 unknown、dispatch_unknown 或 decision_unknown 第四态 |
| Command.resolution | clear / unknown / reconciling | 分开投递进度与是否能判定；unknown 不降成 rejected 或 completed |
| Command.dispatch | not-dispatched / intent-durable / transport-written / native-acknowledged | intent 之后可能发生的 native 写入只能查询/对账，不能因无 ACK 重发 |
| Instance.lifecycle、Run.state、Interaction.state | 各自保留 unknown / reconciling | 断线不表示进程已死或任务成功；已有答案的 Interaction 不回 pending |
| Instance.connectivity | connected / disconnected / reconciling | 与 activity/Run 结果独立；未知 activity 使用 Knowledge，而非默认 idle |
| wait / capability / knowledge | reason:unknown / state:unknown / state:unknown | 没有足够证据时明确拒绝或返回 unknown，不提升为 supported/condition-met |
| RuntimeError.execution / retry | possibly-dispatched / same-command-query 或 after-reconciliation | 非零 API 错误不证明 native 未收到；错误本身不授予重新执行权限 |

[M0_REQUIRED_ERROR_CODES](../../crates/remuda-protocol/src/error.rs) 固定 24 个最低必备 code：UNAUTHENTICATED、SCOPE_DENIED、HOST_OFFLINE、OWNER_FENCED、PROTOCOL_VERSION_UNSUPPORTED、SCHEMA_VERSION_UNSUPPORTED、CAPABILITY_UNSUPPORTED、CAPABILITY_UNKNOWN、BINARY_CHANGED、INVALID_LAUNCH_SPEC、NATIVE_GENERATION_MISMATCH、ATTACH_WOULD_WAKE、CONTROL_UNAVAILABLE、COMMAND_ID_CONFLICT、COMMAND_EXPIRED、COMMAND_OUTCOME_UNKNOWN、NATIVE_RESPONSE_UNKNOWN、JOURNAL_GAP、JOURNAL_DIVERGED、JOURNAL_UNAVAILABLE、STATE_UNKNOWN、RESOURCE_LIMIT、TTY_LEASE_LOST、TTY_HISTORY_GAP。它们继续使用 §9.1 的既有数值分配，不重新编号；完整 47 个 code 仍保留。原计划的 dispatch_unknown/decision_unknown 是上述字段组合的描述，不是可接受的 wire state。

### 12.3 规范化 wire 示例

下面示例均为类型 fixture，不是本次运行的原生命令；fixture binary/version、路径、job ID 与 digest 为占位数据。JSON 文件须保持合法 JSON，来源注释集中在 [wire golden 说明](../../crates/remuda-protocol/tests/wire_golden/README.md) 与测试模块头；不向控制帧加入注释或额外字段。

<!-- golden: question-request -->
统一问题请求（Tea/coffee 原生问答的投影）：

~~~json
{
  "kind": "question",
  "title": "Beverage",
  "fields": [
    {
      "id": "beverage",
      "title": "Tea or coffee?",
      "description": null,
      "input": "single-select",
      "required": true,
      "options": [
        {
          "id": "tea",
          "label": "Tea",
          "description": null
        },
        {
          "id": "coffee",
          "label": "Coffee",
          "description": null
        }
      ],
      "allowFreeText": false,
      "sensitive": false
    }
  ]
}
~~~

<!-- golden: instance-send -->
独立的 prompt Command：

~~~json
{
  "jsonrpc": "2.0",
  "id": "request-16",
  "method": "instance.send",
  "params": {
    "commandId": "cmd_01993ab0-0000-7000-8000-000000000001",
    "payload": {
      "instanceId": "ins_01993ab0-0000-7000-8000-000000000001",
      "runId": "run_01993ab0-0000-7000-8000-000000000001",
      "input": {
        "type": "prompt",
        "mode": "new-turn",
        "blocks": [
          {
            "type": "text",
            "text": "Read the project README."
          }
        ],
        "origin": "human",
        "nativeClientMessageId": "user-fixture-1"
      },
      "completionScope": "native-turn"
    },
    "expected": {
      "ownerFence": "1",
      "processGeneration": "1"
    }
  }
}
~~~

<!-- golden: native-bg-herdr -->
已观测 job 与 Herdr pane；Claude session UUID 仍可未知：

~~~json
{
  "hostId": "hst_01993ab0-0000-7000-8000-000000000001",
  "nativeStoreId": "obj_01993ab0-0000-7000-8000-000000000002",
  "kind": "claude",
  "sessionId": {
    "state": "known",
    "value": "01993ab0-0000-7000-8000-000000000003"
  },
  "transcript": {
    "state": "unknown",
    "reason": "not-emitted",
    "evidenceEventIds": []
  },
  "claude": {
    "sessionId": "01993ab0-0000-7000-8000-000000000003"
  },
  "claudeBg": {
    "jobId": "native-job-fixture-1"
  },
  "herdr": {
    "binaryPath": "/opt/herdr/herdr",
    "version": "fixture-0.9.0",
    "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    "protocolVersion": "fixture-v1",
    "serverIdentity": "obj_01993ab0-0000-7000-8000-000000000010",
    "serverEpoch": "epoch_01993ab0-0000-7000-8000-000000000011",
    "representation": "rendered-ansi",
    "session": "remuda-test",
    "paneId": "w5:p1"
  }
}
~~~

<!-- golden: claude-bg-carrier -->
bg 的首次输入策略：

~~~json
{
  "type": "claude-bg",
  "inputDelivery": "deferred-argv",
  "argvInputPolicy": "explicit-non-secret"
}
~~~

<!-- golden: herdr-carrier -->
PTY carrier 的完整 Herdr pin：

~~~json
{
  "type": "pty",
  "backend": "herdr",
  "server": {
    "binaryPath": "/opt/herdr/herdr",
    "version": "fixture-0.9.0",
    "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    "protocolVersion": "fixture-v1",
    "serverIdentity": "obj_01993ab0-0000-7000-8000-000000000010",
    "serverEpoch": "epoch_01993ab0-0000-7000-8000-000000000011",
    "representation": "rendered-ansi"
  },
  "session": "remuda-test"
}
~~~

<!-- golden: open-terminal -->
显式创建 background attach pane：

~~~json
{
  "jsonrpc": "2.0",
  "id": "request-open-terminal",
  "method": "instance.open_terminal",
  "params": {
    "commandId": "cmd_01993ab0-0000-7000-8000-000000000050",
    "payload": {
      "instanceId": "ins_01993ab0-0000-7000-8000-000000000001",
      "backgroundJobId": "native-job-fixture-1",
      "allowWake": true,
      "carrier": {
        "backend": "herdr",
        "server": {
          "binaryPath": "/opt/herdr/herdr",
          "version": "fixture-0.9.0",
          "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
          "protocolVersion": "fixture-v1",
          "serverIdentity": "obj_01993ab0-0000-7000-8000-000000000010",
          "serverEpoch": "epoch_01993ab0-0000-7000-8000-000000000011",
          "representation": "rendered-ansi"
        },
        "session": "remuda-test"
      }
    },
    "expected": {
      "ownerFence": "1",
      "processGeneration": "1"
    }
  }
}
~~~

<!-- golden: binary-header -->
32-byte header 的解码元数据（payload 为三个原始字节）：

~~~json
{
  "channel": "tty-output",
  "streamUuid": "01993ab0-0000-7000-8000-000000000001",
  "offset": "9007199254740993",
  "payloadLength": 3
}
~~~
