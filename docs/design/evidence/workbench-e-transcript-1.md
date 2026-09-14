# Workbench batch E — 结构化对话：稳定身份、正文搜索、阅读位置、工具失败可见

- 日期：2026-09-15
- 分支：`wt/ux-e/transcript-search`
- 基线：`origin/main` = `eefcbba`（含批次 A、C1/C2 与 r-p3-stream 的 MessageDisplay 流式、transcript regroup、工具 presenter、注入分类）
- 对应需求：[workbench-ux-exploration.md](../workbench-ux-exploration.md) §5 P1-2；[workbench-ux-plan.md](../workbench-ux-plan.md) §1 批次矩阵 E、§4 风险 4（live region × 流式正文）
- 证据性质：全部来自本仓库合成 fixture 与 fake node（`crates/remuda-hub/examples/hub_e2e.rs`），无真实模型调用、无参考站点资料。

## 1. 交付范围

| 文件 | 变更 |
| --- | --- |
| `web/src/features/session/assemble.ts` | 节点身份改为最早修订的 `nodeId`（确定性、与到达顺序无关）；节点按最早 journal seq 排序；失败/被拒工具不进 compact fold；新增 `isToolFailure` |
| `web/src/features/session/transcriptSearch.ts`（新） | 纯函数正文搜索：`findMatches` / `resolveSelection` / `searchableText` |
| `web/src/features/session/readingPosition.ts`（新） | `runtime.reading.v1.<instanceId>` 阅读位置与 follow 持久化 |
| `web/src/features/session/Transcript.tsx` | 搜索栏 UI、滚动定位、compact fold 命中展开、位置恢复、失败行内标记；根节点保持 `aria-live="off"` |
| `web/src/features/session/transcript.module.css`（新） | 本批样式；未编辑 `styles/ui.module.css`（批次 F 独占） |
| `web/src/features/session/TaskTrack.tsx` | 长 prompt 两行截断 + 展开/收起；失败结果标注 |
| `web/src/fixtures/session/batchE.ts`（新）、`web/src/lib/mock.ts` | 2,000 事件合成会话 `ins_mock_batch_e` |
| `web/tests/e2e/session-virtual.spec.ts` | 双模式：mock 几何用例 + Hub fake-node 不变量 |
| 单测 | `assemble.test.ts`、`transcriptSearch.test.ts`、`readingPosition.test.ts`、`Transcript.test.tsx`、`TaskTrack.test.tsx`、`hubJournal.test.ts`（身份断言随契约更新） |

## 2. 稳定内容身份（需求：修订/追加到来时保持当前命中身份）

### 2.1 问题

原 `assembleMessages` 用 `group[0].payload.messageId` 作为节点 id，`group[0]` 是事件**数组中的第一个**。两处不稳定：

1. 节点 id 随数组顺序变化。store 的事件数组在 gap backfill 时会把更早 seq 的事件 **append 到数组尾部**（`lib/store.ts` 的 follow 合并按 `eventId` 去重后 `concat`），重新 follow 又按 seq 顺序重建。一个先看到 close/rename、后补到 open 的消息，`group[0]` 在两次组装间会从 rename 事件变成 open 事件，React key 随之改变——搜索命中和保存的阅读锚点就会跳到别的节点。
2. `messageId` 本身允许在 regroup/close 时被生产者改名（protocol §5.2：`nodeId` 是 mutation-chain 身份；既有测试 “uses node identity as a fallback … messageId: renamed” 正是该场景）。

### 2.2 实现

- 每个 message 分组先按 `(revision, status, operation, seq)` 排序（既有 `compareMessages`），节点 id 取**最早修订的 `nodeId`**——它是纯事件集合的函数，与到达顺序和 messageId 改名都无关。
- 组装器记录每个节点的**最早 journal seq**（`anchors`），最终列表按该 seq 稳定排序：迟到的 backfill 事件回到它在 journal 中的真实位置，而不是落到列表底部。同 seq 保持原相对顺序。
- 工具/思考/workflow 节点本就以各自的协议 id（`toolCallId`、`thoughtId`、`workflowId`）为键，流式 `append` 原地更新同一对象，key 不变。

### 2.3 单测证据

`assemble.test.ts`（新增 “stable content identity (batch E)” 组）：

- 同一节点 open→append→close 三次组装 id 恒定；
- suffix 先到、`messageId` 在 close 时改名（`original`→`renamed`），前缀后补，id 始终为 `same-node` 且文本正确合并；
- 后补历史（事件顺序 `[tail…, early…]`）后节点顺序为 journal seq 顺序，与纯 seq 顺序组装结果完全一致；
- 失败/被拒工具不进入 compact：成功工具进 fold，`failed`/`denied` 留在顶层原位置。

