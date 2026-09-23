# Web / PWA 富交互界面规格

状态：可开工规格 v0.2（2026-09-12）  
产品定位：unified remote agent runtime 的遥控面（产品名已定 **Remuda**：UI 文案、窗口标题与 manifest 均用 Remuda，D-001/D-053）。
**不是** harness，**不造** agent loop。界面只观察 + 下发控制；resume 权威是原生会话。

**v0.2.3 changelog（2026-09-20，task 优先模型，见 D-050）**：新增 §1.5「Task 层」（Task 是 instance 之上的聚合而非第二状态机；看板列只读投影；目录绑定 reuse\|pool 与 attach-lock；任务空间=文件视图过滤投影；批注=composer 草稿；工作台多 session 用 tab 不分屏；/m 任务分组）与 §2.9「任务列表与看板 `/board`」（三列+已归档过滤、failed 角标按 placement 归位、拖卡多跳、`SE-nn` 派生 key、父子嵌套、「需要你」首组、只读预览、任务/项目空间面板、批注徽标、项目切换器）；§1.1 projects 行与「项目」段改为采纳 Hub `Project` 实体（Space 键不放松，D-024）；§1.2 路由表新增 `/board`；§1.3 映射表补看板 compact 落点；§4.7 `/m` 子树补任务分组与看板单列过滤。机械细节（表结构、RPC/路由形状、迁移预算、grant 门控）以 [task-model.md](./task-model.md) 为准。

