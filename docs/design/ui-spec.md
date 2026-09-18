# Web / PWA 富交互界面规格

状态：可开工规格 v0.2（2026-09-12）  
产品定位：unified remote agent runtime 的遥控面（方案草案称 Remuda；未拍板前 UI 文案用 **runtime**）。  
**不是** harness，**不造** agent loop。界面只观察 + 下发控制；resume 权威是原生会话。

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
| `sessions` | 会话 | 跨主机实例列表 + 打开会话 | 左栏列表；主列会话页 | 底栏「会话」 |
| `approvals` | 审批 | 全局 pending Interaction | 顶栏铃铛 + `/approvals` | 底栏「审批」（有待办时红点） |
| `hosts` | 主机 | Node Agent 在线、CLI、登录态 | `/hosts` | 底栏「更多」→ 主机 |
| `projects` | 项目 | Workspace（cwd / worktree）通讯录，按主机分组 | `/projects`；新建会话里的选择器 | 新建会话步骤里选 |
| `providers` | Provider | profile、健康、与 astergate 关系 | `/providers` | 更多 → Provider |
| `bots` | Bot | 飞书 / Telegram 绑定、白名单、会话键 | `/bots` | 更多 → Bot |
| `settings` | 设置 | 设备、推送、权限默认（v1 无浅色开关） | `/settings` | 更多 → 设置 |

「项目」是 UI 文案，**不是**独立实体、也不是 vibe-kanban 看板。协议实体是 **Workspace**（主机上的 cwd 通讯录，id=`workspaceId`）。给新建会话和过滤器提供稳定 `workspaceId`（host + path + 可选 worktree）。

会话页内部两个视图，**不是**两个顶层导航，也**不是** herdr 多 pane 工作台：

- `structured`：transcript（print 默认；pty-backed 的第二视图）
- `terminal`：xterm attach 原生 TUI（`mode=tty-attachable`：`claude-pty` / `generic-pty` / `shell-pty` / kind `terminal` / codex·grok·agy。v1 一实例一终端，无分屏）。pty-backed 默认打开终端 tab。

### 1.2 URL 路由表

Hash 路由不要。用 **React Router**（History API）。认证 cookie 必须 `Secure`（DSH 默认 loopback 无 Secure，不能照搬，`deepseek-harness.md` §2.3）。

| 路径 | 屏 | 备注 |
|---|---|---|
| `/login` | 设备登录 | 未登录唯一公开页 |
| `/` | 重定向 `/sessions` | |
| `/sessions` | 会话列表 | query：`?host=&workspace=&kind=&status=`（`status` 是 §2.1 投影名） |
| `/sessions/new` | 新建会话 | query 可预填 `host` `workspace` `kind` |
| `/s/:instanceId` | 会话页 | print 默认 structured；pty-backed（codex/grok/agy/`claude-pty`/`terminal`）默认终端 |
| `/s/:instanceId/tty` | 会话页终端视图 | 无 tty 时回 structured 并 toast |
| `/s/:instanceId/structured` | 会话页结构化视图 | pty-backed 的第二视图 |
| `/s/:instanceId/files` | 会话页文件/diff（桌面右栏；手机全屏） | |
| `/approvals` | 审批中心 | `?focus=:interactionId` 高亮一条；手机底栏一等入口 |
| `/hosts` | 主机列表 | |
| `/hosts/:hostId` | 主机详情 | |
| `/projects` | 项目列表（Workspace） | 路径保留「项目」文案 |
| `/projects/:workspaceId` | Workspace 详情（最近会话、默认 kind/model） | |
| `/providers` | Provider 列表 | |
| `/providers/:profileId` | profile 详情 | |
| `/bots` | Bot 总览 | |
| `/bots/:channelId` | 飞书或 Telegram 绑定 | |
| `/settings` | 设置 | |
| `/pair` | 设备配对（可选，herdrx `#pair=` 同类） | 第一里程碑可不做 |

深链：Web Push `data.url`、飞书卡片「在 runtime 打开」都进 `/s/:id` 或 `/approvals?focus=`。

### 1.3 桌面三栏 ↔ 手机单列

**断点（建议，已验证 herdrx 公式可抄）**

- Compact：`(max-width: 767px), (pointer: coarse) and (max-width: 1023px) and (max-height: 600px)` — `herdrx/web/src/lib/displayPreferences.ts` `COMPACT_WORKBENCH_QUERY`。手机旋转仍保持单列。
- DSH 的 `SIDEBAR_AUTO_COLLAPSE = 1024`、中栏保 400px（`ui-layout/src/client/columns.ts`）只用于桌面三栏计算，**不要**在 compact 留 56px rail。

**桌面（≥768px 且非 coarse 矮屏）**

```
┌────┬──────────────────┬────────────────────────────────┬─────────────────┐
│ 48 │  280 列表         │  中栏（min 420）                │  右栏 0 或 ≥300 │
│ 航 │  会话 / 主机 / …  │  会话 transcript 或 表单        │  文件/diff/wf   │
│ 轨 │                  │  底：composer                   │                 │
└────┴──────────────────┴────────────────────────────────┴─────────────────┘
```

- 左航轨固定 48px 图标：会话、审批（badge）、新建、更多（主机/项目/Provider/Bot/设置）。
- 第二列是当前区域的索引（会话列表、主机列表…）。会话页打开时第二列仍是会话列表，当前行高亮。
- 右栏默认关。有 diff / Workflow 树 / Artifact 时自动开；空间不够先关右栏，再压中栏（抄 DSH `computeColumns` 顺序，不要先压中栏）。
- 会话页顶栏：标题、status 点（§2.1 投影）、terminal|structured 切换（仅 `tty-attachable`）、Stop、主机/项目芯片。pty-backed 默认 terminal，structured 是第二视图。这排元素在 compact 下允许重排与减铬，但 **`terminal|structured` 切换与 Stop 必须始终留在顶栏，永不进 ⋯ 溢出菜单**（D-040）：另一个视图是一等入口，不是「更多」里的设置。诊断字段（driver / delegation / provider / lifecycle / seq / connectivity / native）允许折进「运行详情」（§2.2）。

**手机**

