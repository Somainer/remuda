# 方案草案：Unified Remote Agent Runtime

> 状态：草案 v0.1（2026-09-12），等待 herdr/herdrx、DeepSeek Harness、网关三份调研补充后定稿。
> 调研报告：`docs/research/*.md`。本文只写结论与决策，证据看报告。

## 0. 一句话定位

**一个自托管的、统一的远程 agent 运行时**：在一台或多台远程主机上托管**原生** agent 进程（Claude Code 第一等；Codex / grok / agy 次级），对外提供统一的实例生命周期 API、统一的 provider / 凭据配置、富交互的 Web/PWA 遥控面，以及飞书 / Telegram 等 bot dispatcher 接入。

它**不是** harness，**不造** agent loop。Claude Code 的 loop（含 dynamic Workflow、subagent、hooks、skills、MCP）原样保留；其它 CLI 也原样跑。

## 1. 需求（来自用户，已对齐）

| # | 需求 | 备注 |
|---|---|---|
| R1 | 富交互界面（Web / PWA / Desktop），不要 CLI | 手机 + Mac 都要能用 |
| R2 | 不重写 agent loop；必须保留 Claude Code dynamic Workflow | 市面桥 codex 的产品都丢了这个 |
| R3 | 远程执行：agent 跑在远程主机，人在外面遥控 | 形态参考 herdrx |
| R4 | 主 agent 能调多种 agent；auth / key 统一管理 + 多 provider 轮换 | 可委托 astergate / newapi，但 runtime 至少支持 provider 轮换 |
| R5 | bot 作 dispatcher（飞书 / Telegram），自动把任务路由到 agent | |
| R6 | 用户追加：可接受「统一 endpoint 下全部用 Claude Code 当运行时」，Workflow 混合调模型、Claude 调 Claude | 网关模型名可用于 Workflow `agent({model})`，不可用于 subagent |
| R7 | 用户追加：定位是 unified remote agent runtime，不是 harness | 命名与架构按此 |

## 2. 已确认的关键事实（证据在 research/）

1. **Workflow 工具在 `claude -p` / Agent SDK 里可用**（实测 init 工具列表含 `Workflow`）；但 TUI 的 `/workflows` 进度面板、`ultracode` 关键字触发只在交互 TUI。`-p` 下要开 workflow 必须让模型显式调用 `Workflow` 工具。（agent-protocols.md §2.2）
2. `--bare` 将来会成为 `-p` 默认，会拆掉 hooks / skills / CLAUDE.md → runtime 的 wrapper **必须钉死不传 `--bare`**。
3. Claude Code 的结构化事件来源：hooks（stdin JSON）+ `~/.claude/projects/<cwd>/<sid>.jsonl`（append-only，可 tail）+ `subagents/workflows/wf_*/journal.jsonl`；本机 Flux Island / Orca / herdr 已经同时挂在 hooks 上，证明「PTY 保真 + hooks/jsonl 取结构」成立。
4. Claude Code 自带 `--bg` 后台会话、`claude agents/attach/logs`、每进程 messaging socket（`/tmp/cc-socks/<pid>.sock`）——正在实测（claude-control-plane.md）。
5. 官方 Remote Control 绑定 Anthropic 账号 + claude.ai UI，不能当我们的协议；只能作备选通道。
6. 网关模式：`claude --settings <overlay>`（`ANTHROPIC_BASE_URL` + `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` + `model: passthrough/…`）已在本机跑通；本机 astergate 常驻 `127.0.0.1:18481`。
7. Codex：`codex app-server`（JSON-RPC，stdio / unix / ws）是 VS Code 扩展同款控制面，含 thread/turn/approval/steer/interrupt；`codex exec --json` 一次性。grok：**原生 ACP**（`grok agent stdio|serve`）。agy：`-p --output-format stream-json`，无 ACP。
8. herdr socket API：102 方法 + 29 事件（`agent.start/prompt/wait`、`pane.read/send_keys/wait_for_output`、`events.subscribe`、`worktree.*`、`layout.apply`），protocol 22，socket `~/.config/herdr/herdr.sock`。herdrx 已证明「Go server + React PWA 远程驱动 herdr」形态可行。
9. hooks：`--settings` 是叠加合并（flagSettings 覆盖在 userSettings 之上），relay 模式完整继承 settings.json 的 hooks；`PermissionRequest` hook 可以**阻塞等待外部决策**（VibeBuddy 用 `nc -w 595 -U sock` 已验证）→ 手机/飞书远程审批可以走 hook 通道；Codex hooks 需要 `config.toml` 里 `trusted_hash` 同步；grok 是 drop-in `~/.grok/hooks/*.json`。（hooks-integrations.md）
10. DeepSeek Harness 的可借鉴点：UI 分层（React-free store + Conversation Node registry + keyed tool renderers + Markdown/Diff primitives）、session projection（snapshot + seq follow、断线补页、checkpoint watermark）、「原生会话是 resume 权威，我们的 observation journal 只负责观察与 UI」的双权威设计、Command 三态（queued / accepted / settled）、审批多设备只接受一次。**不借**它的 agent loop、workflow 引擎（那是它自己的引擎，不是 Claude Workflow 桥）、jobs/schedule/webhook 的进程内实现。（deepseek-harness.md §7、§8）
11. 本机远程主机：`devbox`、`devbox-sg(-host/-small)`、`devbox-gpu`、`devbox-small`、`lyre-devbox`、`forge-doloris`（SSH 已配）；未装 tailscale / tailcat。
12. 额度：grok 最多 > codex > claude-relay > agy。

