# 决策记录（ADR 风格，按时间追加）

| # | 日期 | 决策 | 由谁 | 依据 |
|---|---|---|---|---|
| D-001 | 2026-09-12 | 项目名 **Remuda**；二进制 `remuda`；仓库目录暂留 `hybrid-harness`，稍后改名 | 用户 | naming-check.md：npm/crates/brew 全空 |
| D-002 | 2026-09-12 | 定位 unified remote agent runtime；不造 agent loop；Claude Code 第一等，codex/grok 次级，agy 不进 MVP | 用户 | _context.md 定位修正；gateway-auth §5.4 |
| D-003 | 2026-09-12 | **后端 Rust**（tokio + axum + tokio-tungstenite + portable-pty）；前端 React + TS + Vite + xterm.js。附带两条用户点名的路径：(a) Codex 可以直接以库嵌入（`codex-rs` 的 `codex-core` / `codex-protocol` / `app-server-protocol` 作 git 依赖），M4 与「spawn 已安装 `codex app-server`」二选一，默认 spawn 以保持与用户订阅客户端同版本；(b) Claude 若手写 stream-json 控制协议跟不上官方变化，加一个最小 TS sidecar 包 `@anthropic-ai/claude-agent-sdk`，Node Agent 通过本地 socket 调它 | 用户 | 可直接 lift vibe-kanban 执行器层、ACP Rust SDK、Codex protocol crate、herdr detect |
| D-004 | 2026-09-12 | M0 **两条并行**：`claude-print`（-p stream-json 双工）与 `claude-pty`（herdr headless server 承载）同时做；`claude-bg` 一并支持 | 用户 | claude-control-plane、herdr-herdrx、pty-driver-spike |
| D-005 | 2026-09-12 | 权限默认 = Interaction broker 询问：`--permission-mode default --permission-prompts host --permission-prompt-tool stdio`，审批/提问统一 `can_use_tool` 帧；hook 作旁路；bot 永不 bypass；已授权 cwd 可按会话开 acceptEdits。M0 允许临时 dontAsk 并记债 | 用户 | claude-interaction-probe §0/§5 |
| D-006 | 2026-09-12 | Hub 在 `devbox-sg-host`（Docker，挂现成 Caddy，Cloudflare Tunnel 出 443）；Node 主动出站 WSS；第一台 Node `devbox-sg`；CN 节点后置 | 用户（默认接受） | remote-topology + 补充验证 |
| D-007 | 2026-09-12 | astergate 作唯一 Anthropic-Messages endpoint；runtime 只有薄 ProviderProfile；网关 profile 无 Artifact/RC，另留订阅登录 profile | 用户（默认接受） | gateway-auth §1/§11 |
| D-008 | 2026-09-12 | 飞书独立自建 app + `lark-cli event consume` 先做（M2）；Telegram 后置放 Hub 侧 | 用户（默认接受） | bot-dispatcher §0 |
| D-009 | 2026-09-12 | 不 fork Paseo / vibe-kanban；不碰 claudecodeui（AGPL）；抄 Paseo 产品形状与 vibe-kanban 执行器层 | coordinator | paseo-vibekanban §3 |
| D-010 | 2026-09-12 | PTY 载体 = 每台 Node 上的 herdr server（socket API 子集 + `terminal session observe/control`）；不自建 PTY；runtime 自己接管 `agent start` 参数持久化与 jsonl 结构化 | coordinator | herdr-herdrx §0/§9、pty-driver-spike §0 |

## Cargo workspace 布局（coordinator 定，bootstrap 与计划以此为准）