```
┌─────────────────────────────┐
│ 顶：标题 / 连接 / 铃铛        │
│                             │
│ 唯一一列（当前路由全屏）        │
│                             │
│ 会话页：transcript            │
│ 底：composer（visualViewport）│
├─────────────────────────────┤
│ 会话 │ 审批·n │ ＋ │ 更多     │
└─────────────────────────────┘
```

映射规则：

| 桌面 | 手机 |
|---|---|
| 航轨 + 第二列 | 底栏 + 全屏列表 |
| 中栏会话 | 全屏会话；返回回列表 |
| 右栏文件/diff/Workflow | 底 sheet 或 `/s/:id/files` 全屏 |
| 终端视图 | 全屏 xterm + 本地输入条 + 辅助键（仅 `tty-attachable`） |
| hover 预览 / 拖拽排序 | 禁止；改长按菜单 |
| 多 pane 分屏 | **v1 不做**（不是 herdr 工作台；一实例一终端） |

Paseo compact 是左列表 / 中 agent / 右文件三态互斥（`paseo/docs/mobile-panels.md`）。本产品更简单：列表和会话是路由，不是手势抽屉。审批用独立底栏入口，避免 DSH「折叠组不汇总待交互」的坑（`deepseek-harness.md` §2.2 会话列表）。

---

### 1.4 Space / Tabs 项目工作台（D-024）

本节为 2026-09-13 的信息架构增补，优先于 §1.3 中会话索引始终展开、手机不使用抽屉的旧约定。借鉴 herdr 的 workspace → tabs → panes，当前实现前两个维度：**Space = 一个项目，Tabs = 该项目的 agent 会话**；仍不做多 pane 分屏。

- Space 来源是 D-023 的主机已注册 Workspace，以 `(hostId, workspaceId)` 区分；注册 wire 字段为 `workspaceId / hostId / root`，现有 UI 投影保持 `id / hostId / rootPath / label`。默认名称取根目录 basename，客户端重命名不会修改注册目录或后端 Workspace。实例严格按 host 与 workspace 两个 ID 归属，未注册、取消注册或无匹配的实例统一进「其他」。
- 桌面左侧 Spaces/Sessions 面板列出 space 与组内 sessions，组可展开/折叠；即使折叠，仍显示 live/blocked 数量。面板有持续可达的折叠按钮，收起后保留 space 首字母窄轨。显示名、手动顺序、分组展开状态和面板折叠写入本设备 `localStorage`，不跨设备同步。
- 当前 space 的 tabs 位于会话内容上方，显示标题、harness 字形、状态点和关闭入口。每个 space 分别记住最后选中的 tab；切换项目恢复其选择，不能把上一个项目的选中实例或新建默认值带过来。会话可从列表或深链重新打开，这不自动 resume；会话内 Stop 控制仍可使用。
- **状态与关闭分离（D-024 addendum，优先于本节旧描述）**：状态点永不是 ×（见 §2.1），× 只表示「关闭标签」，桌面在 hover 或当前 tab 上显示，手机长按或滑动显出。已退出会话的 × 直接移除 tab；运行中会话的 × 打开「停止并关闭 / 仅关闭标签」两选项 sheet。「仅关闭」只隐藏 tab、不发送关闭命令，会话继续运行，进入 blocked 时重新出现在 tab 条，在侧栏点击也会重新打开；「停止并关闭」才发送既有关闭命令，失败保留 tab 并提示，实际退出仍以 Instance/journal 更新为准。该偏好按 space 存在本设备 `closedTabs`（`{id, resurface}` 记录，旧的 id 列表按 `resurface: true` 读入）。
- **侧栏当前态与已退出分组**：当前 space 与当前会话都用品牌左条 + 底色 + 加粗标题，两者都能一眼看出。每个 space 下的「已退出 (n)」分组默认折叠，行内提供 **恢复**（既有 resume 能力）与 **删除**（`DELETE /v1/instances/{id}`，确认「删除会话及其记录？」）。运行中会话按钮为「停止并删除」，走 `?force=1` 由 Hub 停止并删除，客户端不再自行先 close；404 按幂等成功处理，`nodePurge` 非 `purged` 时提示主机侧数据待清理。删除失败保留该行并提示，不显示成功文案。
- **活动 tab** 用品牌下划线 + 底色 + 加粗，深浅主题都有足够对比，不单靠颜色；键盘焦点环沿用全局 `:focus-visible`。
- `/s/:instanceId` 及其子视图路由保持有效，直接打开会同时选中实例所属 space 和 tab。当前 space 的「新建」入口带入 host/workspace，cwd 默认该注册根目录；「其他」不虚构注册根。被移除或关闭的选中 tab 回退到该 space 可用 tab，无 tab 时显示该 space 的会话列表或空态。
- 桌面快捷键：⌘/Ctrl+B 折叠面板，⌘/Ctrl+1..9 选择当前 space 的相应 tab，⌘/Ctrl+[ / ] 切换前后 space。约 400px 手机上显示可横向滚动的 space chips 与 tabs，左侧面板通过抽屉访问；使用现有 viewport 和 Night Corral 主题 tokens，深浅主题保持一致的布局及状态含义。
- **compact 下的 space chips 例外（D-040，2026-09-19 增补，仅覆盖上面那句 chips 要求的应用范围）**：列表路由（`/sessions`、`/hosts`、`/projects`、`/approvals` 等工作台首页面）保持整条可横向滚动的 space chips 与抽屉入口不变。`/s/:instanceId` 及其子视图（`/s/:id/structured`、`/s/:id/tty`、`/s/:id/files`）在 compact 下**允许**把整条 chips 行折成**单枚当前 space 芯片**并入会话顶栏：点击该芯片打开同一个抽屉（`spaces-drawer-open` 行为与 testid 不变），切空间能力不降级，只是不再常驻一整行。理由：会话页在 400px 上同时叠 space chips、tabs、会话顶栏、审批卡、composer 与底栏，正文被挤到不足半屏；深链进入某一会话时「我现在在哪个 space」由单芯片回答即可。**`terminal|structured` 分段与 Stop 不受本条影响**：它们仍在顶栏，见 §1.3。

本节只作用于会话工作台。fleet 与全局 approvals 的范围和入口不变，composer 继续以当前实例为控制目标。验收与桌面/400px、深浅主题截图见 [spaces-1.md](./evidence/spaces-1.md)；tab 语义增补的验收见 [tabs-1.md](./evidence/tabs-1.md)。

---

