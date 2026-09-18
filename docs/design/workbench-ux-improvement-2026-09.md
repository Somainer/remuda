# Remuda Web/PWA 工作台改进报告（含 Moshi 对照）

日期：2026-09-19（Moshi 桌面/手机对照补全）  
范围：当前 Remuda `web/` 工作台，加上 **Moshi** 手机 App 与 Moshi Desktop 的机制对照。本文件是体验盘点与优先级建议，**不实施代码**。  
不替代 [ui-spec.md](./ui-spec.md) 或 [workbench-ux-plan.md](./workbench-ux-plan.md) 作为规格权威。

Moshi 截图、文案、产品名只作为 Moshi 证据。它们 **不是** Remuda UI。Remuda 行为只从 `web/src` / `docs/design` 或 Remuda mock 截图得出。

**补观察（同日稍后）：** 用户指出镜像里一直停在 Chat View。随后从 Chat View **左缘滑开 Jump To**，点进另一 pane，手机离开结构对话，进入 **live 终端**（Mosh 徽章、Ctrl-L/Esc/Tab 底栏、agent TUI 正文，不是 hook 日志，也不是 Chat View）。Desktop 点开 **Diff** 侧栏（UNTRACKED 文件列表 + git history）。Chat View 的关闭按钮在镜像坐标下不好点；离开结构面的可靠手势是 Jump To / 切 pane，不是点标题。

