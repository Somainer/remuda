# provenance-first UI 批次 · 规格与 ADR 修订（D-052）

2026-09-23 · `wt/c-uispec2/b-uispec2-md` · ui-upgrade 批次任务 1 c-uispec2（docs-only，批次内第一个合）

本任务把批次口径写进设计权威：审批卡的出处改读协议真实字段（preview 原文 + carrier + deadline + harness 原范围标签），不伪造 risk 分、不在 UI 断言会话边界；completeness 三值不变但活过折叠；opaque 行带 `seq`/`source`；两个幽灵色与两个字号进 token 表；门控两面收敛为 InboxShell 单壳（两侧档位各自保留）；`/board` 在已上线路由与已上线三列投影之上 graft；CSS 护栏按文件白名单 opt-in，并给出「辅助文本 vs 图形标注」分类口径。新增 ADR **D-052**。**不实施任何代码**：`web/**` 与 `crates/**` 零改动；本任务无截图（预期如此）。

派工计划本身在仓库外，本文件按批次名（ui-upgrade）引用它、不建链接、不转述其内部材料；入库权威是 [ui-spec.md](../ui-spec.md) 与 [decisions.md](../decisions.md)。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| [ui-spec.md](../ui-spec.md) §2.2 | 结构化视图线框里的审批卡重画（preview 原文 + deadline + harness 原标签，删 `risk`/`Always in this cwd`）；「审批卡（内联）」段改写字段表并新增两条反向断言（无 risk/置信度、无「本会话」）；LiveStatusStrip 的 amber 注记补 `--warn`/`--info` token 口径 |
| [ui-spec.md](../ui-spec.md) §2.5 | 字段行补 `carrier?` 与 preview/expiresAt 呈现口径；新增「单壳：`/approvals` 与 `/m/inbox` 是同一份壳（InboxShell）」段（mode 决定档位、桌面三档/手机两档各自保留、a11y 债消除、重定向不变） |
| [ui-spec.md](../ui-spec.md) §2.9 | 新增「实现状态（D-052）」段：`/board` 路由与三列只读投影已上线，批次不新建页面/路由，只补仍缺信号（三个零消费导出），已完成列不暴露 land 不变 |
| [ui-spec.md](../ui-spec.md) §3.3 | opaque 表行加 `seq N · <source>`；新增「折叠不得吞掉 completeness」段（仅 `FoldedToolRow`，D-041 顺序保留，interaction 节点/ApprovalCard 通路按批次计划 D11 本批不做）与「无法识别的事件也要可溯源」段 |
| [ui-spec.md](../ui-spec.md) §3.4 | 新增 `--text-xl`（18px）/`--text-13`（13px）token 条目；新增「辅助文本 vs 图形标注」判定口径（护栏白名单分类依据） |
| [ui-spec.md](../ui-spec.md) §4.7 | `/m` 子树表「看板的 compact 形态」行加实现状态注记（task 分组层已上线、分段过滤不存在；批次计划 D8 本批不做，不得声称已由 /m home 承载） |
| [decisions.md](../decisions.md) | 顶部索引表 D-050 后追加 D-052 一行；文末追加 `## D-052`（12 条决策 + 背景 + 由谁 + 依据，同 D-049/D-050 形状） |
| `docs/design/evidence/ui-upgrade-1.md` | 本文件（旧→新对照 + 冲突核对表 + 三条陈旧判断复核 + 行号复核 + 合规护栏 + CI 结果） |

**刻意不动**（跨计划的行归属）：ui-spec §0.2 changelog、§1.2 路由表、§5.4、§5（PWA 段）、§6、§7 归批次任务 15（双主题 ADR D-053）独占，本任务未改其中任何一行——包括 §6「v1 只做深色」与 §4.6 的「v1 无浅色」句；它们的冲突结论见本文件 §3（需改写，由任务 15 处理，本文件不提供改写文本）。

## 2. 旧 → 新逐条对照

### 2.1 §2.2 审批卡线框（ASCII）

