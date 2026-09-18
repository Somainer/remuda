# ux2026 · c-touchhit 证据（P0-2 / P1-8）

- 日期：2026-09-19
- 任务：c-touchhit，计划 `briefs/plans/workbench-ux.md` §B-3
- 规格：`docs/design/ui-spec.md` §3.4（命中尺寸只靠热区；`.meta` 下限 `var(--text-aux)`）、D-039
- 改动文件：`web/src/features/session/session.module.css`、`web/src/features/session/ViewSwitch.tsx`（无视觉变更；ViewSwitch 本身未改）、`web/src/features/session/ViewSwitch.test.tsx`、`web/tests/e2e/ux-touchhit.hub.spec.ts`
- 截图：`ux2026-touchhit-1-390.png`（390×844 hasTouch，fake Node，合成数据）

## 1. 做法

视觉字形尺寸一律不动；44px 命中全部由相对定位元素上的绝对定位 `::after` 提供（与既有 `.effortIconBtn::after` 同一机制），且尺寸一律引用 `var(--touch)`，不再新增裸像素命中尺寸：

| 元素 | 视觉（改前=改后） | 改前命中 | 改后命中 | 手段 |
|---|---|---|---|---|
| `.back` 返回 | 20px 宽字形 | 20px | 视觉 20px + 44×44 `::after`（仅 ≤767px） | 热区 |
| `.stopBtn` 停止 | 32×32 方块（手机） | 32×32 | 视觉 32×32 + 44×44 `::after`（仅 ≤767px） | 热区 |
| `.viewSeg` 终端/结构段 | 25px（桌面）/ 30px（手机） | 25 / 30px | 视觉 25/30px + 44×44 `::after`（仅 ≤767px） | 热区 |
| `.effortIconBtn` 两个图标钮 | 26×26 字形 | `::after` 44px 字面量 | 同机制，改为 `var(--touch)` | token 化 |

`.meta` 字号：

| 视口 | 改前 | 改后 |
|---|---|---|
| 桌面基础规则 | 11px 字面量 | `var(--text-aux)` = 12px |
| 手机（max-width:767px） | 10.5px 覆盖 | 覆盖删除，继承 12px；不再出现「手机字号 > 桌面」 |

改动后 `session.module.css` 中 `var(--touch)` 出现 4 次（改动前 0 次）。

## 2. boundingBox / getComputedStyle 实测

`web/tests/e2e/ux-touchhit.hub.spec.ts` 在 Chromium 中实测（visual = `boundingBox()`，reach = max(visual, `::after` 计算宽高)，fake Node shell-pty 会话，合成数据）：

### 390×844 hasTouch

| 元素 | visual | reach（含 `::after`） |
|---|---|---|
| `.back` 返回 | 20×20 | **44×44** |
| `.stopBtn` 停止 | 14×32（宽度被拥挤 headRow flex 收缩，既有行为） | **44×44** |
| `.viewSeg`（终端 / 结构，各一） | 46×30 | **46×44**（高度由 `::after` 补到 44） |
| `.effortIconBtn` effort-reset | 26×26 | **44×44** |
| `.effortIconBtn` effort-list-back | 26×26 | **44×44** |
| `.meta` 计算字号 | — | **12px**（改前 10.5px） |

### 1440×900

| 元素 | 结果 |
|---|---|
| `.viewSeg` | visual 42×25，`::after` width/height 均为 auto（不挂热区），桌面视觉零变化 |
| `.meta` 计算字号 | **12px**（改前 11px） |
| `.back` | 桌面本就不渲染 |

### 390×844 无触控

`.viewSeg` reach 仍为 44×44：热区挂在 `max-width: 767px` 宽度断点上，与 `pointer: coarse` 解耦，窄桌面窗口不会因无触控丢掉命中面。

同套断言还复核了 radiogroup 语义（`role=radiogroup`、每段 `role=radio`）仍在。

注：390px 实测 stop 的 visual 为 14×32 —— 32px 是 CSS 宽度，但拥挤 headRow 的 flex-shrink 让它收成字形宽；这是改动前就有的行为（c-sessionchrome 的减铬单会把 文件/原始事件 移进 ⋯ 腾出空间），本单不改视觉布局。44×44 热区相对可视方块居中，正是为这种「可视被挤小、命中不能跟着小」的情形兜底。相邻可点区域的最终间距随 c-sessionchrome 落地后复查。

## 3. 不回归

- `ViewSwitch` 的 `role=radiogroup` / `aria-checked` / roving tabindex / 方向键行为：`ViewSwitch.test.tsx` 原 3 个用例不变，新增 1 个「两态都保留 `.viewSeg` 类（热区挂载点）」用例。
- 未抽公共 `Segmented`；`ApprovalsPage` 手写分段本轮不动。
- 桌面视觉无变化：热区规则只在 `@media (max-width: 767px)` 内；`.meta` 桌面由 11px → 12px 是本任务验收要求（§B-3 验收 1）。