> **二次实机（2026-09-19 深夜）修正了本文若干机制判断，见 [§11](#11-二次实机与网关实测2026-09-19-深夜)。**
> 其中 **§7.2 的「网关规范化」写反了**（规范化在客户端，网关送原始 journal 行）、**§7.5 的 Jump To 入口和 `switch` 语义写错了**、§7.6 的 Inbox 已实测、§8.5 的 git 面板手机端也有。
> §11 同时记录了 `web/src` 代码断言的逐行核对结果与 P0/P1 建议的**规格冲突**（3 条与 `ui-spec.md` 直接冲突）。

## 证据标记

| 标记 | 含义 |
|---|---|
| 观察 | 本次实机：Remuda mock PWA；Moshi iPhone（镜像截图）；Moshi Desktop（本机 `127.0.0.1:57482` Playwright，daemon `127.0.0.1:24543`） |
| 代码 | 当前 `web/src` / `docs/design` 源码核对 |
| 推断 | 由观察+代码提出的体验假设，尚未对照测试 |
| 资料 | 官方文档或既有设计意图，不等于本次点开的每一个按钮 |

Remuda mock 未连真实 Hub 做完整 agent 回合，也未发送 prompt、未改 Moshi 设置。Moshi Desktop 官方默认端口是 `24544`（资料：[Install Moshi Desktop](https://getmoshi.app/docs/install-desktop)）；本次该端口未监听。正在运行的是 **Moshi Desktop Tauri**，内嵌 UI 在 `http://127.0.0.1:57482`，gateway 在 `127.0.0.1:24543`。

---

## 1. 桌面工作台（≥768px 且非 compact 查询）

断点：代码 `web/src/lib/viewport.ts` `COMPACT_WORKBENCH_QUERY`。桌面是左轨 + 可选 Spaces 列 + 主列。

### 1.1 Shell / 航轨

- 观察：左轨 48px，图标为 ▤ 会话、◆ 审批（可带数字角标）、＋ 新建、⋯ 更多、底部 `me` 进设置。
- 代码：`web/src/app/Shell.tsx` 航轨与 `MORE_NAV`（主机 / 项目 / Provider / Bot / 设置，`web/src/lib/nav.ts`）。图标按钮 48×48（`Shell.module.css` `.icon`）。
- 代码：⌘/Ctrl+B 折叠 Spaces、1–9 切当前空间 tab、`[` `]` 切空间；输入框/xterm/QuickFind 不抢键（`Shell.tsx` `useLayoutEffect` + `isTypingTarget`）。
- 代码：QuickFind ⌘/Ctrl+K（`web/src/features/search/QuickFind.tsx`），搜已加载的实例/空间/主机元数据，不搜正文。

推断：航轨信息密度对桌面合适，但 ASCII 字形和 `me` 圆点让产品仍像调试壳。

### 1.2 Spaces 面板与 Tabs

- 观察：左侧 `Spaces / Sessions`，组可展开，当前空间品牌左条 + 加粗；组头显示计数；「已退出」默认折叠。顶栏 tab 显示标题、状态点、关闭 ×、右侧 ＋。
- 代码：`SpacesPanel.tsx` / `SpaceTabs.tsx` / `spaces/store.ts`。Space = `(hostId, workspaceId)`；显示名、顺序、折叠、关闭 tab 只存在本机 `localStorage`，不跨设备。
- 代码：运行中 tab 的 × 打开「停止并关闭 / 仅关闭标签」sheet（D-024）。
- 观察：面板底「活跃 / 待处理」与 `⌘/Ctrl+[ ] 切换` 提示。

### 1.3 会话列表

- 观察：分组「待处理 / 进行中 / 最近 / 已退出」；待处理置顶并带「等你批准 Bash」「AskUserQuestion · 1 题」；行内仍打印 `ready · waiting-interaction · connected`、`ins_…`、driver。右侧每行有 `send…`、发送、enter/esc/ctrl+c、stop。
- 代码：`SessionList.tsx` 分组与 `sessionFilters.ts` URL 条件。当前 Space 固定范围（观察截图文案「当前 Space 固定范围」）。
- 推断：列表同时承担「扫一眼」和「遥控台」，诊断字段与快捷键抢标题。

### 1.4 会话 · 结构化视图

- 观察：标题 `空间 / 会话名` + 状态点「运行中」；第二行 meta：`host · driver · delegation · provider · lifecycle · seq · connectivity · $cost · structured-only — 无终端 tab · live · native …`。工具条 Compact / 文件 / 原始事件 / 停止。正文：用户气泡、工具折叠「7 次工具 · 1 段思考」、Workflow 树、assistant 段落、usage、未识别事件。底部 Live「等待操作」、Task 列表、composer。
- 代码：`SessionPage.tsx` 顶栏与 meta；`Transcript.tsx` 虚拟窗口；`ToolCard.tsx` Bash 默认展开命令 + stdout details；`LiveStatusStrip.tsx`；`TaskTrack.tsx`。
- 代码：print 会话 `uiMode === structured-only` 时无终端 tab（`SessionPage.tsx` 与观察一致）。PTY 会话有 `ViewSwitch` 终端/结构（`ViewSwitch.tsx`）。
- 观察：composer 一排芯片（附件、harness、effort `?`、用量 6%、权限）+ 打断/排队。placeholder 写满桌面快捷键。

### 1.5 会话 · 终端视图

- 本次 mock 实例为 `claude-print`，无 tty 切换（观察：无「终端/结构」开关）。
- 代码：`TerminalView.tsx` xterm + `terminalFit` + `AuxKeys` + `LocalInput`；PTY 输入策略与触摸滚动在 `tty/`。
- 资料/仓库证据：`docs/design/evidence/terminal-1-keybar.png` 显示手机终端辅助键与「本地输入 / 直连」；`session-chrome-1-switch-tty-1440.png` 为桌面切换。

### 1.6 Composer

- 代码：`Composer.tsx` 空闲发送、忙碌排队、工作中 ⌘/Ctrl+Enter 插队；插队与 Esc 打断使用 `window.confirm`。
- 观察：审批卡在输入上方；effort 未知显示 `?`。
- 代码：草稿 `lib/drafts.ts` 按 instanceId。

### 1.7 新建会话

- 观察：桌面居中 dialog「新建会话」：要做什么、主机、工作目录/worktree、agent 芯片、模型、权限芯片、effort 滑杆、取消/开始。背后列表被遮罩。
- 代码：`NewSessionPage.tsx` 桌面 `variant=popover`，手机 `sheet`；`Sheet.tsx` 焦点圈定。

### 1.8 审批中心

- 观察：顶栏「待处理 n」、kind 分段、主机/空间芯片；卡上允许一次/拒绝/打开会话；AskUserQuestion 内嵌选项；离线主机按钮禁用。
- 代码：`ApprovalsPage.tsx`；底栏/航轨角标来自 pending interactions。

### 1.9 设置与更多

- 观察：设置分组「外观与输入 / 通知 / 连接与登录 / 主机默认值」；主题 Night Corral / Ledger；权限与 effort 默认；推送说明。
- 代码：`SettingsPage.tsx` `GROUPS`；本地偏好即时生效，文本字段显式保存。
- 观察：主机页可达（本次截了 `/hosts`）。

---

## 2. 手机 / compact 工作台

同一套路由。compact 时隐藏航轨和 Spaces 列，显示底栏。

### 2.1 底栏与空间

- 观察：底栏 会话 / 审批(角标) / ＋ / 更多；顶空间 chips（当前带「· n 待处理」）+ ☰ 抽屉；其下横向 tabs。
- 代码：`Shell.tsx` `.bar`；`SpacesMobile.tsx` chips 最小高度 `--touch`（44px，`spaces.module.css`）。
- 观察：会话页同时出现 chips + tabs + 会话顶栏 + 审批卡 + composer + 底栏，垂直铬叠五层，正文只剩中间一小块。

### 2.2 会话列表

- 观察：标题 + live + 筛选；搜索框；待处理组标题完整；行上 lifecycle 原文、`ins_` id、kind 芯片、批准摘要、行内 send/enter/esc。
- 代码：`SessionList.module.css` compact 媒体查询；筛选进 sheet（`sessionFilters` + `Sheet`）。

### 2.3 会话顶栏与 composer

- 观察：← 返回（标题被挤成单字母「s」）、状态「待处理」、Compact、文件、原始事件、32px 停止方块；meta 两行诊断。composer placeholder 仍是桌面快捷键（Enter 排队、⌘ 插队、Esc 打断）。芯片换行后占约半屏。
- 代码：手机 `stopBtn` 32×32（`session.module.css` `@media max-width: 767px`）；`.meta { font-size: 10.5px }`，低于 tokens「重要状态不低于 12px」（`tokens.css` `--text-label: 11px` 注释）。
- 代码：`.back` 宽 20px（`session.module.css`），小于 `--touch`。
- 代码：文件入口已从 `.deskOnly` 解出（`SessionPage.tsx` 注释）。
- 代码：`window.confirm` 用于插队/打断（`Composer.tsx`），Safari 上会盖住键盘。

### 2.4 结构 vs 终端

- 观察：本次 mock 为 structured-only，手机会话无「终端/结构」开关，与桌面一致。
- 代码：有 tty 时 `ViewSwitch` 在顶栏，seg 高 25–30px（`session.module.css` `.viewSeg` / mobile 30px），仍小于 44px。
- 仓库证据：`session-chrome-1-switch-structured-400.png`、`terminal-1-keybar.png`（含 iOS「加到主屏幕」条）。

### 2.5 审批

- 观察：标题「审批中心」在 390px 折成「审批中 / 心」；分段「全部/审批/提问/计划/表单」挤在顶栏；卡片按钮允许/拒绝/打开会话够大；AskUserQuestion 可用。
- 代码：`ApprovalsPage.module.css` compact 顶栏高度 `--top-mobile`。

### 2.6 新建

- 观察：底部 sheet，主字段大触控；「开始」通栏。权限/effort 需下滚（本次首屏未露出滑杆）。
- 代码：`NewSessionPage` `variant=sheet`。

### 2.7 登录 / 配对 / PWA

- 代码：手机默认 pair tab（`LoginPage.tsx` `mobile ? "pair"`）；Passkey 优先，访问码可展开。
- 代码：`InstallBar.tsx` iOS 文案「分享 → 加到主屏幕」；`manifest.webmanifest` `display: standalone`，`start_url=/sessions`。
- 本次 `/login` 在已 mock 登录时被 `AuthGate` 重定向到会话（观察：login 截图实为列表）。登录页真机图见 `docs/design/evidence/passkey-1-login.png`。

### 2.8 设置

- 观察：顶部分组芯片横向滚动，表单控件够大；底栏仍在。

---

## 3. 两侧共有的结构问题

1. **诊断字段上第一屏。** 列表行和会话 meta 把 wire 三维状态、instance id、seq 直接给人看。桌面有空间消化；手机没有。
2. **会话页铬预算。** 桌面：轨 + Spaces + tabs + 双行 header + dock。手机再加底栏。结构对话不是视觉中心。
3. **Composer 把能力表摊开。** 桌面能横排；手机折成控制板，输入框变小。
4. **结构/终端切换不够像主控件。** print 会话没有开关（正确）；PTY 开关挤在 25–30px 顶栏。参考客户端里，从结构滑进原始流且难返回——Remuda 已有独立路由，应比那更硬，而不是把开关做小。
5. **工具卡偏报告。** `ToolCard` Bash 默认展示命令块；手机更需要一行摘要。
6. **列表行内遥控。** send/enter/esc/stop 让「待处理」不像待办，像每行一个终端。

---

## 4. 指向 Moshi 对照

匿名「参考客户端」已由下面 **§7 Moshi 手机**、**§8 Moshi Desktop**、**§9 映射到 Remuda** 取代。机制可学，品牌/色板/整页布局不临摹。

---

## 5. 优先建议（不在本目标实施）

### P0 — 手机会话先能读、能批、能说

1. **减铬。** `/s/:id` compact：收起 space chips（空间名并进顶栏一芯片）；顶栏只留 返回 / 标题 / 一个状态 / ⋯。Compact、文件、原始事件、driver/seq/cost/native 进 ⋯。停止按钮 ≥44px。
2. **修字号与命中。** `.meta` 10.5px → ≥12px；`.back` 20px → `--touch`；`viewSeg` / `stopBtn` 对齐 44px 命中（可视可以略小，hit slop 必须够）。
3. **Composer 收成一条。** 手机：textarea + 发送；附件/权限/effort/用量一个「选项」sheet。placeholder 不要桌面快捷键。`window.confirm` 换成 `Sheet`。
4. **审批标题不折字。** 「审批中心」单行或改短名「审批」。

### P0 — 桌面会话把诊断降级

5. **Meta 第二行默认折叠。** 默认：空间/标题 + 状态点 + 结构/终端 + 停止。诊断用 disclosure「运行详情」。
6. **列表行去掉 wire 串。** 主行：状态点、标题、一句下一步（等你批准 X / 运行中 / 空闲）。`ready · waiting-interaction · connected | ins_` 放到展开或 tooltip。行内 send/keys 收到 overflow 菜单，待处理组除外可保留「去处理」。

### P1 — 结构 vs 终端

7. 有结构化信号时手机/桌面默认结构（`SessionPage` 已倾向此，当成产品规则写进 UI 文案）。
8. `ViewSwitch` 做成 44px 分段，不要混进 Compact/文件同一行。记住每实例视图（`viewPref.ts` 已有）。
9. Transcript 不渲染 hook/raw 行；原始事件只在 `/events`。

### P1 — 工具与对话形态

10. 手机 ToolCard 默认一行：工具名 + 关键参数（`rm -rf …`、文件路径）；展开再看 stdout/diff。
11. 用户消息右对齐气泡可保留（观察已有 You 气泡）；助手保持块，不要和工具卡抢同样的满宽卡片边框。

### P1 — 列表与主页扫视

12. 待处理卡片把批准摘要当正文（已有「等你批准 Bash」）——去掉其上的 lifecycle 原文。
13. 长按列表行：停止 / 关闭标签 / 复制 id（tabs 已有长按，列表没有）。

### P2 — PWA 与登录

14. 配对码/Passkey 路径保持；已登录访问 `/login` 重定向合理，但独立 PWA 首次安装要能看到 pair（代码已默认 pair）。
15. 推送深链已指向 `/approvals?focus=` 与 `/s/:id`（资料 `ui-spec`）——验证真机通知，本报告未测。
16. 安装条不要在 standalone 里反复出现（`InstallBar` 已区分 update vs install）。

### P2 — 桌面效率

17. QuickFind 已存在，保持不搜 transcript 正文的边界；在更多页（主机/设置）也要能 ⌘K。
18. 航轨图标换成与内容区一致的 lucide，避免 ▤◆me。

### 明确不做（本报告与后续实施默认）

- 不在本文件对应的工作里改 CSS/组件（本目标是分析）。
- 不新做原生 iOS/Android。
- 不改协议/wire。
- 不把 Spaces 做成跨设备同步成员系统。
- 不临摹第三方品牌、色板或整页结构。

---

## 6. 建议落地顺序

手机会话减铬与 composer → 列表去 wire 串 → ViewSwitch 尺寸与默认结构 → ToolCard 折叠 → PWA 通知真机。桌面 meta 折叠可与手机顶栏同一数据结构。

对应主要文件（实施时，非本次）：`Shell.tsx`、`SessionPage.tsx`、`Composer.tsx`、`SessionList.tsx`、`session.module.css`、`ApprovalsPage.module.css`、`ToolCard.tsx`。

---

## 7. Moshi 手机端

Moshi 定位（资料）：「在你已经控制的机器上，给长跑 coding agent / shell / tmux 用的移动终端。」来源：[getmoshi.app/docs](https://getmoshi.app/docs)。它不是第二套 agent 循环。

### 7.1 Home：连接 + 活会话

- 观察：顶栏状态环、地球、设置。「会话」区两张卡：一张「暂无预览」，一张带终端缩略图（Mosh 徽章、host:workspace）。「连接」列出本机与远程 SSH。底「发现 MOSHI」、快捷入口 tmux / Herdr / 跳转到、FAB ＋。长按提示在会话标题行。
- 推断：Home 回答「连着谁、现在哪场还活着」，预览缩略图比 id 更可扫。

### 7.2 Chat View（定义性机制，不可省略）

资料：[Chat View](https://getmoshi.app/docs/chat-view)

> Chat View is a visual layer over the coding agent already running in your Moshi terminal. … Closing Chat View only removes the visual layer. The agent, shell, multiplexer session, working directory, and scrollback keep running underneath it.

要点（资料）：

- **同一场 live terminal**，不是 ACP/API 另开的 agent。
- 网关读主机本地 transcript，规范化成消息、工具卡、计划、问题；prompt 和批准写回同一 TUI。
- 需要 agent 跑在 tmux/Herdr 里 + `moshi-hook` + Enable Chat。
- 工具卡可点开展开；Working 指示器；composer 打字/语音/图；批准条 Approve/Deny。

观察（用户切到结构化之后）：

- 顶栏：会话短名、`hybrid-harness · grok-4.6 · main`、两个状态点。
- 用户绿气泡靠右；助手灰块；`Thinking:` 截断旁白。
- 工具行：`Shell python3`、`Read …tsx +1`，右侧 chevron。
- 截图嵌在消息里。
- 底栏「对 Moshi 说…」＋ / 附件 / 麦克风。
- 顶栏和 composer 会盖住正文（padding 不足）。

这与 **hook/PTY 原始流** 不是同一面。原始流见 §7.3。Chat View 才是产品说的「终端上的对话层」。

### 7.3 Live terminal（含 hook 流陷阱）

- 观察：未切 Chat View、或从对话滑进终端后：Grok TUI 原文、`pre_tool_use hook (global/orca-status) fai` 折行、底栏 Ctrl-L / Esc / Tab。顶栏 `tab 5 · 5/6`、`3 done · 2 working · 11 idle`、`switch`。
- 资料：Chat View 打不开或不完整时，官方要求回终端看权威输出（[Chat View troubleshooting](https://getmoshi.app/docs/chat-view)）。
- 推断：把 hook 日志当「结构对话」是误用。Remuda 的 `/events` 才对应这种流。

### 7.4 Composer

- 观察：Chat View 单行输入 + 发送语义图标。终端面没有这句话，只有辅助键。
- 资料：composer 把 prompt 送进 live terminal，不是 Moshi 云 API；硬件键盘 ⌘Enter；下滑收键盘。
- 观察（设置 → 聊天模式）：「聊天模式」= 在文本框撰写而不是直打终端（开）；「以聊天形式显示」= 把智能体会话显示为聊天（开，实验）；「以聊天视图打开」= 检测到 agent 时直接以聊天打开（关）。页脚写明：聊天视图用主机上的 TUI，消息走加密 SSH，不经过 Moshi 服务器。这就是 Chat View 的产品开关，不是第二套 agent。

### 7.5 会话内导航：Jump To / switch / 多路复用图

- 观察：Jump To sheet「搜索工作区、标签页、窗格」；按 Herdr 工作区分组，当前行高亮，右侧透出终端。状态点绿/橙。
- 资料：[Jump to…](https://getmoshi.app/docs/jump-to) 是 multiplexer 地图（tmux window / Herdr tab），带 waiting 计数；List / Accordion / Grid。
- 观察：终端顶栏 `switch` 打开的是 **「服务器列表」**（PID、端口、Kill）——这是 browser-preview 探测到的 HTTP 端口，不是 Jump To。点错入口会迷路。
- 资料：[Browser preview](https://getmoshi.app/docs/browser-preview) 用 indigo 点开本机 HTTP 服务。

### 7.6 Inbox / 审批

- 资料：[Agents & Usages](https://getmoshi.app/docs/agents-usages) Needs you / Working / Done；批准可在行内答。
- 观察（设置 → 通知）：推送默认关；实时活动、点按打开位置、保持屏幕常亮。横幅要求先开推送才能用收件箱/实时活动。
- **未观察** 真实 Inbox 看板。Home「发现 MOSHI」打开的是 **「探索 Moshi」功能说明目录**；其中「收件箱与用量」是带示例数据的演示页（Mac Studio 用量环、`Claude needs input` / `rm -rf node_modules` 的 Approve 示意），不是当前主机上的待办队列。不得把演示页写成 live 观察。

### 7.7 设置（真实页）与探索目录（说明页）

- 观察：设置分组 订阅 / 终端 / 输入 / 集成。返回是标题行左侧圆钮，不在状态栏。
- 观察（工具栏）：终端底栏实时预览（Ctrl Esc Tab + 图标）；可改按钮列表。对应会话里的辅助键条。
- 观察（Agent Hook）：本机 brew 安装/pair 文案、设备令牌、HOOK 状态。令牌不当作报告内容。
- 观察（在主页显示）：标题栏图标 Agent（开）/ 文件（关）/ Web 服务器（开）/ 模拟器（开）。
- 观察（探索 Moshi）：进阶功能的 **说明目录**，不是 Jump To / Diff / Inbox 本体。点卡片会进预览/文档，不是那条活会话。

---

## 8. Moshi Desktop

资料：[Install Moshi Desktop](https://getmoshi.app/docs/install-desktop)

- **Server**：`moshi-hook serve`，gateway 仅 `127.0.0.1:24543`。
- **Client**：`moshi` 在 `http://127.0.0.1:24544` 开浏览器 UI，经本机或 SSH 打到 daemon。
- 远程只跑 hook + multiplexer，不跑 Web。

本次：`:24544` 未监听。**观察** Tauri「Moshi Desktop」+ 同一套 UI 在 `:57482`。`GET 127.0.0.1:24543/v1/workspaces` 返回 `kind: herdr` 与工作区树。

### 8.1 主机切换

- 观察：未连接时 Machines 卡「Mifune-Workstation (local)」+ SSH 目标框 + Connect over SSH（key-only）。
- 观察：已连接顶栏 `Mifune-Workstation · herdr 2`；底栏 host 下拉。与手机「连接」同一工作流，不是另一套 IDE。

### 8.2 侧栏 WORKSPACES

- 观察：按 git 项目分组（astergate / GravityDB / hybrid-harness…），行是 agent 会话标题；终端-only 行是数字或 `remuda-dev`。⌘K 搜工作区。`view=chat` vs `view=term` 写在 URL。
- 资料：Chat View 与终端是同一 session 的两个 view。

### 8.3 Chat View（桌面）

- 观察：顶栏 `Term | Chat` 分段 + Web / Files / Diff。标题下模型、context 环（91%）、cwd。正文：折叠 Shell/Bash/Monitor 卡、Thinking 一行、Markdown recap、composer「Chat via Moshi… (⌘I)」、＋ 与语音。右侧 DEV SERVERS（Kill）与 TUNNELED。
- 资料：桌面与手机共用 hook 网关，不是第二套 Electron agent。

### 8.4 Terminal

- 观察：`view=term` 是 Herdr/Grok TUI 全屏（spaces 列 + 彩色 tab + 日志）。Chat 分段仍在。同一 cwd、同一 attached。
- 观察：未开 Chat 时默认就是这层终端（desktop-connected.png 中间是 Grok TUI）。

### 8.5 Diff / Files / Browser preview

- 观察：顶栏 **Web / Files / Diff** 按钮与右侧 DEV SERVERS 列表（端口、Kill、container）。点 **Diff** 后右侧出现 `UNTRACKED` 文件列表和 `HISTORY / Working tree`。未打开单个文件的 side-by-side hunk。
- 资料：[Diff viewer](https://getmoshi.app/docs/diff-viewer) 由 hook 读 git working tree，不上传。Browser preview 转发 host HTTP。

---

## 9. 映射到当前 Remuda（SOTA 思路，非临摹）

| Moshi 机制 | Remuda 现状 | 建议学什么 |
|---|---|---|
| Chat View = **同一 PTY 上的视觉层**，关层终端还在 | 代码：`ViewSwitch` 在「有终端」时切 `/s/:id/tty` vs `structured`（`ViewSwitch.tsx`）。`live-structured-view.md` 要求结构反映终端、权威阶梯 Hook>File>OSC>Screen | 把「结构」宣传成 **同一实例的投影**，不要做成第二会话。开关要像 Moshi 的 Term\|Chat 一样大、难滑丢 |
| 工具默认一行卡，点开再展开 | 代码：`ToolCard.tsx` `BashCard` 默认展示 `$ command` 和 stdout details | 手机默认一行摘要 |
| Composer 一句 + 图标 | 代码/观察：`Composer.tsx` 芯片墙 + `window.confirm` | 手机只留发送；档位进 sheet |
| Jump To = multiplexer 地图 | 代码：Spaces 面板 + tabs + QuickFind 搜元数据，不搜 pane | 桌面已有空间树；缺「当前主机上所有 pane/agent」一张图。不要用 QuickFind 冒充 |
| Home 活会话缩略图 | 观察：Remuda 列表是 wire 字段 + 行内 send/keys | 待处理用「等你批准」当正文 |
| Inbox Needs you / Working / Done | 代码：`ApprovalsPage` + 列表分组待处理/进行中 | 审批中心已经接近；不要做成 hook 日志 |
| Desktop = 同一 Chat+Term+gateway | 代码：Remuda 桌面是 Hub PWA，权威在 journal 投影，不是 SSH 附到用户 TUI | Remuda **不应**改成 Moshi 那种「只 overlay 本机 TUI」——它要遥控多 host 进程。可学的是 **双视图同一 instanceId** |
| DEV SERVERS / Diff 在会话旁 | 代码：文件路由 `/s/:id/files`；无端口探测旁栏 | 有 artifact 再做；不要先做 Kill 端口面板 |

**不要抄**

- hook/PTY 原文当 Chat View（观察：phone-terminal-hooks.png）。
- 顶栏/composer 盖住 transcript（观察：phone-chat-view.png）。
- 把「switch」做成服务器 PID 列表却不标明是 preview（观察：phone-switch-server-list.png）。
- 绿色品牌、Space Grotesk、FAB、地球图标。
- 把 Remuda 改成「必须 tmux 才能对话」——Remuda 的 journal 路径正好是 Moshi 做不到的：print/SDK 无 TTY 也能结构流。

SOTA 一句话：**结构视图是 live session 的投影，不是第二个 agent；终端始终可一键回去且不 fork。** Remuda 已有 D-028 阶梯和独立 tty 路由，差的是把这个故事做成人能摸到的主控件，而不是诊断顶栏。

---

## 10. 补强后的优先建议（仍不实施）

在 §5 之上，从 Moshi 对照新增：

19. **Term \| 结构 做成主分段**（桌面顶栏右侧、手机顶栏中间），文案标明「同一会话」。有 TTY 才出现（已有）。  
20. **Chat View 式工具行**：默认 `Bash · rm -rf …` 一行；展开才是现在的 `ToolCard`。  
21. **不要把 `/events` 当默认结构页。**  
22. **会话旁栏（桌面）**：Diff/Files 已经有入口；端口探测不是 P0。  
23. **Jump To 不做第二套空间模型**；把 QuickFind 结果按 host+workspace 分组，带 blocked 计数即可。

§5 的 P0（减铬、composer、列表去 wire）仍然最先做。

---

## 11. 二次实机与网关实测（2026-09-19 深夜）

方法：iPhone 镜像（Codex Computer Use，截图 + 坐标点击）操作 Moshi 手机端；Moshi Desktop 的 Tauri 内嵌 UI（`127.0.0.1:57482`）用 Playwright 驱动；gateway（`127.0.0.1:24543`）只读探测；`web/src` 断言由 34 个 subagent 逐行核对（含对抗复核）。全程只读：没有发 prompt、没有答审批、没有点 Kill/Stop、没有改 Moshi 设置、没有开推送。

### 11.1 推翻 §7.2：规范化在客户端，不在网关

§7.2 写「网关读主机本地 transcript，**规范化**成消息、工具卡、计划、问题」。实测不是。

`/v1/transcripts?session=<id>` 是 **WebSocket**（HTTP GET 返回 `426 Upgrade Required`），帧形如：

```
{ source: "claude", type: "backlog", cursor, entries: [{ line, raw }], startLine, totalLines, hasMore }
```

`raw` 是 **agent 自己 journal 文件里的原始整行**，未做任何归一：claude 会话的 `raw` 就是 Claude Code JSONL 记录（`parentUuid` / `wireToolInputs` / 完整 `usage` / `requestId` / `effort` 全在）；同一端点取 codex 会话，`raw` 变成完全不同的 `{ ordinal, payload, timestamp, type }`。`cursor` base64 解出来是 `{ v, source, session, backend: "file", line, offset, digest }`。

结论：**网关做的是带完整性校验（digest）的文件 tail + 会话发现；把异构 journal 翻译成消息/工具卡/计划是客户端的事，`source` 字段决定用哪个解析器。** 读路径根本不经过 tmux/herdr —— multiplexer 只用于**写**（`terminal.prompt` / `terminal.keys`）和 pane 发现。

这直接影响 §9 的结论。§9 说「Remuda **不应**改成 Moshi 那种『只 overlay 本机 TUI』」—— 前提不成立：Moshi 读的也是 journal 投影，和 Remuda 是同一个架构，不是屏幕抓取。两者真正的差别只在**权威源**：Moshi tail 的是 agent CLI 自己的 journal 文件（所以它天然只支持它认识的那几种 agent，`source` 是封闭枚举），Remuda 的权威在 Hub journal（协议自己的事件，`docs/design/protocol.md`）。**「结构视图是投影而非第二个 agent」这条 SOTA 判断是对的，而且比 §9 写的更站得住 —— 它不是 Remuda 要向 Moshi 学的东西，是两者独立得出的同一个结论。**

gateway 协议全貌（`/v1/version`）：`version 0.3.26`、`protocolVersion 1`、`hostname`，capabilities 只有 8 条：

```
events.watch.workspaces, events.watch.agent-status, events.license,
transcripts.limit, terminal.prompt, terminal.keys,
workspaces.live-session, approvals.answer
```

**写入面只有三个**：`terminal.prompt`、`terminal.keys`、`approvals.answer`。Chat View 的输入框能做的事不超过一个键盘 —— 这是「同一场会话的投影」在协议层的证据，不只是 UI 说法。

### 11.2 Jump To：入口写错了，数据模型可以直接抄

§7.5 把 Jump To 记在「从 Chat View 左缘滑开」，并说终端顶栏 `switch` 打开「服务器列表」。都不对：

- **Jump To 的入口是终端键盘条的 `↷` 图标。** 键盘条实际是 `Ctrl / Esc / Tab / ⁘ / ↷ / 剪贴板 / 历史 / ✳ / 键盘` 九个键。其中 **`⁘` 是 git-DAG 字形，点开是 git 面板**（§11.4），**`✳` 才是 Term↔Chat 切换**。官方演示页写明 Jump To「可从终端工具栏打开，也可绑定到手势：双击、长按键盘按钮或在工具栏上滑动」。
- **`switch` 打开的是 herdr 自己的 TUI 切换器**，渲染在 pane 内：`agents` 段逐行 `Local · <项目>` + `idle/working/done` + agent 名，`spaces` 段带 `+ new workspace`。它是 **herdr 的界面，不是 Moshi 的**，更不是 browser-preview 的端口列表。§7.5 对 `switch` 的判断应删除。

Jump To 界面 = `/v1/workspaces` 那棵树的直接渲染。该端点给出的完整数据模型（本机实测 6 group / 21 tab / 21 pane）：

| 层 | 字段 |
|---|---|
| 顶 | `kind: "herdr"`、`capabilities: {paneList, paneFocus: "exact"}` |
| group（= git 项目） | `id, label, focused, agentStatus, git: {branch, dirty?}` |
| tab | `id, label, focused, agent, agentStatus, model, contextRemaining, cwd, sessionId, title, stateChangeOrder, paneCount, agentPaneCount` |
| pane | 同 tab（去掉 `paneCount`） |

- `agentStatus ∈ {idle, working, done, unknown}`，**逐层上卷**：一个 group 里有 done 就显示 done，否则 working，否则 idle（实测 `hybrid-harness` 组内 1 done + 1 working + 4 idle → 组显示 **done**）。纯终端 pane 没有 `agent`，字段直接缺失而不是 `null` 占位。
- `title` 是**从最近一条用户消息派生的**，不是稳定会话名（本会话顶栏一度显示 "I recovered"）。
- `stateChangeOrder` 是单调计数器，Jump To 的「时钟」排序就是它。

UI 侧：搜索框「搜索工作区、标签页、窗格」+ **时钟/列表切换**（时钟 = 按 `stateChangeOrder` 排最近；列表 = 按名分组）。分组头 `项目名 + git branch`，叶子是会话标题，当前行高亮。

**关键边界（对 §9 表格那一行的修正）**：搜 `recov` 命中标题并显示 `hybrid-harness / 5` 面包屑；搜 `keybar`（这条会话正文里反复出现的词）返回 **无匹配项**。**Jump To 只搜标题/标签/工作区名，不搜 transcript 正文** —— 和 Remuda QuickFind 已有的边界完全一致。所以 §9「缺『当前主机上所有 pane/agent』一张图。不要用 QuickFind 冒充」这句要改：搜索**范围**不是差距，差距只有两条 —— (a) 结果按 host+workspace **分组**并带 git branch，(b) 能下钻到 **pane** 层级。§10 第 23 条（「把 QuickFind 结果按 host+workspace 分组，带 blocked 计数即可」）因此是对的，而且比 §9 的措辞更准。

### 11.3 Inbox：实测了，只有两档，不是三档

§7.6 说未观察到真实 Inbox。已实测（Home 左上状态环进入）。资料里的 **Needs you / Working / Done 三档并不存在**，实际只有 **进行中 / 完成** 两组。每行：

- **环形 context 剩余百分比**（实测 1 / 55 / 96 / 78）+ 彩色菱形状态点；
- 标题 = 最近一条消息（`You: continue`、`DONE 7d054b9a`、`Session started`）；
- 副标题 = 最近事件原文，**错误直接上一等位置**：`API Error: Request rejected (429) · Rate li…`；
- 再跟 workspace 芯片（`<workspace>` / `bytedance` / `hybrid-harness`）· agent（`Claude Code`）· 相对时间。

顶部常驻横幅「需要开启推送通知 —— 收件箱、用量和实时活动都通过 Agent 推送送达」。底部是**按 provider 的配额条**：Claude 绿 ~55%、OpenAI 琥珀 ~85%、第三家空。

对 Remuda 的意义：
1. **「等你批准」不是靠分组表达的，是靠排序 + 副标题原文。** Remuda `ApprovalsPage` 的 kind 分段已经比这个强，不用退回去。
2. **context 剩余做成行内环**，比 Remuda 列表里的 `$cost`/`seq` 有用得多 —— 它回答「这场还能不能继续」。Remuda 已有 usage 数据（composer 里的「用量 6%」），缺的是把它放进**列表行**。
3. **错误文案当正文**。Remuda 列表行现在是 `ready · waiting-interaction · connected`；Moshi 同一位置放的是 `API Error: Request rejected (429)`。这正是 §5-P0-6「一句下一步」的现成例证。
4. 跨 provider 配额条 Remuda 没有对应物，也不该急做 —— Remuda 的 provider 是 Hub 侧配置，不是登录账号。

### 11.4 手机端也有完整 git 面板（§8.5 只记了桌面）

键盘条 `⁘` 打开全屏 sheet，**5 个 tab**：改动 / 历史 / 分支 / PR / 工作树。

- 表头一行扫视串 `main · 6 untracked · 6 files · +809` —— 没有 wire 字段。
- **改动**：`UNTRACKED 6` 分组，每行文件路径（目录灰、文件名白）+ `+455` 增量；行右 `↺`（丢弃）/ `+`（暂存）。`List | Diff` 分段切到统一 diff。
- **历史**：带分叉泳道的提交图，`Uncommitted changes` **钉在第一行**，ref 芯片（`main` / `origin/main`）+ 相对时间。
- **分支**：`LOCAL 7` / `REMOTE 59`，本地行内显示 upstream tracking。
- **PR**：空态直接写仓库绝对路径。
- **工作树**：按文件类型着色图标的目录树。

Diff 正文是**定宽不折行、右侧硬裁**的行号视图 —— 手机上读长行要横滚，这一点不要抄。

对应 Remuda：`/s/:id/files` 已有路由（`files-view-contract.md`）。**可抄的是「表头一行扫视串」和「Working tree 钉在历史第一行」**；不要抄的是硬裁的 diff 正文，以及把 `↺ 丢弃` 放在列表行右侧（手机误触代价太大）。§5-P2-22「端口探测不是 P0」仍然成立。

### 11.5 Desktop：`view` 在 URL 里，Web/Files/Diff 不在

Playwright 实测（`127.0.0.1:57482`，官方 `:24544` 仍未监听，与 §8 一致）：

- 未连接时是 Machines 页：`Mifune-Workstation (local) v0.3.26` + `Connect over SSH`（文案强调必须免密钥入）+ 「No daemon? Run `moshi serve`」。
- 连上后 URL 是 **完全自描述的**：`/session/w5%3At2R?view=chat&source=claude&session=<uuid>&node=w5%3At2R`。即 **view + source + session + node 四元组**，投影是**可寻址的，不是一个模式开关**。
- 但 **`Web` / `Files` / `Diff` 三个按钮不改 URL** —— 它们是右侧栏面板，不是路由。`Diff` 按钮标签上带 `*` 表示工作树脏。
- 单改 URL 的 `?view=chat` **不会**切视图（仍渲染 Term）；必须先在侧栏选中那条会话（`session/main` 不是有效 pane）。所以 §8.2「`view=chat` vs `view=term` 写在 URL」准确，但要补一句：URL 是**结果**，不是入口。
- Desktop 有**浅色主题**（§8 全部证据是深色）。侧栏叶子行右侧带状态点/spinner。
- 终端视图里 pane 内部还有 herdr 自己的 `spaces` / `agents` 两列 —— 即 §11.2 的 `switch` 界面常驻。
- Chat 视图：顶栏 `Term | Chat` 分段 + 标题 `I recovered` + `opus-5` + **context 条 76%** + cwd；正文**工具行就是一行** `>_ Bash  cat > /tmp/…` + 右侧 `✓` 或 `↻ running`，图片消息内联缩略图；composer 「Chat via Moshi… (⌘I)」+ `opus-5 · xhigh`。
- 右栏 `DEV SERVERS 6 running`（端口 + 项目名 + `Kill`，容器标 `container`）与 `TUNNELED 9 active`（`Stop`）。

**注意 D-031 边界**：Moshi Desktop 右栏在本机列出 9 条活跃 ssh tunnel 并提供 `Stop`。Remuda 的 D-031 明确禁止 tunnel 工具。**这块整体不抄**，不只是「不先做 Kill 端口面板」的优先级问题。

### 11.6 `web/src` 断言核对：数字全部站得住，措辞有两处要修

85 条代码断言逐行核对（含对抗复核）。**所有硬数字都确认**：`.meta` 10.5px（`session.module.css:2390`，仅在 `max-width:767px` 内）、`.back` width 20px（`:29`）、`stopBtn` 32×32（`:2399-2401`）、`.viewSeg` 25px / 手机 30px（`:96` / `:2450`）、`--touch: 44px`（`tokens.css:68`，全库唯一定义）。§2.7 的推送深链指向 `/approvals?focus=` 与 `/s/:id` 也确认（`ui-spec.md:91`）。

两处要修：

1. **§2.3 / §5-P0-2 的 `.meta` 应写「桌面 11px / 手机 10.5px」。** 基础 `.meta` 是 11px（`session.module.css:153-159`），10.5px 只在手机媒体查询里。只改手机那一条会留下桌面仍低于 12px，而且会让**手机字号大于桌面**同一元素。修时用 `var(--text-aux)`（12px）而不是字面量。
2. **§2.3 把 12px 下限归给 `--text-label` 的注释不准确。** 那句「Important status never relies on sub-12 px text」是整个 type scale 的**块头注释**（`tokens.css:52-53`），在 `--text-label` 上方四行。更要紧的是：**`.meta` 根本没引用任何 token**，`session.module.css` 全文一次都没用 `var(--touch)` —— 20/25/27/30/32px 全是硬编码。这才是这些数字跑偏的**原因**，值得写进报告而不只是记录现象。

### 11.7 P0/P1 建议的规格冲突（实施前必须先改 spec）

对 §5/§10 的 8 条建议做了可行性核对。**3 条与 `ui-spec.md` 直接冲突，不能照抄实施**：

| 建议 | 结论 | 冲突点 |
|---|---|---|
| **P0-1** 手机顶栏减铬 | **CONFLICTS** | `ui-spec.md:156` 明确要求「约 400px 手机上显示可横向滚动的 space chips 与 tabs」，且 §1.4 自称「本节只作用于会话工作台」，`/s/:id` 正在其内 —— 收起 chips 与规格直接冲突。另外 `ui-spec.md:113` 把 `terminal|structured` 切换列为顶栏必有项，若挪进 ⋯ 即违规。**本建议还和自己矛盾**：P1-8 与 §10-19 要求该开关**更**显眼。要实施必须先改 `ui-spec.md` §1.4/§1.3。 |
| **P1-10 / P1-20** ToolCard 默认一行 | **CONFLICTS** | `ui-spec.md:290` 与 `:307`：`workflow.run` **运行中与结束后都展开，只有读者 dismiss 才折叠**；`:292` `error` 卡**必须展开**。代码里 `ToolCard.tsx:260` 的 folded 分支在 Workflow 分支（`:281-292`）**之前** return，一律默认折叠会让 `WorkflowTimelineCard` 永不挂载、1Hz 走针不启动。另 `ui-spec.md:303` 要求 Bash「命令一行」—— 折成裸「Bash」违规；折行的 key argument **必须**是命令文本（`toolPresenters.ts:199` 已能给出）。 |
| **P0-5** 桌面 meta 默认折叠 | **CONFLICTS** | `ui-spec.md:229-236` 把两行 header 画成**规范 wireframe**，第 233 行就是 `seq 184 · connectivity=connected · $0.12`；`:113` 要求主机芯片在顶栏（现在住在 meta 里）；`:331` 要求 cost 在顶栏。要折叠必须同时改 §2.2 的 ASCII 图，否则代码与规格自相矛盾。 |
| **P0-2** 字号与命中 | **NEEDS_DESIGN** | 方向对，但 `.back 20px → --touch` 的**字面写法违反** `ui-spec.md:342`「手机上触控 ≥ 44px 只靠**热区**，不靠视觉尺寸」。正确做法是 20px 字形 + 44×44 `::after`（同文件 `:2434-2443` 的 `.effortIconBtn::after` 已是先例）。报告 §5-P0-2 自己也写了「可视可以略小，hit slop 必须够」—— 标题和正文不一致，实施单必须取正文那句。 |
| **P0-3** 手机 composer 收成一条 | **CONFLICTS** | `ui-spec.md:339-340` 要求 effort 保持可见的收起触发器并显示**档名**；`:337` 要求权限芯片**显示**当前 `permissionMode`（`permissions.ts` 把「绕过全部/不再询问/完全访问」标为 `danger`，把 bypass 态藏进 sheet 是这里最响的冲突）；`decisions.md:57`（D-028a）要求 composer 呈现发送/排队/打断三态 + 队列 chip + 未验证能力标「尚未验证」。若做 options sheet，**触发器必须带 mode 词**，三态与诚实标注不得入 sheet。 |
| **P0-6** 列表行去 wire 串 | **READY（规格反而要求）** | `ui-spec.md:174-183` 的 wireframe 本来就是「点 + 标题 + 一句」，没有 `ready · waiting-interaction · connected`，也没有 `ins_`；`:164` 明写状态点是三维投影、不是单独 wire 枚举。**当前代码才是偏离。** 缺的是 `nextStep()` —— 全库没有这个派生（`lib/status.ts` 只有 `UI_STATUS_LABEL` / `projectStatus`），建议新建 `features/session/nextStep.ts` + 测试。 |
| **P1-8** ViewSwitch 44px 分段 | **READY（大半已做）** | `ViewSwitch.tsx:39-69` 已是 `role=radiogroup` + `aria-checked` + roving tabindex + 方向键，不是两个按钮。只差尺寸，且要按 `ui-spec.md:342` 用热区而非视觉尺寸。注意 `ApprovalsPage.tsx:92-95` 另有一套非 radiogroup 的手写分段 —— 若抽公共 `Segmented` 会把审批筛选的 a11y 一起拖进来，规模从 M 变 L。 |

**给实施的一句话**：§5 的 P0 里，**只有 P0-6 可以直接开工**（规格站在它这边）；P0-1 / P0-5 / P1-10 / P1-20 **必须先提 `ui-spec.md` 修订**，P0-2 / P0-3 要按规格改写成热区方案和「触发器带 mode 词」方案。§6 的落地顺序建议据此调整为：**P0-6（列表去 wire 串）→ P0-2（热区版）→ P1-8 → spec 修订 → P0-1/P0-5 → ToolCard 折叠（带 Workflow/error 豁免）**。

### 11.8 本次未观察 / 不做

- **推送通知未开**（Inbox 顶部横幅仍在）。实时活动、通知深链、Live Activity 均未验证 —— 那要改系统通知设置，不在只读范围内。
- **未发 prompt、未答审批、未点 Kill/Stop**。`terminal.prompt` / `terminal.keys` / `approvals.answer` 三个写入端点只做了能力枚举，没有调用。
- **未订阅 `events.watch.*`**：`/v1/transcripts` 只拿到 `backlog` 帧，未拿到增量 append 帧（没找到 cursor resume 的正确参数），所以「实时推送延迟」无数据。
- 工具限制：Codex Computer Use 的坐标输入中途整体失效（截图与元素点击仍可用，坐标 click/scroll 全部 `noWindowsAvailable`），重启 Codex 后恢复。排查期间镜像窗口从副屏 `-380,-68` 移到了主屏。

### 11.9 证据

本次截图未入库（`docs/design/evidence/` 下无 `moshi-*`）。若要留档，建议按既有命名放 `evidence/moshi-2-*.png`：手机 `jump-to`、`inbox`、`git-changes`、`git-history`、`chat-view`、`herdr-switch`；桌面 `desktop-machines`、`desktop-chat`、`desktop-diff`、`desktop-files`。gateway 原始响应（`/v1/version`、`/v1/workspaces`、transcript 帧结构）本节已摘录关键字段，不建议整份入库 —— 含真实会话标题与路径。
