# ux2026-chrome-1 — 手机减铬 + 桌面诊断 meta 进「运行详情」(c-sessionchrome, P0-1 / P0-5, D-040)

日期：2026-09-19。范围：`web/src/pages/SessionPage.tsx`、`web/src/app/Shell.tsx`、
`web/src/features/session/RunDetails.tsx`（新）、`runDetails.module.css`（新）、
`web/src/features/session/session.module.css`（自 c-touchhit 移交，保留其热区/`.meta`
规则）、`web/src/features/spaces/SpacesMobile.tsx` + `spaces.module.css`（新增 `chip`
变体，strip 行为零改动）。

依据：`docs/design/ui-spec.md` §1.3（Stop 与分段永不进 ⋯）、§1.4（compact
`/s/:id*` chips 折成单芯片例外，D-040）、§2.2（主行 + 运行详情 disclosure，
D-040）、`workbench-ux-improvement-2026-09.md` §11 P0-1/P0-5。

## 改了什么

### 390px 手机

- Shell 不再在 compact `/s/:id*` 渲染整条 `SpacesMobile` chips 行；`SpacesMobile`
  新增 `variant="chip"`：一枚「☰ 当前 space」芯片出现在会话 header 主行，点击打开
  **同一个**抽屉（`spaces-drawer-open` / `spaces-drawer` 与 `spaces-chips` testid
  全部保留，`spaces-chips` 落在芯片的包裹元素上）。`/sessions` 等索引路由的整条
  chips 不变。`.chip` 基类的 `overflow:hidden` 会裁掉芯片自己的 44px `::after`，
  所以 header 变体把省略裁剪移到内层 label，按钮本身 `overflow:visible`。
- 主行固定为：`← 返回` · 当前 space 芯片 · 标题（省略号，`title` 给全文；完整标题
  同时在 tab 条上）· 状态点（≤640px 只画点，词仍在 `textContent` 与 `title` 里，
  既有 `toContainText` 断言不断）· `terminal|structured` 分段 · Stop · `⋯`。
- Compact / 文件 / 原始事件 进 `⋯` 底 sheet（复用 `components/Sheet`，44px 菜单项，
  focus trap / Esc / 焦点回还由 `useFocusTrap` 提供）。**分段与 Stop 永不进 ⋯**，
  e2e 断言 sheet 内二者 count=0。该 sheet 是单列菜单，后续要追加 tabs sheet 或
  transcript 动作入口时只需加菜单项，无需重构。
- 诊断 meta 不再常驻第二行，改为 `▸ 运行详情 N 项运行信息` 一行触发器；手机展开后
  host/cost/provenance 也在里面（主行只留回答「能不能继续」的状态点/分段/Stop）。
- 已退出会话在拥挤宽度下两个 resume 按钮移到独立的第二 header 行（`resume-row`），
  避免长按钮在 390px 主行折成三行互相重叠。
- 命中尺寸全部沿用 c-touchhit 的「视觉字形不变 + 44px `::after` 热区」口径
  （ui-spec §3.4, D-039）：返回、space 芯片、两个分段、Stop、⋯、运行详情触发器都
  是 44×44 热区；相邻热区靠实测间距拆开。分段自身保持 ≥44px 视觉宽度，使相邻段的
  热区不互相覆盖；返回左缘 12px padding 让其**未夹紧**的 44px 角仍在视口内。

### 1440px 桌面

- 主行保留：space/标题 · 状态点 · **主机芯片** · **cost** · 分段 · Compact ·
  文件 · 原始事件 · Stop；诊断字段（driver/delegation/provider/providerSourceHint/
  lifecycle/seq/connectivity/native/transcript 绑定）进 `<details data-testid="run-details">`，
  默认收起。`session-meta` testid 保留在展开内容上（老断言只需展开一步）。
- 展开态按设备持久化：`localStorage["runtime.run-details.open"]`，与 §1.4 space
  prefs 同口径，不跨设备同步。

