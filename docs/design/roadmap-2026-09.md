# Remuda 路线图（2026-09 下旬 → 11 月上旬）

> 状态：草案（coordinator 综合 2026-09-17 的调研 workflow：herdr / herdrx main 分支扫描 → 三镜头对抗核验 → 三份独立草案 → 三模型评审）。
> 输入：herdr `1a7c6915`（`../herdr`，只读）与 herdrx `183dc6a`（`../herdrx`，只读）自 2026-08-10 起的 main 提交；Remuda `1541cb24`+。
> 核验通过的可借鉴项 107/144（首轮 74/93，补扫 33/51：herdr socket-api、herdrx remote-access、herdrx ops-validation、Remuda 自查）。

## 0. 目标与顺序

目标不变：**Remuda 自我迭代，完全替换 herdr + coordinator 的 shell 脚本**（`coordinator-hierarchy.md` §1.1 目标 6 → M1 → M2）。三位评审一致：以「self-iteration-first」草案为主干，把「user-experience-first」的原生控件验收矩阵和「reliability-first」的有界可靠性契约嫁接进来；不要把六周花在 UX 或可靠性 backlog 上。

前提（不在本路线图内重排，落地后才开始对应里程碑）：co-gate、c-stdioobjects、c-nativecfg、c-modelsync、c-question、c-wfdrill、c-steer、c-permmode、c-effort3、c-usagehover。

## 1. R0 · 收口 M1：一整轮只用 `remuda` 动词（当周）

- c-stdioobjects 落地 → `remuda dispatch` 的 brief 能到达 ssh-stdio Node。
- co-gate 落地 → `remuda gate/land` 走项目 gate lane，`--then` 触发 demo 刷新。
- 用真实任务跑通 dispatch → watch → gate → land → retire，写 `docs/design/evidence/self-host-1.md`，同时删除 `/tmp/remuda-coord/*.sh` 的使用（脚本归档到 coord-state）。
- 已知缺口（本轮发现）：retire 后 Node 未终止/回收 claude-pty 子进程（Linux）；`stop/remove` 必须 SIGTERM + reap。

出口：evidence 记录一轮真实任务的每个动词与耗时；coordinator 不再 scp/ssh 任何脚本。

## 2. R1 · 循环安全底座（destructive 动词、不变量文件、不丢 brief）

来源：herdrx design-docs（`ca2539d` auth-admin-remediation、`e70a358` CLAUDE.md 规范）。评审修正：**不是**把 Agent 一律 403，而是 destructive 动词需要显式确认载荷（branch / worktree / target-dir / 资源名），并且 T2 座位（无论人或 agent）拿到的 grant 里明确列出哪些 destructive 动词可以自动化（§2.2/§2.5 已把 replace/retire 放在 T2 grant 面上）。

- `retire --force`、`worker replace`、`worker.remove`：需要确认载荷；journal 记录来源、载荷、决策者。
- 根 `CLAUDE.md` 作为唯一规范性不变量文件（术语表、三种「session」的区分：设备会话 / follow-tty 观察流 / 原生 agent 会话；destructive 规则；命名纪律），`AGENTS.md` 为符号链接，CI 断言链接与术语表存在。
- UI 文案规则：登录会话 / 断开访问 / 结束任务三者分开用词。
- `pty_queue`：PTY actor 退出时先关输入通道再排空，不静默丢弃排队的提交（herdr 借鉴，2/3 票）。
- `origin_allowed`：Origin 缺失时不再默认放行；Host 头不作信任来源。
- 部署不变量 CI：解析渲染后的 compose 配置断言（仅 bind mount、无 build、端口不外发），接进 `gate.sh`。

出口：`docs/design/evidence/coord-safety-1.md`：Agent/Bot 来源无确认载荷的 destructive 调用 403 + journal 行；带载荷的 T2 调用 200；CI 对 CLAUDE.md/AGENTS.md/compose 断言各一条失败用例。

## 3. R2 · ssh-stdio 载体诚实（quiet ≠ alive）

来源：herdr remote-session 扫描（11 项全部通过核验）。

- 应用层健康 ping（WSS 载体已有 15 s 心跳；ssh-stdio 没有）：静默超时 → Reconnecting，不是 online。
- 失败分类：Reconnecting vs Attention；`Permission denied (publickey)` 这类只有人能修的失败不再无限重试。
- 把 ssh 子进程 stderr（16 KiB 环）作为失败原因上报，而不是下游协议错误；attended 引导时实时转发 stderr，后台探测只捕获。
- 每主机独立写队列（字节+消息双上限），一台卡住的主机不阻塞其它主机。
- 端点世代协商：版本偏差不强制重启（为 R4 铺路）。
- A-022 修正（herdrx 借鉴，评审三人都要求）：独立性验收必须用**第二台机器断电**，不能用进程重启替代；Node 侧记录 PID 与 instance id 证明任务未受影响；写下 Remuda 实际承诺的窄不变量（D-019 vs D-028 A）。

出口：`docs/design/evidence/ssh-carrier-1.md`（假 ssh 二进制驱动的测试）+ `independence-1.md`（两机断电实测）。

## 4. R3 · watch 分类可信到能自动行动

来源：herdr agent-status 扫描（8 项）。前置：R1（destructive 门先有）。

- 覆盖 Claude 全部 spinner 帧，标题与活动行分开识别。
- 权限对话框按完整控件布局识别（含每个光标位置），替换宽泛短语匹配。
- Codex：存在当前 composer 时抑制弱 blocker 文本。
- OSC 证据带 agent epoch，不丢失识别前的启动字节。
- hook 生产者身份校验（不只是 socket 凭据与事件名）。
- 便宜的状态观察与昂贵的进程发现分离（800 ms 轮询不再每拍扫进程表）。
- 几何：不伪造 80×24；未知视口时拒绝而非猜测；最后交互者拥有 PTY 几何，其它观察者跟随（协议级规则，需 D 决策）。