## 3. 命名候选

定位是「托管一群原生 agent、从远处驾驭」，沿 herdr（牧人）的牧场意象：

| 候选 | 含义 | 评价 |
|---|---|---|
| **Remuda**（推荐） | 牧场上供牛仔每天挑选坐骑的马群 | 精确对应「agent 池 + 按任务挑 agent/模型」；独特、无冲突；有 Riverina 式的隐藏含义 |
| Yoke | 把多头牲口套在一起的轭；也是飞机的操纵杆 | 多 agent 协同 + 远程操纵双关；短；但 yokecd/yoke（K8s 工具）已占 |
| Corral | 围栏/圈养；动词「把牲口赶到一起」 | 与 herdr 同族、直白；略平 |
| Reins | 缰绳，harness 里握在手上的那部分 | 「远程握缰」；平实 |

建议：**Remuda**，二进制 `remuda`，仓库目录改名 `remuda`（当前 `hybrid-harness` 保留为工作名直到拍板）。

## 4. 架构（分层）

```
┌──────────────────────────────────────────────────────────────┐
│  Clients                                                     │
│  Web/PWA（手机+Mac）· Desktop 壳（可选）· 飞书 bot · Telegram bot │
└───────────────┬──────────────────────────────┬───────────────┘
                │ HTTPS/WSS（统一 API + 事件流）  │ bot 协议
┌───────────────▼──────────────────────────────▼───────────────┐
│  Hub（一个进程，部署在常驻主机/VPS）                              │
│  · 账号/设备鉴权、推送                                          │
│  · Host registry：多台远程主机的连接（SSH 反向隧道 / relay）        │
│  · Session registry：跨主机的会话索引、transcript 投影、搜索        │
│  · Provider registry：ProviderProfile + 凭据 + 轮换 + 健康        │
│  · Dispatcher：bot 消息 → 路由规则/分类模型 → 会话                 │
└───────────────┬──────────────────────────────────────────────┘
                │ mTLS / SSH 转发的内部 RPC + 事件流
┌───────────────▼──────────────────────────────────────────────┐
│  Node Agent（每台执行主机一个守护进程，单二进制）                    │
│  · Instance manager：create/send/observe/attach/stop            │
│  · Drivers：                                                    │
│      claude-pty（herdr pane 或自建 PTY）+ hooks + jsonl tail      │
│      claude-print（-p stream-json，bot/无人值守）                  │
│      codex-appserver（JSON-RPC）· grok-acp · agy-print            │
│  · Launch materializer：ProviderProfile → 临时 settings/env/HOME  │
│  · Workspace/worktree 管理、文件/diff 服务、终端（xterm）           │
└──────────────────────────────────────────────────────────────┘
```

### 4.0 两个权威（来自 DSH 报告的核心设计原则）

- **原生会话**（Claude jsonl / Codex thread / ACP session）是「继续执行 / resume」的权威；runtime 不从自己的投影重建模型请求。
- **runtime 的 observation journal**（每条带 seq、来源 driver、原生 ID、completeness）是「多设备看到相同已观测事实」的权威；UI 只从它投影。
- 控制命令（prompt / approve / cancel）带 `commandId`，分 queued → accepted(native) → settled 三态；重连只补事件，不重发可能已进入原生 agent 的输入。

