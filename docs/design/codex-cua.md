# Codex Computer Use → Remuda：能力授予、投递与交互路由

**状态**：签约（contract）。实现任务 `c-cua-skill` / `c-cua-hostcap` /
`c-cua-launch` / `c-cua-media` / `c-cua-coord` 以此文为准。
**决策**：[D-045](./decisions.md)（能力按次授权与投递）、[D-046](./decisions.md)（交互路由）。
**来源**：`skills/codex-computer-use/`（计划 §1 的审计）与计划 `codex-cua.md` §3
的集成提案（计划原文在 worker 树外，此处只作来源标注）；被挪出 skill 的证据表见
[evidence/codex-cua-1.md](./evidence/codex-cua-1.md)。

---

## 0 一句话，以及一条必须先说的丑话

一句话：**Remuda 可以按会话、按次把「操作这台 Mac 的桌面」授予一个 worker**——
通过内嵌 skill 物料化进受管 home、加一份 per-instance MCP config、并在两道门
（来源不是 agent、主机回报了能力）之后才允许；每一次桌面动作和它带回的截图都
必须在 journal 与会话页里看得见。

丑话（**先读这条再读其余**）：**这条路目前没有任何一条被证明跑通过。**
skill 自己的验证记录（[evidence/codex-cua-1.md](./evidence/codex-cua-1.md)）写得很清楚：

- 非 Codex 宿主走原生 MCP 路径会拿到 `Sender process is not authenticated`；
- `probe_mcp.py` 的 `metadata_ok` 只证明 `initialize` + `tools/list` 通，**不是 UI 可用的证明**；
- 「Claude Code 实际消费 MCP 截图及操作 UI」一栏的结论是**未验证**。

因此本文描述的是**合同**，不是**已验证的通路**。合同的价值在于：无论最后哪条
后端跑通，授予、投递、拒绝、记账这四件事的形状都不变。任何实现任务都必须在
它的证据文档里如实标注哪些格子是 VERIFIED、哪些是 UNVERIFIED，**不许把
计划当证据**。

---

## 1 四个名词

| 名词 | 含义 | 不是什么 |
|---|---|---|
| **能力**（capability） | `computer-use` 这个字符串。一次 launch 上的一次显式申请 | 不是 host 属性、不是 driver 属性、不是全局开关、不是「装了就能用」 |
| **授予**（grant） | 这次 launch 的审计里出现了一份带 digest 的 MCP config（以及可能的 skill 树） | 不是「Node 这台机器上有个 app」——那是 §3.4 的 host fact |
| **投递**（delivery） | 让被启动的 agent 真的**看得见**这个能力：skill 文件 + MCP server 注册 | 不是「把文件拷到用户家目录」。见 §3.2 |
| **路由**（routing） | `elicitation/create`（按应用审批）由谁回答 | 不是审批中心。见 §5 与 D-046 |

---

## 2 授予：两道门，缺一不可

```
remuda instance create --capability computer-use --host <macOS host> …
remuda dispatch --capability computer-use --host <macOS host> …
```

**门 1 · 来源。** 只有 `LaunchOrigin::Human` 与 `LaunchOrigin::Bot` 可以申请。
`LaunchOrigin::Agent` **一律拒绝**。形状照抄已有的 yolo 双门
（`crates/remuda-driver/src/presets.rs:151-166`）：一个 agent 永远不能为自己
铸出桌面控制，bot 可以把人的显式授权透传，但绝不自行铸造。

**门 2 · 主机。** 目标主机的心跳 `cli[]` 必须含一行 `kind: "computer-use"` 且
`installed: true`（§3.4）。**未回报即拒绝**——不是「假设装了」。
「这台机器没装」与「这个 Node 版本还没上报这一行」在协议上都能看到，但授予
路径只认前者为可授予，后者也拒绝（拒绝消息里点名是哪种，见 §4）。

**命名与拒绝。** 未知能力值（`--capability desktop` 之类）拒绝，消息里**点名
那个值**，绝不静默忽略——静默忽略等于让操作员以为授予成功。本批只有
`computer-use` 一个合法值。

