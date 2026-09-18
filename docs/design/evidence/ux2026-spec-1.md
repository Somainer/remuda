# workbench UX 报告 · 规格修订（D-038…D-042）

2026-09-19 · `wt/c-uxspec/b-uxspec-md` · 任务 c-uxspec（[workbench-ux-plan.md](../workbench-ux-plan.md) §(B) 1）

本任务只改文档，目的是解除 [workbench-ux-improvement-2026-09.md](../workbench-ux-improvement-2026-09.md) §11.7 列出的「规格与实施单互相矛盾」，使后续实施单不再与 [ui-spec.md](../ui-spec.md) 自相矛盾。**不实施任何代码**，`web/` 与 `crates/` 零改动。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `docs/design/workbench-ux-improvement-2026-09.md` | 新增（源报告入库）。除下述三处外与源文件逐字节相同 |
| `docs/design/ui-spec.md` | 六处修订，见 §2 |
| `docs/design/decisions.md` | 追加 D-038…D-042，见 §3 |
| `docs/design/evidence/ux2026-spec-1.md` | 本文件 |

源报告入库时按任务要求把 §11.3 行 423 的个人 workspace 名（原 `remuda-sg`）替换为中性占位 `<workspace>`；已验证其余 497 行与源文件完全一致（`diff` 去掉该行后无输出）。

## 2. `ui-spec.md` 逐条：旧文 → 新文 → 对应 §11.7 冲突行

### 2.1 §2.1 列表行 = 点 + 标题 + 一句下一步（新 D-038）

| | |
|---|---|
| **§11.7 行** | **P0-6 列表行去 wire 串** — 结论 **READY（规格反而要求）**：「`ui-spec.md:174-183` 的 wireframe 本来就是「点 + 标题 + 一句」，没有 `ready · waiting-interaction · connected`，也没有 `ins_`…**当前代码才是偏离。**」 |
| **旧文** | §2.1 只有线框 + 「手机：无航轨；顶搜索+筛选 chips；底栏。待处理置顶，不进折叠组。」一句；**行内可出现哪些文本没有任何明文规定**，实现据此把三维 wire 渲染进了行内 |
| **新文** | 新增「**行内容 = 状态点 + 标题 + 一句下一步（D-038，2026-09-19 增补）**」：默认视口只出现状态点 / 标题 / kind 芯片 / 徽标 / 一句下一步；`lifecycle`/`activity`/`connectivity` 原文三元组、`ins_`、`driver`、`model` 退到 `<details data-testid="session-wire">` 或 `title`；下一步只投影已有字段，`connectivity ≠ connected` 与 `unknown/reconciling` 必须产出「状态待确认」；行内遥控收进 `Sheet`（保留 testid 与命令路径），待处理组保留一个主操作 |
| **为何是「澄清」而非「新增要求」** | §2.1 线框与「三维投影、不是单独 wire 枚举」的既有句子已经蕴含这一点；新增段落把它写成可验收的明文，并补上原规格没写的两件事：**unknown 不得回落成正向文案**、**行内遥控收进 Sheet 但 testid 不迁移语义** |

### 2.2 §1.3 / §1.4 compact space chips 与顶栏必留项（新 D-040）

| | |
|---|---|
| **§11.7 行** | **P0-1 手机顶栏减铬** — **CONFLICTS**：`:156` 要求「约 400px 手机上显示可横向滚动的 space chips 与 tabs」且 §1.4 自称「本节只作用于会话工作台」，`/s/:id` 正在其内；`:113` 把 `terminal\|structured` 切换列为顶栏必有项，「若挪进 ⋯ 即违规」；并指出该建议**与自己矛盾**（P1-8 与 §10-19 要求该开关**更**显眼） |
| **旧文 §1.3** | `- 会话页顶栏：标题、status 点（§2.1 投影）、terminal\|structured 切换（仅 tty-attachable）、Stop、主机/项目芯片。pty-backed 默认 terminal，structured 是第二视图。` |
| **新文 §1.3** | 同句 + 「这排元素在 compact 下允许重排与减铬，但 **`terminal\|structured` 切换与 Stop 必须始终留在顶栏，永不进 ⋯ 溢出菜单**（D-040）…诊断字段（driver / delegation / provider / lifecycle / seq / connectivity / native）允许折进「运行详情」（§2.2）。」 |
| **旧文 §1.4** | `- 桌面快捷键：⌘/Ctrl+B 折叠面板，…约 400px 手机上显示可横向滚动的 space chips 与 tabs，左侧面板通过抽屉访问；…` |
| **新文 §1.4** | 保留该句原样，**另起一条**限定其适用范围：「**compact 下的 space chips 例外（D-040，2026-09-19 增补，仅覆盖上面那句 chips 要求的应用范围）**」——列表路由保持整条 chips；`/s/:instanceId` 及子视图在 compact 下允许折成单枚当前 space 芯片入顶栏，点开同一个抽屉（`spaces-drawer-open` 行为与 testid 不变） |
| **处理方式** | 不改旧句（避免与 `spaces-1.md` 的验收口径脱节），而是**显式限定其作用域**并在同处写明例外的理由；`terminal\|structured` 与 Stop 的「永不进 ⋯」写进 §1.3 而非 §1.4，因为它是顶栏语义不是 space 语义 |

