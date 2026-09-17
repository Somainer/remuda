# 原生 PTY 优先（D-028）：统一各 harness 的会话载体与结构化事件

状态：提案（待评审定稿）
取代/收敛：`claude-print` 为默认 driver 的现状、`claude-pty` / `generic-pty` 对 herdr 的依赖
相关决策：D-010、D-013、D-016、D-022、D-025、D-026、D-027

---

## 1 决策与结论

### 1.0 统一原则（Unification Principle，先于一切实现细节）

> **一个 agent session 就是一个 terminal session，里面跑着 agent 命令。没有第二条路径。**

这是 native PTY first 最大的收益，也是本决策的第一性约束。展开为五条可检查的规则：

1. **同构**：Remuda 启动 agent = 开一个 PTY + 在其中执行 launch command；用户自己在终端里敲 `claude` = 同一个 PTY、同一条命令，只是由人打字。此后两者在系统内**逐位同构**。
2. **唯一入口**：**D-025 promotion 是唯一的检测/水合路径**。不存在「Remuda 启动的 agent 专用通道」；native 启动也必须走同一份 promote 代码，不得抄近路。
3. **New Session 即预填**：New Session 选 `claude` / `codex` / `grok`，等价于**在该终端里预填 launch command 并回车**，不是另一种 driver、不是另一种 carrier。
4. **`mode` 只记录出身**：建议语义改名 `launchedBy: "remuda" | "user"`，它只说明谁敲的命令，**不表示能力等级**。
5. **验收判据**：结构化事件、生命周期、审批、resume、附件、scrollback、鼠标——任何一项出现「只有 Remuda 启动时才有」，即为 P 级缺陷。

这条原则同时消灭了当前架构里最贵的一类分叉：driver 决定 UI 能力（`uiMode()` 因为 `driver === "claude-print"` 而返回 `structured-only`，`tty/gate.ts` 因此直接关掉终端），以及 resume 需要先选「结构化还是终端」。统一之后，**每个 session 天然同时拥有两个投影**：终端投影（原始字节）与结构化投影（事件流），UI 按需要取用，不再二选一。

### 1.1 结论

- **采纳 native PTY 为默认 carrier**：`portable-pty` master + 终端模拟器（`vt100`（P0 实测：`vt100` 未发布到 crates.io，`alacritty_terminal` 缺 §4.6 所需能力；见 evidence/native-pty-0.md））+ 字节 ring，无 herdr 依赖。herdr 降为可选 carrier（feature），不急删。
- **采纳 per-session launch shim 作为 P1 核心机制**（不是补丁）：在 `<data_dir>/instances/<id>/launch/bin/` 生成透明 `exec` 的 `claude` / `codex` / `grok`，把 `--settings` overlay、`CODEX_HOME`、`GROK_HOME`、hook socket 注入进去。**这是让「用户手敲」也拿到结构化信号的唯一机关**，也是统一原则能成立的物理基础。
- **生命周期五件事必须在 native 路径上重建**（§5）：新建 Session、发送消息、停止（打断 turn / 停进程两义）、删除、退出检测，外加 resume。herdr 今天替我们做的是**整个 agent 抽象**，不是一个 PTY 库。
- **采纳信号分层**：`Hook > File(tail) > OSC > Screen`，每条 `Observation` 带 `SourceChannel`，UI 能解释「凭什么说它 blocked」。
- **结构化视图必须实时流式**（§7）：claude 走 `MessageDisplay` hook 的行级 delta + transcript 块级权威值，grok 走 `updates.jsonl` 的 ACP chunk，codex 只有 completed item——**能力诚实上报，不拿 record 级冒充 token 级**。
- **保留 `codex-appserver` / `grok-acp` 枚举与 wire crate**（`remuda-codex-wire` 已 D-013 freeze，含类型化审批）：codex 审批已由 hooks 路径实测打通（§3、§14 风险 3 已解除），它们降为**旁路后备**；而 **grok 的结构化作答目前没有别的出路**（§14 风险 15），它们仍是唯一候选。
- **`claude-print` 按条件退役，不按日程退役**（见 §12），先降为非 TTY 宿主的 legacy。
- **跨 Node 重启存活是唯一没有廉价替代的 herdr 能力**（§8）：本轮**先接受丢失**（方案 A），把 `remuda-ptyd`（方案 B）排到 P8。**用户已拍板（2026-09-14）：先接受丢失，持有进程放后面做。**
- 工期：约 26 人日，4 worker 并行约 12–14 个工作日（不含 P8）。

---

## 2 现状与问题

### 2.1 herdr 桥接的代价

`claude-pty` / `generic-pty` / `claude-bg` 都是 `remuda-herdr` JSON-RPC unix socket 的薄封装，仓库里约 925 处 `herdr` 引用，实际被调用的是 21 个 JSON-RPC 方法 + 1 条 CLI 子进程桥。herdr 拥有的是**整个 agent 抽象**：`agent.start` 认识 CLI、`AgentStatus{Idle,Working,Blocked,Done,Unknown}` 是它的模型、`agent.prompt` 负责 bracketed paste 与提交时序、`agent.wait(until,timeout)` 是 `instance wait` 的实现、`pane.agent_status_changed` 订阅驱动交互检测、`TtyBridge::Herdr` 才能 attach。代价有四：

1. **每台宿主要装一个额外 daemon**，并且要管 socket 目录、named session、durable 资源回收（`PtyResourceStore`）。
2. **状态语义是别人的**：Blocked 才触发交互检测，状态来源不可解释，无法细分「等审批」与「等输入」。
3. **同一件事两套实现**：`shell-pty` 已经用 `portable-pty` 直接拿到了 raw PTY、mouse 透传、256 KiB scrollback、`tcgetpgrp` 前台进程识别、D-025 promote/demote 与 transcript 水合——**完全不依赖 herdr**。也就是说，herdr 路线与 native 路线在仓库里并存，维护双份。
4. **它把统一原则挡在门外**：用户在自己终端里敲的 `claude`，herdr 看不见，也不可能被 `agent.start` 接管。

herdr 剩下的**唯一不可替代价值是跨 Node 重启存活**（以及现成的 `--bg`）。删除节点应以此为准，不以 `--bg` 为准。完整的 29 项能力对账见附录 A。

### 2.2 `claude-print` 的特有能力（退役必须逐条对账）

| 能力 | print 今天怎么做 | native PTY 的代价 |
|---|---|---|
| 结构化流 | `-p --input-format stream-json --output-format stream-json --include-partial-messages`，块级 open/append/replace/close、thinking、`parent_tool_use_id` 嵌套，标 `Completeness::Structured` | 退化为 transcript 水合：现有 `TranscriptMapper` 只处理 `user` / `assistant` 记录，粒度从 token 级降到 record 级——**§7 用 `MessageDisplay` hook 把行级 delta 补回来** |
| `can_use_tool` 审批 | 控制请求 → `Interaction`（`InteractionCarrier::ClaudeControl`，`blocking: true`），带精确 tool input 摘要、支持 `updated_input` | PTY 侧只能屏幕反推（y/n 扫描、编号菜单、`❯` 光标位），`Completeness::ScreenDerived`——**§4.4 的 `PermissionRequest` hook 把阻塞裁决补回来** |
| `AskUserQuestion` | 同一通道，typed `QuestionField` | 退化为通用菜单 scrape；`Elicitation` hook 可覆盖 MCP 侧 |
| 图片投递（D-027） | 唯一发出真实 base64 image block 的路径（上限 3.5 MiB），其余 driver 只能提路径 | **退役后全队没人能真收到图片字节** —— 必须先有替代（MCP 工具，见 §4.5） |
| cost / usage | **全仓唯一 `UsagePayload` 发射点**（从 `result` 帧读 `total_cost_usd`） | 无替代则 UI 的用量页永久显示 `—`；codex `token_usage_record` / grok `usage.json` 可换算，标「估算」 |
| resume 身份（D-026） | `system/init` 的 `session_id` 提升为 lifecycle，Node 覆盖 `nativeRef` 占位 | **可替代**：`SessionStart` hook 写 `session-meta.json` 的并行路径已存在，Node 匹配器共用 |
| 非 TTY 宿主 | 唯一既不需要 herdr 也不需要控制终端的 driver（`node --stdio`、SSH-stdio、机器人会话依赖它） | `pty.fork` 在这些宿主上是否可用**未实测** |

此外 print 是**到处都写死的默认值**：Hub `default_driver()`、seed SQL、fleet、placement、ssh_hosts、Node `default_driver_kind()`、CLI `--driver` 默认、MCP 工具 schema 文案、机器人侧 `driver_for(Claude)`。贸然删除会留下一地 dangling kind。

---

## 3 各 harness 结构化信号矩阵

标注规则：**[V]** = 已实测验证；**[U]** = 未验证。本轮三份实机证据：[codex-signals-1](./evidence/codex-signals-1.md)（codex-cli 0.154.0）、[grok-signals-1](./evidence/grok-signals-1.md)（grok 1.0.30）、[claude-queue-steer-1](./evidence/claude-queue-steer-1.md)（claude 2.1.270）。下表凡标 [V] 并给出证据文件的，以该文件的实机记录为准；模型响应由本地确定性服务提供，测的是 client 侧信号与控制，不是托管模型行为。