**不可继承。** 能力不写进任何默认、不写进 preset、不写进 workspace、不从父
instance 继承。每一次授予都是一次独立的、要出现在 `LaunchRecipe` 审计里的动作。

---

## 3 投递：两条腿，都是 per-instance

### 3.1 前提事实——今天 agent 到底能看见什么

这一节是整个投递设计的依据（`crates/remuda-node/src/native.rs:238-249`、
`crates/remuda-driver/src/launch/shadow.rs:110-220`）：

| launch 形状 | native home | 今天能看见的 skills / MCP |
|---|---|---|
| claude，`delegation none`，driver 为 claude 系或 `shell-pty`，未显式给 config dir | 继承的 `~/.claude` | 操作员自己的 `~/.claude/skills/*`——**只因为 home 被继承了** |
| claude，`delegation gateway`/`direct`，或显式 `claudeConfigDir`，或 `generic-pty` | `REMUDA_CLAUDE_CONFIG_DIR` 或 `<instance>/native-home` | **没有**（该 home 只被种入 onboarding 与合并后的 `--settings` overlay） |
| codex | `<launch_dir>/codex-home`：只有 `config.toml`（features + hook trust）与 `hooks.json` | **没有**；也**没有 `mcp_servers`** |
| grok | `<launch_dir>/grok-home` + `hooks/remuda.json` | **没有** |
| 任意 kind，project scope | cwd 是 provision 出来的 worktree `<repo>/../remuda-wt/<name>` | `<repo>/.claude/skills` —— 仓库里**没有** `.claude/` 目录，所以今天是零 |

结论：**把 skill 目录丢进仓库并不能让任何一个被启动的 worker 看见它**，
只有手工把它拷进自己 `~/.claude/skills` 的**人**看得见。这就是为什么投递必须
由 launch 路径做。

### 3.2 腿 (a)：skill 字节 → 受管 native home

**门不是 `inherit_default_config`。** 那是 `NativeClaudeFactory` 的一个 claude 专属
标志（`crates/remuda-node/src/native.rs:238-240`：`kind != GenericPty && !explicit_config_chosen
&& delegation == None`），对一次 **codex** launch 它照样为真——而 codex 的 home 从来
不是「继承来的」（§3.1）。用它当门会在 codex 上得出错误答案。**门是「本次 launch 的
native home 是不是 Remuda 管的」，逐 kind 定义如下**：

| kind | 「受管 home」是哪个 | 腿 (a) 是否写 skill 树 |
|---|---|---|
| claude | 作用域 config dir（`<instance>/native-home` 或 `REMUDA_CLAUDE_CONFIG_DIR` 或显式 `claudeConfigDir`）；**继承的 `~/.claude` 不算受管** | 受管 → 写；继承 → **不写** |
| codex | `<launch_dir>/codex-home`（始终受管，`shadow.rs:110-168`） | **不写**（见下） |
| grok | `<launch_dir>/grok-home`（始终受管，`shadow.rs:173-223`） | **不写**（见下） |

**codex / grok 本批只有腿 (b)，而这是对事实的承认，不是省事。** **仓库里没有任何
东西、skill 里也没有任何东西表明 codex 或 grok 会读一个 skills 目录**：`skills/` 这个
概念在本仓库只存在于 claude 侧（`crates/remuda-node/src/native_config_access.rs` 的
skills 可读性检查、transcript 里的 `Base directory for this skill:` 文本），codex 的
影子 home 只被 codex 自己当 `CODEX_HOME` 读 `config.toml` / `hooks.json`，grok 同理读
`GROK_HOME`。往 `codex-home/skills/` 写一棵树**没有任何读者**，那是把字节丢进黑洞还
谎称投递成功。所以：codex / grok 的投递**就是那一份 per-instance MCP config**，能力
语义由 MCP server 自己承载；skill 的指令文本对这两个 harness 本批不投递，如实记账。
（grok 在本批**不是**能力目标：§5.3 的 elicitation 路由对 grok 无落点，且没有证据
表明 grok 能消费该 MCP server。写 grok 的行只为让「不写」这件事有出处。）

