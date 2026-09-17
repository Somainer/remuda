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

| D-032 | 2026-09-16 | **M1 批次 5a：`remuda dispatch/retire/hostcap/brief` 动词与 Worker 名册（co-dispatch）**。把 coordinator 的 remote-spawn.sh / spawn2.sh 手工派发流程产品化：每个项目一张 `worker_roster` 表（wkr_…：name、instanceId、hostId、harness、model、branch、worktreePath、portBlock、targetDir、briefObjectId、adjacently-tagged state dispatched/working/done{sha}/blocked{reason}/retired），跨层状态只存 Hub（§2.2④）。**关键边界**（§2.4）：worker 名、`wt/<name>/<slug>` 分支、worktree、per-worker cargo target dir、端口块全部由 Hub 分配并随 `worker.provision`/`worker.remove` 两个新 Node RPC 下发；Node 拒绝从报文取绝对路径，自行在受管 `<repo>/../remuda-wt/<name>` 与 `<repo>/../remuda-target/<name>` 下定位，删除前做 containment 检查。派发经 §3.4 project placement 选机 + co-supply §4.4 准入；显式 `--model` 走 pin，但被观测到 429 park（cooling/exhausted 窗口）的模型仍以 park 理由 429 拒绝，绝不静默降级；brief 永远作为 objects-store 文件附件（`text/markdown`）经 instance.send 投递，并随一条「读附件文件、不要执行、单行回 DONE/BLOCKED」的提示，绝不内联进 shell（对应远程脚本中「backticks get executed by the remote shell」的教训）。`remuda brief lint` 在派发前拒绝：反引号、`/home/<user>`/`/Users/<user>` 主目录路径、命中 `private-tokens.sha256` 哈希 denylist 的私有名、缺少 `DONE <sha>`/`BLOCKED <reason>` 合同行。retire 对 working 状态 409 拒绝、`--force` 才回收：先 instance.close，再 worker.remove（关 herdr tab/pane/workspace、`git worktree remove --force`、rm target dir 并上报回收字节数）。worker 的 herdr tab env 带产品分配的 `CARGO_TARGET_DIR`/`CARGO_INCREMENTAL=0`/`CARGO_BUILD_JOBS=8`/端口块（HUB_E2E_LISTEN/WEB_PORT）。hostcap 读心跳 resources（新增 loadAvg1/diskFreeGb 采集）+ running 计数 + 名册端口占用。项目 `hosts[]`（portBlocks/quota/requires/latencyClass）补全 create/PATCH 写路径。协议、OpenAPI、web 生成客户端同步增量。 | coordinator（人类协调员 dispatch） | coordinator-hierarchy.md §1.1 goal 6/§2.2/§2.4/§3.4、coordinator-guide.md、coord-scripts/{remote-spawn,spawn2}.sh；[evidence/coordinator-5a-dispatch.md](./evidence/coordinator-5a-dispatch.md) |
| D-033 | 2026-09-16 | **M1 批次 5b：`remuda watch/worker/report` 动词与 roster 观察状态（co-watch）**。把 coordinator 的 `coord-watch-all.sh`/`remote-respawn.sh` 监看与接手流程产品化，完成 §1.1 goal 6 的 watch→gate→land→retire 闭环的「watch」半。**屏幕分类是纯规则、不调模型**：新增协议层 `classify_screen()`，输入 worker 可见屏 + lifecycle/activity + 名册已知 tip，输出 working/done{sha}/blocked{reason}/idle-api-error/stalled/gone。两条 echo 抑制：忽略 brief 合同回显（`<sha>`/`<reason>`/「reply on one line」等占位与提示）、以及等于名册已知 tip 的 DONE/BLOCKED——resume 后的会话会重新渲染旧 DONE，旧 tip 不算新报告（行为脚本里就是先 grep 掉已知 sha/提示再取最后一条）。idle-api-error 要求 activity/idle 且屏上有 API Error/Connection closed/Retrying；stalled 要求一个 busy turn 且屏幕摘要跨 poll 静止超过项目 `stallThresholdMins`（默认 30 min，对齐脚本「single API turn >30 min no process activity」），活动时钟从屏幕摘要变化派生、首次观察以 instance.updatedAt 播种。**headless 原始 VT ring 模式**：无 live viewer 驱动模拟器时 `tty.screen` 返回 ANSI-stripped 的 raw-ring（光标定位/宽度填充的重绘串、无换行边界），分类器在 raw 模式下按脚本 grep 语义在拼接 tail 任意位置找严格 7–40 hex 的 `DONE` token（prose「reply DONE <sha>」仍被严格后缀挡住），emulator grid 仍要求行首锚点。观察点状态持久化到 roster 新字段 `watch`（WorkerWatch：status/detail/echo 基线/lastScreenDigest/lastActivityAt），另加 lastNudgeAt/resumedFrom/replaceCount；**只有新 DONE/BLOCKED 推动生命周期 state**，idle/stalled/gone 只是瞬时观察（§7 I1：DONE 是声明不是 gate）。新 Hub 路由全部 Hub→Node→instance.send/tty.write，CLI 绝不 ssh：`POST /workers/observe`（批量读屏分类落库）、`/{id}/nudge`（文件附件投递 continue 提示，按项目 nudgeThrottleMins 节流）、`/answer`（enter|esc|1..9|文本，协议层 `encode_answer_key` 编 PTY 字节）、`/switch-model`（esc×2→`/model <id>`→**仅当屏底确认了 switch/confirm 对话框才补 Enter**，无确认不改 model 也不盲发回车）、`/resume`（有 native session id 走 D-026 `instance.resume` terminal，否则在同一 surviving worktree 重拉一个 agent 并把最后 brief 作为 handback 文件重发，repoint instanceId、记 resumedFrom）、`/replace`（retire --force 后用同一 brief/name 在**新 slug 分支**重派，因 retire 保留旧分支；记 replaceCount）、`/stop`（instance.close 但不回收 worktree）。`remuda watch [--project][--once|--follow][--json]`：--follow 只打印状态变化、所有 worker done/retired 即 exit 0；`remuda report [--project][--for-owner]` 综合 roster + mergequeue 落盘报告（仓库 common git dir 的 remuda/merge-reports），`--for-owner` 落 §5.3 T1 契约：只发 owner 可解项（正式 BLOCKED/stalled/gone/硬 gate 失败）且按持久 marker 仅在状态变化时发，429 后只需 nudge 的 idle-api-error 明确**不**打扰 owner。dispatch 增加可选 `--driver`（`shell-pty` 选原生可读屏 carrier；默认 herdr/print 行为不变），协议/OpenAPI/web 生成客户端纯增量同步。 | coordinator（人类协调员 watch） | coordinator-hierarchy.md §1.1 goal 6/§2.2/§2.4/§4.6/§5.3、coord-scripts/{coord-watch-all,remote-respawn}.sh；[evidence/coordinator-5b-watch.md](./evidence/coordinator-5b-watch.md) |
| D-036 | 2026-09-18 | **M1 自托管一轮通过，带一个例外**。用真实任务（README Status 刷新）只以 `remuda` 动词跑完 dispatch→watch→gate→retire：`remuda dispatch --driver shell-pty --model ark/seed-evolving[1m]`（host-native profile、无网关 overlay、`delegation none`）派到 **ssh-stdio** Node 上的远端 worker（分支/worktree/target dir/端口块全部产品分配，`warnings: []` 无供给替换）；`remuda instance read --source screen` 读真实屏；两个首次运行对话框用 `remuda instance keys` / `remuda worker answer` 回答；`remuda watch --once/--follow` 到 `DONE 30a4be32…` 自行 rc=0；`remuda gate --lane sg-lane1` 在 lane 主机上逐步流式验证通过（`gjb_01a0b030…`，merge `6f7d987ace24`，≈100 s）；`remuda retire` 回收 worktree 与 target dir；`remuda report --for-owner` 只报 owner 可解项。**例外（不判失败，判记账）**：`remuda land` 未使用——lane 主机没有 origin 推送凭据，最后一跳仍是在 home host 上 `git fetch` + CAS `push`（第一次因 merge 对象无 ref 可达而 `src refspec … does not match any` 失败，改为在 lane 主机上打 `refs/gate/<worker>` 临时 ref 再 fetch 才推成 `origin/main` 6f7d987a）。**结论**：§1.1 goal 6 的闭环成立于 dispatch/watch/gate/retire，land 在 Hub/home host 侧 CAS 推送落地之前仍留在 home host；两个首次运行对话框（workspace trust、outside-reads）是**预置任务**而不是人工步骤——Node 应为自己 provision 的 worktree 预写 trust 标记、仅在 `permissionPosture: bypass` 下预置 outside-reads 答案，并且对话框在屏上时 `watch` 必须报 `blocked{reason}` 而不是 `working`（c-onboard2 在跑）。另记：一轮里跑通的是**一个** worker，≥3 worker 的真实批次仍待补；rgate4/coord-guard/remote-spawn 三个过渡脚本仍在环外运行，各自对应的产品缺口见证据文档 §4。 | coordinator（人类协调员 self-host） | coordinator-hierarchy.md §1.1 goal 6/§8.2 M1、roadmap-2026-09.md §R0、D-032/D-033/D-034/D-035；[evidence/self-host-1.md](./evidence/self-host-1.md) |

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
deploy/                    # compose、内网 Caddy
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
| D-025 | 2026-09-13 | **Terminal → agent promotion**：`terminal` + `shell-pty` 实例不再永远是纯 screen text。Node 每 ~1s 轮询该 PTY 的前台进程组（master fd 的 `tcgetpgrp`，再按 pid 读进程名/argv），命中已知 agent CLI 表（MVP：`claude`；`codex`/`grok`/`agy` 只做检测与 kind 切换）就把实例 **promote**：`kind` 改为该 agent，`driver` 仍是 `shell-pty`，新增实例字段 `mode: "native" | "promoted"` 与 `promotedAt`，并记一条 native diagnostic `agent detected: <kind>`。promote 后 agent_status 复用 claude-pty 的屏幕启发式，interaction 复用既有 PTY 检测，composer 的 prompt 以 PTY 文本 + Enter 送入（bracketed-paste 感知，阻塞时走 D-022 队列）。**结构化消息**由 Claude transcript 水合：按 cwd 编码定位 `~/.claude/projects/<encoded>/<session>.jsonl`（优先取 argv 里的 `--session-id` / `--resume`，否则取检测时刻之后新建/更新的最新文件），逐行喂给既有 claude transcript mapper，产出 open→append→close 的 message/thinking/tool 事件。前台 agent 退出即 **demote** 回 `terminal`，同样记 diagnostic。promote/demote 幂等且全部进 journal，Hub/web/MCP 因此实时反映。屏幕签名是 `tcgetpgrp` 不可用时的 fallback，不是主路径。 | 用户 | [remote-terminal.md](./remote-terminal.md#terminal--agent-promotion)；[terminal-promote-1.md](./evidence/terminal-promote-1.md)；D-016 / D-022 |
| D-026 | 2026-09-13 | **会话续接（Session continuity）**：每个 Claude 实例都把驱动**实际上报**的 native session id 记进 `nativeRef`（claude-print 取 stream-json `system/init` 映射出的 `session` lifecycle；claude-pty 取 SessionStart hook 的 `session-meta.json`，并带 `transcriptPath`），取代 create 时用 Instance id 造的占位值——占位值 `claude --resume` 根本不接受。Resume **不复活**已退出的进程：它在**同一 host / workspace** 上新建一个实例，沿用父实例的 provider / permission / model / cwd，由 materializer 走 `SessionAction::Resume` 发出 `--resume <sessionId>`（`--resume` 属 RESERVED，仅 materializer 可发；`--continue` 仍 BANNED，因为它按时间而非身份选会话）。父子双向落账：子实例 `instance.parent` 指向父实例，Hub 记 `resumedFrom`，两侧各journal 一条 lifecycle；旧实例保持 exited，历史完整保留。Resume 控件提供两个目标：**继续（结构化）** 沿用父实例驱动，**在终端中继续** 用 claude-pty 续同一个 session——这也是structured-only 会话「回到 Terminal view」的答案：不是给旧实例补一个终端，而是让同一段对话在一个有终端的新实例里继续。`POST /v1/instances/{id}/resume {mode:"structured"|"terminal"}` 仅 Human/Bot，Agent 一律 403（resume 会占用主机容量并拉起原生进程，超出实例凭证的授权范围）；同一 (instance, mode) 在短窗口内幂等，重复点击复用既有子实例而不是对同一段对话再拉一个进程。未上报过 session id → 409 并说明「没有可续接的 transcript」，退出超过 30 天 → 409 并说明过期；两者分开报，避免 resume 静默开出一个看起来连续、实际为空的新对话。 | 用户 | [resume-1.md](./evidence/resume-1.md) |
| D-027 | 2026-09-13 | **附件与图片直通（MVP）**：浏览器→agent 的图片走「Hub 暂存 + Node 回拉落盘 + driver 按能力投递」三段，字节**只走 HTTP**，命令帧里只带 metadata（绕开 JSON 帧 1 MiB / prompt 64 KiB / TTY input 4096 B 三道上限）。Hub 新增 `POST /v1/objects`（raw body + `Content-Type`，`X-Remuda-Instance-Id`）与 `GET /v1/objects/{id}`，仅该子路由挂 `DefaultBodyLimit`；allowlist `image/png|jpeg|gif|webp` 且**以服务端 magic bytes 嗅探为准**，与声明的 `Content-Type` 不符即拒；丢弃原始文件名，落盘名固定 `<obj_id>.<ext>`；单文件 ≤5 MiB、单条消息 ≤4 个附件、单 instance 暂存 ≤64 MiB；24 h TTL + 惰性删除。EXIF 在**浏览器侧**无条件 canvas 重编码剥除，Hub 不引图片库。鉴权：上传要 `require_origin` + device，**Agent origin 一律 403**（不进 `agent_scope` 白名单）；回拉对 device owner 与**该 instance 所属 host** 双向放行，host 侧按 `object.instance_id -> instance.host_id` 绑定，一台 Node 读不到别台 Node 的附件。协议纯增量：`InstanceSendParams.attachments[]{objectId,mediaType,name,size}`，`prompt`/`prompt_text()` 不变，老 Node 忽略即降级为纯文本。Node 在 dispatch **前同步** materialize 到 `<data_dir>/instances/<ins>/attachments/`（目录 0700、文件 0600，不进 workspace），失败整条 send 失败、**不静默降级为纯文本**；instance 停止/退出时递归清理。driver 分层投递：claude-print 发 base64 image block，claude-pty/generic-pty/shell-pty 追加绝对路径提及（codex 必须显式写「读取 <path>」），grok 记一条「该 agent 不支持读图」的 journal note。**刻意留 v2**：channel 2 `ObjectChunk`、`object.prepare/write/commit` handler、上行 `blocks`、服务端回显、远端 pasteboard、终端粘贴、share target。 | 用户（设计已批准）/ x-clip 实施 | [clipboard-images.md](./clipboard-images.md) §5–§7 |
| D-027a | 2026-09-13 | **§8.1 阻塞项已验证：Node 持有可用的 Hub HTTP base URL + token，因此 MVP 走 HTTP 回拉主路径，不启用 channel 2 `ObjectChunk` 兜底。** `WssConfig.url`/`token` 在生产 Node、daemon 与 `remuda dev` 三条路径上都齐备（`cmd/node.rs:188`、`cmd/node/daemon.rs:117`、`cmd/dev.rs:165`），且 Hub 已经用**同一个** host token 认证 WS 握手（`ws.rs:161` `presented_token` → `store.rs:616` `authenticate_host`），所以只需补 Hub 侧一个复用同一查询的 HTTP `require_host` 提取器 + Node 侧一个 `reqwest` 客户端，不新增凭据种类或信任边界。代价：Node 需要对 Hub 出站 **HTTPS** 而不只是 WSS——D-020 的部署形态本就要求如此，故非新增网络要求；若将来出现只开 WSS 的部署，回退仍是 v2 的 channel 2，Node 侧 materialize 接口不变、只换字节来源。 | x-clip（开工前第一件事验证） | [clipboard-images.md §8.1](./clipboard-images.md) 源码复核表 |
| D-027b | 2026-09-15 | **附件从「仅图片」泛化为「任意文件」（owner：现在上传附件竟然只支持图片吗，应该各种文件都支持）。** 三段架构（浏览器→Hub 暂存→Node 回拉→driver 投递）与「字节只走 HTTP、命令帧只带 metadata」不变，放宽的是类型与名称：**(1) Hub**——`POST /v1/objects` 接受任意 MIME；PNG/JPEG/GIF/WebP 仍走 magic-byte 嗅探且声明类型必须一致，其余文件只校验声明类型 essence（合法 token、≤255 字节，参数丢弃），**声明 `image/*` 却嗅不出图片魔数的一律降级为 `application/octet-stream`**，杜绝把 HTML/SVG 当图片渲染；新增 `?name=` 原始文件名，服务端净化（无 `/`、`\`、控制字符，trim 首尾空白与点，≤255 字节，净化后为空则回退派生名）；单文件默认上限 5 MiB → **25 MiB**（配置键 `attachmentMaxBytes` / `REMUDA_ATTACHMENT_MAX_BYTES`，body 层 slack 同步放大），单条消息 4 → **8** 个，单 instance 暂存 64 → 256 MiB；`GET` 对图片 inline、非图片固定 `Content-Disposition: attachment`（净化文件名）+ `nosniff`；`objects` 表加 `original_name`、`kind('image'|'file')` 两列（ensure_column 迁移，老行默认 image）；send manifest 回写 Hub 自己的 `kind/mediaType/name/size/digest`，`AttachmentRef` 增 `kind`（缺省 image，老帧兼容）与 `digest`。**(2) Node**——落盘名从派生 `<obj_id>.<ext>` 改为净化后的原始文件名（Hub 净化 + Node 再净化，纵深防御），重名加 `-<n>` 数字后缀（同批次与已存在文件都避让）；回拉后校验 sha256，上限 25 MiB / 8 个，文件 0600、目录 0700、永不执行；journal user-message 记录 image/file + resource blocks，**绝对落盘路径作为 journal metadata**（PTY 队列在落盘完成后的 Replace 修订里补 blocks）。**(3) driver 投递**——图片保持原生路径（claude-print base64，>3.5 MiB 退化路径）；非图片**永不内联**，在用户文本**之前**逐文件展开一行 `[File #n] <name> (<mime>, <human size>) saved at <absolute path>`（`remuda-driver/src/attachment.rs` 共享 `file_mention_lines()`），claude/codex 用各自 Read/文件工具打开；grok 的「不支持读图」journal 徽标只对图片触发，普通文件路径不触发；shell-pty 追加 shell-quoted 路径。**(4) Web**——picker 改 `accept="*/*"`、拖拽与粘贴接受任意文件；chip 图片显缩略图、文件显类型图标+人名+可读大小；`[File #n]` 与 `[Image #n]` 共用 imageAnchors 同一编号空间（chip 位置），插入/删除/重编号/未引用标记完全复用；已发送消息里文件是指向 Hub 对象的下载 chip，图片仍是缩略图；结构化 transcript 把 harness 回显的 `[File #n] … saved at …` 行折叠成可展开行。证据 `evidence/attachments-4.md`（REMUDA_EVIDENCE=1）。 | 用户（owner nit）/ r-files | [clipboard-images.md §5.3.1](./clipboard-images.md)；[evidence/attachments-4.md](./evidence/attachments-4.md) |
| D-028 | 2026-09-14 | **原生 PTY 优先（native PTY first）：统一「在终端里启动 agent」与「直接开 Session」两条路径。** **决策**：Remuda 自持的 native PTY（`portable-pty` master + `vt100` 模拟器 + 字节 ring）成为所有 harness 的默认 carrier，herdr 降为可选 feature（`REMUDA_PTY_CARRIER`），`claude-print` 按条件退役（见下）。统一原则：**一个 agent session 就是一个 terminal session，里面跑着 agent 命令，没有第二条路径**——New Session 等价于「在该终端里预填 launch command 并回车」，D-025 promotion 是唯一的检测/水合路径，`mode` 只记录出身（建议改名 `launchedBy`）而不表示能力等级。配套机制：per-session launch shim（`<data_dir>/instances/<id>/launch/bin/` 下透明 `exec` 包装置于 PATH 最前，让**用户手敲**的 `claude` 也命中 overlay）、per-instance hook unix socket（0600，取代 200 ms 轮询 JSON 文件，使 `PermissionRequest` 能阻塞等裁决）、信号分层 `Hook > File(tail) > OSC > Screen`（每条 Observation 带 `SourceChannel`）、capabilities 由 `DriverKind` 改为按**本 session 实际拿到的信号层级**运行时上报。**理由**：(a) herdr 拥有的是整个 agent 抽象（`agent.start`/`agent.prompt`/`agent.wait`/`AgentStatus`/`pane.agent_status_changed`），状态语义不可解释且无法细分「等审批」与「等输入」；(b) `shell-pty` 已用 `portable-pty` 独立实现了 raw PTY、mouse 透传、scrollback、`tcgetpgrp` 前台识别、D-025 promote 与 transcript 水合，两条路线在仓库里并存维护双份；(c) herdr 的 `rendered-ansi` 是视口 blit，帧内没有换行，xterm 永远没有行被逐出——**这是「agent pty 滚不动」的根因**，换真字节即解；(d) 用户在自己终端里敲的 `claude`，herdr 看不见也无法接管，统一原则在 herdr 下不可能成立；(e) herdr 的「trained classifier」实为可提取的 TOML 规则表（regions/priority/combinators），移植成本从「重训模型」降为「搬表 + 写引擎」。**影响**：**直接反转 D-010**（「PTY 载体 = herdr，不自建 PTY」）；D-016 线协议不变，仅升级服务端 snapshot 生成方式并新增 `altScreen` 上报；D-022 不变量（写前置 `attempted`、不重放回车、单次自动 trust）全保留，hook 回执成为队列的上位输入；D-025 升格为唯一入口；D-026 语义不变且 `ResumeMode{structured,terminal}` 可删；D-027 附件投递改走 MCP 工具，首次让 codex/grok 真拿到图片字节；D-013 的 `remuda-codex-wire` / `remuda-acp-wire` 保留为 codex 类型化审批旁路。**唯一没有廉价替代的 herdr 能力是跨 Node 重启存活**：本轮先接受丢失（Hub 既有 `node-epoch-changed` reconcile + UI 诚实说明 + D-026 Resume 兜底），`remuda-ptyd`（detached per-instance PTY holder + UDS + 重启认领）排 P8，**该项待用户拍板**；herdr 的删除以此为准，不以 `--bg` 为准。print 退役**按条件不按日程**：usage 三家覆盖 ≥1 周、MCP 附件三家验证、parity gate 连续 3 次全绿后逐 harness 翻默认（claude 最后），再降 `feature = "non-tty"` legacy，`pty.fork` 在无控制终端宿主实测可用前**不得删源码**。工期 P0–P7 约 25 人日，4 worker 并行约 12–14 个工作日。 | 用户 | [native-pty-first.md](./native-pty-first.md)（§5 生命周期、§6 steer/排队/打断、§7 实时流式、§8 重启存活、§9 effort/tui、§10 规则表、附录 A 覆盖矩阵） |
| D-028a | 2026-09-14 | **D-028 增补 · 用户点名必须覆盖的五项**（本增补与 D-028 同时生效，任一项缺失即视为 D-028 未落地）：**(1) 无 herdr 的生命周期操作**——新建 Session（kind/driver 矩阵放开 `(claude\|codex\|grok\|agy, agent-pty)`，per-kind recipe 与 yolo argv 搬出 herdr driver，materializer/flags/hook overlay 必须对 native 路径生效）、发送消息（ready 阶梯 `hook 回执 > 模拟器模式+静默 > 字形`，`?2004` 观测到才用 bracketed paste，body 与 Enter 分两次写，**移动端 LocalInput 的一次性 `text+\r` 必须拆分**）、停止（**打断 turn 与停进程是两件事**：`instance.cancel` 发 per-harness 键而非 `\x03`；`instance.close` 对**进程组**走 SIGINT→SIGHUP→SIGKILL 阶梯，修掉 `try_lock` 拿不到锁就静默跳过 kill 的缺陷并复查进程树已消失）、删除（`DELETE /v1/instances/{id}`，`?force=1` 复用同一条停止阶梯，purge 实例目录但永不碰用户自己的 transcript）、退出检测（`child.wait()` + PTY EOF 双证据 → `exited`/`failed`；今天 EOF 只 `break`，崩掉的 agent 在 UI 上永远 ready）、resume（= 新 session 预填 `--resume`，session id 来自 `SessionStart` hook / `session_index.jsonl` / `active_sessions.json`）。**(2) steer / 排队 / 打断**——codex 已验证 Enter 立即发送、**Tab 排队**（0.154 起 `Ctrl+;` 已移除）、`Esc` 打断；claude 的「turn 中打字+Enter 到底是排队还是 steer」**未验证**，判据是 transcript 的 `queue-operation{enqueue}` 记录；grok/agy 全未验证。composer 呈现 发送/排队/打断 三态 + 队列 chip + `status: "interrupted"`；**未验证的能力一律 `unknown`，显示「尚未验证」，不得假装支持或假装不支持**。**(3) 实时流式**——claude 走 `MessageDisplay` hook 的行级 delta（`turn_id`/`message_id`/`index`/`final`/`delta`）为主、transcript block 为权威值，grok 走 `updates.jsonl` 的 ACP chunk，codex 只有 completed item（**如实标注无 delta**）；同时修复 `TranscriptMapper`：按 `(requestId, message.id)` 缓冲并按 `apiBlockIndex` 重组（一条记录只装一个 content block、同一 message.id 跨 2–7 条记录，这是 tool-call 串位的根因）、把不存在的 `parentToolUseId` 换成 `sourceToolUseID`/`sourceToolAssistantUUID`、保留 `queue-operation`/`permission-mode`/`toolUseResult`。**(4) 跨 Node 重启存活 = 未决**：in-process PTY 随 Node 进程死亡，本轮接受丢失（方案 A）并保留 herdr 为需要存活的宿主的可选 carrier，`remuda-ptyd`（方案 B）排 P8，**待用户拍板**。**(5) effort 与 tui overlay**——per-session settings overlay 钉死 `tui`（`fullscreen`/`default` **两个方向都显式写**）、`showStatusInTerminalTab`、`terminalProgressBarEnabled`（后两者关掉即丢失整层 OSC 信号）；effort 走 launch `--effort <v>` + 会话内 `/effort <level>`，**禁用 `CLAUDE_CODE_EFFORT_LEVEL` 并从子进程环境中剥离**（它压过会话内改档），UI 一律显示 **effective**（从 transcript 的 `effort`/`perTurnEffort` 回读），回读不到显示 `?` 而**绝不回落成请求值**；保留 `--setting-sources`（代价是会话内 `/tui` 被拒，而渲染器已在启动时决定），并从字节流探测 `ESC[?1049h` 判定实际全屏模式后经 `altScreen` 上报。 | 用户 | [native-pty-first.md](./native-pty-first.md) §5–§9；附录 A.2 逐项对账 **(6) Node 重启存活（用户 2026-09-14 拍板）**：本轮接受丢失（方案 A：Hub `node-epoch-changed` + Resume 兜底，herdr 保留为 optional carrier），`remuda-ptyd` 持有进程（方案 B）排到 P8 之后。 |
| D-029 | 2026-09-14 | **Claude 首次运行 onboarding 与 PTY pane 回收**：Node-scoped `CLAUDE_CONFIG_DIR` 首次使用是空目录，2.1.270 因此跑 first-run wizard（theme → security → terminal-setup）而不是挂出 prompt composer；Herdr 把这种 TUI 菜单报成 **idle + interactive_ready**，于是 D-022 队列看到「就绪」、SessionStart 永不触发、prompt 永远 `queued`——D-022 的 folder-trust 自动确认根本到不了。三层修复：**(1) 启动前 seed**：往 scoped `.claude.json` 写 `hasCompletedOnboarding`，并按**显式白名单**镜像宿主用户的 `lastOnboardingVersion` / `bypassPermissionsModeAccepted`（global config）与 `theme` / `skipDangerousModePermissionPrompt`（user settings）；`oauthAccount`/`userID`/`projects`/`.credentials.json` 一律不复制，scoped 目录里已有的键永远优先（pane 内改过的 theme 不被覆盖），文件 0600、目录 0700。继承用户默认 config 时跳过（那本就已 onboarded），`REMUDA_CLAUDE_SEED_ONBOARDING=0` 可关闭以复现 wizard。这与 Claude Code 自己 `plugin eval` 沙箱给一次性 config dir 的做法一致。**(2) 兜底识别屏幕**：claude-pty 认出 theme / security / terminal-setup / login / bypass 五种首屏，每种至少要两个独立标记（单句 stock phrase 可能出现在模型输出里），每个 carrier 每屏只自动答一次并记 `claude-onboarding` diagnostic。**terminal-setup 按 `escape` 而不是 `enter`**——enter 会改写操作者终端的键位与响铃配置，那不是 Remuda 该动的东西；login 与 bypass disclaimer 含真实决定，只上报、交人类。**(3) wizard 不算 idle**：`prompt_ready_for` 在识别到首屏时保持 blocked，prompt 不会被打进 theme picker；Claude 自己的 SessionStart 一到就解除该屏幕监视，之后的模型输出不会被误判成 wizard。另修 **stop 不回收 pane**：driver 只回收自己内存里的 Herdr 资源，而 durable `pty_resources` 行——rebuilt/adopted driver 或报错的 close 之后唯一的记录——此前只在 Node 启动与 shutdown 被读，**stop 时从不**，于是 12 次 stop 留下 12 个 idle pane。现在 stop 结算后按实例回收 durable ownership（逐个 carrier best-effort，关不掉的保留 ownership 交给下次 sweep 而不是遗忘），`PtyResource::close` 显式关 pane 并校验没有 owned pane 存活、`ctrl+c` 后有具名 2s grace，ownership 仍以唯一 creation label 匹配（server 重启后复用的 id 绝不误关）。 | 用户 | [claude-onboarding-1.md](./evidence/claude-onboarding-1.md)；D-022 |
| D-030 | 2026-09-14 | **Passkey（WebAuthn）登录**：登录主路径改为 discoverable passkey + conditional mediation（autofill），访问码降为次级兜底；设置页可添加/重命名/删除 passkey。**决策要点**：(1) 第一次配对仍必须用访问码（D-018 不变），注册接口只接受已登录设备，不存在纯 passkey 自助开通；(2) passkey 登录成功发放与 `/v1/login` **完全相同**的设备 token / HttpOnly `remuda_device` cookie / DeviceSession，passkey 只是新的兑票方式而非新主体；(3) RP ID 与 expected origins 来自 Hub 配置（`--public-origin` / `REMUDA_PUBLIC_ORIGIN` + `allowed_origins`），未配置时仅对 `127.0.0.1`/`localhost` 回环来源按请求 Host 派生，**永不硬编码主机名**；凭据与来源绑定，内网 `https://remuda.<zone>` 与本地 demo 各自独立注册，UI 明确提示；(4) attestation `none`、UV required、resident key required、user.id 用按 RP ID 派生的固定 UUID v5；(5) 挑战进程内存放、120 s TTL、一次性取出、容量上限；新表 `passkeys`，credential id 全库唯一；签名计数器沿用 webauthn-rs 克隆检测；登录失败统一 401 防枚举；start/finish 全部并入既有认证 token-bucket 限流；`passkey.register/login/delete/rename` 进审计日志。实现直接依赖 `webauthn-rs-core = "=0.5.5"`（MPL-2.0）：其安全包装 crate 的 builder 因 `Url::domain()` 对 IP 字面量返回 `None` 无法构造回环 RP，且未导出 `new_unsafe_experts_only`，故在 core 上逐参数复刻包装 crate 的 passkey ceremony，origin allowlist 自行收敛。 | 用户 / x-passkey | [passkey-login.md](./passkey-login.md)；[evidence/passkey-1.md](./evidence/passkey-1.md) |
| D-031 | 2026-09-14 | **关停，禁止使用隧道工具**。禁止安装、探测或使用隧道类工具，包括 cloudflared、ngrok、frp（frpc/frps）、bore、tailscale funnel，以及用于暴露服务的 `ssh -R` / `ssh -D` 等反向、转发或代理隧道；项目移除相关部署配置和探测。理由：内网风控将旧 `deploy/m1/preflight.sh:119` 的 cloudflared 探测标记为「建立隧道」，用户明确要求「不能使用内网穿透协议」并决定「关停，禁止使用隧道工具」。影响：公网部署待定，`deploy/intranet/` 的内网 Caddy 为唯一支持路径；本决策覆盖 D-006 与 D-020 的公网暴露安排，旧记录保留为历史。**Worker 规则：不得执行 deploy/ 下脚本或探测隧道工具。** | 用户 | 内网风控告警与用户原话；[deploy-runbook.md](./deploy-runbook.md)、[coordinator-guide.md](./coordinator-guide.md#worker-rules) |

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

### D-024 addendum · tab 语义（同日，用户反馈后的协调决定）

用户反馈：活动 tab 与侧栏当前项不够可分辨；退出状态字形与关闭按钮都是 ×，
两个相同符号并排、含义不同。据此调整（本增补优先于 D-024 中「关闭」相关的
旧描述）：

- **状态与关闭分离**。状态点只表示状态，永不是 ×：⚠ 待处理（琥珀）、
  ● 运行中、○ 空闲（描边）、■ 已退出（灰方块），均带 `aria-label`，形状本身
  可区分，不只靠颜色。× 只有一个含义——**关闭标签**：桌面在 hover 或当前 tab
  上显示，手机通过长按或横向滑动显出。
- **关闭标签绝不静默停止会话**。已退出会话：× 直接移除 tab。运行中会话：×
  打开两选项 sheet「停止并关闭 / 仅关闭标签」。「仅关闭」是每个 space 的本设备
  偏好（扩展既有 tab 持久化：`closedTabs` 由 id 列表升级为
  `{id, resurface}` 记录，旧数据按 `resurface: true` 读入），会话继续运行；
  被收起的会话**进入 blocked 时重新出现在 tab 条**，在侧栏点击它也会重新打开
  tab。在 blocked 期间再次收起只压制本次事件，该事件结束后重新武装。
- **侧栏强当前态**：当前 space 与当前会话都用品牌左条 + 底色 + 加粗标题。
  每个 space 下有默认折叠的「已退出 (n)」分组，提供 **恢复**（既有 resume 能力）
  与 **删除**（`DELETE /v1/instances/{id}`，确认文案「删除会话及其记录？」）。
  该路由已由 x-ttyhub 落地（main `288fde4`）：终态实例直接删除，运行中实例返回
  409，需 `?force=1` 由 Hub 先停止再删除——因此「停止并删除」只发这一个请求，
  客户端**不再**自行先 close（否则与 Hub 的停止相互竞争）；重复删除返回 404，
  按幂等成功处理。响应 `nodePurge` 非 `purged` 时 Hub 记录已删、主机侧数据待
  清理，提示如实说明，不谎称已全部清除。前端不再保留「隐藏」退化路径。
- **活动 tab** 使用品牌下划线 + 底色 + 加粗，深浅主题均有足够对比；键盘焦点环
  沿用全局 `:focus-visible`。沿用 Night Corral tokens，400px 可用。

证据见 [tabs-1.md](./evidence/tabs-1.md)。

## D-034

**2026-09-17 · M1 batch 6 co-lanes：gate verify/land 通过项目 lane 上的 Node 落地（gate queue + lane runner + D-034）**

| 日期 | 2026-09-17 |
|---|---|
| 状态 | adopted |
| 相关 | D-028 §13.6、D-031、[coordinator-hierarchy.md](./coordinator-hierarchy.md) §3.2/§8 批 6、[remuda-cli.md](./remuda-cli.md) §134-173 |

**背景**：r-mergequeue/merge2 已在 `crates/src/cmd/merge.rs` 实现
`remuda merge <branch> --gate --onto/--land`（临时 worktree 内跑权威
`scripts/ci/gate.sh`、verify-only 报告落
`<gitdir>/remuda/merge-reports/`、land-only 消费报告做 CAS、退出码 1 门禁失败 /
2 冲突 / 3 cas_lost / 2 base_moved、`--queue` 多 lane 乐观队列）。批 6 之前，
批 5 的人工回路靠两个 shell 脚本：Mac 上 `rgate2.sh` 串行加锁、scp、ssh 到
bolt-devbox-sg 跑 `remote-gate.sh`，后者在共享 checkout 里手动 fetch/ff 分支、
拒绝 worker worktree 的 stale tip、`flock e2e.lock` 跑 merge CLI，成功后再由
Mac 把远端 sha 推上 origin/main。需要把这条链路产品化：Hub 排队、lane 主机上的
Node 执行，coordinator 只用 `remuda gate/land/watch/report`。

**决策**：

1. **Hub 拥有队列（`gate_jobs` 表，doc_json 行）**：作业 `{branch, mode:
   verify|land, web: auto|always|never, laneId?, requestedBy, state:
   queued→running→passed|failed|landed（canceling/canceled 旁路）, steps[],
   hostId, queuedAt/startedAt/finishedAt, base/head/merge/currentMainSha,
   attempts, thenCommand/thenOutput}`。verify 可跨 lane 并行；land 按项目串行
   （`ProjectGate.landSerialization = global-cas`）：调度器 FIFO 遍历每个项目的
   queued 作业，只要有更早的 land 在跑，后续 land 不派出，verify 仍可填空闲
   lane；一条 lane 同时只跑一个作业（Hub 调度 + Node lane 锁双保险）；队列顺序
   按 `queuedAt` 严格 FIFO，前一个派不出去就 break 保持顺序。Hub 重启时把
   running/canceling 的作业 reconcile 回 queued。
2. **Node 跑的是它自己的 `remuda` 二进制，不是重写 merge**：lane runner
   （`remuda-node/src/gate.rs`，`gate.run`/`gate.cancel`/`gate.then`）在 lane
   checkout 内调用
   `remuda merge <branch> --onto main --gate [--land] --json --repo <lane>
   --target-dir <lane-target> [--web --web-e2e] [--no-push]`。gate 步骤权威仍在
   合并 worktree 内的 `scripts/ci/gate.sh`（remuda-cli.md §134-137），Node 只
   承担 remote-gate.sh 的 lane 机械动作：`git fetch origin`、把本地 main 对齐
   origin/main、`git branch -f <br> origin/<br>`，分支被 worker worktree 占用时改
   为在该 worktree `merge --ff-only`，之后本地 tip ≠ origin tip 即以
   `stale-tip` 拒绝（rc 70 的产品化），不 gate 一个 diverged tip。这样 r-mergequeue
   被 wrap 而非修改（`crates/remuda/src/cmd/merge.rs`、`merge/` 零改动）。
3. **global-cas 落地语义**：land 作业在一次 merge 调用里 verify+land（
   `--gate --land --onto main`）；merge 返回 `base_moved`（main 在 verify 后
   前进、CAS 失败）时，Hub 把作业重新置为 queued、清空 lane/host、下一个 tick
   在**新 main** 上重新 verify 再推，最多 3 次（`attempts`），超过记 failed。
   push 从 **lane 主机**发出（`push:true`，沿用 lane checkout 的 git 凭据），
   Mac/协调机不再经手 main 的推送——这正是 §1.1 I1「依赖 gating 而非信任」。
4. **流式结果用 Node 主动发起的 `gate.event`（不是一 RPC 多响应）**：协议上
   Hub→Node 的 JSON-RPC 一律一请求一响应（oneshot per id，32 in-flight），没有
   server-streaming。Node 在 broadcast
   `GateRegistry.events` 上发 `phase` / `step`（tail
   `<tmp>/remuda-mq-*/gate.jsonl`，gate.sh 每步 flush，400ms 轮询 + 去重）/
   `log` / `finished`，stdio carrier 用一个与 journal/tty 并列的 mpsc pump 发
   NDJSON `gate.event`，WSS carrier 用 tty pump 同款 socket pump；Hub 在
   `ws::handle_node_method` 加 `gate.event` 臂，校验发送方 host == 作业 lane host
   后落 `steps[]`/终态。`gate.run` 的最终回复与 `finished` 事件同一 verdict，
   `apply_result` 对 running 状态幂等，回复丢失也不影响终态。
5. **lane 环境在 Project 上设置一次，永不按 run 重打**：`ProjectGateLane` 新增
   纯增量字段 `env`（BTreeMap，CARGO_HOME/RUSTFLAGS/…）、`lockPath`
   （REMUDA_E2E_LOCK）、`pwEndpoint`（PW_TEST_CONNECT_WS_ENDPOINT +
   PW_CHANNEL=chromium）、`toolchainPath`（PATH 前缀）；原有
   repoPath/targetDir/ports/remote 不变。Hub 派出 gate.run 时整体下发，Node
   再补 CARGO_TARGET_DIR/CARGO_INCREMENTAL=0/VITE_NO_WATCH=1 和由 ports 推导的
   HUB_E2E_LISTEN/WEB_PORT；per-step 超时走 gate.sh 已有的
   REMUDA_GATE_STEP_TIMEOUTS，整跑预算由 Node 自己兜底（默认 1h）。无 tunnel：
   lane runner 只在 Node 本地 spawn，不提供任意命令执行。
6. **取消与超时**：`gate.cancel` 置 watch 标志并对该步骤进程组
   `killpg(SIGTERM)→SIGKILL`（子进程 `process_group(0)` 成组组长）；queued 作业
   取消直接 canceled（从不调 Node）；running 作业先 canceling、Node 以
   `canceled` verdict 收尾。整跑超时同样 kill 整组并 failed。
7. **`remuda land --then "<cmd>"`**：land 成功后 Hub 通过项目 homeHost 的 Node
   发 `gate.then`（`bash -lc`，独立超时，输出截断 16KB 落 thenOutput），用于
   demo refresh；命令不经过 lane 主机。
8. **carrier 偏好（dispatch 附带项）**：`remuda dispatch --carrier
   native|herdr|print`；默认看 Node hello 里
   `capabilities.driverInventory[shell-pty].launchable`（D-028 已上报），可用即
   shell-pty，否则 herdr；print 只在显式 `--carrier print` 时选，绝不静默降级。
   旧 `--driver` 细粒度覆盖保留、优先。
9. **CLI**：`remuda gate <branch> [--web auto|always|never] [--lane] [--wait/
   --no-wait]`、`remuda land <branch> [--then]`、`remuda gate list [--state
   …] [--branch]`、`remuda gate cancel <gjb|branch>`；等待时按 merge
   `--gate --json` 的同款 `name: status (duration ms)` 行实时打印，exit code =
   outcome（passed/landed 0，canceled 2，其余 1）。`remuda watch` 增加活动 gate
   作业行，`remuda report [--for-owner]` 增加 gate 队列活动/失败与 lastLandedSha。
   转换审计落 `audit_log`（subject=job id，detail 带 projectId/branch/mode/state/
   mergeSha），即「每次转换 journal 为 project-scoped observation」——复用审计流
   而非新增观测类型（协议没有 project-scoped observation，§5 co-watch 也是行内
   状态 + audit；此处同构）。
10. **为什么不让 merge queue 直接承担跨机调度**：r-mergequeue 的 `--queue` 是单
    checkout 本地队列（文件 flock + lane CLI 偏移），没有主机在线判定、没有跨
    checkout 的 lane 概念、也没有取消/推送回执通道；这些属于 Hub 编排层，merge
    保持纯本地原语。Hub 队列调度 + Node lane runner 的分层让 merge.rs 可继续被
    本地 CI/人工直接调用，语义不分叉。

**影响**：新增 `remuda-protocol/src/gate.rs`（GateJob/GateStep/枚举/gate.* RPC
线类型）、`remuda-hub/src/gatequeue.rs`（表/路由/调度器）、
`remuda-node/src/gate.rs`（lane runner）、`remuda/src/cmd/gate.rs`
（gate/land CLI）；`ProjectGateLane` 纯增量 4 字段；`merge.rs`/`merge/`
零改动；agent 路由白名单、OpenAPI、gen-types/gen-api 同步；4 个分发面
（server.rs、hubnode_codec.rs、runtime_wss.rs、stdio.rs）与两 carrier 的 event
pump 均接好。测试：hub `tests/gate_queue.rs`（并行 verify、串行 land、CAS
reverify、FIFO、queued/running cancel、输入校验 8 例）、node gate runner 7 例
（假 merge 二进制 + 真 git fixture：stream/stale-tip/lane-busy/cancel
killpg/超时/argv）、CLI `tests/gate_cli.rs` 5 例。真实 proof 见
[gate-lane-1.md](./evidence/gate-lane-1.md)。

## D-035

**2026-09-17 · `claude-print` 降级为「显式指定的诊断 carrier」：禁止作为任何默认或回落值**

| 日期 | 2026-09-17 |
|---|---|
| 状态 | adopted |
| 相关 | D-028、D-028a、D-034、[native-pty-first.md](./native-pty-first.md) §5.1、[coordinator-hierarchy.md](./coordinator-hierarchy.md) §2.4、[dispatch-driver-1.md](./evidence/dispatch-driver-1.md) |

**背景**：2026-09-17 一次 `remuda dispatch --harness claude` 打到 devbox-sg
（`ssh-stdio` Node，`REMUDA_PTY_CARRIER=native`），roster 行与 Hub instance 记录
都写 `claude-pty`，Node 却跑 `claude-print`：journal 的 `source.driverKind` 是
`claude-print`，进程是 `remuda node --stdio` 的直接子进程、argv 为
`claude -p --input-format stream-json`，且**没有任何 journal 记录说 driver 被换过**。
owner 判定 print 作为 worker carrier 无用——一个 session 跑完一轮就结束，必须手工
resume——因此要求把 print 从**每一条**默认与回落路径里剔除，而不只是 `driver_for`。

**根因**（两层，互不依赖）：(a) Hub 的 `worker_launch_spec` 用 `json!` 造 spec，
未设的 `Option` 全部以显式 `null` 落到线上（`prompt` 在派发路径上恒为 `null`，
brief 走附件），而 Node 侧 `CreateInstanceRequest` 的 `#[serde(default)]` 只覆盖
**缺失键**、不覆盖显式 `null`，于是整结构解析失败；(b) `dispatch_create` 的
`Err(_)` 分支把整个 spec 丢掉、换成硬编码 `driver: ClaudePrint` 的默认请求，而
`apply_spec_launch_fields` 不回读 `kind`/`driver`，所以请求的 carrier 就此消失。
同时 `driver_for` 只看 herdr 广播、从不读 `capabilities.driverInventory`，`shell-pty`
永远不可能成为默认，`else` 分支还把 print 当成默认。

**决策**：**(1) Hub** —— `driver_for` / `select_carrier` 的默认顺序为「宿主
`driverInventory` 报 `shell-pty` launchable → `shell-pty`；否则 herdr 已广播 →
`claude-pty`；两者皆无 → 以理由拒绝」；`driver_for` **不再**在 `select_carrier`
失败后回落 print。显式 `--driver` / `--carrier` 一律照办或以理由拒绝，绝不静默替换。
`POST /v1/instances` 省略 driver 时回落 `claude-pty`（多轮 TUI，且非 shell driver，
不改变该请求既有的 agent 审批门槛）。**(2) Node** —— 请求的 carrier 解析/构造失败
时一律 `instance.create` 带 reason code 拒绝（`unsupported-driver` /
`unsupported-kind`），**不降级**；拒绝发生在命令被持久接受之前
（`DriverRegistry::is_registered`），否则 Hub 会留着一行「运行中」而产品从未启动。
**(3) print 只可显式选中**（`--driver claude-print` / `--carrier print`、web picker
显式选择），并在该处标注为诊断/legacy；web New Session 默认跟随宿主
`driverInventory`，`prefs` 空值不再预置 print。**(4) 名册与实例记录一律记录 Node
create 结果里实际跑起来的 driver**（`reconcile_instance_driver` 在
`forward_if_online` 里回写），`remuda watch` 打印该列——Hub 请求值与实跑值不允许
再无声明地分叉。**(5) 派发失败回收** —— 任一步在 `worker.provision` 之后失败时，
dispatch 对其已 provision 的 worktree / target dir 调 `worker.remove` 回收（该泄漏此前正是因 400 落在 provision 之后、roster 行存在之前而泄漏、只能手工清理）。

**影响**：D-028 的「print 按条件退役」提前落地为「显式才可用」；D-034 的
`--carrier print` 语义与之一致（print 仅显式可达）。证据
[dispatch-driver-1.md](./evidence/dispatch-driver-1.md)：本机真实派发在
`shell-pty` 上端到端成立（roster / instance / journal `driverKind` /
`remuda watch` / `instance read --source screen` 可读屏），显式 driver 被 409 拒绝且
CLI 非零退出，`carrier-not-enabled` 的 stdio Node 以 `unsupported-driver` /
`unsupported-kind` 拒绝而非替换成 print。