### 2.3 §2.2 两行 header → 主行 + 运行详情（新 D-040）

| | |
|---|---|
| **§11.7 行** | **P0-5 桌面 meta 默认折叠** — **CONFLICTS**：`:229-236` 把两行 header 画成**规范 wireframe**，第二行（修订前 **:235**）就是 `seq 184 · connectivity=connected · $0.12`；`:113` 要求主机芯片在顶栏、`:331` 要求 cost 在顶栏；「要折叠必须同时改 §2.2 的 ASCII 图」 |
| **旧文（线框）** | `┌  ← 列表   sfe-root / spill  · bolt · claude-print · passthrough/…  ● working   [结构] [■]` / `│  seq 184 · connectivity=connected · $0.12` |
| **新文（线框）** | 第一行 `[结构] [■ 停止]`；第二行 `│  ● bolt  ·  $0.12  ·  ▸ 运行详情`；并新增「运行详情展开后」的第二张图（`driver claude-print · delegation none · provider passthrough · lifecycle ready` / `seq 184 · connectivity connected · native claude:1a2b · transcript 绑定 hook`） |
| **新文（正文）** | 新增「**两行 header（D-040，修订 §11.7 冲突「P0-5」）**」：主行 = 返回 + space/标题 + harness + model + 状态点 + 分段 + Stop；**host 芯片与 cost 必须留在可见主行**（保住 `:113` / `:331`）；其余诊断进第二行「运行详情」disclosure，默认收起、展开态**按设备持久化**、`session-meta` testid 保留在展开内容上；compact 不加第三层 |
| **处理方式** | 旧图与旧文都被替换/增补，不留矛盾表述；同时保留 `:331` 的 cost 与 `:113` 的主机芯片在**主行**，因此这两条既有要求无需修改 |

### 2.4 §2.2 工具卡折叠的豁免与最小信息量（新 D-041）

| | |
|---|---|
| **§11.7 行** | **P1-10 / P1-20 ToolCard 默认一行** — **CONFLICTS**：`:290` 与 `:307` 要求 `workflow.run` 运行中与结束后都展开、只有读者 dismiss 才折叠；`:292` 要求 `error` 卡必须展开；「代码里 `ToolCard.tsx:260` 的 folded 分支在 Workflow 分支（`:281-292`）**之前** return，一律默认折叠会让 `WorkflowTimelineCard` 永不挂载、1Hz 走针不启动」；`:303` 要求 Bash「命令一行」，折成裸「Bash」违规 |
| **旧文** | tool 表之后只有 `Generic：tool 名 + 折叠 JSON 入参/出参。` 与 `Diff 三分态文案：拟修改 / 已写入 / 结果未知。…`——**没有任何默认折叠规则**，实施会自由发挥 |
| **新文** | 新增「**手机默认折叠（D-041，修订 §11.7 冲突「P1-10 / P1-20」）**」5 行表：生效条件（compact **且** settled；running 永不折叠）、豁免集合（`Workflow` / `error` / `interaction.*`）、折行内容（family + 关键参数；Bash = 命令首行、Edit/Write/Read = 路径；裸 `Bash` 不合规）、实现约束（**折叠分支必须在 family 判定之后**；验收断言卡头 elapsed 递增而非「卡可见」）、展开态（与桌面完全一致，同一组件与 testid） |
| **处理方式** | §11.7 只说了「冲突 + 陷阱」，没有给出折行内容的下限；本行按计划 §(C) 第 3 条默认值补上「必须带关键参数」，使 `:303` 的「命令一行」在折叠态仍成立 |