## 2. 关键屏

每屏：ASCII 线框、状态清单、数据字段。字段名跟 `protocol.md`：Instance / Command / Interaction / NativeRef / Workspace。列表上的 `status` 点是 `lifecycle × activity × connectivity` 的投影，不是单独的 wire 枚举。

### 2.1 会话列表 `/sessions`

```
┌─ 航轨 ─┬──────────────────────────────────────────────┐
│ 会话●  │  会话                    [筛选] [＋ 新建]     │
│ 审批 3 │  ┌ 搜索标题 / cwd / 原生 id ─────────────┐   │
│ ＋     │  └──────────────────────────────────────┘   │
│ 更多   │  待处理 (2)                                  │
│        │  ● blocked  bolt · sfe-root · claude         │
│        │    「能否动这段 spill？」  等你批准 Bash      │
│        │  ● blocked  sg · valhalla · claude           │
│        │    AskUserQuestion · 3 题                    │
│        │  进行中 (1)                                  │
│        │  ● working  bolt · bolt · claude  12s        │
│        │    Workflow wf_ab12 · phase compile          │
│        │  最近                                        │
│        │  ○ idle     forge · …  （回合结束、进程仍在）  │
│        │  ■ exited   small · …  exit 1                │
└────────┴──────────────────────────────────────────────┘
```

手机：无航轨；顶搜索+筛选 chips；底栏。待处理置顶，不进折叠组。

**行内容 = 状态点 + 标题 + 一句下一步（D-038，2026-09-19 增补）**

上面线框里每行的第二句（「等你批准 Bash」「AskUserQuestion · 3 题」「Workflow wf_ab12 · phase compile」）是**派生出来的一句话**，不是 wire 字段的转写。据此把行内容钉死：

- **默认视口里只出现**：状态点（§2.1 上表的三维投影）、标题、kind 芯片、unread/pending 徽标、以及一句「下一步」。行内**不出现** `lifecycle`/`activity`/`connectivity` 的原文三元组（`ready · waiting-interaction · connected`），也不出现 `ins_…` 短码、`driver`、`model`。
- 那句「下一步」由**已有字段投影**得出（`projectStatus` / `Interaction.request.kind|description` / `exitLabel` / screen 终态），**不新造状态机、不猜成功**。`connectivity ≠ connected` 与 `lifecycle ∈ {unknown,reconciling}` 必须产出「状态待确认」，**不得**回落成 idle/空闲一类正向文案；`exited` 用「已退出」而不是「完成」。
- 三维 wire、`ins_`、driver、model 退到每行的 `<details data-testid="session-wire">` 展开内容或 `title`：**默认视口不可见**，可读性不丢。
- 行内遥控（`send…` 输入框、enter / esc / ctrl+c 按键）收进长按/溢出 `Sheet`（**保留全部既有 testid 与命令路径**，只是不再常驻行内）；「待处理」组在行上保留**一个**主操作（直达 `/approvals?focus=` 或该会话）。
- 状态点仍是三维投影（上表），本条只改**文本**的呈现位置，不改状态语义。

理由与依据见 [workbench-ux-improvement-2026-09.md](./workbench-ux-improvement-2026-09.md) §11.3（Moshi 在同一位置放的是错误原文，不是 wire 串）、§11.7 的 P0-6 行（该行结论是**当前代码偏离了本节线框**，不是建议与规格冲突）。

**状态点（列表 + 会话顶栏共用）= 协议三维投影**

wire 用 `lifecycle` × `activity` × `connectivity`（`protocol.md` §2.3）。UI 只画一个点，形状可区分、不只靠颜色，且**任何状态都不画成 ×**（× 保留给「关闭标签」）。规则：

| UI 点 | 色 | 投影 | 禁止 |
|---|---|---|---|
| `blocked` | 琥珀 ⚠ | `activity=waiting-interaction` | 优先级最高（DSH pending 琥珀点） |
| `working` | 蓝 | `activity=working` | |
| `starting` | 灰闪 | `lifecycle ∈ {requested,preparing,starting}` | |
| `idle` | 绿描边 ○ | `activity=idle` 且 `lifecycle=ready`。含 `--bg` 原生 `state=done` 但进程仍在（可 send） | **不要**把这种情况画成会话结束 |
| `exited` | 灰 ■ | `lifecycle=exited` | jsonl 可能还在；**禁止**画成 ×，× 只表示关闭标签（D-024 addendum） |
| `unknown` | 虚线灰 | `lifecycle ∈ {unknown,reconciling}` 或 `connectivity ≠ connected` | **禁止**画成 idle/exited 成功 |

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

---

### 2.2 会话页 · 结构化视图 `/s/:instanceId`

```
┌  ← 列表   sfe-root / spill · claude · passthrough/… · bolt · $0.12  ● working   [结构] [■ 停止]
│  ▸ 运行详情
├──────────────────────────────────────────────┬──────────────────┐
│  You                                      12:01                  │
│  看 TaskManager spill 这段为啥抖                                │
│                                                                  │
│  ▸ thinking（折叠，默认关）                                      │
│                                                                  │
│  ┌ Bash  running ─────────────────────────────────────────────┐  │
│  │ $ ninja -C build TaskManagerTest                           │  │
│  │ ▾ stdout  12 行                                            │  │
│  └────────────────────────────────────────────────────────────┘  │
│  ┌ Edit  ok  src/exec.cc  +12 −4  [已写入]                      │  │
│  │  unified diff                                              │  │
│  └────────────────────────────────────────────────────────────┘  │
│  ┌ Workflow  wf_9f3  running ─────────────────────────────────┐  │
│  │ ▾ compile   2 members                                      │  │
│  │   · agent haiku   running   [打开]                         │  │
│  │   · agent gpt-6   idle                                     │  │
│  └────────────────────────────────────────────────────────────┘  │
│  ┌ 审批  Bash  rm -rf /tmp/coord-media  ──────────────────────┐  │
│  │  Allow once   Deny   Always in this cwd                    │  │
│  │  多设备：第一台点的算数                                     │  │
│  └────────────────────────────────────────────────────────────┘  │
│  最终回答 Markdown…                                              │
│  usage  in 12.1k / out 800  ·  $0.12                             │
├──────────────────────────────────────────────┴──────────────────┤
│  输入提示词…                                    [送出]  权限:询问 │
└─────────────────────────────────────────────────────────────────┘

「运行详情」展开后（主行不变，第二行的 `▸` 翻成 `▾`，其内容展开）：
│  ← 列表   sfe-root / spill · claude · passthrough/… · bolt · $0.12  ● working   [结构] [■ 停止]
│  ▾ 运行详情
│    driver claude-print · delegation none · provider passthrough · lifecycle ready
│    seq 184 · connectivity connected · native claude:1a2b · transcript 绑定 hook
```