### 4.1 统一实例模型（已按 review-consistency.md C3/C8 修正）

- `Instance {id, host, kind: claude|codex|grok|agy, driver, cwd, workspace/worktree?, providerProfile, nativeRef, lifecycle × activity × connectivity}`。UI 的状态点是三维状态的投影，不再用单一 `status` 字段冒充。
- **Claude 三种 driver 互斥，不混用**（来自 claude-control-plane.md 实测）：
  | driver | 启动 | 适用 | UI 视图 |
  |---|---|---|---|
  | `claude-print` | `claude -p --input-format stream-json --output-format stream-json …`（可 `--resume`） | bot / 无人值守 / 手机默认 | 仅结构化 |
  | `claude-bg` | `claude --bg --name X …`（daemon 托管，可 `attach` 唤醒，忽略 `--session-id`） | 人机、不需要 TUI 但要可 attach / SendMessage 寻址 | 结构化 + 可选 tty |
  | `claude-pty` | 交互 TUI `claude` 在 PTY 里（自建 PTY 或 herdr 承载） | 需要 `/workflows` 面板、Artifact、Remote Control | 结构化 + tty |
  统一硬约束：永不传 `--bare` / `--safe-mode` / `--no-session-persistence`；记录绝对二进制路径 + 版本。
- 次级 driver：`codex-appserver`（JSON-RPC stdio；unix 是 WS-over-UDS）、`grok-acp`（`grok agent stdio|serve`，客户端不声明 fs/terminal 让工具落在远端）、`agy-print`（MVP 不做）。
- 统一事件流（所有 driver 归一化）：`message`、`thought`、`tool_call`、`tool_result`、`interaction.requested|answered|expired`、`workflow.run|phase|member`、`lifecycle`、`usage`、`artifact`、`raw_tty`（仅 tty）、`opaque`；每条带 `completeness: structured|partial|screen-derived`。
- Claude 结构化事件来源：print 的 stdout JSONL；bg/pty 的 hooks + jsonl tail + workflow journal。**不 scrape 屏幕**。

### 4.2 「Claude 调 Claude / 混合模型」的实现路径

| 场景 | 路径 |
|---|---|
| 同 settings、换模型 / 隔离 worktree | Workflow `agent(prompt, {model, isolation})`（网关模型名可用） |
| 少量 subagent、同 provider | Agent(Task) 工具 |
| 换网关 / 换 key / 换 settings | 通过 runtime API 起新 Instance（`claude -p --settings X --session-id UUID`），结果走事件流回收 |
| 需要 Codex / grok 特有能力 | runtime API 起对应 kind 的 Instance |

主 agent 访问 runtime API 的方式：给 Claude Code 装一个 MCP server / CLI（`remuda instance create …`），让主 agent 像用 herdr skill 一样用它。

### 4.3 Provider / 凭据层（已按 gateway-auth.md 收敛）

- **网关可选（D-012）**：`delegation: none` 为默认——各 CLI 用自己的原生登录态（herdr 现状），runtime 不碰凭据；`delegation: gateway` 时把任意 Anthropic-Messages 兼容网关（astergate 只是其中一个实例，闭源，代码不绑它）作为 Claude 的 endpoint，混合模型走 Workflow；`direct` 多 key 轮换留 v2。无网关时多模型 = Codex / grok / agy 多 CLI 并行。
- runtime 保留一层**薄的 ProviderProfile**：`{name, protocol, base_url, secret_ref, models[], health, policy}`，只回答「哪个 CLI 用哪份配置接哪个 endpoint」和「endpoint 级故障切换」；只有直连 provider（无网关）模式才启用 runtime 自己的多 key 轮换。
- **物化规则**：Claude → 临时 `--settings` JSON（0600；只放 endpoint/model/helper，secret 走 env 或 `apiKeyHelper`，不要同时设 `ANTHROPIC_AUTH_TOKEN` 和 `ANTHROPIC_API_KEY`）；Codex → `$CODEX_HOME/<profile>.config.toml` 的 `model_providers`（`wire_api="responses"`，先 `supports_websockets=false`、retry=0）；grok → 隔离 `$GROK_HOME/config.toml` 的 `[model.grok-4.6] base_url`；agy → **当前不能直连 astergate**（缺 Gemini ingress），MVP 不做。
- **resume 必须重申 settings + model**：换 endpoint/key 后 `--resume` 同一 session 可续，但不指定 model 会退回默认模型。同一 session 禁止两个进程并发 resume（需要 lease；并行用 `--fork-session`）。
- **网关模式的代价（P0 待验收）**：官方 tools reference 把 Artifact 绑定订阅登录，gateway credential 会取代它；Remote Control 也不可用。实测 `-p` 即使 Max 登录也无 Artifact。→ 保留一个「订阅登录原生 profile」给需要 Artifact/RC 的会话；网关 profile 用于混合模型。
- gateway model discovery 的 picker 只保留 ID 含 `claude`/`anthropic` 的模型；`passthrough/…` 不会出现在 picker 但显式指定可用 → runtime 自己展示网关目录。
- `[1m]` 是 wire 前剥离的装饰；context 策略要单独配置（`CLAUDE_CODE_DISABLE_1M_CONTEXT` / `CLAUDE_CODE_MAX_CONTEXT_TOKENS`），不要从字符串猜。