**claude 受管 home 的写路径**：

```
<native_home>/skills/codex-computer-use/**      目录 0700，文件 0600
```

字节来自**内嵌在 `remuda` 二进制里**的副本（与内嵌 web assets 同形），
不是从磁盘上的仓库读——一个没有 checkout 的 musl Node 也必须能投递。

**继承来的操作员 home 只读不写。** claude 且 home 是继承的 `~/.claude` 时，
skill 文件**不写**，本次 launch 只得到腿 (b)。这不是优化，是硬规则，权威表述
在 `crates/remuda-driver/src/launch/overlay.rs:25-28`：「**Never touch the user's
own config.** The only file written is under `<instance dir>/launch/`」。
`~/.claude`、`~/.codex`、`~/.grok` 在**任何**分支上都不会被打开写。这条要有
测试钉住（「没有任何分支会为写而打开 `$HOME` 下的路径」），不能只靠代码审查。

**内嵌副本不许漂移。** 内嵌字节与 `skills/codex-computer-use/` 的 digest 必须
相等，由一个测试断言。两者一旦分叉，被启动的 agent 拿到的就是过期指令。

### 3.3 腿 (b)：per-instance MCP config

一律写（无论 home 是否受管）：

```
<launch_dir>/mcp-cua.json      0600
```

内容用**绝对主机路径**指明 launcher（原生 MCP 路径给
`launch-mcp.sh`，未认证宿主给 `launch-cua-repl.sh`；选哪条是 skill 自己的
判断，见 SKILL.md「选择后端」），然后挂上：

| kind | 怎么挂上去 | 依据 |
|---|---|---|
| claude（本批唯一真正的目标） | argv 追加 `--mcp-config <path>` | `mcp-config` 早在四族白名单且取值：`crates/remuda-driver/src/flags.rs:64,82,89,333` |
| grok / agy | **本批不授予**（§3.2 末：grok 无 elicitation 落点）。若将来授予，机制与 claude 同（argv），但届时须先证明该 host 能消费这个 MCP server | `crates/remuda-driver/src/flags.rs:82,89` |
| codex | shadow `config.toml` 追加 `[mcp_servers.codex-computer-use]`，不扰动既有 `[features]` / `[hooks.state.*]` | `crates/remuda-driver/src/launch/shadow.rs:110-168` |
| `shell-pty` / `generic-pty` | **按 `spec.kind` 分派，不看 driver**——见下 | `crates/remuda-driver/src/flags.rs:100-108`、`crates/remuda-driver/src/materializer.rs:208-216` |

**pty 驱动的规则必须按 `spec.kind` 说，不能按 driver 说。** `shell-pty` 与
`generic-pty` 是 kind 多态的：同一个 driver 既承载 claude 也承载 codex。它们的 argv
白名单是 `EXTRA_CLAUDE`（最宽那张，`flags.rs:100-108`），所以从**白名单**看
`--mcp-config` 对它们合法——但这只回答「这个 flag 不被拒」，不回答「codex 读不读它」。

**因此：**

- **`shell-pty` hosting claude**（**D-036 自托管那一轮就是这条**：`--driver shell-pty`
  配 harness claude，证据 [self-host-1.md](./evidence/self-host-1.md) 记的是 Claude
  的 config dir 与两个首运行对话框）→ 与 claude 一路，argv 追加 `--mcp-config`。
- **`shell-pty` hosting codex** → 与 codex 一路：走 shadow home。`materialize_shell_pty_agent`
  （`materializer.rs:500`）已经为 codex 挂上 `CODEX_HOME`（preset 的 `home_env`，
  `materializer.rs:625-632` + `presets.rs:73`），而 `HookSession::start` 为
  `AgentKind::Codex` 物料化同一棵 `<launch_dir>/codex-home`（`launch/session.rs:99-110`）。
  **落点在 `config.toml` 的 `[mcp_servers.codex-computer-use]`，不追加 argv。**