**两行 header（D-040，修订 §11.7 冲突「P0-5」）**

主行 = `← 返回` + `space / 标题` + `harness` + `model` + `主机芯片` + `cost` + 状态点 + `terminal|structured` 分段 + Stop。诊断不占主行，但下面的元素**必须留在可见主行**，因为它们回答「这场能不能继续」，不是诊断：

- **主机芯片**（§1.3 要求顶栏有主机/项目芯片）。
- **cost 累积**（§2.2「Usage / cost」要求顶栏 cost；未知标「—」）。

其余诊断字段（`driver` / `delegation` / `provider` / `providerSourceHint` / `lifecycle` / `seq` / `connectivity` / `native` / promoted `transcript 绑定`）进第二行的 **「运行详情」disclosure**：第二行**只有这一个触发器**（`▸ 运行详情`，无其他 token），默认收起，展开态**按设备持久化**（本设备 `localStorage`，同 §1.4 的显示名/顺序口径，不跨设备同步）。收起时用 `▸`/`▾` 与可见计数提示「有 N 项运行信息」，不隐藏到无从发现。`session-meta` testid 保留在展开内容上（老断言迁移只需展开一步）。

紧凑（compact）下不新增第三层：「运行详情」在 compact 同样只有一行触发器，展开后按宽度换行。原来「手机字号小于桌面字号」的问题随字号统一（§3.4）消失。

手机：无右栏；Workflow / diff 点开进 sheet。composer 贴 `visualViewport` 底边。

**实时状态条（dock，`LiveStatusStrip`）——回合是否结束由会话已有的每条通道共同决定，不由 hook 相位锁存单独决定**

状态条渲染纯 reducer `turnEnd` 的唯一裁决（`working | waiting | ended | unknown` + `decidedBy` + `endedAt`），不再直接渲染 hook 相位锁存。`phase.ts` / `liveStatus.ts` 仍是纯 fold，所有消费决策归 reducer。优先级从严到宽：

1. **人机回合高于一切结束信号**：本 instance 有 pending 交互、screen 锁存自身报 blocked、或新鲜的 hook `blocked` 相位时，无论 hook 通道多静默、spinner 是否已清空，都判 `waiting`。parked 权限 hook 本身就不发记录、其对话框又会清掉 spinner，否则会在 6 s stall 预算后误判「回合结束」并把代持消息 POST 给仍卡在对话框上的 agent。一个 hook 已静默又无 pending 的 blocked 是 `unknown`，绝不是「等待操作」。
2. hook 的 `turn-ended` / `interrupted` **无条件**直接定终（harness 自己的终态词，新到即覆盖 `decidedBy=hook`）；hook 通道**新鲜**时，hook 活动相位持有回合（设计 §2.4 规则 6，screen 的中途 idle 边沿被忽略）。
3. hook 通道一旦 `stalled` / `never-materialised`（`channelHealth`，3×2 s）即降为参考，screen 的非活动 `live.status`（或 pty `agent_status`）结束回合——但 screen 清空每「离开」只发一次、其 `observedAt` 会留到下一回合，所以要求 screen 锚 **不早于** 锁存相位的 `since`，避免短回合沿用上一回合的清空时间。
4. 再没有 screen 时，晚于锁存 `since` 的 assistant 消息从 transcript 收尾结束。

- `ended` 显示「回合结束」+ 小芯片 `data-testid=live-decided-by`（值 hook/screen/transcript）；elapsed 始终锚在**回合开始**（submit/spinner 再锚，跨回合从事件列表重取，不依赖会被 turn-ended/clear 覆盖的锁存 `since`），结束时冻结在该回合时长 `endedAt − start`（即终端显示的时长），而不是塌成 0:00 或在稍后打开页面时变成「结束以来」；移除 Esc 打断按钮。迟到的 hook `Stop` 改判 `decidedBy`，但 composer 的 `ended→idle` 边沿幂等，不二次 flush、不移动时长。
- composer 相位由该 reducer 与 `projectStatus` 合并：`ended→idle` 是工作→空闲边沿，已有 effect 恰好调用一次 `flushHeld`；`unknown` 退回 instance 投影，不塌成 idle/blocked。
- amber 静默注记可附 Node 侧实际跑过的检查名（`relay-missing` / `socket-refused` / `link-stalled`，来自 instance 上的 `hook.silence` 诊断）。屏幕轮询层只真正探测 relay 与 socket：一个正常结束的会话本来就不再发 hook，relay/socket 都健康时**不报** `link-stalled`（该判定只属于持有 journal flush 游标的 transport 层），新鲜 tier（Stop 刚到）也不写记录；没有检查结果时徽标保持原样，绝不猜原因。
- `Notification` hook（含 Claude Code 的 `idle_prompt`「等待你输入」）是结束后的咨询，不是阻断请求：不抬相位、不算 waiting；在站内以 toast + 会话页小通知列表呈现（文案、时间、可忽略；`permission_prompt` 类链接到待处理对话卡），既有 push 路径不变。注意 grok 没有单独的阻断权限 hook——它唯一的权限提示就是 `Notification(permission_prompt)`；移除其 waiting 语义后，grok 的 blocked 状态**只**由 screen 层（OSC/屏幕 blocked 锁存与 pending 对话框识别）给出。

**Transcript 节点（journal fold，稳定 `nodeId`）**