## 测量（390×844，generic-pty 会话，boundingBox + elementFromPoint，touch 与无 touch 各一遍）

| 指标 | 改前 | 改后 |
|---|---|---|
| Stop 位置 x / 是否在屏内 | x=430（屏宽 390，**出屏 40px**） | x=305，32px 方块完全在屏内 |
| headRow scrollWidth | 454（溢出 92px） | 376（≤390，无横向滚动） |
| `session-body` 顶部 y | 226.17 | 143.39（**正文多 82.8px**，计划要求 ≥56px） |
| 整条 space chips 行（56px） | 常驻 | `/s/:id*` 消失；折叠为 81px 单芯片入主行 |
| 原始 44px 热区角（未夹紧） | — | 返回/芯片/分段/Stop/⋯/详情全部在视口内，四角自命中 |

桌面 1440：主行 host 芯片 + cost 常驻；`run-details` 默认 `open=false`，展开后
`seq N` / `connected` / driver 可读，reload 后仍为展开。

截图（合成 fake-node 夹具，无真实路径/用户名/主机名）：

- `ux2026-chrome-1-390.png` — 390 收起态：单芯片入主行、Stop/分段/⋯ 在屏。
- `ux2026-chrome-1-1440.png` — 1440 收起态：host + cost 在主行，运行详情收起。
- `ux2026-chrome-1-1440-open.png` — 1440 展开态：driver/seq/connectivity/native 可读。

## 测试

- 新增 vitest `src/pages/SessionPage.chrome.test.tsx`（4 例）：桌面主行元素 +
  details 默认收起；手机 chips 折叠 + ⋯ sheet 只含三项且不含分段/Stop + 文件可经
  sheet 导航；767px 无 touch 的 compact 宽窗仍内联三按钮；展开态跨 remount 持久化。
- 新增 hub e2e `ux-chrome.hub.spec.ts`（2 例）：
  1. 390px 在 **hasTouch 与无 touch** 两种上下文断言：索引路由整条 chips 仍在；
     `/s/:id*` per-space chips 缺席而 header 单芯片在且开同一个抽屉；⋯ 内三项可达、
     原始事件经 sheet 导航；无横向溢出；6 个主行控件 + 详情触发器的 44px 四角全部
     自命中（boundingBox + elementFromPoint）；正文顶部 ≤ 226.17−56。
  2. 1440px：host/cost/分段/Stop/三按钮在主行（无 ⋯）；details 默认收起、
     `session-meta` 隐藏；展开后 driver/seq/connectivity 可读；reload 后保持展开。
- c-touchhit `ux-touchhit.hub.spec.ts` 的 Stop xfail 翻为通过断言（连同其 round-3
  的 32px 守卫一起保留为普通断言；仅改该用例）。
- `session-structured.spec.ts`（3 处）、`session-virtual.spec.ts`（1 处）的
  `session-meta` 可见性断言前加一步展开 `run-details-summary`；断言内容不变。
- `ux-files.hub.spec.ts` 390 手机用例：文件入口改经 header `⋯` sheet（toggle
  testid/命令路径不变）；并在驱动滚动前先等 `transcript-scroller` 挂载——改动前该
  用例在加载态上读 `scrollTop`（元素尚不存在 → 0），基线能过是因为旧 header 下
  remount 的 scroller 高度被压成 2px、pin 到底仍是 0；header 收回 83px 后 scroller
  拿到真实高度、pin 正确到底，用例的早读取而暴露为 110≠0（在 30c4b580/bfa1f039
  上 3/3 通过、在本分支改前 3/3 失败，加挂载等待后本分支 3/3 通过）。
- 既有 `spaces.spec.ts` / `tabs-semantics.spec.ts` / `spaces-hub-live.spec.ts`
  未改：`spaces-chips` / `spaces-drawer-open` testid 在折叠芯片上保留，抽屉行为
  逐字节复用 `SpacesMobile` 原实现。

`pnpm --dir web test` 全绿（1176 例）；tsc/lint 干净。
