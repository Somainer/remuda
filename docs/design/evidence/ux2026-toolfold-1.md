# c-toolfold 证据 · 工具卡手机折叠（D-041 / P1-10 / P1-20）

- 日期：2026-09-19（review 修订：2026-09-19，按协调人 8 条意见）
- 规格：`docs/design/ui-spec.md` §2.2「手机默认折叠（D-041）」；ADR `docs/design/decisions.md` D-041
- 代码：`web/src/features/session/ToolCard.tsx`、`toolRegistry.ts`（`shouldFoldToolCard` 判定表）、`toolPresenters.ts`（`foldedKeyArgument`）、`transcript.module.css`
- 测试：`toolRegistry.test.tsx`（判定表全豁免 + 折叠态 Bash/Edit 渲染 + 展开跨 remount 存活）、`toolPresenters.test.ts`（关键参数）、`web/tests/e2e/ux-toolfold.hub.spec.ts`（5 例，390px hub 实机）、`session-virtual.spec.ts` 与 `session-structured.spec.ts`（双 project 分叉）、`ux-live-view.hub.spec.ts`（Final 落地后先展开再断言）

## 1. 折叠判定（family → settled → compact，顺序不可换）

判定集中在纯函数 `shouldFoldToolCard({ family, settled, compact, failed, interaction, requested })`，
`ToolCard` 在 **family 判定之后**才允许走折叠提前返回（D-041 实现约束）：

| 条件 | 结果 | 覆盖测试 |
|---|---|---|
| compact + settled + 普通族（Bash/Edit/Read/Write/Task/MCP/Generic） | 自动折叠 | `shouldFoldToolCard · D-041 decision table` |
| 桌面（非 compact）默认 | **不折叠**，桌面默认态不变 | 同上 + hub e2e「desktop default」例 |
| running / 未 settled（无 final result） | 自动折叠下永不折叠；但一旦 result 在实机落地立即折叠（无闩、无 remount 依赖） | 单测「folds the moment its final result settles」+ hub e2e「seen running folds live」例 |
| `family === "Workflow"` | 自动折叠下永不折叠（含运行中与结束后），`WorkflowTimelineCard` 始终挂载、1Hz 走针 | 单测 + hub e2e「Workflow … head elapsed keeps ticking」例 |
| error（`outcome` failed/denied） | 两种路径都永不折叠 | 单测「keeps a failed (error) card open」+ 判定表 |
| `interaction.*`（`AskUserQuestion` / `ask_user_question` 兜底卡） | 自动折叠下永不折叠 | 单测「keeps an interaction fallback card open」+ 判定表 |
| 桌面显式「全部折叠」(`requested`) | **main 原语义：折叠一切非 failed 卡，running 与 Workflow 也折叠**；D-041 豁免只约束自动折叠 | 判定表「collapse-all … including running and Workflow」+ hub e2e「collapse-all folds every non-failed card」例 |

函数形状（协调人裁决 2）：`if (requested) return !failed; if (!compact) return false;
if (failed || interaction || family === "Workflow") return false; return settled;`。

settled 的定义：`settle && result?.stage === "final"`（partial result 与 gap-backfill 都不算）。
failed 定义：settled 且 `result.outcome ∈ {failed, denied}`；`Transcript` 的 `ToolRow` 原本就把
failed 卡排除在 collapse-all 之外，两侧口径一致。

**协调人裁决 1（无 seenLive 闩）**：实机（手机）会话里一张正在跑的卡必须在 settle 的瞬间折叠——
实时手机会话正是 D-041 要解决的滚动问题；代码里不允许存在任何「看着跑过就保持展开」豁免。
`ux-live-view.hub.spec.ts` step 7 改为先断言折成一行、点展开后再断言 `exit 0`（该 spec 最后视口
就是 390px）。给 owner 的一句话：实现按裁决 live-settle 立即折叠；不持异议，仅记录被改动的
`ux-live-view` 旧预期（final 瞬间仍展开）——若未来要恢复，需要 owner 修订 D-041 而非加代码豁免。

分支顺序问题（报告 §11 P1-10/P1-20 点名的静默回归）：旧代码 `if (folded) return …` 在
`family === "Workflow"` 分支之前，一律默认折叠会让 `WorkflowTimelineCard` 永不挂载。
现在折叠判定发生在 family/grok 计算之后，且 Workflow 族在自动路径豁免——hub e2e 用「卡头
elapsed 在运行中递增」（不是「卡可见」）作为硬指标。显式 collapse-all 仍能折叠 Workflow 行
（main 原行为），展开后 `WorkflowTimelineCard` 原样重挂。

## 2. 折行内容 = family + 关键参数

`foldedKeyArgument(nativeName, call, result)` 返回 `{ text, title }`：