| harness | A 可直接作答 | B 结构化观测 | C OSC（地板） | D 屏幕签名 |
|---|---|---|---|---|
| **claude** | **[V]** `PermissionRequest` hook 阻塞返回 `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"｜"deny"}}}`，无需按键（**裸 `{"behavior":…}` 无效**，见 §3.1）；`Elicitation` 可返回 `action` | **[V]** 二进制内含 **33 个 hook 事件**（远超公开的 9 个），含 `MessageDisplay`（行级 delta）、`Notification`、`SessionEnd`、`StopFailure`、`PostToolBatch`；payload 直接给 `transcript_path`；transcript JSONL 追加 | **[V]** `OSC 9;4;3/0`、`OSC 0` 标题、`OSC 777;notify` | 兜底 |
| **grok** | **[V]** **没有 `PermissionRequest` hook**：该事件名注册时被静默忽略，相邻条目照常加载；但 `PreToolUse` 返回 `{"decision":"deny"｜"ask"}` **[V]** 生效（deny 连 `--always-approve` 都拦得住，`ask` 能逼出可见审批框）——**只能拦，不能替用户答**；**[V]** ACP `session/request_permission` **不写进 `updates.jsonl`**（54 帧全是 `session/update` / `_x.ai/session/update`），作答仍靠按键（[grok-signals-1](./evidence/grok-signals-1.md) A3/A5） | **[V]** `updates.jsonl` 是 **ACP `session/update` 逐字帧**（`remuda-acp-wire` 可直接解，含 `turn_completed{stop_reason,elapsed_ms}`）+ `events.jsonl`（`turn_started`/`phase_changed`/`first_token`/**`turn_ended{outcome}`**/`permission_requested`/`permission_resolved`）+ `usage.json`；**[V]** `active_sessions.json` 的 `pid` 就是 TUI 进程本身（不是 recorder、不是父 shell），但**条目在 shutdown 时先于进程退出被移除** → 只能当发现路径，**不能当 liveness**（同上 A1/A2） | **[V]** 原始 PTY 抓包实测到 `OSC 0` 标题（载荷含 `⚠ Action Required` / `Thinking` / `Running: <tool>`）与 `OSC 9;4;1;-1` / `9;4;0;0`，72 个去重样本；**herdr 的 rendered-ansi 已把这些控制序列吃掉**，必须抓原始字节（同上 A4） | 作答路径 |
| **codex** | **[V]** `hooks.json` 的 `PermissionRequest` **同步阻塞并返回真实裁决**：`{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"｜"deny"}}}`，allow/deny 双向实测落地（deny 侧 rollout 记 `Rejected(...)`，allow 侧记 exit 0）；**stdin 无 `tool_use_id`**、tool 名被归一成 `Bash` → **不得复用 `PreToolUse`/`PostToolUse` 的关联 schema**（[codex-signals-1](./evidence/codex-signals-1.md) A1） | **[V]** rollout JSONL（`task_started`/`task_complete`/`item_completed`/`response_item`/`token_usage_record`/`turn_aborted`/`turn_context`）实时追加 + `session_index.jsonl`（**[V]** 只是 append-only 的**名字索引**，不是 PID 注册表，也不是全量线程目录）；**[V]** **hooks trust gate 已破**：`[features] hooks = true` 是开关，**per-handler canonical hash**（handler 归一 → 补 snake_case `event_name` → TOML → JSON 递归排序 → compact → `sha256:`）写进 `hooks.state` 即持久化 trust，无需任何 bypass | **[U]** | tier D 兜底（**不再是审批的唯一出路**） |
| **agy** | — | **[V]** hook 只有 namespace 化的 `PreInvocation`/`PostInvocation`；transcript 是 SQLite + protobuf blob，无 schema 不可读 | **[V]** 二进制内含 `OSC 9;4` 进度发射 | 状态与作答 |

### 3.1 已验证的坑

- **[V]** claude 的 `permissionDecision` 键（`PreToolUse` 用的）在 `PermissionRequest` 上被静默忽略；**裸的 `{"behavior": …}` 同样被静默忽略**（2.1.221 实测：审批框照常弹出、工具被拒、回复里没有任何出错迹象——这正是 §14 风险 1 的「点了批准但没动」）。唯一生效的形状是嵌套的 `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"｜"deny"}}}`，且 `hookEventName` 必须与被调用的事件一致，否则整条被丢弃并报 `Hook returned incorrect event name`。allow-always 走同一 `decision` 下的 `updatedPermissions`，原样回传 `permission_suggestions` 即可。见 [native-pty-5](./evidence/native-pty-5.md) §3–§4。
- **[V]** **confined session 里 allow 无效**：二进制明确告知「受限会话只接受命令行上的授权」。→ 审批链路必须能回落按键，否则会出现「点了批准但没动」。
- **[V]** `SubagentStop` 在没有 subagent 时也会触发（recap/summary），**绝不能当作 working**。
- **[V]** grok 即使重定向 `GROK_HOME`，仍会吃 `~/.claude/settings.json` 里的 hook 条目 → hook 脚本必须按 `$0`/env 自辨 harness 并幂等去重。**[V]** `CLAUDE_CONFIG_DIR` **重定向不了**这条发现路径（实测失败），唯一有效的中和开关是 **`GROK_CLAUDE_HOOKS_ENABLED=0`**（实测把 26 条 Claude hook 从 `enabled` 变 `disabled`）；它只停用**激活**，不阻止 grok 继续读 Claude 配置做清单。
- **[V]** grok 的 `--plugin-dir`（文档称「始终受信」）仅 `grok agent` 子命令可用，顶层交互式被拒。
- **[V]** codex 0.154 的排队键是 **Tab**，`Ctrl+;` 已移除；帮助文本原话是「任务运行时按 Tab 排队，否则立即发送」。
- **[V]** codex 的 hook trust key 必须用 `hooks/list` 返回的那一条（macOS 会把 `/tmp` 规范成 `/private/tmp`），**不能自己拼路径**；RPC 的 `eventName` 是 camelCase，**不是**参与哈希的拼写。哈希只认 **handler 配置**，**不认脚本内容**——脚本被换掉仍然是 trusted。
- **[V]** codex 的 `--dangerously-bypass-hook-trust` 放在 `app-server` 前面时**不生效**（未受信 hook 照样不执行）；实测只有 `codex exec` 路径能绕。因此 Remuda 必须走「写 `hooks.state` 持久化 trust」这条正路。
- **[V]** grok 的 `turn_completed` **不等于成功**：被取消的 turn 同样发这个 tag，必须读 `stop_reason`（`end_turn` vs `cancelled`）与 `events.jsonl` 的 `turn_ended.outcome`。
- **[V]** 各家 headless 专属开关（`--output-format stream-json` / `--include-partial-messages`）在交互式 PTY 下不可用——这正是要走 hook + 文件 tail 的原因。

---

## 4 目标架构

### 4.1 载体（Carrier）

`remuda-pty`：唯一载体。`portable-pty` master + `vt100` 模拟器 + 256 KiB 字节 ring。提供：

- **身份**：`tcgetpgrp` 读前台进程组（内核真相，无 `unsafe`），匹配 agent 表；屏幕签名兜底。
- **模式集**：模拟器观测到的 DECSET —— `?2004`（bracketed paste）、`?1049`（alt-screen）、`?25`（光标）。
- **网格**：渲染后的行文本 + 光标位置，取代「去 ANSI 的字节尾」，消灭光标移动被丢弃导致的签名漂移。
- **raw 双向字节**：不过滤。

`Carrier` trait 上 `NativePty` 为默认，`Herdr` 为可选 feature。

### 4.2 信号适配器

`remuda-signal` 定义 `SignalAdapter` trait：输入（hook socket 事件 / 文件 tail 记录 / OSC token / 屏幕网格）→ 输出 `Observation` 与 `Interaction`。

- **传输**：per-instance unix socket（`<instance dir>/hook.sock`，0600）。socket 路径由 driver 计算后**刻意注入**子进程环境，不进继承白名单——环境变量边界是既有安全约束（凭据泄漏、`LD_PRELOAD`/代理/CA 注入）的分界线，新增变量必须走 driver-computed 一侧。凭据 per-instance，独立于主设备凭据。**替换今天 `spawn_hook_watch` 每 200 ms 轮询一个 JSON 文件的做法**——轮询无法让 `PermissionRequest` 阻塞等答案。
- **overlay materializer**：生成 per-session 的 `--settings` 合并文件、`CODEX_HOME` / `GROK_HOME` 影子目录与 hook 脚本。**只 merge、只在 session 生命周期内存在、从不写用户自己的配置文件。**
- **launch shim**：`<instance dir>/launch/bin/` 下的透明 `exec` 包装，置于 PATH 最前。用户在该终端里手敲 `claude` 也会命中 overlay ——**这是统一原则的物理实现**。
- `remuda-screen`：从现有 promote 与交互识别代码抽出的签名库，**零 herdr 依赖**，输入改为模拟器网格，规则来自 §10 的可版本化规则表。

### 4.3 生命周期状态

优先级 **Hook > File(tail) > OSC > Screen**。`AgentStatus` 由 adapter 产出，而不是由载体猜。每条 `Observation` 带 `SourceChannel::{Hook,File,Osc,Screen}` 与 `Completeness`。`SubagentStop` 恒不视为 working。`unknown` 永不塌缩成 `idle`；`done` 由「idle ∧ 自上次 turn 结束后未被任何设备读过」派生，不是一种检测结果。

**capabilities 改为运行时上报**（`nativeRef.capabilities` + `signalTier`），静态能力表退为 fallback；Hub placement 的 `requiredCapabilities` 读运行时值。今天 `capabilities.rs` 的矩阵**只按 `DriverKind` 取值**，所以 promoted kind 无法改变能力，这是结构性缺陷（Node 侧还有第二份硬编码副本）。这直接兑现统一原则第 5 条：能力由**这个 session 实际拿到的信号层级**决定，与「谁启动的」无关。

### 4.4 审批与提问

全部落进现有 `InteractionRuntime` / broker：

1. **tier A（hook 阻塞裁决）**：hook 在 socket 上阻塞等 broker 结论，把裁决作为 hook 返回值交还 agent 自己执行。请求带真实 tool 名与输入，没有「我的回车到底有没有落地」的歧义。**这让 native PTY 首次实现 `respond_interaction`**（今天 `shell-pty` 直接返回 `CapabilityUnsupported`）。等待必须有界、对齐 broker TTL，**超时一律 deny**。
2. **tier D（屏幕作答）**：D-022 既有不变量**一字不改** —— 写入前先置 `attempted`（丢 ACK 也绝不重放回车）、`answerable &= !screen_truncated`、每 carrier 仅一次自动 trust、自动 trust 仅限已注册 workspace 内的双选项目录对话框。
3. `InteractionCarrier` 新增 `HarnessHook`（不删 `ClaudeControl`，print 尚在）。`AskUserQuestion` 由 `PreToolUse` 的 `tool_input` 重建 typed 字段，作答退化为按键；**hook 不携带渲染后的选项文案**，选项文本仍需从屏幕取，这是 tier A 与 tier D 必须共存的原因。

### 4.5 投递（prompt 与附件）

**ready 阶梯 = hook 回执 > 模拟器模式 + 静默 > 字形**，细则见 §5.2。**附件**：从「driver 分层」升级为 MCP 工具 `remuda_attachment(objectId)` 返回 image block。MCP server 已经注入，三家 harness 都支持——覆盖面**大于** print 的 base64 路径，路径提及降为 fallback。D-027 的 Hub 暂存 + Node 落盘、限额与 403 规则不变。