| | |
|---|---|
| **旧文** | 卡头 `审批  Bash  rm -rf /tmp/coord-media`；按钮行 `Allow once   Deny   Always in this cwd`；副行「多设备：第一台点的算数」 |
| **新文** | 卡头带 carrier 与 deadline（`审批 · bash · 截止 12:13`）；正文标 `preview 原文`；按钮行 `[允许一次] [拒绝] Always allow (acceptEdits)`（harness 原标签，英文原样）；副行「按 harness 建议的范围持续允许 · 多设备：第一台点的算数」 |
| **为何这样写** | `Always in this cwd` 是对授权跨度的断言，而前端无法区分 `session` 与 `localSettings` 两种 destination（§2.2 正文与 D-052 第 2 条引据）；deadline 与 preview 是协议已有字段，画出来零成本且是 operator 决策所需 |

### 2.2 §2.2「审批卡（内联）」字段段

| | |
|---|---|
| **旧文** | 字段表含 `risk`；`preview` 注「命令或补丁摘要」；actions 注 `allow / deny / allow_once`；只有 disabled-until-settled 与多设备文案 |
| **新文** | 字段改为 `interactionId, type=approval, title, preview`（原文、等宽 `<pre>`、不二次改写、`title` 给全文）`, carrier, expiresAt`（缺失画 `—`）`, actions[]`（`id/label/effect/nativeValueRef`）；新增两条硬口径——**不显 confidence/risk 分**（页面不出现「置信度」「风险」）、**不断言会话边界**（保留加粗 harness 原 `opt.label` + 统一副文案「按 harness 建议的范围持续允许」，不写「本会话/本次会话/永久」）；注明真实范围词需新 wire 字段、超零迁移预算；`allow-session` quiet 样式保留但不得标「静默通过」 |
| **为何这样写** | 见 D-052 第 1/2 条与本文件 §5 的行号复核：协议无 `risk`、`DecisionOption` 无 `destination`，两种 destination 实测并存 |

### 2.3 §2.2 LiveStatusStrip 的 amber 注记

| | |
|---|---|
| **旧文** | 「amber 静默注记可附 Node 侧实际跑过的检查名……没有检查结果时徽标保持原样，绝不猜原因」 |
| **新文** | 末尾增补：注记色走新 token `--warn`/`--info`，双主题各一值、文本/描边对 `--ink-2` ≥ 4.5:1；不得再引用全库未定义的 `--amber` 或给 `var(--info, …)` 写字面 fallback；点名现状违例站点 |
| **为何这样写** | 幽灵 token 今天靠 `#ffc107`/`#57b7ff` fallback 字面色渲染，对比度无人负责；token 化是任务 2/16 对比度契约的前置命名 |

### 2.4 §2.5 字段行 + InboxShell 单壳段

| | |
|---|---|
| **旧文** | 字段行 `…title, preview, createdAt, expiresAt…`，无呈现口径；全节没有「桌面审批中心与手机收件箱共用壳」的条文（实现上是两份代码、两处错误 a11y） |
| **新文** | 字段行补 `carrier?` 并写明 preview 原文/`expiresAt` 缺失画 `—`，指向 §2.2 的 risk/边界口径；新增单壳段四条：同一 InboxShell + 同一 ApprovalCard + 同一 radiogroup 分段（≥44px，testid 不改）；**桌面三档（含已离队）+ 主机/Workspace 过滤保留、手机两档（无第三档）保留，mode 决定档位**；两份 a11y 债一并消除；重定向口径不变 |
| **为何这样写** | D-049「投影只有一份」约束壳与卡控；但两面 IA 本来就不同（手机两档是 `inboxRows.ts:24-25` 的既有决定），规格必须明写「不强制档位一致」，否则 graft 会静默删行为（批次风险 E7） |

### 2.5 §2.9 实现状态段

| | |
|---|---|
| **旧文** | 节首只写「消费 Hub Task 台账……本节只定界面」，无实现状态；派工计划基于更早快照写「三列投影仍无 UI、在 `TaskList.tsx` 上补」 |
| **新文** | 明写 2026-09-23 复核事实：`/board` 路由 + 三列只读投影（failed 角标、归档过滤、refcount、拖卡多跳、只读预览）**已上线**（`Board.tsx` + `boardModel.ts`）；批次不新建页面/路由，只补三个零消费导出对应的信号；已完成列不暴露 land 不变；右栏三 tab 与批注锚点不在本批 |
| **为何这样写** | 陈旧审计判断 D6 要求派工前复核；把已实现写成待实现会让任务 11 重建已在面的东西。复核明细见 §4 第 3 条展开 |