一句话规则：**能力怎么挂，取决于 `AgentKind`（claude / codex / …），与 driver 是
`claude-print` / `claude-pty` / `shell-pty` / `generic-pty` 无关。** 唯一真正不走
argv 的 kind 是 codex（它拿 shadow home），其余 kind 一律走 argv。两条路各自要有
测试钉住，否则「granted but not delivered」会是一次静默失败。

**绝不发 `--strict-mcp-config`。** 该 flag 被永久禁用（`flags.rs:15`），而这是
**对的默认**：Remuda 提供的能力应该**增加** agent 自己的 server，绝不替换。
`--strict-mcp-config` 会让「授予桌面控制」附带「取消你原有的 MCP 工具」，
这是两件不该绑在一起的事。

> **D-045 实现注记（2026-09-19，c-cua-launch 落地）**：codex 的 shadow-home 投递
> 只在 **`shell-pty` + HookSession（Node 配置 `pty_hooks = true`，默认关）** 下成立；
> generic-pty 等无 HookSession 的 carrier 不会生成完整 shadow home，Node 工厂与
> materializer 对这些组合的 codex `computer-use` 一律按名拒绝，直到 shadow home
> 被完整播种（见 [D-045](./decisions.md)）。

**环境变量握手（与 `c-cua-skill` 的硬化 launcher 对齐）。** 硬化后的 launcher
（`skills/codex-computer-use/scripts/launch-cua-repl.sh`）在启动前要求子进程环境里有
`REMUDA_CAPABILITY_COMPUTER_USE=1`，否则直接拒绝并讲明原因——因为它启动的 REPL
对本机每一个应用持有 `click` / `typeText` / `pressKey`（该脚本顶部注释即此意）。
合同：

- **`c-cua-launch` 只在能力被授予时**把 `REMUDA_CAPABILITY_COMPUTER_USE=1`
  注入子进程环境（走既有的 env allowlist，`LaunchRecipe.env_allowlist`，与
  `CODEX_HOME` / `GROK_HOME` 同一条路）；未授予时**不设该变量**，launcher 自己拒绝。
- **该变量是信号，不是边界。** 任何能从 shell 里 `export` 它的东西都能绕过它——
  被启动的 agent 只要有一个 shell 就能自己导出再拉 REPL。所以它**不是**安全边界，
  只是「Remuda 这次确实授予了」这个事实的传递。**真边界是「没有授予就绝不物料化」**：
  能力没被授予时，腿 (b) 的 `mcp-cua.json` 根本不存在，agent 没有可指的 launcher
  路径，也就无从启动它。把这条写清楚是为了让实现者不要误以为那个 env 变量承担了
  它承担不起的职责。
- 未授予时 agent 的正确行为是**报告缺少的能力**，不是重试、不是自己导出变量
  （硬化 skill 已如此要求）。

注意 `launch-mcp.sh`（原生 MCP 那条）**没有**同等的 env 门——它的存在前提是宿主
已经把自己认证成了一个 Codex 内部会话，而 skill 的验证记录说非 Codex 宿主在这条
路上会拿到 `Sender process is not authenticated`（§0）。所以本批真正需要授权的
是 cua-repl 那条；原生 MCP 那条是「宿主本来就允许」的情形。

**记账。** 物料化的 `mcp-cua.json`（以及腿 (a) 的 skill 树）以带 digest 的
`MaterializedFile` 进 `LaunchRecipe.materialized_files`
（`crates/remuda-driver/src/recipe.rs:43-73,157-179`），生命周期取
`FileLifetime::Launch`——随子进程退出清掉（`recipe.rs:203-213` 已有 shred
语义，`ApiKeyHelper` 的先例）。于是 launch 记录能回答「这次到底授予了什么」，
而不是「操作员说他申请了」。

### 3.4 主机事实：一个只读的 `cli[]` 行

`/hosts/:hostId` 上 `computer-use` 是**一行普通的 CLI 行**（ui-spec §2.6），由 Node
的探针上报：

| 字段 | 值 |
|---|---|
| `kind` | `"computer-use"` |
| `installed` | 该路径上存在一个**可执行常规文件**（与 PATH probe 同一套 `is_executable` 判定，不是「这个名字下有任何东西」） |
| `path` | `${CODEX_HOME:-$HOME/.codex}/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient` |
| `version` | 读 app bundle 的 `Contents/Info.plist` 的 `CFBundleShortVersionString` |
| `auth` | `"unknown"` —— **刻意不探测** |