出口：`docs/design/evidence/watch-fidelity-1.md` + `crates/remuda-screen/tests/` 抓屏夹具库；分类抖动率与误判率有数字。

## 5. R4 · 版本偏差与自升级不杀死机群

来源：herdr dist-config 扫描（11 项）+ herdrx 更新事务（`ca2539d` internal/updater）。这是自我迭代的结构性阻塞：Remuda 构建出新 Hub/Node 后，今天版本不一致会拒绝整条 SSH 连接（`managed.rs:130`）。

- 世代协商（R2 已铺）+ 「已安装 ≠ 运行中」分开记录与展示。
- 先校验 staged 产物再替换可执行文件；激活有版本、串行、限于 installer 拥有的路径；拒绝不支持的环境。
- `remuda node update`：签名清单 + SHA-256 + 原子切换 + 回滚，且回滚不能复活已吊销的授权。
- 发布契约：installer / manifest / 版本化文档一起发；操作技能与发布说明随稳定二进制。

出口：`docs/design/evidence/upgrade-1.md`：N+1 Hub 保持 N Node online；坏产物不动运行中二进制；回滚后被吊销 token 仍无效。

## 6. R5 · 交出座位：常驻 coordinator、bot 出口、实测容量（M2）

- 常驻 T2 座位：长生命周期 Instance，带 scope/grants（§2.5）；退出后从 Hub ledger 重新 brief，而不是靠历史上下文。
- Feishu intake 改道到项目 + 三类卡片（placement 带 `reasons[]`、gate 结果、interaction），owner 在卡片上答一次交互（200，不是 403）。
- 实测容量表（10/50/100 实例对 fake-harness：Hub RSS/FD、Node 每 PTY RSS）→ 默认保护上限，替代未测的承诺；`hostcap` 用实测值决策。
- 延迟提醒按接收者注意力复核后再发；WS 应用层关闭码分类 + 客户端按码决定重连策略（R2 借力）。
- 原生控件验收矩阵（UX 草案 UX1，嫁接为 M2 验收门）：model / effort / permission / question 的 requested→effective→provenance 每格有证据，无「假生效」。

出口：`docs/design/evidence/coord-m2-1.md`：§8.2 M2 验收全程对 fake Node 跑通；`capacity-1.md` 数表。

## 7. 待补与明确不做

- 补扫结果已并入：
  - herdr socket-api → R3/R5：提交后的活动门（只有 working|blocked 算 prompt 已落地）；能力广播的端点世代（缺方法只禁用一个动作，不断连接，R2/R4 同项）；优先控制通道（stop 在 socket 层应答并进入 fail-fast 排空）；前台进程组 leader 的 cwd 为准；`-c safe.directory` 按请求信任而非改用户 git 配置；CLI 对非人类调用者的人体工学（顺序无关参数、静默 EPIPE、JSON 错误）→ 作为 M2 常驻 coordinator 的调用契约。
  - herdrx remote-access → R2：无论用户配置如何都强制非转发 SSH 行为（D-031 的执行面）；取消自有 SSH 进程树并有界清理管道；ControlMaster 所有权与连接世代显式化；接受 Node 产物前做离线保身份自测（R4 同项）；可恢复的分阶段接入进度不绑定弹窗生命周期。
  - herdrx ops-validation → R4/R5：带表格与「明确不声称」的日期化验证报告；本地验证不推断公开发布；SBOM + notices 检查作为发布阻断门；镜像回滚≠数据回滚、拒绝 schema 降级、恢复不复活已吊销访问；SQLite WAL + synchronous=FULL（断电发现）；访问面运维不得杀远端任务的双证据吊销测试（R1/R2 同项）。
  - Remuda 自查缺口（补入对应里程碑）：supply 反馈回路开路——限流帧没有接进冷却机制（R3）；Claude 在 print 之外没有 usage 尾流，print 退役门永远满足不了（R3，c-usagehover 部分覆盖）；parity gate 已建成但不是 gate 步骤（R1）；coordinator 动词只有 CLI，没有 MCP 工具与 skill，常驻 coordinator 无法调用（R5 前置）；Project/Task/roster 没有 Web 面，M1 循环对手机端不可见（R5）；bots 页是硬编码 mock、路由是单一全局三元组（R5）；README 状态段落落后约 5 个里程碑（R0）；隧道禁令之后场外访问没有任何受支持路径（需要 owner 决策：仅内网 / 受支持的反向代理变体）。
- 本期不做：alt-screen 历史收割（驱动 agent 自己的鼠标滚轮）；每观察者独立 tab 视图；surface interest / 每连接投影 epoch；隐藏面板独立节奏；键位表驱动的模式栏；PWA precache/boot fallback（UX5，延后到 M2 之后）。
- 触碰热点的写入规则：`TerminalView.tsx`、`tty/client.ts`、`ws.rs`、`ssh_hosts.rs`、`store.ts`、生成的 API 文件同一时间只允许一个 worker 持有。

## 8. 风险

- 分类器是干预的承重输入：R3 只提高保真度，不使其绝对可信 → R1 的确认门是兜底。
- R4 最大也最可能吃掉窗口；先做世代协商与「安装≠重启」，签名更新事务可以滑到下一期。
- 用进程重启冒充 A-022 会给出假的独立性结论（herdrx 明说过）；必须两机断电。
- 容量在 R5 才实测，前四周 `hostcap` 仍按代理指标决策。