### 4.6 终端

D-016 线格式零变更（二进制通道、snapshot-then-live、字节透明），无需协商新的 snapshot 种类：

- **snapshot 改为模拟器合成重绘**（reset + 当前网格 + 当前模式集），不再从字节 ring 中间截断、不再重放过期 DECSET；顺带治掉 web 侧鼠标追踪的 workaround，并补上 herdr `frame.full` 才有的「重绘 vs 增量」区分。
- **scrollback 的根因是载体**：herdr 的 `rendered-ansi` 是视口 blit，帧内没有 `\n`、没有 `ESC D`/`ESC[S`，xterm 只是原地覆写单元格，**永远没有行被逐出**，所以 `term.scrollLines()` 必然 no-op。换成真字节后 scrollback 自然成立，这是 native PTY 对用户最直观的收益。
- `?1049` 生效时只快照 alt 网格（全屏 TUI 没有有意义的 scrollback），`TtyAttach` 上报 `altScreen: bool`，web 据此禁用本地滚轮劫持而不是靠推断。
- 字节 ring 保留，作为解析失败时的诚实兜底；鼠标报文双向透明，服务端**永不过滤** CSI/SGR，门禁是客户端的职责。

---

## 5 生命周期操作（没有 herdr 之后必须补齐的五件事）

本章是用户点名的硬需求：**「现在不使用 herdr 了之后，记得开启新的 Session、停止/删除 Session、发送消息的功能也记得要补上」**。它们不是细节，是 `agent.start` / `agent.prompt` / `agent.wait` / `pane.close` 被移除后留下的洞。

### 5.1 新建 Session（替代 `agent.start` + `workspace.create` + `pane.split`）

New Session = 在一个 Remuda 自持 PTY 里**预填 launch command 并回车**，用户在终端投影里**看得见那条命令**。四步：

1. **开 PTY**：`portable-pty` + cwd（已注册 workspace 或 worktree，D-023/D-014）+ `env_clear` + allow/denylist（`crates/remuda-driver/src/child_env.rs`），并**剥离**宿主继承的 `CLAUDE_CODE_EFFORT_LEVEL`（§9）。
2. **materialize**：按 kind 取 recipe → argv + overlay 文件。`crates/remuda-driver/src/materializer.rs` 今天对 shell-pty **显式拒绝**，必须新增一个 arm；`crates/remuda-driver/src/flags.rs` 的 BANNED/RESERVED/EXTRA 白名单今天只被 materializer 调用，shell-pty 完全绕过——native 路径必须先过白名单再拼 argv。
3. **起进程**：shim 置于 PATH 最前；argv[0] 走 recipe，而不是把 `launch.request.args` 原样透传给 `CommandBuilder`。
4. **落审计**：`LaunchRecipe` 要真填 `settings_digest`、`env_allowlist`、provider kind（今天 shell-pty 发的是 stub：空白名单、硬编码 provider）。

**kind / driver 矩阵**（今天 `validate_kind_driver` 只允许 `(terminal, shell-pty)` 与 `(generic, shell-pty)`，这是 native 路径最硬的 blocker）：

| kind | 今天的 driver | D-028 后 | 说明 |
|---|---|---|---|
| `terminal` | `shell-pty` | `shell-pty` | 登录 `$SHELL`，D-025 promote 的入口，不变 |
| `claude` | ~~`claude-print`（默认）~~ / `claude-pty` | `agent-pty` | print **已是显式专用 carrier，绝不再作默认或回落**（D-034）；`claude-pty` 随 herdr 转 optional |
| `codex` / `grok` / `agy` | `generic-pty`（herdr） | `agent-pty` | 每 kind 一份 recipe + adapter |
| `generic` | `shell-pty` | `shell-pty` | 未识别 CLI 的 fallback，保留 |

> **D-034：默认 carrier 的选择顺序**（适用于上表所有 agent kind）。宿主 `driverInventory` 报 `shell-pty` launchable → `shell-pty`；否则 herdr 已广播 → `claude-pty`（codex/grok 为 `generic-pty`）；两者皆无 → **以理由拒绝**。`claude-print` 只能显式指定（`--driver claude-print` / web picker 显式选择），并作为诊断 carrier 标注——print 跑完一轮即结束，需要手工 resume，作为 worker carrier 无用。Node 侧请求的 carrier 构造失败时一律 `instance.create` 带 reason code 拒绝，**不降级**；名册与实例记录记录的是 Node 实际跑起来的 driver。

**per-kind launch recipe / preset**（把 `generic_pty.rs` 里的 `PRESETS` / `merge_yolo_argv` 搬出 herdr driver，成为载体无关的表）：

| kind | 基础 argv 与环境 | yolo argv（仅 Human/Bot 且显式 bypass） | overlay | session id 来源 |
|---|---|---|---|---|
| claude | `claude` + `--settings <overlay>` + `--effort <v>` | `--dangerously-skip-permissions` | settings overlay：hooks、`tui`、`showStatusInTerminalTab`、`terminalProgressBarEnabled` | `SessionStart` hook → `session-meta.json` |
| codex | `codex`，`CODEX_HOME` 指向影子目录 | `--dangerously-bypass-approvals-and-sandbox` | 影子 `config.toml`（`hooks = true`、`notify`）+ `hooks.json` | `SessionStart` hook；否则 `session_index.jsonl` 最新 `updated_at` |
| grok | `grok`，`GROK_HOME` 指向影子目录 | `--always-approve` | 影子 `hooks/*.json`（须自辨 harness，见 §3.1） | `active_sessions.json` 按 PTY 子进程 pid 匹配 |
| agy | `agy` | `--yolo` | `config/hooks.json`（namespace 键） | `PreInvocation.conversationId` |

yolo 取值沿用 `generic_pty.rs` 现值，迁移时逐条对照 binary pin 复验。**D-011 / D-017 的授权规则原样保留**：Agent origin 一律使用非 yolo preset，bot dispatcher 不自动 bypass。

### 5.2 发送消息（替代 `agent.prompt`）

herdr 把 bracketed paste 与提交时序整个藏在 `agent.prompt` 里；native 路径必须自己构造，且**不得把「写成功」当成「已接纳」**。

**ready 阶梯**（从强到弱，取第一个可用的）：

| 级别 | 判据 | 适用 |
|---|---|---|
| 1 hook 回执 | claude `UserPromptSubmit`（codex/grok 同名 hook 存在，待实测） | 唯一能证明 composer 真的吃下了文本 |
| 2 模拟器模式 + 静默 | 观测到 `?2004` 且屏幕静默 ≥80 ms（上限 500 ms） | 无 hook 时的默认 |
| 3 字形 | 规则表的 idle prompt box 规则（§10） | 最后兜底，`unknown` 时排队而不是硬发 |

规则：

- **bracketed paste 只在观测到 `?2004` 时使用**，否则 `ESC[200~` 会被当字面量打进去。
- **body 与 Enter 分两次写**：先写正文（多行时包 `ESC[200~`…`ESC[201~`），等静默，再写 `\r`。固定 500 ms 改为「静默 ≥80 ms，上限 500 ms」。
- **移动端本地输入 dock 必须同样拆分**：`web/src/features/session/tty/LocalInput.tsx` 今天把 `text + "\r"` 一次写出——这正是文档里记着的「永远提交不进 Claude TUI」的模式。要么在 web 侧拆两次写，要么把 dock 路由到 `instance.send`。
- **投递账本**：D-022 队列、`wait_control` 三态、`ControlUnavailable` 契约零改动；protocol §5.2 的 `status: "queued" → replace 为 complete` 也不变，只是 ready 判定换了更可靠的来源。

### 5.3 停止：打断 turn 与停进程是两件事

| 动作 | 入口 | 语义 | 实现 | 验收 |
|---|---|---|---|---|
| 打断当前 turn | `instance.cancel` | 结束这一轮，**session 与进程都活着** | 按 harness 发键（§6），**不是 `\x03`** | 出现原生终止证据（claude `Stop`/`StopFailure`、codex `TurnAborted` 或 `■ Conversation interrupted`），实例仍 ready |
| 停止进程 | `instance.close` | 终止本实例托管进程，保留原生 transcript | 对**进程组**走信号阶梯 | 前台 pgid 消失、`waitpid` 已回收、无残留子进程 |
| 删除 | `DELETE /v1/instances/{id}` | 连记录一起删 | 终态直接删；运行中须 `?force=1` | §5.4 |

今天 `shell_pty` 的 `cancel` 写 `\x03`。对登录 shell 这是对的，对 agent TUI 是错的：Claude 里 Ctrl+C 不是「打断本轮」，连按两次会退出。**promote 之后 cancel 必须改走 per-harness 键位。**

**停进程的信号阶梯**（对 **process group**，不是单个 pid）：

1. `killpg(pgid, SIGINT)`，等 ≤2 s；
2. `killpg(pgid, SIGHUP)` 并关闭 master fd，等 ≤2 s；
3. `killpg(pgid, SIGKILL)`；
4. `waitpid` 回收，并**复查进程组已消失**；未消失就记 `stop-incomplete` 诊断，**不谎报 exited**。

必须同时修掉两个已知缺陷：`state.child.try_lock()` 拿不到锁时**静默跳过 kill**（改为可等待获取或带超时）；`child.kill()` 只 SIGKILL 直接子进程（登录 shell），在其中启动的 `claude` 会被孤儿化。

### 5.4 删除 Session