| 节点 | 默认 | 数据 |
|---|---|---|
| `user.message` | 展开 | `role=user` blocks；本地乐观气泡在 accepted 后换成权威节点（DSH Chat 规则） |
| `assistant.message` | 展开 | 流式 Markdown |
| `thought` | 折叠 | 未完成可展开看；`completeness=screen-derived` 时标「从屏幕猜测」 |
| `tool.*` | 见下 | call/result 配对，`callId` |
| `interaction.*` | 未决时钉在 composer 上方并接管输入 | 见审批卡 / 表单 |
| `workflow.run` | 运行中与结束后都展开；只有读者 dismiss 才折叠（按 workflowId 持久化于 runtime localStorage）。未 dismiss 的卡永不进 compact 汇总行；dismiss 后折进「N 次工具」行，打开该行仍能到达卡片 | 见树；运行中每 1s 走墙钟 |
| `usage` | 回合结束后一条 | 缺字段就整条隐藏，不写假合计（DSH `ui-chat` usage 行） |
| `error` | 展开 | 不推断成功 |
| `opaque` | 一行「未识别事件」+ 原始 kind | 禁止当终态 |

Compact：回合结束后把 thinking + 中间 tool 折成「N 次工具 · M 段思考」，最终答案保留。默认 Compact（DSH `ui-chat` Compact 默认）。设置可改。

**有界 journal 窗口与「加载更早」**

Hub 的 journal 读是有界尾部窗口（至多 2000 行 / 8 MiB，新行优先）：attach 与 follow snapshot 只拿尾部，窗口元数据 `fromSeq`（窗口底，空窗口为 null）与 `reachedAfterSeq`（底是否到 afterSeq）随行返回，更深的历史用同一 `afterSeq` + `beforeSeq = fromSeq - 1` 向下翻页。

- 初始加载只读一个尾部窗口；**禁止**再写「512 行循环向上翻」这类读法——有界窗口下它切的是尾部中段，下面的历史被静默丢弃。
- snapshot 的 `fromSeq` 是**窗口底，不是保留底**：不得喂给 `applyBatch` 的 `from < floorSeq` 陈旧判定（那会把正常的深页误判 readonly-stale）。
- gap 回填向下降页直到 `reachedAfterSeq=true`，缓冲后按 seq 顺序一次性 flush；翻页有界（约 16 页 / 20k 事件），预算耗尽时 flush 已连续的前缀、只报一次残余 gap、落到 readonly-stale，绝不永久缓冲。
- 已加载底 > 1 时，transcript 顶部显示「加载更早的记录」行（JournalBanner 旁边的独立行）：一次点击取一页（`beforeSeq = floor - 1`），按 seq 升序 prepend；虚拟列表以点击前最顶部可见节点为锚，保持其视口偏移（padTop 增长只推动锚上方的留白）。到 seq 1 后该行消失。
- banner 状态：补页中 `gap-backfill`（「正在补事件 · 工具卡暂不结算」），预算耗尽/分歧 `readonly-stale`，补齐回 `live`。

**Tool 卡片（keyed registry，未知 → Generic）**

注册键 = `driverKind + '.' + nativeToolName`，再映射到族。不要把 Codex `commandExecution` 硬叫 Bash（`deepseek-harness.md` §2.2）。

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

**手机默认折叠（D-041，修订 §11.7 冲突「P1-10 / P1-20」）**

compact（手机）下已结束的普通工具卡默认折成**一行**，点开才是上面的完整卡；桌面默认态不变（仍是各自族的上表默认）。

| 规则 | 内容 |
|---|---|
| 生效条件 | 仅 **compact**（§1.3 断点）**且**该卡已 settled（call/result 配对完成，或明确失败）时默认折叠。**running / 未 settled 的卡永不折叠**——正在发生的事要看得见 |
| 豁免集合（永不折叠，无论 settled） | `family === "Workflow"`（`workflow.run` 按上表：运行中与结束后都展开，只有读者 dismiss 才折叠）；`error` 卡（上表「`error` 展开」）；`interaction.*`（未决时钉在 composer 上方） |
| 折行内容 | **family + 关键参数**，不能只写族名。Bash = 命令首行（截断到宽度，`title` 给全文）；Edit / Write / Read = 路径（同样截断 + `title` 全文）；Workflow/Task/MCP 见豁免与各自族默认。裸 `Bash` 不合规 |
| 实现约束 | 折叠分支**必须在 family 判定之后**才可提前 return。当前 `ToolCard.tsx` 的 `if (folded) return …` 早于 `family === "Workflow"` 分支，一律默认折叠会让 `WorkflowTimelineCard` 永不挂载、1Hz 走针不启动——这是本条的验收硬指标，不能只断言「卡可见」，要断言卡头 elapsed 在运行中递增 |
| 展开态 | 展开后的卡与桌面完全一致（同一组件、同一 testid）；折叠只改默认开合，不改内容 |

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
  所以 UI 不需要一个「图片坏了」的占位分支——它看到的就是一条文本；
- 不新增 `ObservationKind`，不新增卡片族：CUA 的调用按 MCP 族渲染
  （`server/tool`，`tool_name` 形如 `mcp__codex-computer-use__<verb>`）。


**审批卡（内联）**

钉在 composer 上，同时出现在审批中心。字段：`interactionId`, `type=approval`, `title`, `preview`（命令或补丁摘要）, `risk`, `actions[]`（allow / deny / allow_once）。提交后按钮 disabled，直到 journal `interaction.answered` 或 `expired`。文案：「多台设备同时点，只记第一次。」

**AskUserQuestion 表单**

接管 composer（DSH `ui-user-questions`）：多题 pager；单选点完前进；多选 + 自定义；IME 期间 Enter 只上屏不上交。提交一次 `InteractionAnswer{id, items[]}`。Skip 单题；关闭 = cancel 整批。

**Workflow 树**

只展示身份和状态，不把 script/log 塞进节点（DSH `ui-workflow-run`）。完整 journal 放「原始事件」抽屉。native Workflow member **不是** runtime child Instance。仅当 `childInstanceId` 存在（显式 `instance.create`）才点进 `/s/:childId`；否则只展开本树。冷 child 用 jsonl 投影，composer 禁用除非 driver 能 resume。