两条硬约束：

1. **`version` 是文件读取，绝不是 exec。** 跑那个 vendor 二进制去问版本会启动
   一个 Mach service。探针里**不许**出现任何 vendor 二进制的 `Command::new`。
2. **非 macOS 主机也回报这一行**，`installed: false`、无 path。这样 web 能区分
   「没有」与「没上报」（ui-spec §3.3 的规则），而不是把两者画成同一件事。

Hub 侧零改动：`crates/remuda-hub/src/inventory.rs:88-127` 对任意 kind 都是通用
透传，所以这一行端到端出现在 `/v1/hosts`、`remuda hostcap` 和主机页上，不需要
协议、Hub 或 web 类型变化。

---

## 4 拒绝：每一条都有自己的消息

拒绝必须发生在 create/dispatch 被持久接受**之前**，且**绝不静默降级**——
同 D-035 对「静默替换 driver」的处置。六条，各自独立：

| 触发 | 消息必须点名 |
|---|---|
| `LaunchOrigin::Agent` 申请 | 「agent 不能为自己授予桌面控制」+ 来源 |
| 主机 `cli[]` 无 `computer-use` 行 | 该 hostId + 「主机未回报此能力」 |
| 主机有该行但 `installed: false` | 该 hostId + **探针找过的路径** |
| 目标主机非 macOS | 该 hostId + 其 `os` |
| `bypassPermissions` 与 `computer-use` 同一次 launch | 两者都点名，说明组合被拒的理由 |
| launcher 脚本在主机上不存在 | 绝对路径 |

**为什么 bypass 要拒（不是「忽略其一」）。** 无人值守的桌面控制叠加跳过的工具
审批，是唯一没有回收路径的组合：agent 可以点任何按钮、批准任何对话框、花任何
钱，而 journal 之外没有任何东西能拦住它。要这个组合，必须是另一个显式 flag，
**不能是任何东西的隐含结果**。参照：`--dangerously-skip-permissions` 本身是
合法的一次等选项（D-011），但它与桌面控制**不能同时**授予。

**拒绝消息要说清楚重试方向**（计划 §6 风险 8 的教训）：一个把「驱动 iPhone 镜像」
派到 Linux lane 主机的操作员，应该在 launch 处立刻看到「这台主机不是 macOS，
请用 `--host` 指定一台 Mac」——而不是自己去猜。

---

## 5 交互路由：`elicitation/create` 今天只能由 worker 自己答

### 5.1 事实

agent 侧**只有一条** elicitation 桥，且只搭在 Claude 的 hook 事件上：

| 环节 | 位置 | 状态 |
|---|---|---|
| `Elicitation` 是注册事件 | `crates/remuda-signal/src/event.rs:114-135` | 有 |
| 它是阻塞事件，可回 `action` | 同上（`BLOCKING_EVENTS`） | 有 |
| 生成 `Interaction{kind: Elicitation}` | `crates/remuda-signal/src/approval.rs:153-208` | 有 |
| 从 `InteractionAnswer::Elicitation` 回一个动作 | `crates/remuda-signal/src/bus.rs:1017-1030` | 有 |

**但它够不到 MCP 的 `elicitation/create`。** `cua-repl` 是 stdio MCP server，它的
elicitation 走 **MCP 协议本身**；codex shadow 的 `hooks.json` 只注册
`PermissionRequest`（`crates/remuda-driver/src/launch/shadow.rs:110-168`），
没有把 MCP elicitation 抬进 interaction bus 的 producer。所以**今天没有任何
路径能把一个 MCP server 的 `elicitation/create` 变成 `/approvals` 里的一张卡**。

### 5.2 本批的合同（D-046）

**worker 自己答，回答权被 launch 请求约束。**

- worker 在 cua-repl 的 `initialize` 里声明 `capabilities.elicitation`；
- 对 `elicitation/create`：**仅当目标应用是本次请求点名的应用**时 `accept` 且
  `persist: session`；未点名的应用**一律不批**（拒绝或取消）；
