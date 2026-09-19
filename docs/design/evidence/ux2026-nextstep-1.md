# ux2026-nextstep-1 — 列表行去 wire 串（P0-6，D-038）

- 日期：2026-09-19
- 任务：`briefs/plans/workbench-ux.md` §B-2 `c-nextstep`（仓库内仅此一份计划文件；没有 docs/design/plans 目录，引用以仓库实际路径为准）
- 规格依据：`docs/design/ui-spec.md` D-038（§2.1「行内容 = 状态点 + 标题 + 一句下一步」）；线框第二句 `Workflow wf_ab12 · phase compile`
- 截图（合成 fake-node fixture，无真实路径/主机名/用户名）：`ux2026-nextstep-1-1440.png`、`ux2026-nextstep-1-390.png`

## 1. 旧文 → 新文（逐项对照验收）

| # | 旧实现 | 新实现 |
|---|---|---|
| 1 | 行首是 `.meta`：`lifecycle·activity·connectivity \| host/worktree \| driver`，下方多行 model/effort 与 tty snippet；exited 行一律写 `可恢复` | 首行为 headline（StateDot、标题、kind 芯片、DONE badge），第二行为 `nextStep()` 投影句；wire 字段全部进 `session-wire` details；exited 仅在 `capabilities.resume.state === "supported"` 时写 `可恢复`，无 transcript 时不承诺 |
| 2 | 工作行（最常见）恒为常量 `运行中…`，旧 `summaryOf()` 被删 | 工作行优先显示 `liveSummary(events)` 投影出的 journal live 短语（running workflow run + 当前 running phase，否则最近一条单行 assistant 消息），无已知短语才落 `运行中…`；短语从行自身的轮询投影（见 §2） |
| 3 | 行内常驻 send/enter/esc/ctrl-c/stop | 全部收进每行一个溢出 Sheet（桌面 popover / 手机 sheet），testid 与 store 命令路径全部保留；面板带真实 id，触发器 aria-controls 指向它；待处理 interaction 的行保留一个 `去处理`（`/approvals?focus=`） |
| 4 | wire 字段直接铺在默认视图 | 默认视口不可见：closed details 用 sr-only 隐藏且对子内容 `display:none`，对行高贡献 ≤1px；触发器是行侧 `session-wire-toggle`（不是可见 summary），title 携带完整 wire 文本（含相对时间）；缺 workspace 不渲染 `undefined`/多余斜杠 |
| 5 | 筛选 Sheet 只有 testId | filter 触发器 aria-controls 解析到面板真实 id（`session-filter-panel`），与 board-actions 一致 |

**明确不做**（plan §C 默认项）：context 剩余环、toolcard 折叠、tunnel/Kill、`deploy/` 均未触碰；`session.module.css`、`playwright.hub.config.ts`、`ui-spec.md` 未改。

## 2. 工作行短语的读取方式

短语**不**来自 2s 的 `refresh()`（它被 close/cancel/create/resume 等多条路径重入，不能新增轮询）。它挂在 `SessionList` 既有的行数据轮询 effect 上（与 TTY screen 轮询同一个 effect）：

- 只处理当前渲染的行 id（`hydrateRowSummaries(rowIds)`）；
- 记录每个实例上次投影时的 `durableSeq`（`Instance.durableSeq` 随实例列表已在手里，**不**为拿 seq 多发一次读），seq 未变直接跳过；
- 每个实例**一次** `eventsRead({afterSeq: max(0, durable-64), limit:64})` 有界尾窗读；
- 单一 in-flight promise，轮询 tick 重叠时 coalesce，不并发；
- 投影结果对账：finished run / 尾窗无 assistant → 返回空串并删除该实例的 key；行不再渲染（切 Space/卸载）→ 删除 key 与 seq 缓存，行立即回到常量句，不留陈旧「发明」状态。

follow 的 session 页仍从 history replay 与 live batch 更新短语（finished run 同样清除）。mock 客户端继续直接提供脚本 summary。

## 3. 测试与证据

- 单测（`pnpm --dir web test`，1219 全绿）：
  - `nextStep.test.ts`：blocked（approval 描述 / question 题数 / plan-review 标题 / elicitation 表单标题）/ working（短语已知、空白回落、DONE 屏幕标记不升级成功）/ idle / exited（exit 码已知/未知 × resume supported/unsupported/unknown 两分支）/ unknown（断连优先级最高，即使 stale pending）/ resolved interaction 被忽略；
  - `liveSummary.test.ts`：running run + running phase、queued phase 不顶替、跨 workflow 的 phase 忽略、run 完成回落到 assistant 尾句、无信号 undefined、nativeRunId 超长不截断；
  - `SessionList.test.tsx`：headline 在前、wire 默认不可见且无 wire 文本、toggle 的 title 带 wire 三元组与相对时间、两个 Sheet（filter / board-actions）的 aria-controls 都解析到面板 id、溢出 sheet 开关（含 Esc 回焦）、send 后关闭 / 键连按保持、无 workspace 无 `undefined`、工作行短语投影与回落、手机单行；