- **Bash / grok run_terminal_command**：`text` = 命令首个物理行（`split("\n")[0]`），
  `title` = 命令全文；输入没有 `command` 时退化为**单行** JSON 预览
  （`JSON.stringify`，不是多行 `jsonPreview` 的首行 `{`），保证折行永远不是裸 `Bash`、
  也不会渲染成 `Bash {`。
- **Edit / Write / Read / NotebookEdit**：`file_path`（NotebookEdit 用 `notebook_path`），
  输入缺路径时取 `result.changes[0].path`。
- **grok**：`read_file` → `target_file`，`list_dir` → `target_directory`，
  `write` / `search_replace` → `file_path`。
- Task / MCP / Generic 规范表未规定关键参数，返回 null，只显示标题。

行内按宽度 CSS 截断（`.foldTitle/.foldArg` 均 ellipsis；`.fold { overflow: hidden }`），
全文在 `title`。族名即标题（Claude 的 Bash/Edit/Read/Write）时不重复渲染族词；
grok 卡保留共享 `NativeLabel`（标题≠原生名时才出现），折叠态同样不重复。

无障碍：折叠开关带 `aria-expanded="false"` 与 `aria-label="展开 <标题> <关键参数>"`
（如 `展开 Bash echo workflow-running`），N 张折行对辅助技术可区分；渲染单测断言该 label。
触控：开关视觉仍是 22px 按钮，compact 下用居中 `::after` 热区撑到 `var(--touch)`（44px），
照抄 `.effortIconBtn::after` 先例（ui-spec §3.4）；hub e2e 用 `getComputedStyle(el,"::after")`
断言 44×44，并断言按钮盒不越出 390px、整页无横向溢出（>40 字符的长 MCP 名用例）。

## 3. hub e2e（fake node，390×844，5 例）

脚本场景：复用 `workflow card live`；新增 `toolfold settle`（call 帧 → 3s → final result 帧，
同一会话无 reload）与 `toolfold settle mcp`（已 settled 的超长
`mcp__remuda-very-long-integration-server__search_files_everywhere` 卡）两个
`crates/remuda-hub/examples/hub_e2e.rs` 分支，纯增量、不影响其他场景。

截图（`REMUDA_EVIDENCE=1` 时落在本目录，否则在 `test-results/evidence/`）：

- `ux2026-toolfold-fold-390.png` — 390px 下已结束 Bash 卡折成一行，行内带命令片段。
- `ux2026-toolfold-expanded-390.png` — 点「展开」后挂载与桌面一致的 BashCard。
- `ux2026-toolfold-wf-clock-a.png` / `ux2026-toolfold-wf-clock-b.png` — Workflow 两帧，
  卡头 elapsed 读数递增。
- `ux2026-toolfold-live-settled-390.png` — running→settled 实时折叠（主路径）。
- `ux2026-toolfold-longname-390.png` — 超长 MCP 名折行，无横向溢出。

五例断言：

1. **折叠 + 展开**（390px 无触控，reloaded 历史）：Bash 卡 `data-folded=1`、
   `tool-fold-arg` 文本/`title` 正确、aria-label 正确、折叠时卡体不挂载；展开后完整卡可见。
2. **豁免 + 走针**（`hasTouch: true` 的 390px）：Workflow 卡不折叠且卡头 elapsed 5s 内递增；
   同回合已结束 Bash 仍折叠，Workflow 不进汇总行。
3. **桌面默认不变 + collapse-all 是 main 语义**（1280px）：默认 `data-folded=0`；
   「全部折叠」后 Bash 与 live Workflow **都**折，展开 Workflow 行后 live 卡原样重挂。
4. **live settle 实时折叠**（390px 无触控，无 reload）：先见 running 完整卡，
   final 落地 10s 内自动变成折行；展开仍见 `exit 0`。
5. **超长 MCP 名**（390px 触控）：折行 ellipsis、aria-label 带全名、`::after` 44×44、
   按钮不越界、`scrollWidth ≤ innerWidth+1`。

error 豁免在 hub e2e 的说明：共享 fake harness 没有 failed/denied 普通工具场景，
`interaction.*` 帧在 web 侧投影为 QuestionForm 而不是 ToolCard——二者无法在 hub
实机挂出可折叠卡，故由渲染单测覆盖（见 §1 表）。

`session-virtual.spec.ts`（mock，默认 playwright 配置的 chromium + mobile-webkit iPhone 13
两个 project 都跑）：「Compact/Full persists and collapse-all folds tools」里第一张已 settled
Bash 卡在 mobile-webkit 下默认就是 `data-folded=1`；断言改为按 project 分叉
（chromium=0 / mobile-webkit=1），collapse-all 后两 project 仍都是 1。