- 「点名的应用」= 派发 brief 里写明的 bundle id。`c-cua-coord` 要求 brief 写出
  确切 bundle id，并禁止为未点名的应用批准 elicitation。

**如实记账的代价**（不许在文档或 UI 里假装它不存在）：这条路线意味着桌面审批
**没有 journal 行、没有 `/approvals` 卡、没有第二人复核**，只有 worker 的一面
之词。UI 与文档都不能画出一张并不存在的审批卡。

### 5.3 后续（不在本批）

把回答权从 worker 交回人，`/approvals` 成为 CUA 审批的唯一出口。前置是**每个
harness 各造一个 producer**，把它的 MCP `elicitation/create` 抬进 interaction bus
——**不需要新的 `InteractionCarrier` 取值**：`crates/remuda-protocol/src/enums.rs:242-249`
已经有七个（`ClaudeControl` / `ClaudeHook` / `HarnessHook` / `CodexRpc` / `AcpRpc` /
`NativeTty` / `Unsupported`），要用的那个**按 harness 选**：

| harness | MCP elicitation 会走哪条 | 用哪个既有 carrier |
|---|---|---|
| claude | 已经以 `Elicitation` hook 事件浮上来（`crates/remuda-signal/src/event.rs:114-135`），链路已通 | `HarnessHook`——**已经就是它**（`crates/remuda-signal/src/approval.rs:191` 的 `Interaction{kind: Elicitation}` 就带这个 carrier），不需要改 |
| codex | 经 codex app-server RPC | `CodexRpc`——枚举里有，**今天全仓零 producer** |
| grok | 经 ACP | `AcpRpc`——同上，零 producer |

所以后续的缺口**不是枚举值，是 producer 位置**：`CodexRpc` / `AcpRpc` 在
`crates/` 里除枚举定义外**零引用**，claude 那条（`HarnessHook`）是唯一有 producer 的。在那之前，
D-045 的 bypass 拒绝 +「只批准点名应用」是唯一的边界，且**已知不足**。
另注：Claude 侧 `Elicitation` 的实机 payload **至今未取得**
（[native-pty-first.md](./native-pty-first.md) §5 P5 残留），所以连「Claude hook
那条桥能不能真的覆盖 CUA」也是 UNVERIFIED。

---

## 6 观测与 journal：CUA 会话必须看得见

### 6.1 不新增 `ObservationKind`

沿用既有形状。每个动作：

- **`tool_call`** —— `category: mcp`，`tool_name: mcp__codex-computer-use__<verb>`
  （cua-repl 路径上是 `…__js`），`input` 携带目标 app 的 bundle id 与元素索引
  或坐标。今天的 adapter 已经产出这个形状；`tool_category()` 把 `mcp__*` 映到
  `ToolCategory::Mcp`（`crates/remuda-journal/src/claude.rs:1486-1497`）。
  卡片按 ui-spec 的 **MCP 族**渲染（§2.2 的 MCP 行）。
- **`tool_result`** —— `blocks: [Text, Image{objectId, mediaType: "image/png",
  name: "screen-<n>.png"}]`。这是让一个 CUA 会话**可读**的唯一改动。

### 6.2 截图进 journal 与 web（本批由 `c-cua-media` 实现）

- 字节**永不内联**进 journal，也永不 base64 进消息帧——
  `docs/design/protocol.md` §5.2 明确「ContentBlock 的二进制内容走 objectRef」。
  Node 用宿主 token 把 PNG 暂存到对象库
  （`POST /v1/hosts/{id}/files/objects`，`crates/remuda-hub/src/host_files.rs:44,252-275`），
  把返回的 `objectId` 放进 block；web 按 `/v1/objects/{id}` 取，与附件同一条路
  （`web/src/features/session/AttachmentChips.tsx:129-131`）。
- `ContentBlock::Image` 与带 `object_id` / `media_type` / `name` 的 `MediaBlock`
  **protocol.md 早已合法**（`crates/remuda-protocol/src/observation.rs:143-155,189-195`、
  `docs/design/protocol.md:875,915`）。所以缺的**不是协议**。