### 2.5 §2.2 compact composer 边界（新 D-042）

| | |
|---|---|
| **§11.7 行** | **P0-3 手机 composer 收成一条** — **CONFLICTS**：`:339-340` 要求 effort 保持可见的收起触发器并显示**档名**；`:337` 要求权限芯片**显示**当前 `permissionMode`，且 `permissions.ts` 把 bypass / dontAsk 一类标 `danger`，「把 bypass 态藏进 sheet 是这里最响的冲突」；`decisions.md:57`（D-028a）要求三态 + 队列 chip + 「尚未验证」，若做 options sheet，**触发器必须带 mode 词**、三态与诚实标注不得入 sheet |
| **旧文** | §2.2「Composer」只有零散子弹（权限芯片要显示 `permissionMode`、effort 触发器只显示档名、`role=slider` 等），**没有 compact 口径**；placeholder 与 `window.confirm` 完全未被规定 |
| **新文** | 新增「**compact composer 边界（D-042，修订 §11.7 冲突「P0-3」）**」：触发器必须同时显示 `permissionMode` **词**与 effort **档名**（`manual · high`），`danger` 模式必须在触发器上可见；可进 sheet 清单（附件 / harness 只读芯片 / context 用量 / 权限选择器 / effort 滑杆）；**不得进 sheet** 清单（三态按钮、队列 chip、「尚未验证」标注，D-028a）；placeholder 分平台；两处 `window.confirm` 换 `Sheet`（桌面 `popover`、手机 `sheet`），命令语义与 `commandId` 路径不变；桌面布局与 testid 零变化 |
| **处理方式** | 旧子弹全部保留（`:337` / `:339-340` 的要求不因收起而豁免），新增段落只补 compact 下的**边界**与两处交互替换 |

### 2.6 §3.4 命中尺寸与辅助文本字号（新 D-039）

| | |
|---|---|
| **§11.7 行** | **P0-2 字号与命中** — **NEEDS_DESIGN**：「方向对，但 `.back 20px → --touch` 的**字面写法违反** `ui-spec.md:342`「手机上触控 ≥ 44px 只靠**热区**，不靠视觉尺寸」。正确做法是 20px 字形 + 44×44 `::after`…报告 §5-P0-2 自己也写了「可视可以略小，hit slop 必须够」——标题和正文不一致，实施单必须取正文那句。」**P1-8** 另记 ViewSwitch 只差尺寸、且要按 `:342` 用热区 |
| **旧文** | 热区口径只出现在 §2.2「Effort」那一条（pill 40px + 48px 热区、图标 26px 字形 + 44×44 `::after`），**没有全局化**；`.meta` 类文本的字号下限只在 `tokens.css` 的注释里，规格正文没有 |
| **新文** | 新增 §3.4「**命中尺寸与辅助文本字号（D-039，修订 §11.7 「P0-2 / P1-8」）**」：命中一律走热区、视觉字形不变、新增命中尺寸用 `var(--touch)`、明确把「把 `.back` 的 width 改成 44px」列为禁止反例、热区不得互相重叠；`.meta` 类文本桌面与手机统一 `var(--text-aux)`（12px）、用 token 不用字面量、点明 12px 下限出自 type scale 块头注释、只管辅助文本；并写明 compact（布局）与 `coarsePointer`（触屏）不得混用 |
| **处理方式** | 把既有的单点口径升为全局规则并给出反例，正是 §11.7 所说的「取正文那句」；同时把「桌面手机同值」写成明文，堵住「只改手机那一条」的走偏 |

## 3. `decisions.md` 新增条目

按任务要求从 **D-038** 起编号（D-037 已被另一分支的 claude-sdk carrier 占用）。每条含 决策 / 由谁 / 依据 三段，依据指向报告 §11.x 与本文件 §2 的行号。