- hub e2e `web/tests/e2e/ux-nextstep.hub.spec.ts`（fake node，3 个实例）：blocked approval 行（句子=批准摘要，go handle 指向真实 interaction）；working workflow 行断言 live 短语 `Workflow wf-native-demo · phase Review`（queued `Verify` 不顶替 running `Review`）；terminal 行溢出 sheet 发 ESC → fake harness 回 `QUICKFIND_ESC_RECEIVED` 且列表 snippet 出现；wire 展开后可见 `connected` 与短码。
- mock 规格 `agent-board.spec.ts` 用 `/sessions?scope=all`（默认固定 Space 不再渲染这些跨 Space 的 worktree 行——未改 main 上即如此的既有现象，见上方 scope note；行形状本身与 scope 无关），展开 disclosure 后断言完整分支 `wt/x-acpwire/canary`，遥控全部走迁移后的 Sheet。

## 4. fake-node 夹具（独立提交，全 hub 规格共用）

- `tty.write` 用与真实 Node 相同的 `remuda_driver::logical_keys_to_bytes` 解码 `keys:["esc"]`（行 sheet 发的就是逻辑键）；
- `tty.screen` 是**唯一**一个 arm，体内是 per-instance 的 if/else-if 链：未来 c-mobilenew 的 seeded-exited 分支折进同一 arm；只有 create 初始输入带 `screen-read` 哨兵、登记进 `screen_enabled` 集合的终端才回放 cooked 屏幕行；**默认回复与改动前完全一致**（`{ok:true}`、无 lines），保证其它规格的列表行行为不变；
- create 提示词为 `workflow card demo-running row-phrase` 时不起 approval、直接 journal running workflow 场景并保持 native status working（`row-phrase` 是显式哨兵，不做裸子串匹配）。

## 5. 390 px 几何（bounding-box，非 nowrap scrollHeight）

基线在 **b0dd8389**（`merge: wt/c-grokpartials`，本次 rebase 的直接基底）上用同一 fake-node fixture、同一 390×844 视口实测；探针脚本（Playwright，锁 `/tmp/remuda-local-e2e.lock-b`，独立 target 目录）：

```ts
// 基线：在 b0dd8389 临时 worktree 起 hub_e2e，建 blocked-question + terminal 两实例后
const card = page.getByTestId("board-card").filter({ has: page.locator(`a[href="/s/${id}"]`) });
const h = await card.evaluate((el) => Math.round(el.getBoundingClientRect().height * 10) / 10);
// BASELINE390 blocked=292.5 terminal=260.5
```

改动后同一探针：blocked 行 **152.6 px**，terminal 行 **120.6 px**（closed disclosure sr-only 绝对定位、对子内容 display:none，行高不含 wire）。满套件并发负载下 flex 回流会让绝对高度多约 54px，所以**套件里的几何门**不钉死改后绝对高度，而是用 bounding-box 断言三个不变量（wrap 回归任一会破）：① blocked/terminal `cardH ≤ 基线值`；② headline `height ≤ computed line-height + 1`；③ sentence 同。探针与记录的新值 152.6/120.6 保留在本文件作为设计目标。

## 6. 复跑命令（本 worker 指定端口 + 锁）

```bash
export HUB_E2E_LISTEN=127.0.0.1:58910 HUB_E2E_WEB_PORT=58919 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:58911
export PW_CHANNEL=chromium PW_TEST_CONNECT_WS_ENDPOINT=ws://127.0.0.1:3177/
# 单元
pnpm --dir web install --frozen-lockfile
pnpm --dir web exec vitest run          # 131 files / 1219 tests
pnpm --dir web exec tsc -b              # typecheck
pnpm --dir web exec oxlint <changed src>
# mock + hub e2e（串行，flock /tmp/remuda-local-e2e.lock-b）
cd web
./node_modules/.bin/playwright test --project=chromium tests/e2e/agent-board.spec.ts
./node_modules/.bin/playwright test -c playwright.hub.config.ts   # 130→138 tests: 120 passed / 0 failed / 16 skipped (post-rebase run on d58be746)
# Rust
cargo fmt --all --check
cargo build -p remuda-hub --examples --locked
```

## 7. 兼容性

- 保留 testid：`board-card / session-row / session-lifecycle / session-model / session-effort / board-snippet / board-done / board-select / board-prompt / board-send / board-key-* / board-stop / session.next-step(新) / session-wire(新) / session-wire-toggle(新) / board-go-handle(新) / board-actions-panel / board-more`；
- `SessionPage.tsx` 另有独立 `session-lifecycle`（会话顶栏 meta），未触碰；
- 未改 `playwright.hub.config.ts`、`session.module.css`、`ui-spec.md`、`deploy/`。
