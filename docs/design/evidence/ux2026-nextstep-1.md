# ux2026-nextstep-1 — 列表行去 wire 串（P0-6）

- 日期：2026-09-19（评审修订同日）
- 任务：`briefs/plans/workbench-ux.md` §B-2 `c-nextstep`
- 规格依据：`docs/design/ui-spec.md` D-038（§2.1，2026-09-19 增补的「行内容 = 状态点 + 标题 + 一句下一步」）；线框行第二句 `Workflow wf_ab12 · phase compile`
- 截图：`ux2026-nextstep-1-1440.png`、`ux2026-nextstep-1-390.png`（合成 fake-node fixture，无真实路径/主机名）

## 1. 旧文 → 新文（逐项对照验收）

| # | 旧实现 | 新实现 |
|---|---|---|
| 1 | 行内无「下一步」派生函数（全库 grep `nextStep` 零命中） | 新纯函数 `web/src/features/session/nextStep.ts`：`nextStep(instance, pending, screen, summary) → { text, tone }`，只投影 `projectStatus` / `Interaction.request.kind\|title\|description\|fields` / `exit` / `capabilities.resume` / screen `done` / journal live phrase，不新造状态机、不猜成功 |
| 2 | 行 DOM 首行是 `.meta`：`lifecycle · activity · connectivity \| host / worktree · driver · model · effort \| ins_xxxxxxxx`（`SessionList.tsx` 旧 :583-665）；工作行曾用 `hubStore.summaryOf()` 的 mock 短语 | 首行 `headline`：`StateDot` + 标题（截断）+ kind 芯片 + DONE badge；第二行仅 `nextStep().text`（`data-testid="session-next-step"`，ellipsis 单行，`title` 给全文）。工作行句子来自新投影 `web/src/features/session/liveSummary.ts`：最新 running `workflow.run` 的 nativeRunId + 该 run 当前 **running** phase 的 label（queued phase 不顶替，别的 workflow 的 phase 不算）→ `Workflow <id> · phase <label>`；无 run 则取最新 assistant 行（压成一行）；都没有才落常量 `运行中…`。run completed/failed 后回落到 assistant 尾句。store 在列表 2s 轮询时以 bounded journal tail（`eventsRead` limit 64，仅读尾部）hydrate `state.summaries`（不为列表开 follow 流）；已 follow 的会话则随历史/实时事件即时刷新；mock 客户端继续走自己的脚本 summary |
| 3 | wire 三维与短码默认视口可见 | 全部移进 `<details data-testid="session-wire">`（默认收起）；`session-lifecycle`（含 host/worktree/driver/model/effort）、`ins_` 短码、exit 码、tty snippet 只在展开或 `summary` 的 `title` 里可读 |
| 4 | 行内常驻 `board-prompt` 输入框 + `board-send` / `board-key-enter\|esc\|ctrl-c` / `board-stop`（旧 :706-779），每行 220px 遥控台 | 全部收进每行一个溢出 `Sheet`（复用 `components/Sheet.tsx`；`variant` 随 `useWorkbenchViewport().mobile`：桌面 popover / 手机 sheet）。testid 与命令路径（`hubStore.send` / `sendKeys` / `close`）全部保留；新增触发器 `board-more`、面板 `board-actions-panel`（带真实 `id`，触发器 `aria-controls` 指向它）、关闭 `board-actions-close` |
| 5 | 待处理组没有行上主操作 | 有待处理 interaction 的行保留一枚「去处理」`board-go-handle`，直达 `/approvals?focus=<interactionId>`（既有 push 深链形状，`pushLink.ts:18`） |
| 6 | 390px 下 badge 折行、`.actions` 全宽占第二屏 | 390px headline 不折字（`flex-wrap: nowrap` + 标题 ellipsis），next-step 单行 ellipsis；relative time 在手机端隐出行内但进了 `summary` 的 `title`；行高 ≤ 改前基线（见 §3，按 bounding box 实测） |
| 7 | `unknown` 行文案 `connectivity=<值> · 不推断…` 仍是 wire 值 | `状态待确认 · 不推断成功或结束`；断连优先级最高（即使 activity=working 或有 stale pending 也投影 unknown） |
| 8 | 退出行一律写 `可恢复`，但无 transcript 的会话在会话页其实禁用 Resume（`SessionPage.tsx` canResume） | 退出行只在 `capabilities.resume.state === "supported"` 时写 `可恢复`；否则仅 `会话已退出 · exit N`，不空头承诺恢复（D-026） |
| 9 | workspace 不在快照时模板拼出字面量 `undefined`（`host/undefined`） | 缺 workspace 时该行与 title 都不渲染 `/` 洞 |

**明确不做**（plan §C 默认项）：不添加 context 剩余环（争同一行空间）；不删任何遥控能力（只收进 sheet）；不碰 tunnel/Kill。

## 2. 测试与证据