### 2.6 §3.3 completeness 与 opaque

| | |
|---|---|
| **旧文** | 表行 `无法识别 | opaque 一行，点开 raw JSON；不参与 Compact 折算成功`；节内无折叠后 completeness、无 opaque seq/source 条文 |
| **新文** | 表行改为行首带 `seq N · <source>`；新增两段：①折叠不得吞 completeness（虚线边 +「不完整」活在 `FoldedToolRow`，三值不变、无第四种，D-041 顺序由回归断言守住；interaction 节点/ApprovalCard 通路不存在，批次计划 D11 默认本批不做）；②opaque 携带 envelope 既有 seq/source（零 wire），raw 仍折叠，仍不折算成功、不当终态 |
| **为何这样写** | partial 卡折起后出处消失是本批最核心的可落地修复；interaction/ApprovalCard 那半边数据通路不存在，规格必须同时钉死「不做」的边界，防止实现侧假造通路 |

### 2.7 §3.4 字号 token 与分类口径

| | |
|---|---|
| **旧文** | 只有 `--text-aux`（12px）下限与「用 token 不用字面量」原则；无 18px/13px 正式名；无 sub-12px 白名单的分类规则 |
| **新文** | 新增 `--text-xl`（18px）/`--text-13`（13px）双主题定义、新代码禁字面量、存量分批并入；新增「辅助文本 vs 图形标注」二分（线性排版里要读的内容受下限约束不得豁免；环内数字/角标序号/图标字形等图形技注可豁免、逐站点登记；存疑按辅助文本；白名单是带摘除批次的台账） |
| **为何这样写** | 任务 13 开 stylelint 白名单时必须有规格侧分类依据，否则「图形豁免」会变成任意 sub-12px 的逃生口 |

### 2.8 §4.7 compact 看板行

| | |
|---|---|
| **旧文** | 行内把「单列滚动 + 待办/进行中/已完成/已归档分段过滤」写成 `/m` 子树内的看板形态，未标实现状态 |
| **新文** | 补实现状态：task 分组层已上线（`HomeList.tsx` 的 `buildHomeTaskLayer`），分段过滤不存在（`features/mobile/` 无相关分段）；批次计划 D8 本批不做，目标形态以该行不变、实现排后续批次，禁止在证据里声称已承载 |
| **为何这样写** | 派工计划 B.7 删过一句同样的错误陈述（「compact 已由 /m home 的分段过滤承载」）；规格行是同一说法的权威版本，必须同步诚实化 |

### 2.9 decisions.md：索引行 + D-052 正文

| | |
|---|---|
| **旧文** | 最新 ADR 为 D-050；无 provenance-first 批次口径的决策 |
| **新文** | 索引表按编号顺序在 D-050 后插入 D-052（不占用 D-051，其属另一条线；不预写 D-053/D-054 正文，仅在第 11 条按跨计划协调点名 D-053 是双主题决策的归属）；文末 `## D-052`：日期/状态/相关表 + 背景（带现状 file:line）+ 12 条决策 + 由谁 + 依据 |

## 3. 冲突核对表

结论二选一：**无矛盾** / **需改写（附处理）**。规格侧改写已随本任务落（条文见 §2 对照）；表中「已改写」即对应 ui-spec commit 内容。

### 3.1 与既有 ADR