### 4.4 远程拓扑（已按 remote-topology.md 收敛）

- **Hub**：部署在 `devbox-sg-host`（SG 宿主机，常开，已有 Docker + `deploy-caddy-1` + `deploy-astergate-1` + postgres/redis/minio），Docker 服务挂在现成 Caddy 后，**Cloudflare Tunnel** 出 443 给手机（PWA 安装、Web Push、飞书回调都需要 HTTPS）。Mac 只做开发/跳板，不当 Hub（合盖离网）。
- **Node Agent**：每台执行机一个 Rust 单二进制，**主动出站 WSS/gRPC 连 Hub**（Node 不暴露端口、不需要 Hub 持 SSH 钥匙；CN 与 SG 两个内网段互不可达，只有 Mac 同时能 SSH 到两边，所以 SSH 不能当生产控制面）。**第一台远程 Node 用 `devbox-sg`**（与 Hub 同机房；能出站 Cloudflare/Telegram/飞书；已 oauth 登录）。CN `devbox`（claude 2.1.268，无 oauth）**实测打不到 Cloudflare**，只能等 Hub 提供 CN 内网可达入口或走代理后再接入；`devbox-small` 能到 Cloudflare 可作 CN 备选；`devbox-gpu` 出站全断只能内网模型。Telegram 出站只有 SG 通，bot 侧放 Hub。
- 开发期：Mac 用 SSH + unix socket 转发直连 Node（herdrx 路线），复用 `~/.ssh/config`。
- 手机永不直连 `10.x`；官方 Remote Control 只作备选通道。
- 待修：`forge-doloris` 的 ssh config `User` 字段带注释导致 GSSAPI 失败；`devbox-sg-small` 超时。

### 4.6 多主机统领与跨主机调度（D-013，M1 主线）

- **主机接入三种传输**：`ssh-stdio`（Hub/Mac 用系统 `ssh`（honor `~/.ssh/config`、ProxyJump、ControlMaster）执行 `remuda node --stdio`，Node 协议跑在 stdio 上，无需远端开端口；Hub 可先 `scp` 推送 musl 静态二进制）、`outbound-wss`（Node 常驻，主动连 Hub；生产）、`local`（同机）。三者对 Hub 暴露同一个 Node 协议，只是 carrier 不同。
- **Host registry**：`Host {id, name, transport, labels[], online, lastSeen, cli[]{kind,path,version,auth}, herdr{version,socket}, resources, maxInstances}`；Node 上线时上报清单，Hub 缓存。
- **Placement**：`InstanceSpec.placement = {host: <id> | labels: {...} | any}`；Hub 按标签/能力（需要 claude-pty → 主机有 herdr；需要网关 profile → 主机能出站到网关）/负载选主机；不可满足时返回明确错误，不静默降级。
- **Fleet 操作**：`POST /v1/fleet/instances`：同一 spec 在 N 台主机各起一个 Instance（返回一组 instanceId），`fleet` 视图聚合状态；Command 可对 fleet 广播（cancel/send）。
- **主 agent 控制面**：`remuda` CLI 子命令 + MCP server（`instance.create/send/wait/read/stop`，均带 `--host`/`placement`），让 Claude Code 主会话（或 Workflow 里的 agent()）把子任务派到指定主机的 worker 上并回收结果。
- **一致性**：跨主机的 seq 只在各自 Instance 内单调；fleet 视图不假设跨主机因果序。

### 4.5 与现有开源产品的关系（paseo-vibekanban.md、reference-repos.md）

