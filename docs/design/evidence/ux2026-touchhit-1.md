# ux2026 · c-touchhit 证据（P0-2 / P1-8）

- 日期：2026-09-19
- 分支：`wt/c-touchhit/b-touchhit-md` · 任务 c-touchhit（coordinator dispatch plan workbench-ux, 2026-09-19, section (B) task 3；派工计划未入库，按名字引用）
- 规格：`docs/design/ui-spec.md` §3.4（命中尺寸只靠热区；`.meta` 下限 `var(--text-aux)`）、D-039；Stop 留顶栏见 D-040
- 改动文件：`web/src/features/session/session.module.css`、`web/src/features/session/ViewSwitch.test.tsx`、`web/tests/e2e/ux-touchhit.hub.spec.ts`（`ViewSwitch.tsx` 未改，radiogroup 行为原样）
- 截图：`ux2026-touchhit-1-390.png`（390×844 hasTouch，fake Node shell-pty 会话，合成数据）

## 1. 做法

视觉字形尺寸一律不动；44px 命中全部由相对定位元素上的绝对定位 `::after` 提供（与既有 `.effortIconBtn::after` 同一机制），且尺寸一律引用 `var(--touch)`，不新增裸像素命中尺寸：

| 元素 | 视觉（改前=改后） | 命中 | 手段 |
|---|---|---|---|
| `.back` 返回 | 20px 宽字形 | 44×44 `::after`（仅 ≤767px） | 热区 |
| `.stopBtn` 停止 | 32×32 方块（手机） | 44×44 `::after`（仅 ≤767px），并新增 `flex: none` 防止 32px 被 headRow flex 收缩 | 热区 + 防收缩。**390px 下该控件整体在视口外、不可点，见 §2** |
| `.viewSeg` 终端/结构段 | 25px（桌面）/ 30px（手机） | 44×44 `::after`（仅 ≤767px） | 热区 |
| `.effortIconBtn` 两个 26px 图标钮 | 26×26 字形 | 既有 `::after`，字面量 44px 改为 `var(--touch)` | token 化 |

`.meta` 字号：

| 视口 | 改前 | 改后 |
|---|---|---|
| 桌面基础规则 | 11px 字面量 | `var(--text-aux)` = 12px |
| 手机（max-width:767px） | 10.5px 覆盖 | 覆盖删除，继承 12px。改前手机（10.5px）比桌面（11px）**更小**，方向与「重要状态不低于 12px」相反；改后两视口同为 12px |

改动后 `session.module.css` 中 `var(--touch)` 出现 4 次（改动前 0 次）。

## 2. 实测：几何命中，不是 getComputedStyle

`web/tests/e2e/ux-touchhit.hub.spec.ts` 在 Chromium 实测。第一轮只读 `::after` 的计算宽高，无法发现「热区被删了居中 transform / 被兄弟盖住 / 控件在视口外」；第二轮改为几何契约：

1. 控件可视盒必须完整落在 `page.viewportSize()` 之内；
2. 以可视盒中心 ±21px 的四个角（44×44 热区角，0.5px 内缩）做 `document.elementFromPoint`，必须命回控件自身或其后代（每个控件打唯一 `data-touchhit-owner` 标记，用 `closest()` 归属）。

第三轮加固（评审 r3）：rect 读取与四角 probe 合并进**同一个 `page.evaluate`** 同步完成 —— AnchoredPopover 会在 requestAnimationFrame（含 pill→list 翻转）时重新定位，跨往返采样可能读到移动中的面板；四角坐标用**原始值**并先断言在视口内，越界即失败，绝不钳回可视盒（否则零热区也能通过）。

### 390×844 hasTouch —— 在屏控件四角全部命中自己