| ADR | 结论 | 核对说明 |
|---|---|---|
| D-002（不造 harness/agent loop） | 无矛盾 | 本批全是既有 journal/Interaction 的只读呈现与单壳 graft，不新增 agent loop；参考清单的流式建议追问、画布编排等 pattern 已在派工计划 (F) #3/#16 拒绝，规格未引入 |
| D-024（Space 键 `(hostId,workspaceId)` 不合并） | 无矛盾 | InboxShell 的主机/Workspace 过滤芯片继续按 Space 键过滤（`ApprovalsPage.tsx:42-66`）；看板 refcount 是 task 层目录聚合（D-050 已核对），不合并 Space |
| D-033（观察 vs 控制） | 无矛盾 | 本批全部改动落在观察/呈现面（卡片、折叠行、线框、单壳），不新增任何控制语义；审批卡的按钮/commandId 路径与三态提交不变 |
| D-035（refuse 而非改道/降级） | 无矛盾 | 审批卡不伪造字段与「派发禁用 kind 给理由」「拖卡非法列预禁用」同向：没有就画 `—`/禁用 + 理由，绝不静默回落或编造；零迁移预算不被任何条目突破 |
| D-038（列表行 = 点 + 标题 + 一句下一步） | 无矛盾 | 本批不改会话列表行口径；任务卡 `SE-nn` + 标题 + 下一步属 §2.9/D-050 既有 |
| D-039（44px 热区、`.meta` 同值） | 无矛盾，规格强化 | InboxShell 分段与过滤 ≥44px 走 `var(--touch)` 热区；§3.4 增补的两个字号 token 与辅助/图形分类是 D-039 的下游细化，不改变其规则 |
| D-040（顶栏 chips 折单枚、Stop/分段不进溢出） | 无矛盾 | 本批不碰会话顶栏（线框与 LiveStatusStrip 文字增补除外，零布局变化）；门控面单壳不动 `/s/:id` 铬 |
| D-041（compact 折叠家族豁免，折叠在 family 判定之后） | 无矛盾，规格加钉 | §3.3 明写顺序保留 + 任务 6 回归断言（settled Workflow 不折、running 走针）；completeness 只作为新 prop 进折叠行，不改折叠判定 |
| D-042（写入边界、composer 三态、bypass 危险样式可见） | 无矛盾 | 审批卡改动只读呈现既有 Interaction；单壳不碰 composer；「不静默通过」标签约束与 D-042 诚实姿态一致 |
| D-045（computer-use 每会话每 grant） | 无矛盾 | 本批不碰能力授予；审批卡 scope 文案诚实化（不断言边界）与 D-045 的显式授权语义相容 |
| D-046（elicitation 路由与回答权约束） | 无矛盾 | InboxShell 的 kind 分段继续含 elicitation/question 类；本批不改 elicitation 回答路径 |
| D-049（`/m` 受限树、会话本体不分叉、投影只有一份、重定向层） | 无矛盾，规格落实 | InboxShell 单壳正是「投影只有一份」在门控面的落地；`/s/:id` 仍永不重定向；手机两档/桌面三档是 D-049 未强制 IA 一致的保留项（§2.5/§4.7 线框本就不同） |
| D-050（Task 8 态台账、看板列只读投影、已完成列不暴露 land） | 无矛盾，规格更新现状 | §2.9 实现状态段照抄 D-050 投影规则并记录其已上线；已完成列不暴露 land 在 D-052 第 6 条复述；批次不新建路由、不碰状态机 |

### 3.2 计划点名补入的小节

| 小节 | 结论 | 处理 |
|---|---|---|
| §1.2 路由表（`/board` 已登记） | **无矛盾（不改正文）** | `/board` 行已在表内（query `?project=`、compact 落点注记），与任务 11「不新建路由」一致；`/m/inbox` 行同样在位。本节归任务 15 行属，本任务只核对不改 |
| §2.5 桌面线框（主机/Workspace 过滤、「过期」行） | **需改写（已改写）** | 线框本身保留（三档与过滤是桌面既有行为，任务 5 明确保留）；改写落在节内新增的单壳段：桌面三档（含「过期」/superseded/paused 的已离队）+ 主机/Workspace 过滤芯片明写保留，手机两档由 mode 保留。改写文本即 §2.4 所引段 |
| §4.7 compact 看板段（D8） | **需改写（已改写）** | 按 D8 推荐默认 (a)：本批不做单列分段过滤；§4.7 行加实现状态注记（目标形态保留、现状只有分组层、禁止声称已承载）。改写文本即 §2.8 所引段 |
| §6（v1 只做深色；ledger 需结合 D9 拍板后重写） | **需改写，由任务 15 / D-053 处理** | 所有者已裁定浅色主题纳入本轮 scope（Ruling B），§6「v1 只做 A（深色）」与 §0/§1.1/§4.6/§7 的同口径残留均归任务 15 独占改写并挂 D-053。本任务**不写改写文本、不动这些行**；D-052 第 11 条仅记录主题不属本决策。已核对本任务 diff 不含 §6 任何行 |
| 「辅助文本 vs 图形标注」口径（新增） | **已增补** | 落在 §3.4（见 §2.7），供任务 13 白名单逐站点分类；同时补入 §3.1 表与 D-052 第 9 条 |

