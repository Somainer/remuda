# Workflow 时间线卡片（workbench 批次 W）落地证据

- 日期：2026-09-14/15
- 设计基线：产品负责人批准的静态稿（结构与 8 条产品决定，不复制其 token 与 mock 数据名）
- 信号实测：[`workflow-progress-signals-1.md`](./workflow-progress-signals-1.md)（本机 claude 2.1.221 一手捕获）

## 交付物

| 文件 | 作用 |
|---|---|
| `web/src/features/session/workflow/workflowProgress.ts` | 纯投影：观察 → 卡片视图模型（状态、分栏、折叠、窄屏取舍、降级、已终止措辞） |
| `web/src/features/session/workflow/workflowProgress.test.ts` | 20 个单测（状态映射、折叠规则、8 列阈值、20 agent 折叠、窄屏、×2、降级） |
| `web/src/features/session/workflow/WorkflowTimelineCard.tsx` | 卡片：表头即摘要、终态自动收成一行、「当前 <phase>: <agent>」、点阵进度轨、`还有 n 个` |
| `web/src/features/session/workflow/WorkflowTimelineCard.test.tsx` | 7 个组件测试（运行展开、终态自动折叠、已终止、折叠、失败行不折叠、×2、降级） |
| `web/src/features/session/workflow/workflow.module.css` | 新 CSS module（遵守 workbench 文件归属：不写 session.module.css / ui.module.css / tokens.css） |
| `web/src/features/session/assemble.ts` | **E 拥有文件的最小 hook**：`workflow.run` 带 `toolCallId` 时把工作流挂到对应 Workflow tool 节点；仅加 `workflow?` 字段与一个挂载函数 |
| `web/src/features/session/ToolCard.tsx` | 用 `WorkflowTimelineCard` 替换原来的空 `WorkflowCard`（presenter 契约保留为兜底） |
| `crates/remuda-protocol/src/observation.rs` | additive：`WorkflowRunPayload.{name,description,totals,live,note}`、`WorkflowMemberPayload.{latestTool,tokens,calls,durationMs,startedAt,endedAt}` |
| `crates/remuda-hub/examples/hub_e2e.rs` | 合成 2-phase workflow 夹具（demo / fold 20-agent / fail / legacy 四种），通过假 Node 的 journal.append 驱动 |
| `web/tests/e2e/ux-workflow-card.spec.ts` | hub-live e2e：实时→自动折叠、失败行不折叠、20 agent 折叠、390px 单行、降级行、Escape 不影响卡片 |

## 8 条产品决定对账

1. **挂载点**：卡片直接长在 Workflow tool 行上（assemble 以 `toolCallId` 挂载），无外框阴影。
2. **折叠**：运行中展开并实时刷新；终态自动收成表头一行（name · chip · rail · done/total · 时长 · tokens · calls），点击展开。表头就是摘要，只有一套结构。
3. **agent 行**：状态字形+sr-only 文案、label、模型短名、最近工具、时长、tokens、attempt>1 显 `×n`；无 prompt/结果预览。
4. **≤560px**：CSS 媒体查询隐藏 model/tool 两个 `.soft` meta，表头数字换到第二行；390px e2e 逐行断言高度 ≤28px。
5. **大规模**：`>8` 自动 grid（`repeat(auto-fill, minmax(280px,1fr))`），保持 index 顺序；分栏后可见行 `>12` 时尾部连续安静行折进「… 还有 n 个」；running/failed 永不折叠（纯函数 `foldAgents` 单测覆盖 14 pinned 等极端情形）。
6. **降级**：run 带 `note` 或无任何 phase/member 时退化为普通行 + 一句说明，没有展开箭头、不留空壳（`wf-card--flat`）。
7. **killed 文案**：`cancelled` → 「已终止」（单测 + 组件测试锁定）。
8. **不做**：单 agent 暂停/停止、远程 workflow、prompt/结果预览、脚本正文、「只看活跃」筛选。

附加契约：`data-status`（running/completed/failed/killed/paused）在卡上、`data-state`（queued/running/done/failed/killed）在行上；`aria-expanded`/`aria-controls` 驱动所有折叠；Escape 只在卡片自己的 toggle 获焦时折叠，终端（xterm）里的 Escape/Tab 字节不被拦截（`lib/keyboardScope` 护栏不变）；等宽 tabular-nums meta；72px 点阵进度轨 + accent 填充。

## 实机信号→实现的取舍

实测（claude 2.1.221）确认 hook socket 能拿到 SubagentStart/Stop 与子 agent PostToolUse，但 label/phase/model/token 需要有界读取 run 目录文件才能完整还原。本期 Node 侧落 **additive 协议字段**（totals/live/成员明细），实时数据链以观察事件驱动：

- 已有的 `workflow.run/phase/member` 观察携带卡片所需全部字段；
- hub-live e2e 由假 Node 合成完整 2-phase 事件序列（先 running 快照、900ms 后终态），验证实时展开→自动折叠；
- 真实 2.1.221 会话当前没有生产者发这些富字段 → 卡片按决定 6 降级（普通行 + 说明），不臆造数据；
- 真 Node 的文件折叠器（hook 触发 + journal/agent jsonl 有界尾读 + 终态收口）是下一阶段工作，证据文档第 4 节给出的最小协议面就是本期落地的这批 additive 字段，无需再改 schema。

## 检查记录

- `cargo fmt --all --check` / `cargo clippy --workspace --all-targets` / touched crates `cargo test`（post-merge origin/main）
- `pnpm --dir web lint` / `typecheck` / `vitest`（27 个 workflow 测试，全套 672 测试）
- `cargo run -p remuda-protocol --example gen_types` 后 `web/src/types/generated.ts` 与 schema 同步
- hub-live e2e：`ux-workflow-card.hub.spec.ts` 6/6（flock + 规定端口/WS 环境）；spec 按批次约定命名为 `.hub.spec.ts`，config 以数组形式追加 `/\.hub\.spec\.ts$/`
- `./scripts/ci/secret-scan.sh` 通过

## 截图

合成夹具（无真实模型、无参考稿 mock 名），1440 / 768 / 390 三档：

- 运行中（2 phase、实时行、进度轨）
- 已完成自动折叠的一行摘要
- 20-agent 阶段分栏 + 「还有 8 个」
- 失败（红色 chip、失败行不折叠）