`Transcript.test.tsx` 保留 r-p3-stream 的两个流式回归（同一气泡跨修订不重挂载）。

## 3. 正文搜索（需求：命中虚拟窗口外的已加载节点；不写 journal/原生会话）

### 3.1 模型

搜索作用于**组装后的已加载节点**（`transcriptSearch.ts`），不是 DOM——虚拟化只挂载约 20 行，浏览器查找无法触达窗口外。

- `findMatches(nodes, query)`：大小写不敏感（可选敏感）的纯字符串匹配，覆盖 message/thought 文本、工具输入与结果块、workflow 标题/阶段、opaque 摘要。compact fold 的子节点可被搜到，命中携带**子节点 id**（用于展开 fold 并定位真实行）。`usage` 节点不可搜。
- 命中身份是 `(nodeId, ordinal)`。`resolveSelection` 在列表重建（流式追加、regroup）时优先保持同一 `(nodeId, ordinal)`；ordinal 消失则夹到该节点剩余命中；整个节点消失才回退到列表位置。单测 “keeps the same hit when nodes shift above it” 证明：上方插入新命中后，选中的仍是原节点的同一条命中，而不是索引 N 现在指向的别的节点。
- 搜索没有任何 I/O。UI 层（`Transcript.tsx`）只在内存中计算与滚动。

### 3.2 UI

工具栏新增「搜索正文」按钮（`aria-expanded`），快捷键 `Cmd/Ctrl+F`（不拦截 xterm 与可编辑目标）和 `/`；`Escape` 关闭并把焦点还给触发按钮。栏内有输入框、`当前/总数` 计数（自身 `aria-live="off"`）、上一项/下一项/退出。Enter 下一项、Shift+Enter 上一项。

命中定位只用几何：`rowOffsets(...)` 按已测行高（未测用收敛后的平均行高估计）算出目标行顶部并 `scrollTop`，窗口挂载该行后再精修；命中在 compact fold 内时自动展开该 fold。当前命中行 `data-search-current="1"`，其余命中行 `data-search-hit="1"`。

### 3.3 e2e 证据（mock，2,000 事件）

`ins_mock_batch_e` 是严格 2,000 条事件的合成会话（`buildBatchEObservations`，单测可直接构造）：1..1990 为交替对话，事件 7 与 1001 各带唯一标记，尾部含一个长 prompt 的 Task、一个 `failed` Bash、一个 `denied` Bash 和一个仍为 `streaming` 的助手节点。

`session-virtual.spec.ts`（mock 子集）实测：

- 初始挂载行数 `< 80`；搜事件 7 的标记 `ESEARCH-NEEDLE-FAR-7741`，计数 `1/1`，Enter 后滚动到列表顶部附近（`scrollTop < 1000`）且 `data-anchor="obj_batch_e_n_7"`；再搜事件 1001 的标记，`scrollTop > 30000` 且锚点为 `obj_batch_e_n_1001`；窗口行数仍 `< 80`。
- 搜 `reply` 命中数 `> 900`，计数自动选中第一项，下一项/上一项滚动位置变化且计数在 `1/…`/`2/…` 间切换；Escape 后输入框消失、按钮 `aria-expanded="false"`。
- 搜索全程（输入、next、prev、关闭）记录非 GET/OPTIONS 请求：**0 个**。

### 3.4 e2e 证据（Hub + fake node）

hub 配置新增 project metadata `appMode: "hub"`，spec 据此分支。fake-node 子集中：

- 创建会话、回答开业审批后发两轮带唯一标记 `zulu-4173` 的消息；监听所有指向 `/v1/instances/.../commands|input|interrupt` 与 `/v1/interactions/.../answer` 的非 GET 请求；搜索（输入、Enter、prev、next、关闭）后等待 500ms，原生动作写请求为 **0**。搜索不改 journal、不驱动原生会话得到真实验证。

## 4. 阅读位置与 follow 状态（需求：离开再进入后恢复，按实例、无敏感新数据）