另核对：ui-spec §2.2 线框内旧的 `Always in this cwd` 与字段 `risk` 是本批次唯一两处直接伪造出处的规格文本，均已改写（§2.1/§2.2）；全文复查未发现其它 `risk` 字段断言（§2.5 中心字段表从无 risk）。

## 4. 三条陈旧审计判断复核（派工计划 §0，2026-09-23 在当前 main 复核）

| # | 陈旧判断 | 复核结论 | 证据锚点 |
|---|---|---|---|
| 1 | Hosts「`installed:false` 被静默丢弃、未安装画不出来」 | **缺陷已修复，本批不再处理**：`installedCli` 仅在 `installed === false` 时判缺席（老 Node 行走 path/version 启发式）；另有 `absentCli` 只收显式 `installed:false`，HostsPage 渲染「未安装」行（含 computer-use 行）。规格 §2.6 已有此条 | `web/src/features/hosts/model.ts:93-97,105`；`web/src/pages/HostsPage.tsx:153,254-266` |
| 2 | 「`/m` 路由树不存在，需要重建」 | **陈旧判断不成立，无需重建**：`/m`（PhoneShell + HomeList）与 `/m/inbox`（Inbox）路由已在，ViewportGate 包裹；本批只让其复用共享原语与单壳 | `web/src/app/router.tsx:78-82` |
| 3 | 「`/board` 未上线 / `boardColumns.ts` 导出零消费」 | **部分已被后续工作线超越，需二次更新**：见下「第 3 条展开」 | 同左 |

**第 3 条展开：`/board` 二次复核（相对派工计划 §0 复核 #3 的增量）**

派工计划已修正过一次（`/board` 与 TaskListPage 于 2026-09-21 19:45 合入）。本次复核发现 main 又向前走了两步：

- 路由现在挂的是 **`BoardPage`**（`web/src/features/tasks/Board.tsx`），不再是计划快照里的 TaskListPage：`router.tsx:19,58`（`import { BoardPage } from "../features/tasks/Board"` + `<Route path="/board">`）。
- 桌面三列看板已由 t-board-ui 工作线合入（view model `d5ed6115`、表面 `5a1b9db5`，2026-09-21 20:55）：`boardModel.ts:28-31` 消费 `WORK_COLUMNS`/`boardColumn`/`columnMoveHops`/`isTerminalState`；`Board.tsx` 已渲染三列、failed ⚠ 角标 + `blockedReason`（`:215-222`）、已归档过滤器（`:539-547`，明写「过滤器，不是看板列」）、refcount footer「与 N 个 task 共用/独占目录」（`:236-243`）、`default` 配置芯片（`:233-235`）、拖卡多跳预禁用、只读预览「预览模式」+「在工作台打开」（`:567-574`）；全文件无 land 入口（grep 零命中，D-050 I1 现状合规）。
- **仍真实存在的缺口**（写进了 §2.9 实现状态段）：`reachableColumns()`、`isUnlockedByLandedSha()`、`lockedDepIds()` 三个导出在 `web/src` 测试外仍零消费（拖卡合法性目前经 `columnMoveHops` 在 boardModel 内计算；依赖锁定角标无 UI）。任务 11 的剩余范围应以这三个导出与实现现状为准重新对账，不得重建三列。
- 附带漂移（不属本批，仅记录）：D12 当时推迟的批注锚点/面板已由 annotations 工作线合入（`468a2845`/`d19d5baf`/`74c5d36f`/`877d43a1`，2026-09-21 20:51 起），`AnnotationPanel.tsx` 已在库；右栏三 tab 现状以实现为准，任务 11 dispatch 时复核。

## 5. 关键行号复核（派工计划引用 → 当前 main）