- **不 fork Paseo**（最接近的同类：Node daemon + Expo 原生端 + relay；Claude 走 Agent SDK 重画 chat，丢 TUI；协议未稳定；栈不合）。**不 fork vibe-kanban**（已 sunset）。**不碰 claudecodeui 源码**（AGPL）。
- **抄 Paseo 的产品形状**：多 HostProfile、permission kind=`tool|plan|question`、attention→push 抑制策略（有人在看就不推）、subagent track、`task_type=local_workflow` 观察。
- **抄 vibe-kanban 的执行器层**：Claude `-p --input-format stream-json` 双工 + stdin 控制协议 `initialize(hooks)` 注入 `PreToolUse` 审批回调（不用改 settings 文件）、`set_permission_mode`、`interrupt`；`NormalizedEntry`/`ToolStatus::PendingApproval{approval_id}` + JSON Patch 增量日志；审批分流 matcher（plan 只拦 ExitPlanMode/AskUserQuestion）。
- 借 herdr 的检测 manifests 与 herdrx 的 PWA/xterm/移动输入/push 实现（Apache-2.0 / MIT）。PTY spike（pty-driver-spike.md）已验证 Go 可直接加载 herdr 的 `claude.toml` 做状态检测，但 M0–M2 不需要；TTY 场景直接用 Node 上的 herdr server。

## 5. 技术栈（用户已拍板，见 decisions.md）

- Hub + Node：**Rust 单二进制**（`remuda hub|node|dev` 子命令；tokio + axum + tokio-tungstenite + portable-pty），Hub 用 `rust-embed` 托管前端。驱动层直接 lift vibe-kanban 执行器（Apache-2.0）、ACP Rust SDK、Codex protocol crate。
- Web/PWA：React 18 + TypeScript + Vite + xterm.js + React Router；React-free store；CSS token；lucide 图标（细节见 ui-spec.md §5）。
- Desktop：不做原生壳，PWA 安装。
- 飞书 dispatcher：**独立企业自建 app + `lark-cli event consume` 子进程**（Go 进程管理），出站用 lark-cli；只有必须收 lark-cli 没有的事件时才进程内 SDK。Telegram：M2 之后，long polling，放 Hub（SG）侧。
- 前端不引入 Cordis/Typert；不 fork Paseo/vibe-kanban；不碰 claudecodeui 源码（AGPL）。

## 6. 已拍板的决策（2026-09-12，详见 decisions.md；其余按 review-consistency.md D1–D26 的推荐执行）

| # | 问题 | 我的推荐 |
|---|---|---|
| 1 | 名字 | **Remuda** ✅ |
| 2 | 人机会话形态 | **M0 两条并行 ✅**：`claude-print` + 结构化 UI，与 `claude-pty`（herdr 承载）+ xterm 同时做 |
| 2b | 优先级（D-013） | 次级驱动停在最小可用；**多主机统领（SSH remote + Host registry + placement + fleet + 主 agent MCP）提前为 M1 主线** |
| 3 | PTY 底座 | **M0 = `claude-print`，不需要 PTY，也不依赖 herdr。需要 TTY 的 `claude-bg attach` / `claude-pty`（M3，或你要求提前）以每台 Node 上的 `herdr server` 为 PTY 载体**：`agent.start/prompt/wait/send_keys` + `events.subscribe` 管状态，`herdr terminal session observe/control` 给 xterm.js 原始 ANSI 流（herdrx 同款）。PTY spike 实测自建要 6–12 人周且要追 TUI 改版，不值；spike 代码留作对照 |
| 4 | 权限默认 | **Interaction broker 询问 ✅**：`--permission-prompts host --permission-prompt-tool stdio`，`can_use_tool` 统一审批与提问；新建会话可选 **bypass（yolo）**（D-011）；bot 永不 bypass；M0 临时 dontAsk 记债 |
| 5 | 远程拓扑 | Hub 在 `devbox-sg-host`（Docker + 现成 Caddy + Cloudflare Tunnel）；M1 第一台 Node = `devbox-sg`；CN 节点后置 |
| 6 | Provider 层 | **网关可选**（D-012）：默认原生登录态；有网关则用；runtime 只有薄 profile；不做多 key UI（v2） |
| 7 | 第一个 dispatcher | 飞书独立 app（M2）；Telegram 后置 |
| 8 | M0 范围 | 见 plan-phase0.md（待出）；核心：本机 Node 跑 print/bg 两种 spawn + journal + 最小 Web 会话页 |