| 控件 | 可视盒（含位置） | 四角 elementFromPoint |
|---|---|---|
| `.back` 返回 | 20×20 @ (14,120) | 4/4 → back |
| `.viewSeg` 终端 | 46×30 @ (186,115) | 4/4 → seg-tty |
| `.viewSeg` 结构 | 46×30 @ (233,115) | 4/4 → seg-structured |
| `effort-reset`（effort pill 26px 图标） | 26×26 @ (277,681) | 4/4 → effort-reset |
| `effort-list-back`（档位列表 26px 图标） | 26×26 @ (25,672) | 4/4 → effort-list-back |
| `.meta` 计算字号 | 12px（改前 10.5px） | — |

角点取可视盒中心 ±21px（0.5px 内缩、视口缘处钳到屏内），即「44×44 热区的角、可视字形之外」的位置；每个控件打唯一 `data-touchhit-owner`，probe 用 `closest()` 归属。这套几何断言删 transform、盖兄弟、推出屏都会红，而 `getComputedStyle(::after)` 读数不会。

### Stop：热区合格但 390px 下整体在视口外 —— 显式 blocker 移交 c-sessionchrome

390px 实测 `.headRow`（flex、不换行、不横向滚动）把 Stop 停在视口右缘之外：Stop 的可视盒实测 **32×32 @ x=502**（视口宽 390，`flex: none` 生效，不再被压成 14px；整行约 540px 宽，溢出约 144px）（见 `ux2026-touchhit-1-390.png` 右缘，Compact/文件/原始事件/Stop 被依次推出屏）。因此 Stop 此刻**无人能点到**——热区再大也不构成可达目标。

处理（评审第 2 条取方案 A）：

- Stop 的四角几何断言保留在独立用例中，并用 `test.fail(offscreen, …)` 标为 **expected-to-fail**，注释点名 c-sessionchrome 与 D-040（Stop 永不进 ⋯，只能由减铬把顶栏拉回屏内）；c-sessionchrome 落地后该 xfail 会自行翻转报警。
- 本单不通过让 headRow 换行/滚动来救场：那会改变 c-sessionchrome 即将重排的视觉密度，且文件所有权上 `SessionPage.tsx` 不属本单。
- 同时为修评审第 3 条做了本文件内可做的部分：`.stopBtn { flex: none }`，让 32px 可视方块不再被压成 14px（压窄会使居中的 44px 热区向左多吞 9px、越过 10px 间距落到邻居上）。

### Stop 与邻居不互吞（D-039 的非重叠前置）

≤767px 宽度下整行能入屏时（spec 取 767×900，仍是手机断点、热区规则生效），在 Stop 与左邻「原始事件」之间逐点 hit-test：

- 实测：邻居可视右缘 x=711；Stop 44px 热区左缘 x=715；Stop 可视方块左缘 x=721 → 热区入间距 6px、与邻居留 **4px** 净空。
- 邻居右缘列（x=710）→ 命中 neighbour；热区左缘外 1px（x=714）→ 空隙，不是 stop；热区左缘内（x=715.5）→ stop；可视方块内（x=722）→ stop。
- 390px 无触控收窄同页复做：结构化段四角仍全部命中自己（热区是宽度断点，不是 pointer:coarse）。

### 1440×900

`.viewSeg` visual 高 25px，`::after` 计算 width/height 为 auto（桌面不挂热区），桌面视觉零变化；`.meta` 计算字号 12px（改前 11px）；`.back` 桌面本就不渲染。

## 3. 不回归

- `ViewSwitch` 的 `role=radiogroup` / `aria-checked` / roving tabindex / 方向键行为：`ViewSwitch.test.tsx` 原 3 个用例不变；新增的第 4 个类名用例**不是**命中断言（jsdom 无布局、不应用样式表），只守住「on 态类是追加的、`.viewSeg` 永远在两个 radio 上」这个热区挂载前提，注释中已标明。
- 未抽公共 `Segmented`；`ApprovalsPage` 手写分段本轮不动。
- 桌面视觉无变化：热区规则只在 `@media (max-width: 767px)` 内；`.meta` 桌面由 11px → 12px 是本任务验收要求。