```
Cargo.toml                 # workspace
crates/
  remuda-protocol          # protocol.md 的实体/事件/命令/envelope 类型（serde，版本化）
  remuda-claude-wire       # Claude -p stream-json NDJSON 编解码 + 控制协议（lift vibe-kanban protocol/client）
  remuda-herdr             # herdr socket API 客户端（JSON-RPC over UDS）+ terminal observe/control 桥
  remuda-driver            # Driver trait + claude-print / claude-pty(herdr) / claude-bg 实现；后续 codex-appserver / grok-acp
  remuda-journal           # observation journal（append-only，seq，本地文件/SQLite）
  remuda-node              # Node Agent：instance manager、出站 WSS、开发期本地 HTTP
  remuda-hub               # Hub：axum HTTP/WSS、鉴权、registry、静态前端 embed
  remuda                   # 单二进制：`remuda hub|node|dev` 子命令
web/                       # React + TS + Vite PWA（ui-spec.md）
deploy/                    # compose、Caddy、cloudflared
docs/                      # design/ research/
```
| D-011 | 2026-09-12 | 新建会话支持 **bypass permissions（yolo）** 作为一等选项（人发起的会话；UI 带风险提示；print 用 `--permission-mode bypassPermissions --allow-dangerously-skip-permissions`，pty 用 `--dangerously-skip-permissions`）。bot/dispatcher 路径仍默认询问，不自动 bypass | 用户 | 个人遥控场景 |
| D-012 | 2026-09-12 | **统一网关可选，不绑 astergate**（闭源）。ProviderProfile `delegation` 三态：`none`（原生登录态，各 CLI 自带鉴权——Claude 订阅 / Codex ChatGPT / grok xAI / agy Google，即 herdr 现状；**默认**）、`gateway`（任意 Anthropic-Messages 兼容网关，含 astergate；Claude 通过 Workflow 混合模型）、`direct`（直连 provider，多 key 轮换 v2）。无网关时多模型 = 多 CLI 并行，因此 Codex / grok 次级驱动提前到 M0–M1 完成 wire crate。代码中不出现 astergate 专有 API | 用户 | astergate 闭源；保持 herdr 式多 CLI 用法 |
| D-013 | 2026-09-12 | **次级驱动不上调**（用户已有统一网关）：`remuda-codex-wire` / `remuda-acp-wire` 完成最小可用后冻结在 M4；**多主机统领提前到 M1 主线**：(a) SSH remote 是一等主机接入方式（复用 `~/.ssh/config` 含 ProxyJump；`ssh <host> remuda node --stdio` 作为无端口传输，Hub 也可通过 SSH 推送/更新 Node 二进制），(b) Hub 统一注册多台主机（在线、CLI 清单、登录态、标签、负载），(c) 跨主机调度：placement（按标签/能力/负载选主机）、fleet 操作（同一任务在 N 台主机并行）、主 agent 通过 MCP/CLI 指定 `--host` 起远程 worker，Workflow 的 agent() 可借此把子任务分发到不同主机 | 用户 | 用户手边有多台 devbox（CN/SG），要「一起调度」 |
| D-014 | 2026-09-12 | **Dogfooding 目标（D0）**：Remuda 要能用来迭代 Remuda 自身，打平当前「coordinator 用 herdr 派多 agent」的工作流。为此提前实现：(a) `generic-pty` driver（herdr 承载，任意 kind：claude/codex/grok/agy…，每种 kind 的 yolo 参数预设与命名），(b) worktree 集成（`remuda instance create --worktree <name>` 自动 `git worktree add -b`），(c) MCP/CLI 控制面补齐 `list/wait(until)/read(lines)/send-keys/broadcast`，(d) coordinator 用的 Claude Code skill（等价于现在的 herdr skill），(e) 用一次真实的 `claude -p --mcp-config remuda` 会话派一个 codex + 一个 grok 任务并回收结果作为验收。结构化 codex/grok 驱动仍留在 M4 | 用户 | 用户 2026-09-12 设定 |
| D-015 | 2026-09-13 | **D0 herdr 对标达成**（main `30e4abe` / dogfood-3）：一次真实 haiku coordinator 会话对 codex+grok `generic-pty` 得到 `codexDone=true`、`grokDone=true`（wait `line:(?m)^DONE` `condition-met`）。Fable 级模型（Claude Fable 及同级 coordinator）**只做统领**，不执行仓库改动；执行面是 **codex / grok / opus**（用户规则）。结构化次级驱动仍留 M4（D-014）。已知缺口不挡 D0：grok 有时要跟发一次 send；`instance stop` 不回收 herdr workspace；Node shutdown 可能打 `driver shutdown did not settle` | 用户 | [dogfood.md](./dogfood.md) D0 验收结果；[dogfood-3-report.md](./dogfood-3-report.md) |
| D-016 | 2026-09-13 | **Remote terminal**：Web 合同见 [remote-terminal.md](./remote-terminal.md)（文首）。输出 = `tty.frame` binary channel `1`；输入 = binary channel `3` 原字节（键盘+鼠标，不过滤）；resize = JSON `tty.resize {cols,rows}`；attach = `GET /v1/follow?tty=1` 先重放 ≤256 KiB / 全屏 ANSI snapshot。`generic-pty` / `claude-pty` 仍经 herdr `terminal session observe/control`（D-010）；新 kind `terminal` + driver `shell-pty` 用 `portable-pty` 跑 login `$SHELL`，并作为无法识别的 agent CLI 的 fallback。写权限 = 能发 instance commands 的设备；`maxTtyInputBytes` 限长。 | 用户 | [remote-terminal.md](./remote-terminal.md)；herdr-herdrx §7.2；protocol.md §7.4 |