三个时钟的唯一定义（行 tooltip 原文）：**用时** = `endedAt − startedAt`，运行中为 `now − startedAt`；**空闲** = `endedAt − lastProgressAt`，运行中为 `now − lastProgressAt`（stall 时用时照走、空闲照涨，不靠 member 的 `durationMs`）；**排队等待** = `startedAt − run.launchedAt`。缺输入一律 `—`。注意 `launchedAt` 是 **journal 登记该 run 的时刻**，不是它真正启动的时刻：Remuda 从磁盘发现（attach）一个已经在跑的 run 时，launchedAt 晚于真实启动，`startedAt − launchedAt` 会为负 → 排队等待显示 `—`（这是数据语义，不是 bug）。卡头 elapsed 因此取两者较大值：运行中 = `max(totals.elapsedMs + 距上次快照的墙钟, now − launchedAt)`，旧 Node 没有 `launchedAt` 时单独由快照外推继续每秒走针。phase 跨度 = 该 phase 最早 `startedAt` 到最晚 `endedAt`（运行中以 now 收口），没时间戳返回 undefined 显示 `—`。运行中的卡每 1s 自己走针（同时驱动卡头 elapsed、每-agent 用时/空闲），不加 fetch 轮询，卡折叠/结束后 interval 立即拆除。

**Usage / cost**

回合脚：`inputTokens` `outputTokens` `costUsd?`。缺任何精确字段 → 不显示合计。顶栏 cost 是会话累加，未知标「—」。

**Composer**

- 桌面：Enter 发送（IME composing / keyCode 229 / key=Process 时忽略，抄 herdrx `Composer.tsx` `composing()`）。Shift+Enter 换行。
- 手机：发送按钮；Enter 换行。不要抢中文候选。
- 权限芯片显示当前 `permissionMode`（dontAsk/acceptEdits/manual…），点开改本会话（发 Command，不是只改本地 chip）。
- Effort：收起为 compact 触发器，只显示**档名**（`ultracode ▾`），宽度固定不顶布局。点开是一张 ~300px 的 card popover，钉在触发器上方：
  - 第 1 行 grid：左闪电图标 · 中间品牌色档名（18px）+ `›`（点开档位/模型列表）· 右复位图标。第 2 行居中 muted 型号（13px）。
  - 下方一条 40px 高的圆角 pill：左侧已填部分是品牌实色（`--cold`），右侧未填是中性 surface；每个档位一个小圆点 marker，两侧都看得见；钮是 36px 白圆 + 柔和投影，拖动吸附到点上。钮心行程内缩半个钮宽（`--knob` = 18px），钮不会溢出 pill。
  - **填充必须压到钮下**：fill 宽 = 钮心 + 钮半径（`--knob * 2 + pos * (100% - --knob * 2)`），fill 右端正好落在钮右缘、圆头藏在钮底下，钮左侧和钮下不留暗轨；第一档 fill 正好一个钮宽，同样不留缝。
  - 手机上触控 ≥ 44px 只靠**热区**，不靠视觉尺寸：pill 仍是 40px，外面套 48px 热区；图标按钮保持 26px 字形，用 44×44 的 `::after` 扩大命中面。
  - 最高档：整条 pill 换 Night Corral ember 琥珀渐变，上面叠一层暖光 + 三层疏密不同的 ember 星点，各自以不同速度横向漂移（其中一层反向）并各自闪烁，钮带一圈呼吸的琥珀光晕；收起态触发器同频率轻微发光。只用 transform / opacity，不触发布局；popover 关闭即卸载，`prefers-reduced-motion` 下全部停成静帧。
  - 吸附 harness 原生档（claude `low/medium/high/xhigh/max` 加 `ultracode` workflow stop，codex `low/medium/high/xhigh/max/ultra`（显示 Low / Medium / High / Extra high / Max / Ultra；说明见 protocol.md effort 表），grok `low/medium/high/xhigh`）。`role=slider`，`aria-valuetext` = 档名。←/→/Home/End、触摸拖动、44px 触控。
  - `›` 展开的列表里才有档位说明和模型选择（`‹` 返回 pill）；pill 视图本身不列模型。
  - 变更走 `instance.configure`（journal + persist）。无档位或会话不可配置时禁用。不用档位芯片作第二套控件。
- 本地草稿按 `instanceId` 存（herdrx `composerDrafts`）；未 accepted 的乐观气泡可撤回。

**compact composer 边界（D-042，修订 §11.7 冲突「P0-3」）**

compact（手机）下 control bar 允许收成一个**选项触发器**，但「收进 sheet」有硬边界，因为它和 D-028a、上面的权限/effort 条款直接相关：

- **触发器必须同时显示两样东西**：当前 `permissionMode` 的**词**（`manual` / `acceptEdits` / `dontAsk` / `bypassPermissions` 的 label 或 native 词）**和** effort 的**档名**（上一条「只显示档名」的要求不因收起而豁免），形如 `manual · high`。`permissions.ts` 标为 `danger` 的模式（绕过全部 / 不再询问 / 完全访问）必须在触发器上就用 danger 样式可见——**把 bypass 态藏进 sheet 是本条最响的禁止项**：权限档位是「这台机器现在会不问就动手吗」的回答，不能只在点开后才看得见。
- **可以进 sheet**：附件、harness 只读芯片、context 用量、权限选择器本身、effort 滑杆。
- **不得进 sheet**（留在 sheet 外，D-028a）：发送/排队/打断**三态**按钮、队列 chip、能力未验证时的「尚未验证」标注。三态是当回合的事实，`unknown` 是诚实的欠缺，两者都不是「选项」。
- placeholder 分平台：手机「输入提示词…」（不写桌面快捷键——手机上 Enter 是换行、⌘ 不存在，写快捷键是误导）；桌面保留快捷键说明。
- 确认类交互：插队与 Esc 打断**不再用 `window.confirm`**（Safari 上会盖住键盘，且样式不可控），改用 `Sheet`（桌面 `popover`，手机 `sheet`，焦点圈定、Esc = 取消、返回焦点到触发器）。确认后的命令语义与 `commandId` 路径**完全不变**。
- 桌面布局与 testid **零变化**。

**状态清单**

`loading-snapshot` → `live` → `reconnecting` → `gap-backfill` → `readonly-stale`（`connectivity ≠ connected`）。`blocked`（`activity=waiting-interaction`）时 composer 换成 interaction 表面。`exited` 显示 Resume（若 `capabilities.resume`）或「开新会话继承 cwd」。`--bg` 空闲保持 `idle`，不要显示「已完成」。

---

### 2.3 会话页 · 终端视图 `/s/:instanceId/tty`

pty-backed 会话（kind `terminal` / driver `shell-pty` / `generic-pty` / `claude-pty` / codex·grok·agy）打开 `/s/:id` 即终端。结构是第二视图 `/s/:id/structured`。