`session-structured.spec.ts`（同样双 project）：working-session 用例在 mobile-webkit 下先
`scrollIntoView({block:"center"})` + 直接 `.click()` 打开 compact-fold（浮动 composer dock
在 390px 会盖住汇总行；这是 host 上既有现象，pristine main 同样被 intercept），再按
`aria-label="展开 Edit src/exec.cc"` 找到折叠 Edit 行展开（展开会卸载折行本身，故展开后
直接断言卡体 `已写入` 出现，不用被卸载的祖先 locator）；chromium 走原点击路径。全文件
grep 过默认配置（非 `.hub.`、未被 `testIgnore`）下所有 spec，仅此一例含「只在展开普通卡内
才出现」的文本（`已写入`）；其他文件要么不碰 transcript，要么只断言 testid/标题。

## 3b. round-3 修订（协调人 8 条）

1. **去掉 seenLive 闩**：live settle 立即折叠（见 §1 裁决段）；`ux-live-view.hub.spec.ts`
   step 7 先断言 `data-folded=1` 再展开断言 `exit 0`；新增 hub e2e live-settle 主路径。
2. **collapse-all 恢复 main 语义**：`requested → !failed`，running/Workflow 都折；
   判定表新增对应行。
3. **横向溢出**：`.foldTitle` 改 `flex 0 1 auto; min-width:0; ellipsis`，`.fold` 加
   `overflow:hidden`；长 MCP 名 hub 例断言无横向溢出、按钮不越 390px。
4. **44px 热区防假绿**：compact media 块里给 `.foldHead { min-height: var(--touch) }`
   （否则 8+22+8=38px 行高把 `::after` 上下各裁 3px，被裁像素不 hit-test）；hub e2e 在
   `getComputedStyle(::after)` 之外，用 `document.elementFromPoint` 探热区中心 ±21px 的
   上下边缘都落到 toggle（或其子节点）。
5. **展开状态上提到 Transcript**：`expandedTools`（按 node id 的 Set）与
   `dismissedWorkflows` 并列，经 `renderNode`/`ToolRow` 传 `expanded`/`onExpand` 进
   ToolCard；路由切换与「全部折叠」清空。修掉手机上展开的卡滚出虚拟窗口（OVERSCAN 8）后
   remount 被静默重折、行高回弹的问题。ToolCard 保留 local 态兜底给不传状态的嵌套卡/单测；
   单测「expanded card stays expanded across unmount/remount」覆盖。c-journalpage 的
   ResizeObserver 代码未动。
6. **标题 title**：`.foldTitle` 带 `title={完整标题}`——MCP/Generic 折行没有关键参数时
   截断标题仍能给出全文（ui-spec §2.2）。
7. **文档限定语**：`ui-spec.md` §2.2 豁免表与 `decisions.md` D-041 决策段各加一句——
   豁免只约束**自动 compact 默认折叠**；显式「全部折叠」保持既有桌面行为（非 failed 皆折，
   含 Workflow/running）。未改这两个文件的其他内容。
8. **脚本 gap 3s**：fake harness 的 live-settle 场景 call→result 间隔从 1.5s 提到 3s，
   避免 loaded gate 上两帧同批导致 running 断言不可恢复；折叠断言保留。
9. hub_e2e.rs 的 `toolfold settle`/`toolfold settle mcp` 是纯增量分支，gated 会编 Rust；
   `cargo fmt --all --check` 与 `cargo clippy -p remuda-hub --all-targets -- -D warnings` 通过。

## 4. 回归面

- 未改 `session.module.css`（热点文件属其他 worker）；折叠行样式与热区落在本任务独占的
  `transcript.module.css`。
- 未改 `playwright.hub.config.ts`；新 spec 按约定命名 `ux-toolfold.hub.spec.ts` 自动入选。
- 桌面 testid 与结构零变化（非折叠仍渲染 `<div data-testid="tool-card" data-folded="0">`）。
- c-grok-web 的共享 `NativeLabel` 守卫、按原生名分发的 grok presenter、grok 渲染卡测试
  全部保留并通过。
- `web/tests/e2e/__screenshots__/1b-session-390.png` 是 write-only golden（`design-align-1b`
  只写不比、仅在 chromium 截图）：D-041 后 390px 下 TaskManager spill 的工具卡已折成一行，
  该 golden 内容已过期；本任务不刷新它（owner 属 design-align，且 mobile-webkit 下它本就不跑）。
- round-4：D-041 折行现在与 in-transcript search 打通——当前 search hit 落在某张已折叠普通卡时
  （含 compact-fold 子卡，`hitChildId === child.id`），该卡自动展开（`expandedTools.has(id) ||
  searchCurrent`），与 CompactFold/SubagentFolds 既有的 hit 自动展开一致；单测覆盖
  「折叠卡成为当前 search hit 后展开、清空后回到折叠默认」。此前这导致
  `session-virtual.spec.ts` 的 batch-E 搜索用例在 mobile-webkit 下找不到藏在折行里的 result
  文本，属于真实回归（已修）。
- 全量 hub e2e 跑过一次（协调人修订前的版本）：唯一失败为 `ux-code` 的剪贴板权限环境 flake，
  隔离重跑 2/2 通过；修订后再次跑全量（见交付说明）。