**v0.2.2 changelog（2026-09-19，手机优先路由树，见 D-049）**：§1.2 路由表新增 `/m` 与 `/m/inbox`，并写明 compact/桌面双向重定向与 query 保留（`/s/:id` 永不重定向）；§1.3 手机线框区分「首页级屏」与「会话路由」两套铬（compact 主行含状态点：返回 / space 芯片 / 标题 / 状态点 / 分段 / Stop / ⋯），会话路由 compact 不渲染 app 底部导航栏，§1.4 的 chips 行折成单芯片、tabs 行整行收起（两条 compact 例外分别挂 D-040 / D-049，列表路由不变）；compact 主行的 host 芯片与 cost 折进「运行详情」（仅 compact，D-040 的桌面主行规则不变）；§4.5 补应用角标（badge）与推送权限横幅的位置；新增 §4.7「手机优先路由树与铬预算」（一条顶栏 `--top-mobile` + 一条底栏 `--bar`、正文 ≥ 60% 视口、截断优先级、`终端|结构` 分段与 Stop 永不截断/进溢出）与 §4.8「语音输入」（平台听写优先、先成文再发送、Web Speech API 仅增强且默认关、不做云转写、iOS Safari 无 `SpeechRecognition`、终端段不提供语音）；§4.6 PWA `start_url` 从 `/sessions` 改为 `/`。依据 [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §5 / §6 / §7.1 / §9 / §10-19 / §10-23 / §11.2 / §11.3 / §11.4 / §11.5。

**v0.2.1 changelog（2026-09-19，依据 [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.7 的冲突核对，见 D-038…D-042）**：本修订只解除「规格与实施单互相矛盾」，不改变产品方向。四处增补——§1.3 顶栏把 `terminal|structured` 与 Stop 钉成永不进 ⋯ 溢出；§1.4 的 400px space chips 要求限定到列表路由，`/s/:id*` 允许折成单枚当前 space 芯片；§2.2 的 header 从「规范两行诊断」改为「主行 + 可折叠运行详情」，并新增手机工具卡折叠（含 Workflow / error / running 豁免）与 compact composer 边界；§3.4 新增全局的「命中尺寸只靠热区」与「`.meta` 类文本桌面手机同值、下限 `var(--text-aux)`」。**依据行号以本文件为准**（报告 §11.7 引的 `ui-spec.md:233` 实为修订前的 `:235`，该行现已随 §2.2 重画）。

**v0.2 changelog**（对照 `docs/research/review-consistency.md`）：DriverKind 三值（`claude-print` / `claude-bg` / `claude-pty`），UI `mode` 只是投影；手机默认 print、桌面仅在需要 `/workflows` 面板时才 pty；Artifact 不是 M0 门槛，自动切 tty 默认关且须 `capabilities.artifact`；Provider 页 M0–M2 只展示 `astergate-default`；主 UI 是结构化 transcript + 可选第二视图 tty，v1 无 herdr 分屏；herdrx 只抄交互/viewport 算法并重接 runtime API；路由钉 React Router、审批进底栏、只做深色；术语对齐 protocol（status 三维投影、Workspace、`outbound-wss`/`ssh-dev`、NativeRef）。

依据（只读）：

- `docs/design/proposal.md` §4–§5（Hub / Node Agent、双权威、技术栈）
- `docs/research/deepseek-harness.md` §2（DSH UI 逐项判断）、§8.3–§8.4（envelope / completeness / 多设备）
- `docs/research/claude-control-plane.md` §0 / §7（create / send / observe / attach(tty) / stop）
- `docs/research/bot-dispatcher.md` §3（路由命令、审批回路、session_key）
- `docs/research/reference-repos.md` §2.4–§2.7（claudecodeui / vibe-kanban / paseo）
- 前端源码：`herdrx/web/src`、`deepseek-harness/packages/client`、`claudecodeui/src`、`paseo/packages/app`、`vibe-kanban/packages/{web-core,ui}`、`vibe-kanban/docs/workspaces/interface.mdx`

证据口径：**已验证** = 读到的源码/文档；**建议** = 据此定的本产品规则。未跑真机。

---

## 0. 开工约束（先读再写代码）

1. UI **只**订阅 Hub 的 observation journal（`seq` follow）和 Instance/Interaction 列表。禁止从 PTY 文本解析 tool / Workflow / 审批 ID（DSH `TerminalBlock` 已证明不够当 TUI；`deepseek-harness.md` §2.2）。
2. 控制命令一律带 `commandId`，三态 `queued → accepted → settled`。重连只补事件，不重发可能已进原生 agent 的 prompt / approve（`proposal.md` §4.0、`deepseek-harness.md` §8.4）。
3. Claude **DriverKind 三值，互斥，不要混**（`claude-control-plane.md` §0：`-p` 与 `--bg` 互斥；`--bg` 不是交互 TUI）。UI `mode` 只是投影，见下表。M0 只暴露 print + 结构化 transcript；Workflow **工具**在 print 验收。`/workflows` 面板与 Artifact 页是 M3 的 `claude-pty` / `claude-bg` attach。结构化 transcript 来自 hooks + jsonl，**不 scrape 屏幕**。
4. 手机是一等公民。DSH 56px 常驻 rail、hover 预览、拖拽排序只作桌面参考，不搬到窄屏（`deepseek-harness.md` §2.3）。
5. 不引入 Cordis / Typert / DSH Host boot。Vite 独立入口。路由用 **React Router**。Desktop 壳第一阶段不做，PWA 安装即可（`proposal.md` §5）。v1 不做 herdr 多 pane 分屏：PTY 是会话的**第二视图**，不是整页只剩 xterm。

**DriverKind → UI `mode` 投影**

| DriverKind | 原生形态 | UI `mode` | 终端 tab | M0 暴露 |
|---|---|---|---|---|
| `claude-print` | `claude -p --output-format stream-json` | `structured-only` | 无 | **默认**（手机唯一；桌面默认） |
| `claude-bg` | `claude --bg`（可 `attach`；忽略 `--session-id`） | `tty-attachable`（若 `capabilities.ttyAttach`） | 有 | spawn 可做，**UI 先不提供入口** |
| `claude-pty` | 交互 TUI（无 `-p`/`--bg`；自建 PTY 或可选 herdr carrier） | `tty-attachable` | 有 | M3；仅当用户明确要 `/workflows` 面板 |

`--bg` **不是** `claude-pty`。禁止在同一 Instance 上从 `-p` 热切 `--bg`。

---

## 1. 信息架构

### 1.1 顶层导航

六个一等区域。个人产品，没有「组织 / 团队」层。

| id | 中文 | 职责 | 桌面落点 | 手机落点 |
|---|---|---|---|---|
| `sessions` | 会话 | 跨主机实例列表 + 打开会话 | 侧栏主导航首项「会话」（`/sessions`，左侧索引列挂 SpacesPanel）；主列会话页 | 底栏「会话」 |
| `approvals` | 收件箱 | 全局 pending Interaction | 侧栏「收件箱」（带计数）+ `/approvals` | 底栏「收件箱」（有待办时计数；compact 下即 `/m/inbox`，D-049） |
| `hosts` | 主机 | Node Agent 在线、CLI、登录态 | 侧栏底部「管理」菜单 → `/hosts` | 底栏「更多」→ 主机 |
| `projects` | 项目 | Hub `Project` 实体（任务看板、任务列表、项目切换器；`members[]` 引用 Space 键，D-024/D-050） | 侧栏「项目」区（每行就是范围控件：点项目行 → `/board?project={id}`，点「全局」→ `/board`）；另有 `/projects` 与其页头的 `ProjectSwitcher` | 新建会话步骤里选；compact 看板塌缩进 `/m` |
| `providers` | Provider | profile、健康、与 astergate 关系 | 「管理」菜单 → `/providers` | 更多 → Provider |
| `bots` | Bot | 飞书 / Telegram 绑定、白名单、会话键 | 「管理」菜单 → `/bots` | 更多 → Bot |
| `settings` | 设置 | 设备、推送、权限、外观默认 | 「管理」菜单 → `/settings`；外观分段为 `[跟随系统 \| 深色 \| 浅色]`（D-053，默认跟随系统） | 更多 → 设置 |

「项目」对应 Hub **`Project` 实体**（D-050 起 Web 采纳；`members[]` 是 `(hostId, workspaceId)` 列表，直接引用 Space 键，不合并同名/跨主机目录，D-024 不变）。任务列表与看板按 projectId 过滤；桌面只有两个项目范围入口：侧栏「项目」区的行与 `/projects` 页头的 `ProjectSwitcher`，二者写同一个偏好。已注册目录的协议实体仍是 **Workspace**（主机上的 cwd 通讯录，id=`workspaceId`），为新建会话、文件视图与过滤器提供稳定 `workspaceId`（host + path + 可选 worktree）；Project 引用 Workspace，不替代它。task 聚合层与看板投影见 §1.5 / §2.9。

桌面只有一条带文字的侧栏（248px，768–1023 为 216px，⌘B 收成 48px 图标列），不再有 48px 图标轨，也不再有会话路由上的第二列；每页一条 48px 页头（D-053，细节见 §1.3）。

会话页内部两个视图，**不是**两个顶层导航，也**不是**多 pane 工作台：

- `structured`：transcript（print 默认；pty-backed 的第二视图）
- `terminal`：xterm attach 原生 TUI（`mode=tty-attachable`：`claude-pty` / `generic-pty` / `shell-pty` / kind `terminal` / codex·grok·agy。一实例一终端，无分屏）。pty-backed 默认打开终端 tab。

### 1.2 URL 路由表

Hash 路由不要。用 **React Router**（History API）。认证 cookie 必须 `Secure`（DSH 默认 loopback 无 Secure，不能照搬，`deepseek-harness.md` §2.3）。

| 路径 | 屏 | 备注 |
|---|---|---|
| `/login` | 设备登录 | 未登录唯一公开页 |
| `/` | 按视口落点（D-049） | compact（§1.3 断点）→ `/m`；其余 → `/sessions`。PWA `start_url` 用 `/`（§4.6） |
| `/sessions` | 会话列表 | query：`?host=&workspace=&kind=&status=`（`status` 是 §2.1 投影名）。compact 下 `<Navigate replace>` 到 `/m`（D-049） |
| `/m` | 手机会话 home（D-049） | 手机优先路由树的首页（§4.7）：仅 compact 使用独立形态；桌面访问 `<Navigate replace>` 回 `/sessions` |
| `/sessions/new` | 新建会话 | query 可预填 `host` `workspace` `kind`；compact 与桌面**都不重定向**（共享页） |
| `/s/:instanceId` | 会话页 | print 默认 structured；pty-backed（codex/grok/agy/`claude-pty`/`terminal`）默认终端。**任何视口下都不重定向**（D-049） |
| `/s/:instanceId/tty` | 会话页终端视图 | 无 tty 时回 structured 并 toast；不重定向 |
| `/s/:instanceId/structured` | 会话页结构化视图 | pty-backed 的第二视图；不重定向 |
| `/s/:instanceId/files` | 会话页文件/diff（桌面右栏；手机全屏） | 不重定向 |
| `/approvals` | 收件箱 | `?focus=:interactionId` 高亮一条；桌面保留一等入口。compact 下 `<Navigate replace>` 到 `/m/inbox`，`?focus=` 等 query **原样保留**（D-049） |
| `/m/inbox` | 手机收件箱（D-049） | 两档 + 沿用 `?kind=` / `?focus=`（§2.5、§4.7）；桌面访问 `<Navigate replace>` 回 `/sessions` |
| `/hosts` | 主机列表 | |
| `/hosts/:hostId` | 主机详情 | |
| `/board` | 任务看板（D-050，§2.9） | query：`?project=`（缺省=全局，全部项目）。三列只读投影 + 已归档过滤；compact 下不进桌面看板，改由 `/m` 单列分段过滤承载（§4.7） |
| `/projects` | 项目列表（Hub `Project` 实体，D-050） | 路径保留「项目」文案；旧 Workspace 通讯录内容移到新建会话的主机/目录选择与项目详情 |
| `/projects/:workspaceId` | Workspace 详情（最近会话、默认 kind/model） | |
| `/providers` | Provider 列表 | |
| `/providers/:profileId` | profile 详情 | |
| `/bots` | Bot 总览 | |
| `/bots/:channelId` | 飞书或 Telegram 绑定 | |
| `/settings` | 设置 | |
| `/pair` | 设备配对（可选，herdrx `#pair=` 同类） | 第一里程碑可不做 |

深链：Web Push `data.url`、飞书卡片「在 runtime 打开」都进 `/s/:id` 或 `/approvals?focus=`。

重定向层（D-049，全部 `<Navigate replace>`，不新增鉴权、不改 query，详见 §4.7）：

| 命中 | compact | 桌面 |
|---|---|---|
| `/sessions` | → `/m` | 原样 |
| `/approvals?focus=…`（及其他 query） | → `/m/inbox?focus=…`，query 原样保留 | 原样 |
| `/m`、`/m/inbox` | 原样 | → `/sessions` |
| `/s/:instanceId*`（含 `/tty` `/structured` `/files` `/events`） | **永不重定向** | **永不重定向** |
| `/sessions/new`、`/login`、`/pair`、`/settings` | 不重定向（共享页） | 不重定向 |

推送深链因此在两套壳下都成立：`/s/:id` 是共享路由，`/approvals?focus=` 被重定向层带到 `/m/inbox?focus=`，`focus` 不丢。

### 1.3 桌面单侧栏 ↔ 手机单列

**断点（建议，已验证 herdrx 公式可抄）**

- Compact：`(max-width: 767px), (pointer: coarse) and (max-width: 1023px) and (max-height: 600px)` — `herdrx/web/src/lib/displayPreferences.ts` `COMPACT_WORKBENCH_QUERY`。手机旋转仍保持单列。
- 触控判定与布局判定分离：44px 命中区按 `pointer: coarse`，不按宽度（§3.4，D-039/D-053）。

**桌面（≥768px 且非 coarse 矮屏）**

```
┌──────────┬──────────────────────────────────────────────────────────┐
│ 侧栏 248 │ 页头 48：面包屑/标题  ●状态·host·cost  [终端|结构] 文件 ■ ⋯ │
│ Remuda ⌕ │ tab 36（仅 /s/*）：本任务的会话 或 本空间的会话        +  │
│ 会话      │        ┌──── 阅读列 720（页边距 32）────┐  文件 300（≥1280）│
│ 收件箱 3  │        │ transcript / 表单 / 看板       │  并排；<1280 覆盖│
│ 任务看板  │        └───────────────────────────────┘                │
│ 项目 …    │        停靠区（待处理卡 / 通知 / live / composer）      │
│  全局     │                                                         │
│ ＋ 新建   │                                                         │
│ ⚙ 管理    │                                                         │
└──────────┴──────────────────────────────────────────────────────────┘
```

- 一条带文字的侧栏（≥1024 为 248px，768–1023 为 216px）：品牌行（Remuda + QuickFind + 折叠按钮）、`<nav aria-label="主导航">`（首项必须是「会话」，其后「收件箱」「任务看板」）、「项目」区、底部「新建会话」与「管理」菜单。⌘/Ctrl+B 在所有桌面路由把侧栏收成 48px 图标列。取消旧的 48px 图标轨（D-053）。
- 每页一条 48px `PageHeader`：面包屑 + 当前页 `<h1>` + 本页动作。
- `/sessions` 和 `/board` 在内容区左边多一条索引列（宽 `--index`）：`/sessions` 挂 SpacesPanel，`/board` 是任务清单；宽度 <1024 时索引列收成页头上的按钮，点开是覆盖层，里面是同一个组件、同样的 testid。会话路由 `/s/*` **没有**第二列。
- tab 条只在 `/s/*` 上渲染（36px），列表路由不再有 tab 条；集合语义见 §1.4。
- 右栏文件视图在 ≥1280 时并排（300px）；<1280 时从页头「文件」按钮或 ⋯ 菜单以覆盖 sheet 打开。
- 会话页页头：面包屑/标题、host、cost、状态点（§2.1 投影）、`终端|结构` 分段、Stop、文件开关、⋯。这排元素在 compact 下允许重排与减铬，但 **`终端|结构` 分段与 Stop 必须始终留在页头，永不进 ⋯ 溢出菜单**（D-040 (2) 不变）：另一个视图是一等入口，不是「更多」里的设置。host 与 cost 留在桌面主行；诊断字段（driver / delegation / provider / lifecycle / seq / connectivity / native / promoted）进 ⋯ 菜单里的「运行详情」（§2.2；D-053 取代旧 ui-spec §2.2 第二行「只有这一个触发器」的版式（基线 `80db05b8:docs/design/ui-spec.md:335`），D-040 (3) 钉住的面板内容、按设备持久化与 `session-meta` 不变）。

**手机**

compact 下有**两套铬**，按路由切换（D-049，预算见 §4.7）：

```
首页级屏（/m、/m/inbox …）               会话路由（/s/:id*：structured/tty/files）
┌─────────────────────────────┐          ┌────────────────────────────────────────┐
│ 页头 52：标题 / 搜索          │          │ ‹ 标题块(Space·状态) [终端|结构] ■ ⋯ 52 │
│                             │          │                                        │
│ 唯一一列（全屏，可滚动）     │          │ transcript / xterm 正文                │
│                             │          │ （无「运行详情」触发行；live 行 20）     │
│                             │          │ ≥60% 视口（键盘收起）                     │
├─────────────────────────────┤          ├────────────────────────────────────────┤
│ 会话 收件箱·n 新建 更多 56   │          │ composer 56／本地输入 36·44(coarse)│
└─────────────────────────────┘          │ ＋键盘条 44（仅终端段）                │
                                         │ （无 app 底栏、无 tab 行）             │
                                         └────────────────────────────────────────┘
```

- 首页级屏：一条 52px 页头 + `PhoneNav`（56px，另加 `--safe-bottom`；由 64 改为 56，D-053 修订 §4.7）。
- 会话路由只有一条 52px 页头，从左到右：返回 `‹`（字形 20px）、标题块、`终端|结构` 分段（可见项 26px）、Stop（32px 方块）、⋯（32px）。标题块兼任 Space 抽屉触发（`spaces-drawer-open`）：第一行是状态点 + 标题，第二行是 `spaces-chips` 里的 Space 名 + 状态词；D-049 的截断三步逐字保留（§4.7）。
- **52px 是 compact 布局尺寸，44px 是 coarse 命中尺寸，两者分开**：上述可见字形在任何指针下都保持 20 / 26 / 32 / 32；44px 命中区（返回的 padding 44×44、标题块触发行高 44、分段 `::after` 上下扩到 44 且每项最小宽 44、Stop 与 ⋯ 的 `::after` 44×44）只在 `pointer: coarse` 下加上，热区互不重叠（§3.4）；落入 compact 查询的 fine 指针窄屏保持桌面密度，不强加 44。
- 该路由的「底部条」由 composer（结构段，收起态 56px，D-042）或本地输入条 + 键盘条担任：本地输入条 fine 指针下高 36px、coarse 指针下可见高 44px，键盘条 44px 仅 coarse 指针设备渲染（§2.3）。app 级底栏（`nav[aria-label="手机底栏"]`）不渲染，SpaceTabs 行也不渲染。切空间由标题块的抽屉与 Jump To sheet 承担，能力不降级。
- 键盘弹起时的隐藏 / 永不隐藏规则与正文 40% 保底见 §4.7。

映射规则：

| 桌面 | 手机 |
|---|---|
| 侧栏 + 每页页头（列表 / 看板另有索引列） | 底栏 + 全屏列表 |
| 会话页（无第二列） | 全屏会话；返回回列表 |
| 右栏文件（≥1280 并排，<1280 覆盖 sheet） | 覆盖 sheet 或 `/s/:id/files` 全屏 |
| 终端视图 | 全屏 xterm + 本地输入条 + 辅助键（仅 `tty-attachable`） |
| hover 预览 / 拖拽排序 | 禁止；改长按菜单 |
| 多 pane 分屏 | **不做**（一个实例一个终端） |
| 看板（桌面三列，§2.9） | **不复制看板**：compact 塌缩为 `/m` 单列分段过滤（待办/进行中/已完成/已归档，同一投影一次一列；D-049/D-050） |

Paseo compact 是左列表 / 中 agent / 右文件三态互斥（`paseo/docs/mobile-panels.md`）。本产品更简单：列表和会话是路由，不是手势抽屉。收件箱用独立底栏入口，避免 DSH「折叠组不汇总待交互」的坑（`deepseek-harness.md` §2.2 会话列表）。

---

### 1.4 Space / Tabs 项目工作台（D-024；面板位置与 tab 范围经 D-053 修订）

本节为 2026-09-13 的信息架构增补。借鉴 herdr 的 workspace → tabs → panes，当前实现前两个维度：**Space = 主机上一个已注册 Workspace，Tabs = 会话**；仍不做多 pane 分屏。D-053 修订的是面板所在位置与 tab 条的出现范围，Space 身份、关闭语义与快捷键原则不变。

- Space 来源是 D-023 的主机已注册 Workspace，以 `(hostId, workspaceId)` 区分；注册 wire 字段为 `workspaceId / hostId / root`，现有 UI 投影保持 `id / hostId / rootPath / label`。默认名称取根目录 basename，客户端重命名不会修改注册目录或后端 Workspace。实例严格按 host 与 workspace 两个 ID 归属，未注册、取消注册或无匹配的实例统一进「其他」。
- **Space 面板移到 `/sessions` 的索引列（D-053）。** SpacesPanel（重命名、手动排序、分组展开、「已退出 (n)」分组）挂在 `/sessions` 内容区左侧，宽 `--index`；宽度 <1024 时收成页头上的「空间」按钮，点开是覆盖层，里面是同一个组件、同样的 testid。不属于任何项目的 Space 也要能在这里管理。面板**不再有自己的折叠开关**，也删除旧的品牌色左竖线与首字母窄轨；折叠统一是侧栏的概念（⌘B）。显示名、手动顺序、分组展开状态继续写入本设备 `localStorage`，不跨设备同步。
- **tab 条只在 `/s/*` 上渲染**（36px，位于页头下方）；列表路由（`/sessions`、`/board`、`/approvals` 等）不再有 tab 条。tab 显示标题、harness 字形、状态点和关闭入口。每个 Space 分别记住最后选中的 tab；切换容器恢复其选择，不能把上一个容器的选中实例或新建默认值带过来。会话可从列表或深链重新打开，这不自动 resume；会话内 Stop 控制仍可使用。
- **tab 集合（所有者 2026-09-23 拍板，D-050：实例经既有 `instances.task_id` 归属 task）**：
  - 会话的 `instance.taskId` 非空：tab 集合是该 Task 的全部会话——取 hub 快照里 `taskId` 相同的所有实例，按所属 Space 的 dismissal 过滤后排序；
  - 否则：tab 集合是所在 Space 的 `visibleTabs`。
  - 零新增请求，不在 Shell/容器里调用 `useSessionTask`，也不依赖轮询。
  - tablist 的 aria-label 随集合变化：「本任务的会话」或「空间 {名称} 的会话」。
  - 关闭一个 tab = 在该会话**所属的 Space** 里关闭它，所以两种容器共用同一份 dismissal 记录。
- **状态与关闭分离（D-024 addendum，优先于早期描述；D-053 不改这部分语义）**：状态点永不是 ×（见 §2.1），× 只表示「关闭标签」，桌面在 hover 或当前 tab 上显示，手机长按或滑动显出。已退出会话的 × 直接移除 tab；运行中会话的 × 打开「停止并关闭 / 仅关闭标签」两选项 sheet。「仅关闭」只隐藏 tab、不发送关闭命令，会话继续运行，进入 blocked 时重新出现在 tab 条，在侧栏/面板点击它也会重新打开；「停止并关闭」才发送既有关闭命令，失败保留 tab 并提示，实际退出仍以 Instance/journal 更新为准。该偏好按 space 存在本设备 `closedTabs`（`{id, resurface}` 记录，旧的 id 列表按 `resurface: true` 读入）。
- **面板选中态与已退出分组（D-053 显式修订 D-024 addendum「侧栏强当前态」，见 D-053 第 10 条）**：addendum 原要求「当前 space 与当前会话都用品牌左条 + 底色 + 加粗标题」；现取消品牌左条，当前 Space 与当前会话统一用 `--bg-selected` 底 + `--fg-strong` 字 + 加粗标题，两者都能一眼看出，深浅两态都有足够对比、不单靠颜色。addendum 的关闭 / dismissal、「已退出 (n)」分组等其余语义不变：分组默认折叠，行内提供 **恢复**（既有 resume 能力）与 **删除**（`DELETE /v1/instances/{id}`，确认「删除会话及其记录？」）。运行中会话按钮为「停止并删除」，走 `?force=1` 由 Hub 停止并删除，客户端不再自行先 close；404 按幂等成功处理，`nodePurge` 非 `purged` 时提示主机侧数据待清理。删除失败保留该行并提示，不显示成功文案。
- **活动 tab**（D-053 显式修订 D-024 addendum「活动 tab 使用品牌下划线 + 底色 + 加粗」，见 D-053 第 10 条）：用 2px `--fg-strong` 下划线 + `--bg-selected` 底 + 加粗（下划线即强文字色，无品牌色角色；深浅两态均有足够对比，且底色 + 字重同时在、不单靠颜色）；键盘焦点环沿用全局 `:focus-visible`。
- `/s/:instanceId` 及其子视图路由保持有效，直接打开会同时选中实例所属容器（Task 或 Space）和 tab。当前 Space 的「新建」入口带入 host/workspace，cwd 默认该注册根目录；「其他」不虚构注册根。被移除或关闭的选中 tab 回退到该容器可用 tab，无 tab 时显示该容器的会话列表或空态。
- **快捷键**（⌘/Ctrl+1..9 永远等于「你眼前的第 n 个编号」）：
  - ⌘/Ctrl+B 在所有桌面路由折叠侧栏（复用侧栏折叠偏好，不再有「面板折叠」这个第二概念）；
  - `/s/*` 上 ⌘/Ctrl+1..9 按 tab 条显示顺序编号（`tabSlots()`）；`/sessions` 上按列表徽标编号（`switchSlots(activeSpace, prefs)`，徽标与按键同源，数据源不分叉）；两个函数都在 `lib/sessionSlots.ts`；
  - ⌘/Ctrl+[ / ] 切换前后 Space。
- **compact**：会话路由 `/s/:instanceId*` 不渲染 tab 条，也不渲染常驻 Space chips 行；页头标题块兼任抽屉触发（`spaces-drawer-open` 行为与 testid 不变），Space 名包在 `<span data-testid="spaces-chips">` 里，`/s/` 上不使用 `space-chip`，切空间与切 tab 能力不降级（另见 §4.7 的截断三步）。compact 列表路由（`/m`）的 Space 切换由首页页头的 Space 按钮打开同一个抽屉承担。**`终端|结构` 分段与 Stop 不受影响**：它们仍在页头，见 §1.3。

本节只作用于会话工作台。fleet 与全局收件箱的范围和入口不变，composer 继续以当前实例为控制目标。验收与桌面/400px、深浅两态截图见 [spaces-1.md](./evidence/spaces-1.md)；tab 语义增补的验收见 [tabs-1.md](./evidence/tabs-1.md)。

### 1.5 Task 层：任务聚合、看板列、目录绑定与任务空间（D-050，2026-09-20）

本节是 task 优先模型的界面口径；实体字段、表结构、RPC/路由形状、迁移预算与门控动词以 [task-model.md](./task-model.md) 为机械权威，ADR 为 [D-050](./decisions.md)。

- **Task 是 instance（会话）之上的聚合层，不是第二套状态机。** Hub 的 Task 台账 8 态（pending/placed/running/stalled/done/failed/parked/deferred）是唯一权威；看板列、任务分组、人读 key、共用计数全是只读派生投影，UI 不把投影写回为状态。
- **一个 task 聚合多个 session**：实例经既有 `instances.task_id` 归属 task。一个 task 的多个 session 用 **tab/卡片**呈现，tab 集合按 §1.4 的 D-053 规则：会话 `instance.taskId` 非空时是该 Task 的会话，为空才回落到所在 Space 的 `visibleTabs`（`spaces/store.ts:93-99`），点开进共享 `/s/:id`。**不做并排多 pane 工作台**：§1.3 映射表「v1 不做多 pane 分屏（一实例一终端）」对 task 面同样成立；某参考看板产品的并排多面板不抄。
- **看板列是只读投影**：待办 / 进行中 / 已完成 三列 + 已归档过滤（不是第五列）。投影表、failed 按 placement 归位 + 角标、拖卡列→列多跳映射、done≠解锁见 §2.9 与 [task-model.md](./task-model.md) §5。
- **目录绑定二选一**（task 创建时显式选择，按项目记忆，无静默默认）：`reuse` = 复用注册根或既有 `remuda-wt` 兄弟目录，一目录一分支、attach 期间独占、多 task 顺序轮用，归还对目录零操作；`pool` = app 管理的 worktree 池，detached-HEAD 停放、租借时切 `wt/<slot>/<task-slug>`、归还时 reset/clean/park 但不删除。池满、目录忙、分支冲突一律显式 `blocked`（卡片/行显示理由），**绝不静默换目录或回退主 checkout**（D-035）。「与 N 个 task 共用」= 同目录 lease 的引用计数，是顺序轮用不是并发；Space 键 `(hostId, workspaceId)` 不放松（D-024）。
- **项目空间 vs 任务空间**：项目空间 = 既有文件视图（轴 `hostId+workspaceId`，[files-view-contract.md](./files-view-contract.md)），即 worktree 全树；任务空间 = 同一视图按 task 的 session 集合 + `owns[]` glob 的**客户端过滤投影**，空态「还没有文件」，不新增端点。两空间在会话右栏/`/s/:id/files` 内以 tab 切换。
- **批注是设备本地 composer 草稿**（不是 wire 字段、不是表）：卡片级或消息内锚点 `①` 两级载体，composer 显示「本次发送带 N 条批注」，随下一次发送拼进 prompt 前缀后清零。
- **手机（D-049）**：`/m` home 在既有 项目+branch 分组上叠加 task 分组；桌面看板在 compact 塌缩为单列分段过滤（§4.7），不复制看板；会话本体与文件视图永远是共享 `/s/:id`、`/s/:id/files`，不造第二份 transcript。
- **看板与任务列表是 project 维度的表面，不取代会话主导航**：§1.1 的会话区仍是一等导航；看板不是整站 IA（§5.1 对四栏看板产品「不要把主 IA 做成看板」的保留成立——主 IA 仍是会话，看板是项目内的任务表面）。

---

## 2. 关键屏

每屏：ASCII 线框、状态清单、数据字段。字段名跟 `protocol.md`：Instance / Command / Interaction / NativeRef / Workspace。列表上的 `status` 点是 `lifecycle × activity × connectivity` 的投影，不是单独的 wire 枚举。

### 2.1 会话列表 `/sessions`

**页面骨架（D-053）**：侧栏之后，内容区在 ≥1024 时左边挂 SpacesPanel 索引列（§1.4），右侧是页头 + 工具行 + 列表。页头为面包屑「会话 / {Space}」，右侧放「新建」主按钮。工具行高 48px：搜索框 `session-search`（高 36px，最宽 480px）、范围 `.seg`、「筛选」按钮、右侧计数（12px `--fg-muted`）。

```
┌ 侧栏 ─┬ SpacesPanel ─┬─ 会话 / sfe-root                         ［＋ 新建］ ┐
│        │ （索引列）    │  [搜索标题/cwd/原生 id        ] [全部▾] [筛选]  12 条 │
│ 会话 ● │ ▾ sfe-root   │──────────────────────────────────────────────────┤
│ 收件箱  │   bolt      │  ● 标题………………  下一步……  host/工作区·分支   时间  ⋯ │
│ 看板   │   forge     │  ○ …                                                    │
│ 项目…  │ ▸ 已退出 2  │                                                        │
└────────┴─────────────┴──────────────────────────────────────────────────┘
```

compact（手机）不渲染这个页面：`<Navigate replace>` 到 `/m`（D-049），手机首页见 §4.7。

**行内容 = 状态点 + 标题 + 一句下一步（D-038，2026-09-19 增补）**

上面线框里每行的第二句（「等你批准 Bash」「AskUserQuestion · 3 题」「Workflow wf_ab12 · phase compile」）是**派生出来的一句话**，不是 wire 字段的转写。据此把行内容钉死：

- **默认视口里只出现**：状态点（§2.1 上表的三维投影）、标题、kind 芯片、unread/pending 徽标、以及一句「下一步」；容器 ≥960 时宽行 grid 另有「主机/工作区·分支」与相对时间两列（D-053 对本条的显式增补，见下「行栅格」）。行内**不出现** `lifecycle`/`activity`/`connectivity` 的原文三元组（`ready · waiting-interaction · connected`），也不出现 `ins_…` 短码、`driver`、`model`。
- 那句「下一步」由**已有字段投影**得出（`projectStatus` / `Interaction.request.kind|description` / `exitLabel` / screen 终态），**不新造状态机、不猜成功**。`connectivity ≠ connected` 与 `lifecycle ∈ {unknown,reconciling}` 必须产出「状态待确认」，**不得**回落成 idle/空闲一类正向文案；`exited` 用「已退出」而不是「完成」。
- 三维 wire、`ins_`、driver、model 退到每行的 `<details data-testid="session-wire">` 展开内容或 `title`：**默认视口不可见**，可读性不丢。
- 行内遥控（`send…` 输入框、enter / esc / ctrl+c 按键）收进长按/溢出 `Sheet`（**保留全部既有 testid 与命令路径**，只是不再常驻行内）；「待处理」组在行上保留**一个**主操作（「去处理」，`board-go-handle`，用 `.btnPrimary .btnSm`，直达 `/approvals?focus=` 或该会话）。
- 状态点仍是三维投影（下表），本条只改**文本**的呈现位置，不改状态语义。

**行栅格（D-053；宽行修订 D-038 的默认视口清单）**：列表容器设 `container-type: inline-size`。

- 容器宽 ≥960px：单行 48px，grid 列为 `[28 | 标题 minmax(220px,2fr) | 下一步 minmax(200px,3fr) | 主机/工作区·分支 200 | 时间 56 | 动作]`；后两列（主机/工作区·分支、相对时间）是 D-053 对 D-038 默认视口五要素的**显式增补**——三维 wire 原文、`ins_`、driver、model 仍只进 `session-wire` / `title`；
- 容器宽 <960px：两行，共 56px（不额外加列，仍是五要素口径）；
- 多选框（`board-select`）只在悬停、行内 `focus-within`、已有选中项或 coarse 指针时显示；
- 行高与热区按 §3.4 与 [visual-system.md](./visual-system.md)：coarse 指针下行可见高度可直接取 44px。

理由与依据见 [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.3（Moshi 在同一位置放的是错误原文，不是 wire 串）、§11.7 的 P0-6 行（该行结论是**当前代码偏离了本节线框**，不是建议与规格冲突）。

**编号徽标与 ⌘/Ctrl+1..9 同源**：行首数字徽标的来源保持 `switchSlots(activeSpace, prefs)` 不变；`/sessions` 上的 ⌘/Ctrl+1..9 解析同一份结果（§1.4），徽标命名的会话与按键打开的会话永远一致。

**状态点（列表 + 会话页头共用）= 协议三维投影**

wire 用 `lifecycle` × `activity` × `connectivity`（`protocol.md` §2.3）。UI 只画一个点，形状可区分、不只靠颜色，且**任何状态都不画成 ×**（× 保留给「关闭标签」）。规则：

| UI 点 | 色 | 投影 | 禁止 |
|---|---|---|---|
| `blocked` | 琥珀 ⚠（`--attention-fg`） | `activity=waiting-interaction` | 优先级最高 |
| `working` | `--link` 实心点 | `activity=working` | |
| `starting` | `--fg-faint`，dim 脉冲 | `lifecycle ∈ {requested,preparing,starting}` | |
| `idle` | `--fg-muted` 描边 ○ | `activity=idle` 且 `lifecycle=ready`。含 `--bg` 原生 `state=done` 但进程仍在（可 send） | **不要**把这种情况画成会话结束；在线不用绿色 |
| `exited` | `--fg-faint` 灰 ■ | `lifecycle=exited` | jsonl 可能还在；**禁止**画成 ×，× 只表示关闭标签（D-024 addendum） |
| `unknown` | 虚线环 | `lifecycle ∈ {unknown,reconciling}` 或 `connectivity ≠ connected` | **禁止**画成 idle/exited 成功 |

没有名为 `done` 的 Instance 点。Run 成功只出现在 transcript usage / 回合脚，不改列表点。

过滤：`hostId`、`workspaceId`、`kind`（claude/codex/grok/agy）。多选。URL query 同步，可分享。

**行字段（列表 API `GET /v1/instances`）**

| 字段 | 说明 |
|---|---|
| `instanceId` | Hub 分配（`ins_…`）。对外文案「会话」，wire 不是 Session |
| `title` | 首条 user prompt 截断；可改 |
| `lifecycle`, `activity`, `connectivity` | 协议三维；UI 点按上表投影 |
| `kind` | claude / codex / grok / agy |
| `driver` | Claude：`claude-print` \| `claude-bg` \| `claude-pty` |
| `mode` | 投影：`structured-only` \| `tty-attachable` |
| `hostId`, `hostName` | |
| `workspaceId`, `cwd`, `worktreeLabel?` | Workspace；UI 芯片写「项目」 |
| `providerProfileId`, `model` | |
| `nativeRef` | `NativeRef`；列表只展示 `kind` + 短码，不把 paneId 当 instanceId |
| `pendingInteraction?` | `{id, type: approval\|question\|plan-review, summary}` |
| `lastEventAt`, `updatedAt` | |
| `unread?` | 本设备 last-read seq 落后 |
| `capabilities` | 见 §3，决定行上是否显示「终端」入口 |

子 agent / Workflow member **不**出现在默认列表（DSH `origin:'subagent'` 隐藏）。父行可显示「3 个子任务 running」。点进父会话的 Workflow 树再打开 child。

空态：无主机 → CTA「添加主机」；有主机无会话 → 「新建会话」。

### 2.2 会话页 · 结构化视图 `/s/:instanceId`

```
│ 页头 48：sfe-root / 修复滚动 · 会话标题  ● 等待 · bolt · $0.12        [终端|结构] 文件 ■ ⋯ │
│ tab 36（仅 /s/*）：本任务的会话 或 本空间的会话                                   +  │
│            ┌──── 阅读列 720（页边距 32）────┐        文件右栏 300（≥1280 并排）        │
│            │ You                                      12:01  │                        │
│            │ 看 TaskManager spill 这段为啥抖                          │                        │
│            │ （thinking / 工具卡 / 审批卡 / Markdown…）                  │                        │
│            │  usage  in 12.1k / out 800 · $0.12                  │                        │
│            └───────────────────────────────────────┘                        │
│            停靠区：待处理卡 / 路由故障 / 通知 / live 行 / 批注行 / TaskTrack          │
│            ┌ composer（与阅读列同宽，圆角 12）────┐ 或 EndedBar                 │
```

**页头（D-053；D-040 的桌面主行规则不变，旧 §2.2 的第二行触发版式被取代）**

- **非 compact · 宽桌面（≥1024）**：一行 48px，左右 20px。左侧：面包屑「{Space} / {Task 标题}」（13px `--fg-muted`，未绑定 task 的会话没有 Task 段）+ `<h1>` 标题（`--text-title`，单行省略）+ 12px 元信息行，用 `·` 分隔：StateDot + 状态词（`session-status-label`；状态词就是 §2.1 三维投影的统一词表——`UI_STATUS_LABEL`：待处理 / 运行中 / 启动中 / 空闲 / 已退出 / 状态未知，见 `web/src/lib/status.ts`，页头不另造词；颜色随投影：blocked（待处理）用 `--attention-fg`，unknown（状态未知）用 `--unknown-fg` 中性色 + 虚线状态点，其余用常规文字色）、host（`session-host`，最宽 160px）、cost（`session-cost`，未知显示「—」）。右簇：`ViewSwitch`（`.seg`）、「文件」（`files-toggle`）、Stop（32px `.iconBtn`，aria-label `Stop`，hover 变 danger 色）、⋯（`session-more-open`）。API 路由故障不是状态词：由停靠区 `session-api-route-down`（`role=alert`，danger）承载。
- **非 compact · 768–1023**：同样是 48px 桌面页头；面包屑只留最近一段，host 最宽 120px，「文件」进 ⋯。宽度落在此区间、但命中 §1.3 compact 查询（coarse 指针且宽 ≤1023、高 ≤600 的横屏）的设备**不走本形态，走下一条 compact 页头**。
- **compact（§1.3 查询：宽度 <768，或 coarse 指针且宽 ≤1023、高 ≤600 的横屏）**：一行 52px，左右各 4px。从左到右：返回 `‹`（aria-label「返回」，字形 20px）、标题块（`<button data-testid="spaces-drawer-open">`，flex 1，打开 Space 抽屉）、`终端|结构` `.seg`（每项可见 26px）、Stop（32px 方块，与 ⋯ 间距 12px）、⋯（32px）。命中区只在 `pointer: coarse` 下扩到 44：返回用 padding 44×44、标题块触发行高 44、分段每项 `::after` 上下扩到 44 且最小宽 44、Stop 与 ⋯ 用 `::after` 44×44；fine 指针下保持 20/26/32/32 可见字形（§3.4）。标题块两行：第一行 7px StateDot + 标题（16px/600）；第二行 12px——`<span data-testid="spaces-chips">{Space 名}</span>` + ` · {状态词}`。状态词与桌面用**同一份词表**（§2.1 的 `UI_STATUS_LABEL`：待处理 / 运行中 / 启动中 / 空闲 / 已退出 / 状态未知），不出现「已断开」「节点丢失」这类页头自造词；颜色随投影：「待处理」用 `--attention-fg`，「状态未知」（`connectivity ≠ connected` 按 §2.1 投影即 unknown）用 `--unknown-fg` 中性色并配虚线状态点，其余常规文字色。API 路由故障 / 节点丢失只由停靠区 `session-api-route-down`（`role=alert`，danger）承载，手机桌面一致，不写进标题块。`/s/` 上不使用 `space-chip`。截断按 D-049 三步（§4.7），状态点始终在第一行。
- **「运行详情」从第二行移进 ⋯ 菜单（D-053）**：页头不再有第二行触发器。**取代的是旧 ui-spec §2.2（基线 `80db05b8:docs/design/ui-spec.md:335`）「第二行只有这一个触发器」的版式**，不是 D-040 (3) 的条文；D-040 (3) 钉住的内容（哪些诊断进 disclosure、默认收起、按设备持久化、`session-meta` testid）全部不变。`RunDetails` 新增受控 prop `open` / `onClose`，由菜单项 `run-details-summary`（「运行详情 · N 项」）控制；不传 props 时保持自带触发行为。面板仍在页头下方的文档流里展开（不是浮层），12px 文字、值用等宽字体，内容为 driver / delegation / provider / providerSourceHint / lifecycle / seq / connectivity / native / promoted / LaunchedBy；默认收起，展开态按设备持久化到 `runtime.run-details.open`；`run-details`、`session-meta` testid 不变。
- **⋯ 菜单**（`SessionMoreMenu`；桌面 `.menu`，手机 `.sheet`，每项 48px），顺序固定：

  | 顺序 | 菜单项 | testid | 说明 |
  |---|---|---|---|
  | 1 | 运行详情 · N 项 | `run-details-summary` | 控制 `RunDetails` 的 `open` prop |
  | 2 | 搜索正文 | `transcript-search-open` | 仅结构视图；调 `TranscriptHandle.openSearch()` |
  | 3 | 全部折叠 | `collapse-all` | |
  | 4 | 紧凑工具卡 | `density-toggle` | |
  | 5 | 文件 | `files-toggle` | 仅 <1024 |
  | 6 | 原始事件 | `events-toggle` | |
  | 7 | 加批注 | `annotation-add` | 只读时是禁用项「只读预览 · 不可批注」，testid `annotation-readonly-tag` |

- `终端|结构` 分段与 Stop **永不进 ⋯**（D-040 (2) 不变）。

**阅读列（D-053）**

- 正文列 `max-width: var(--read-measure)`（720px），左右 `--read-gutter`（32 / 24 / 16），水平居中；上下内边距 ≥1024 为 32/40px，<768 为 20/24px。停靠区子项（通知、live、composer、结束条等）宽度 `min(100% - 2*var(--read-gutter), var(--read-measure))`，与阅读列居中对齐；1440 下 720、768 下 504、390 下约 358。
- 用户与 assistant 消息同宽、同字号：`--text-read`（16px/28px），颜色 `--fg-body`。作者行 12px `--fg-muted`，显示「You」或原始 role；queued、held、interrupted、steer 这些标签照原文保留。用户消息左侧一条 2px `--border-control` 竖线，左内边距 12px（手机 10px）。删除旧气泡的边框、宽度上限与 `align-items: flex-end`。
- 行间距只放在被测量元素自身的 padding 上，不用兄弟选择器：`.row { display: flow-root; margin: 0; padding-bottom: 12px }`，`.row[data-role="user"] { padding-top: 16px }`；虚拟列表上报高度只报 `getBoundingClientRect().height`，不再额外 +12。
- 代码块：`--bg-inset` 底，8px 圆角，13px/1.6 等宽，内边距 12px 14px，横向在块内滚动；删除 backdrop-filter；语法色走 `--tok-*`。
- 流式：光标行内渲染，宽 7px，预留位置；渲染时把奇数个 ``` 补齐闭合（`fenceBalance`，只影响显示，不改存储）。
- `Transcript` 用 `forwardRef<TranscriptHandle>` 暴露 `{ openSearch(); collapseAll() }`，prop `toolbar?: boolean` 默认 true。

**停靠区（`data-testid="session-dock"`）**

- 子项水平居中、宽度同阅读列，子项间距 6px（键盘态 4px）。从上到下：
  1. **待处理卡区**：`flex: 0 1 auto; min-height: 44px; max-height: calc(var(--workbench-height) * .45); overflow-y: auto`——空间紧张时由它先收缩、内部滚动；新卡出现时滚入视野。
  2. **路由故障**（`session-api-route-down`，`role=alert`）：左侧 2px `--danger-border` 竖线，13px 字，**永不隐藏**。
  3. **最新通知**（`SessionNotifications`）：24px 一行，只显示最新一条，多的用「+N」原地展开；「查看待处理对话」用 `--link`；取消浅色反转底。
  4. **live 行**：见下。
  5. **批注行**：只在 N>0 时出现，24px 高，12px `--fg-muted`，文字「本次发送带 N 条批注」，testid `annotation-badge-row`。正文上方不再有浮层（旧的 `.floatLayer` / `.annotationBar` 删除）；批注入口只剩 ⋯ 菜单一项。
  6. **TaskTrack**。
  7. **Composer**，或者会话已结束时的 **EndedBar**。
- **EndedBar**（`data-testid="ended-bar"`）：`status === exited` 时挂载、Composer 不挂载；旧的重启横幅与页头续接入口删除，续接只保留这一个入口。surface 底、12px 圆角、1px `--border`、内边距 12px 14px。第一行（13px）：圆点 +「会话已结束」；节点重启时后接「 · Node 重启」（挂 `node-restart-banner`）。有未送出的排队消息时，显示 12px `--attention-fg` 的「有 N 条排队消息未送出」（`ended-held-note`）。`canResume` 为真时：「继续（结构化）」用 `.btnPrimary .btnLg`，所在组挂 `resume-control`（节点重启时按钮上挂 `node-restart-resume`）；「在终端中继续」挂 `resume-terminal`；手机上两个按钮并排、各占 flex 1、高 48px。为假时：显示「该会话没有可续接的 transcript」和「开新会话继承 cwd」。
- **turn 时钟**：删除整页每秒重渲染的 `useNow(true)`；改由 `useTurnDecision` 只在 working / waiting / unknown 时开 1s 定时器，只在 state、decidedBy、endedAt 变化时 setState，`document.hidden` 时停表。空闲时会话页每秒提交 0 次。`useNow` 本身不改，其余订阅者照旧。

**实时状态条（dock，`LiveStatusStrip`，一行 20px，12px，`nowrap`）——回合是否结束由会话已有的每条通道共同决定，不由 hook 相位锁存单独决定**

呈现顺序，从左到右：6px 圆点 → 阶段词（`live-phase`）→ 工具名（最宽 180px，手机 120px）→ elapsed（`live-elapsed`）→ `↓ 1.2k`（`live-token-count`）→ phrase（`live-phrase`，最先省略）。最右侧是 tier 与 decided-by 两段纯文字，以及健康提示 token「hook 静默 / hook 无记录」——这是**新鲜度未知**的证据（没有验证到失败），用 `--unknown-fg` 中性色并配虚线标记，不用琥珀也不用红；testid `live-health-<tier>` 和 `data-reason` 保留。宽度 <480px 时先去掉 token 计数和 tier。`live-interrupt` 的去留：先验证「凡是 `canInterrupt` 为真时，composer 的 `controls.interrupt.available` 也为真」，验证通过就删除；不通过就在行尾保留一个安静的「打断」文字按钮。

状态条渲染纯 reducer `turnEnd` 的唯一裁决（`working | waiting | ended | unknown` + `decidedBy` + `endedAt`），不再直接渲染 hook 相位锁存。`phase.ts` / `liveStatus.ts` 仍是纯 fold，所有消费决策归 reducer。优先级从严到宽：

1. **人机回合高于一切结束信号**：本 instance 有 pending 交互、screen 锁存自身报 blocked、或新鲜的 hook `blocked` 相位时，无论 hook 通道多静默、spinner 是否已清空，都判 `waiting`。parked 权限 hook 本身就不发记录、其对话框又会清掉 spinner，否则会在 6 s stall 预算后误判「回合结束」并把代持消息 POST 给仍卡在对话框上的 agent。一个 hook 已静默又无 pending 的 blocked 是 `unknown`，绝不是「等待操作」。
2. hook 的 `turn-ended` / `interrupted` **无条件**直接定终（harness 自己的终态词，新到即覆盖 `decidedBy=hook`）；文件层（grok 的 `turn.live` 相位打在既有 lifecycle 上、`tier=file`）的终态相位同样无条件定终，但 `decidedBy=file`——grok run 仍登记 `SignalTier::Hook`（`ApprovalChannel::EmulatedScreen`，见 `crates/remuda-node/src/adapter_registry.rs`），只是这条相位证据自身带 `tier=file` 标签，终裁按证据的 tier 记为 file，而不是按 run 的登记 tier 记成 hook。hook 通道**新鲜**时，hook 活动相位持有回合（设计 §2.4 规则 6，screen 的中途 idle 边沿被忽略）。
3. hook 通道一旦 `stalled` / `never-materialised`（`channelHealth`，3×2 s）即降为参考，screen 的非活动 `live.status`（或 pty `agent_status`）结束回合——但 screen 清空每「离开」只发一次、其 `observedAt` 会留到下一回合，所以要求 screen 锚 **不早于** 锁存相位的 `since`，避免短回合沿用上一回合的清空时间。
4. 再没有 screen 时，晚于锁存 `since` 的 assistant 消息从 transcript 收尾结束。

- `ended` 显示「回合结束」+ 小芯片 `data-testid=live-decided-by`（值 hook/file/screen/transcript；`file` 来自锁存相位的 `tier=file` 标签，grok 文件层结束的回合显示 `file` 而非 `hook`）；elapsed 始终锚在**回合开始**（submit/spinner 再锚，跨回合从事件列表重取，不依赖会被 turn-ended/clear 覆盖的锁存 `since`），结束时冻结在该回合时长 `endedAt − start`（即终端显示的时长），而不是塌成 0:00 或在稍后打开页面时变成「结束以来」；移除 Esc 打断按钮。迟到的 hook `Stop` 改判 `decidedBy`，但 composer 的 `ended→idle` 边沿幂等，不二次 flush、不移动时长。
- composer 相位由该 reducer 与 `projectStatus` 合并：`ended→idle` 是工作→空闲边沿，已有 effect 恰好调用一次 `flushHeld`；`unknown` 退回 instance 投影，不塌成 idle/blocked。
- 静默注记可附 Node 侧实际跑过的检查名（`relay-missing` / `socket-refused` / `link-stalled`，来自 instance 上的 `hook.silence` 诊断）。屏幕轮询层只真正探测 relay 与 socket：一个正常结束的会话本来就不再发 hook，relay/socket 都健康时**不报** `link-stalled`（该判定只属于持有 journal flush 游标的 transport 层），新鲜 tier（Stop 刚到）也不写记录；没有检查结果时徽标保持原样，绝不猜原因。这些注记表达的是**新鲜度未知**（长时无 hook 记录不等于已验证的失败或待办），统一用 `--unknown-fg` 中性色 + 虚线标记 + 说明文字；真正验证到的失败走 `--danger-fg`、需要人操作走 `--attention-fg`，二者不混用。
- `Notification` hook（含 Claude Code 的 `idle_prompt`「等待你输入」）是结束后的咨询，不是阻断请求：不抬相位、不算 waiting；在站内以 toast + 会话页小通知列表呈现（文案、时间、可忽略；`permission_prompt` 类链接到待处理对话卡），既有 push路径不变。注意 grok 没有单独的阻断权限 hook——它唯一的权限提示就是 `Notification(permission_prompt)`；移除其 waiting 语义后，grok 的 blocked 状态**只**由 screen 层（OSC/屏幕 blocked 锁存与 pending 对话框识别）给出。

**Transcript 节点（journal fold，稳定 `nodeId`）**

| 节点 | 默认 | 数据 |
|---|---|---|
| `user.message` | 展开 | `role=user` blocks；本地乐观气泡在 accepted 后换成权威节点（DSH Chat 规则） |
| `assistant.message` | 展开 | 流式 Markdown |
| `thought` | 折叠 | 未完成可展开看；`completeness=screen-derived` 时标「从屏幕猜测」 |
| `tool.*` | 见下 | call/result 配对，`callId` |
| `interaction.*` | 未决时钉在 composer 上方并接管输入 | 见审批卡 / 表单 |
| `workflow.run` | 运行中与结束后都展开；只有读者 dismiss 才折叠（按 workflowId 持久化于 runtime localStorage）。未 dismiss 的卡永不进折叠行；dismiss 后折进「N 次工具」行，打开该行仍能到达卡片 | 见树；运行中每 1s 走墙钟 |
| `usage` | 回合结束后一条 | 缺字段就整条隐藏，不写假合计（DSH `ui-chat` usage 行） |
| `error` | 展开 | 不推断成功 |
| `opaque` | 一行「未识别事件」+ 原始 kind | 禁止当终态 |

**工具调用折叠（D-041；D-053 修订「桌面默认态不变」）**

已 settled 且成功的工具调用，在**所有宽度下**都默认渲染为一行折叠的 `FoldedToolRow`：

- 无边框；fine 指针下高 24px，coarse 指针下热区 44px；13px 字。前置圆点：成功用 `--fg-faint`，运行中用 `--link`。关键参数用 12px 等宽 `--fg-muted`（Bash = 命令首行，Edit/Write/Read = 路径，截断到宽度、`title` 给全文），展开控件 `tool-fold-open`。裸「Bash」不合规。
- **永不自动折叠**：失败、运行中、未 settled、partial、interaction、Workflow。豁免集合与 D-041 完全相同；**折叠分支仍必须在 family 判定之后**（否则 Workflow 卡被误折、1Hz 走针不启动、elapsed 出处静默死掉），D-052 第 4 条的回归断言不变。
- 读者显式「全部折叠」保持既有行为：一切非 failed 卡（含 Workflow 与 running）都折。
- 展开后的卡与桌面完全一致（同一组件、同一 testid）；「N 次工具 · N 段思考」「未识别事件 · {kind}」等 compact 文案与 testid 不变。

**有界 journal 窗口与「加载更早」**

Hub 的 journal 读是有界尾部窗口（至多 2000 行 / 8 MiB，新行优先）：attach 与 follow snapshot 只拿尾部，窗口元数据 `fromSeq`（窗口底，空窗口为 null）与 `reachedAfterSeq`（底是否到 afterSeq）随行返回，更深的历史用同一 `afterSeq` + `beforeSeq = fromSeq - 1` 向下翻页。

- 初始加载只读一个尾部窗口；**禁止**再写「512 行循环向上翻」这类读法——有界窗口下它切的是尾部中段，下面的历史被静默丢弃。
- snapshot 的 `fromSeq` 是**窗口底，不是保留底**：不得喂给 `applyBatch` 的 `from < floorSeq` 陈旧判定（那会把正常的深页误判 readonly-stale）。
- gap 回填向下降页直到 `reachedAfterSeq=true`，缓冲后按 seq 顺序一次性 flush；翻页有界（约 16 页 / 20k 事件），预算耗尽时 flush 已连续的前缀、只报一次残余 gap、落到 readonly-stale，绝不永久缓冲。
- 已加载底 > 1 时，transcript 顶部显示「加载更早的记录」行（JournalBanner 旁边的独立行）：一次点击取一页（`beforeSeq = floor - 1`），按 seq 升序 prepend；虚拟列表以点击前最顶部可见节点为锚，保持其视口偏移（padTop 增长只推动锚上方的留白）。到 seq 1 后该行消失。
- banner 状态：补页中 `gap-backfill`（「正在补事件 · 工具卡暂不结算」），预算耗尽/分歧 `readonly-stale`，补齐回 `live`。
- 已知缺口（待修，不在本期）：空的下降窗口目前按「已到达 afterSeq」处理，因此被删除行造成的真空缺（窗口有界但中间缺 seq）会被静默当作已补齐，客户端停在 `gap-backfill`。后续应区分「窗口底到达 afterSeq」与「范围内无洞」。

**Tool 卡片（keyed registry，未知 → Generic）**

注册键 = `driverKind + '.' + nativeToolName`，再映射到族。不要把 Codex `commandExecution` 硬叫 Bash（`deepseek-harness.md` §2.2）。

**各 harness 原生名 → 族（注册表按原生名分发，绝不先改名成 Claude 工具名；卡片可共用 shell/read/write/task 布局）。** grok/codex 文件适配器的 `driverKind` 都是 `shell-pty`，身份只由原生名承载。grok 卡片的标题目标形态是 ACP 帧的人类 `title`（`displayTitle`），稳定原生名作为弱化次级标签（`data-testid=tool-native-name`）跟随其后——presenter 按原生名分发，人类 title 只用于标题。**时效说明：人类 title 要等 D-043（c-grok-toolid，in flight）把 statusless `tool_call_update` 的 `title` 带进 `display_title` 之后才有；在此之前 adapter 对 `display_title` 与 `tool_name` 填同一字符串（`adapters/mod.rs`），所以现网卡片标题就是原生名。** 次级标签在「标题 = 原生名」时一律不渲染（五处卡片共用同一 guard），名字不会印两遍；D-043 落地后两者分叉、标签自动出现，无需再改 web。MCP 卡例外：标题固定为族名 **MCP**（claude/grok 一致），不使用人类 title。grok 这张表与 Rust 侧 adapter 的 name→`ToolCategory` 表是同一张（`grok-structural-translation.md` §3.1），两侧必须同步：

| harness | 原生名 | 族 / 呈现要点 |
|---|---|---|
| claude | `Bash` `Edit` `Read` `Write` `Workflow` `Task`/`Agent` | 同名族 |
| claude | `mcp__server__tool` | MCP |
| grok | `run_terminal_command` | Bash 布局；读 `command`/`description`/`is_background`，cwd 在 completed 帧的 `rawOutput.current_dir`（运行中输入里没有）。标题：D-043 落地后 = ACP 人类 title（`displayTitle`，如 `Execute \`printf …\``），现网 = 原生名；原生名次级标签仅在与标题不同时出现；折叠态同规则 |
| grok | `read_file` / `list_dir` | Read 布局；路径取 `target_file` / `target_directory`（不是 `file_path`），范围取 `offset`/`limit`（`limit` 是行数不是结束行号，文案「第 N 行起，M 行」） |
| grok | `write` / `search_replace` | Write / Edit 布局；`file_path` + `old_string`/`new_string` |
| grok | `grep` `web_search` `web_fetch` `open_page` `open_page_with_find` `x_*` | Generic（Search 样式） |
| grok | `spawn_subagent` | Task 布局；`prompt`/`description`/`subagent_type`/`isolation` |
| grok | `workflow` | Workflow 族；解析 Rhai `let meta = #{ name: "…", description: "…" };`，**不**走 Claude `export const meta`；解析失败退回 source 类型（script/script_path/resume/pause/stop/name）+ 截断脚本，永不 blank 卡 |
| grok | `search_tool` / `use_tool` | MCP；标题固定为族名 **MCP**（不用人类 title）；grok 限定名是 `server__tool`（无 `mcp__` 前缀），server/tool 从 `use_tool.tool_name` 取，原生名次级标签照常显示 |
| grok | `ask_user_question` | Generic 兜底卡（问题 + 选项 labels + multiSelect）；正常应升格为 `interaction` 由 QuestionForm 渲染，卡只是无 interaction 帧时的 fallback |

MCP 启发式收紧：**只有** `mcp__…` 限定名或表中显式 MCP 族才映射到 MCP；未知工具名里裸含 `__` 不再判 MCP（grok 的 `server__tool` 只经 `use_tool` 输入到达）。

| 族 | 线框要点 | 字段 | 状态 |
|---|---|---|---|
| **Bash** | 命令一行；stdout/stderr 折叠；exit pill | `command`, `cwd`, `stdout?`, `stderr?`, `exitCode?` | running 无 exit；缺 exit **不准**画成功（DSH TerminalBlock 限制） |
| **Edit** | 路径 + DiffBlock | `path`, `oldText?`, `newText?`, `applied: boolean` | 运行中=拟修改（参数）；settled 且 result.diffs=已写入；只有参数没有 result=「结果未知」 |
| **Read** | 路径 + 行号片段 | `path`, `range?`, `snippet?` | 失败走 Generic |
| **Write** | 路径 + 拟写入/已写入 | 同 Edit，无 oldText | 同上三分态 |
| **Workflow** | run → phase → member 树 | `runId`, `launchedAt`, `phases[].title`, `members[{name,status,startedAt,endedAt,lastProgressAt,tokens,childInstanceId?}]` | 运行/失败/结束都默认展开，只有用户 dismiss 才折叠（持久化）。member 行依次显示 状态 / 模型 / label / 排队等待 / 用时 / 空闲 / tokens；缺字段显示 `—` 并在 tooltip 点名缺失字段，绝不写 0；phase 头带该 phase 的真实跨度（最早 start 到最晚 end）与 tokens/调用合计。member 可点条件：child 在 registry 且 `capabilities.openChild`（DSH 只允许本地 running child，我们要远程+历史，用 `hostId+nativeSessionId+parentRunId`） |
| **Task**（native 名 Agent/Task，同一族） | 子 agent 名 + 模型 + 状态 | `taskId`, `model?`, `status`, `childInstanceId?` | 网关模型名只会出现在 Workflow member，不会在 Task（用户已确认）。无 `childInstanceId` 时不可点进 `/s/:id` |
| **MCP** | `server/tool` + 参数 JSON | `server`, `tool`, `args`, `result?` | 未知 schema → Generic |

Generic：tool 名 + 折叠 JSON 入参/出参。

Diff 三分态文案：**拟修改** / **已写入** / **结果未知**。未知用虚线边，不用绿色勾。

**tool result 里的图片 block（[codex-cua.md](./codex-cua.md) §6）**

工具结果里合法带 `image` block（`ContentBlock.Image` + `MediaBlock{objectId,
mediaType, name}`，`protocol.md` §5.2），字节在对象库、不在 journal 里。渲染规则：

- 图片 block 在**工具卡内部**渲染成一张**有上限的缩略图**：`max-height` 写死在
  CSS 里（不随卡片高度变化），`loading="lazy"`，`alt` 取 block 的 `name`；
- **绝不自动展开**：不点不开、不放大、不进灯箱。想看大图是**点击打开**
  `/v1/objects/{id}`（与附件同一条路，`AttachmentChips.objectUrl`）；
- 一张工具结果里的多张图按 block 顺序排列，同样受 max-height 约束；
- **文本部分照旧**：`resultText()` 仍只返回文本，图片走新的 `resultMedia()`，
  不让任何既有调用方改形状——纯文本结果的外观与行为**逐字节不变**；
- 拿不到或过大的图已经在 Node 侧退化成一条文本 block（点名 media type 与字节数），
  所以 UI 不需要「图片坏了」的占位分支——它看到的就是一条文本；
- 不新增 `ObservationKind`，不新增卡片族：CUA 的调用按 MCP 族渲染
  （`server/tool`，`tool_name` 形如 `mcp__codex-computer-use__<verb>`）。


**审批卡（内联）**（出处口径见 D-052）

钉在 composer 上方，同时出现在收件箱（两路由同一张卡，见 §2.5 单壳）。字段：`interactionId`, `type=approval`, `title`, `preview`（命令或补丁**原文**，等宽 `<pre>` 呈现，不二次改写；超长以 `title` 给全文，摘要截断由 Node 侧完成）, `carrier`（harness 产出的工具/规则载体）, `expiresAt`（deadline；缺失画 `—`，**不**画「无限期」或 0）, `actions[]`（`id`/`label`/`effect`/`nativeValueRef`）。提交后按钮 disabled，直到 journal `interaction.answered` 或 `expired`。文案：「多台设备同时点，只记第一次。」

- **不显 confidence / risk 分**：协议 `ApprovalRequest` 没有 `risk` 字段（`web/src/types/generated.ts:98-106`），harness 不产校准风险分；编一个分值或「高/中/低风险」标签就是 §3.3 禁止的伪造出处。页面 UI 内不出现「置信度」「风险」字样（反向断言）。
- **不在 UI 里断言会话边界**：`DecisionOption` 只有 `id`/`label`/`effect`/`nativeValueRef`（`web/src/types/interaction.ts:4-8`），没有 `destination`；而 harness 的 `permission_suggestions` 实测同时包含 `destination: "session"` 与 `destination: "localSettings"`（后者写持久本地设置、跨会话存活），前端无法区分这两者——写「本会话内一直允许」就是伪造出处。按钮**保留并加粗** harness 已产出的原 `opt.label`（如 `Allow once` / `Always allow (acceptEdits)` / `Always allow this rule`，`crates/remuda-signal/src/decision.rs:69-74`；`AllowSession` 选项对 `request.suggestions` 逐条生成，`crates/remuda-signal/src/approval.rs:83-90`，一字不改），按钮下方以统一次要文字复述含义：「**按 harness 建议的范围持续允许**」——不加「本会话」「本次会话」「永久」修饰（反向断言）。若要给出真实范围词，需把 `destination` 提到 `DecisionOption`，那是新 wire 字段，与本批零迁移预算冲突。`allow-session` 的 quiet按钮样式 class保留，但标签**不得**写「静默通过」。

**AskUserQuestion 表单**

接管 composer（DSH `ui-user-questions`）：多题 pager；单选点完前进；多选 + 自定义；IME 期间 Enter 只上屏不上交。提交一次 `InteractionAnswer{id, items[]}`。Skip 单题；关闭 = cancel 整批。

**Workflow 树**

只展示身份和状态，不把 script/log 塞进节点（DSH `ui-workflow-run`）。完整 journal 放「原始事件」抽屉。native Workflow member **不是** runtime child Instance。仅当 `childInstanceId` 存在（显式 `instance.create`）才点进 `/s/:childId`；否则只展开本树。冷 child 用 jsonl 投影，composer 禁用除非 driver 能 resume。

三个时钟的唯一定义（行 tooltip 原文）：**用时** = `endedAt − startedAt`，运行中为 `now − startedAt`；**空闲** = `endedAt − lastProgressAt`，运行中为 `now − lastProgressAt`（stall 时用时照走、空闲照涨，不靠 member 的 `durationMs`）；**排队等待** = `startedAt − run.launchedAt`。缺输入一律 `—`。注意 `launchedAt` 是 **journal 登记该 run 的时刻**，不是它真正启动的时刻：Remuda 从磁盘发现（attach）一个已经在跑的 run 时，launchedAt 晚于真实启动，`startedAt − launchedAt` 会为负 → 排队等待显示 `—`（这是数据语义，不是 bug）。卡头 elapsed 因此取两者较大值：运行中 = `max(totals.elapsedMs + 距上次快照的墙钟, now − launchedAt)`，旧 Node 没有 `launchedAt` 时单独由快照外推继续每秒走针。phase 跨度 = 该 phase 最早 `startedAt` 到最晚 `endedAt`（运行中以 now 收口），没时间戳返回 undefined 显示 `—`。运行中的卡每 1s 自己走针（同时驱动卡头 elapsed、每-agent 用时/空闲），不加 fetch 轮询，卡折叠/结束后 interval 立即拆除。

**Usage / cost**

回合脚：`inputTokens` `outputTokens` `costUsd?`。缺任何精确字段 → 不显示合计。页头 cost 是会话累加，未知标「—」。

**Composer**

- 外壳：`--bg-input` 底，12px 圆角，1px `--border`，宽度与阅读列对齐（720px，停靠区内居中）；`:focus-within` 时边框变 `--focus` 并加 `0 0 0 1px var(--focus)`。
- 文本框：无边框，16px/24px，`--fg-body`，placeholder「输入提示词…」。行数：桌面 1–10 行，手机 1–5 行，键盘态 1–3 行。自增高用 CSS grid 镜像（`::after{content: attr(data-value)}`）实现，不读 `scrollHeight`。
- 桌面是两行：文本行 + 32px 工具行。控件 28px 高，12px `--fg-muted`，无边框；hover `--bg-hover`，打开时 `--bg-selected`。左：`attach-file` 与「粘贴附件」；中：`harness-chip`（静态文字）、`model-effort-chip`（显示「opus · high ▾」：切换中或排队中带 `--link`，与生效值不一致时带 `--attention-fg`，未知时显示「?」）、`context-chip`（圆环 + 「42%」）、`permission-chip`（danger 模式下仅 `--danger-fg` 字 + ⚠ 前缀，不加边框——带边框的 danger 触发器只属于 compact 单行的选项触发器，见下）；右：状态文字（`composer-queue-status`、`composer-interrupted-chip`、`composer-cap-note`）、忙碌时出现 `composer-steer` 和 `composer-interrupt`、发送按钮（32px 圆形，`--primary-fill` 底，里面是 ↑；`data-mode`、`data-holder` 不变）。composer 自身宽度 <600px 时先隐藏 harness 名，再把 context 收成只剩圆环。
- 顶部区只在有内容时出现：最大高度 112px（键盘态 36px），自身可滚动。排队行（`composer-queued-row`、`composer-queued-chip`，28px，带「插队发送」和 ✕）、附件块（40px，`--bg-inset`；上传失败边框用 `--danger-border`）、代码引用芯片；手机额外显示「尚未验证」（`composer-cap-note`，`--attention-fg`）。
- 桌面 ≥1024 时文本行下方显示说明（`composer-caption`），12px `--fg-faint`：「Enter 发送（工作中排队）· ⌘/Ctrl+Enter 插队 · Esc 打断 · 组字中 Enter 不发送」。
- 发送语义：桌面 Enter 发送（IME composing / keyCode 229 / key=Process 时忽略，抄 herdrx `Composer.tsx` `composing()`），Shift+Enter 换行；⌘/Ctrl+Enter 插队；手机 Enter 换行、主发送是按钮。不要抢中文候选。
- 权限芯片显示当前 `permissionMode`（dontAsk/acceptEdits/manual…），点开改本会话（发 Command，不是只改本地 chip）。
- Effort：收起为 compact 触发器，只显示**档名**（`ultracode ▾`），宽度固定不顶布局。点开是一张 ~300px 的 card popover（手机改 sheet），钉在触发器上方：
  - 第 1 行 grid：左闪电图标 · 中间档位名（18px `--fg-strong`）+ `›`（点开档位/模型列表）· 右复位图标。第 2 行居中 muted 型号（13px）。
  - 下方一条 40px 高的圆角 pill：左侧已填部分是中性轨道色，右侧未填是中性 surface；每个档位一个小圆点 marker，两侧都看得见；钮是 36px 圆 + 柔和投影，拖动吸附到点上。
  - **填充必须压到钮下**：fill 宽 = 钮心 + 钮半径（`--knob * 2 + pos * (100% - --knob * 2)`），fill 右端正好落在钮右缘、圆头藏在钮底下，钮左侧和钮下不留暗轨；第一档 fill 正好一个钮宽，同样不留缝。
  - 手机上触控 ≥ 44px 只靠**热区**，不靠视觉尺寸：pill 仍是 40px，外面套 48px 热区；图标按钮保持可见字形，用 44×44 的 `::after` 扩大命中面。
  - 最高档：整条 pill 换 `--ember-*` 余烬琥珀渐变，上面叠一层暖光 + 三层疏密不同的 ember 星点，各自以不同速度横向漂移（其中一层反向）并各自闪烁，钮带一圈呼吸的琥珀光晕；收起态触发器同频率轻微发光。只用 transform / opacity，不触发布局；popover 关闭即卸载，`prefers-reduced-motion` 下全部停成静帧（essential 动效按 visual-system §7.4 标记）。
  - 吸附 harness 原生档（claude `low/medium/high/xhigh/max` 加 `ultracode` workflow stop，codex `low/medium/high/xhigh/max/ultra`（显示 Low / Medium / High / Extra high / Max / Ultra；说明见 protocol.md effort 表），grok `low/medium/high/xhigh`）。`role=slider`，`aria-valuetext` = 档名。←/→/Home/End、触摸拖动、44px 触控。
  - `›` 展开的列表里才有档位说明和模型选择（`‹` 返回 pill）；pill 视图本身不列模型。
  - 变更走 `instance.configure`（journal + persist）。无档位或会话不可配置时禁用。不用档位芯片作第二套控件。
- 本地草稿按 `instanceId` 存（herdrx `composerDrafts`）；未 accepted 的乐观气泡可撤回。

**compact composer 边界（D-042，修订 §11.7 冲突「P0-3」；收起态单行 56px 不变）**

compact（§1.3 查询；56px 是 compact 布局行高，与指针无关）下 control bar 是单行 56px：`[选项触发器] [文本框 flex 1] [插队] [打断] [发送]`。

- **选项触发器**（`composer-options-trigger` / `model-effort-chip`）：可见 32px，13px 字，最宽 132px，显示「manual · high ▾」；coarse 指针下命中区 44（fine 指针下不撑大）。`danger` 模式时 `--danger-fg` 字、⚠ 前缀、1px `--danger-border`。触发器**必须同时显示两样东西**：当前 `permissionMode` 的**词**（`manual` / `acceptEdits` / `dontAsk` / `bypassPermissions` 的 label 或 native 词）**和** effort 的**档名**（上一条「只显示档名」的要求不因收起而豁免），形如 `manual · high`。`permissions.ts` 标为 `danger` 的模式（绕过全部 / 不再询问 / 完全访问）必须在触发器上就用 danger 样式可见——**把 bypass 态藏进 sheet 是本条最响的禁止项**。
- **插队、打断**：只在忙碌时出现。都是 32px 的 `.iconBtn`，间距 12；coarse 指针下命中区 44（`::after`），fine 指针下保持 32；aria-label 分别为「插队发送」「打断」；testid 分别为 `composer-steer`、`composer-interrupt`。**发送**：可见 32px 的圆，coarse 指针下命中区 44。
- **可以进 sheet**（D-042 允许）：附件、粘贴、harness 只读信息、context 用量、权限选择器本身、effort 滑杆。
- **不得进 sheet**（留在 sheet 外，D-028a）：发送/排队/打断**三态**按钮、队列 chip、能力未验证时的「尚未验证」标注。三态是当回合的事实，`unknown` 是诚实的欠缺，两者都不是「选项」。
- placeholder 分平台：手机「输入提示词…」（不写桌面快捷键——手机上 Enter 是换行、⌘ 不存在，写快捷键是误导）；桌面保留快捷键说明。
- 确认类交互：插队与 Esc 打断**不再用 `window.confirm`**（Safari 上会盖住键盘，且样式不可控），改用 `Sheet`（桌面 `popover`，手机 `sheet`，焦点圈定、Esc = 取消、返回焦点到触发器）。确认后的命令语义与 `commandId` 路径**完全不变**。
- 键盘收起时底部内边距取 `max(6px, env(safe-area-inset-bottom))`。

**状态清单**

`loading-snapshot` → `live` → `reconnecting` → `gap-backfill` → `readonly-stale`（`connectivity ≠ connected`）。`blocked`（`activity=waiting-interaction`）时 composer 换成 interaction 表面。`exited` 时 Composer 不挂载、挂 EndedBar（见上；若 `capabilities.resume`）显示续接，否则显示「开新会话继承 cwd」。`--bg` 空闲保持 `idle`，不要显示「已完成」。

### 2.3 会话页 · 终端视图 `/s/:instanceId/tty`

pty-backed 会话（kind `terminal` / driver `shell-pty` / `generic-pty` / `claude-pty` / codex·grok·agy）打开 `/s/:id` 即终端。结构是第二视图 `/s/:id/structured`。

```
┌  [终端 | 结构]    80×24  raw  webgl  [本地输入|直连] [esc tab ctrl alt ↑ ↓ pgup pgdn ctrl+c] [全屏]
├─────────────────────────────────────────────────────────────────────┐
│  xterm.js · fit · WebGL/canvas · Unicode11 · TERMINAL_THEME · 标准 256 │
│  原生 TUI（vim / grok / claude 全屏，鼠标跟踪开启时点击进 PTY）        │
│                                                                     │
├─────────────────────────────────────────────────────────────────────┤
│  raw：击键 / 粘贴 / 鼠标序列直接进 PTY                                │
│  keys：本地输入条（手机默认；IME composing 不送）                      │
└─────────────────────────────────────────────────────────────────────┘
```

**真·全屏**：隐藏 chrome，`position:fixed; inset:0; height:100dvh`，`env(safe-area-inset-*)`。

**恒深色仪器（D-053）**：终端在深浅两态下相同，不订阅外观变化。pane 与 viewport 背景都用 `var(--term-bg)`，`TerminalView.module.css` 不保留色值字面量；xterm 用 `TERMINAL_THEME`（基础色、16 ANSI 与对比度见 [visual-system.md](./visual-system.md) §3.4/§4：background `#1a1917`、foreground `#e4dfd6`，每个非黑 ANSI 色 ≥ 4.5:1）；标准 256 色立方不再染色，终端输出的颜色不改写。

**从 herdrx 抄交互与 viewport 算法，重接 runtime API**（不要搬 herdr snapshot 绑定）

| 能力 | 算法来源 | 接到 |
|---|---|---|
| xterm + fit + WebGL/canvas + Search/Unicode11/WebLinks | `TerminalPane.tsx` | Hub `/v1/follow?tty=1` binary `tty.frame` |
| 恒深色调色板 | `tty/theme.ts` `TERMINAL_THEME`（标准 256 色立方） | xterm `ITheme` |
| 手机默认 keys、桌面默认 raw | `WorkbenchPage.tsx` | follow 输入 channel |
| IME 安全发送 | xterm composition + `composing()` | raw onData 不拆候选 |
| 鼠标 | xterm mouse tracking (`onData` + `onBinary`) | 应用 DECSET 1000/1002/1003/1006 时转发 |
| 辅助键条 | `terminalTouch.ts` + `AuxKeys` | esc/tab/ctrl/alt/arrows/pgup/pgdn/ctrl+c |
| `visualViewport` 缩键盘、pinch zoom 不 reflow | `displayPreferences.ts` | 本页 layout |
| fit/fixed/responsive 字号 | `terminalFit.ts`；手机默认 `responsive` | `tty.resize {cols,rows}` |
| 断线：终端保留最后一帧，标 reconnecting | follow 重连 + snapshot replay | Instance.connectivity |

**不要抄** DSH `TerminalBlock`（剥了绝对光标、清屏、alternate-screen）。

**外框、工具条与键盘态（D-053）**

- 桌面工具条高 32px；TUI 模式标识 12px 胶囊；**过期提示「⚠ 画面可能过期」（`StaleScreenBadge`，`data-testid="tty-stale"`）是新鲜度未知而不是失败**：链路暂时取不到实时附着时用 `--unknown-fg` 中性色 + 虚线标记 + 说明文字（带上一帧年龄与原因 token），不用琥珀；区分 `node-link-unavailable`（链路不可达、会话可能仍在远端运行）与 `instance-gone`（实例确已终结，走结束态/Resume 语义，不画成过期）。进度条 2px。
- 辅助键条 `AuxKeys`：按键高 28px，12px 等宽字。
- 本地输入 `LocalInput`：输入框高度统一——fine 指针下 36px（`--control-h-lg`），coarse 指针下可见高 44px；字号用 `--text-input`（coarse 指针下 16px，不再是 15px；fine 下 14px），提示文字 12px（不再是 10.5px）。手机键盘条 `PhoneKeyBar` 仅在 coarse 指针设备渲染：按键 44×44，13px 等宽字，整条横向滚动。
- **键盘弹起时冻结行数，不发 resize**：`html[data-keyboard="1"]` 期间 fit 检测到该属性就直接返回——不调 fit、也不发 `sessionRef.resize`；`.viewport` 设为 `min-height: 0; overflow: hidden; display: flex; flex-direction: column; justify-content: flex-end`，xterm 保持键盘弹起前的尺寸、贴底显示，上方溢出部分被裁掉，本地输入与键盘条始终在可见带内。键盘收起后正常 fit；行数不变时不发 resize（开合前后 PTY 行数相同是回归断言）。非键盘态 `min-height: 240px` 维持不变。

**数据 / 线协议**

- 输出：`/v1/follow?tty=1` 上已有 32-byte `tty.frame` envelope，channel = output，payload = 原始 ANSI。
- attach：打开 follow 且 `tty=1`，先吃完整 snapshot replay，再跟 live 帧。
- 输入：同一 binary envelope，channel = input，payload = xterm `onData` + `onBinary` 的原始字节（含鼠标序列）。
- resize：JSON `tty.resize {cols, rows}`。
- TTY 字节**不**进 transcript 节点。结构化卡片仍在「结构」tab 看同一 journal。
- attach 失败：回 structured + toast。

**何时自动切到终端**

`autoRevealTty` **默认关**，M3 才考虑打开（审查 D20）。打开后仍须同时满足：

- `capabilities.ttyAttach=true`
- `mode=tty-attachable`（`claude-bg` 或 `claude-pty`；**print 没有终端 tab，也不会冒出 artifact 事件**）

触发（M3）：

1. `capabilities.artifact=supported` **且** journal 出现产物节点（print **没有** Artifact 工具，`claude-control-plane.md` §5；Artifact 不是 M0 验收）。
2. 用户从结构化视图点「打开 /workflows 面板」——发 TTY 键序列前先切过去（不要在 structured 里假造 Workflow TUI）。
3. `driver=claude-pty` 且尚无任何 structured message——避免空白 transcript。

**不**因为 Bash 输出 ANSI 就切走。用户可钉在 structured。

Bot / `claude-print` 实例没有终端 tab。Artifact 产物页走订阅登录的 `claude-pty` profile，网关混合模型会话保持 print。

---

### 2.4 新建会话 `/sessions/new`（手机 30 秒）

不要五步向导。**一页 sheet**，默认全填上次成功值。

```
┌ 新建会话                                      ✕ ┐
│ 提示词（第一焦点）                               │
│ ┌────────────────────────────────────────────┐  │
│ │ 查 bolt TaskManager spill…                 │  │
│ └────────────────────────────────────────────┘  │
│ 主机   [devbox ▾]     在线 · claude 2.1.268 │
│ 项目   [sfe-root      ▾]   /home/…/sfe-root      │
│        （Workspace）         新 worktree □       │
│ 运行时 [Claude ●] [Codex] [Grok] [agy] [Terminal]│
│ 模型   [passthrough/auto_model/… ▾]              │
│ 权限   [询问 ●] [可改文件] [全自动]               │
│                    [取消]  [开始]                │
└─────────────────────────────────────────────────┘
```

手机打开即聚焦 textarea；主机/项目是大触控下拉，最近 5 个置顶。30 秒路径：打开 → 打字 → 开始（其余已预填）。**手机不展示驱动选择，固定 `claude-print`。**

桌面高级（折叠，M3 才启用入口）：

- 结构化 print（默认）→ `driver=claude-print`，`mode=structured-only`
- 后台可唤醒 → `driver=claude-bg`（**不是** pty；`--bg` 忽略 `--session-id`）
- 需要 `/workflows` 面板 / Artifact TUI → `driver=claude-pty`

**字段 → `InstanceSpec`**

| UI | 提交 |
|---|---|
| 主机 | `hostId` |
| 项目 / worktree | `workspaceId`, `cwd`, `worktree?` |
| 运行时 | `kind=claude\|codex\|grok\|agy\|terminal`；`terminal` → `driver=shell-pty`（cwd/worktree，prompt 可空） |
| 模型 | `model`（网关名原样；禁止分类器式裸 `claude-sonnet-5`）。网关混合模型走 print；Artifact/TUI 用订阅登录 profile + pty（M3） |
| Provider | `providerProfileId`（M0–M2 默认 `astergate-default`） |
| 权限 | `permissionMode`：询问=`manual`+`permission-prompts host`；可改文件=`acceptEdits`（仅本 cwd 覆盖）；全自动=`dontAsk`（个人遥控默认不要） |
| 提示词 | `prompt`；空 prompt + pty = 只 attach 空 TUI（M3） |

校验：主机 `connectivity`/offline 禁用开始；cwd 不在该主机 Workspace 表则先「登记项目」。开始后进 `/s/:id`，顶栏 `starting`，失败停留本页并显示错误。

高级（折叠）：`--settings` overlay、max budget。`CLAUDE_CONFIG_DIR` 与 `--bg --name` 不进 M0 表单。禁止同一 Instance 从 print 热切 `--bg`。

---

### 2.5 收件箱 `/approvals`

桌面与 compact 用同一个壳：`InboxShell`（`mode` 由父路由传入，不消费 viewport hook），桌面路由 `/approvals`，compact 路由 `/m/inbox`（重定向与 query 保留见 §1.2、§4.7）。

```
桌面 /approvals                                  compact /m/inbox
┌ 收件箱                              待处理 4 ┐      ┌ 收件箱 52              [3] ┐
│ [全部|审批|提问|计划]（.seg, radiogroup） 主机/Workspace ▾ │      │ [全部|审批|提问]（全宽 .seg）     │
│                                                 │      │ （推送横幅，一行 44）             │
│ ● 12:04  bolt / sfe-root / claude        来源…    │      │ ● 审批卡（左右 16px，两档）…       │
│   Bash  rm -rf /tmp/coord-media   （原文 <pre>）    │      └─────────────────────────────┘
│   来源 harness · 截止 12:13 · 位置 sfe-root          │
│   [允许一次]  [拒绝]                                │
└─────────────────────────────────────────────────┘
```

- 页头面包屑「收件箱」（原「审批中心」），右侧显示「待处理 N」；compact 页头 52px，计数挂 `m-inbox-pending-count`。
- 类型筛选两侧都是 `.seg` + `role=radiogroup` + roving tabindex：桌面一档，compact 全宽；项可见 26px，coarse 下热区 44px。
- 桌面保留主机、工作区芯片（query `host` / `workspace`）；compact 保留推送横幅（一行 44px）。
- 队列：桌面最宽 720px、分三档；compact 左右 16px、分两档（§4.7）。列表不翻页。

**状态**

| UI | 数据 |
|---|---|
| pending | `Interaction.status=pending` 且未过期；状态行文案「等待你的选择」，用 `--attention-fg` |
| answering | 本设备已 POST，等 `accepted`；状态行「已提交 · 等待确认」，用 `--link`；提交后按钮保持禁用，直到结算完成（`m-inbox-submitting`） |
| settled | journal `interaction.answered`；从待处理移除，可在会话内看到终态卡 |
| expired | 超 TTL（建议 10–15 min，短于飞书 token 30 min，`bot-dispatcher.md` §3.2） |
| superseded | 别的设备先答；本设备按钮变「已在 Mac 处理」 |
| paused | Node Agent 断线；禁止点，文案「主机离线，交互暂停」（`m-inbox-paused`） |

**字段** `interactionId, instanceId, hostId, type, title, preview, carrier?, createdAt, expiresAt, answeredByDeviceId?`

**决策卡（`approval-row`）**：surface 底，1px `--border`，8px 圆角，内边距 20px 24px（手机 16px）；`?focus=` 命中的卡加 2px `--focus` 环。从上到下：

1. **上下文行**：12px。
2. **状态行**：见上表。
3. **标题**：17px/600。
4. **原文**：`<pre>`，等宽 13px/1.6，`--bg-inset` 底，最高 16em；`preview` 是命令/补丁**原文**，不二次改写；`expiresAt` 缺失画 `—`。
5. **事实 `<dl>`**：来源、截止、位置三行——来源缺失写「未知」，截止缺失写「—」；**不显示** risk、置信度、「本会话」（出处口径见下与 D-052）。
6. **动作**：按钮用 `.btnLg`；允许类用 `.btnPrimary`，拒绝用 `.btn`。按钮标签就是 `opt.label` 原文，所以「允许一次」等字样与 testid 都不变；allow-session 下方加一行「按 harness 建议的范围持续允许」。手机上前两个选项并排放。提交后按钮禁用直到结算完成。
7. **兜底**：选项不可回答时显示「请打开会话查看完整终端提示」。

`preview` 是命令/补丁**原文**（等宽呈现，不二次改写）；`expiresAt` 缺失画 `—`。审批卡不显 risk、不断言会话边界，完整口径见 §2.2「审批卡（内联）」与 D-052。

多设备：Hub 只接受第一次有效 `commandId`。所有打开此页的客户端靠 journal 更新，不要本地抢锁。Push：`tag=interaction:{id}`，点通知进 `?focus=`。

AskUserQuestion 不在列表里填完（题太长）；「去回答」进会话页表单。Allow/Deny 可在列表一键完成。

**QuestionForm、ElicitationCard、PtyQuestionAnswers**：套用同一张卡片外壳；选项行在 fine 指针下 36px，coarse 下 44px。

**单壳口径（InboxShell，D-052）**

- 两个路由渲染同一个 InboxShell（`mode="desktop" | "compact"`；mode 只由父路由 pageshell 决定，**不消费 viewport hook**）+ 同一张 `ApprovalCard`（字段口径见 §2.2，此处不重复）+ 同一个 kind 分段原语（`role=radiogroup` + roving tabindex，`pointer: coarse` 下每项命中区 ≥ 44px；fine 指针下保持 26px 可见密度，§3.4）。`?focus=` 高亮滚动、`?kind=` 过滤、`ApprovalCard` DOM 结构在两侧一致；既有 `approval-row` 等 testid 不改名。
- **两侧档位各自保留，不强制一致**：桌面保留三档（待你处理 / 进行中·最近 / **已离队**——即上面线框的「过期」行与 superseded / paused 行）+ 主机 / Workspace 过滤芯片（query `host` / `workspace`）；compact 保留两档（待你处理 / 进行中·最近，**没有**已离队第三档，`web/src/features/mobile/inboxRows.ts` 的既有决定）。档位组成由 `mode` prop 决定，**不是**静默删掉任一方行为。D-049 关于会话本体不分叉的口径（D-049 决策段与背景段：会话本体永远是共享的 `/s/:instanceId`、不做第二份实现）约束壳与卡控，但不强制两面的信息架构相同（两面线框本来就不同，§4.7）。
- 单壳同时消除今天的两份 a11y 债：桌面旧 kind 分段没有 radiogroup role、compact 旧列表声明了不存在的 `tabpanel`——收敛后全应用只剩一份正确的 radiogroup。
- compact 重定向口径不变（§1.2 重定向表、§4.7：`/approvals` → `/m/inbox`，query 原样保留）。

### 2.6 主机页 `/hosts` · `/hosts/:hostId`

```
┌ 主机                              [添加]              ┐
│ ● devbox     在线  12ms   claude 2.1.268  会话 4 │
│ ○ devbox-sg  离线  —      —               会话 1 │
│ ● forge-doloris   在线         claude · grok          │
└───────────────────────────────────────────────────────┘

详情：
  传输  outbound-wss  （开发机可 ssh-dev）
  Node Agent  v…  启动 2h  负载  cpu 8% mem 31%
  CLI   按本机 `cli[].path`+`version` 列出，不假设与 Mac 同版本
        claude  …/claude  2.1.268   登录 unknown
  Workspace  最近 cwd …
  [新会话]
```

**不要**复用 herdrx `transport: local|ssh|tailcat`，也**不要**用 herdr paneId 当 instanceId。无「打开 herdr 工作台」按钮。

| 字段 | 说明 |
|---|---|
| `hostId, name` | |
| `transport` | `outbound-wss`（生产 Node 出站）\| `ssh-dev`（开发期 SSH 转发）。无 tailcat |
| `hostname, port` | `ssh-dev` 才展示 |
| `online, lastSeenAt, rttMs?` | 心跳（Hub 所见 Host.state 的投影） |
| `agentVersion` | Node Agent |
| `resources?` | cpu/mem，缺则不画仪表 |
| `cli[]` | `{kind, version, path, auth: gateway-native\|logged_in\|logged_out\|unknown, installed: boolean}` — **绝对路径+版本**，按主机盘点。Claude `gateway-native` 只报布尔，不含 token。`installed` 是探针自己的判断（`path` 存在与否），与 `path` 是否为空**不是**同一件事：一台没装某 CLI 的主机仍可回报该行并带 `installed: false` |
| `instanceCount` | |

**`computer-use` 行（D-045 / [codex-cua.md](./codex-cua.md) §3.4）**

`computer-use` **就是一行普通 CLI 行**，不是新组件、不是新徽标、不是新面板：

- `kind: "computer-use"`，`path` / `version` / `installed` 按上面同一张表渲染；
  `auth` 恒为 `unknown`——Remuda **刻意不探测**这个 vendor 的登录态；
- `installed: false` 渲染为「**未安装**」，与其它 CLI 同一套措辞。
  **这条要求改一处现有代码**：`web/src/features/hosts/model.ts:85-87` 的
  `installedCli` 过滤是 `Boolean(entry.path || entry.version)`——一条
  `installed: false` 的行既没有 `path` 也可能没有 `version`，**今天会被这行直接丢掉**，
  于是「未安装」永远画不出来，只会静默消失（而 §3.3 要求它与「未上报」是不同的画法）。
  过滤条件必须改读 `installed`（`installedCli` 的语义本来就是「装了的那些」，
  未安装行需要另一条渲染路径）。该代码归 `c-cua-hostcap`；本节只写规格。
- **这一行缺席**（老 Node 不上报）渲染为「**未上报**」，**绝不**渲染成
  「不支持」——这正是 §3.3 的规则在这个字段上的落点：没有数据 ≠ 否；
- `cliSummary` 的 `<kind>-cli ` 前缀剥离（`web/src/features/hosts/model.ts:89-101`）
  必须让 `computer-use` **存活**：`kind` 为 `computer-use`、`version` 为裸版本号
  时它不该被吞掉。测试钉住这一行；
- 它**只**表示「这台机器上有没有」，**不**表示「本会话有没有被授予」——授予是
  按会话的事（D-045）。主机页不画会话级状态，也不提供授予开关（本批
  `/sessions/new` 上没有 CUA 开关；那是后续）。

添加主机第一阶段：登记 Node 出站身份；开发期可填 SSH config 名。Tailcat 不做。

离线主机：列表仍可看历史会话（journal 在 Hub），不能 create/send。

---

### 2.7 Provider 页 `/providers`

```
┌ Provider                                           ┐
│ ● astergate-default   anthropic-messages           │
│   endpoint · 健康 200  12ms                        │
│   轮换：发生在 AsterGate，不在 runtime              │
│   data-plane key  ****（前 4 位）                  │
│   模型 discovery 开 · 最近错误 —                    │
└────────────────────────────────────────────────────┘
```

**M0–M2：只展示 `astergate-default`。** 不画 Direct 多 key、权重、冷却。账号池 CRUD 不在本页；需要时外链 astergate 管理 UI。

- 轮换权威在 AsterGate。runtime 只持一把受限 data-plane key。
- Claude 启动物化成临时 `--settings` JSON；模型名 `passthrough/…` 原样。
- Direct 多 key UI 标 **v2**，本规格不实现。

字段（M0–M2）：`profileId=astergate-default, protocol, baseUrl, health{ok,latencyMs,checkedAt}, secretRef`（脱敏）、`models[]`, `lastError?`。

健康红点不自动切会话中的 key；UI 只提示「新会话将避开不健康 profile」。

---

### 2.8 Bot 页 `/bots` · `/bots/:channelId`

```
┌ Bot                                                         ┐
│ 飞书  Doloris-runtime     长连接 ●  白名单 1 open_id         │
│       会话键  feishu:{chat_id}:{thread_id\|\|root_id\|\|main}│
│ Telegram  @…              polling ●  白名单 1 chat_id        │
│       会话键  tg:{chat_id}:{message_thread_id\|\|0}          │
├─────────────────────────────────────────────────────────────┤
│ 绑定                                                        │
│  通道 [飞书]  profile [lark-cli oncall-helper]               │
│  owner_open_ids  [ou_…]   chat_allowlist  [oc_…]            │
│  默认主机 [bolt] 默认项目 [sfe-root] 默认 driver print      │
│  TTL 4h   群策略 仅 @bot                                     │
│  最近投递  12:04  甘露寺  → 实例 abc  accepted               │
└─────────────────────────────────────────────────────────────┘
```

字段来自 `bot-dispatcher.md` §3.1 / §1.5：命令 `/new /host /agent /model` 只是 IM 侧，本页展示默认路由和 allowlist。不在 UI 里贴 app_secret。

映射表（只读）：最近 N 条 `session_key → instanceId`，点进会话。

独立 dispatcher app，不和会议纪要共用连接（dispatcher 报告建议）。本页不提供「挂到现有 hub」开关，除非待决策拍板。

---

## 2.9 任务列表与看板 `/board`（D-050，2026-09-20）

消费 Hub Task 台账（`GET /v1/board?project=` 与 task 列表），把任务而不是会话作为左栏/看板的卡片单位。机械规则（状态→列投影、lease、refcount、迁移预算）见 [task-model.md](./task-model.md)；本节只定界面。

**布局（D-053）**：侧栏 ｜ 任务清单索引列 ｜ 看板主区。索引列 `--bg-nav` 底，宽 `--index`（≥1200 为 272，1024–1199 为 240）；<1024 收成页头上的「清单」按钮，点开是同一个组件的覆盖层。看板主区上边距 24px、左右 32px（<1200 为 16px），页面本身不横向溢出。每页一条 48px 页头：面包屑「任务看板 / {项目或全局}」，右侧「{n} 个任务」与 `board-archive-toggle`；旧的 12px 标题行删除。`board-move-error` 是一行 13px 的 `--danger-fg` 文字，前缀 ⚠。

**桌面线框（看板）**

```
┌ 侧栏 ─┬─ 任务清单（272/240）─┬─ 任务看板 / 全局              {n} 个任务  [归档过滤] ─┐
│       │ [搜索 Task……] 36     │  待办             进行中             已完成         │
│ 会话  │ 需要你 · 2            │ ┌───────────┐  ┌───────────┐  ┌───────────┐ │
│ 收件箱 │  ▸ SE-03 适配…  2    │ │SE-05 ⚠    │  │SE-02      │  │SE-01 a1b2…│ │
│ 看板  │ 项目 remuda ▾        │ │修 spill    │  │worktree 池 │  │ 1 session │ │
│ 项目… │  ├ SE-07 子任务      │ │claude ●    │  │2 sessions │  │           │ │
│       │  └ SE-08 子任务      │ │2 session   │  │default    │  │ 已合入     │ │
│       │  已归档 · 3          │ └───────────┘  └───────────┘  └───────────┘ │
└───────┴──────────────────────┴──────────────────────────────────────────────┘
```

**任务清单（索引列；compact 形态见 §4.7）**

- **首组「需要你 · N」**：有待人类 Interaction 或 blocked 待所有者处理的 task 置顶，用 attention 角色色；关联既有 Interaction 收件箱（`/m/inbox`）与 space 的 `blockedCount`；这是 attention/inbox 聚合，区别于原始 blocked 计数。
- 其余按 **project + git branch** 分组；组内按 **`parentTaskId`** 嵌套父子任务——父行显示 `▸ N` 子计数，子任务嵌在父下，与 project/branch 分组并存。
- 每行在 coarse 指针下最小高 44px（fine 指针下用 `--control-h-sm` 28px 密度）；派生 **`SE-nn`** 人读 key（project 内顺序派生，display-only，无 schema 改动；不显示裸 `tsk_…`）、标题、session 数、一句下一步；归档 task 折进「已归档 · N」。选中行用 `--bg-selected` 底，加 2px `--focus` 左竖线。
- 顶部搜索框高 36px（coarse 下 44px），复用既有会话过滤机制，不新造搜索栈。
- 任务清单数据复用 Shell 已加载的 `useProjects()`，不再为项目名单独轮询。

**看板列与卡片**

- **三列**：`repeat(3, minmax(240px, 1fr))`，列间距 20px，列本身不填底色、没有外框；列头高 32px：形状图标（○ ◐ ✓）+ 13px/600 标签 + 计数。放不下时列在 `.columns` 内横向滚动，页面本身不横向溢出；任何工具条都不横向滚动、不折两行，放不下的项进 ⋯。已归档是过滤器（或折叠组），**不是第五列**，归档不改 task state。
- **列投影（只读，权威是 8 态机）**：待办 = pending/placed/deferred/parked（+ failed 且无 placement）；进行中 = running/stalled（+ failed 且有 placement）；已完成 = **只有 done**。
- 拖拽中：可放下的列用 `--bg-hover` 底 + 虚线 `--focus` 框；不可放下的列整体 opacity .55，列头下方显示 `board-drop-reason`。
- **卡（`board-card`）**：surface 底，1px `--border`，8px 圆角，内边距 12px 14px。自上而下：
  1. 元行：等宽 SE 键，加归档按钮；
  2. 标题：14px/500，最多两行；
  3. **信号行**，按以下顺序取第一个成立的：
     - 失败：显示 `board-failed-badge`，卡片左侧加 2px danger 竖线（failed 仍按 `placement` 归位，不单开列、不折进已完成；`blockedReason` 可见，角标带形状与文案，不只靠颜色）；
     - 需要你：琥珀圆点 +「需要你处理」；
     - 已合入：`{sha7}`，用 `--success-fg`；
     - 尚未合入：用 `--fg-muted`，**不给 land 入口**；
     - 以上都不成立：显示 `taskNextStep()` 派生的下一步（不新造状态机、不猜成功，口径同 §2.1）。
  4. 会话行：最多两个 `board-session`（harness 字形 + 相对时间，复用 tab 渲染语义），多出的写「另有 N 个会话」；
  5. 页脚：左 `board-config`（当前 profile 的 `default` 配置标签，点击回落 composer 模型/权限三元组，无新存储；「与 N 个 task 共用」的 lease refcount / 「独占目录」语义保留在此），右 `board-shared`。
- **拖卡合法性由当前 state 决定**：UI 用状态机的合法迁移预计算每卡可达列，不可达列在拖拽时禁用并给提示（不是静默拒绝，也不是松手后 4xx 才报错）；待办→进行中对 pending/deferred 卡走多跳（pending→placed→running），每跳都是合法 PATCH，任一跳非法整体复位。进行中→已完成同理：只有 running 卡能单跳 → done（状态机无 stalled→done 边），**stalled 卡走 stalled→running→done 多跳**；路径一律由 `can_transition_to` 计算，不发非法单跳。已完成列上的卡**不暴露 land 动作**——land 只走 gate；卡进已完成列不解锁依赖（I1）。
- **门控按 grant 动词，UI 不假设「agent 一律禁写」**：拖卡发 `PATCH /v1/tasks/{id}`、归档发 `POST /v1/tasks/{id}/archive`，Hub 按 `GrantVerb::Dispatch` 门控；持 grant 的协调员 agent 可写，无 grant/越 scope 收 403，UI 照常渲染拒绝理由。

**只读预览（覆盖式抽屉，D-053）**

从卡片点开的详情不再是固定右栏，而是覆盖式抽屉：宽 `min(400px, 100% - 48px)`，`--bg-raised` 底，`--shadow-3`；Esc 关闭，焦点回到触发的卡片。顶部横幅「预览模式」+ 按钮 `board-preview-open`（「在工作台打开」，跳共享 `/s/:id`，镜像归档会话的只读态）。标题（`task-detail-title`）18px/600；正文渲染 mandate/title/blockedReason；`task-detail-mandate` 与 `task-detail-open-session` 行为不变；composer 禁用。批注锚点 `①` 选中此正文或 transcript 生成，**只读预览不可批注**。项目空间/任务空间的文件与变更在打开的工作台会话里看（共享 `/s/:id/files`：项目空间 = worktree 全树，任务空间 = 按 task 的 session集合 + `owns[]` glob 的客户端过滤投影，空态「还没有文件」，无新端点，非 git/离线/权限不足透传六可用性状态）。

**批注（composer 草稿，零 wire）**

- 两级载体：卡片级（挂 task 卡）/ 消息内锚点（选中详情或 transcript 正文生成 `①`）。
- composer 停靠区徽标「本次发送带 N 条批注」（§2.2 的 `annotation-badge-row`）；发送时批注作结构化前缀拼进 prompt，发送后清零；终端段不提供批注。

**项目切换器**：只在 `/projects` 页头的动作区（`ProjectSwitcher`，`navigateOnSelect`，testid 不变）；侧栏项目区的行是桌面的另一个范围入口，二者写同一个 projectId 过滤偏好；全局不过滤。`Project.members[]` 映射到 Space（复用 `(hostId, workspaceId)` 键，D-024）；`/projects` 页读 `Project` 实体，不再是 Workspace 通讯录 stub。

**手机（compact）**：不渲染三列看板；`/m` home 在 project+branch 组上叠 task 分组，看板需求由单列分段过滤承载（待办/进行中/已完成/已归档，一次一段，§4.7）；任务空间/项目空间在共享 `/s/:id/files` 内作 tab；批注在手机只读，建锚点桌面优先。会话本体永不分叉（D-049）。

---

## 3. 实时性与状态语义

### 3.1 两路数据，不要混

| UI 状态 | 权威 | 例子 |
|---|---|---|
| transcript 节点、tool 卡、Workflow 树、usage、interaction 终态 | observation journal `seq` follow | 消息、call/result、wf member |
| Instance `lifecycle`/`activity`/`connectivity`、主机 online、CLI 版本、capabilities | Hub registry + Node 心跳 / `capabilities()` | 列表点（§2.1 投影）、终端 tab 是否存在 |
| 本设备 composer 草稿、侧栏宽度、主题、`autoRevealTty`（默认 false） | 设备 localStorage | 不跨设备 |
| last-read seq、推送订阅 | 按用户+设备存在 Hub | 未读、badge |

`capabilities` 快照在 create 时写入 Instance，UI 按它藏按钮：无 `ttyAttach` 或 `driver=claude-print` 无终端 tab；无 `resume` 无 Resume；无 `structuredWorkflow` 则 Workflow 卡降级 Generic（仍可有 tool 名 Workflow）；无 `artifact` 则不因产物切 tty。**不要**用「曾经见过某种事件」反推能力。print 会话不会出现 Artifact 工具。

### 3.2 Follow / 断线 / 补页

抄 DSH history：先订阅增量，再下 snapshot，再放缓冲，避免洞（`deepseek-harness.md` §3.3）。

| 连接 UI | 行为 |
|---|---|
| 首屏 | 骨架屏直到 snapshot `asOfSeq` |
| live | 顶栏小圆点；新事件 append |
| reconnecting | `ConnectionIndicator` 样式（DSH primitives：断开 / 每 500ms 点点 / 已恢复）。TTY 冻最后一帧。composer 可打字，发送进 `queued`，**不**在重连成功时自动再 POST |
| gap | `lastSeq+1` 对不上 → 请求 `[fromSeq, snapshot.seq]` 补页；期间该会话标「正在补事件」，tool 卡不结算 |
| 补页失败 | `readonly-stale`；允许打开终端（若 attach 仍活）；禁止把 status 画成 done |
| 多设备 | 同一 journal；本机 queued 命令只在本机显示发送中，直到 accepted |

控制：queued 气泡带 spinner；accepted 显示原生 id；settled 对齐 journal。超时 unknown：**不**重发，提供「仍要再送一条？」显式按钮。

### 3.3 「不知道」怎么画

envelope `completeness`（`deepseek-harness.md` §8.3）：`structured` / `partial` / `screen-derived`。

| completeness | UI |
|---|---|
| `structured` | 正常卡 |
| `partial` | 虚线边 + 「不完整」；Bash 无 exit 不标成功；diff 无 result 用「结果未知」 |
| `screen-derived` | 标签「来自屏幕」；只出现在 thought/status 启发式；**不能**生成审批 ID 或 Workflow member |
| 无法识别 | `opaque` 一行，**行首带 `seq N · <source>`**（等宽，置于 kind 前），点开 raw JSON；不参与 Compact 折算成功 |

**折叠不得吞掉 completeness（D-052）。** partial 卡被 compact 折叠行（§2.2 `FoldedToolRow`）收起后，虚线边 +「不完整」标记必须**原样活在折叠行上**——一张 partial 卡折起来后 operator 仍要能判断结果不完整；折叠态与展开态的 partial 标记同时存在或同时不存在。completeness 仍只有上表三值，**不新增第四种**。D-041 的硬顺序（折叠分支在 family 判定之后；Workflow / error / interaction 豁免）原样保留，由实现批次的回归断言钉死（settled Workflow 卡 compact 下 `data-folded="0"`；running Workflow 卡头 elapsed 每秒递增）。**本批不把 completeness 推到 interaction transcript 节点或审批卡**：那条数据通路不存在（interaction 节点不经 transcript 节点分发；审批卡的数据源是 hub 的 interactions 列表而非 transcript 节点，`Interaction` 类型上既无 completeness 也无回指 observation 的 eventId），要接通需先做 store / assemble 连接键调研，且可能要求新 wire 字段——按批次计划 D11 的默认结论，本批不做，只做 `FoldedToolRow` 这完全可落地的一半。

**无法识别的事件也要可溯源（D-052）。** opaque 节点携带 envelope 上**既有**的 `seq` 与 `source`（零新 wire；现状 `assemble.ts` 构造 opaque 节点时把两者丢掉，由实现批次补上），折叠行 summary 显 `seq N · <source>` 前缀，raw JSON 仍折叠在 details 内。opaque 依旧不参与 Compact 折算成功、依旧禁止当终态。

禁止：把 PTY 里看到的 “✓” 写成 tool ok；把 `logs` ANSI 录像当 transcript（`claude logs` 是 PTY 转储，`claude-control-plane.md` §1.4）。

流式 Markdown：已完成 block 冻结，开着的 fence 按行高亮（DSH `MarkdownText`）。崩溃发生在 settlement 前：保留临时行并标「未落盘」，不要改写成完整 assistant/message。

### 3.4 命中尺寸与辅助文本字号（D-039；D-053 改为按指针判定）

这两条是全局口径，适用于本规格所有屏（尤其 §2.2 / §2.3 页头与 composer），机械取值见 [visual-system.md](./visual-system.md) §5.2/§6.2/§8。

**命中尺寸一律按 `pointer: coarse` 判定，不按宽度。** 旧实现在 `@media (max-width: 767px)` 里把按钮整高撑到 44px——窄桌面窗口会命中宽度查询但没有触屏，按宽度砍密度/加高度都会误伤鼠标用户（`web/src/lib/viewport.ts` 注释已警告）。

- **视觉字形尺寸保持不变**。要 44px 的是可点击区域，靠 `::after`（绝对居中、或只向上下扩）或 padding 形成，不靠把字形撑大；热区互不重叠（两个 44px 热区挤在 30px 间距里会互相偷点击）。相邻 iconBtn 间距 ≥ 12px（32 + 12 = 44）。

| 控件 | 可见尺寸（两态相同） | coarse 指针下的命中区 |
|---|---|---|
| 返回箭头 `‹` | 20px 字形 | 44×44（padding） |
| `终端\|结构` 分段 `.segItem` | 26px 高（在 D-039 的 25–30 区间内），fine 指针下无最小宽约束 | 每项 44 高、`min-width: 44px`；`::after { inset: -9px 0 }` 只向上下扩，相邻项不重叠 |
| 手机 Stop 方块 | 32px | `::after` 44×44 |
| 图标按钮 `.iconBtn` | 32×32，字形 16px | `::after` 44×44 |
| 芯片 `.chip` | 24px 高 | 竖向热区补到 44，`min-width: 44px` |

- **文字按钮和输入框不是字形控件**：coarse 下它们的**可见**高度可以直接取 44px（文字按钮/输入框 `--control-h` coarse = 44），不需要 `::after`；小控件 `--control-h-sm` 可见 36px、`::after` 补到 44。
- 反例（禁止）：把 `.back` 的 `width` 从 20px 改成 44px 来「满足 44px」。那是把字形撑成方块，既破坏排版也不是规格要的东西。
- 新增命中尺寸一律用 `var(--touch)`，不得再写裸像素。验收要同时覆盖「390 无触控（fine pointer）」与「390 有触控（coarse pointer）」：无触控的窄屏断言布局与密度，44px 检查只在 coarse 上下文。

**辅助文本字号：所有宽度都是 12px，下限 `--text-meta`。**

- `.meta` 一类辅助/诊断文本在所有宽度下统一 `var(--text-meta)`（12px/16px），桌面手机同值（D-039）。状态、诊断、时间戳、计数、meta 行、tooltip 正文、按钮副文案、列标题与分组头都走这个令牌。
- 承载信息的文字 ≥ 12px；用令牌而不是字面量。
- 正文 16px（`--text-read`，三档相同）、输入在 coarse 指针下 16px（`--text-input`，iOS 不放大页面）、fine 指针下输入 14px。
- 11px（`--text-2xs`）**只**用于与形状绑定的徽标数字和 kbd 字形——它是图形的一部分，不是线性排版里要读的文字。
- 布局断点、指针、字号与折叠的分工（不要混用）：**工具卡默认折叠与宽度无关**——已 settled 且成功的调用在所有宽度都折一行（§2.2，D-053 修订 D-041），只有 Workflow/error/running/interaction 等豁免集合不折；**字号随宽度的变化只有** [visual-system.md](./visual-system.md) §5.2 在 `@media (max-width: 767px)` 里重设 `--text-ui`、`--text-title`、`--text-page` 三个令牌，组件内不写宽度分支；命中区、输入字号（coarse 16px）、键盘/直连相关行为一律按 `pointer: coarse` 决定——窄桌面窗口会命中 compact 布局查询但没有触屏，按宽度加 44px 热区会误伤鼠标用户。

**「辅助文本」vs「图形标注」判定口径（D-052 第 9 条，供 CSS 护栏白名单分类）。** sub-12px 下限约束的是「要读的内容」，逐站点按它在界面里的角色二分：

- **辅助文本（受下限约束，不得豁免）**：行与卡片团体里线性排版的文字——状态/诊断句、时间戳、计数（如「N 项运行信息」）、meta 行、tooltip 正文、按钮副文案、列标题与分组头。这些桌面手机同值且 ≥ `--text-meta`。
- **图形标注（可豁免，逐站点登记）**：与形状绑定、作为图形一部分被读的字符——徽标数字、环内数字、kbd 字形、状态点/角标内嵌序号、图标里的字形。豁免不改变其含义必须另有形状编码（不靠颜色/小字传达状态，§6 与 D-039）。
- 拿不准的一律按辅助文本处理；白名单是**带摘除批次的台账**，不是永久地毯。

## 4. 移动端专项

### 4.1 软键盘

- 工作台高度绑 `visualViewport.height`，键盘顶起时 **scrollTo(0,0)**，composer 贴视口底。pinch `scale!==1` 时不要用 visualViewport 改布局（herdrx 已验证）。
- claudecodeui 同样有 `useVisualViewportKeyboardOffset`（`ProjectWorkspaceRoute.tsx`）。
- Modal 也跟 visualViewport（herdrx `Modal.tsx`）。
- `env(safe-area-inset-*)`；standalone 底栏避开 Home Indicator。

### 4.2 中文输入

- `isComposing` / `key=Process` / `keyCode=229` 时不发送（herdrx Composer；DSH 提问表单同样）。
- 手机主发送是按钮。桌面 Enter 发送。
- 终端直连在中文 IME 下会把候选送进 PTY → 手机默认 **本地输入**。
- 辅助键条不要抢焦点导致键盘收起。

### 4.3 触控选择

- transcript：原生选择；代码块提供「复制」。
- xterm：`attachTerminalTouch`，不要和页面手势抢；第一阶段不做双指当浏览器缩放以外的终端手势。
- 审批按钮在 `pointer: coarse` 下命中区 ≥ 44px；fine 指针下用常规控件高度（§3.4）。
- 不依赖 hover。

### 4.4 前后台

- `visibilitychange` / `focus`：回前台立刻 `journal.catchup(fromSeq)` + `checkForUpdate`（herdrx PWA 60s 节流）。
- 后台不保 WebSocket 时，靠 Push 唤醒。
- 离线文案抄 herdrx Auth：远程任务继续跑，恢复后再连（`App.tsx`）。

### 4.5 Web Push

事件：`interaction.requested`（审批/提问）、`activity=waiting-interaction` 持续、`lifecycle=exited` 且失败。不要对 `--bg` idle 发「已完成」。  
载荷：`title, body, tag`（`interaction:{id}` 或 `instance:{id}` 去重）, `data.url`，外加一个**可选**整数字段 `badge`（D-049）：等于该设备当前 pending interaction 数。

实现抄 herdrx：`GET /push/config` 公钥、`POST /push/subscriptions`（endpoint/p256dh/auth）、VAPID（`herdrx/internal/httpapi/push.go`、`internal/push/service.go`）。

**应用角标 badge（D-049，2026-09-19 增补）**：service worker 在 `push` 事件里对支持的平台调 `navigator.setAppBadge(n)` / `clearAppBadge()`；站内 pending 归零时同步清零。**降级必须诚实**：Badging API 不存在的平台什么都不做，绝不用通知条数冒充角标数。载荷缺 `badge` 字段时行为与今天逐字节一致（老 Hub / 老客户端混搭不炸）——这是一个可选 Rust 字段，不新增推送事件类型。今天全库无 `setAppBadge` 调用，角标只有站内计数。

**权限申请位置（D-049）**：绝不在 App 启动时弹——用户还没有待办时消耗唯一一次授权机会没有意义。只允许两处由用户手势触发：(a) compact 收件箱 `/m/inbox` 顶部横幅（报告 §11.3 实测 Moshi 同位置），点「开启」才调 `Notification.requestPermission()` 与 `subscribePush()`（`push.ts:108-114`），横幅可关闭；(b) 设置 → 通知（既有，`SettingsPage.tsx:724` 的通知组）。横幅文案随能力变：未加主屏幕的 iOS 改为「先加到主屏幕」并复用 `needsHomeScreenForNotifications()`（`push.ts:19`），点「开启推送」静默无效的情况不允许发生。

**PWA 关闭态**：SW 仍被系统唤醒并 `showNotification`（`sw.src.js:97`），点按走 `notificationclick` 聚焦既有窗口或 `openWindow`（`sw.src.js:106-120`），落点仍是 `data.url`（`/s/:id` 或 compact 下重定向到的 `/m/inbox?focus=`，§1.2）。已知边界保持不变：Hub 在有设备正 follow 该实例时抑制推送（`crates/remuda-hub/src/alerts.rs:159-160`）——「PWA 关着」正是推送真正生效的场景，这条抑制规则不改。

iOS：必须加到主屏幕才有 Notification（herdrx `needsHomeScreenForNotifications()`）。设置页写明。HTTPS + `isSecureContext` 才能注册 SW。

### 4.6 PWA 安装

- `manifest.webmanifest`：`display: standalone`（herdrx；DSH 用 fullscreen，手机不要 fullscreen 以免挡状态栏）。
- 图标 any + maskable 192/512。
- `start_url: /`（D-049，2026-09-19 从 `/sessions` 改）：落点由 §1.2 的重定向层按视口判定——手机安装的 PWA 落 `/m`，桌面安装的落 `/sessions`。只有一份 manifest、一份 SW（SW 预缓存壳清单已含 `/`，`sw.src.js:9`）。
- `theme_color` / `background_color` 都用深色 canvas `#232220`（两态下的浏览器栏外观由两个带 `media` 的 theme-color meta 处理，D-053）：`index.html` 同时声明
  `<meta name="theme-color" media="(prefers-color-scheme: dark)" content="#232220">`
  与
  `<meta name="theme-color" media="(prefers-color-scheme: light)" content="#f9f8f5">`；
  manifest 的 `name` / `short_name` 为 `Remuda`。
- SW：install 时预缓存 SHELL 壳清单（`/`、`/index.html`、manifest、favicon、图标，`sw.src.js:9`）；导航请求 network-first、失败回退缓存壳，`/v1/` 等 API 不经过缓存。更新策略抄 herdrx `pwa.ts`（waiting + `ACTIVATE_UPDATE`）。
- `beforeinstallprompt` 横条「添加到主屏幕」。
- 安装要求：**Hub 公网或内网 HTTPS**。明文 HTTP 无 Push、无 clipboard、无 SW。loopback 开发除外。

### 4.7 手机优先路由树与铬预算（D-049，2026-09-19）

**路由树：受限的 `/m`，会话本体不分叉。**

手机首页级信息架构放在 `/m` 子树，与桌面路由共享同一个 Vite PWA、同一份 store / auth / SW；桌面路由零改动（报告 §5「明确不做」：不新做原生 iOS/Android，不改协议/wire）。`/m` **只拥有导航与「首页级」信息架构**，不拥有会话本体：

| `/m` 子树内容 | 说明 |
|---|---|
| `/m` 会话 home | 分组会话首页（项目 + git branch 组头、一句下一步、context 剩余环，行口径同 §2.1 / D-038）；D-050 起在项目/branch 分组上**叠加 task 分组层**（「需要你」首组、父子任务嵌套、`SE-nn` 派生 key、已归档折叠，§2.9） |
| 看板的 compact 形态 | 不复制桌面三列：单列滚动 + 待办/进行中/已完成/已归档分段过滤（`GET /v1/board` 同一投影，一次一段；D-050）；桌面 `/board` 在 compact 不重定向到一个新页面，分段过滤就是 `/m` 子树内的看板形态。**实现状态（D-052，批次计划 D8）**：`/m` home 的 task 分组层已上线（`HomeList.tsx` 的 `buildHomeTaskLayer`），但单列**分段过滤尚不存在**（`features/mobile/` 无「待办/进行中/已完成」分段）；compact 访问 `/board` 仍由既有重定向落 `/m`（`mobileRoute.ts`）。ui-upgrade 批次**不实现**分段过滤（目标形态以本行为准，实现排后续批次），不得在证据里声称它已由 /m home 承载 |
| `/m/inbox` 收件箱 | 两档（待你处理 / 进行中·最近，无第三档），与桌面 `/approvals` 渲染同一 InboxShell、同一张 ApprovalCard 与同一 kind 分段（§2.5，D-052） |
| Jump To sheet | 从 home 顶栏与终端键盘条打开的覆盖层，不独占路由；分组 + 时钟/列表，**不做第二套空间模型**（报告 §10-23 / §11.2） |
| phone 底栏 | 会话 · 收件箱(n) · 新建 · 更多，只挂在 `/m*` 外壳上（compact 底栏文案以本条为准，§1.1 的「审批」在手机上即「收件箱」） |

**会话本体永远是共享的 `/s/:instanceId`（及 `/tty` `/structured` `/files` `/events`），不进 `/m`、不做第二份实现。** `/sessions/new`、`/login`、`/pair`、`/settings` 同样保持共享（它们已有 compact 形态）。理由是报告 §9 的 SOTA 一句话：**结构视图是 live session 的投影，不是第二个 agent；终端始终可一键回去且不 fork。** 会话页承载 journal follow、turn 裁决、composer 三态（D-028a）、terminal attach、内联审批与 resume（D-026）——复制它就是造第二个投影，并会让 transcript 实现分叉。手机上的改善靠这一条共享路由的 compact 形态（D-040 / D-041 / D-042 与本节预算），不靠第二份代码。

**重定向与深链**：见 §1.2 表后那张重定向表。要点三连：compact 下 `/sessions`→`/m`、`/approvals?focus=`→`/m/inbox?focus=`（query 原样保留）；桌面下 `/m*`→`/sessions`；**`/s/:id` 永不重定向**——它在两套壳下是同一条路由。

**铬预算（390×844 起测，全部可测量）**

- 任一手机屏最多**一条顶栏 + 一条底栏**。顶栏高 `--top-mobile`（52px）；首页级屏底栏高 `--phone-nav-h`（**56px**，D-053 从 64px 改）+ `--safe-bottom`。
- **会话路由 `/s/:id*` 在 compact 下不渲染 app 底栏、也不渲染 tab 行**（§1.3 / §1.4）：结构段底部只有收起态 composer（56px，D-042），终端段底部是本地输入条（fine 指针 36px / coarse 指针 44px）；键盘条 44px 仅 coarse 指针设备渲染（§2.3）。顶栏返回键字形 20px；`pointer: coarse` 下其热区 ≥ `--touch`（§3.4），fine 指针下不撑大。
- **正文（transcript 或 xterm）在 composer 收起、无软键盘时，可视高度 ≥ 视口高的 60%。** 结构段测 `data-testid="session-body"`，终端段测 xterm 容器。390×844 的预算余量：

  | 视图 | 占用 | 正文高度 | 占比 |
  |---|---|---|---|
  | 结构 | 安全区 47 + 页头 52 + live 行 20 + dock 间距 12 + composer 56 + 底部 34 | 623px | 74% |
  | 结构（有一条通知） | 同上，另加通知 24 | 599px | 71% |
  | 终端（coarse 指针） | 安全区 47 + 页头 52 + 本地输入 44 + 键盘条 44 + 底部 34 | 623px | 74% |
  | 终端（fine 指针，如无触屏窄窗口） | 安全区 47 + 页头 52 + 本地输入 36（无键盘条）+ 底部 34 | 675px | 80% |

  **60% 是验收下限，不是设计目标。**
- `pointer: coarse` 下触控一律走热区（§3.4）：视觉字形在任何指针下都不变（返回 20、分段 26、Stop 32、图标按钮 32、芯片 24），`::after` 或 padding 撑到 `--touch` 只在 coarse 下生效，热区不得互相重叠；fine 指针的窄屏保持桌面密度。

**软键盘弹起（`html[data-keyboard="1"]`，以 323px 可见带为例）**

- **隐藏**：live 行、通知行、TaskTrack、运行详情、composer 说明、批注行、InstallBar。live 行也隐藏——标题块第二行已经显示状态词，状态点在第一行持续显示，信息不丢。
- **永不隐藏**：页头、待处理卡区、路由故障提示、composer（含顶部区）。
- **布局规则**（全部 flex，不在 JS 里算高度）：
  - 正文：`min-height: calc(var(--workbench-height) * .4)`——正文保底占可见带的 **40%**；
  - 待处理卡区：`flex: 0 1 auto; min-height: 44px; max-height: calc(var(--workbench-height) * .45); overflow-y: auto`，空间紧张时由它先让出；
  - composer 的文本框最多 3 行。
- **结果**：无卡时正文 = 323 − 52 − 56 − 12 = 203px（63%）；有卡时卡区 ≈ 323 − 52 − 56 − 12 − 129 = 74px（内部滚动），正文 129px（**40%**）。验收条件：idle 与 working 两态、单行输入、没有排队芯片。
- **终端视图**：xterm 保持键盘弹起前的行数，外框裁切并贴底显示，本地输入与键盘条始终在带内，不向远端发 resize（§2.3）。

**截断优先级（页头宽度不足时按此顺序牺牲；D-049 三步，D-053 只改控件形状，不改三步）**

标题块设 `container-type: inline-size`：

1. **标题先截断**（始终单行省略，`title` 给全文）；
2. 容器 ≤ 120px 时，标题块里 `spaces-chips` 的 Space 名**退成首字母**（点开仍是 `spaces-drawer-open` 的同一个抽屉）；
3. 容器 ≤ 72px 时，**状态文字收起**，只留 §2.1 的状态点（三维投影语义不降级，unknown 不得画成正向）。

三步牺牲之后 compact 页头仍恒为：**返回 `‹` / 标题块（内含 Space 名与状态点）/ `终端|结构` 分段 / Stop / ⋯**——状态点始终留在第一行，与三步牺牲无关。390px 宽时标题块约 150px；360px 宽时触发第 2 步。

**主机与 cost 的 compact 例外（D-049，仅 compact；D-040 的桌面规则一字不动）**：D-040 钉的是**桌面**主行必须可见 host 与 cost；compact 下二者移入 ⋯ 菜单的「运行详情」面板（§2.2；D-053 取代的是旧 ui-spec §2.2 第二行「只有这一个触发器」的版式（基线 `80db05b8:docs/design/ui-spec.md:335`），D-040 (3) 的面板内容/持久化/`session-meta` 不变），不删除、不猜值——标题块里的 Space 名已经同时点名主机与项目（§1.4 的 `(hostId, workspaceId)` 归属），「这场要花多少钱、在哪个主机上」打开运行详情即可读到。桌面主行的 host 与 cost 保持原位，本条不改它。

**`终端|结构` 分段与 Stop 在任何宽度下都不截断、不折行、不进 ⋯ 溢出菜单**（D-040 (2)）：另一个视图是一等主控件（报告 §10-19 要求做成主分段），不是可收纳的设置项；「一键回终端且不 fork」必须在最窄宽度下仍然一键可达。热区与字形尺寸见 §3.4。

依据：报告 §5-P0-1（减铬）、§6（落地顺序把减铬放在第一）、§7.1（home 回答「连着谁、哪场还活着」）、§9（SOTA 一句话与「不要抄」：不抄顶栏/composer 盖住正文、绿色品牌/Space Grotesk/FAB/地球图标；该节对端口旁栏只说「有 artifact 再做、不要先做 Kill 端口面板」，并未说整体不抄）、§10-19（Term｜结构 主分段）、§10-23（Jump To 不做第二套空间模型）、§11.2（分组 + 时钟/列表、搜索只命中标题/标签/工作区名）、§11.3（Inbox 两档、错误文案当正文、权限横幅位置）、§11.4（git 一行扫视串可学，硬裁不折行 diff 与列表行右侧的丢弃按钮不抄）、§11.5（DEV SERVERS / Kill 端口旁栏与隧道列表**整体不抄**——不只是优先级问题，D-031）。里程碑：**M1 = home、会话、收件箱、新建、登录、语音（§4.8）**；终端键盘条、分组 Jump To、推送 badge 与真机验证排 M2；git 面板五 tab 在 M2 之后，本规格不出任务。

### 4.8 语音输入（D-049，2026-09-19）

第一里程碑「能说」（报告 §5-P0）的定义：**平台键盘听写优先，先成文再发送，零云转写。**

1. **默认路径 = 系统键盘自带的听写**（iOS / Android 键盘麦克风）。Remuda 不录音、不上传音频、不做云端转写、不新增协议字段——报告 §5「明确不做」禁止改协议/wire，且 Remuda 对会话的写入面只有 prompt / keys / 交互回答。
2. **先成文再发送**：听写内容只进入 composer 输入框，听写中途**绝不自动发送**。手机 composer 本来就是按钮发送、Enter 换行（§2.2 / §4.2）；`isComposing` / `key=Process` / `keyCode 229` 守卫（`composing()`，`viewport.ts:67-68`）对听写产生的 input 事件同样适用，任何语音路径都不得绕过它，也不得在听写期间抢焦点或重排布局（§4.1）。
3. **Web Speech API 只是「可用时的增强」，默认关**：先探测 `'SpeechRecognition' in window || 'webkitSpeechRecognition' in window`；能力不存在就**不渲染**麦克风按钮（不画一个点了没反应的按钮）；存在时也默认关闭，在设置里显式打开。识别结果**只写进输入框**，任何路径都不触发发送。音频不经过 Remuda 的 Hub / Node。
4. **iOS Safari 没有 `SpeechRecognition`（WebKit 未实现）——这句话必须原样写进设置页文案。** iPhone 上的「能说」就是系统键盘的听写按钮，不需要 Remuda 写任何代码；标准 PWA（加到主屏幕）里键盘听写可用，Remuda 不承诺离线听写。
5. **终端段不提供语音**：直接往 PTY 灌听写文本会把候选词与标点送进 TUI（§4.2 因此规定手机终端默认走本地输入条）。要说话就切到结构段，或在本地输入条成文后再送出。

依据：报告 §5-P0「手机会话先能读、能批、**能说**」与 §5「明确不做」（不改协议）；§7.4（Moshi 的 composer 也是把 prompt 写回主机上的 live terminal、不经其服务器——可借鉴是「成文再写入」，不是云转写）。

---

## 5. 组件来源与前端栈

### 5.1 抄结构，不抄插件运行时

**从 DSH `packages/client/ui-primitives` 抄结构（重写进本仓，不 import Cordis）**

| 件 | 用途 |
|---|---|
| `MarkdownText` / `CodeBlock` | 流式 GFM、冻结 block、Shiki、禁 raw HTML |
| `DiffBlock` | Edit/Write 三分态 |
| `ReadBlock` / `SearchBlock` / `WebBlock` | 只读输出 |
| `StateDot` / `ConnectionIndicator` / `DisclosureRow` | 状态与折叠 |
| `JsonBlock` | Generic / opaque |

**从 DSH 功能包抄交互模型，不抄 Cordis slot**

| 包 | 抄什么 |
|---|---|
| `ui-tool` keyed `tool.call.toolview` | 本仓 `toolRegistry.set(kind.name, Component)` |
| `ui-chat` Compact 折过程、usage 缺则藏、语义滚动锚 | |
| `ui-workflow-run` run/phase/member 开合默认 | 但 child 导航按我们的远程 id |
| `ui-approval` + `ui-user-questions` | 接管 composer、IME、多设备以 Hub 为准 |
| `ui-workspace` 琥珀 pending 优先于 running | 列表置顶待处理 |
| `ui-layout` 先砍右栏再压中栏 | 桌面 only |

**明确不抄**

- `apps/web` boot / `__DSH_BOOT__` / Cordis
- `TerminalBlock` 当 TUI
- Desktop Electron 私有管道
- DSH 自己的 Workflow 引擎 UI 当 Claude Workflow（那是另一套事件）

**从 herdrx 抄交互与 viewport 算法，重接 runtime API**（不要整文件搬运、不要绑 herdr socket）

| 件 | 算法来源 | 接到 |
|---|---|---|
| xterm 封装、fit、touch、主题 | `TerminalPane.tsx` `terminalFit.ts` `terminalTouch.ts` `themes.ts` | Hub tty / attach |
| 本地输入条 + IME + 草稿 | `Composer.tsx` `composerDrafts.ts` | Command `instance.send`，key=`instanceId` |
| compact 断点 + visualViewport | `displayPreferences.ts` | 本页 CSS 变量 |
| PWA SW / 安装条 / 离线 | `lib/pwa.ts` `components/PWA.tsx` manifest | 本产品 origin |
| Web Push 订阅 | herdrx 的 VAPID 订阅形状 | Hub `/push/*` |
| 路由 | 不抄 herdrx 手写 `/h/:id` | **React Router** |
| 主机表单 | `HostsPage.tsx` 布局可参考 | `transport=outbound-wss\|ssh-dev`，Node Agent 字段 |

**从 claudecodeui / paseo / vibe-kanban 只借形态，不借驱动**

- claudecodeui：`/` + `/session/:id`、手机 viewport hook、PTY+结构化双通道——双通道是会话的 structured / tty **两个视图**，driver 是 `claude-print` vs `claude-pty`/`claude-bg`，不引入它的 Express 后端。
- paseo：手机三态面板、hooks 判 `needs-input`；我们用 Interaction 队列，不靠 Notification 字符串。
- vibe-kanban：四栏工作区、任务看板——借看板的列投影与卡片形态（D-050 的 `/board` 是项目维度表面，§2.9），但**不要把主 IA 做成看板**（会话仍是一等导航，§1.5）；权限 hook 是 Node Agent 的事，不是 UI。

**自写**

- 会话列表跨主机、审批中心、Provider、Bot、新建会话 sheet、tool registry 的 Claude 族（Bash/Edit/Read/Write/Workflow/Task/MCP）、journal 客户端（snapshot+seq+gap）。

### 5.2 栈

| 层 | 选择 | 理由 |
|---|---|---|
| 语言 | TypeScript | 与 herdrx / DSH 一致 |
| UI | React 18 | herdrx、claudecodeui、vibe-kanban、DSH renderer |
| 打包 | Vite | herdrx、claudecodeui |
| 路由 | **React Router** | 深链、basename；不抄 herdrx 手写 `/h/:id` |
| 终端 | `@xterm/xterm` + addon-search/unicode11/web-links | herdrx |
| Markdown | micromark/mdast + Shiki（DSH）或同等；禁 `dangerouslySetInnerHTML` | |
| 状态 | React-free store（`useSyncExternalStore`），Immer 可选。不要 Redux | DSH `dsh-client-store`；herdrx 已用 `useSyncExternalStore` 做 PWA |
| 样式 | CSS Modules 或一份 token CSS。第一阶段不要 Tailwind 全家桶（vibe-kanban 用了，DSH 没有） | token 见 §6 |
| 图标 | lucide-react（herdrx） | 不要再引一套 |
| 测试 | Vitest + Testing Library（herdrx 已有 `*.test.tsx`） | |

Hub API：HTTPS JSON + 一条 WSS（journal + tty 多路，具体帧协议见 protocol 规格）。UI 包一层 `api.ts`。

### 5.3 目录（建议）

独立前端，不进 Go 树里和 herdrx 缠在一起。Hub 用 `embed` 或反代 `web/dist`。

```
web/
  index.html
  public/  manifest.webmanifest  sw.js  icons/
  src/
    main.tsx
    app/           路由、AuthGate、Shell（航轨/底栏）
    pages/         SessionsPage NewSessionPage SessionPage
                   ApprovalsPage HostsPage HostDetailPage
                   ProjectsPage（Workspace）ProvidersPage BotsPage SettingsPage LoginPage
    features/
      session/     transcript assembler、toolRegistry、composer、tty/
      approvals/
      hosts/
      providers/
      bots/
    components/    Button StateDot MarkdownText DiffBlock ConnectionIndicator Modal
    lib/           api.ts journal.ts push.ts pwa.ts viewport.ts nav.ts
    styles/        tokens.css
    types/         instance.ts observation.ts interaction.ts nativeRef.ts workspace.ts
```

第一里程碑页面：Login、Sessions、New、Session structured。TTY / `claude-pty` / 自动切终端 / Artifact 页是 M3。Approvals 是 M2（底栏入口预留）。Hosts 详情、Provider（只读 `astergate-default`）、Bot 可第二。

---

## 6. 视觉方向（D-053；令牌契约见 [visual-system.md](./visual-system.md)）

本节是 [visual-system.md](./visual-system.md) 的摘要；机械细节（令牌值、对比度矩阵、控件尺寸）以 visual-system.md 为唯一权威。

- **角色颜色**：颜色只按角色引用（背景 / 文字 / 线条 / 链接与焦点 / 状态 / 主按钮 / 终端与代码专用域），不按色相命名；十六进制字面量只允许出现在 `tokens.css`、`tty/theme.ts`、`index.html`、`manifest.webmanifest` 四处。
- **「墨」一套调色板，深浅两态**：默认外观跟随系统，由纯 CSS 媒体查询解析，无阻塞脚本；显式选择（`system | dark | light`）由 `main.tsx` 挂载前写 `:root[data-appearance]`；旧值 `night` / `ledger` 分别读作 `dark` / `light`。两态各自调校、不做反相。状态色有纪律：琥珀=需要你，红=失败/破坏，绿=权威确认（idle、在线、已连接不用绿），未知=中性虚线+文字。主按钮是中性反色填充，不用状态色。
- **终端恒深色**：深浅两态终端相同；标准 256 色立方不再染色，每个非黑 ANSI 色 ≥ 4.5:1。
- **字体**：UI 与长文同用平台系统无衬线字体栈（含 PingFang SC、微软雅黑、Noto Sans CJK），不打包正文 webfont；等宽只保留 IBM Plex Mono（OFL-1.1，许可入库），用于代码、路径、终端、ID 与对齐数字。
- **度量**：正文 16px/28px；阅读列上限 720px，页边距 32 / 24 / 16；composer、live 行、结束条都与阅读列对齐。
- **字号下限**：承载信息的文字 ≥ 12px，辅助文字 `--text-meta` 所有宽度都是 12px（D-039）；11px 只用于徽标数字与 kbd 这类图形标注（D-052 第 9 条）；coarse 指针下输入 16px。
- **命中区**：按 `pointer: coarse` 判定、不按宽度；字形尺寸不变（返回 20、分段 26、Stop 32、图标按钮 32、芯片 24），44px 只靠 `::after` / padding 形成的热区，热区互不重叠。
- **层级**：靠色阶与留白分隔，相邻同色用 1px 细线；只有浮层（菜单、弹出层、对话框、sheet）带阴影（`--shadow-2` / `--shadow-3` 两级）；全站不用 backdrop-filter。
- **圆角**：6 / 8 / 12 分层（按钮输入 6，卡片菜单 8，composer/对话框/结束条 12），sheet 顶角 16。
- **焦点**：2px `--focus` 环、offset 2，只在 `:focus-visible` 出现；滚动容器内 offset -2；文本输入用边框变色 + 1px 外环。
- **动效**：只动 opacity、transform、颜色，时长 100–240ms；流式内容没有入场动画；模式切换即时生效；reduced-motion 下只保留 `data-motion="essential"`。
- **基础控件**：按钮（primary / 默认 / ghost / danger）、`.iconBtn`、`.seg` 分段、输入框、菜单、`.sheet`、芯片、徽标、StateDot、`.prose` / `.md` 的规格见 visual-system.md §8，类名保留。

产品是「远处挑马、看它干活」：多主机、原生 TUI、审批。状态色是唯一的高饱和色，其余克制。不抄通用圆角 SaaS 卡堆、全大写 eyebrow、中间点分隔元数据这类生成默认。

## 7. 待决策

已定（v0.2，对照审查）：手机默认 `claude-print`；桌面仅在需要 `/workflows` 面板时才 `claude-pty`（M3）；`--bg` 不是 pty；自动切 tty 默认关至 M3；审批进底栏；v1 无 herdr 分屏；React Router；默认外观跟随系统、「墨」一套调色板深浅两态（D-053，2026-09-23 所有者拍板）；Provider M0–M2 只有 `astergate-default`。

仍开：

1. 产品名与窗口标题：**已定 Remuda**（D-001 命名，2026-09-23 所有者拍板 UI 文案、窗口标题、manifest 均用 Remuda，D-053；`index.html` 的 `<title>` / `apple-mobile-web-app-title` 与 `manifest.webmanifest` 的 `name` / `short_name` 同步改名，旧的「runtime」占位文案删除）。
2. 权限默认：询问 vs 可改文件。建议询问；`acceptEdits` 仅作本 cwd 覆盖。
3. Provider 页是否外链 astergate 管理 UI。建议只读健康 + 外链。
4. Bot 页是否允许「挂到现有飞书 hub」。建议否。
5. protocol.md 补完 envelope 时字段名再对一次（不改屏）。status 投影与 Workspace / NativeRef / transport 已按审查对齐。
6. 真机未测：iOS Safari standalone Push、Android Chrome 键盘、中文 IME+xterm。落地后按 §4 清单验收。