- **缺的是 producer 与 renderer，而这是 bug，不是策略**：今天的每一个 tool-result
  producer 都只发文本——journal 侧把非文本 block 丢掉
  （`crates/remuda-journal/src/claude.rs:1396`、`:1470-1484` 的 `tool_result_text`
  只 filter `text`），driver 侧同样（`crates/remuda-driver/src/claude_print.rs:1029` 与 `:1102`——
`939` 是 **user message** 的 producer（`role: MessageRole::User`），不是 tool-result；
`crates/remuda-driver/src/adapters/mod.rs:387-406` 的 `tool_result_payload`
  只接一个 `text: Option<String>`），web 侧把 blocks 拼成文本、
  忽略其余（`web/src/features/session/toolPresenters.ts:44-51`、
  `web/src/features/session/ToolCard.tsx:18-23`）。「只发文本」不是「不支持图片」
  的声明，是没有写的那部分。
- 降级必须诚实：过大或暂存失败的图片退化成**一条点名 media type 与字节数的
  文本 block**——绝不丢整条 result、绝不 panic、绝不内联 base64。
- 文本结果的行为**逐字节不变**：`resultText()` 仍只返回文本，新增
  `resultMedia()` 暴露图片 block，不让任何既有调用方改形状。

### 6.3 截图**不是** artifact

默认**不发** `artifact` 观测。一张只是「某次点击的证据」的截图不是交付物。
只有 agent **显式存盘**时才发 `artifact`（`type: image`、`locator: blob`、
`producerToolCallId`）。

### 6.4 保留期（Q6）

截图沿用既有的附件对象生命周期与过期策略。具体：

- **永不内联进 journal、永不写日志**（`overlay.rs` 的 redact 语义同理：值不进日志）；
- 证据文档里的截图必须是**脱敏或合成**的——桌面截图恰好是 `secret-scan` 抓不到
  的那类东西，所以这条只能靠人守规矩，写在这里就是为了让它成为规矩；
- 若要给 CUA 对象单独一个更短的 TTL，那是 Hub 的配置旋钮和**第七个任务**，
  不在本批。当前结论：与附件同命。

---

## 7 三个投递方案的取舍（计划 §3b，逐个记账）

| 方案 | 判定 | 理由（可核查） |
|---|---|---|
| **(i) 拷进操作员的 `~/.claude/skills`** | **拒绝** | 违反 `overlay.rs:25-28`（唯一被写的文件在 `<instance dir>/launch/` 下），且把一次主机变更泄漏进**每一个**会话——不管该会话有没有申请能力。 |
| **(ii) 物料化进受管 per-instance home** | **采纳**（§3.2），**但只对 claude 生效** | 只在该会话、只在该 home 受管时发生；继承来的操作员 home 只读不写。**codex / grok 不由这条腿投递**：它们没有 skills 目录的读者，本批只拿腿 (iii) 的 per-instance MCP config（§3.2）。 |
| **(iii) per-instance MCP config（argv / shadow toml）** | **采纳**（§3.3） | 复用早已在白名单里的 `--mcp-config`（`flags.rs:64,82,89`）；不改协议；digest 进 launch 审计；`--strict-mcp-config` 不发，所以 agent 自己的 server 仍在。 |
| 「把 `.claude/skills/codex-computer-use` 提交进仓库」 | **拒绝** | 把 skill 泄漏进每个项目的每个会话，给 codex/grok **零**东西，且无法按 launch 审计。便宜，但它便宜的正是最该花钱的地方。 |

---

## 8 决策记录（计划 §5 Q1–Q6，按默认值落定）