```
┌  ●终端 | 结构    80×24  raw  webgl  [本地输入|直连] [esc tab ctrl alt ↑ ↓ pgup pgdn ctrl+c] [全屏]
├─────────────────────────────────────────────────────────────────────┐
│  xterm.js  ·  fit  ·  WebGL/canvas  ·  Unicode11  ·  Night Corral 256│
│  原生 TUI（vim / grok / claude 全屏，鼠标跟踪开启时点击进 PTY）        │
│                                                                     │
├─────────────────────────────────────────────────────────────────────┤
│  raw：击键 / 粘贴 / 鼠标序列直接进 PTY                                │
│  keys：本地输入条（手机默认；IME composing 不送）                      │
└─────────────────────────────────────────────────────────────────────┘
```

**真·全屏**：隐藏 chrome，`position:fixed; inset:0; height:100dvh`，`env(safe-area-inset-*)`。

**从 herdrx 抄交互与 viewport 算法，重接 runtime API**（不要搬 herdr snapshot 绑定）

| 能力 | 算法来源 | 接到 |
|---|---|---|
| xterm + fit + WebGL/canvas + Search/Unicode11/WebLinks | `TerminalPane.tsx` | Hub `/v1/follow?tty=1` binary `tty.frame` |
| 256 色 Night Corral | tokens.css → `theme.ts` `extendedAnsi` | xterm `ITheme` |
| 手机默认 keys、桌面默认 raw | `WorkbenchPage.tsx` | follow 输入 channel |
| IME 安全发送 | xterm composition + `composing()` | raw onData 不拆候选 |
| 鼠标 | xterm mouse tracking (`onData` + `onBinary`) | 应用 DECSET 1000/1002/1003/1006 时转发 |
| 辅助键条 | `terminalTouch.ts` + `AuxKeys` | esc/tab/ctrl/alt/arrows/pgup/pgdn/ctrl+c |
| `visualViewport` 缩键盘、pinch zoom 不 reflow | `displayPreferences.ts` | 本页 layout |
| fit/fixed/responsive 字号 | `terminalFit.ts`；手机默认 `responsive` | `tty.resize {cols,rows}` |
| 断线：终端保留最后一帧，标 reconnecting | follow 重连 + snapshot replay | Instance.connectivity |

**不要抄** DSH `TerminalBlock`（剥了绝对光标、清屏、alternate-screen）。

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

### 2.5 审批中心 `/approvals`

```
┌ 审批中心          待处理 4 · 本设备已处理的会从队列消失 ┐
│ 筛选 [全部] [审批] [提问] [计划]     主机/Workspace     │
│                                                         │
│ ● 12:04  bolt / sfe-root / claude                       │
│   Bash  rm -rf /tmp/coord-media                         │
│   [打开会话]  [允许一次] [拒绝]                           │
│                                                         │
│ ● 12:01  sg / valhalla / claude                         │
│   问你 3 题 · AskUserQuestion                           │
│   [去回答]                                              │
│                                                         │
│ ○ 过期  11:40  …  「过期，未作用于新进程」               │
└─────────────────────────────────────────────────────────┘
```

**状态**

| UI | 数据 |
|---|---|
| pending | `Interaction.status=pending` 且未过期 |
| answering | 本设备已 POST，等 `accepted` |
| settled | journal `interaction.answered`；从待处理移除，可在会话内看到终态卡 |
| expired | 超 TTL（建议 10–15 min，短于飞书 token 30 min，`bot-dispatcher.md` §3.2） |
| superseded | 别的设备先答；本设备按钮变「已在 Mac 处理」 |
| paused | Node Agent 断线；禁止点，文案「主机离线，交互暂停」 |

**字段** `interactionId, instanceId, hostId, type, title, preview, createdAt, expiresAt, answeredByDeviceId?`

多设备：Hub 只接受第一次有效 `commandId`。所有打开此页的客户端靠 journal 更新，不要本地抢锁。Push：`tag=interaction:{id}`，点通知进 `?focus=`。

AskUserQuestion 不在列表里填完（题太长）；「去回答」进会话页表单。Allow/Deny 可在列表一键完成。

---

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
| 无法识别 | `opaque` 一行，点开 raw JSON；不参与 Compact 折算成功 |

禁止：把 PTY 里看到的 “✓” 写成 tool ok；把 `logs` ANSI 录像当 transcript（`claude logs` 是 PTY 转储，`claude-control-plane.md` §1.4）。

流式 Markdown：已完成 block 冻结，开着的 fence 按行高亮（DSH `MarkdownText`）。崩溃发生在 settlement 前：保留临时行并标「未落盘」，不要改写成完整 assistant/message。

### 3.4 命中尺寸与辅助文本字号（D-039，修订 §11.7 「P0-2 / P1-8」）

这两条是全局口径，适用于本规格所有屏（尤其 §2.2 / §2.3 顶栏与 composer）：

**命中尺寸一律走热区。** §2.2「Effort」那条已经写明：手机上触控 ≥ 44px 只靠**热区**（`::after` / padding）扩大命中面，**不靠视觉尺寸**。本节把它升为全局规则：

- 视觉字形尺寸保持不变。20px 的返回箭头、25–30px 的 `terminal|structured` 分段、32px 的手机 Stop 方块**继续是这些尺寸**；要 44px 的是它们的**可点击区域**。
- 热区写法照既有先例：绝对居中的 `::after`（`session.module.css` 里 `.effortIconBtn::after` 已经是 44×44 的合规写法），或足以撑到 44px 的 padding。新增命中尺寸一律用 `var(--touch)`，**不得**再写裸像素。
- 反例（禁止）：把 `.back` 的 `width` 从 20px 改成 44px 来「满足 44px」。那是把字形撑成方块，既破坏排版也不是规格要求的东西。
- 可点区域**不得互相重叠**：两个 44px 热区挤在 30px 的间距里会互相偷点击，等于两个都不准。

**辅助文本字号：桌面与手机同值，下限 `var(--text-aux)`。**

