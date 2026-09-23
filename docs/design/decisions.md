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
| D-037 | 2026-09-19 | **新增 `claude-sdk` 作为 Claude 的结构化 carrier：与 print 同一条 stream-json/stdio 传输，只少一个 `-p`。** `-p` 是 "Print response and exit"，print 一轮就结束、当不了 worker carrier（D-035）；去掉它之后 stdin 跨轮保持打开，每次 `send(Prompt)` 就是再写一行 `user` NDJSON，非交互来自 piped stdout 而非该 flag（print-replacement.md §1.3/§2.1）。不 vendor SDK、不进 agent loop（D-002）、默认 `native-rust-wire`（D-003 的 `claude-sdk-sidecar` 仍只是跟不上官方变化时的退路）。mapper/`handle_can_use_tool`/`prompt_content` 的 base64 图片（D-027）/`usage_from_result` 全部**参数化复用**而非分叉，print 行为不变（内容观测仍是 `Structured`；只有 sdk 的流式增量报 `Partial`、最终块为权威，D-028a item 3）。它是 kind `claude` 的**第二条 carrier**、不是挂在 `shell-pty` 上的结构化源：该实例**没有 Terminal view**（stdio 不是 PTY），`SignalTier::None`，`shell-pty` 仍是 D-028 的默认双视图 carrier。显式 only——工厂注册与 `(claude, claude-sdk)` 矩阵到位，但不碰任何默认/回落路径（D-035）。`dontAsk` 直接拒绝而不继承 print 的 M0 auto-deny 负债（§2.8）。**steer / queue / interrupt 一律留 `unknown`**：cancel 已接到原生 `control_request`/`interrupt`，但 CI 里只有 fake 按脚本 ack，那证明 Remuda 写出了请求、不证明真实 CLI 掐断了一轮。M2 负责：print 退役、§2.4 的 suggestions 白名单与 launch-bypass 拒绝、AskUserQuestion 的活链路、§4.2 网关探针（在探针跑之前 gateway-on-sdk 留 unknown）。 | 用户（owner ask 2026-09-18）+ coordinator 派发 c-sdkdriver | [print-replacement.md](./print-replacement.md) §1.3/§1.6/§1.7/§2.1-§2.8/§3；[evidence/claude-sdk-1.md](./evidence/claude-sdk-1.md)（真实 Haiku 两轮同进程）；D-002/D-003/D-026/D-027/D-028/D-028a/D-035 |
| D-038 | 2026-09-19 | **会话列表行 = 状态点 + 标题 + 一句下一步；三维 wire 与 `ins_` 退到展开/tooltip；行内遥控收进溢出菜单**（workbench UX 批次 P0-6）。行内默认只出现状态点（§2.1 三维投影）、标题、kind 芯片、徽标与一句「下一步」；`lifecycle`/`activity`/`connectivity` 原文三元组、`ins_` 短码、`driver`、`model` 退到每行 `<details data-testid="session-wire">` 或 `title`。那句「下一步」只投影已有字段（`projectStatus` / `Interaction.request.kind\|description` / `exitLabel` / screen 终态），不新造状态机、不猜成功；`connectivity ≠ connected` 或 `lifecycle ∈ {unknown,reconciling}` 一律产出「状态待确认」，绝不回落成正向文案。行内遥控收进长按/溢出 `Sheet`（保留既有 testid 与命令路径），「待处理」组在行上保留一个主操作。**这条不是新要求**：§2.1 的线框本来就只有「点 + 标题 + 一句」，当前代码是偏离规格的一方。 | coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, 决策 5「收进 Sheet，不删能力」） | [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.3（Moshi 同位置放事件原文与错误文案）、§11.7 P0-6 行（**READY（规格反而要求）**）；[ui-spec.md §2.1](./ui-spec.md)；D-024 addendum |
| D-039 | 2026-09-19 | **触控命中只靠热区；`.meta` 统一 `var(--text-aux)`，桌面手机同值；禁止在 `session.module.css` 新增裸像素命中尺寸**（workbench UX 批次 P0-2 / P1-8）。命中尺寸一律走热区（`::after` / padding），视觉字形尺寸不变（20px 返回箭头、25–30px `terminal\|structured` 分段、32px 手机 Stop 继续是这些尺寸）；新增命中尺寸一律 `var(--touch)`；可点区域不得互相重叠。`.meta` 一类辅助/诊断文本桌面与手机统一 `var(--text-aux)`（12px），用 token 而非字面量；12px 下限出自 `tokens.css` type scale 的块头注释。compact（布局）判定与 `coarsePointer`（触屏）判定不得混用。反例：把 `.back` 的 `width` 改成 44px 来「满足 44px」——那是把字形撑成方块，规格要的是命中面。 | coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, 决策 2 按规格正文取热区那句） | [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.6（两处措辞）、§11.7 P0-2 行（**NEEDS_DESIGN**）与 P1-8 行；[ui-spec.md §3.4](./ui-spec.md)（新增）；热区先例 `session.module.css` `.effortIconBtn::after`；D-024 |
| D-040 | 2026-09-19 | **`/s/:id` compact 铬预算：space chips 折成单芯片入顶栏；诊断 meta 进「运行详情」disclosure；分段与 Stop 不得进 ⋯**（workbench UX 批次 P0-1 / P0-5）。§1.4 的「约 400px 显示可横向滚动的 space chips 与 tabs」限定到列表路由；`/s/:instanceId` 及子视图在 compact 下允许折成单枚当前 space 芯片并入顶栏，点开同一个抽屉（`spaces-drawer-open` 行为与 testid 不变）。§2.2 header 从「规范两行诊断」改为「主行 + 可折叠**运行详情**」，ASCII 图同步重画：主行保留 host 芯片 + cost + 状态点 + 分段 + Stop，其余诊断进 disclosure（默认收起、展开态按设备 `localStorage` 持久化、`session-meta` testid 保留）。`terminal\|structured` 分段与 Stop **始终留在顶栏，永不进 ⋯ 溢出菜单**。 | coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, 决策 1 与 2） | [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.7 P0-1 行（**CONFLICTS**，且指出该建议与自己矛盾）与 P0-5 行（**CONFLICTS**，修订前 `:235` 误写作 233）；[ui-spec.md §1.3 / §1.4 / §2.2](./ui-spec.md)；D-024、D-028 |
| D-041 | 2026-09-19 | **工具卡默认折叠的豁免集合与折行最小信息量**（workbench UX 批次 P1-10 / P1-20）。默认折叠**只对 compact 且已 settled、且 family ∉ {Workflow, error}** 的卡生效；`interaction.*` 同样豁免；running / 未 settled 的卡永不折叠。折行内容 = family + **关键参数**（Bash = 命令首行、Edit/Write/Read = 路径），按宽度截断且 `title` 给全文，裸 `Bash` 不合规。**折叠分支必须在 family 判定之后**才可提前 return——`ToolCard.tsx` 的 `if (folded) return …` 早于 `family === "Workflow"` 分支，一律默认折叠会让 `WorkflowTimelineCard` 永不挂载、1Hz 走针不启动；验收硬指标是卡头 elapsed 在运行中递增，不是「卡可见」。桌面默认态不变，展开后与桌面完全一致（同一组件、同一 testid）。 | coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, 决策 3） | [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.7 P1-10 / P1-20 行（**CONFLICTS**，点名 `ToolCard.tsx:260` 早于 `:281-292`）；[ui-spec.md §2.2](./ui-spec.md)；`toolPresenters.ts` Bash 分支给出折行参数 |
| D-042 | 2026-09-19 | **手机 composer 单行 + 选项 sheet 的边界：触发器带 mode 词与档名，三态与诚实标注留在外面**（workbench UX 批次 P0-3）。compact 下 control bar 允许收成一个选项触发器，但触发器**必须**同时显示当前 `permissionMode` 的词与 effort 档名（形如 `manual · high`），`danger` 模式必须在触发器上可见——把 bypass 态藏进 sheet 是本条最响的禁止项。可进 sheet：附件、harness 只读芯片、context 用量、权限选择器、effort 滑杆。**不得**进 sheet：发送/排队/打断三态按钮、队列 chip、「尚未验证」标注。placeholder 分平台；插队与 Esc 打断的 `window.confirm` 换 `Sheet`（桌面 `popover`、手机 `sheet`，焦点圈定、Esc = 取消、返回焦点到触发器），确认后的命令语义与 `commandId` 路径完全不变；桌面布局与 testid 零变化。 | coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, 决策 4） | [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.7 P0-3 行（**CONFLICTS**）；[ui-spec.md §2.2](./ui-spec.md)；D-028a（三态 / 队列 chip / 「尚未验证」） |
| D-043 | 2026-09-19 | **Grok 结构通道以 TUI 落盘的 ACP 帧为权威，文件层按 protocol §5.7 翻译：工具稳定名取 `_meta["x.ai/tool"].name`（缺失即 unknown/not-emitted，绝不用人类 `title` 冒充；title 只作 `display_title`）；`category` 先走设计文档 §3.1 名字表、再按帧 `kind` 回落、最后 Other，不再恒 Shell；无 `status` 的 `tool_call_update` 与 `in_progress` 都是同 node 的 Running（`Replace`，revision+1，缺字段保持旧值）；只有 `completed/failed/denied/cancelled` 是终态（发 Final，revision 严格大于最后一次 ToolCall），`pending` 与未知 status 非终态、保持 opaque 不关节点；`content[]` 的 `content`/`diff`/`terminal` 分别译文本块/`FileChange`（Applied 仅 completed 且无 error）/terminal 引用文本标记，非 text 内容不再静默丢弃，且 `rawOutput.output_for_prompt` 只在没有 content 型文本块时兜底；thought 收尾改 `Close`+全文。零协议增量、不叠加第二条 grok-acp 进程（D-028）；共享的 `adapters/mod.rs` 构造函数不动，codex 字节级不变。`turn.live` file-tier 相位、question 升格 interaction、terminal log tail、1.0.34 实帧相关的 workflow/子代理下钻（PR8）不在本决策内；合成帧以 [U] 标注。 | coordinator 派发 c-grok-toolid | [grok-structural-translation.md](./grok-structural-translation.md)；[protocol.md](./protocol.md) §5.7 grok-acp；D-028 |
| D-045 | 2026-09-19 | **computer-use 能力按次授权与投递（capability grant and delivery）**。新增能力名 **`computer-use`**：它**不是** host 属性、不是 driver 属性、不是全局开关，而是**按会话、按次、显式申请**的授权——只有 `remuda instance create` / `remuda dispatch` 上显式的 `--capability computer-use` 能授予，任何默认路径都不授予、不继承、不因「装了就可用」而生效。**两道门都必须过**：(1) 来源必须是 Human 或 Bot，`LaunchOrigin::Agent` 一律拒绝——一个 agent 永远不能为自己铸出桌面控制，门形照抄 `presets::merge_yolo_argv` 的双门（`crates/remuda-driver/src/presets.rs:151-166`）；(2) 目标主机的心跳 `cli[]` 必须回报已安装的 `computer-use` 行（[codex-cua.md](./codex-cua.md) §3.4），未回报即拒绝。**投递只有两条，两条都不碰操作员自己的配置**：(a) 仅当本次 launch 的 native home 是 **Remuda 管的**时，把 skill 字节**从 `remuda` 二进制内嵌物**化到 `<native_home>/skills/codex-computer-use/**`（目录 0700、文件 0600）——「受管」**逐 kind 定义**：claude 是它的作用域 config dir（**继承来的 `~/.claude` 不算受管**），codex 是 `<launch_dir>/codex-home`，grok 是 `<launch_dir>/grok-home`；**门不是 `inherit_default_config`**——那是 claude 专属标志，对一次 codex launch 同样为真（`crates/remuda-node/src/native.rs:238-240`），拿它当门会在 codex 上给出错误答案。继承来的操作员 home **只读不写**，即 `~/.claude`、`~/.codex`、`~/.grok` 在**任何**分支上都不会被打开写（`crates/remuda-driver/src/launch/overlay.rs:25-28` 是这条的权威表述，也是本决策存在的理由）；**codex 与 grok 本批只拿腿 (b)**：仓库与 skill 都没有证据表明它们读任何 skills 目录，往影子 home 写 skill 树没有读者，所以它们的投递**就是那份 per-instance MCP config**。(b) 一律写 per-instance `<launch_dir>/mcp-cua.json`（0600），并**按 `AgentKind` 而非 driver 挂载**：claude 走 argv `--mcp-config <path>`（`mcp-config` 早已在四族白名单且取值，`crates/remuda-driver/src/flags.rs:64,82,89,333`），codex 走 shadow `config.toml` 的 `[mcp_servers.codex-computer-use]`（`crates/remuda-driver/src/launch/shadow.rs:110-168`）——`shell-pty` 是 kind 多态的，跑 claude 时走 argv、跑 codex 时走 shadow home，**规则随 kind 不随 driver**。**环境变量握手**：`c-cua-launch` **只在能力被授予时**向子进程注入 `REMUDA_CAPABILITY_COMPUTER_USE=1`（硬化的 `launch-cua-repl.sh` 未见它即拒绝启动）；该变量是**信号不是边界**——任何有 shell 的东西都能自己导出它，**真边界是「没有授予就绝不物料化」**。**绝不发 `--strict-mcp-config`**（该 flag 被永久禁用，`flags.rs:15`）：Remuda 提供的能力只**增加** server，绝不替换 agent 自己的 server 集合。物料化文件带 digest 进 `LaunchRecipe.materialized_files`（`crates/remuda-driver/src/recipe.rs:157-179`），所以 launch 审计能说清这次到底授予了什么。**拒绝**各有独立消息：Agent 来源、主机未回报、非 macOS、launcher 缺失，以及 **`bypassPermissions` 与 `computer-use` 在同一次 launch 上同时申请**——无人值守的桌面控制叠加跳过的工具审批，是唯一没有回收路径的组合，两者同时出现即拒绝（不是「忽略其一」，也不是需要另一个隐含 flag）。所有拒绝都发生在 create/dispatch 被持久接受**之前**，绝不静默降级、绝不静默丢弃能力请求（同 D-035 的拒绝形状）。**本决策同时记下截图两条**（计划里曾分别为 D-038，因 id 已被占，合并入本行并加此标记）：(1) **截图进 journal 与 web** —— tool result 合法携带 `image` content block，字节只存对象库（Node 用宿主 token 走 `POST /v1/hosts/{id}/files/objects` 暂存，`crates/remuda-hub/src/host_files.rs:44`），`ContentBlock::Image` + `MediaBlock` 协议早已合法（`crates/remuda-protocol/src/observation.rs:143-155,189-195`）；**只发文本的 producer 是 bug，不是策略**（journal `crates/remuda-journal/src/claude.rs:1396,1470-1484`、driver `crates/remuda-driver/src/adapters/mod.rs:387-406`、web `web/src/features/session/toolPresenters.ts:44-51` 三处今天都丢弃非文本）；**不新增 `ObservationKind`**；截图**不是 artifact**，只有 agent 显式存盘才发 `artifact`；渲染规则见 ui-spec §2.2（卡内定高缩略图、`loading="lazy"`、点开 `/v1/objects/{id}`、绝不自动展开）。(2) **截图保留期** —— 沿用既有附件对象的生命周期与过期，**永不内联 journal、永不写日志**，提交的证据文档必须用脱敏或合成屏；更短的 CUA 专属 TTL 是 Hub 旋钮与第七个任务，不在本批。 | coordinator（批次 cua） | [codex-cua.md](./codex-cua.md) §2/§3/§4/§6；各条约束的 file:line 见该文 §3.2；Q1–Q6 默认值见其 §8 |
| D-046 | 2026-09-19 | **CUA 交互路由：`elicitation/create` 由 worker 自己答，但回答权被 launch 请求约束**。事实基础：agent 侧今天**只有一条** elicitation 桥，且只搭在 Claude 的 hook 事件上——`Elicitation` 已是注册事件、阻塞、可带 `action` 回复（`crates/remuda-signal/src/event.rs:114-135`），已能生成 `Interaction{kind: elicitation}` 卡（`crates/remuda-signal/src/approval.rs:153-208`），也能从 `InteractionAnswer::Elicitation` 回一个动作（`crates/remuda-signal/src/bus.rs:1017-1030`）。**但这条链路够不到 MCP 的 `elicitation/create`**：`cua-repl` 是 stdio MCP server，它的 elicitation 走 MCP 协议本身，而 codex shadow 的 `hooks.json` 只注册 `SessionStart` 与 `PermissionRequest` 两个事件（`CODEX_EVENTS`，`crates/remuda-driver/src/launch/shadow.rs:40-45`；grok 的 `GROK_EVENTS` 同样不含 elicitation 类事件，`shadow.rs:47-55`），**两者都不是 MCP 的 `elicitation/create`**，所以**今天没有任何路径能把一个 MCP server 的 `elicitation/create` 送到 `/approvals`**（缺的是一个从 harness 到 Hub 的 producer，不是 `InteractionCarrier` 取值——那个枚举已有七个值，见 [codex-cua.md](./codex-cua.md) §5.3）。因此本批的合同是：**worker 自己在 cua-repl 的 `initialize` 里声明 `capabilities.elicitation` 并自行回答**——按应用审批时 `accept` 且 `persist: session`，而**回答权被本次 launch 的请求约束**：只有请求中点名的 bundle id 可以批准，未点名的应用一律不批（`skills/codex-computer-use/SKILL.md` 的既有规则，本决策把它从「skill 的自律」升级为「能力授权语义的一部分」）。**同时如实记账**：这条路线意味着桌面审批没有 journal 行、没有 `/approvals` 卡、没有第二人复核，只有 worker 的一面之词——这是本批明确接受的代价，写进 D-045 的「后续」而不是假装它已解决。**后续**（不在本批，前置是一条 MCP 级 elicitation 桥：为不经过 Claude hook 的 harness 造 producer，并给它一个 `InteractionCarrier` 取值）：把回答权从 worker 交回人，`/approvals` 成为 CUA 审批的唯一出口。在那之前，`D-045` 的 bypass 拒绝与「只批准点名应用」共同构成唯一的边界。 | coordinator（批次 cua） | [codex-cua.md](./codex-cua.md) §5；[native-pty-first.md](./native-pty-first.md) §5 P5 残留（原文：`Elicitation` 仅按二进制读取端形状实现，**未取得实机 payload**）|
| D-047 | 2026-09-19 | **模型 API 交付方式可选：`direct`（默认）或 `via:<hostId>` 经由某台机器出去（路由子模式 `auto`/`hub-relay`/`direct-net`）；拒绝而非改道（refuse-never-reroute）。** profile 上是 `ProviderProfile.delivery`（嵌套对象，缺省 = `{mode:direct, route:auto}`），逐次派发是 `apiVia`（字符串：`<hostId>` \| `self` \| `none`）加可选 `apiRoute`；瀑布 = 请求 > 项目 > profile > `direct`，在**放置之后**解析（`via:<W>` 收敛为 `direct`）。H 的属性 `relayBind` 未配置时 W 与 H 之间无直连路径，只能走 `hub-relay`。决议**只在启动时做一次**并写明在实例上；会话中途直连失败**不**静默切到 hub-relay，请求失败、实例报 `blocked{api-route-down}`。D-031 仍然成立：不装不探任何隧道工具、不监听非 loopback（除非操作员显式配置 `relayBind`）、单一固定 origin、实例作用域、字节走既有链路。 | coordinator（实现方，承接 owner 2026-09-19 指示） | [api-routing.md](./api-routing.md) §2/§3/§4/§5/§8、D-031、D-021、D-035；task 2（Hub 侧）已落地：瀑布解析、4 个拒绝码、`api.*` 流表与 Hub 进程内 egress、Node echo 校验投影、凭据仅向 H 释放；Node 侧 listener/egress 与 CLI/Web 在 tasks 3–6 |
| D-048 | 2026-09-19 | **`api.*` 带内流类（`api.open`/`body`/`head`/`chunk`/`end`/`cancel`/`credit`）承载代理请求，与 `object.pull`/`object.chunk` 同构。** 全部是 notification，**自带每链路 stream 注册表**，绝不进 Hub→Node 的 32 槽 pending map（否则几条流就卡死 `instance.create`/`tty.write`）；新增 `TransportLimits.maxApiStreams`（默认 8/链路、2/实例）与 `apiChunkBytes`（默认 64 KiB 原始 ≈ 87 KiB base64，远低于 1 MiB 帧上限）；生产者每流最多 4 个未确认 chunk，消费者以 `api.credit` 放行，块间 `yield_now()`；SSE 必须合并（≥16 KiB 或 ≥50 ms 或流结束），body 是**不透明字节**、不按 SSE 解析。 | coordinator（实现方） | [api-routing.md](./api-routing.md) §7、protocol.md §7.4 object.pull 先例、ws.rs 出站队列容量 32 |
| D-049 | 2026-09-19 | **手机优先 UI：受限 `/m` 路由树 + 会话本体不分叉 + 视口重定向层 + `start_url: /` + badge/权限横幅口径 + 平台听写真名。** (1) **受限路由树**：同一 Vite PWA 内新增手机优先路由树，`/m` **只拥有导航与首页级信息架构**——`/m`（会话 home）、`/m/inbox`（收件箱）、Jump To sheet、phone 底栏（会话·收件箱(n)·新建·更多）；桌面路由零改动，不做独立客户端、不做原生 app。(2) **会话本体不分叉**：手机打开会话仍是共享的 `/s/:instanceId`（及 `/tty` `/structured` `/files` `/events`），`/sessions/new` `/login` `/pair` `/settings` 同样共享；改善靠该路由的 compact 形态（D-040/D-041/D-042 + ui-spec §4.7 铬预算：一条顶栏 `--top-mobile` 52px + 一条底栏 `--bar` 64px、正文 ≥ 60% 视口、截断优先级=标题→space 芯片→状态文字、状态点恒在主行、`终端\|结构` 分段与 Stop 永不截断/进 ⋯；§1.4 chips 折单芯片 + tabs 行收起；**host 芯片与 cost 仅 compact 折入「运行详情」，D-040 桌面主行规则不变**），不复制 transcript。(3) **重定向与深链**（`<Navigate replace>`）：compact 下 `/sessions`→`/m`、`/approvals?focus=`→`/m/inbox?focus=`（**query 原样保留**）；桌面下 `/m*`→`/sessions`；**`/s/:id` 永不重定向**（两套壳下同一条路由）；`/sessions/new` `/login` `/pair` `/settings` 两侧不重定向。(4) **`start_url` 从 `/sessions` 改为 `/`**，由重定向层按视口判定落点（手机装的 PWA 落 `/m`，桌面落 `/sessions`），一份 manifest、一份 SW。(5) **badge**：推送 payload 增加一个**可选**整数 `badge` 字段（该设备 pending interaction 数），SW 调 `setAppBadge`/`clearAppBadge`，无 Badging API 则什么都不做、不用通知条数冒充，字段缺失行为逐字节不变；**不新增推送事件类型**。(6) **权限申请不在启动时弹**，只在 `/m/inbox` 顶部横幅（用户手势触发）与设置→通知两处；iOS 未加主屏时横幅文案改「先加到主屏幕」。(7) **语音**：平台键盘听写优先、先成文再发送（`composing()` 守卫对听写同样生效）、Web Speech API 仅作可用时增强且默认关、不做云转写/录音上传/协议字段、iOS Safari 无 `SpeechRecognition`（写进设置文案）、终端段不提供语音。(8) **里程碑**：M1 = home、会话、收件箱、新建、登录、语音；键盘条、分组 Jump To、badge 与推送真机验证排 M2。取舍：会话路由 compact 去掉 app 底栏后「回家」靠 44px 返回键 + Jump To（ui-spec §4.7 / 计划风险 E9），若实测反感只回滚该条、不回滚整棵 `/m` 树。 | coordinator（mobile-ui 计划 section (B)/(D) D1–D8 默认值，所有者未拍板即按默认执行；任务 c-mspec 落规格） | [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §5（P0 能读能批能说 + 明确不做）、§6（落地顺序）、§7.1（home）、§9（SOTA 一句话「投影不是第二个 agent、终端一键可回不 fork」+「不要抄」清单）、§10-19（主分段）、§10-23（Jump To 不做第二套空间模型）、§11.2（分组/时钟/只搜标题）、§11.3（Inbox 两档、错误当正文、权限横幅）、§11.4（git 扫视串可学、硬裁 diff 不抄）、§11.5（端口/Kill 面板与隧道整体不抄，D-031）；[ui-spec.md §1.2/§1.3/§4.5/§4.6/§4.7/§4.8](./ui-spec.md)；D-026/D-028a/D-031/D-038/D-039/D-040/D-041/D-042；[evidence/mobile-ui-1.md](./evidence/mobile-ui-1.md) |

| D-050 | 2026-09-20 | **Task 优先模型：Task 是 instance 之上的聚合（非第二状态机）；目录绑定 `workspaceBinding{reuse\|pool}` + 唯一新表 `worktree_leases`（`mode`、复合键 `(host_id,workspace_id,dir_key)`、可空 `worktree_name`、`holder_instance_id` attach-lock）；reuse=顺序轮用、归还对目录零操作，pool=detached-HEAD 停放、租借切 `wt/<slot>/<task-slug>`、return-don't-delete（reset/clean/park 仅 pool）；既有回收路径（`remove_record`/`worker.remove_worker`/`delete_instance`/`retire_worker`）必须 lease-aware；看板 8 态→4 列只读投影，failed 按 `placement.is_some()` 归位 + 角标（不存 pre-fail 列），拖卡列→列多跳；门控用 grant 动词（create/set-state=`GrantVerb::Dispatch`、land=`GrantVerb::Land`，持 grant 的协调员 agent 授权，非「agent 一律 403」）；迁移预算=一个增量列 `archived_at`（落 `tasks.rs` migrate）+ 一张新表；批注=composer 草稿（零 wire）；任务空间=文件视图客户端过滤投影（零端点）；参考产品合规护栏（只泛称、开源项目可具名、不引述内部文档、截图只提交 Remuda 渲染） | coordinator（task-model 计划任务 1 t-spec，docs-only） | [task-model.md](./task-model.md) 全文；[ui-spec.md §1.5/§2.9](./ui-spec.md)；[evidence/task-model-1.md](./evidence/task-model-1.md)；D-024/D-033/D-035/D-047/D-049 |
| D-052 | 2026-09-23 | **provenance-first UI 批次口径（ui-upgrade 批次，docs 先合）**：(a) 本批零新 wire/表/端点，出处读既有 envelope（`seq`/`source`/`completeness`）与既有 `Interaction`，迁移预算零；(b) 审批卡**不显** confidence/risk 分（`ApprovalRequest` 无 `risk`，`web/src/types/generated.ts:98-106`）且**不在 UI 断言会话边界**（`DecisionOption` 无 `destination`，`web/src/types/interaction.ts:4-8`；harness 的 permission suggestions 实测含 `session` 与 `localSettings` 两种 destination）——线框 `risk`/`Always in this cwd` 改为 preview 原文 + carrier + deadline + harness 原范围标签，副文案统一「按 harness 建议的范围持续允许」，引 §3.3；(c) completeness 三值不变、仅活过 `FoldedToolRow` 折叠（interaction 节点/ApprovalCard 需先做 store 连接键调研，批次计划 D11 默认本批不做）；(d) D-041「折叠在 family 判定之后」保留并被回归断言守住；(e) `/board` 路由与三列只读投影已上线，本批只在既有面上 graft、不新建页面/路由，已完成列不暴露 land；(f) ledger 浅色主题由 D-053（任务 15）正式化，D-052 不处理主题；(g) `/approvals` 与 `/m/inbox` 收敛为 InboxShell 单壳，桌面三档/手机两档各自保留；(h) 新增 `--warn`/`--info`/`--text-xl`/`--text-13` 四个 token（双主题各一值、文本对 `--ink-2` ≥ 4.5:1）；(i) stylelint 按文件白名单 opt-in，白名单是带摘除批次的台账，「辅助文本 vs 图形标注」分类口径进 ui-spec §3.4；(j) 参考清单定性「MIT 组件画廊，不声明任何 spacing/type/colour 规则」，证据只用 Remuda 自身 390/1440 渲染 | coordinator（ui-upgrade 计划任务 1 c-uispec2，docs-only） | [ui-spec.md §2.2/§2.5/§2.9/§3.3/§3.4/§4.7](./ui-spec.md)；[evidence/ui-upgrade-1.md](./evidence/ui-upgrade-1.md)；D-002/D-024/D-035/D-038/D-039/D-040/D-041/D-042/D-045/D-046/D-049/D-050 |
| D-053 | 2026-09-23 | **UI 整体重做：角色颜色令牌 + 深浅双态（默认跟随系统，纯 CSS 解析）+ 同源系统字体与 720 阅读列 + 桌面单侧栏；终端恒深色**。取代 ui-spec §6「v1 只做 A」、D-052 第 8 条「不新增 z-index / elevation / 阴影 / disabled token」中的阴影部分（z-index/disabled 口径不变）；落实 D-052 第 11 条预留的浅色主题正式化；修订 D-024「内容上方 tabs / 可折叠 Spaces/Sessions 左栏」的面板位置与 tab 条出现范围，并修订 D-024 addendum「侧栏强当前态」的品牌左条（改为 `--bg-selected` 底 + `--fg-strong` 字 + 加粗，关闭语义不变，详见 ui-spec §1.4）、D-038 的会话列表宽行默认视口（≥960 单行另显「主机/工作区·分支」与相对时间两列，三维 wire/`ins_`/driver/model 仍退 `session-wire`）、D-040 (1) 的 compact 单芯片形状、旧 ui-spec §2.2（80db05b8 时 `:335`）的运行详情「第二行只有这一个触发器」版式（D-040 (3) 的 disclosure 内容/按设备持久化/`session-meta` 保留）、D-041 的「桌面默认态不变」、ui-spec §4.7 的底栏高度（64→56） | 所有者（重做授权与四项拍板）+ coordinator | [visual-system.md](./visual-system.md)、ui-spec §1/§2/§3.4/§4.6/§4.7/§6 |

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

## D-037

**2026-09-19 · 新增 `claude-sdk`：与 print 同一条 stream-json/stdio 传输，只少一个 `-p`**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted（M1 已落地：batch 1 + batch 2 + batch 3 的注册部分） |
| 相关 | D-002、D-003、D-026、D-027、D-028、D-028a、D-035、[print-replacement.md](./print-replacement.md)、[evidence/claude-sdk-1.md](./evidence/claude-sdk-1.md)、[evidence/print-sdk-research-1.md](./evidence/print-sdk-research-1.md) |

**背景**：D-035 已经把 `claude-print` 降为显式诊断 carrier——一个 print session
跑完一轮就结束，必须手工 resume，当不了 worker。但 print 还占着一个别人替不掉的
位置：**没有 TTY 的主机上唯一的结构化 carrier**，以及想要 `can_use_tool` 而不去
刮屏的调用方。`print-replacement.md` 把 VS Code 扩展的 Agent SDK `query()` 这条路
读完之后，结论是那个位置不需要 print：扩展的 argv builder 里**根本没有 `-p`**，
非交互来自 piped stdout（CLI `X$r()` 对 `!process.stdout.isTTY` 就判非交互），
stdin 跨轮保持打开，UI 的每一轮只是往同一个子进程再 enqueue 一行 `user`。

**决策**：`claude-sdk` = print 的传输减掉 `-p`。同一个 `initialize` 握手、同一个
mapper、同一条 `--permission-prompt-tool stdio` 审批通道；差别只有那一个 flag，
以及它带来的后果——子进程活过一轮。

**为什么是参数化而不是分叉**：print 的 mapper 是约 1000 行加一整套夹具。sdk 复用
`Mapper`（`driver_kind` 参数化）、`handle_can_use_tool`、`prompt_content` 的
base64 图片块（D-027）、`usage_from_result`。§2.8 点名过：sdk 若少了图片块或
usage，clipboard 与 usage 页面就是回退。唯一按 carrier 分叉的行为是 completeness：
sdk 的流式增量报 `Partial`、最终 assistant 块为权威（D-028a item 3），print 的内容
观测仍然全程 `Structured`（它的消费方与夹具就是按这个建的，本次没有任何 print 的
实测结论改变）。

**为什么是第二条 carrier 而不是挂在 `shell-pty` 上**：stdio 不是 PTY，交互式 TUI
也不吃这些 headless flag，一个子进程当不了两者；扩展自己就是两条互斥的启动路径。
两个子进程就是两个会话、两个 session id，不是「一个实例」。所以 D-028 的统一
（一个 agent session 就是一个 terminal session）继续指 `shell-pty`——Terminal 与
Structural 同步是 PTY 实例的事。`claude-sdk` 实例**没有 Terminal view**，UI 不该
给它空 xterm 或 `tty.attach`；journal 的 `driverHint` 直接说「没有 live screen」。
`SignalTier::None`：这条 carrier 没有 hook/file/OSC/screen 阶梯，stdout 不是
`Hook`，不许为了好看而升格。

**诚实的能力矩阵**（`capabilities.rs`）：`TtyAttach` / `LiveAttach` / `Artifact`
为 `NotProvided`（结构性缺失，是事实不是猜测）；`Resume` / `InteractiveApproval` /
`Question` / `CompletionNativeTurn` / `ModelSwitch` / `Fork` / `StructuredWorkflow`
/ `Hooks` 沿用 print 的原生证据，`Resume` 另有跨轮 stdin 佐证；**`Steer` 留
`unknown`**——一轮之中再写一行 `user` 可能是原生 queue 也可能是 steer，没实测就不
声称（D-028a item 2）。**`Queue` 与 `Interrupt` 也留 `unknown`，sdk 不开例外**：
`instance.cancel` 确实接到了原生 `control_request`/`interrupt`（而不是 `\x03`），
但 CI 里唯一的见证者是 `fake-claude`，它 ack 是因为脚本让它 ack——那证明 Remuda
把请求写出去了，不证明真实 CLI 掐断了一轮。接线不等于测量。

**权限**：bypass 与 host 两条路照搬 print；`dontAsk` **直接拒绝**，不继承
print 的 M0 auto-deny 负债（`TD-M0-PERM-01`）——默默拒掉每一次工具调用不是该带进
新 carrier 的行为，所以 recipe 的 `debt` 是空的。Bot/Agent 仍然不得 bypass
（D-011/D-017 的 `reject_bot_bypass` 与 `BypassNotAllowedForBot` 照常生效）。

**显式 only**：工厂注册进 `native_driver_registry`、`(claude, claude-sdk)` 进
`validate_kind_driver`，于是 `--driver claude-sdk` 能落到 driver；但没有碰任何默认
或回落路径——Node 请求的 `driver` 默认仍是 print，Hub 的 `select_carrier` 仍是
native → herdr 且从不选 print/sdk。D-035 的次序没有改。

**Resume**：仍是 D-026——不复活已退出的子进程，而是带 `--resume <id>` 起**新**进程；
session id 的权威是原生 `system/init.session_id`（mapper 抄到 `session`/`started`
lifecycle，Node 无需 driver 专用代码就把它抬进 `nativeRef`）；id 缺失或空是
`NativeSessionNotFound`，不是一段静默的空对话；永不 `--continue`。轮内的多轮**不**
走 resume，就是 stdin 上再来一行 `user`（§2.2）。

**M1 边界**。已交付：protocol 枚举 + 重生成的 schema/TS、两套 argv 模板（wire 与
materializer）、driver 本体与参数化的 mapper、能力矩阵、fake 的 turn barrier 与不带
`-p` 的 spawn、Node 工厂与矩阵、汇编器/进程/矩阵三层测试、真实两轮 live 证据；
交接复审一轮另补：`Driver::close` 的有界阶梯（stdin EOF 内含有界等待 → 子进程组
SIGTERM 内含有界等待 → 子进程组 SIGKILL 并回收；含 EOF 请求本身——子进程不再读 stdin
且 writer 通道写满时 `close_stdin` 自己也会阻塞；close 前 join/中止 reader，`exited`
恰好一次）与 web 侧的 `claude-sdk` 标签（此前 UI 把 sdk
实例显示成 print，等于告诉操作者"这个会话一轮就结束"）。

**M2 owns**：print 退役（夹具对账变绿之后）、§2.4 的 suggestions 白名单与
launch-bypass 拒绝、AskUserQuestion 的活链路验证、§4.2 网关探针——在那个探针跑之前
**gateway-on-sdk 留 `unknown`**，不作为 supported 发布。`onUserDialog`、subagent
drill-in、sidecar、IDE lock/MCP 都不在 M1（§2.9）。

**另有一项 M2 账（复审记账，不在本轮修）**：`map_result` 的 `affects_completion`
仍用 print 的启发式 `result_index > 0 && queued_turn_count == 0`。那条规则是为
「一个 print 轮次里 Workflow 先发 `result_index` 0 再发 1」写的；可是在活过多轮的
子进程上 `result_index` 是**按轮**递增的，于是它读作「第一轮之后每一轮都是终结」——
live 证据里第 1 轮报 `false`、第 2 轮报 `true`，两个都不对（第 1 轮确实完成了，第 2
轮也不是会话终点，子进程还活着并随后接了 `close`）。§2.5 的规则是「只有终结时才
`affects_completion`」。M1 没有任何下游在 sdk carrier 上依赖这个字段，所以先记账：
要修得先定义「对一个长寿子进程而言什么算终结」，那是 M2 的决定，不是同轮改动。
证据见 [claude-sdk-1.md](./evidence/claude-sdk-1.md) §6。

**close 的残留（复审记账，M2；详见
[claude-sdk-1.md](./evidence/claude-sdk-1.md) §9）**：有界阶梯只在 `close` **已经拿到
`inner.live` 锁之后**消除了「无界 wait」。锁本身仍是另一道门：`Driver::send` 持有
这把锁、一直 await 到 `send_user` 把一行写进 64 槽 writer channel 为止，而 Node 路径
（`crates/remuda-node/src/native.rs` 的 `DriverRequest::Send`）对这次 await **没有任何
超时**。于是对一个停止 drain stdin 的子进程，writer 通道写满后，先到的 `send` 会一直
占着 `inner.live`，后到的 `instance.close` 拿不到锁、阶梯根本开不起来——仍可能从这扇
门挂死。这不是本轮的有界 EOF 请求能覆盖的（那条只在 close 已持锁后生效）。**M2 的
所有权改造项**：给 `send` 加界（写通道超时），或让 `close` 以超时方式取锁 / 不持锁地
驱动终止（把 live 取出后在锁外跑阶梯、或控制操作走独立通道）。那会动到 `Live` 的
所有权形状，超出 M1。

**实测**：[claude-sdk-1.md](./evidence/claude-sdk-1.md) 在一台一次性本地 dev server
上用真实 CLI 跑了一轮 `LIVE`：`--driver claude-sdk` 被原样转发，进程表里的 argv
没有 `-p`，两次 `instance.send` 由**同一个 pid**、同一个原生 session id 服务，
journal 51 条全部 `driverKind: claude-sdk`，流式增量以 `Partial` 汇聚成
`Structured` 的最终块，两轮各有自己的 `result` 与 usage/cost。该文档同时列出这轮
**没有**测到的东西（interrupt、steer、网关、审批活链路），以免 ADR 超额声称。

## D-038

**2026-09-19 · 会话列表行 = 状态点 + 标题 + 一句下一步；三维 wire 与 `ins_` 退到展开/tooltip；行内遥控收进溢出菜单**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | [ui-spec.md §2.1](./ui-spec.md)（会话列表）、[workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.3 / §11.7、D-024 addendum |

**背景**：`web/src/features/session/SessionList.tsx` 把 `lifecycle · activity · connectivity | host / worktree · driver` 渲染成行内 `.meta`，并追加 `shortId(instance.id, 8)`，行内还有 `send…` 与 enter/esc/ctrl+c。而 `ui-spec.md` §2.1 的线框本来就只有「点 + 标题 + 一句」——**当前代码才是偏离规格的那一方**，不是规格要为新建议让路。

**决策**：列表行默认视口只渲染状态点（§2.1 三维投影）、标题、kind 芯片、徽标与一句「下一步」；`lifecycle`/`activity`/`connectivity` 原文三元组、`ins_` 短码、`driver`、`model` 退到每行的 `<details data-testid="session-wire">` 或 `title`。那句「下一步」只**投影**已有字段（`projectStatus` / `Interaction.request.kind|description` / `exitLabel` / screen 终态），不新造状态机、不猜成功；`connectivity ≠ connected` 或 `lifecycle ∈ {unknown,reconciling}` 一律产出「状态待确认」，绝不回落成正向文案。行内遥控收进长按/溢出 `Sheet`（保留既有 testid 与命令路径），「待处理」组在行上保留一个主操作。

**由谁**：coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, decision 5「收进 Sheet，不删能力」）。该计划是派工单，未入库；理由与默认值见报告 §11.3 与 §11.7 的 P0-6 行。

**依据**：报告 §11.7 的 P0-6 行结论为 **READY（规格反而要求）**——`ui-spec.md` 线框无 wire 串、`:164` 明写状态点是三维投影而非单独 wire 枚举，缺的只是 `nextStep()` 派生（全库不存在）；§11.3 记录 Moshi 在同一位置放的是事件原文与错误文案（`API Error: Request rejected (429)`），支持「一句下一步」而非诊断串。

## D-039

**2026-09-19 · 触控命中只靠热区；`.meta` 统一 `var(--text-aux)`，桌面手机同值；禁止在 `session.module.css` 新增裸像素命中尺寸**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | [ui-spec.md §3.4](./ui-spec.md)、§2.2（Effort）、§1.3、报告 §11.6 / §11.7、D-024 |

**背景**：两条独立的偏差。**(a) 字号**：`.meta` 基础值 11px（`session.module.css:153-159`）、手机媒体查询再降到 10.5px —— 两个值都低于 `tokens.css` type scale 块头注释的 12px 下限，而且**手机字号小于桌面**同一元素，方向是反的。**(b) 命中**：`.back` `width: 20px`（`:25-31`）、`.viewSeg` 25px／手机 30px（`:96-98` / `:2450-2453`）、手机 `.stopBtn` 32×32（`:2398-2403`）都低于 44px。报告 §5-P0-2 的字面写法（「`.back` 20px → `--touch`」）**违反** `ui-spec.md` 既有的 §2.2 条款「手机上触控 ≥ 44px 只靠**热区**，不靠视觉尺寸」——报告自己的正文也写了「可视可以略小，hit slop 必须够」，是标题与正文不一致。

**决策**：命中尺寸一律走**热区**（`::after` / padding），视觉字形尺寸**不变**；新增命中尺寸一律 `var(--touch)`，`session.module.css` 里不得再新增裸像素命中值（该文件全文对 `var(--touch)` 的引用数为 **0**，20/25/27/30/32 全是硬编码，这正是跑偏的原因）。可点区域不得互相重叠。`.meta` 一类辅助/诊断文本**桌面与手机统一** `var(--text-aux)`（12px），用 token 而非字面量；本条只管辅助文本，正文 14px 与输入/强调 16px 不变。compact（布局）判定与 `coarsePointer`（触屏）判定不得混用。

**由谁**：coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, decision 2；规格口径，实现由 c-touchhit 落地）。该计划是派工单，未入库；理由与默认值见报告 §11.6 与 §11.7 的 P0-2 / P1-8 行。

**依据**：报告 §11.7 的 P0-2 行判 **NEEDS_DESIGN**，要求按规格改写成热区方案并取正文那句；§11.6 两处措辞修正（桌面 11px / 手机 10.5px；12px 下限的出处是 type scale 块头注释，不是 `--text-label` 的注释）。热区先例在同文件 `.effortIconBtn::after`（44×44，`:2434-2443`）。`var(--touch)` 全库唯一定义在 `styles/tokens.css`。

## D-040

**2026-09-19 · `/s/:id` compact 铬预算：space chips 折成单芯片入顶栏；诊断 meta 进「运行详情」disclosure；分段与 Stop 不得进 ⋯**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | [ui-spec.md §1.3 / §1.4 / §2.2](./ui-spec.md)、报告 §11.7、D-024、D-028 |

**背景**：`ui-spec.md` §1.4 要求「约 400px 手机上显示可横向滚动的 space chips 与 tabs」，且该节自称「只作用于会话工作台」——`/s/:id` 正在其内，所以「收起 chips」原本与规格直接冲突；`ui-spec.md` §1.3 又把 `terminal|structured` 切换列为顶栏必有项，挪进 ⋯ 即违规。另一半：§2.2 把两行 header 画成**规范 wireframe**（第二行就是 `seq 184 · connectivity=connected · $0.12`），而 `:113` 要求主机芯片在顶栏、`:331` 要求 cost 在顶栏——桌面 meta 折叠若不改这张图，代码与规格立刻自相矛盾。

**决策**：**(1)** §1.4 的 400px chips 要求**限定到列表路由**（`/sessions`、`/hosts`、`/projects`、`/approvals` 等）；`/s/:instanceId` 及其子视图在 compact 下允许折成**单枚当前 space 芯片**并入会话顶栏，点击打开同一个抽屉（`spaces-drawer-open` 行为与 testid 不变），切空间能力不降级。**(2)** `terminal|structured` 分段与 Stop **始终留在顶栏，永不进 ⋯ 溢出菜单**。**(3)** §2.2 header 从「规范两行诊断」改为「主行 + 可折叠**运行详情**」，ASCII 图同步重画；主行保留 **host 芯片 + cost + 状态点 + 分段 + Stop**；`driver / delegation / provider / providerSourceHint / lifecycle / seq / connectivity / native / promoted 绑定` 进 disclosure，默认收起、**展开态按设备持久化**（本设备 `localStorage`，不跨设备）；`session-meta` testid 保留在展开内容上以便老断言迁移。

**由谁**：coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, decisions 1 与 2）。该计划是派工单，未入库；理由与默认值见报告 §11.7 的 P0-1 / P0-5 两行。

**依据**：报告 §11.7 的 P0-1 与 P0-5 两行均判 **CONFLICTS**，明确「要实施必须先改 `ui-spec.md`」；P0-1 行还指出该建议**与自己矛盾**（P1-8 与 §10-19 要求该开关**更**显眼）。

## D-041

**2026-09-19 · 工具卡默认折叠的豁免集合与折行最小信息量**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | [ui-spec.md §2.2](./ui-spec.md)、报告 §11.7、`web/src/features/session/ToolCard.tsx` |

**背景**：`ui-spec.md` 要求 `workflow.run` **运行中与结束后都展开，只有读者 dismiss 才折叠**，`error` 卡**必须展开**；而 `ToolCard.tsx` 的 `if (folded) return …` 分支在 `family === "Workflow"` 分支**之前** return（约 `:260` vs `:281-292`）。一律默认折叠会让 `WorkflowTimelineCard` 永不挂载、1Hz 走针不启动——是**静默杀死功能**，不是视觉问题。另 `ui-spec.md` 要求 Bash「命令一行」，折成裸「Bash」违规。

**决策**：默认折叠**只对 compact 且已 settled、且 family ∉ {Workflow, error}** 的卡生效；`interaction.*` 同样豁免；running / 未 settled 的卡**永不自动折叠**。豁免只约束自动 compact 默认折叠；读者显式「全部折叠」保持既有桌面行为——一切非 failed 卡（含 Workflow 与 running）都折。折行内容 = **family + 关键参数**（Bash = 命令首行，Edit/Write/Read = 路径），按宽度截断且 `title` 给全文，裸 `Bash` 不合规。**折叠分支必须在 family 判定之后**才可提前 return。桌面默认态不变；展开后的卡与桌面完全一致（同一组件、同一 testid）。

**由谁**：coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, decision 3）。该计划是派工单，未入库；理由与默认值见报告 §11.7 的 P1-10 / P1-20 行。

**依据**：报告 §11.7 的 P1-10 / P1-20 行判 **CONFLICTS**，并点名 `ToolCard.tsx:260` 早于 `:281-292` 的 return 顺序；折行关键参数可由 `toolPresenters.ts:191-203` 的 Bash 分支直接给出。验收硬指标是「卡头 elapsed 在运行中递增」，不是「卡可见」。

## D-042

**2026-09-19 · 手机 composer 单行 + 选项 sheet 的边界：触发器带 mode 词与档名，三态与诚实标注留在外面**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | [ui-spec.md §2.2](./ui-spec.md)、报告 §11.7、D-028a、`web/src/features/session/Composer.tsx` |

**背景**：`ui-spec.md` 要求权限芯片**显示**当前 `permissionMode`、effort 触发器**显示档名**；`permissions.ts` 把 bypass / dontAsk 一类标为 `danger`——把 bypass 态藏进 sheet 是这一片里最响的冲突。D-028a 另要求 composer 呈现发送/排队/打断三态 + 队列 chip + 未验证能力标「尚未验证」。同时 `Composer.tsx` 的 placeholder 写满桌面快捷键，两处 `window.confirm`（插队 / Esc 打断）在 Safari 上会盖住键盘。

**决策**：compact 下 control bar 允许收成**一个选项触发器**，但触发器**必须**同时显示当前 `permissionMode` 的**词**与 effort 的**档名**（形如 `manual · high`），`danger` 模式必须在触发器上就以 danger 样式可见。**可进 sheet**：附件、harness 只读芯片、context 用量、权限选择器、effort 滑杆。**不得进 sheet**：三态按钮、队列 chip、「尚未验证」标注（它们不是「选项」）。placeholder 分平台（手机不写桌面快捷键）。插队与 Esc 打断的 `window.confirm` 换成 `Sheet`（桌面 `popover`、手机 `sheet`，焦点圈定、Esc = 取消、返回焦点到触发器），**确认后的命令语义与 `commandId` 路径完全不变**；桌面布局与 testid 零变化。

**由谁**：coordinator（coordinator dispatch plan workbench-ux, 2026-09-19, decision 4）。该计划是派工单，未入库；理由与默认值见报告 §11.7 的 P0-3 行。

**依据**：报告 §11.7 的 P0-3 行判 **CONFLICTS**（权限与 effort 的可见性、D-028a 三态与诚实标注）；`decisions.md` D-028a 的三态/队列 chip/「尚未验证」要求是本条的边界来源。

## D-043

**2026-09-19 · Grok 结构转译契约：文件层 ACP 帧按协议 §5.7 翻译，不新增 driver、不改 wire schema**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | D-028、[grok-structural-translation.md](./grok-structural-translation.md)、[protocol.md](./protocol.md) §5.7 grok-acp |

**背景**：P6 已把 grok TUI 落盘的 `updates.jsonl` / `events.jsonl` tail 进 journal，
但 `GrokAdapter` 的工具转译与协议 §5.7 相反：稳定工具名取的是会变化的人类
`title`（协议要求 `_meta["x.ai/tool"].name`，title 只作展示）；`category` 恒为
`Shell`；无 `status` 的 `tool_call_update`（夹具实帧：先 Pending、再无 status
进度帧、最后 completed）被直接丢弃，导致 `ToolCallState::Running` 与
`ResultStage::Partial` 在 grok 通道零产出；`content[]` 只取第一段文本，`diff` /
`terminal` / 非 text 块全部消失；thought 收尾用 `Open` 重发全文而不是 `Close`。

**决策**（仅翻译层，零协议增量；不叠加第二条 grok-acp 进程，延续 D-028 一终端一会话）：

1. **身份**：`tool_name` 只取 `_meta["x.ai/tool"].name`；缺失即
   `Knowledge::Unknown{reason:"not-emitted"}`，绝不用 `title` 冒充稳定名。
   `display_title` 取帧上 `title`，缺失时回落工具名。
2. **类别**：先查名字表（设计文档 §3.1：`run_terminal_command→Shell`、
   `read_file/list_dir→FileRead`、`write/search_replace→FileWrite`、
   `grep/web_search/web_fetch/open_page/open_page_with_find/x_*→Search`、
   `spawn_subagent→Agent`、`workflow→Workflow`、`search_tool/use_tool→Mcp`），
   名字未知再按帧 `kind`（`execute→Shell`、`write/edit→FileWrite`、
   `ask_user/other→Other`），最后才 `Other`。Rust 表与 web registry 表同引 §3.1。
3. **Running**：无 `status` 的 `tool_call_update` 以及 `status=="in_progress"`
   都是同一 node 的 Running `ToolCall`：`state=Running`、revision 2、`Replace`；
   只对已见过 Pending 的 `toolCallId` 生效（中途加入的 tail 看到陌生进度帧仍忽略，
   但不影响后续终态结果）。帧上缺的字段保持旧值（`display_title` / `rawInput` /
   name / kind）。
4. **终态判定按协议 §5.7，不把所有带 `status` 的帧都当终态**：只有
   `completed` / `failed` / `denied` / `cancelled` 发 `ToolResult` `Final`，
   revision 严格大于该 node 最后一次 `ToolCall` revision（web `newerMutation`
   要求单调，否则卡片冻结在 proposed）；`pending` 与未知 status 不是终态，
   保持 opaque、不关节点（等后续帧），不伪造结果；`outcome` / `exit_code` /
   `structured_result` 行为不变。
5. **content[]**：`{type:"content"}` 内层 text → 文本块；`{type:"diff"}` →
   `FileChange{path（diff.path 或 locations[0].path）, diff, application}`，
   `Applied` 仅当 `status==completed` 且无 error，否则 `Unknown`；
   `{type:"terminal"}` → 只命名 terminal id 的文本块，不是 tty-attach 承诺；
   其余非 text 内容以带原生类型的文本标记出现，不再静默丢弃。
   `rawOutput.output_for_prompt` 仅在本帧没有产出任何 **content 型文本块**时兜底
   追加（terminal/image/未知类型标记不抑制该兜底，避免真实命令输出被标记挤掉）。
6. **thought 收尾**：`turn_completed` 用 `Close` + 累积全文（与 message 的
   open/append/close 一致），取消回合带 `Interrupted` 状态。
7. 共享的 `adapters/mod.rs` 负载构造函数签名不动（codex 字节级不变）；grok 专用
   构造函数全部住在 `grok_adapter.rs`。不新增 `ObservationPayload` 变体、不改
   `protocol.md`、不重生成 schema。

**范围边界**：本决策只覆盖 PR1/PR2 的帧→负载翻译。`turn.live` file-tier 相位、
`ask_user_question` 升格 interaction、`terminal/<id>.log` 增量 stdout、fake-harness
保真度与 web presenter 分别由后续任务落地；grok workflow engine 与子代理下钻
（PR8）依赖 1.0.34 实帧重采（PR7），不在本决策内，且任何测试不得据 1.0.30
`--no-subagents --no-plan` 夹具断言「grok 不支持 X」。夹具中未捕获的 diff /
terminal 内容帧形状以 [U] 标注为合成帧，1.0.34 重采时校正。

**影响**：`crates/remuda-driver/src/adapters/grok_adapter.rs` 及单测、
`crates/remuda/tests/p6_adapter_parity.rs`；夹具三帧序列现在翻译为
Proposed(rev1 Open) → Running(rev2 Replace) → Final(rev3 Close)，
`remuda journal diff --no-whitelist` 自比对仍相等，codex 与 fake-harness 事实集
不变。

## D-045

**2026-09-19 · computer-use 能力按次授权与投递（capability grant and delivery）**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | D-011、D-017、D-025、D-028 §4.2、D-035、D-046、[codex-cua.md](./codex-cua.md)、[native-pty-first.md](./native-pty-first.md) §5、[ui-spec.md](./ui-spec.md) §2.2/§2.6 |

**背景**：`skills/codex-computer-use/` 让一个编码 agent 通过本机已安装的 Codex
Computer Use 读取或操作 macOS 应用界面：或走宿主注入的原生 MCP 工具，或在宿主
未认证时拉起一个 stdio `cua-repl`，后者**求值任意 JS** 并对本机每个应用持有
`click` / `typeText` / `pressKey`。skill 自己的验证记录（2026-09-17 / 09-19）
写明：非 Codex 宿主走原生 MCP 会拿到 `Sender process is not authenticated`，
直连 `computeruse.sock` 会被服务端断开，而「Claude Code 实际消费 MCP 截图及操作
UI」一栏的结论是**未验证**——**没有任何一条路径被证明对 Remuda 启动的 agent 跑通过**。

把它接进产品前先要回答三件事：谁有权要它、它怎么到达 agent、它做了什么之后能看见。
今天的现状是：Remuda **不给任何 agent 投递 skill**（`skills/remuda` 只由
`scripts/gen-skill.sh` 组装给人类协调员）；一个被启动的会话能看见什么，完全取决于
native home 是不是继承来的（[codex-cua.md](./codex-cua.md) §3.1）；`--mcp-config`
早在四族 argv 白名单里，但**从任何产品面上都够不到**。

**决策**：

1. **能力名与门**：能力名 `computer-use`，**按会话按次显式申请**，只有
   `remuda instance create` / `remuda dispatch` 上的显式 `--capability computer-use`
   能授予。它**不是** host 属性、不是 driver 属性、不是全局开关，不写进任何默认、
   preset 或 workspace，不从父 instance 继承。
2. **两道门，缺一不可**：(a) 来源必须是 `LaunchOrigin::Human` 或 `Bot`，
   `LaunchOrigin::Agent` **一律拒绝**（门形照抄 `presets::merge_yolo_argv`，
   `crates/remuda-driver/src/presets.rs:151-166`）；(b) 目标主机心跳的 `cli[]`
   必须回报 `kind: "computer-use"` 且 `installed: true`。
3. **投递两条腿**：(a) skill 字节从 `remuda` 二进制内嵌物化到**受管的 claude
   native home**（`<native_home>/skills/codex-computer-use/**`，目录 0700、文件
   0600）——**门是「本次 launch 的 home 是不是 Remuda 管的」，逐 kind 定义**
   （claude：作用域 config dir；codex：`launch_dir/codex-home`；grok：
   `launch_dir/grok-home`），**不是** `inherit_default_config`（那是 claude 专属
   标志，对 codex launch 也为真）；(b) 一律写 per-instance
   `<launch_dir>/mcp-cua.json`（0600）。**codex / grok 本批只有腿 (b)**：仓库与
   skill 都没有证据表明它们读任何 skills 目录，往影子 home 写 skill 树没有读者，
   投递就只是那份 MCP config。
4. **按 `AgentKind` 挂载，不按 driver**：claude / grok / agy 走 argv
   `--mcp-config <path>`；codex 走 shadow home 的
   `[mcp_servers.codex-computer-use]`。`shell-pty` / `generic-pty` 是 kind 多态的，
   **同一 driver 承载 claude 与 codex**，所以规则按 kind 说：`shell-pty` 跑 claude
   （D-036 自托管那一轮）走 argv，`shell-pty` 跑 codex 走 shadow home。
   **绝不发 `--strict-mcp-config`**（该 flag 永久禁用，`flags.rs:15`）：能力只
   **增加** agent 自己的 MCP server，绝不替换。
5. **环境变量握手**：`c-cua-launch` **只在能力被授予时**向子进程注入
   `REMUDA_CAPABILITY_COMPUTER_USE=1`；硬化后的 `launch-cua-repl.sh` 未见到它就
   拒绝启动。**该变量是信号不是边界**——任何有 shell 的东西都能自己导出它；**真
   边界是「没有授予就绝不物料化」**：未授予时 `mcp-cua.json` 根本不存在，agent
   没有可指的 launcher 路径。
6. **拒绝各有独立消息**，且都发生在 create/dispatch 被持久接受**之前**：Agent
   来源、主机未回报、主机回报 `installed: false`、非 macOS、launcher 缺失，以及
   **`bypassPermissions` 与 `computer-use` 同一次 launch**——无人值守的桌面控制
   叠加跳过的工具审批是唯一没有回收路径的组合，两者同时出现即拒绝（不是「忽略
   其一」，也不是需要另一个隐含 flag）。
7. **截图两条**（原计划曾分别为 D-038；因该 id 已被占用，并入本行并加此标记）：
   (a) **截图进 journal 与 web** —— tool result 合法携带 `image` content block，
   字节只存对象库（Node 用宿主 token 走 `POST /v1/hosts/{id}/files/objects`），
   `ContentBlock::Image` + `MediaBlock` 协议早已合法；**只发文本的 producer 是
   bug，不是策略**；**不新增 `ObservationKind`**；截图**不是 artifact**，只有
   agent 显式存盘才发 `artifact`；渲染规则见 ui-spec §2.2。(b) **保留期** ——
   沿用既有附件对象的生命周期与过期，**永不内联 journal、永不写日志**，证据文档
   必须用脱敏或合成屏；更短的 CUA 专属 TTL 是 Hub 旋钮与第七个任务。
8. **host fact 只读**：`computer-use` 是一行普通 `cli[]` 行（`installed` / `path` /
   `version` 读 `Info.plist` / `auth: unknown`），非 macOS 也回报该行
   （`installed: false`）。`version` 是**文件读取，绝不 exec**——跑 vendor 二进制
   问版本会启动一个 Mach service。Hub 侧零改动。

**后果**：

- Remuda 第一次向被启动的 agent **投递文件**（内嵌字节 → 受管 home），因此新增一条
  「内嵌副本与 `skills/` 源目录 digest 相等」的测试要求，防两者漂移。
- `~/.claude`、`~/.codex`、`~/.grok` 在**任何**分支上都不会被打开写——这是
  `overlay.rs:25-28` 既有规则的延伸，需要有测试钉住，不能只靠代码审查。
- 能力请求一旦被拒绝就**不能静默降级或静默丢弃**，否则操作员会以为授予成功。
- `computer-use` 是 Remuda 授予过的**最大爆炸半径**：一个握有全机 `click` /
  `typeText` 的 worker 能发消息、点对话框、花钱，而今天的 journal 什么都看不见。
  缓解是 D-045 的两道门、Q4 的 bypass 拒绝、以及让每个动作与截图在能力被真用之前
  就先在会话里可见。
- 本批**不**为 CUA 加 Hub placement 谓词：拒绝发生在 launch 而不是 placement，
  由操作员的 `remuda dispatch --host` 负责选对机器。
- **合并闸门不是绿灯单测**：`c-cua-launch` 的合并前提是一条真实探针（至少一个
  Remuda 启动的 agent 端到端驱动 `cua-repl`），记录进
  `docs/design/evidence/codex-cua-1.md`。探针失败则只交拒绝路径与物料化，能力
  **默认关闭**。

**附记（2026-09-22，c-nodecosign；追加说明，不改动上方既有条目）**：
Node 侧 device passkey 联署规格（[passkey-login.md](./passkey-login.md)
§6）在 D-045 的两道门（来源 Human/Bot、主机心跳回报 installed）之外规划
第三道门——非空 `capabilities` 的 create 帧在 Node carrier 边界必须带一
个已配对 human 设备对该帧规范字节的 WebAuthn 联署，且 `computer-use`
不接受预先签名的无人值守联署券（coupon），设备必须在 create 当时在场。
这不改变 D-045 的任何现有门与拒绝组合（含 bypassPermissions 与
computer-use 同帧即拒），只是给「来源声称是 Human/Bot」补上设备级证据；
该规格本批为 docs-only，wire 与实现均未改。

## D-046

**2026-09-19 · CUA 交互路由：`elicitation/create` 由 worker 自己答，回答权被 launch 请求约束**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted |
| 相关 | D-005、D-017、D-045、[codex-cua.md](./codex-cua.md) §5、[native-pty-first.md](./native-pty-first.md) §5 P5、[ui-spec.md](./ui-spec.md) §2.5 |

**背景**：`cua-repl` 会发 `elicitation/create` 做**按应用审批**——「允许这个 agent
操作这个应用吗」。agent 侧今天**只有一条** elicitation 桥，且只搭在 Claude 的 hook
事件上：`Elicitation` 是注册事件、是阻塞事件、可回 `action`
（`crates/remuda-signal/src/event.rs:114-135`），已能生成
`Interaction{kind: Elicitation}` 卡（`crates/remuda-signal/src/approval.rs:153-208`），
也能从 `InteractionAnswer::Elicitation` 回一个动作
（`crates/remuda-signal/src/bus.rs:1017-1030`）。**但这条链路够不到 MCP 的
`elicitation/create`**：cua-repl 是 stdio MCP server，它的 elicitation 走 MCP 协议
本身；codex shadow 的 `hooks.json` 只注册 `SessionStart` 与 `PermissionRequest`
（`shadow.rs:40-45`），grok 的 `GROK_EVENTS` 同样不含 elicitation 类事件
（`shadow.rs:47-55`）。所以**今天没有任何路径**能把 MCP server 的
`elicitation/create` 变成 `/approvals` 里的一张卡。

**决策**：**worker 自己答，回答权被本次 launch 的请求约束。**

- worker 在 cua-repl 的 `initialize` 里声明 `capabilities.elicitation`；
- 对 `elicitation/create`：**仅当目标应用是本次请求点名的应用**时 `accept` 且
  `persist: session`；未点名的应用**一律不批**；
- 「点名的应用」= 派发 brief 里写明的 bundle id；`c-cua-coord` 要求 brief 写出确切
  bundle id，并禁止为未点名的应用批准 elicitation。
- **后续（不在本批）**：把回答权从 worker 交回人，`/approvals` 成为 CUA 审批的
  唯一出口。前置是**每个 harness 各造一个 producer**，把 MCP `elicitation/create`
  抬进 interaction bus——**不需要新的 `InteractionCarrier` 取值**：该枚举已有七个
  （`ClaudeControl` / `ClaudeHook` / `HarnessHook` / `CodexRpc` / `AcpRpc` /
  `NativeTty` / `Unsupported`，`crates/remuda-protocol/src/enums.rs:242-249`），
  按 harness 选即可（claude 已经就是 `HarnessHook`，`approval.rs:191`；codex → `CodexRpc`；grok → `AcpRpc`）。
  缺的是 **producer 位置**，不是枚举值。

**后果**：

- **本批的桌面审批没有 journal 行、没有 `/approvals` 卡、没有第二人复核**，只有
  worker 的一面之词。这是明确接受的代价，写进文档而不是假装它已解决；UI 与文档
  都**不能**画出一张并不存在的审批卡。
- claude 侧 `Elicitation` 的实机 payload **至今未取得**
  （[native-pty-first.md](./native-pty-first.md) §5 P5 残留：仅按二进制读取端形状
  实现），所以连「Claude hook 那条桥能不能真的覆盖 CUA」也是 UNVERIFIED。
- 在 MCP 级桥存在之前，D-045 的 bypass 拒绝 +「只批准点名应用」是唯一的边界，
  且**已知不足**：它约束的是 worker 的自律，不是一台会拒绝的机器。
- 本批不为 grok 建能力目标：§5.3 的路由对 grok 无落点，且没有证据表明 grok 能
  消费该 MCP server。

## D-047

**2026-09-19 · 模型 API 交付方式：可选 `via:<hostId>` 代理、路由子模式、拒绝而非改道（D-031 例外条款）**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted（类型与线格式已落地；Hub/Node/CLI/web 行为见 api-routing 计划 tasks 2–6） |
| 相关 | D-021（secret 只留一台机器）、D-031（禁隧道）、D-035（禁止静默替换，记录实跑值）、D-048（`api.*` 流类）、[api-routing.md](./api-routing.md) §2/§3/§4/§5 + §8、[protocol.md](./protocol.md) §4.4/§7.6 |

**背景**：模型网关凭据是**主机绑定**的。操作员的 Mac 上跑着 claude-relay
（`ANTHROPIC_BASE_URL` 指向内网网关 origin + 一个 token），而远端 devbox 上的
worker 今天完全拿不到那条路径：凭据不能离开 Mac（D-021），即便把凭据发过去，
那个 origin 从 devbox 也未必可路由。于是「只在 Mac relay 上可达的模型」**无法**
交给远端 worker——要么放弃，要么把凭据发到一台可能到不了网关的机器上。
owner 2026-09-19 指示：加一个**可选**参数，让一次派发的模型 API 请求都从指定
机器出去，这台机器可以是本机（跑 Hub 的那台）也可以是远程机器。

**决策**：

1. **两种交付方式，一个可选参数。** `delivery = direct` 是今天的行为（baseUrl
   与凭据一起送到 worker 主机 W）；`delivery = via:<hostId H>` 让该会话的所有
   模型 API 请求从 H 出去。profile 上是嵌套对象
   `ProviderProfile.delivery = {mode: direct|via, viaHostId?, route}`，缺省
   `{mode: direct, route: auto}`——**线格式是嵌套的，CLI 的人话拼写
   `--delivery direct|via:<host>` 由 CLI 解析**，两者不混为一谈。逐次派发是
   `apiVia`（字符串 `<hostId>` \| `self` \| `none`）加可选 `apiRoute`。
2. **瀑布**：请求 `apiVia` > 项目 `provider.apiVia` > profile `delivery` >
   `direct`，与既有 provider 瀑布同构，并且在**主机放置之后**解析
   （`providers.rs::resolve_and_attach_with_project`）——因为 `via:<H>` 在
   `H == W` 时收敛为 `direct`：worker 主机自己就是出口主机，没有东西可代理。
   收敛只发生在决议时，**不改写线上的值**。
3. **路由子模式（Amendment A1）。** H 与 W 之间可能没有网络路径（owner 的实际
   拓扑正是如此：Mac 无公网入口，D-031 禁止造一个）。因此 `via` 有一个路由
   子模式：
   - `auto`（默认）——H 配了非 loopback 的 `relayBind` 时，W 先探直连路径
     （带实例 bearer 的 HEAD/OPTIONS，3 s）；没有配置或探测失败则回落
     `hub-relay`。
   - `hub-relay`——总是走既有 Hub↔Node 链路（W Node → Hub → H Node，或 H 就是
     Hub 主机时由 Hub 进程自己出去）。这是**永远可行**的路径，也是 W 到不了 H
     时唯一可行的路径。operator 的 Mac + SG devbox 就是这种情形。
   - `direct-net`——要求直连；探测失败就在启动时以 `api-via-unreachable` 拒绝。
   路由**只在启动时决议一次**，并由 Node 作为 `apiRoute: direct-net | hub-relay`
   回显。会话中途直连失败**不静默切换**到 hub-relay：请求失败，实例报
   `blocked{api-route-down}`，操作员重派（或后续任务加显式 re-route 命令）。
4. **H 的 relay 端点**默认只绑 loopback；只有操作员在 host 上显式设置
   `relayBind`（一个明确地址，默认绝不是 `0.0.0.0`，从不自动发现）才绑定
   非 loopback。两条路径都要求每实例 bearer。
5. **拒绝，绝不改道。** 所有失败都是**拒绝**，且没有一条路径会回落到
   `direct`：H 未知/未注册 → 400 `api-via-unknown-host`；H 已注册但离线 →
   409 `api-via-host-offline`（在任何名字/端口/worktree 分配之前，与
   `workers.rs` 的供给拒绝同序）；H 的 Node 太旧不会说 `api.*` → 409
   `api-via-unsupported`；`direct-net` 探测失败 → 409 `api-via-unreachable`。
   代号与 HTTP 状态都由协议层 `ApiViaRefusal` 固定。落到 `direct` 会把请求
   **连同凭据**推到操作员明确排除的机器上，并让 UI 说谎——这是最糟的一类 bug，
   所以测试断言启动**失败**而不是改道。
6. **记录实跑值（D-035 规则 4）。** 实例上存 `ApiRoute {mode, route,
   viaHostId, viaHostLabel}`，与既有 `providerSource`/`providerSourceHint`
   并列，**取 Node 的 create 回执**而非请求。`ApiRouteKind` 只有两个已决议的
   值——`auto` 是请求、不是观测，记录里出现 `auto` 就等于 Hub 在报告「我要了
   什么」而不是「实际跑了什么」。
7. **secret**：网关凭据**只在 H**（或 H 就是 Hub 主机时的 Hub 进程里）加载，
   从不落到 H 的磁盘上；W 拿到的是一枚每实例 relay bearer，在别处一文不值。
   这**加强**了 D-021：`host:<hostId>` 作用域的 profile 现在能服务任意 worker，
   而不必把凭据放出那台主机——`secret_release_allowed(profile, H)` 检查的是
   **H**，不是 W。
8. **usage** 仍记在 worker 实例上（`usage_events` 用真实网关 `profile_id`），
   供给核算、429 park、预算区间零迁移；H 只记字节/流计数器，并把观测到的
   真实 `429`/`529` 投影进既有供给证据路径（`remuda profile event
   --http-status`），把限流检测从「读屏」升级成「真状态码」。

**D-031 例外条款（本决策的边界，写下来以免日后重新争论）**：这是**不是**隧道。
不安装、不探测任何隧道二进制；不用 `ssh -L/-R/-D`；默认配置下不在任何非
loopback 接口上开端口（只有操作员显式设置 `relayBind` 才会）；不能到达任意
主机。字节走的是**已经授权、已经审计**的 Hub↔Node 链路，和今天的
`object.pull` 附件字节完全一样（[protocol.md](./protocol.md) §7.4 记录了为什么
用 JSON 而非二进制 channel 2）。被转发的是一个**单一 origin、白名单、
实例作用域**的应用请求——与 `host.files.read`、`tty.write` 同类，不是内网穿透。
目标 origin 固定为 `profile.baseUrl` 的 origin，路径必须是 base path 下的后缀，
请求头与响应头都有白名单（`set-cookie` 丢弃）。一个 relay 流只能到达恰好一个
origin、为恰好一个活实例、只在该实例存活期间。

**影响**：新增协议类型 `ProviderDelivery`/`ProviderDeliveryMode`/`ApiRoute`/
`ApiRouteKind`/`ApiRouteMode`/`RequestedApiRoute`/`ApiViaOverride`/
`ApiViaRefusal`/`HostRelayBind`；`ProviderProfile.delivery`、`InstanceSpec.apiRoute`、
`Host.relayBind`、实例投影 `apiRoute`、`TransportLimits` 两个新字段；生成物
schema/OpenAPI/web 客户端同步。全部**加性**：缺 `delivery` 反序列化为
`direct`、缺 `apiRoute` 为 `None`、缺两个新 limit 取默认值，因此旧 Hub 与旧
Node 仍能解析（测试 `profile_without_delivery_parses_as_direct_auto`、
`instance_spec_without_api_route_parses_and_stays_absent`、
`transport_limits_written_before_d048_still_parse`、`via_without_a_host_is_a_parse_error`）。
Node 在 `InstanceCreateResult.apiRoute` 上回显**实跑**的路由——这是 Hub 唯一允许
取观测值的地方，spec 上的是**请求**值（其 `route` 可能是 `auto`）。行为与 CLI/web
面是 tasks 2–6。

**与计划的一处用词差异**：`ProviderOverlaySpec` 带的是 `delivery` 而不是计划
§B.8 写的 `route`。理由是同一份 profile 快照要能自描述完整的交付方式——只带一个
`route` 的 overlay 描述不出 `mode` 与 `viaHostId`，而 Node 需要读 `delivery.mode`
才决定要不要起 relay 监听器。真正的逐实例路由走 `InstanceSpec.apiRoute`，overlay
只带 profile 级意图，两者职责不重叠。

## D-048

**2026-09-19 · `api.*` 带内流类：代理请求在既有 Hub↔Node 链路上的分帧、信用与分块**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted（类型与线格式已落地；Hub/Node 行为见 api-routing 计划 tasks 2–3） |
| 相关 | D-047、D-027a（`object.pull` 先例）、[protocol.md](./protocol.md) §7.4/§7.6、[api-routing.md](./api-routing.md) §7 |

**背景**：`via` 交付（D-047）需要把一个 HTTP 请求/响应从 W 的 Node 搬到 H、
再搬回来，而 W 到 H 之间唯一保证可达的通路就是既有的 Hub↔Node 链路。
`object.pull`/`object.chunk` 已经证明了这类带内搬运可行且安全
（[protocol.md](./protocol.md) §7.4：ssh-stdio 桥只逐帧转发 JSON 文本，所以
二进制通道会改变桥的全部安全边界）。

**决策**：新增七个 `api.*` 帧——`api.open`（W Node→Hub，或 Hub→H Node，
`{instanceId, streamId, method, path, query, headers[], bodyBase64?,
bodyChunked, deadlineMs}`）、`api.body`（请求体续帧）、`api.head`
（H→Hub→W，`{streamId, status, headers[]}`）、`api.chunk`（响应体分块）、
`api.end`（`{streamId, error?: {code, message}, bytesUp, bytesDown, ms}`）、
`api.cancel`（客户端断开/超时/实例退出/链路丢失）、`api.credit`
（消费者→生产者放行）。

1. **独立的 stream 表，不进 RPC pending map。** 七个帧**全部是
   notification**，各自维护每链路 stream 注册表，因此它们**永不消耗**那个
   被 `instance.create`/`tty.*` 依赖的 32 槽在途 RPC 上限
   （`transport.rs`）。`HubNodeMethod::is_api()` 就是这个分流的入口。
2. **默认限额。** `TransportLimits.maxApiStreams`（默认 8/链路、2/实例）与
   `apiChunkBytes`（默认 64 KiB 原始 ≈ 87 KiB base64，远低于 1 MiB 的
   `maxJsonFrameBytes`）。两个字段都在读侧有默认值，所以 D-048 之前的
   `hello.limits` 仍能解析。
3. **信用，因为出站队列是 32 且与 tty 共享。** 一个生产者每流最多 4 个未确认
   chunk，消费者边排空边发 `api.credit`；配合与 `object_pull` 同款的
   `yield_now()` 纪律，一条长 SSE 流无法饿死 tty 帧。
4. **SSE 必须合并。** 每个 token 一帧在 NDJSON/ssh-stdio 上是病态的；H 在
   **≥16 KiB 或 ≥50 ms 或流结束** 时合并（严格保序——body 是**不透明字节**，
   不按 SSE 解析）。
5. **超时阶梯**：连接 10 s、首字节 60 s、块间空闲 120 s（SSE keepalive）、
   硬上限 30 min。越界即向上游 `api.cancel`、向下游 `api.end{error}`，
   监听器以 `504` 和 Anthropic 形状的错误体回答，让 CLI 渲染出真正的 API 错误。
6. **审计只记计数器。** journal 从不带 body 或 header：启动时一条
   `apiRoute` 观测，每条流在 `api.end` 时记 `{streamId, status, bytesUp,
   bytesDown, ms, errorCode?}`。

**为什么是一个可复用的类**：D-048 与 D-047 分开记录，因为它们可以独立评审，
而且这个流类对**任何**未来的带内请求类型都可复用——有界队列上的多路复用、
信用式流控、不透明字节分块，都不是模型 API 专有的。

**影响**：`crates/remuda-protocol/src/hubnode.rs` 七个方法常量 +
`HubNodeMethod` 七个变体 + `is_api()` + 九个参数类型；`TransportLimits` 两个
字段。这些参数类型与其余 M1 Hub↔Node 操作帧同属一族，因此和
`object.pull`/`tty.*` 的参数一样不进 `protocol.md` §12 的生成目录，而是在
§7.6 以文档记录（carrier 按手写路径把它们路由进 stream 注册表）。

## D-049

**2026-09-19 · 手机优先 UI：受限 `/m` 路由树、会话本体不分叉、视口重定向层、`start_url: /`、badge/权限横幅与平台听写**

| 日期 | 2026-09-19 |
|---|---|
| 状态 | adopted（规格层；实施按 mobile-ui 计划任务 2–9 跟进） |
| 相关 | [ui-spec.md §1.1/§1.2/§1.3/§1.4/§4.5/§4.6/§4.7/§4.8](./ui-spec.md)、D-026、D-028a、D-031、D-038、D-039、D-040、D-041、D-042、[workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md)、[evidence/mobile-ui-1.md](./evidence/mobile-ui-1.md) |

**背景**：报告 §2.1 观察到手机会话页「垂直铬叠五层，正文只剩中间一小块」，而
手机需要的首页（§7.1 / §11.2 的分组跳转）与收件箱（§11.3 两档、错误当正文）
在信息架构上**不是桌面列表的密度变体**：桌面 `/sessions` 要筛选器/多选/广播/
行内遥控，手机要分组 + 一句下一步 + context 环 + 一键处理。mobile-ui 计划
§(B) 评估了三个选项：

1. **只做共享路由的 compact 分支（拒绝）**：把手机首页硬塞进
   `SessionList` 会同时压死桌面形态与在飞的列表重写，且既有 e2e 断言全部压在
   同一组件上——改动面比新建一个手机首页更大。
2. **同一 PWA 内的受限 `/m` 路由树（采纳）**：新增手机壳只拥有导航与首页级
   信息架构，共享 store / auth / API / SW，桌面路由零改动。
3. **独立客户端 / 第二个 Vite 入口（拒绝）**：报告 §5「明确不做」写明不新做
   原生 iOS/Android（§0.5 钉死单一 Vite 入口）；两份 auth / store / SW 会立刻
   分叉，违反报告 §9 的 SOTA 一句话——**结构视图是 live session 的投影，不是
   第二个 agent；终端始终可一键回去且不 fork。**

方案 2 的关键取舍是**只分一半**：首页与收件箱可以新做（它们在手机上本来就是
另一套 IA），会话屏**绝不**分叉——会话页承载 journal follow、turn 裁决、
composer 三态（D-028a）、terminal attach、内联审批与 resume（D-026），复制它
就是第二份 transcript，并会与 D-040/D-041/D-042 的在飞实施正面冲突。手机上的
会话改善全部落在共享 `/s/:id` 的 compact 形态上。另一笔记下的取舍：会话路由
compact 去掉 app 底栏后「回家」靠 44px 返回键与（M2 的）Jump To；若实测反感，
只回滚这一条、不回滚整棵 `/m` 树（计划风险 E9）。

**决策**：规格条文以 [ui-spec.md §4.7](./ui-spec.md)（路由树、重定向、可测量
铬预算、截断优先级）与 [§4.8](./ui-spec.md)（语音）为准，此处不重复条文，只
钉边界与编号归属：

1. **受限 `/m` 树**：只含 `/m`（home）、`/m/inbox`、Jump To sheet、phone
   底栏；会话本体与新建/登录/配对/设置保持共享路由。
2. **重定向层**：compact `/sessions`→`/m`、`/approvals?focus=`→
   `/m/inbox?focus=`（query 原样保留）；桌面 `/m*`→`/sessions`；
   **`/s/:id` 永不重定向**。
3. **铬预算**：一条顶栏 `--top-mobile` + 一条底栏 `--bar`；正文（composer
   收起、无软键盘）≥ 60% 视口；截断顺序 标题 → space 芯片 → 状态文字；分段与
   Stop 永不截断/进 ⋯（与 D-040 一致）。**compact-only 例外**：会话主行的
   host 芯片与 cost 折进「运行详情」（space 芯片已点名主机与项目），D-040 的
   **桌面**主行规则不变；状态点恒在主行。
4. **PWA**：`start_url` 从 `/sessions` 改为 `/`，一份 manifest / SW，落点由
   重定向层按视口判定。
5. **badge**：推送 payload 增加一个**可选**整数 `badge` 字段；无 Badging API
   什么都不做；不新增推送事件类型（详见 §4.5）。
6. **权限申请**：不在启动时弹，只在 `/m/inbox` 横幅与设置 → 通知两处由手势
   触发；iOS 未加主屏时文案为「先加到主屏幕」。
7. **语音**：平台键盘听写优先、先成文再发送；Web Speech API 仅增强且默认关；
   不做云转写；iOS Safari 无 `SpeechRecognition`（写进设置文案）；终端段无语音
   （详见 §4.8）。
8. **里程碑**：M1 = home、会话、收件箱、新建、登录、语音；键盘条、分组 Jump
   To、badge 与推送真机验证排 M2；git 面板五 tab 在 M2 之后。
9. **不抄清单**：报告 §9 的「不要抄」（顶栏/composer 盖正文、绿色品牌/Space
   Grotesk/FAB/地球图标）照行；DEV SERVERS / Kill 端口旁栏与隧道列表以 §11.5
   的「**整体不抄**」为准（§9 只说不要先做），D-031 的隧道禁令不变。

**由谁**：coordinator（mobile-ui 计划 §(B) 设计与 §(D) D1–D8 推荐默认值；
所有者未另行拍板即按默认执行；任务 c-mspec 落规格，零代码）。

**依据**：报告 §5（P0「能读、能批、能说」与「明确不做」）、§6（落地顺序）、
§7.1（home）、§9（SOTA 一句话、「不要抄」清单）、§10-19 / §10-23、§11.2
（Jump To 分组、只搜标题）、§11.3（Inbox 两档、错误当正文、权限横幅）、§11.4
（git 扫视串可学、硬裁 diff 与行内丢弃不抄）、§11.5（端口/Kill 旁栏与隧道整体
不抄）；与 D-038…D-042 的逐条无冲突对照见 [evidence/mobile-ui-1.md](./evidence/mobile-ui-1.md) §4。

## D-050

**2026-09-20 · Task 优先模型：任务聚合层、目录绑定 reuse|pool、worktree 池租借与看板只读投影**

| 日期 | 2026-09-20 |
|---|---|
| 状态 | adopted（规格层；实施按 task-model 计划任务 2–9 跟进，本决策先合） |
| 相关 | [task-model.md](./task-model.md)（机械权威：实体/RPC/路由/迁移）、[ui-spec.md §1.5/§2.9](./ui-spec.md)、[coordinator-hierarchy.md §2.6](./coordinator-hierarchy.md)、[files-view-contract.md](./files-view-contract.md)、D-002、D-017、D-024、D-031、D-033、D-035、D-047、D-049、security-review-2 M4、[evidence/task-model-1.md](./evidence/task-model-1.md) |

**背景**：Hub 的 Task 台账已完整存在（`crates/remuda-protocol/src/task.rs:194`；8 态 `:16-23`、`can_transition_to` `:52-64`、`is_terminal` `:67-69`；Hub 路由 `crates/remuda-hub/src/tasks.rs:185-199`），实例经 `instances.task_id`（`store.rs:4441`）归属 task，依赖只由 `landed_sha` 解锁（I1，`task.rs:49-51`、`unmet_deps` `:329-335`）。但没有任何 Web 面消费它（`web/src/pages/ProjectsPage.tsx:10` 仍是 Workspace 通讯录 stub）；目录侧只有「按名+分支幂等复用」的 `provision_record`（`crates/remuda-node/src/worktree.rs:60`，复用条件 `:81`，遇同名分支拒绝 `:101-105`），没有租借/池/引用计数；既有回收路径按 worker 名无条件 `git worktree remove --force`（`remove_record` `worktree.rs:153-181`，主路径 `worker.remove_worker` `worker.rs:217`/`:293`、`retire_worker` `workers.rs:1284`、`delete_instance` `http.rs:473`/`store.rs:2093`）。本决策把「每个 task 复用既有目录或由 app 管理新建 worktree」「任务作为看板单位」落成规格，使任务 2–9 的实现不再与 ui-spec / 既有 ADR 冲突。

**决策**：条文的机械细节（完整 schema、RPC/route 形状、池容量）以 [task-model.md](./task-model.md) 为准；此处钉边界与编号归属。

1. **Task 是 instance 之上的聚合层，不是第二套状态机。** 8 态机保持唯一权威，迁移只由 `can_transition_to` 判定；看板列、任务分组、人读 `SE-nn` key、共用计数都是只读派生，不可写回为状态。一个 task 聚合多个 session（一实例一终端不变，ui-spec §1.3），多 session 用 tab/卡片呈现，**不做并排多 pane 分屏**。
2. **目录绑定二选一 `workspaceBinding {mode:"reuse"|"pool", hostId, workspaceId, worktreeName?, …}`**，存进既有 `doc_json`（`tasks.rs:154`，serde default 零迁移）；首次绑定强制显式选择、按项目记忆。
   - **reuse**：绑注册根或既有 `remuda-wt` 兄弟目录，cwd 准入完全复用 `resolve_instance_cwd`（`worktree.rs:355-401`，受管根集合 `:382-393`）；不做任何 git 操作；多 task 同目录 = 同分支**顺序轮用**，attach 期间以 `holder_instance_id` 独占，第二个 task `refcount += 1` 但排队或 `blocked{reason:"dir-busy"}`；并发隔离是运行期 attach-lock，不是 gate 期纯函数的 `remuda own check`（`tasks.rs:641-679`）。**reuse lease 归还是对工作树的零操作**（无 clean/reset/remove），只改 lease 行。
   - **pool**：每 `(host, repo)` 固定容量（默认 4、可配）的 warm worktree 池，叠在 `provision_record` 之上（新模块 `worktree_pool.rs`）。slot 空闲停池 base 的 detached HEAD；lease 时原地切每 task 分支 `wt/<slot>/<task-slug>`（分支由 Node 从 taskId 确定性派生，仍满足 `validate_branch` `worker.rs:58`，分支名不过 wire）。lease 三决策：parked warm 命中（**不 fetch**，避开 `worktree.rs:95-99`/`:185-205` 的无界致命 fetch）→ 池未满则 fetch-first 新建 → 否则**拒绝**（典型 `blocked{reason}` / 429 `SUPPLY_DEFERRED`，`error.rs:154`），绝不静默 reroute、绝不复用脏 slot。**return-don't-delete 严格 pool-only**：refcount 归零且 task 归档才 `git clean -fd`（保留 ignored 暖目录）+ 切回 detached base + `state=parked`；`git status --porcelain` 非空拒清。
3. **唯一新表 `worktree_leases`**：列含 `mode`、复合身份键 `(host_id, workspace_id, dir_key)`（`dir_key` 是相对 workspace 根的规范化路径，注册根为 `'.'`）、**可空 `worktree_name`**（reuse-to-root 为 NULL）、`holder_instance_id`（可空，attach-lock 持有者）、`refcount`、`state(leased|parked|provisioning)`、`branch/base_oid/project_id/created_at/released_at`。「与 N 个 task 共用」= 同复合键行的 refcount，对 reuse-to-root 同样计数。目录端 catalog（`WorktreeRecord` `worktree.rs:12-19`）增量 `state` 与 `leasedBy`，serde default 零迁移。
4. **既有回收路径必须 lease-aware（根因修复）**：`remove_record`（`worktree.rs:153`）、`worker.remove_worker`（`worker.rs:217`/调用 `:293`）、`delete_instance`（`http.rs:473`/`store.rs:2093`）、`retire_worker`（`workers.rs:1284`）在物理删除 pool slot 前必须查 `worktree_leases.refcount`：`refcount > 0` 改走 return/park 不删目录；reuse 绑定下本就零操作。`worktree.rs:168-169` 的注释记录的是单 worker retire 的 `--force` 语义，不是多 task 共享安全的证据；任务 2 以「对 refcount>1 的 slot 发 worker.remove 不物理删除」为回归断言。
5. **看板投影：8 态 → 4 列，只读，不存储**。`GET /v1/board?project=` 独立于池层；待办 = pending/placed/deferred/parked（+ failed 且 `placement == None`），进行中 = running/stalled（+ failed 且 `placement != None`），已完成 = **只有 done**，已归档 = `archived_at != null`（正交过滤，不是第五列）。**failed 归位规则**：`TaskState` 无历史字段且 failed 可从六态进入（`task.rs:52-64`），无法反推失败前列，故不引入 pre-fail 列存储（守第 8 条预算），按可派生的 `placement.is_some()`（字段 `task.rs:225`）固定归位并叠红色 failed 角标（携 `blockedReason` `task.rs:232`），不折进已完成。**拖卡列→列可多跳**：可拖性由当前 state 按 `can_transition_to` 预计算，不可达列禁用并提示；待办→进行中目标 running，pending/deferred 走 pending→placed→running 多跳、placed/parked 单跳，每跳合法 PATCH、任一非法整体复位；进行中→已完成目标 done 时 running 单跳、**stalled 走 stalled→running→done 多跳（`can_transition_to` 无 stalled→done 边，`task.rs:57-58`）**，路径一律由 `can_transition_to` 计算；归档只走 `POST /v1/tasks/{id}/archive`，不改 state。**已完成列永不解锁依赖**（I1；land 只走 gate/`POST /v1/tasks/{id}/land` `tasks.rs:442-465`）。
6. **门控用 grant 动词表达，不是「agent 一律 403」。** task 创建（`create_task` `tasks.rs:256`）与状态迁移（`set_task_state` `tasks.rs:327`）经 `require_dispatch` → `require_grant(.., GrantVerb::Dispatch)`（`tasks.rs:203-208`，授权调用在 `:208`）；land 经 `require_grant(.., GrantVerb::Land)`（`tasks.rs:449`）。`require_grant`（`crates/remuda-hub/src/agent_scope.rs:104-127`）对非 Agent 来源放行，对 Agent 要求其 instance 逐字持有该 grant；被派发的协调员本身也是 Agent 来源（`agent_scope.rs:680`「a launched coordinator is always agent」，另见 `:168`；GrantVerb 枚举 `crates/remuda-protocol/src/enums.rs:106`）——持 grant 且在 project scope 内的协调员 agent 被授权例行建 task/迁移/落地，无 grant 或越 scope 才 403。「Human-only、agent 403」仅保留给 D-017 真正限制的动词（跨实例控制 / 跨 host create / agent keys），不适用于 task 写操作。非法迁移由 Hub 按状态机拒绝、UI 预禁用，是状态机权威而非身份门。
7. **批注 = composer 设备本地草稿（零 wire/零表）**：两级载体（卡片级 / 消息内锚点 `①`，选中只读详情正文或 transcript 生成），「本次发送带 N 条批注」徽标，发送时作结构化前缀拼进 prompt 后清零，不新增协议事件流、不把看板协议注入 agent 提示词。**任务空间 = 文件视图的客户端过滤投影（零新端点）**：项目空间保持既有 `hostId+workspaceId` 轴；任务空间按 task 的 session 集合 + `owns[]` glob 在客户端过滤已实现的 `workspace.scm.status/diff/file`（`workspace_scm.rs:37-53`、可用性 `:170-176`；Web `filesViewModel.ts:75-83`、`filesApi.ts:43`），空投影「还没有文件」，六可用性状态透传（files-view-contract §3.6）；`Workspace` 实体写死 `repository: NotApplicable`（`runtime.rs:3055-3056`）与该独立 RPC 路径无关。
8. **迁移预算硬封顶**：一个增量可空列 `archived_at`（`ALTER TABLE` 落 `tasks.rs:143` 的 `migrate()`，**不是** `store.rs:4430-4445` 只迁 instances/hosts 的 `ensure_column` 区块）+ 一张新表 `worktree_leases`；`workspaceBinding` 落 `doc_json`、catalog 字段与旧行靠 serde default、看板列与 `SE-nn` 不存储、failed 不增 pre-fail 列。旧行行为字节一致。超预算需单开决策。
9. **wire 增量与 M4 安全边界**：新 Node RPC `worktree.lease`/`worktree.return` 必须同时加进 `is_worktree_method`（`worktree.rs:207-209`）与 `handle_rpc` 的 `match`（`worktree.rs:29-36`），经显式 dispatch 臂（`runtime_link.rs:164-166`），不落兜底（兜底 `runtime_link.rs:178` → `dispatch_method` `hubnode_codec.rs:198-200`，未知方法今为错误而非历史 `{"ok":true}`，注册义务不变）。新 Hub 路由 `POST /v1/worktrees/{name}/lease|return` 紧邻 `create_worktree`（`http.rs:1783-1805`）、同门控（`:1789-1790`）；转发 Node 的 params **只含 `{hostId, workspaceId, name, base, taskId}`**，绝不转发 `path`/`repo`（镜像 `http.rs:1795-1802`，security-review-2 M4）。reuse 不触发这两个 Node RPC；reuse return 不向 Node 发 RPC。
10. **冲突核对（实施前已逐条核对，全文见 [task-model.md](./task-model.md) §11 与证据 §3）**：与 **D-024** 无矛盾——共用是 task 层 refcount，Space 键不合并、`Project.members[]` 仍引用该键；与 **D-033** 无矛盾——I1 原样，done 列不解锁、只认 `landed_sha`；与 **D-035** 无矛盾——池满/冲突显式 blocked、不静默替换或降级，carrier 次序不触碰；与 **D-047** 无矛盾——lease 拒绝复用 `SUPPLY_DEFERRED` 形状，不碰 API 交付/路由子模式；与 **D-049** 无矛盾——看板 compact 塌缩为 `/m` 单列分段过滤，会话本体与文件视图仍是共享 `/s/:id`、不造第二 transcript。另遵守 ui-spec §1.3（不分屏）、D-031（池只跑本地 git，无端口/隧道）、files-view-contract（轴与六状态）、M4（参数白名单）。
11. **参考产品合规护栏（人工审查门，脚本不覆盖）**：凡入库文件（设计文档、ADR、ui-spec、证据）只以泛称描述参考产品（如「某参考看板产品」）；开源项目（如 herdr、paseo、vibe-kanban、Codex）可具名引用；**绝不写内部产品名，绝不粘贴/引用/转述内部文档（内部文章、wiki），绝不写公司主机名（既有 `devbox`/`devbox-sg` 占位约定除外）、用户名、主目录路径；证据截图只提交 Remuda 自身渲染（390/1440），绝不提交参考产品截图。**

**由谁**：coordinator（task-model 计划 §(B) 设计与 §(D) D1–D12 推荐默认值，所有者未另行拍板即按默认执行；任务 1 t-spec 落规格，零代码）。

**依据**：[task-model.md](./task-model.md)（由派工计划 §(B)/§(D) 派生的入库设计文档，含全部 file:line 复核）、[ui-spec.md §1.5/§2.9](./ui-spec.md)、[coordinator-hierarchy.md](./coordinator-hierarchy.md)（Project schema §3.2、台账分层）、[files-view-contract.md](./files-view-contract.md)、[coordinator-guide.md](./coordinator-guide.md)（证据/密钥/gate 规则）；代码锚点见 [evidence/task-model-1.md](./evidence/task-model-1.md) §4（全部在 `1468a2ba` 上复核，含两处相对派工计划漂移的记录：`runtime_link.rs` 兜底臂与 `runtime.rs` 的 `NotApplicable` 行）。

## D-052

**2026-09-23 · provenance-first UI：零新 wire 的出处修复、审批卡不伪造 risk/会话边界、completeness 活过折叠、opaque 溯源、InboxShell 单壳、已上线 `/board` 上 graft、token 与护栏口径**

| 日期 | 2026-09-23 |
|---|---|
| 状态 | adopted（规格层；实施按 ui-upgrade 批次任务 2–13 跟进，本决策先合） |
| 相关 | [ui-spec.md §2.2/§2.5/§2.9/§3.3/§3.4/§4.7](./ui-spec.md)、[evidence/ui-upgrade-1.md](./evidence/ui-upgrade-1.md)、D-002、D-024、D-033、D-035、D-038、D-039、D-040、D-041、D-042、D-045、D-046、D-049、D-050 |

**背景**：operator 在派发 / 审阅 / 门控 / 落地 agent 工作时要能看清「刚才到底发生了什么」，而现状审计（研究阶段完成，2026-09-23 在当前 main 重新复核）发现多处出处缺损，且全部可在**既有 wire** 上补齐：token 采用率约 13%（`var(--text-*)` 68 处对字号字面量 434 处），`--amber` / `--info` 全库无定义，`session.module.css:1003,1016` 靠字面 fallback 色渲染；折叠行 `FoldedToolRow`（`web/src/features/session/ToolCard.tsx:386-401`）不接收 `completeness`，partial 卡一折起就看不出结果不完整；opaque 节点（`web/src/features/session/assemble.ts:626-646`，节点类型 `:133`）构造时丢掉 envelope 已有的 `seq` / `source`；内联审批卡只有 title + description + 按钮，而协议 `ApprovalRequest`（`web/src/types/generated.ts:98-106`）**没有** `risk` 字段、`DecisionOption`（`web/src/types/interaction.ts:4-8`）**没有** `destination` 字段——harness 实测的 permission suggestions 同时包含 `destination: "session"` 与 `destination: "localSettings"`（仓库自带两份 Claude fixture 各含两者），前端无法区分，画 risk 分或写「本会话内一直允许」都是 §3.3 禁止的伪造出处。另有两条 IA / a11y 债：`/approvals` 与 `/m/inbox` 是两份代码路径且各带一处错误 a11y（桌面 kind 分段无 radiogroup role、手机侧声明了不存在的 `tabpanel`）；loading/empty/error 三态多页空白。审计另列的三条陈旧判断及 `/board` 上线后复核结论全文见证据 §4。

**决策**：规格条文以 [ui-spec.md](./ui-spec.md) §2.2 / §2.5 / §2.9 / §3.3 / §3.4 / §4.7 为准；此处钉边界、预算与编号归属，不重复条文。

1. **迁移预算：零。** 本批零新 wire 字段、零新表、零新端点。所有新画的出处都读既有 envelope 的 `seq` / `source` / `completeness`（`web/src/types/generated.ts:2658,2667,2863`）与既有 `Interaction` / `DecisionOption`。凡触及存储 / wire 的提案默认预算即零；真实范围词（session vs localSettings）需要把 `destination` 提到 `DecisionOption`，明确不属本批。
2. **审批卡不显 confidence/risk 分，也不在 UI 里断言会话边界。** 线框旧文案 `risk` 与 `Always in this cwd` 改写为「**preview 原文 + carrier + deadline + harness 原范围标签**」：命令/补丁以等宽 `<pre>` 呈现原文（不二次改写，超长 `title` 给全文，截断由 Node 侧完成）；deadline 取既有 `expiresAt`，缺失画 `—`（不画「无限期」/0）；harness 已产出的 `opt.label`（`crates/remuda-signal/src/decision.rs:69-74`，如 `Always allow (acceptEdits)` / `Always allow this rule`；`AllowSession` 选项逐条生成于 `request.suggestions`，`crates/remuda-signal/src/approval.rs:83-90`）原样保留并加粗，按钮下方统一次要文字「**按 harness 建议的范围持续允许**」，不写「本会话」「本次会话」「永久」。理由是 §3.3：协议没有的字段不允许 UI 编造。实现批次以反向断言钉「页面无『置信度/风险/本会话』字样」。
3. **completeness 三值不变，只让它活过折叠。** `structured` / `partial` / `screen-derived` 不新增第四种；`FoldedToolRow` 折叠后仍渲染虚线边 +「不完整」，与展开态同生共死。**不**把 completeness 接到 interaction transcript 节点或审批卡——数据通路不存在（interaction 不经 transcript 节点分发；审批卡读 hub 的 interactions 列表，类型上无 completeness、无回指 observation 的 eventId），接通需先做 store/assemble 连接键调研且可能触 wire，按批次计划 D11 默认结论本批不做。
4. **D-041 的折叠顺序保留并加回归断言。** 「折叠分支必须在 family 判定之后」不动（否则 Workflow 卡被误折、1Hz 走针不启动、elapsed 出处静默死掉）；实现批次必须断言 settled Workflow 卡 compact 下不折叠、running Workflow 卡头 elapsed 递增，而不是只断言「卡可见」。
5. **opaque 行带 `seq N · <source>`。** 节点从 envelope 取既有 `seq`/`source`（零 wire），折叠行行首等宽显示，raw JSON 仍折叠；opaque 依旧不参与 Compact 折算成功、不当终态。
6. **`/board` 已上线，本批只 graft。** `/board` 路由与桌面三列只读投影（failed ⚠ 角标、归档过滤器、refcount footer、拖卡多跳、只读预览）已在 main；本批**不新建页面、不改路由**，只在既有文件上补仍缺信号（`reachableColumns()` / `isUnlockedByLandedSha()` / `lockedDepIds()` 目前在 `web/src` 测试外零消费）。**已完成列永不暴露 land**（land 只走 gate，D-050 I1）；右栏三 tab 与批注锚点不在本批。
7. **门控面收敛为 InboxShell 单壳，两侧档位各自保留。** 桌面 `/approvals` 与 compact `/m/inbox` 渲染同一 `InboxShell`（`mode` 由父路由决定、不消费 viewport hook）+ 同一 `ApprovalCard` + 同一 `role=radiogroup` 分段（命中 ≥ 44px），`?focus=`/`?kind=` 与 testid 两侧一致；桌面三档（含已离队）+ 主机/Workspace 过滤芯片保留，手机两档（无第三档）保留，档位由 mode 组成而非静默删行为；compact 重定向口径不变（D-049）。
8. **只新增四个 CSS 变量层 token。** `--warn` / `--info`（取代从未定义的 `--amber` 与带字面 fallback 的 `--info`）与 `--text-xl`（18px）/ `--text-13`（13px），双主题各给一值，作文本/描边对 `--ink-2` ≥ 4.5:1；不新增 z-index / elevation / 阴影 / disabled token（与 §6 的 1px line、无投影方向相反，且不服务出处论点）。
9. **护栏按文件白名单 opt-in。** stylelint 只对已迁净的目录打开 error 级；白名单每项必须带摘除批次注释，是台账不是地毯。「辅助文本 vs 图形标注」的分类口径由 [ui-spec.md §3.4](./ui-spec.md) 给出：线性排版里要读的文字受 `var(--text-aux)` 下限、不得豁免；环内数字等与形状绑定的图形技注可豁免、逐站点登记。
10. **合规护栏（人工审查门，脚本不覆盖）。** 可点名开源项目；研究阶段的参考清单只定性为「**MIT 许可的交互组件画廊，不声明任何 spacing/type/colour 规则**」——采纳其交互 pattern，像素与视觉默认整体拒绝。入库文件不得出现参考站母产品名、内部文档转述、主机名（既有占位约定除外）、用户名、家目录路径；证据截图只提交 Remuda 自身在 **390 与 1440** 两个宽度的渲染。
11. **ledger 浅色主题由 D-053（任务 15）正式化；D-052 不处理主题。** 本决策的对比度口径（两主题各给一值、AA）只是新 token 的验收方式，不构成双主题决策本身。
12. **冲突核对（实施前已逐条核对，全文见 [evidence/ui-upgrade-1.md](./evidence/ui-upgrade-1.md) §3）**：与 D-002 / D-024 / D-033 / D-035 / D-038 / D-039 / D-040 / D-041 / D-042 / D-045 / D-046 / D-049 / D-050 均无矛盾；ui-spec §1.2（`/board` 路由已登记）、§2.5 桌面线框（已离队档与主机/Workspace 过滤保留）、§6（深色 v1 口径需随双主题裁决改写，归任务 15）、§4.7（compact 看板分段过滤，批次计划 D8 本批不做）四节的逐条结论同见证据 §3。

**由谁**：coordinator（ui-upgrade 批次派工计划 §(B)/§(D) 设计与推荐默认值，所有者未另行拍板即按默认执行；任务 1 c-uispec2 落规格，零代码）。

**依据**：[ui-spec.md §2.2/§2.5/§2.9/§3.3/§3.4/§4.7](./ui-spec.md)；代码锚点（均在 2026-09-23 当前 main 复核）：`web/src/types/generated.ts:98-106,2658,2667,2863`、`web/src/types/interaction.ts:4-8`、`crates/remuda-signal/src/decision.rs:69-74`、`crates/remuda-signal/src/approval.rs:83-90`、`web/src/features/session/ToolCard.tsx:386-401,500-522`、`web/src/features/session/assemble.ts:133,626-646`、`web/src/features/session/OpaqueRow.tsx:4-16`、`web/src/features/session/session.module.css:1003,1016`、`web/src/pages/NewSessionPage.tsx:298-302`、`web/src/pages/ApprovalsPage.tsx:42-66,232`、`web/src/features/mobile/inboxRows.ts:24-25`、`web/src/features/tasks/{Board.tsx,boardModel.ts,boardColumns.ts}`；Claude fixtures `crates/remuda-testing/fixtures/claude/claude-permission-host-allow.jsonl` 与 `crates/remuda-claude-wire/tests/fixtures/claude-permission-host-deny.jsonl`（各含 `session` 与 `localSettings` 两种 destination）；逐条旧→新对照、冲突核对表与三条陈旧判断复核见 [evidence/ui-upgrade-1.md](./evidence/ui-upgrade-1.md)。

## D-053

**2026-09-23 · UI 整体重做：角色令牌、深浅双态、同源字体、阅读列与单侧栏**

| 日期 | 2026-09-23 |
|---|---|
| 状态 | adopted（规格层；实施按 UO-1…UO-13 跟进，本决策先合） |
| 相关 | [visual-system.md](./visual-system.md)、[ui-spec.md §1.1/§1.3/§1.4/§2.1/§2.2/§2.3/§2.5/§2.9/§3.4/§4.6/§4.7/§6/§7](./ui-spec.md)、D-024、D-038、D-039、D-040、D-041、D-042、D-049、D-050、D-052 |

**背景**：所有者判断现有界面缺乏质感，授权完全重新设计。所有者同时反馈三件事：webview 操作发涩；手机上键盘遮住内容；终端不显示、问题到不了手机。在 main（`42ccd7ee`）上复核现状，结论如下：

- 颜色按色相命名。同一个 `--dust` 同时承担主按钮、选中态和「需要你」三种含义（`web/src/styles/ui.module.css:10-15` 的 `.btnPrimary` 填充、`:323-324` 的 `.chipOn` 选中描边、`:52` 的 blocked 点与 `:386-391` 的待审批卡）。
- 对比度不足。次级文字 `--mute` 在 `--ink-2` 上只有 4.12:1；终端 brightBlack 在终端底上只有 1.42:1（`web/src/features/session/tty/theme.ts:61`）。
- 字体混排。正文 webfont 只打了拉丁子集（`web/src/main.tsx:7-9`），中文逐字回落到系统字体，同一行里出现两套字形。
- 字号与焦点。`font-size` 字面量中 11px 有 99 处，另有 11.5px 12 处、10px 19 处、10.5px 11 处、9px 2 处、8px 1 处，内联样式里还有 10px（`web/src/features/session/LaunchedBy.tsx:47`）；全局焦点环只有 1px（`web/src/styles/tokens.css:83`）。
- 触控尺寸判定错误。触控 44px 按宽度（`@media (max-width: 767px)`）而不是按指针判定（`web/src/styles/ui.module.css:356-372`），违背 D-039 关于 compact（布局）判定与 coarsePointer（触屏）判定不得混用的要求。
- iOS 放大。fine 指针下文本输入 14px、手机本地输入 15px，均低于 iOS 不放大的 16px 阈值，聚焦时会放大页面。
- 渲染开销。会话页每秒整页重渲染（`web/src/pages/SessionPage.tsx:154` 的 `useNow(true)`）；另有 `optimizeLegibility`（`web/src/styles/tokens.css:130`）、`backdrop-filter` 与 `transition: all` 等开销。
- 导航层数。桌面会话路由叠了四层导航（48px 图标轨、会话列表第二列、SpaceTabs 行、页头）。
- 主题。ui-spec §6 仍写着「v1 只做 A（深色）」。

**决策**：机械细节（令牌值、对比度矩阵、控件尺寸）以 [visual-system.md](./visual-system.md) 为准，这里只钉边界。

1. **颜色只按角色引用。** 角色族如下：
   - 背景：`--bg-nav/--bg-canvas/--bg-surface/--bg-raised/--bg-input/--bg-inset/--bg-hover/--bg-selected`
   - 文字：`--fg-strong/--fg-body/--fg-muted/--fg-faint`
   - 线条：`--border/--divider/--border-control`
   - 链接与焦点：`--link/--focus`
   - 状态：`--attention-*/--danger-*/--success-*/--unknown-*`
   - 主按钮：`--primary-fill/--on-primary`
   - 专用域：`--diff-*-bg`、`--tok-*`、`--term-*`

   十六进制字面量只允许出现在 `tokens.css`、`tty/theme.ts`、`web/index.html`（theme-color）、`manifest.webmanifest` 四处。尺寸令牌保留 `--text-*` 前缀，颜色角色不得占用 `--text`。
2. **状态色纪律。**
   - 琥珀只表示「需要你 / 注意」。
   - 红只表示失败和破坏性动作。
   - 绿只表示权威确认的成功（merged、passed、回执已确认）。idle、已连接、在线都不用绿。
   - 未知 = 中性色 + 虚线形状 + 文字「未知 / 待确认」（ui-spec §3.3、D-038 的「状态待确认」口径）。
   - 按钮不用状态色填充；主按钮是中性的反色填充。
3. **深浅两态各自调校，不做反相；由 CSS 解析，不加任何阻塞脚本。**
   - 深色值写在 `:root` 上。浅色值写两处，内容逐字相同：`@media (prefers-color-scheme: light) { :root:not([data-appearance="dark"]) }` 和 `:root[data-appearance="light"]`。
   - 偏好键 `runtime.theme.v1` 取值 `system | dark | light`，默认 `system`。旧值 `night`、`ledger` 分别读作 `dark`、`light`；键缺失视为 `system`。
   - 显式选择时，由 `main.tsx` 在挂载前写入 `:root[data-appearance]`；跟随系统时不写这个属性。
   - `data-appearance` 只允许出现在 `:root` 上。组件模块里不得出现任何模式分支；凡是随模式变化的值，一律做成令牌。
   - theme-color 用两个带 `media` 的 meta 标签。
   - 只发布一套调色板「墨」（微暖、低彩）。**本条落实 D-052 第 11 条预留的浅色主题正式化。**
4. **终端是恒深色的仪器。** 深浅两态下终端相同。xterm 标准 256 色立方不再染色；每个非黑 ANSI 色在终端底上都 ≥ 4.5:1；终端输出的颜色不被改写。
5. **字体。**
   - UI 与长文同用平台系统字体栈（含 PingFang SC、微软雅黑、Noto Sans CJK），不打包任何正文 webfont。
   - 等宽只用 IBM Plex Mono（OFL-1.1，许可证文件入库），只用于代码、路径、终端、ID 和需要对齐的数字。
   - 阅读感靠度量：正文 16px/28px，行宽上限 720px。
6. **字号下限。**
   - 承载信息的文字 ≥ 12px。辅助文字 `--text-meta` 在所有宽度下都是 12px，落实 D-039 对 `.meta` 一类辅助文本桌面与手机统一 12px 的要求。
   - 11px 只用于与形状绑定的徽标数字和 kbd 字形，属于 D-052 第 9 条的图形标注豁免，逐站点登记。
   - 文本输入在 `pointer: coarse` 下为 16px。
7. **命中区按 `pointer: coarse` 判定，不按宽度。**
   - 字形与图形控件的可见尺寸不变，44px 只靠 `::after` 或 padding 形成的热区，热区互不重叠（ui-spec §3.4）：返回箭头 20px、`终端|结构` 分段项 26px、手机 Stop 方块 32px、图标按钮 32px、芯片 24px。
   - 本条澄清：文字按钮和输入框不是字形，coarse 下可见高度可以直接取 44px。
8. **层级靠色阶与留白。**
   - 只有浮层（菜单、弹出层、对话框、sheet）带阴影，共两级：`--shadow-2` 和 `--shadow-3`。**本条取代 D-052 第 8 条中「不新增 z-index / elevation / 阴影 / disabled token」一句里的阴影部分**——该条其余口径（不新增 z-index / disabled token）不变。
   - 全站不使用 backdrop-filter。
9. **动效。**
   - 只动 opacity、transform 和颜色，时长 100–240ms。
   - 流式内容没有入场动画；模式切换即时生效。
   - reduced-motion 下只保留标记了 `data-motion="essential"` 的元素。
10. **桌面导航骨架**（≥768 且非 coarse 矮屏）。
    - 一条带文字的侧栏，`<nav aria-label="主导航">`：会话、收件箱、任务看板，外加项目区和管理菜单。每页一条 48px 页头。取消 48px 图标轨和会话路由上的第二列。
    - tab 条只在 `/s/*` 上出现；列表路由不再有 tab 条。**本条修订 D-024**：D-024 原口径为「当前 space 的 agent 会话显示为内容上方 tabs」「桌面支持可折叠 Spaces/Sessions 左栏及首字母窄轨」（详见原 ui-spec §1.4）——现改为 tab 条只属于会话路由，Space 面板（重命名、排序、已退出分组）移到 `/sessions` 的索引列；侧栏只有一个折叠概念。
    - **选中态（显式修订 D-024 addendum 的「侧栏强当前态」）**：addendum 原条文要求「当前 space 与当前会话都用品牌左条 + 底色 + 加粗标题」；现取消品牌左条与首字母窄轨，当前 Space、当前会话与主导航选中项统一为 `--bg-selected` 底 + `--fg-strong` 字 + 加粗，深浅两态都不靠品牌色条做唯一指示（仍保留底色 + 字重，不单靠颜色）。addendum 的关闭 / dismissal /「已退出 (n)」分组 / 恢复与删除语义全部不变。
    - 项目范围由侧栏项目区设定，写入同一个项目过滤偏好；`/projects` 页头的切换器保留。
    - 快捷键：
      - ⌘/Ctrl+B 在所有桌面路由上折叠侧栏；
      - ⌘/Ctrl+1..9 永远对应「眼前的第 n 个编号」：`/s/*` 上是 tab 条，`/sessions` 上是列表徽标，列表徽标仍来自 `switchSlots()`；
      - ⌘/Ctrl+[ 和 ] 切换 Space。
    - tab 集合采用所有者 2026-09-23 拍板的方案：会话的 `instance.taskId` 非空时（D-050：实例经既有 `instances.task_id` 归属 task），tab 是该 Task 的会话；否则是所在 Space 的 `visibleTabs`。零新增请求，不依赖轮询。关闭语义按 D-024 addendum 不变，关闭记在会话所属的 Space 上。
    - compact 路由树、重定向与共享会话页按 D-049 不变。
    - **会话列表宽行（修订 D-038 的默认视口清单）**：`/sessions` 容器宽 ≥960 时行高 48px、单行 grid，在 D-038 的五要素（状态点、标题、kind 芯片、徽标、一句下一步）之外**允许额外**出现「主机/工作区·分支」与相对时间两列；<960 时两行共 56px，不额外加列。三维 wire 原文、`ins_`、driver、model 仍只进 `<details data-testid="session-wire">` 或 `title`，D-038 其余口径不变。
11. **会话页。**
    - 桌面页头只有一行；host 与 cost 仍在主行（D-040 的桌面规则不变）。
    - 「运行详情」的触发从第二行移进 ⋯ 菜单。**取代的是旧 ui-spec §2.2（基线 `80db05b8:docs/design/ui-spec.md:335`）中「第二行只有这一个触发器（`▸ 运行详情`，无其他 token）」的版式**——该约束是规格旧文，不是 D-040 (3) 的条文。D-040 (3) 钉住的内容（哪些诊断进 disclosure、默认收起、展开态按设备持久化、`session-meta` testid）全部不变；面板仍在页头下方的文档流里展开。
    - compact 页头仍是一行 52px。原来的单枚 space 芯片并入标题块：标题块兼任 Space 抽屉的触发（`spaces-drawer-open`），Space 名包在 `<span data-testid="spaces-chips">` 里；`/s/` 上不使用 `space-chip`。**本条取代 D-040 (1) 的单枚芯片形状**，抽屉行为与 testid 不变。
    - D-049 的截断三步逐字不变：标题先省略；其次 Space 名退成首字母；最后状态文字收起。状态点始终在行上。
    - `终端|结构` 分段与 Stop 永不进 ⋯（D-040 (2) 不变）。
    - 已结束的会话由停靠区的结束条承载续接；页头和重启横幅里的续接入口删除，只保留拇指区这一个入口。
12. **工具调用折叠。**
    - 已 settled且成功的工具调用，在所有宽度下都默认折成一行。**本条修订 D-041 的「桌面默认态不变」**——该条原允许折叠只在 compact 默认生效。
    - D-041 的豁免集合（Workflow、error、interaction、running / 未 settled）与「折叠分支必须在 family 判定之后」不变，D-052 第 4 条的回归断言不变；读者显式「全部折叠」的既有行为不变。
13. **手机。**
    - compact 顶栏是一条 52px。
    - 首页级屏的底栏改为 56px（原 64px，即 ui-spec §4.7 的 `--bar`），**本条修订 ui-spec §4.7 的这个数值**。
    - 会话路由不渲染 app 底栏，也不渲染 tab 行（D-049）。
    - composer 收起态是单行 56px（D-042 不变）。
    - 软键盘状态沿用 `html[data-keyboard="1"]` 与 `--workbench-height`。键盘弹起时正文保底占可见带的 40%，待处理卡、路由故障和 composer 永不隐藏。
14. **兼容层与护栏。**
    - 旧 token 名在 `tokens.css` 末尾保留为别名，过渡期内由 JS 维护 `data-theme` 镜像。二者都在最后一个界面任务合入后删除。
    - 重做过的模块首行标 `/* @tokens strict */`。单测在这些模块里禁止以下写法：十六进制、旧 token 名、`[data-theme`、`[data-appearance`、`prefers-color-scheme`、小于 12px 的字号字面量（登记过的图形站点除外）、`transition: all`、`backdrop-filter`。
    - `tokens.contrast.test.ts` 在以下全集上断言：文本 ≥ 4.5:1，控件描边与焦点 ≥ 3:1。
      - 背景：五种不透明底；hover、selected 分别叠在这五种底上。
      - strong、body 另外叠在三种状态色底 × 五种底上。
      - 各状态文字叠在自己的状态底 × 五种底上。

**后果**：

- 从未选过主题、且系统为浅色的设备，第一次会看到浅色，并且首帧就是正确的颜色。
- 显式选了与系统相反模式的设备，首帧行为与今天相同：在模块脚本执行前可能短暂出现系统色。
- 移除正文 webfont 后，截断点和虚拟行的估算高度会一次性变化；行高由 ResizeObserver 重新测量。
- 切换模式只改属性，不触发 React 重渲染，终端不受影响。
- 多处可见文案会变。每个任务在同一个 PR 里更新对应的 e2e。
- ui-spec 相关章节在本决策的同一个 PR 内改写。

**不做什么**：

- 不打包衬线或 CJK webfont；不做阅读字体偏好。
- 不做调色板选择器、URL 参数外观、顶栏主题开关、阅读模式、检查器。
- 不做数学排版、交互可视化、成果预览。
- 不做新建 Task 的界面；不做 compact 看板分段。
- 不改路由、wire、端点；不新增轮询；不做第二份 transcript；不做手机专用会话路由。
- 审批卡不显示 risk、置信度，也不断言会话边界。
- 不改写终端输出的颜色。
- 不加阻塞首帧的脚本。
- 入库文字不点名任何闭源参考产品。证据截图只提交 Remuda 自身在 390 与 1440 宽度下的渲染，且只在设置了 `REMUDA_EVIDENCE` 时生成。

**由谁**：所有者（重做授权）+ coordinator。所有者 2026-09-23 的四项拍板：

1. 默认外观跟随系统（计划 §7 选项 1A）；
2. UI 与正文同用系统无衬线字体，不打包衬线或 CJK webfont（2A）；
3. 只发布「墨」一套调色板，深色、浅色两态（3A）；
4. 会话有 `instance.taskId` 时 tab 集合是该 Task 的会话，否则是所在 Space 的会话；⌘1..9 跟随眼前的可见编号（4A）。

**依据**：[visual-system.md](./visual-system.md)（令牌契约与对比度全集）。代码锚点在 `42ccd7ee` 上复核：`web/src/styles/tokens.css:1-196`、`web/src/styles/ui.module.css:10-15,52,323-324,356-372,386-391`、`web/src/features/session/tty/theme.ts:15-68`、`web/src/pages/SessionPage.tsx:95,154,452-596,669`、`web/src/app/Shell.tsx:207-237,337`、`web/index.html:2,9`、`crates/remuda-hub/src/web.rs:1-9,100-107`。
