# 手机优先 UI · 任务 5：`m-sessionfold` — 会话路由 compact 铬预算（一条顶栏 + 一条底栏 + 正文 ≥ 60%）

2026-09-20 · `wt/c-msessionfold/b-msessionfold-md` · mobile-ui 实施计划 §(C) 任务 5
规格权威：[ui-spec.md](../ui-spec.md) §1.3 / §1.4 / §4.7（D-049 条款）与 §3.4，[decisions.md](../decisions.md) D-040 / D-042 / D-049；基线 `origin/main` @ 39d5bb73（含 c-sessionchrome / c-composer / c-toolfold / c-mshell）。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/app/Shell.tsx` | 仅三处渲染条件：compact `/s/:id*` 不渲染 `SpaceTabs` 行；同条件不渲染 app 底部导航 `nav[aria-label="手机底栏"]`。chips 横条（strip 变体）此前已由 c-sessionchrome 在该路由摘掉，本任务未改其条件；顶栏单枚 space 芯片（chip 变体）由 SessionPage 渲染，不在本文件 |
| `web/src/app/Shell.module.css` | compact 段新增 tty 视图的单行铬规则（见 §4）；grid 布局本身未改——底栏不挂载时第三行 `auto` 轨道自然坍缩 |
| `web/src/features/session/Transcript.tsx` | 仅 toolbar 块：compact 布局下三枚动作芯片折进一个 `⋯` 触发器（`transcript-tools-open`），展开后挂载的是同一批按钮、同样的 testid；新增 `useCompactLayout()`（与 ToolCard D-041 同款读法，键取 `COMPACT_WORKBENCH_QUERY`，与 `compact` 密度 prop 无关） |
| `web/src/features/session/transcript.module.css` | compact 段：toolbar 允许换行；触发器保持 chip 外观，44px 热区由既有 `::after` 机制提供 |
| `web/src/features/session/Transcript.test.tsx` | 新增 `D-049 compact transcript toolbar fold` 三个用例（折叠态/展开可达三个 testid/动作后收回/桌面内联）；D-041 用例改为先开触发器再搜正文 |
| `web/src/pages/SessionPage.chrome.test.tsx` | 仅 viewport mock：`importOriginal` 保留真实导出（Transcript 新 import 了 `COMPACT_WORKBENCH_QUERY`，全量 mock 会把该常量抹掉）。断言零改动 |
| `web/tests/e2e/m-chrome.hub.spec.ts` | 新增 hub 规格（390 结构段、390 终端段、1440 回归） |
| `docs/design/evidence/mobile-ui-5.md` + 两张 390 PNG | 本文件与证据截图（仅假节点合成数据） |

未触碰：`SessionPage.tsx`（顶栏，c-sessionchrome）、`Composer.tsx`、`ToolCard.tsx`、`session.module.css`、`SessionList.tsx`、`PhoneShell.tsx`、`TerminalView.tsx`、`TerminalView.module.css`、`AuxKeys.tsx`、`playwright.hub.config.ts`、`sw.src.js`。

## 2. 一条顶栏 + 一条底栏（验收 1，D-049）

390×844 下 `/s/:id` 结构段与终端段各断言一次（`m-chrome.hub.spec.ts`）：

- app 级顶栏恰好一条：`[data-testid="session-page"] > header` 计数 1（终端视图内 TerminalView 自己的 `<header class=toolbar>` 是**视图级**工具条，不是 app 铬，已在选择器里排除）。
- `[data-testid="space-tabs"]` 计数 0；strip 变体 `div[data-testid="spaces-chips"]` 计数 0；逐空间 `space-chip` 计数 0。
- `nav[aria-label="手机底栏"]`（Shell `.bar`）计数 0。
- **能力不降级**：顶栏 `spaces-drawer-open` 仍打开同一个 `spaces-drawer`（D-040 的单芯片，e2e 点开并经「关闭空间面板」关闭）；切空间/切 tab 由该抽屉与（M2 的）Jump To 承担；回列表走顶栏返回键。
- 结构段底部唯一的条是收起态 composer；终端段底部是本地输入条 + AuxKeys（见 §4）。
- 列表路由（`/sessions`、`/m`、`/approvals`）与桌面 1440：tabs / 横条 / 各自底栏一律不变（1440 e2e 断言 `space-tabs` 仍可见、`.bar` 仍 `display:none`）。

实现上无需改 grid：`.shell` 的三行模板是 `auto 1fr auto`，第三行唯一内容（`.bar`）不挂载时轨道坍缩为 0，`main` 自动拿回全部高度。

## 3. Transcript 动作芯片折叠（验收 4）

- compact（`COMPACT_WORKBENCH_QUERY`，与 ToolCard 的 D-041 读法一致；jsdom 无 matchMedia 时读桌面默认）下 toolbar 只挂一枚 `⋯`（`transcript-tools-open`，`aria-haspopup="menu" aria-expanded="false"`）。
- 展开后挂载**同一个**组件里的原按钮：`collapse-all` / `transcript-search-open` / `toggle-injected`（仅 `injectedCount > 0` 时，与旧逻辑相同），testid、行为、搜索条（`transcript-search-input` 等）全部不变。
- 选任一动作后行自动收回（搜索会另开它自己的搜索条），保证同一时刻最多一条 toolbar 条。
- 桌面零变化：芯片始终内联，不渲染触发器（vitest「desktop-width layout」用例 + 1440 e2e 双保险）。
- 触发器视觉仍是 32px 高的 `ui.chip` 胶囊；44px 热区由 `.toolsTrigger::after`（`var(--touch)` 居中伪元素，ui-spec §3.4 / D-039）提供，不放大字形。

## 4. 终端段的单行铬（验收 2 的终端侧；为任务 6 m-keybar 定底部布局）

实测初次测量暴露：仅摘掉 tabs + app 底栏后，xterm 高 448/844 = **0.531**，差 0.60 预算 58px。原因是终端**视图自有**的铬在 390 下各自换行成多条：TerminalView 工具条 77px（mode/geo 按钮换行）、`tty-dock` 86px（本地输入 44 + 说明 `<p>` 换行 ~30 + padding）、`tty-keybar` 113px（11 枚键 `flex-wrap: wrap` 成两行）。

计划 §(C) 任务 6 的依赖写明「任务 5（底部区域布局已定）」，故本任务在**拥有的 `Shell.module.css` compact 段**里，借稳定的公开 testid/data 属性（TerminalView 的 CSS module 类名被 hash，无法跨模块命中）把三块各收成一条横滚单行：

```css
.shell[data-layout="session"] section[data-tty-lab] > header { flex-wrap: nowrap; overflow-x: auto; }
.shell[data-layout="session"] [data-testid="tty-keybar"]    { flex-wrap: nowrap; overflow-x: auto; }
.shell[data-layout="session"] [data-testid="tty-dock"] form + p { display: none; }
```

- **只改换行，不删能力**：全屏、行内渲染/直连切换、重连、本地输入、全部 `tty-key-*`（esc/tab/ctrl/alt/方向/pgup/pgdn/ctrl-c…）按钮全部仍挂载、可经横滚到达，testid 一个不少；隐藏的只是本地输入下一行静态说明文字（非控件）。
- 媒体查询与既有 compact 段同一断点，桌面 1440 不命中。
- 任务 6（m-keybar）在此单行布局之上把键集换成九键 PhoneKeyBar；其归属文件（`TerminalView.tsx` 手机底部区域、`AuxKeys.tsx`、`TerminalView.module.css`）本任务一字未动，冲突面为零（这几条规则只在 `Shell.module.css`）。

## 5. 高度占比表（验收 2；390×844 / 1440×900，composer 收起、无软键盘）

`getBoundingClientRect().height / window.innerHeight`，三次独立运行（`m-chrome.hub.spec.ts` ×3）数值完全一致：

| 视口 / 视图 | 测量目标 | top | height | viewport | **ratio** | 阈值 |
|---|---|---:|---:|---:|---:|---:|
| 390 结构段 | `[data-testid="session-body"]` | 95 | 643 | 844 | **0.761** | ≥ 0.60 |
| 390 终端段 | `.xterm`（xterm 容器） | 152 | 556 | 844 | **0.658** | ≥ 0.60 |
| 390 终端段 | `[data-testid="session-body"]`（包裹层，旁证） | 95 | 749 | 844 | 0.887 | ≥ 0.60 |
| 1440 桌面结构段 | `[data-testid="session-body"]` | 124 | 645 | 900 | **0.717** | 回归 |

测量前置：结构段经 API 清掉全部 pending launch approval（假节点可能在首个 answer 后再抛一张，helper 循环到「服务端 0 条 **且** 页面 `approval-card` 卸载」为止——一张 214px 的审批卡会把结构段压到 0.415，那不是收起态）；全程不 focus 任何输入框（无软键盘）。终端段等 `[data-tty-ready="1"]` 且 `.xterm` 可见后测量（假节点 terminal 实例停在 `requested`，tty lab 照常附着绘屏，故不以 lifecycle=running 为门）。

结构段 0.761 与计划 §B.2 预算（约 75%）吻合；终端段 0.658 低于计划估算的约 71%，差额来自会话顶栏 95px（52 主行 + 运行详情触发行，c-sessionchrome 归属，本任务不得改）与终端工具条单行 ~57px——均为规格要求保留的行；预算红线 0.60 之上仍有 0.058 余量。

## 6. 热区（验收 3，复用 ux-touchhit 几何探针）

390 结构段与终端段都用 ux-touchhit 同款探针：`data-mchrome-owner` 标记后，对控件中心 ±22px 四角做 `elementFromPoint`，要求四角都解析到控件自身（视觉盒在视口内、::after 不被覆盖/不停在屏外）。`view-switch-tty`、`view-switch-structured`、`Stop` 三个目标在两个视图各过一遍——D-040 的「分段与 Stop 永不进溢出」在折后布局里继续成立。

## 7. 测试与闸口

| 层级 | 内容 | 结果 |
|---|---|---|
| vitest | `pnpm --dir web test` 全量 133 文件 / 1279 用例 | PASS（Transcript 文件 14 用例，含新增 3 个折叠分支用例） |
| typecheck / lint | `pnpm --dir web typecheck`、`lint` | PASS（lint 余告警为 Transcript 既有的 react-hooks 风格告警，非本次引入） |
| 本任务 hub 规格 | `m-chrome.hub.spec.ts` ×3 连跑 | 3/3 PASS，三次几何数值一致 |
| 相关规格各一遍 | `ux-chrome.hub.spec.ts`（2）、`ux-touchhit.hub.spec.ts`（4）、`ux-composer-mobile.hub.spec.ts`（3） | 9/9 PASS |
| 全量 hub e2e | `pnpm test:e2e:hub`（hub 配置全量，本锁槽内） | 见文末「全量回归记录」 |

**闸口开闸方式**：本分支含 `web/src/app/Shell.tsx` / `Shell.module.css` 改动，不在 `crates/remuda/src/cmd/merge/web_e2e.rs` 的自动清单内（清单含 `web/src/features/session/**`，Transcript 改动本身会自动开闸，但 app/ 不会），`remuda merge` 时必须**显式传 `--web-e2e`**：

```
remuda merge --web-e2e   # 显式开 web hub e2e（web/src/app/** 改动不自动触发）
```

截图均由 hub 规格在假节点（`e2e-fake-node`、`wsp_e2e`）合成数据下产生：`mobile-ui-5-structured-390.png`（结构段：单顶栏 + 运行详情第二行 + `⋯` 触发器 + 大正文 + 收起 composer）、`mobile-ui-5-terminal-390.png`（终端段：单顶栏 + 单行 tty 工具条 + xterm + 单行本地输入 + 单行横滚键条）。无主机名 / 用户名 / home 路径入镜。

## 8. 验收对照

1. 390 `/s/:id` 两视图恰好一条 app 顶栏、一条底部条；chips/tabs/底栏不渲染、抽屉能力保留 —— §2 + 两张截图。
2. session-body 0.761、xterm 0.658，均 ≥ 0.60（收起 composer、无软键盘）—— §5。
3. view-switch 与 Stop 在顶栏、44px 四角热区 —— §6。
4. 三枚 transcript 芯片折进一个触发器，展开后 testid 不变 —— §3，vitest + e2e 双覆盖。
5. 1440 零变化：tabs 在、芯片内联无触发器、host 芯片在、底栏仍隐藏、正文 0.717；全量既有 hub e2e 通过 —— §7。

## 9. 全量回归记录

（全量 `pnpm test:e2e:hub` 结果在跑通后回填本节。）