- 单测 `nextStep.test.ts`：blocked（approval/question/plan-review/elicitation/无 interaction 五分支）/ working（短语已知、空白短语回落、DONE 屏幕标记不升级成功且可叠加短语）/ idle / starting（文案不含 wire 值）/ exited（exit 码已知/未知 × resume supported/unsupported/unknown）/ unknown 六态；断连优先级、resolved interaction 忽略。
- 单测 `liveSummary.test.ts`：无信号 undefined；最新 assistant 行；多行压一行；running run+phase 的精确文案；无 phase 只报 run；run 完成后回落 assistant；queued phase 不顶替 running；跨 workflow phase 忽略。
- 单测 `SessionList.test.tsx` 增补：headline 在前、wire 收起时 `session-lifecycle` 不可见且行文本无 `waiting-interaction`、`summary` title 同时带 wire 三元组/路径/相对时间；approval 摘要投影 + go-handle href；无 pending 不画 go-handle；工作行短语投影与常量回落；workspace 缺失无 `undefined`；溢出 sheet 开关（`aria-expanded`/`aria-controls`↔面板 id、Esc 关闭、焦点回触发器）、sheet 内保留全部 testid、send 后自动关 sheet；mobile variant。
- hub e2e `ux-nextstep.hub.spec.ts`（fake node 三个实例）：
  1. 用 `blocked-question` 哨兵起在脚本 approval `echo e2e` 上且 native status=blocked → 落「待处理」组，句子为批准摘要、默认无 `waiting-interaction`，go-handle 指向真实 `interactionId`；
  2. `workflow card demo running row` 哨兵（create 即 journal running workflow + running phase `Review`/queued `Verify`，不起 approval，保持 working）→ 工作行句子断言为 `Workflow wf-native-demo · phase Review`，证明句子投影自 live journal 而非常量；
  3. terminal：`<details>` 默认收起、展开后 `connected` 与短码可读；溢出 sheet 六个 testid 全可见；sheet 内发 `board-key-esc`，fake-harness 屏幕回 `QUICKFIND_ESC_RECEIVED`，列表轮询 snippet 出现该标记——证明键到达进程；
  4. 390×844 bounding-box 几何（见 §3）。
- fixture 配套（`crates/remuda-hub/examples/hub_e2e.rs`，仅测试夹具；该文件作为独立提交，tty.screen 对**所有** hub spec 返回真实屏幕行）：
  - `tty.write`：无 `dataBase64` 时取 `keys:[…]` 并以与真实 Node 相同的 `remuda_driver::logical_keys_to_bytes` 翻译；
  - 新增 `tty.screen`：把 cooked 屏幕缓冲回放为 `{ lines: string[] }`（此前落默认 ok，所有 spec 的屏幕轮询都为空），未建 TTY 的实例返回空行数组；
  - create 分支支持 `workflow card … row` 哨兵：不起 pending approval、直接 journal running workflow 场景。
- `agent-board.spec.ts`（mock 后端既有规格）随遥控迁移更新：在全局 scope 下（其行跨多个 mock Space）先开 `board-more` 面板再操作；保留完整分支断言 `wt/x-acpwire/canary`（展开 disclosure 后断言）。

## 3. 390px 几何（bounding box，评审项 3）

旧的「scrollHeight vs clientHeight」断言对 CSS 已禁止折行的元素不可能失败，已替换为几何测量。

- **改前基线**：在本提交的父提交（12058ccb）上，以同一 fake-node fixture、同一 390×844 视口、一个临时 Playwright 探针用 `getBoundingClientRect()` 测得：待处理行 **292.5px**、terminal 行 **260.5px**（旧行含常驻 send 表单与多行 wire meta）。
- **新行**：e2e 在同一 fixture 同一视口断言待处理行 `cardH ≤ 292.5`、terminal 行 `cardH ≤ 260.5`；且 headline 与句子各自的 bounding-box 高度 `≤ 自身 computed line-height + 1px`（单行几何，不靠 nowrap 自证）。标题折行或句子折行都会让该断言失败。
- headline：`flex-wrap: nowrap`；标题 `overflow:hidden; text-overflow:ellipsis; white-space:nowrap`；kind/DONE/快捷键 badge `flex:none`。next-step：`white-space:nowrap` + ellipsis。

## 4. 兼容性

- 保留 testid：`board-card` / `session-row` / `session-lifecycle` / `session-model` / `session-effort` / `board-snippet` / `board-done` / `board-select` / `board-prompt` / `board-send` / `board-key-*` / `board-stop`；新增 `session-next-step` / `session-wire` / `board-more` / `board-go-handle` / `board-actions-panel`（同 id）/ `board-actions-close`。
- 未改 `playwright.hub.config.ts`、`session.module.css`、`ui-spec.md`。
- `SessionPage.tsx` 另有独立的 `session-lifecycle`（会话页 meta），未触碰。