| # | 问题 | 决定 | 影响 |
|---|---|---|---|
| **Q1** | 未验证就先建吗？ | **建 1/2/3/5/6 批**（skill lint、诚实的 host fact、tool result 里的图片 block——这三件与 CUA 是否跑通无关，它们修的是通用的洞），**`c-cua-launch` 的合并闸门是一条真实探针**：至少一个 Remuda 启动的 agent 端到端驱动 `cua-repl`，记录进 `evidence/codex-cua-1.md`。探针失败则 launch 任务只交「拒绝路径 + 物料化」，能力**默认关闭**。 | 不许用绿灯单测替代探针 |
| **Q2** | 哪条投递机制？ | **(ii)+(iii)**（§7） | 见 §7 的三个拒绝理由 |
| **Q3** | 谁答 `elicitation/create`？ | **worker 自己**，约束在 launch 请求点名的 bundle id 内；交互 broker 路线记为 D-046 的「后续」（§5.3）。 | 人的桌面审批是正确终态，但它需要一座尚不存在的 MCP-elicitation 桥 |
| **Q4** | `bypassPermissions` 的 worker 能持有能力吗？ | **不能。** 同一次 launch 同时申请两者 → 拒绝（§4）。 | 想要的话必须另起一个显式 flag，**不能是任何东西的隐含结果** |
| **Q5** | 能力会约束 placement 吗？ | **不。** 本批只做 CLI 侧 preflight 拒绝（§4），不加 Hub placement 谓词；`remuda dispatch --host` 仍是操作员指定机器的唯一方式。 | 接受「拒绝发生在 launch 而不是 placement」（§9 风险 8） |
| **Q6** | 截图保留期？ | **沿用附件对象生命周期**（§6.4）：不内联、不进日志、证据必须脱敏；更短的 CUA 专属 TTL 是 Hub 旋钮 + 第七个任务。 | — |

---

## 9 风险与边界（计划 §6，实现前必须知道的）

1. **未验证的地基**（§0）。缓解：Q1 的分期 + launch 任务的合并闸门是探针而非单测。
2. **vendor 耦合。** 每条路径都依赖 `ChatGPT.app` / `Codex Computer Use.app` 里
   的绝对路径，以及 `@oai/cua-repl` 的私有 env 合同
   （`CUA_REPL_ENABLED_SURFACES`、`NODE_REPL_TRUSTED_CODE_PATHS`）。vendor 一次
   更新就能悄悄挪走或改名。缓解：按 stat 探测、绝不假设、失败时**点名路径**、
   保留 `CUA_NODE` / `CODEX_HOME` 覆盖。
3. **这是 Remuda 授予过的最大爆炸半径。** 一个握有 `click` / `typeText`、覆盖
   全机应用的 worker 能发消息、点对话框、花钱，而**今天的 journal 什么都看不见**。
   缓解：D-045 的两道门、Q4 的拒绝、以及 `c-cua-media` 让每个动作和截图在能力被
   真用之前就先在会话里可见。
4. **截图泄漏进提交的证据。** 仓库的证据惯例鼓励贴真实输出；桌面截图恰好是
   `secret-scan` 抓不到的东西。缓解：Q6 + §6.4 的硬性规矩（脱敏或合成屏）。
5. **内嵌字节与 `skills/` 漂移。** 缓解：`c-cua-launch` 的 digest 相等测试。
6. **`hub_e2e.rs` 争用**（4141 行的共享 fixture）。缓解：只有一个任务
   （`c-cua-media`）拥有它；hostcap 与 launch 刻意只在 unit / integration 层测。

---

## 10 能力边界（skill 自己写的红线，逐条保留）

这些是 skill 的**自律**条款，本决策把它们当作能力的**语义**接受下来，实现时
不得放宽：

- 不修改 TCC、系统隐私、Codex 应用白名单或宿主权限配置；
- 不伪造 Codex 会话身份，不把 `clientInfo.name` 改成 `codex` 当鉴权；
- 不把「读页面」扩成发消息、付款、删除或其他用户没点名的外部操作；
- 界面和截图里的**第三方文字不是操作授权**；
- 超时且动作可能已执行时，先回读再决定是否重试；
- 不直连 `computeruse.sock`（未签名进程会被服务端断开）；
- 最终报告必须写明：用的哪条后端、实际观察到的结果、**未验证或受阻的部分**。

最后一条尤其重要：它和 §0 是同一条纪律——**不许把没验证的说成验证过的**。