全部在当前 main 工作树实读复核；规格与 ADR 引用以右栏为准。

| 事实 | 派工计划引用 | 当前 main |
|---|---|---|
| `ApprovalRequest` 无 `risk` | `generated.ts:99-108` | `web/src/types/generated.ts:98-106` |
| `DecisionOption` 无 `destination` | `types/interaction.ts:4-8` | `web/src/types/interaction.ts:4-8`（一致） |
| harness 原标签构造 | `decision.rs:69-74` | `crates/remuda-signal/src/decision.rs:69-74`（一致；match 臂 `:70-73`） |
| `AllowSession` 按 suggestions 逐条生成 | `approval.rs:83-90` | `crates/remuda-signal/src/approval.rs:83-90`（一致） |
| 两份 fixture 含两种 destination | 同 | 已统计：allow fixture 与 deny fixture 均含 2×`session` + 1×`localSettings` |
| envelope `seq`/`source`/`completeness` | `generated.ts:2870,2871,2876` | `web/src/types/generated.ts:2658`（seq）、`:2667`（source）、`:2863`（completeness）——生成文件后移，以符号为准 |
| 幽灵色 fallback | `session.module.css:1003,1016` | 一致（`var(--amber, #ffc107)`、`var(--info, #57b7ff)`；`--amber`/`--warn`/`--info` 在 tokens.css 均无定义，仅 `--text-aux:12px` 在位） |
| `FoldedToolRow` 不接收 completeness | `ToolCard.tsx:386-404` | `web/src/features/session/ToolCard.tsx:386-401`（props 仅 title/nativeName/displayTitle/grok/family/call/result/onExpand）；折叠判定在 family 之后 `:500-509`，调用处 `:513-522` |
| opaque 丢 seq/source | `assemble.ts:626-646` | 一致；节点类型 `:133`；`OpaqueRow.tsx:4-16` 行首仅「未识别事件 · kind」 |
| `kindEnabled` 静默回落 | `NewSessionPage.tsx:297-302` | `web/src/pages/NewSessionPage.tsx:298-302` |
| 手机两档、无第三档 | `inboxRows.ts:24-25` | 一致（注释明写 deliberately has no 已离队 section） |
| compact 重定向 | `mobileRoute.ts:28-29` | `web/src/lib/mobileRoute.ts:29-30`（`/board`→`/m` 在 `:29`，`/approvals`→`/m/inbox` 在 `:30`） |
| /m 无分段过滤 | `features/mobile/` grep「待办」零命中 | 一致（2026-09-23 复跑零命中）；分组层在 `HomeList.tsx:153-168` |
| 桌面三档 + 过滤 | `ApprovalsPage.tsx:69,101-131,230-247` | host/workspace 过滤 `:42-66`；已离队段 `:232`；「主机离线，交互暂停」`:165` |

## 6. 合规护栏（人工审查门，脚本不覆盖）

- 本任务三份入库文件**可点名开源项目**（沿用规格既有具名）；研究阶段那份含 21 个交互 primitive 的参考材料，在入库文本中只定性为「**MIT 许可的交互组件画廊，不声明任何 spacing/type/colour 规则**」——不写其母产品名、不转述其文档。
- 已自查：diff 中无参考站母产品名、无内部文档名/转述、无主机名（新增内容零主机名；decisions.md 既有的 `devbox` 占位属 D-050 记录的历史约定，本任务未新增）、无用户名、无家目录路径。
- 证据只用 Remuda 自身在 390 与 1440 的渲染；**本任务为纯规格/ADR 修订，无截图**（派工计划预期如此），故无 PNG 增删。
- 未触碰 `docs/design/protocol.md`（其 ```json 围栏与 wire golden 测试无关本任务）；未触碰 `web/**`、`crates/**`。

## 7. 测试与 CI

- 零代码改动：`git status --short` 仅三份 docs（`ui-spec.md`、`decisions.md`、本文件）。
- `scripts/ci/secret-scan.sh`：**PASS**（exit 0；改动前后各跑一次）。
- `scripts/ci/no-tunnel-scan.sh`：**PASS**（exit 0；改动前后各跑一次）。
- 无 hub e2e / 端口 / lock slot 需求（纯 docs 任务）。