- 存储沿用 `runtime.*` 命名空间，键 `runtime.reading.v1.<instanceId>`，值仅含：锚点节点 id（不透明 journal id）、行内像素偏移、滚动比例、实测平均行高、follow 布尔。无正文、无 journal 内容、无 native session id。
- 滚动停止 250ms 防抖写入；离开会话（组件卸载、ref 已脱离 DOM）时用 ref 中的最新偏移和节点/行高引用做一次同步 flush。
- 再进入且 `follow=false`：先用上次的平均行高估算位置，锚点行挂载后用 **DOM 相对修正**（锚点行与容器顶部的实际差值 − 保存的行内偏移）收敛。纯估算会因两次访问已测行集合不同（follow 首次开在底部、恢复时开在顶部）而漂移；DOM 修正与其它行是否已测无关。follow=true 时不恢复旧偏移，继续钉在最新。
- e2e（mock）：滚到 12000px → `/sessions` → 回来，恢复位置与 12000 差值在容差内且「跳到最新」可见；钉到底部再离开回来，仍在底部（`< 48px`）。e2e（fake node）：10 轮填充后滚到顶 → 离开回来 `scrollTop < 160`；钉底再离开回来仍在底部。

## 5. 工具失败可见（需求：不能因折叠失去可发现性）

- `assemble.ts`：`isToolFailure`（`outcome` 为 `failed`/`denied`）的工具被排除在 compact fold 之外，并保留其原始位置。
- `Transcript.tsx`：失败工具行外包 `data-testid="tool-failure"`（`data-tool-outcome`）加行内标记 `工具失败 · 失败/已拒绝`；传给 `ToolCard` 的 `defaultFolded` 对失败强制为 false，「全部折叠」不会把它收起。
- `TaskTrack`：失败/被拒子任务用危险色标注，`data-task-outcome` 暴露结果。
- e2e（mock）：尾部分页两个 `tool-failure-tag`，outcome 集合为 `{denied, failed}`；点「全部折叠」后仍是 2 个、卡片 `data-folded="0"`，两个调用 id 的锚点行可见。

## 6. live region 边界（plan §4 风险 4）

- Transcript 根节点保持 `aria-live="off"`，搜索计数 span 也显式 `aria-live="off"`；唯一礼貌播报区仍是 Shell 中 C1 的 `role="status"`/`aria-live="polite"` 短文本区。
- e2e（fake node）：发送 `stream …` 前缀消息触发 fake node 的 open/append 两帧链，在 live region 上挂 MutationObserver 计数，回复落地并等 800ms 去抖后，**变更次数 ≤ 4**，且区域文本不含任何 `bounded-region-delta` 正文。C1 既有断言（区域无 `echo:`/正文、短状态仍可达）继续通过。

## 7. TaskTrack 长 prompt

超过 140 字的 prompt 两行截断并以 `…` 结尾，提供 `展开/收起` 按钮（`aria-expanded`）；短 prompt 无按钮。单测 `TaskTrack.test.tsx` 覆盖截断、展开/收起往返、running/succeeded/failed/denied 标注。

## 8. 验证记录

命令在 worktree `/tmp/remuda-agents/wt/r-ux-e` 内执行；端口使用派工指定的 `HUB_E2E_LISTEN=127.0.0.1:58080`、`HUB_E2E_WEB_PORT=58089`，Playwright 经共享 `ws://127.0.0.1:3177/`，所有 Hub e2e 运行包在 `flock /tmp/remuda-agents/e2e.lock` 内。

- `pnpm --dir web lint`：通过（仅既有 fast-refresh/set-state warning，无新增 error）。
- `pnpm --dir web exec tsc -b`：通过。
- `pnpm --dir web test`（vitest）：81 个文件、629 个用例全部通过；其中本批新增单测：assemble 稳定身份/失败 5 个、transcriptSearch 12 个、readingPosition 4 个、Transcript 组件 5 个、TaskTrack 4 个。
- mock e2e（`playwright.config.ts`，session-virtual）：12 passed / 3 skipped（hub 用例在 mock project 下按 metadata 跳过；mobile-webkit 的桌面用例跳过）。
- Hub e2e（`test:e2e:hub`，flock 串行）：结果见下方“最终运行结果”小节。
- `./scripts/ci/secret-scan.sh`：见最终提交前记录。

### 最终运行结果

（Hub e2e 完成后填写实际通过/失败与已知 flake 比对结论。）

## 9. 未做与边界

- 搜索范围明确为**已加载**节点；更早历史仍走既有 journal backfill，未新增远程正文索引（exploration §8 待定项保持第一期决策）。
- 未改 `store.ts`、`Shell.tsx`、`Composer.tsx`、`SessionPage.tsx`、`SessionList.tsx`、`styles/ui.module.css`；阅读位置的 instanceId 由 Transcript 经路由参数取得（无路由的单测挂载退化为不持久化）。
- 未为 UI 回退改写任何旧 journal 或原生 session；搜索与位置恢复全部是只读投影与本地偏好。