`DELETE /v1/instances/{id}` 语义在 D-024 addendum 与 [remote-terminal.md](./remote-terminal.md#deleting-a-session) 已定稿，native 路径只需保证两件事：

- `?force=1` 的「先停后删」走的是 §5.3 的**同一条信号阶梯**，而不是另写一份 kill；
- `instance.purge` 删掉 `<data_dir>/instances/<id>`（launch 产物、overlay、hook socket、附件、pty 日志），**永不碰**用户自己的 `~/.claude` / `~/.codex` / `~/.grok` transcript。

herdr 时代遗留的已知债「`instance stop` 不回收 herdr workspace」随 carrier 一起消失：native 路径没有 workspace/tab/pane 三层，只有一个进程组。

### 5.5 退出检测

今天 `read_pty` 在 EOF 只 `break`，没有任何 journal —— **崩掉的 agent 在 UI 上永远 ready**。native 路径必须双证据：

- 一个 waiter task `child.wait()` 拿退出码/信号；
- PTY master EOF 作为第二证据（`wait()` 竞态时先到）。

任一先到即发 `native-exit` → Node `record_task_exit` → lifecycle `exited`（退出码 0）或 `failed`（非零/信号），并写明来源。

**promoted 实例要区分两种退出**：前台 agent 消失是 **demote**（D-025），shell 还活着，实例不终结；只有 shell 自己退出才是实例 `exited`。

### 5.6 Resume

resume = **新开一个 session，预填 `--resume <sid>`**——与 New Session 完全同路径。D-026 的全部规则不变（不复活进程、父子双向落账、Human/Bot 限定、未上报 session id 409、过期 409、短窗口幂等）。native 路径要打通的是两处今天必然 409 的地方：`shell_pty` 没有语义 resume，以及 Hub 的 `mode.driver` 从不返回 shell-pty。

`ResumeMode{structured,terminal}` **可以删除**：统一之后每个 session 天然有两个投影，用户不需要先选。session id 三家来源见 §5.1 表。

---

## 6 steer / 排队 / 打断

用户要求：「agent 正在工作时，composer 要能 **发送（steer）/ 排队 / 打断**，并且能力要诚实」。三家的键位语义**已逐一实测**（agy 仍未验证），**三家互不相同，且都不是「Enter 立刻插话」这么简单**。

| harness | turn 中打字 + Enter | 排队 | 打断 turn | 证据 / 缺口 |
|---|---|---|---|---|
| **claude** | **[V] 入队 + 边界 steer**：Enter 把消息交给原生队列（`queue-operation{enqueue}`），在**当前工具结果返回之后、下一次工具调用之前**被消费（`remove{reason:"absorbed_mid_turn"}` + 追加一条 `attachment.queued_command`）——**既不打断正在跑的工具，也不等整个 turn 结束**（实测：三次 `sleep 20` 的 turn 中，插入项在第 1 次结果后 6 ms 被吸收，第 2 次 sleep 照常开始）。`UserPromptSubmit` 在**入队时**触发（+53…91 ms）→ 它只证明「composer 已接纳」，**不是投递回执** | **[V]** 同一条路径就是队列；`Ctrl+X`+Enter（`chat:queueSubmit`）绑定确实存在，但一次性发键与分键两种发法**都**在工具边界被吸收，**没有「只在 turn 结束后才投」的保证**。`Tab` **[V]** 什么都不做（文本留在 composer，无 hook、无 enqueue） | **[V]** `Esc`：既能打断重试中的请求，也能打断正在跑的工具（实测 `sleep` 进程消失、Claude 进程存活）；**队列里的文本不会被清掉**，会作为新 `user` record 自动接着跑 → `Esc` ≠「全停了」 | [claude-queue-steer-1](./evidence/claude-queue-steer-1.md)（2.1.270）。另 **[V]**：bracketed paste 里含 CR **不提交**、body+CR 一次写也**不提交**，必须分两次写（呼应 §5.2）；未观测到 `popAll`，不得臆造 |
| **codex** | **[V]** **立即 steer**：消息进入**当前 turn**（与基础消息共享同一 `turn_id`），在工具 yield 边界被提交；**不打断正在跑的工具** | **[V]** `Tab` = **排到下一个 turn**（实测排队项在上一个 `task_complete` 之后才开新 turn）；`Ctrl+;` 0.154 已移除 | **[V]** `Esc` → `event_msg/turn_aborted{reason:"interrupted"}`，该 turn 无 `task_complete`；但**已启动的工具进程不一定随之结束**（实测 `sleep 20` 事后仍以**旧 `turn_id`** 落了一条完成 item）→ 打断 turn ≠ 杀掉工具 | [codex-signals-1](./evidence/codex-signals-1.md) A5 |
| **grok** | **[V]** **默认排队**（footer `Enter:queue`）：当前 turn 结束后 +32 ms 才开新 turn。**[V]** 排队后在**空 composer** 上再按 Enter = **取消当前 turn 并立刻发出已排队项**（`turn_ended{outcome:"cancelled",trigger:"send_now"}`，下一 turn 带 `redirect_kind:"queued_after_cancel"`）——这是**唯一实测到的 send-now 传输**，代价是丢掉当前 turn | **[V]** 即 Enter；但**没有普通入队记录**，只有取消后重投的 `redirect_kind`（`cancel_then_send` / `queued_after_cancel`）可事后佐证 | **[V]** `Esc` **不是打断键**：它保留正在跑的 turn **与草稿**，只提示「Press Ctrl+c to cancel the turn」。第一次 `Ctrl+C` **只清草稿**，第二次才取消（`turn_ended{outcome:"cancelled",trigger:"ctrl_c"}`）→ **一次 `Ctrl+C` 不是打断回执** | [grok-signals-1](./evidence/grok-signals-1.md) A4。footer 另标 `Ctrl+Enter:send now` / `Ctrl+;:queue`，这两个物理组合键**未单独验证**；键位可配置，规则表须按 footer 实况校准 |
| **agy** | **[U]** | **[U]** | **[U]** | 信号面最弱，v1 只承诺打断进程 |

**composer 三个动作 → 每 harness 的实现**（`native` = 用 harness 自己的语义，`emulated` = Remuda 代劳）：

| composer 动作 | claude | codex | grok | agy |
|---|---|---|---|---|
| **发送**（尽快交给模型） | **native（边界 steer）**：Enter → 入队 + 下一个工具边界消费。UI 文案说「下一个工具边界」，**不承诺瞬时** | **native（立即）**：Enter | **无原生 send-now**。Remuda **不得**把「发送」偷偷实现成「取消+发送」；只有用户显式选 **打断并发送** 时才走 emulated 的「空 Enter = 取消当前 turn → 发出排队项」 | **[U]** |
| **排队**（等这轮结束再投） | **emulated**：Remuda 自持队列，收到 `Stop` 后投递（原生 Enter 与 `Ctrl+X`+Enter 都没有 after-turn-only 保证） | **native**：`Tab` | **emulated**：Remuda 自持队列，收到 `turn_ended` / `turn_completed` 后投递（原生 Enter 虽然也排队，但账本在 harness 内部且无 enqueue 记录，无法增删改） | **[U]** |
| **打断** | **native**：`Esc` | **native**：`Esc` | **emulated 序列**：`Ctrl+C`（清草稿）→ `Ctrl+C`（取消）；**`Esc` 不是打断键** | 仅停进程（§5.3） |

映射规则（三条，逐条对应上表）：

1. **发送 = harness 原生的 send-now，有才用**：codex 用 Enter，claude 用 Enter 但语义是**边界 steer**，grok **没有**——缺失时 Remuda 不自作主张，由用户在 UI 上显式选「打断并发送」。
2. **排队 = Remuda 自持队列 + 在 harness 的 turn 结束信号上投递**（claude `Stop` / codex `task_complete` / grok `turn_ended`）——**唯一例外是 codex 的 `Tab`**，那是原生的，直接用原生。
3. **打断 = `Esc`（claude / codex）或 grok 的取消序列**；三家都必须等到原生终止证据才算成功（§5.3 验收列）。

**协议面**：今天 `PromptMode` 只有 `new-turn` 一个变体，`steer` 在 `DriverInput` 里存在但没有任何 node 代码读 `mode`；排队实际由 D-022 的 `pty_queue` 完成。新增 `queue` 语义与 `steer` 的 PTY 实现由**协议 worker 一次性增量落地**（§13 冲突规避规则②）。**capabilities 必须逐 harness 分别标出 `steer` / `queue` / `interrupt` 三项各自是 `native | emulated | unknown`**（而不是一个笼统的 bool），UI 据此说明「排队由 Remuda 代持」还是「harness 原生排队」。

**协议面**：今天 `PromptMode` 只有 `new-turn` 一个变体，`steer` 在 `DriverInput` 里存在但没有任何 node 代码读 `mode`；排队实际由 D-022 的 `pty_queue` 完成。新增 `queue` 语义与 `steer` 的 PTY 实现由**协议 worker 一次性增量落地**（§13 冲突规避规则②）。

**composer 三态 UI**（主按钮由上表的「发送」一列决定）：

| 实例状态 | 主按钮 | 次按钮 | 已排队内容 |
|---|---|---|---|
| idle | 发送 | — | — |
| working，有原生 send-now（codex） | 发送（立即） | 排队（原生 `Tab`）· 打断 | 队列 chip（`pty_queue`），可 remove |
| working，发送 = 边界 steer（claude） | 发送（下一个工具边界） | 排队（Remuda 代持）· 打断 | 队列 chip（`pty_queue` 为准，`queue-operation` 对账），可 remove |
| working，无原生 send-now（grok） | 排队 | 打断并发送 · 打断 | 队列 chip（`pty_queue`），可 remove |
| blocked（等审批/等输入） | 发送（进 D-022 队列） | 打断 | 队列 chip |

**诚实性规则**：claude / codex / grok 的 `steer` / `queue` / `interrupt` 现已有实测结论（上表），capability 直接上报 `native` 或 `emulated`；**agy 三项仍为 `unknown`**，调用返回 `CAPABILITY_UNKNOWN`，UI 显示「尚未验证」而不是灰按钮假装不支持。**emulated 必须在 UI 上可见**（「排队由 Remuda 代持」「打断并发送会取消当前 turn」），不得包装成原生；未发出即被取消的消息按 protocol §5.2 更新为 `status: "interrupted"`，不留在 `queued` 里骗人。

---

## 7 实时流式结构化视图

用户要求：结构视图要**逐字/逐行增量出现**（文本、思考、工具调用），不是一条消息一次性蹦出来。

| harness | 主通道（实时） | 权威通道（最终值） | 能承诺的粒度 |
|---|---|---|---|
| **claude** | `MessageDisplay` hook：`turn_id` / `message_id` / `index` / `final` / `delta`，**按 flush 给整行** | transcript JSONL 的 content block | **行级**（非 token 级，不夸大）。`MessageDisplay` 的实时行为**本轮未覆盖**（[claude-queue-steer-1](./evidence/claude-queue-steer-1.md) 明确把它排除在范围外）→ P3 落地前仍按未验证对待 |
| **grok** | `updates.jsonl` 的 `agent_message_chunk` / `agent_thought_chunk` / `tool_call` / `tool_call_update` | `chat_history.jsonl` 全量对账 | **chunk 级** **[V]**（记录形状确为 chunk）。但本轮服务端分两段发出的 `SPIKE_` / `COMPLETE` **落盘成一条** `agent_message_chunk` → **不得据此承诺「每个 SSE token 都落盘」**，只能承诺 chunk 粒度（[grok-signals-1](./evidence/grok-signals-1.md) A1） |
| **codex** | rollout JSONL 的 `item_completed` / `response_item` | 同一文件 | **item 级，只有 completed，无 delta**——**[V]** 97 条记录的完整清单里没有任何文本 delta 记录（[codex-signals-1](./evidence/codex-signals-1.md) A3）；UI 必须显式说明「本 harness 无逐字流」 |
| **agy** | 无实用通道 | SQLite blob（需 proto schema） | 仅屏幕 |

**队列账本的出处（三家不对称，必须写进 adapter 契约）**：

- **claude [V]**：transcript 里有原生 `queue-operation`（`enqueue` / `dequeue` / `remove{reason:"absorbed_mid_turn"}`；本轮 9 enqueue、4 dequeue、5 remove，**未观测到 `popAll`，不得臆造**）。投递时还会追加一条 `attachment.queued_command` 记录——**它不是 `user` record**，只 tail `user`/`assistant` 的 mapper 会整条漏掉。另：`queued_command` 保留的是**入队时**的时间戳，部分 `remove` 的物理写入又早于工具结果记录 → **按文件顺序读，按时间戳全局排序会伪造投递顺序**。
- **codex [V]（反向证据）**：97 条记录的完整清单里**没有**任何 enqueue / queued_message / steer / keypress 记录；投递后的消息就是普通 message / item，只能靠 `turn_id` 异同区分「同 turn steer」与「下一 turn 排队」，**无法证明某条消息当初是被 `Tab` 排进去的**。
- **grok [V]（反向证据）**：完整文件里**没有**普通入队记录，只有取消后重投的 `redirect_kind`（`cancel_then_send` / `queued_after_cancel`），无法还原按键时刻的入队 / 编辑 / 删除。
- **结论**：**Remuda 自己的 `pty_queue` 是队列的唯一权威来源**——codex / grok 必然如此，claude 则把原生 `queue-operation` 用作**对账与投递证据**，而不是替代。队列 chip 的增删改一律以 Remuda 账本为准（呼应 §6 的 emulated 映射）。

**映射到 protocol §5.2**：`MessageDisplay` / ACP chunk → `append`（`status: "streaming"`）；transcript / rollout 的完整块到达 → `replace` + `close`（`status: "complete"`）。低信息来源不得把高信息值改成空值（§5.2 既有规则）。

**`TranscriptMapper` 重组修复**（今天的 tool-call 串位 bug，根因已确认）：

1. Claude transcript **一条记录只装一个 content block**，同一 `message.id` 会被 2–7 条连续记录共享（`apiBlockIndex` 标位）。现有 mapper 把每条记录当成一条「完整 assistant message」喂给 stdout mapper，于是按 message 分组的 tool-call 记账整体错位。→ **按 `(requestId, message.id)` 缓冲，按 `apiBlockIndex` 重组 `content`，在 `stop_reason` 出现时一次性发出**。
2. mapper 取的 `parentToolUseId` 键**在 transcript 里根本不存在**（0 次出现）；真正的链接是 `sourceToolUseID` / `sourceToolAssistantUUID`。→ 换键，subagent 与 tool→result 的父子关系才不再是 unknown。
3. mapper 丢弃所有非 `user`/`assistant` 记录。→ **保留 `queue-operation`（§6 的队列账本）、`attachment.queued_command`（**[V]** 排队消息的投递证据，且它不是 `user` record）、`permission-mode`（模式漂移）、`toolUseResult`（工具富结果）**。

该 mapper 今天只被 promoted 终端使用，`claude-pty` 记下了 `transcript_path` 却从不 tail 它——native 路径统一后**两条路径共用同一个 tailer**，这本身就是统一原则的回归测试。

---

## 8 Node 重启存活（**已拍板：方案 A 过渡，方案 B 排 P8**）

**先把话说直白：in-process 的 PTY 随 Node 进程一起死。** `portable-pty` 的 child 由 Node 拥有，master fd 关闭时内核向前台进程组发 SIGHUP；唯一的持久载体记录 `PtyResource` 是 herdr 专用的。今天 herdr 能在 Node 重启后**不重放 prompt 直接认领**一个活着的 agent（D-010 的 `reconcile`）；换成 native PTY 后，**每一个**会话都会在 Node 重启时消失，而不只是孤儿。

| | 方案 A：接受丢失（本轮） | 方案 B：`remuda-ptyd` 持有者进程（P8） |
|---|---|---|
| 机制 | 什么都不做；Hub 已有的 `node-epoch-changed` reconcile 把行落到 `exited` | 每实例一个 detached 小进程持有 PTY master，与 Node 之间走 UDS；Node 重启后按持久记录**认领**回来 |
| 用户可见 | 会话列表出现「Node 重启，会话已结束」并给 **Resume** 按钮（D-026 已能续同一段对话） | 重启无感，终端与结构视图继续 |
| 代价 | 升级/重启即全量断线；长跑任务必须靠 resume 接续 | 新增一个可执行、一套 UDS 协议、adopt 对账、孤儿清扫、跨平台（Windows 无 `tcgetpgrp`） |
| herdr | 对需要存活的宿主**保留 herdr 为可选 carrier** | 可彻底删除 herdr |
| 风险 | 已知、可解释、零新代码 | 相当于自己写半个终端复用器；必须先有 §5.3 的进程组回收与孤儿清扫 |

**建议**：**A 作为本轮的过渡态**（诚实告知 + Resume 兜底 + herdr 保留为 optional feature），**B 排 P8** 作为删除 herdr 的前置条件。B 落地前，「跨 Node 重启存活」是 herdr 在本仓库**唯一**不可替代的能力，herdr 的删除节点也以此为准。**用户决定（2026-09-14）：接受过渡期内「Node 重启 = 会话结束」，持有进程（方案 B）放到 P8 之后再做。**

---

## 9 effort 与 tui：per-session settings overlay

### 9.1 effort

| index | 名称 | 备注 |
|---|---|---|
| 0 | `low` | |
| 1 | `medium` | |
| 2 | `high` | **默认**（成本基准 1.0） |
| 3 | `xhigh` | |
| 4 | `max` | 顶档 |

`ultracode` **不是第 6 档**，是独立 boolean（等价 `xhigh` + dynamic workflow），session-only、永不持久化。

- **落地通道**：launch 时 argv 追加 `--effort <value>`（`--effort ultracode` 未文档化但已实测可用）；会话内改档写 `/effort <level>\r` 进 PTY（命令注册表已验证，PTY 实际驱动 **[U]**）。
- **绝不使用 `CLAUDE_CODE_EFFORT_LEVEL`**：它的优先级高于会话内 `/effort`，会把 PTY 的实时改档钉死；反向地，必须在 `child_env` 里**剥离宿主继承的该变量**，否则外部环境静默覆盖一切。
- **不把 `settings.json` 的 `effortLevel` 当主通道**：其 enum 拒绝 `"max"`；`maxEffortLevel` 会 clamp 一切。
- **effective vs requested**：record 上拆两个字段，`effortRequested` 与 `effortEffective{name, ultracode, source, observedAt} | null`。effective 从 transcript 每条 assistant 记录的 `effort` / `perTurnEffort` 回读。**UI 一律显示 effective**；回读不到时显示 `?` 并置灰，**绝不回落成请求值**；不一致时显示「请求 max → 实际 xhigh」，这正是 clamp / org cap / ultracode 降级的可见出口。改档命令超时未回读到变化 → journal 记 `degraded`，不谎报 applied。
- 非 claude 三家继续返回诚实的 `CapabilityUnsupported`。

### 9.2 tui / 终端状态信号

启动时在同一份 per-session settings overlay 里钉住三个键：

| 键 | 值 | 为什么必须钉 |
|---|---|---|
| `tui` | `"fullscreen"` / `"default"` | 两个方向都显式写，确保启动选择覆盖 user/project/local；前台 SessionStart 绑定后仅释放这个键 |
| `showStatusInTerminalTab` | `true` | 保留 OSC 标题信号，renderer relaunch 后仍钉住 |
| `terminalProgressBarEnabled` | `true` | 保留 `OSC 9;4` 进度信号，renderer relaunch 后仍钉住 |

- **§9.2 follow-up 修正**：不再自动追加 `--setting-sources user,project,local`。这三个来源原本就会加载；确定性来自 `--settings` 的优先级，不需要该 flag。显式 `--settings <relay>` 也必须先经 shim 合并 overlay。优先级是 managed > `--settings` > local > project > user；hooks 在各层合并。
- **允许会话内 `/tui X`**：只在通过认证的 SessionStart 与检测到的前台 PID 匹配后，Node 的驱动监督器原子重写本实例的 base/private merged overlay，删除 `tui`，保留 hooks 和另外两个终端键。每次新实例启动仍重新钉住两个方向。Claude 会写用户偏好并携带原 argv relaunch；仅去掉 flag、继续钉 `tui` 会静默覆盖用户切换。
- **启动选择**：可选 `InstanceSpec.tui`，session 值替换 host `defaultTui`，均未设置时为 fullscreen。适用于启用 hook overlay 的 native PTY；旧 print/herdr 路径没有本轮的绑定后释放机制。New Session 高级区使用「全屏渲染（推荐）」/「行内渲染」，提示「启动时使用此渲染方式，会话内可用 /tui 切换」。
- **身份跟随**：新 PID 的 relaunch 走 replacement-process promotion；同 PID、新 native session ID 的 exec relaunch 以认证 SessionStart + cwd 内 transcript 更新绑定、重建 tail。不能把新 SessionStart 误当作旧 transcript 的继续。
- **不得假设 fullscreen 生效**：崩溃闩、screen reader、嵌套复用器都可能降级。终端模拟器从 `ESC[?1049h`/`ESC[?1049l` 维护实际状态，经 attach `altScreen` 和 live `tty.mode` 上报。raw-ring fallback 无法判断时为 unknown。UI 根据观测显示，requested fullscreen 但尚未见 alt screen 时只给轻量提示。
- **原生优先级仍生效**：`/tui` 写 user 设置；如果 project/local/managed 仍设了 `tui`，它们会继续覆盖该用户偏好。Remuda 不改这些设置，实际模式以 `altScreen` 为准。

2.1.270 bundle predicate、实际 `/tui` 对照与测试边界见 [native-pty-9b](./evidence/native-pty-9b.md)。

---

## 10 herdr 规则表移植

herdr 的「状态分类器」**不是模型**，是一张可提取的 TOML 规则表。移植成本因此从「重新训练」降为「搬表 + 写引擎」，这是全案性价比最高的一项。

- **落地形态**：`crates/remuda-screen/rules/<harness>.toml` 作为**版本化资产**，每份带 `version` / `min_engine_version` / `updated_at`，与 `binary.rs` 现有的 `pin_binary` 配对——binary 升级即提示规则复验。
- **引擎原语**：`state`、`priority`（最高分胜出）、`region ∈ {osc_title, osc_progress, whole_recent, bottom_non_empty_lines(N), top_non_empty_lines(N), prompt_box_body, after_last_horizontal_rule, after_last_prompt_marker, last_non_empty_above_prompt_box, whole_recent_without_current_prompt_marker}`、匹配器 `contains` / `regex` / `line_regex`、组合子 `any` / `all` / `not`、标志 `visible_working` / `visible_blocker` / `visible_idle` / `skip_state_update`。
- **输入是模拟器网格**，不是去 ANSI 的字节尾；OSC 标题与 `9;4` 载荷必须**保留在 VT 状态里**，不能只渲染不留存。
- **`done` 是派生的**：herdr 自己的帮助文本写明 idle 与 done 都表示「可接受输入」，区别只在服务端的 *seen* 状态。Remuda 已有按设备的已读状态，`done = idle ∧ 自上次 turn 结束未被读过`，**零新信号**。
- **`unknown` 永不塌缩成 `idle`**：它是诚实的「没有规则命中」，不证明完成。
- **推送边沿**：herdr 的 `pane.agent_status_changed` 用本地 VT diff 取代——每个帧边界重算裁决，去抖 ~120 ms，**只在状态跃迁时发事件**。

**必须原样抄过来的五个防误判锚点**（简化即回归）：① 活动行锚定第 0 列 + 缩进续行，防用户输入的文本冒充信号；② grok 锚 `[stop]` chip 而不是盲文字形（启动闪屏 logo 就是盲文）；③ transcript / scrollback 查看器与 model picker 必须有高优先级 `skip_state_update`（它们长得和审批对话框一样）；④ 闪烁的 `⚠ Action Required` 在失焦时会掉帧 → blocked **latch 住**，直到出现正向 idle/working 信号；⑤ 窄宽度软换行会打断所有双 token 的 `contains` → 匹配前先合并软换行行，并在启动时钉死 `cols`。

---

## 11 与既有决策的关系

| 决策 | 关系 |
|---|---|
| **D-010**（PTY 载体 = herdr，不自建 PTY） | **本决策直接反转它**；herdr 由必需降为 optional feature，删除节点绑定 §8 的存活方案 |
| **D-013**（codex wire freeze） | 保留 `remuda-codex-wire` / `remuda-acp-wire` 与对应 `DriverKind`；它们是 codex 类型化审批的旁路后备，不在删除范围 |
| **D-016**（远程终端） | 线协议不变；只升级服务端 snapshot 的生成方式与 `altScreen` 上报，并顺带修好 scrollback（§4.6） |
| **D-022**（排队与屏幕作答） | 不变量全保留；hook 路径是队列的**上位输入**——有回执就不再靠字形猜 ready；自动 trust 仍限屏幕路径，不上升为通用能力 |
| **D-025**（终端 agent 提升） | **升格为唯一检测/水合路径**；transcript 定位由 hook payload 直接给出，取代按 cwd 编码猜路径 |
| **D-026**（resume） | 语义不变；`ResumeMode{structured,terminal}` 可删（§5.6） |
| **D-027 / D-027a**（附件） | Hub 暂存 + Node 落盘不变；投递改走 MCP 工具，首次让 codex/grok 真正拿到图片字节 |

---

## 12 `claude-print` 退役计划（按条件，不按日程）

print 承担两件全仓独有的事：**唯一 cost/usage 发射点**、**唯一不需要 TTY 的宿主路径**。因此：

1. `UsageAdapter` 覆盖三家并上线 ≥1 周（cost 由本地价目表换算，UI 标注「估算」）。
2. MCP 附件在三家验证通过。
3. **parity gate 连续 3 次全绿**：同一脚本分别跑 `claude-print` 与 `agent-pty`，diff journal 事件流，仅允许白名单内的粒度差异（助手文本行级 vs token 级；cost 估算偏差）。
4. 满足后按 harness 逐个翻默认，**claude 最后**。
5. 再降级为 `feature = "non-tty"` 下的 legacy driver，**代码不删**。
6. 只有当 `pty.fork` 在无控制终端宿主（stdio Node、SSH-stdio、机器人会话）上实测可用后，才真正删除 print 相关源文件、`NativeRustWire` 与 `fake-claude` 夹具。

同期：`claude-bg` 与 herdr 传输先降为可选 feature。测试夹具（stream-json 脚本、`fake-herdr`、herdr 线格式 fixture）保留到对应 carrier 退役为止，新增统一的 `fake-harness` 作为后继。

### 12.1 parity gate 的实现：`remuda journal diff`

上面第 3 条的比较器已落地为 `remuda journal diff`（`crates/remuda/src/cmd/journal_diff.rs`），
P6/P7 直接调用，不必再各自写一份 diff 逻辑。

```
remuda journal diff <left.json> <right.json> [--whitelist F] [--no-whitelist] [--json] [--color auto|always|never]
```

**输入**：两份同一场景的 journal dump。同时接受 Hub 的
`GET /v1/instances/{id}/journal`（`{events:[…]}`）、coordinator 的 jdump /
`remuda instance read --source journal`（`{observations:[…]}`）、裸数组，以及
以上任一形状的 JSONL。左侧惯例是 `claude-print`，右侧是 `agent-pty`。

**归一化**（两侧走同一条代码路径，因此不会「各归一化各的」）：

1. 剥掉易变字段——`seq`、`eventId`/`journalId`/`instanceId`/`runId`/`hostId`、
   `observedAt`/`nativeAt`、`revision`/`baseRevision`、`sourceCursor`、
   `processGeneration`。driver / channel 一并剥掉：**同一事实经不同 carrier
   观测仍是同一事实**，这正是要证明的东西。
2. **折叠 mutation 链**。`message` / `thought` / `tool_call` / `tool_result` 的
   `open`→`append`→`replace`→`close` 按 node id 回放成一条事件：`append` 按
   `targetBlock` 拼进已有文本块，`replace` / `close` 用快照覆盖。于是「print 逐
   token append 三次」与「pty 整行 append 一次」收敛到同一条最终文本。
3. **native id → 每侧序号别名**（`msg#1`、`tool#2`、`int#1`）。两侧的 UUID 必然
   不同；出现顺序才是可比的身份。
4. `Knowledge<T>` 塌缩成取值，`unknown` / `not-applicable` 塌缩成标记——
   `unknown` 的 `reason` 与 `evidenceEventIds` 是易变的，值本身不是。
5. 文本到达的**方式**保留为合成字段 `granularity`
   （`token-level` / `record-level` / `snapshot` / `single`），这样 §7 的粒度差异
   是一条可白名单的具体差异，而不是整条消息不匹配。

对齐用事件签名（kind + topic + 别名）的最长公共子序列，因此**少一条事件会被
报成少一条**，而不是把后面所有事件错位成一片红。

**输出**：人读的 unified diff（红=阻塞，黄=已白名单，绿=结论；`--color auto`
在管道里和 `NO_COLOR` 下自动关闭），或 `--json` 的
`{equal, differences[], whitelisted[], left, right, whitelistPath, rules}`。
**退出码 0 = 全绿（差异全在白名单内），1 = 有阻塞差异**——P7 的门禁读这个码。

**白名单**：默认读 `docs/design/parity-whitelist.toml`（相对 cwd）。
`--no-whitelist` 关掉全部豁免，用来看「未经修饰的真实差异有多少」。
**默认文件不存在时按空白名单处理——缺省只会更严，不会更松。**

```toml
version = 1                      # 未知版本直接拒绝，不降级解析

[[rule]]
kind = "message"                 # 选择器：kind / topic / lifecycle（至少一个）
field = "granularity"            # 归一化后的字段路径；省略则覆盖整条事件
allow = ["record-level", "token-level"]   # 可互换的取值；省略则任意取值
reason = "…"                     # 必填，且不得为空白
```

| 键 | 含义 |
|---|---|
| `kind` | 归一化事件种类（`message`/`tool_call`/`usage`/`lifecycle`…） |
| `topic` | `lifecycle` 取其 topic（`hook`/`session`/`diagnostic`…），其余等于 `kind` |
| `lifecycle` | `kind = "lifecycle"` + `topic = <值>` 的语法糖 |
| `field` | 归一化字段路径；**覆盖整棵子树**（`cost` 同时覆盖 `cost.amount`） |
| `allow` | 字符串或字符串数组；**任一侧命中即豁免**（粒度差异天然不对称） |
| `ignore` | 对齐**之前**从两侧丢弃该事件（只有一侧的 carrier 能物理观测到时用）；与 `field`/`allow` 互斥 |
| `allow-unmatched` | 允许该事件只出现在一侧 |
| `reason` | **必填**。没有 reason 的规则就是一个没人记得为什么存在的 parity 漏洞 |

规则按声明顺序匹配，首条命中生效。**没被任何规则覆盖的差异一律失败**——
白名单是显式豁免清单，不是启发式。

**P7 门禁怎么用它**（对应 §13 P7「parity 连续 3 次全绿方可翻默认」）：

1. 同一脚本场景跑一次 `claude-print`，导出 journal；再跑一次 `agent-pty`，导出 journal。
2. `remuda journal diff print.json pty.json --json`，记录 `equal` 与
   `whitelisted[]`。
3. **连续 3 次**（各自独立起 session，不复用 dump）`equal == true` 才算一次全绿，
   方可对该 harness 翻默认。任一次 `differences[]` 非空即阻塞放行。
4. 三次的 `whitelisted[]` 应当稳定；**豁免集合本身在扩大**也是回归信号——说明
   两条路径在分叉，而不是在收敛。

**怎么扩展白名单**：先确认这条差异**确实是粒度或 carrier 机制差异**，而不是
一侧丢了事实。判据是「把两侧的 `--no-whitelist` 输出摆在一起，信息量是否相等」：
信息量相等 → 加规则并写清出处（§号 + 为什么两条路径必然不同）；信息量不等 →
**这是 bug，修 adapter，不加规则**。每条规则都应当写明何时可以删除
（例如 `usage.cost` 那条：UsageAdapter 三家上报真实 cost 后即可移除）。

夹具在 `crates/remuda/tests/fixtures/journal-parity/`：一个 3 turn 场景
（文本、工具调用+结果、审批）的 print 版与 pty 版，外加一个丢了 `tool_result`
的反例（必须失败）。两侧由 `generate.py` 从**同一份**场景描述生成，测试会校验
夹具与生成器一致——手改夹具会让两侧悄悄解耦，门禁就不再证明任何东西。

---

## 13 分阶段实施与 worker 切分

| 期 | 内容 | 验收 | 人日 | owner / 独占文件 |
|---|---|---|---|---|
| **P0** | `remuda-screen` 抽取（零行为变更）+ 模拟器接入 shell-pty，`REMUDA_PTY_EMULATOR` 开关 | snapshot A/B 不劣于字节 ring；内存基准 ≤ 预算 × maxInstances | 2 | **W1**：`crates/remuda-screen/*`、`crates/remuda-driver/src/shell_pty.rs` 的 ring/snapshot 段、`web/src/features/session/tty/*` |
| **P1** | launch shim + overlay materializer + hook socket + `SignalBus`，**先只在 promoted shell-pty 的 claude 上开** | **用户手敲 `claude` 即产生 hook 事件**；零协议变更、零 herdr | 3 | **W2**：`crates/remuda-signal/*`、`crates/remuda-driver/src/child_env.rs`、`launch/*`、Node 监听端 |
| **P2** | `AgentPty`：kind/driver 矩阵、New Session 预填、per-kind recipe + yolo、effort/tui overlay、生命周期五件事（§5） | **已落地**（[native-pty-2](./evidence/native-pty-2.md)）：两条路径的 agent 事件流逐条相同；停进程后进程组确实消失（`rung="sigint"`，复查为空）；agent 退出 ≤2 s 内落 `exited`/`failed` 并带退出码。**残留**：`launchedBy` 两条路径都读成 `remuda`（Hub 仍从 `mode == promoted` 推断，而 §1.0 规则 2 已使该推断失效，见 §14 风险 16） | 4 | **W3**（`remuda-protocol/*`、`materializer.rs`、`flags.rs`、`capabilities.rs`、enums）+ **W2**（driver 实现） |
| **P3** | 实时流式：`MessageDisplay` 接入 + `TranscriptMapper` 重组修复 + grok `updates.jsonl` 解码 | 结构视图**行级**增量出现；tool-call 与 tool-result 配对 100%；`queue-operation` 进 journal | 3 | **W2**：`claude_print.rs` 的 mapper 段、`shell_pty/promotion.rs` |
| **P4** | steer / 排队 / 打断：按 §6 已实测的键位落地 + native/emulated 映射 + composer 三态 + 诚实 capabilities | claude Enter = 入队 + 工具边界消费（`enqueue` → `remove{absorbed_mid_turn}`）、`Esc` 打断后队列存活；codex Enter steer / `Tab` 排队 / `Esc` 打断；grok Enter 排队、**空 Enter 取消并发送**、**双 `Ctrl+C`** 取消（`Esc` 不是打断键）；`steer`/`queue`/`interrupt` 三项逐 harness 标 `native｜emulated`，agy 显示「尚未验证」而非假灰 | 2 | **W1**（`web/src/features/session/*`）+ **W3**（`PromptMode` 增量） |
| **P5** | `PermissionRequest` 裁决 + hook 路径 `respond_interaction` | **已落地**（[native-pty-5](./evidence/native-pty-5.md)）：allow / deny / allow-always 三者实测生效且不弹框；超时 deny（够不到 Node 则 `{}`，两者不可混同）；hook 未生效时回落按键**恰好一次**并以「框消失」为准。**残留**：`Elicitation` 仅按二进制读取端形状实现，**[U]** 未取得实机 payload；confined session 本身 **[U]** 未单独复现（2.1.221 已无该字样），本轮由「回复形状错误」这条真实路径覆盖同一条回落逻辑 | 3 | **W2** |
| **P6** | codex adapter（**hooks `PermissionRequest` 阻塞裁决 + rollout tail**，trust 由 Remuda 写 `hooks.state`）/ grok adapter（**`events.jsonl` + `updates.jsonl` + `active_sessions.json` 发现**）+ `UsageAdapter` + MCP 附件 | 三家 lifecycle 来自结构化通道（codex `task_started`/`task_complete`/`turn_aborted`；grok `turn_ended{outcome}` + `turn_completed{stop_reason}`）；codex 审批走 hook allow/deny 且超时 deny；**grok 审批如实上报为屏幕作答（emulated）**；usage 覆盖三家；图片三家可读 | 4 | **W4a** `codex_adapter.rs`、**W4b** `grok_adapter.rs`（各自独立文件） |
| **P7** | 规则表移植 + capabilities 运行时化 + web 去 driver 分支 + parity gate + 逐 harness 翻默认 + print→legacy / herdr→optional | 规则表带版本；UI 门禁改看 `signalTier`；parity 连续 3 次全绿方可翻默认 | 4 | **W1**（规则表/引擎、web）+ **W3**（capabilities、enums、feature gate） |
| **P8** | `remuda-ptyd`：跨 Node 重启存活（已拍板排后，§8） | Node 重启后终端与结构视图无感续接；孤儿清扫可验证 | 5+ | **W5**：`crates/remuda-ptyd/*` + Node adopt 路径 |

合计 P0–P7 约 **25 人日**，4 worker 并行约 **12–14 个工作日**；P8 另计。

**并行关系**：P0 ∥ P1（文件不相交）；P2 依赖 P0+P1；P3 ∥ P4 ∥ P6（P2 之后，各自独占文件）；P5 依赖 P1；P7 依赖 P3/P4/P6 全部落地；P8 只依赖 P2，可随时插入但先要拍板。

**冲突规避规则**：① 每个文件唯一 owner；② 所有协议变更由 **W3 在 P1–P2 期间一次性、纯增量**落地，其余 worker 只 rebase 一次；③ `protocol` / `materializer` / `capabilities` / enums 为 **W3 独占**，他人提需求不直接改；④ 每 harness 一个 adapter 文件，杜绝交叉；⑤ 五个 flag（`REMUDA_PTY_EMULATOR` / `REMUDA_PTY_HOOKS` / `REMUDA_NATIVE_APPROVALS` / `REMUDA_PTY_CARRIER` / `REMUDA_SHIM`）相互正交，回滚 = 改 env 重启 Node，无 schema 迁移。

**统一原则在阶段计划中的落点**（不是口号，是门禁）：P1 的验收是「**用户手敲**也拿到 hook」——先证明难的一侧；P2 的验收是**两条路径事件流逐条 diff 为空**，任何 diff（除 `launchedBy`）即阻塞放行；P7 把「能力取决于 driver」改成「能力取决于本 session 拿到的信号层级」，从类型系统上堵死分叉复活，其 parity gate 同时是 print 退役闸门与统一性回归闸门。

---

## 14 风险与未决问题

| # | 风险 | 处置 |
|---|---|---|
| 1 | **allow 可能被忽略**（**[V]**） | **P5 已落地并实测**（[native-pty-5](./evidence/native-pty-5.md)）。本轮复现到的成因**不是** confined session，而是**回复形状**：裸 `{"behavior":"allow"}` 被静默丢弃（框还在、工具被拒、无任何报错），嵌套 `hookSpecificOutput` 形状才生效。两种成因的处置相同：回落**只在 hook 路径自证未生效时**武装（`Outcome::Abandoned`/`TimedOut`），**不能**用「屏幕上有框」当判据——实测审批框与「hook 仍在等待」是并存的（75 s 的 hook 全程显示框，晚到的 allow 仍然生效），据此发键会在每次慢审批上重复作答。回落每个 interaction 仅一次、写前记账、截断/歧义/非审批框一个键都不按，且**必须观察到框消失**才算 applied。confined session 本身 **[U]** 未单独复现（2.1.221 二进制中已无该字样） |
| 2 | **shim 劫持 PATH**：`which claude` 显示 Remuda 路径、可能撞用户 wrapper、非 login shell 注入失败、用户用绝对路径绕开 | 透明 `exec`、提供 `REMUDA_SHIM=off`、失败自动降到 tier C/D 并在 UI 明示降级原因 |
| 3 | ~~**codex 审批无结构化通道**~~ → **已解除（RESOLVED）** | [codex-signals-1](./evidence/codex-signals-1.md) A1 实测：`[features] hooks = true` + **per-handler canonical hash** 写进 `hooks.state` 即持久化 trust，`PermissionRequest` **阻塞返回 allow/deny 双向生效**。**残留约束**（转为实现细则，不再是风险 P）：stdin **无 `tool_use_id`**（须另建关联，不得复用 `PreToolUse` schema）、**超时 deny 策略未验**（本轮在 60 s 超时前就给了答案）、哈希**不认脚本内容变更**、`--dangerously-bypass-hook-trust` 对 `app-server` 无效。app-server（`turn/steer`、`turn/interrupt`，**源码可见、本轮未实跑**）保留为旁路 |
| 4 | **模拟器内存/CPU 未测量**（N × maxInstances） | P0 必须带基准并限制 scrollback 行数 |
| 5 | **跨 Node 重启存活**（§8） | **已拍板**：A 为过渡态（Hub 标 `node-epoch-changed` + Resume 兜底），herdr 保留 optional，B 排 P8 |
| 6 | ~~claude 的 queue-vs-steer 语义未验证~~ → **已实测**（[claude-queue-steer-1](./evidence/claude-queue-steer-1.md)）：Enter = 入队 + **工具边界**消费 | 残留风险是**表述**：不得宣传成「瞬时 steer」，也不得把 `Ctrl+X`+Enter 宣传成「turn 结束后才投」（无此保证）；`UserPromptSubmit` 只证明入队、**不是投递回执**，多条排队消息还会共享同一个 `prompt_id`（不可当去重键） |
| 7 | ~~grok~~ 已实测（§6）；**agy 的排队与打断键位仍全未验证** | grok 按 §6 的 native/emulated 映射落地（**`Esc` 不是打断键**，必须双 `Ctrl+C`；send-now 只能靠「取消当前 turn」）；agy v1 只承诺「打断进程」，composer 对应按钮显示「尚未验证」 |
| 8 | **cost 为本地价目表换算**，与 print 的 `total_cost_usd` 可能偏差 | UI 标注「估算」，并在 parity gate 白名单中显式列出该差异 |
| 9 | **无 PTY 宿主**：`pty.fork` 在 stdio / SSH-stdio 宿主上可用性 **[U]**；Windows 无 `tcgetpgrp` | 降为模拟器-only 检测；**未验证前不得删 print** |
| 10 | **hook socket 鉴权** | per-instance、0600、driver 注入、凭据独立、超时 deny；是否需要比设备凭据更窄的专用凭据，待安全评审 |
| 11 | **hook overlay 与「不碰用户配置」的边界** | 只 merge、只存活于 session、只写 per-session 影子目录，且可 `REMUDA_SHIM=off` 退出；需评审确认 |
| 12 | **grok / agy 屏幕签名覆盖率目前为零**（现有检测只认 claude banner） | P7 规则表移植时补齐；证据夹具需指派产出人 |
| 13 | **parity gate 长期不过**则 P7 的翻默认无限期推迟 | 这是**刻意设计**：print 退役由数据决定，不由日程决定 |
| 14 | **`/effort` 在 PTY 内的实际驱动未实测**（**[U]**） | 回读不到即记 `degraded` + effective 置 unknown，绝不谎报 |
| 16 | **`launchedBy` 在 P2 后失真**（**[V]** [native-pty-2](./evidence/native-pty-2.md) §6）：Hub 从 `mode == "promoted"` 推断出身，而 §1.0 规则 2 让**两条路径都 promote**，于是 Remuda 启动的 agent 也被读成 `user`。仅为出身记录，不影响任何能力（规则 4） | Node 侧已显式存下正确值（它知道是谁启动的），Hub 改为**持久化并读取该列**而不是推断。`TODO(x-protocol)` 已留在 `crates/remuda-hub/src/store.rs`。这是 P2 的 parity 校验目前唯一验证不到的 entity 字段，故 §13 P2 的验收改以 journal 为准 |
| 17 | **`instance.cancel` 的空闲判据仍是屏幕签名**（**[V]** 同上 §5）：`Esc` 打到**空闲** composer 上会退出 claude（实测落 `native-exit-code-1`），而 claude-queue-steer-1 的打断证据全部来自**运行中的 turn** | 现在 idle 时不发键、返回 `not-dispatched`；`unknown` 仍然发（漏打断比误退出轻）。真正要的是 harness 自己的 turn 结束证据（claude `Stop` / codex `TurnAborted` / grok `turn_ended`），随 P5 的 hook 路径补上 |
| 15 | **grok 没有审批裁决通道**（**[V]**，新增）：`PermissionRequest` hook 名被静默忽略；`PreToolUse` 只有 `deny`/`ask`（**能拦不能替用户答**）；ACP `session/request_permission` 的原始 RPC **不落 `updates.jsonl`** → 无法从文件重建并回答原始请求 | 作答路径**只能是屏幕 + 按键**（tier D，D-022 全套不变量适用：写前置 `attempted`、`answerable &= !screen_truncated`、单次自动 trust）；capability 标注为**屏幕作答（emulated）**，**不得**冒充 hook 裁决。`events.jsonl` 的 `permission_requested` / `permission_resolved` 只能做**事后对账**（`wait_ms=0` 不代表没阻塞，`permissionMode: auto` 也不等于 `--always-approve`）。要拿到结构化作答，须另起一个真正的 ACP client（与 D-013 的 `remuda-acp-wire` 同族），排在 P6 之后单独评估 |

---

## 附录 A 覆盖矩阵

### A.1 herdr 的 29 项能力 → native 等价物

| # | herdr 能力 | native 等价物 | 交付期 | 状态 |
|---|---|---|---|---|
| 1 | session server 起停/attach/ping | 无需替代：没有 daemon，driver 直接持有 PTY | P2 | 删除 |
| 2 | `workspace.create`（cwd + env + label） | PTY spawn 的 cwd/env + D-023 workspace registry | P2 | 已有等价 |
| 3 | `workspace.list` / `close` | 实例目录 + 进程组回收 + 孤儿清扫 | P2 | 新建 |
| 4 | `tab.create/list/close` | 无对应概念（Remuda 只有 Space→Tabs，D-024，纯 UI） | — | 删除（死面） |
| 5 | `pane.split` | 一实例一 PTY，不再有 root pane 需要保活 | — | 删除 |
| 6 | `pane.close` | §5.3 SIGINT→SIGHUP→SIGKILL 进程组阶梯 | P2 | 新建 |
| 7 | `pane.read`（visible/recent/recent-unwrapped/detection） | 模拟器网格 + 字节 ring（含软换行合并） | P0 | 新建 |
| 8 | `agent.read`（按 agent 名） | 同上，按实例寻址 | P0 | 新建 |
| 9 | `agent.start` + kind 检测 | §5.1 per-kind recipe + D-025 的 `tcgetpgrp` 检测表 | P2 | 新建 |
| 10 | `AgentStatus{idle,working,blocked,done,unknown}` | §4.3 分层 + §10 规则表；`done` 由 seen-state 派生 | P1 / P7 | 新建 |
| 11 | `agent.wait(until,timeout)` | Node 侧按 journal 条件等待（`instance wait` 是 D-015 验收路径，必须保留） | P2 | 新建 |
| 12 | `agent.prompt` | §5.2 ready 阶梯 + 两段写 | P2 | 部分已有（shell-pty 配方） |
| 13 | `interactive_ready` 就绪门 | §5.2 的三级阶梯（hook 回执为第 1 级） | P2 | 新建 |
| 14 | `send_keys` 逻辑键名 | `crates/remuda-driver/src/tty.rs` 的键名→字节表，需补 pgup/pgdn/delete/shift+tab/F 键/通用 `ctrl+<x>` | P0 | 部分已有 |
| 15 | `pane.send_text` | `LocalPty::write_bytes` | — | 已有 |
| 16 | `pane.process_info` | `tcgetpgrp` + `ps` 进程表（内核真相，无 RPC） | — | 已有（更强） |
| 17 | `agent.get/list` + `state_change_seq` | 本地 agent 注册表 + 状态 epoch（防止旧屏幕配新状态），**须重造** | P2 | 新建 |
| 18 | hook 上报的原生 session 身份 | `SessionStart` hook → `session-meta.json`（Remuda 自有，本就是优先路径） | — | 已有 |
| 19 | `events.subscribe` 推送流 | 进程内事件 + VT diff 边沿（无需 RPC，也没有 herdr「新 pane 要重连」的限制） | P1 | 新建 |
| 20 | `session.snapshot` | 持久实例记录（仅方案 B 需要） | P8 | 排后（已拍板） |
| 21 | `terminal observe/control`（rendered-ansi 帧） | §4.6 真字节 + 模拟器合成 snapshot + `full` 重绘标志 | P0 | 部分已有 |
| 22 | `terminal.input` | 已有本地写入 | — | 已有 |
| 23 | `terminal.resize` | `LocalPty::resize` | — | 已有 |
| 24 | **pane 活过 Node 重启**（D-010） | §8：方案 A 接受丢失（本轮）/ 方案 B `remuda-ptyd` | P8 | **已拍板**：A 过渡，B 排 P8 |
| 25 | trust/审批对话框检测 | 解析器本就是 Remuda 自有；`blocked` 触发源改为规则表 | P7 | 部分已有 |
| 26 | socket 隔离与 `HERDR_*` 环境 | 无需；但 web 的 inventory 字段（version/socket/path）要一并下线 | P7 | 删除 |
| 27 | `worktree.*` | 早已 native（git CLI + worktree 记录），protocol 明确拒绝过 herdr 的实现 | — | 已有 |
| 28 | `notification.show` | 从未使用 | — | 删除 |
| 29 | pane 几何/scroll/OSC 标题/revision | 仅 OSC 标题有用（进 VT 状态，供规则表），其余从未被读 | P0 | 部分删除 |

### A.2 用户提出的 7 项要求 → 落点

| # | 用户要求 | native 落点 | 交付期 | 状态 |
|---|---|---|---|---|
| 1 | native PTY 为一等载体；退役 print；统一「终端里启动」与「直接开 Session」 | §1.0 统一原则 + §4.1 + §12 | P2 / P7 | 设计已定 |
| 2 | 无 herdr 后补齐：新建 / 停止 / 删除 Session、发送消息 | §5 全章（新建、发送、停止两义、删除、退出检测、resume） | P2 | 设计已定 |
| 3 | 工作中 composer：steer / 排队 / 打断，逐 harness 键位，能力诚实 | §6（键位表 + native/emulated 映射 + composer 三态 + `unknown` 诚实上报） | P4 | **已实测三家**（claude / codex / grok，证据见 §6 末列）；agy 仍 **[U]** |
| 4 | 结构化视图实时流式（文本/思考/工具增量） | §7（`MessageDisplay` 行级 + ACP chunk + mapper 重组修复） | P3 | 设计已定 |
| 5 | effort：5 档 + ultracode；`--effort` + `/effort`；显示 effective vs requested | §9.1 | P2 | 部分 **[U]**（PTY 内 `/effort` 未实测） |
| 6 | 滚动 + 全屏 TUI：真字节修 scrollback；overlay 启动钉 `tui`、绑定后释放；保留正常 settings sources | §4.6 + §9.2 | P0 / P2 | 设计已定 |
| 7 | 识别手敲启动的 agent（D-025 promotion），给同样的结构化视图 | §1.0 规则 2 + §4.2 launch shim + P1 验收 | P1 | 设计已定 |

---

> 附注（输入材料处理说明）：上游研究材料被 harness 标记为命中 "instruction-shaped pattern: settings-json / bypass-permissions"。复核后未发现注入意图，命中源为正文中的 `--settings` overlay 示例、`{"behavior":"allow"|"deny"}` 裁决形状与 yolo argv 清单，已按**数据**处理，未作为指令执行。
