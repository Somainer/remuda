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
| D-017 | 2026-09-13 | **Agent instance 不是受信任 principal**（延续 D-011）。Hub 依据设备 kind 和 MCP 的 instance 绑定决定 Human / Bot / Agent；未知来源按 Agent。实例内的 MCP/CLI 使用实例绑定的设备凭据；人类设备运行的 coordinator `claude -p` 保持 Human。Agent 可 send/stop/rm 自己及直接创建的子实例；同 host create 记录不可自报的 parent-child，跨实例控制与跨 host create 必须取得绑定具体动作的一次性人类 Interaction 审批。Agent 的 keys 总要审批，`fleet --all` 永久禁止；Human/Bot 全量发送和 keys 必须显式 `confirm`。generic-pty 只在显式 bypassPermissions 且 Human/Bot 来源时可合并 yolo preset，Agent create 使用非 yolo preset；原有 Bot 的 D-011 materializer 拒绝规则继续有效。 | 用户 | security-review-2 M0–M3 / tasks 3–6；[protocol.md Origin](./protocol.md#origin) |
| D-018 | 2026-09-13 | **Bootstrap/enroll 拆分**（security-review-2 A2/A1，任务 12）：`bootstrap-token` 从此只是 **设备配对 access code**——默认 24h TTL（`bootstrapTtlHours`，`0` 关闭）、可用 `remuda hub rotate-bootstrap` 轮换（已配对设备的 device token 不受影响）。它**不再**认证 Node。刻意**不做** one-shot-per-deviceName：`deviceName` 由调用方提供且未经认证，拒绝重名挡不住攻击者（换个名字即可），却会打断合法的重复登录——`HubClient` 只在内存里保存 device token，所以每次 CLI 调用和 `hub --with-dispatcher` 组合模式都会以固定名字重新登录。真正的一次性配对需要客户端先持久化 device token，在那之前 A2 可执行的部分是 TTL + 轮换。**Node enrollment** 改用独立的 enroll token：由已认证设备 `POST /v1/hosts/enroll-token` 铸造（默认 60 分钟 TTL `enrollTokenTtlMinutes`、单次使用、只存 Argon2 hash、明文只返回一次），Node 以 `REMUDA_ENROLL_TOKEN` 呈现；**已注册 host 只能用自己存储的 node token 重新上报**，enroll token 命中已存在的 `host_id` 一律拒绝（关闭 A1 的冒名路径）。`remuda dev` 同时铸造两者供本地使用。同进程组合模式（`hub --with-dispatcher`、`remuda dev`）不再持有 access code 调 API：`RunningHub::mint_device_token` 在进程内直接写一条 device 行，dispatcher 拿到的是可撤销的 scoped device token（设备名 `remuda-dispatcher`），与 F13 对 standalone 路径的要求一致，而不是给它开例外。迁移：新增 `enroll_tokens` 表；旧 data dir 缺 `bootstrap-issued-at` 时按首次读取时间补写，不会把运维人员锁在门外。 | 用户 | [security-review-2.md](./security-review-2.md) A2/A1、任务 12 |
| D-019 | 2026-09-13 | **Remote Node 生命周期独立于控制连接**：主路径为持久 Node daemon 主动 WSS 连接公网可达 Hub，以 D-018 一次性 enroll token 换取持久 host token，退避重连并按 Hub 已确认 seq watermark 重放 SQLite journal。后备路径为同一个 daemon 加 Hub 托管 SSH **bridge**（stdio ↔ 本地 `0600` Unix socket），SSH 仅 bootstrap/转发，断线不终止 daemon 或实例；新 bridge takeover 后恢复 journal 并对账实例状态。既有 `node --stdio` 仅开发用、进程不持久；`remuda dev` 保留进程内 Node。此决策替代 D-013 的 connection-bound SSH 启动方式。 | 用户 | [remote-modes.md](./remote-modes.md)；[remote-daemon-1.md](./evidence/remote-daemon-1.md) |
| D-020 | 2026-09-13 | **公开部署改为公网 VPS 上的 Hub + Caddy**，取代 D-006 的内网 Hub + Cloudflare Tunnel。SG 宿主机没有公网 IP，现有 Caddy/网关域名解析到内网地址；企业风控禁止 cloudflared、frp、ngrok、长期 `ssh -R` 等隧道/内网穿透。所有 Node（SG、devbox、笔记本）按 D-019 主模式以常驻 daemon + 出站 WSS/HTTPS 连接公网 Hub，首次接入用 D-018 一次性 enroll token；手机 HTTPS 直达公网 Hub。内网 provider 网关只由对应 Node 直接访问，配置和凭证按主机保存，Hub 不访问内网 provider。`deploy/public/` 是支持的部署包；旧 M1 Tunnel 操作步骤不再适用。仅记录、未实现的替代方案：内网 Hub 与客户端都主动连接薄公网自定义 WSS relay（Tailcat 风格，无第三方隧道软件），以保留内网 Hub 持久化，代价是额外组件、协议和延迟。 | 用户 | [deploy-public.md](./deploy-public.md)、[deploy/public](../../deploy/public/README.md)；本次为部署包、runbook 和测试，不执行部署或 SSH |
| D-021 | 2026-09-13 | **Provider scoping**：Hub 存储的 Provider 分 **universal**（任意主机经 SecretBroker 拿到 profile + secret）与 **host:<hostId>**（secret 只对该机释放）。Host `providerBinding` 为 `auto`（默认）\| `native`（该机自己的 CLI 登录/网关，Hub 不发 API key）\| `profile:<id>`。Claude launch 在 Hub 侧按 显式 request → host binding → host-scoped default → universal default → 主机 inventory 原生登录（若检测到）解析，否则 422。 | 用户 | [providers.md](./providers.md) |
| D-022 | 2026-09-13 | **PTY 启动对话框与排队**：启动后的原生 TTY 阻塞保留 ready/blocked 实例和 interaction，prompt 排队并在 idle 后发送（Claude 另需当前 SessionStart hook），create 仍 accepted。Node `auto_trust_registered_workspaces` 默认 true，仅 claude-pty 的完整、精确 folder-trust 对话框且 canonical cwd 位于本 Node 注册 workspace 内时，按当前选项游标自动确认；目录外或关闭配置时交用户。记录 interaction 与 auto-accepted diagnostic，按键 ACK 不明时不重放。 | 用户 | [pty-trust-1.md](./evidence/pty-trust-1.md) |
| D-023 | 2026-09-13 | **运行中注册 Workspace**：Node data dir 持久化 workspace registry，重复 `--workspace` 与已有记录合并；Node `workspace_roots` 限制可注册范围，默认本机用户 HOME。注册须是存在的绝对目录并 canonicalize，禁止落入其他 workspace 的 worktree 目录。Hub↔Node 增加 `workspace.list/register/unregister`，变更经 prepare/commit 两阶段，Node 持久化后 Hub 才保存并广播最新 registry。Human/Bot 可用 `GET/POST/DELETE /v1/hosts/{id}/workspaces`，Agent 一律 403。新建会话选择主机注册目录并填写可选相对子路径；Node 展开开头的 `~`/`$HOME`，仍按注册目录或其 worktree sibling 检查 containment，越界错误列出已注册根和注册方式。取消注册仅撤销后续会话准入，保留目录与已有会话。 | 用户 | [workspace-reg-1.md](./evidence/workspace-reg-1.md) |
| D-024 | 2026-09-13 | **项目 Space → agent Tabs**：借鉴 herdr 的 workspace → tabs → panes，Remuda 的 Space 对应某台主机上一个已注册 Workspace（D-023），以 `(hostId, workspaceId)` 唯一标识；同名目录或跨主机目录不合并。展示名默认取 workspace 根目录 basename，重命名、手动排序、分组展开状态、侧栏折叠和每个 space 的上次选中 tab 存本设备 `localStorage`。每个实例按 host/workspace 精确归属一个 space，无匹配项归入「其他」。当前 space 的 agent 会话显示为内容上方 tabs（标题、harness 字形、活动点、关闭），切换 space 恢复该 space 的 tab，并为新建会话预填其 host 与 cwd；`/s/:instanceId` 深链反向选中对应 space 和 tab。桌面支持可折叠 Spaces/Sessions 左栏及首字母窄轨，⌘/Ctrl+B 切换侧栏、⌘/Ctrl+1..9 切换 tabs、⌘/Ctrl+[ / ] 切换 spaces；手机使用横向 space chips、可滚动 tab 条和侧栏抽屉。沿用 Night Corral tokens，不引入 panes；fleet、全局审批及 composer 的职责保持原状。 | 用户 | [ui-spec.md §1.4](./ui-spec.md#14-space--tabs-项目工作台d-024)；[spaces-1.md](./evidence/spaces-1.md) |

### D-020 优先级调整（同日，用户后续指示）

当前先在 SG 内网运行 Hub，复用现有 Caddy 的 DNS-01，为 `remuda.<zone>`
配置内网 A 记录，只追加独立站点 include 和必要的 import，不修改既有网关站点。
`deploy/intranet/` 与 [deploy-runbook.md](./deploy-runbook.md) 为当前操作入口；
`deploy/public/` 保留为后续公网 VPS 变体。此调整授权了内网实施，取代上表
本轮仅准备包的执行范围；实际执行结果以 [内网实施证据](./evidence/intranet-hub-1.md)
为准。禁止隧道的约束保持不变；DNS-01 证书不产生公网可达性，内网 Hub
只承诺具备内网路由的客户端可达，普通蜂窝网络访问需后续公网方案。

### D-020 执行边界调整（同日，最新用户指示）

当前仅准备内网变更，状态为 `awaiting-caddy-restart-approval`。准备一次性
apply/rollback 脚本：apply 添加 Remuda import，并将全局 admin 改为
`localhost:2019`，验证后重启现有 Caddy，再检查网关与 Hub；rollback 恢复
原 import/admin 设置，验证后重启并检查网关。两个路径中的重启均须等待明确
批准，当前不执行，也不继续 Node 验收。之前的临时 reload/恢复结果只记入
实施证据，不代表对最新执行边界的继续授权。