- `.meta` 一类辅助/诊断文本**桌面与手机统一** `var(--text-aux)`（12px）。当前实现桌面 11px、手机 10.5px，两个值都低于 12px 下限，且**手机字号小于桌面**——同一元素在更小的屏上更难读，方向是反的。
- 12px 下限的出处是 `tokens.css` type scale 的块头注释「Important status never relies on sub-12 px text」，不是 `--text-label` 自己的注释。
- 用 token 而不是字面量：`.meta` 这类元素此前完全没有引用任何 token（`session.module.css` 里的字号与命中尺寸全是硬编码），这正是这些数字跑偏的原因。改的时候引 token，不要照抄一个 12px 字面量。
- 这条只管**辅助文本**。正文 14px、输入/强调 16px 不变；`--text-label`（11px）作为标签仍可用于**非状态**的短标签，但状态、诊断、时间戳这类要读的内容走 `--text-aux`。
- compact 判定（布局）与触屏判定（`coarsePointer`）**不要混用**：字号与折叠按「布局是否 compact」决定即可；键盘/直连相关行为一律按 `coarsePointer` 决定——窄桌面窗口会命中 compact 查询但没有触屏，按它砍键盘能力会让人丢鼠标键盘（`web/src/lib/viewport.ts` 注释已警告）。验收要同时覆盖「390 无触控」与「390 有触控」。

---

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
- 审批按钮 min 44px。
- 不依赖 hover。

### 4.4 前后台

- `visibilitychange` / `focus`：回前台立刻 `journal.catchup(fromSeq)` + `checkForUpdate`（herdrx PWA 60s 节流）。
- 后台不保 WebSocket 时，靠 Push 唤醒。
- 离线文案抄 herdrx Auth：远程任务继续跑，恢复后再连（`App.tsx`）。

### 4.5 Web Push

事件：`interaction.requested`（审批/提问）、`activity=waiting-interaction` 持续、`lifecycle=exited` 且失败。不要对 `--bg` idle 发「已完成」。  
载荷：`title, body, tag`（`interaction:{id}` 或 `instance:{id}` 去重）, `data.url`。

实现抄 herdrx：`GET /push/config` 公钥、`POST /push/subscriptions`（endpoint/p256dh/auth）、VAPID（`herdrx/internal/httpapi/push.go`、`internal/push/service.go`）。

iOS：必须加到主屏幕才有 Notification（herdrx `needsHomeScreenForNotifications()`）。设置页写明。HTTPS + `isSecureContext` 才能注册 SW。

### 4.6 PWA 安装

- `manifest.webmanifest`：`display: standalone`（herdrx；DSH 用 fullscreen，手机不要 fullscreen 以免挡状态栏）。
- 图标 any + maskable 192/512。
- `start_url: /sessions`。
- `theme_color` / `background_color` 跟 Night Corral（v1 无浅色）。
- SW：预缓存壳；**不**缓存 journal API。更新策略抄 herdrx `pwa.ts`（waiting + `ACTIVATE_UPDATE`）。
- `beforeinstallprompt` 横条「添加到主屏幕」。
- 安装要求：**Hub 公网或内网 HTTPS**。明文 HTTP 无 Push、无 clipboard、无 SW。loopback 开发除外。

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
- vibe-kanban：四栏工作区、任务看板——**不要**把主 IA 做成看板；权限 hook 是 Node Agent 的事，不是 UI。

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

## 6. 视觉方向

不要：暖奶油底 + terracotta、黑底酸绿、报纸细线、通用圆角 SaaS 卡、全大写 eyebrow、中间点分隔元数据。这些是 frontend-design skill 列出的生成默认。

产品是「远处挑马、看它干活」：多主机、原生 TUI、审批。状态点是唯一高饱和色，其余克制。

### 方案 A · Night Corral（推荐工作台默认）

夜间围栏 / 马具间：深靛黑，不是纯 `#0B0B0B`。灰尘琥珀作 blocked/审批，冷青作 working。

| token | hex | 用途 |
|---|---|---|
| `ink` | `#12161C` | 页面底 |
| `ink-2` | `#1A212B` | 中栏、卡 |
| `line` | `#2A3340` | 分割 |
| `paper` | `#E7DCC8` | 主文字（暖，不是冷灰白） |
| `mute` | `#8B7F6A` | 次级 |
| `dust` | `#C9842A` | blocked / 审批 |
| `cold` | `#6A8B9A` | working / 链接 |
| `ok` | `#7A8F62` | idle/done，灰绿不是荧光 |

字体：正文 **IBM Plex Sans**；代码/路径/终端 **IBM Plex Mono**。14px 正文，终端默认 14。圆角 4px，卡片靠 1px `line` 而不是阴影。

### 方案 B · Shop Ledger

白天机修账本：冷纸白、墨蓝线、铁锈红只用于 error。

| token | hex | 用途 |
|---|---|---|
| `paper` | `#F3F0EA` | 底（偏冷，不是奶油黄） |
| `ink` | `#1E2A32` | 文字 |
| `line` | `#C5C1B7` | 规则线 |
| `steel` | `#3D5A68` | 导航、链接 |
| `rust` | `#8F3D2C` | error / 拒绝 |
| `stamp` | `#B0892E` | blocked |

字体：**Source Serif 4** 标题 + **IBM Plex Sans** 正文 + Plex Mono 代码。更适合列表/设置；会话工作台仍建议 A（终端对比足够）。

**v1 只做 A（深色）。** 不设浅色开关；方案 B 留作以后文档，不进设置页。`prefers-reduced-motion`：除 spinner 与 ConnectionIndicator 点点外无入场动画。

---

## 7. 待决策

已定（v0.2，对照审查）：手机默认 `claude-print`；桌面仅在需要 `/workflows` 面板时才 `claude-pty`（M3）；`--bg` 不是 pty；自动切 tty 默认关至 M3；审批进底栏；v1 无 herdr 分屏；React Router；只做深色 A；Provider M0–M2 只有 `astergate-default`。

仍开：

1. 产品名与窗口标题：Remuda / 其它（`proposal.md` §3）。未定前品牌字用 runtime。
2. 权限默认：询问 vs 可改文件。建议询问；`acceptEdits` 仅作本 cwd 覆盖。
3. Provider 页是否外链 astergate 管理 UI。建议只读健康 + 外链。
4. Bot 页是否允许「挂到现有飞书 hub」。建议否。
5. protocol.md 补完 envelope 时字段名再对一次（不改屏）。status 投影与 Workspace / NativeRef / transport 已按审查对齐。
6. 真机未测：iOS Safari standalone Push、Android Chrome 键盘、中文 IME+xterm。落地后按 §4 清单验收。