| ADR | 内容 | 计划表中的对应行 |
|---|---|---|
| **D-038** | 会话列表行 = 状态点 + 标题 + 一句下一步；三维 wire 与 `ins_` 退到展开/tooltip；行内遥控收进溢出菜单 | P0-6 |
| **D-039** | 触控命中只靠热区；`.meta` 统一 `var(--text-aux)`，桌面手机同值；禁止在 `session.module.css` 新增裸像素命中尺寸 | P0-2 / P1-8 |
| **D-040** | `/s/:id` compact 铬预算：space chips 折成单芯片入顶栏；诊断 meta 进「运行详情」disclosure；分段与 Stop 不得进 ⋯ | P0-1 / P0-5 |
| **D-041** | 工具卡默认折叠的豁免集合与折行最小信息量 | P1-10 / P1-20 |
| **D-042** | 手机 composer 单行 + 选项 sheet 的边界：触发器带 mode 词与档名，三态与诚实标注留在外面 | P0-3 |

## 4. 源报告的三处必要修正

报告入库后只允许改这三处（§11.6 的两处措辞 + §11.7 的行号）：

| 位置 | 旧文 | 新文 |
|---|---|---|
| §2.3 | `.meta { font-size: 10.5px }`，低于 tokens「重要状态不低于 12px」（`tokens.css --text-label: 11px` 注释） | `.meta` **桌面 11px / 手机 10.5px**（基础值 `session.module.css:153-159`，10.5px 只在手机媒体查询里），两者都低于 type scale 的 12px 下限（`tokens.css:52-53` 块头注释，**不是** `--text-label` 自己的注释） |
| §5-P0-2 | `.meta` 10.5px → ≥12px；`.back` 20px → `--touch` | `.meta` 桌面 11px / 手机 10.5px → 统一 `var(--text-aux)`；`.back` 20px 字形 → 配 44×44 `::after` 热区（**不是**把字形撑到 44px） |
| §11.7 P0-5 行 | 「第 **233** 行就是 `seq 184 · connectivity=connected · $0.12`」 | 「第二行（修订前是第 **235** 行，本节此前误写作 233）就是 …」 |

已核对：`tokens.css:52-53` 是 `/* Type scale (P1-3): body 14, inputs/emphasis 16, aux 12-13, labels 11.` / `Important status never relies on sub-12 px text. */`，而 `--text-aux` / `--text-label` / `--touch` 分别在 `:57` / `:58` / `:68`——两处措辞的更正都成立。

## 5. 边界与不做项

- **零代码改动**：本分支不含 `web/` 或 `crates/` 的任何改动，因此没有新增 hub e2e spec，也不需要新的单测。
- **不改 `spaces-1.md` 的验收口径**：§1.4 的 chips 例外是**限定作用域**而不是改写既有验收；`spaces-drawer-open` 等 testid 全部保留，`spaces.spec.ts` / `tabs-semantics.spec.ts` / `spaces-hub-live.spec.ts` 不需要改。
- **不抽公共 `Segmented`**：§11.7 的 P1-8 行指出抽公共组件会把 `ApprovalsPage.tsx` 的 a11y 债一起拖进来（M→L）；本任务只把 ViewSwitch 的**尺寸口径**写清楚。
- **列表行的 context 剩余环不做**：与「行去 wire 串」争同一行空间，且 `contextUsage.ts` 目前只在会话页取数（计划 §(C) 第 6 条默认值）。
- **Moshi 的 tunnel / Kill 端口旁栏不做**：D-031 明确禁止隧道工具。

## 6. 验证

| 检查 | 结果 |
|---|---|
| 源报告逐行比对 | 去掉 §11.3 行 423 后与 `/tmp/remuda-agents/briefs/src/workbench-ux-improvement-2026-09.md` **无差异** |
| 个人标识 | 提交内容仅含机器名（`bolt` / `devbox-sg`）与产品名，均为仓库既有文档中的既有写法；个人 workspace 名已按任务要求替换为 `<workspace>` |
| `scripts/ci/no-tunnel-scan.sh` 口径 | 新入库的两份文档对禁用 token 集合（cloudflared / ngrok / frpc / frps / bore / tailscale funnel / `ssh -R` / `ssh -D`）零匹配 |
| 文档内引用一致性 | `ui-spec.md` 新增段落引用的 ADR 编号与 `decisions.md` 新增条目一一对应；§2 表格里的「旧文」逐字取自修订前文件 |
| 全量 web hub e2e | 见本任务 DONE 前的运行记录（docs-only 分支，用于确认没有连带回归） |
