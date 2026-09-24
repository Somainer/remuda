# 视觉系统（UI Overhaul 令牌契约）

状态：可开工规格（2026-09-23，D-053）。
本文是 UI 整体重做的**机械权威**：颜色、字体、字号、间距、圆角、层级、焦点、动效与基础控件的取值以此文为准；D-053 只钉边界与取舍。UO-1 在 `web/src/styles/tokens.css` 实现令牌与别名，各界面任务只允许引用角色令牌。

设计目标：1440 / 768 / 390 三档共用一套视觉系统——颜色按角色命名、深浅两态各自调校；中西文同源的系统字体；720px 阅读列；区域靠色阶与留白分隔，只有浮层带阴影。唯一发布的调色板是微暖低彩的「墨」，深色为默认底色、浅色单独调校，两态之间**不做反相**。

---

## 1. 属性、偏好与首帧

### 1.1 `tokens.css` 的结构

模式分支只允许出现在 `web/src/styles/tokens.css` 这一个文件里：

```css
:root { color-scheme: dark; /* 深色角色值 */ }
@media (prefers-color-scheme: light) {
  :root:not([data-appearance="dark"]) { color-scheme: light; /* 浅色角色值 */ }
}
:root[data-appearance="light"] { color-scheme: light; /* 与上一块逐字相同，单测比对 */ }
```

tokenGuard 断言两条：

- 模式属性只允许出现在 `tokens.css` 中、且只允许以下两种**作用于 `:root`** 的选择器形式：`:root[data-appearance="dark|light"]`（显式选择）与 `:root:not([data-appearance="dark"])`（系统浅色块）；组件模块里不得出现任何 `[data-appearance` 选择器；
- 两个浅色块（媒体查询块与显式选择器块）的声明集合**逐字相等**。

### 1.2 偏好键

- 键名 `runtime.theme.v1`，取值 `system | dark | light`，默认（键缺失）为 `system`。
- 旧值映射：`night` 读作 `dark`，`ledger` 读作 `light`。
- 写入失败时抛错、调用方回滚——与今天的存储契约一致，由 `appearance.test.ts` 覆盖。

### 1.3 运行时 `features/settings/appearance.ts`

由今天的 `theme.ts` 改名，导出 `readAppearance / applyAppearance / writeAppearance`。

`applyAppearance` 的行为：

- 显式 `dark` 或 `light`：在 `:root` 写 `data-appearance`，并把两个 theme-color meta 的 content 都设为 `getComputedStyle(:root).getPropertyValue('--bg-canvas')`（只在切换时读一次）；
- `system`：移除 `data-appearance`，并把两个 meta 恢复为 `index.html` 里的默认值。

`main.tsx` 在挂载前调用一次；模式切换只改属性，不触发 React 重渲染。

**过渡镜像（最后一个界面任务合入后删除）**：`applyAppearance` 同时写 `data-theme="night|ledger"`，值取当前解析结果，让尚未迁移的 `[data-theme]` 选择器继续生效；`system` 下挂一个 `matchMedia('(prefers-color-scheme: light)')` 的 change 监听，只用来更新这个镜像。新代码一律不读 `data-theme`。

### 1.4 首帧与静态资源

`web/index.html`：

- 删除静态的 `data-theme="night"`；
- 加 `<meta name="color-scheme" content="dark light">`；
- 加两个 theme-color：`<meta name="theme-color" media="(prefers-color-scheme: dark)" content="#232220">` 与 `<meta name="theme-color" media="(prefers-color-scheme: light)" content="#f9f8f5">`；
- `<title>` 与 `apple-mobile-web-app-title` 改为 `Remuda`。

外观由纯 CSS 媒体查询解析，**不加任何阻塞首帧的脚本**。缓存层按现状描述、本次不改：

- HTTP 头（`crates/remuda-hub/src/web.rs:103-112`）：`index.html` 与 `sw.js` 返回 `no-cache`，哈希 `/assets/*` 为 immutable 长缓存；manifest、图标、favicon 等其余非资产路径无显式缓存指令。
- Service worker（`web/sw.src.js`，本次不改、不新增资源）：install 时 `cache.addAll` 预缓存 SHELL 清单（`/`、`/index.html`、`/manifest.webmanifest`、`/favicon.svg`、两个图标）；导航请求 network-first——优先取网络文档并在成功时刷新缓存壳，网络失败才回退缓存的 `/index.html`；`/v1/`、`/node/`、`/push/` 不经过缓存，其他同源 GET 走 cache-first。
- 首帧颜色因此由 `no-cache` 的 `index.html` + network-first 导航保证：系统浅色设备首帧即为浅色，不依赖任何同步脚本。

`manifest.webmanifest`：`name` / `short_name` 改为 `Remuda`，`theme_color` 与 `background_color` 都用 `#232220`。

首帧行为：从未选过主题、系统为浅色的设备第一帧就是浅色；显式选了与系统相反模式的设备，在模块脚本执行前可能短暂出现系统色——与今天的行为相同。

---

## 2. 颜色角色（「墨」）

十六进制字面量只允许出现在四个地方：`tokens.css`、`features/session/tty/theme.ts`、`web/index.html`（theme-color）、`manifest.webmanifest`。尺寸令牌保留 `--text-*` 前缀，颜色角色不得占用 `--text` 这个名字。

| 角色 | 用途 | 深 | 浅 |
|---|---|---|---|
| `--bg-nav` | 侧栏、手机底栏 | `#1c1b19` | `#f1efea` |
| `--bg-canvas` | 页面与阅读底，也用于 `html`、`body` 和页头 | `#232220` | `#f9f8f5` |
| `--bg-surface` | 卡片、列表组、次按钮、结束条 | `#2a2926` | `#ffffff` |
| `--bg-raised` | 菜单、弹出层、对话框、sheet、分段的选中项 | `#312f2c` | `#ffffff` |
| `--bg-input` | 输入框、composer | `#2a2926` | `#ffffff` |
| `--bg-inset` | 代码块、分段轨道、禁用输入、原文 `<pre>` | `#1d1c1a` | `#f2f0eb` |
| `--bg-hover` | 悬停覆盖层 | `rgb(255 246 230 / .05)` | `rgb(60 45 20 / .05)` |
| `--bg-selected` | 选中覆盖层 | `rgb(255 246 230 / .075)` | `rgb(60 45 20 / .07)` |
| `--border` | 装饰性分隔 | `#3a3834` | `#e2ded6` |
| `--divider` | 表面内部的细线 | `#2e2d2a` | `#ebe8e2` |
| `--border-control` | 控件边缘及其状态（全集 ≥ 3:1） | `#8e8980` | `#847e74` |
| `--fg-strong` | 标题、选中项、当前面包屑段 | `#f1ede6` | `#1f1e1b` |
| `--fg-body` | 正文 | `#dcd7ce` | `#393731` |
| `--fg-muted` | 元信息 | `#b3ada2` | `#57524a` |
| `--fg-faint` | 占位文字、时间戳 | `#b0a99d` | `#69635a` |
| `--link` | 链接、working 状态 | `#9ec0dc` | `#2f5f86` |
| `--focus` | 焦点环 | `#a9c9e6` | `#2f6aa0` |
| `--attention-fg` / `-bg` / `-border` | 需要你、blocked、待审批 | `#e0b872` / 同色 .13 / 同色 .45 | `#835610` / .10 / .35 |
| `--danger-fg` / `-bg` / `-border` | 失败、破坏性动作 | `#ec9a94` / .13 / .45 | `#a3383e` / .10 / .35 |
| `--success-fg` / `-bg` | 只用于权威确认 | `#a3c29a` / .13 | `#3a6843` / .10 |
| `--unknown-fg` / `-border` | 未知：等于 `--fg-muted` 与 `--border-control`，以虚线绘制且必须配文字 | 同左 | 同左 |
| `--primary-fill` / `--on-primary` | 中性主按钮（反色填充） | `#ece7de` / `#23221f` | `#2a2926` / `#faf9f6` |
| `--on-attention` | 琥珀色计数徽标上的数字 | `#1f1e1b` | `#ffffff` |
| `--scrim` | 遮罩 | `rgb(10 9 8 / .62)` | `rgb(31 30 27 / .28)` |
| `--diff-add-bg` / `-del-bg` / `-ctx-bg` | diff（必须配 +/- 字形） | `rgb(163 194 154/.14)`、`rgb(236 154 148/.14)`、`rgb(255 246 230/.03)` | `rgb(58 104 67/.10)`、`rgb(163 56 62/.09)`、`rgb(60 45 20/.03)` |
| `--term-bg` / `--term-fg` | 终端，两态相同 | `#1a1917` / `#e4dfd6` | 同左 |
| `--tok-*` | 代码语法色，两态分设 | UO-1 从 `components/codeBlock.module.css` 现有的两组 `[data-theme]` 值平移，并逐个校到 `--bg-inset` 上 ≥ 4.5:1 | 同左 |
| `--ember-*` | effort 顶档余烬 | UO-1 从 `features/session/session.module.css` 现有 `.effortCard` / `.effortKnobUltra` 的 night/ledger 模式分支原值平移 | 同左 |

角色族一览（D-053 第 1 条）：

- 背景：`--bg-nav/--bg-canvas/--bg-surface/--bg-raised/--bg-input/--bg-inset/--bg-hover/--bg-selected`
- 文字：`--fg-strong/--fg-body/--fg-muted/--fg-faint`
- 线条：`--border/--divider/--border-control`
- 链接与焦点：`--link/--focus`
- 状态：`--attention-*/--danger-*/--success-*/--unknown-*`
- 主按钮：`--primary-fill/--on-primary`
- 专用域：`--diff-*-bg`、`--tok-*`、`--term-*`

**状态色纪律**：

- 琥珀只表示「需要你 / 注意」；
- 红只表示失败和破坏性动作；
- 绿只表示权威确认的成功（merged、passed、回执已确认）——idle、已连接、在线都不用绿；
- 未知 = 中性色 + 虚线形状 + 文字「未知 / 待确认」（ui-spec §3.3、D-038 的「状态待确认」口径）；
- 按钮不用状态色填充；主按钮是中性的反色填充。

说明：深色的 `--fg-faint` 与 `--fg-muted` 亮度很接近（在 canvas 上分别为 6.82 和 7.13），这是「faint 在 selected 覆盖层叠 raised 的底上也要 ≥ 4.5」的直接代价。深色下两者的层级靠用途区分：faint 只用于占位文字和时间戳，而且固定 12px。

---

## 3. 对比度（WCAG 全集复算）

所有数值按 WCAG 相对亮度公式计算；半透明覆盖层（hover、selected、状态底）先按 sRGB alpha 合成到不透明底上再算对比度。

### 3.1 全集定义

`tokens.contrast.test.ts` 在这个全集上断言：

- **底**：五种不透明底（nav、canvas、surface、raised、inset），加上 hover、selected 分别叠在这五种底上，共 15 种；
- strong、body 另外叠在 attention、danger、success 三种状态底 × 五种底上；
- 各状态文字叠在自己的状态底 × 五种底上；
- `--bg-input` 与 surface 同值，不单列。

文本角色 ≥ 4.5:1；`--border-control` 与 `--focus` ≥ 3:1。

### 3.2 文本矩阵

「最差」列同时给出发生的底色。

| 文本角色 | 深：canvas / surface / raised / nav / inset | 深·最差（底） | 浅：canvas / surface=raised / nav / inset | 浅·最差（底） |
|---|---|---|---|---|
| strong | 13.62 / 12.47 / 11.44 / 14.75 / 14.59 | **8.64**（attention 底叠 raised，`#484135`） | 15.70 / 16.67 / 14.51 / 14.64 | **12.55**（danger 底叠 nav，`#e9ddd9`） |
| body | 11.09 / 10.15 / 9.31 / 12.01 / 11.88 | **7.04**（同上） | 11.20 / 11.90 / 10.36 / 10.45 | **8.96**（同上） |
| muted | 7.13 / 6.52 / 5.98 / 7.72 / 7.63 | **4.78**（selected 叠 raised，`#403e3a`） | 7.29 / 7.75 / 6.74 / 6.80 | **5.94**（selected 叠 nav，`#e4e1db`） |
| faint | 6.82 / 6.24 / 5.72 / 7.38 / 7.30 | **4.58**（同上） | 5.60 / 5.94 / 5.17 / 5.22 | **4.55**（同上） |
| link | 8.35 / 7.64 / 7.01 / 9.04 / 8.94 | **5.60**（同上） | 6.37 / 6.77 / 5.89 / 5.94 | **5.19**（同上） |
| attention | 8.53 / 7.81 / 7.16 / 9.24 / 9.14 | **5.41**（自身底叠 raised） | 5.99 / 6.36 / 5.53 / 5.58 | **4.84**（自身底叠 nav） |
| danger | 7.28 / 6.66 / 6.11 / 7.88 / 7.80 | **4.78**（自身底叠 raised，`#493d3a`） | 6.19 / 6.58 / 5.72 / 5.78 | **4.95**（自身底叠 nav） |
| success | 8.13 / 7.44 / 6.83 / 8.80 / 8.71 | **5.22**（自身底叠 raised，`#40423a`） | 6.10 / 6.48 / 5.64 / 5.69 | **4.94**（自身底叠 nav） |

### 3.3 非文本

| 项 | 深 | 浅 |
|---|---|---|
| `--border-control` | 不透明底最低 3.84，全集最差 3.07（selected 叠 raised） | 不透明底最低 3.50，全集最差 3.08（selected 叠 nav） |
| `--focus` | 不透明底最低 7.74，全集最差 6.19 | 不透明底最低 4.95，全集最差 4.36 |
| `--on-primary` | 12.92 | 13.82 |
| `--on-attention` | 8.94 | 6.36 |

### 3.4 终端（相对 `#1a1917`）

| 类别 | 对比度 |
|---|---|
| 前景 fg | 13.24 |
| 常规色 | red 6.67，green 8.56，yellow 9.02，blue 7.65，magenta 7.56，cyan 8.43，white 10.67 |
| brightBlack | 5.06（旧值 1.42） |
| 其余 bright 色 | 8.97–15.73 |
| 选区上的前景 | 8.53 |

旧值对照：`--mute` 在 `--ink-2` 上为 4.12；终端 brightBlack 为 1.42。

---

## 4. 终端配色（`features/session/tty/theme.ts`）

- **命名**：新增 `export const TERMINAL_THEME`，同时保留 `export const NIGHT_CORRAL_THEME = TERMINAL_THEME` 作为过渡别名；引用方切到新名字后，别名在最后一个界面任务删除。
- **基础色**：background `#1a1917`，foreground `#e4dfd6`，cursor `#e0b872`，cursorAccent `#1a1917`，selectionBackground `#3d3a35`。
- **常规色**：black `#2c2a27`、red `#e8837c`、green `#9dbf87`、yellow `#dcb46a`、blue `#86aee0`、magenta `#c79ad8`、cyan `#7fbfbb`、white `#cfc9be`。
- **亮色**：brightBlack `#8f897e`、brightRed `#f4a59e`、brightGreen `#b9d6a4`、brightYellow `#ecd08f`、brightBlue `#a9c7ee`、brightMagenta `#dcb8e8`、brightCyan `#a0d8d3`、brightWhite `#f6f2ea`。
- **256 色立方**：删除自绘的 `nightCorralExtendedAnsi`，改用 xterm 的标准 256 色立方，不再染色。
- **字体**：`TERMINAL_FONT_FAMILY` 仍以 IBM Plex Mono 打头。
- 终端跟随外观（2026-09-24 所有者改定，取代恒深色口径）：深、浅两套调色板随外观切换，切换只设 `term.options.theme`、渲染器就地重绘，不重建终端、不丢滚动历史；两套板各自的 16 个非黑 ANSI 色在本板终端底上 ≥ 4.5:1，终端输出的颜色（含 256 色立方与 truecolor）不被改写。浅板 white/brightWhite 为深灰（TUI 默认文字仍可读），真白走 truecolor/256 色 231。

---

## 5. 字体与字号

### 5.1 字体族

- `--font-ui`：`-apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei UI", "Microsoft YaHei", "Noto Sans CJK SC", "Source Han Sans SC", system-ui, sans-serif`
- `--font-read`：`--font-ui` 的别名——UI 与长文同源，不打包任何正文 webfont，也不打包衬线或 CJK webfont。
- `--font-mono`：`"IBM Plex Mono", ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, "PingFang SC", "Microsoft YaHei", monospace`

等宽只用于代码、路径、终端、ID 和需要对齐的数字。

**依赖与许可**：

- 删除 `@fontsource/ibm-plex-sans`（`main.tsx` 的三处导入与 `package.json` 依赖）；
- plex-mono 保留 latin 400 和 500 两个字重，位于 `/assets` 下，带 hash，走长缓存；
- 提交 `web/LICENSES/IBM-Plex-Mono-OFL.txt`（OFL-1.1），并在 `NOTICE` 追加条目。

**body 设置**：删除 `text-rendering: optimizeLegibility`；加 `-webkit-text-size-adjust: 100%`。

**字重**：400 正文，500 控件，600 标题；不用 700。中文不加负字距。

**数字**：计数与时长用 `.num { font-variant-numeric: tabular-nums }`。

### 5.2 字号令牌

| 令牌 | 用途 | ≥768 | <768 |
|---|---|---|---|
| `--text-ui` | 导航项、列表行、按钮、菜单项 | 13px/19px | 15px/22px |
| `--text-meta` | 辅助文字、时间、芯片、徽标文字 | **12px/16px** | **12px/16px**（所有宽度相同，D-039） |
| `--text-title` | 行、卡、面板标题（600） | 14px/20px | 16px/22px |
| `--text-page` | 手机首页、收件箱、登录页的标题，空态主句（600） | 18px/26px | 20px/28px |
| `--text-read` | transcript 正文、composer | 16px/28px | 16px/28px |
| `--text-input` | 文本输入 | 14px（`pointer: fine`） | 16px（`pointer: coarse`，不看宽度） |
| `--text-code` | 代码块、原文 `<pre>` | 13px/1.6 | 13px/1.6 |
| `--text-2xs` | 只用于徽标数字、kbd 字形 | 11px | 11px |

规则：

- 承载信息的文字 ≥ 12px；`--text-meta` 在所有宽度下都是 12px，落实 D-039 对 `.meta` 一类辅助/诊断文本桌面与手机统一为 12px 的要求（D-039 决策原文为「`.meta` 一类辅助/诊断文本**桌面与手机统一** `var(--text-aux)`（12px）」）；
- 11px 只用于与形状绑定的徽标数字和 kbd 字形，属于 D-052 第 9 条的图形标注豁免，逐站点登记；
- 文本输入在 `pointer: coarse` 下为 16px，iOS 聚焦不再放大页面；
- 按宽度变化的值只在 `tokens.css` 的 `@media (max-width: 767px)` 里重设 `--text-ui`、`--text-title`、`--text-page` 三个；组件内不写宽度分支；
- 旧的字号令牌暂时保留原值，过渡期结束后删除；
- `.md` 标题：h1 20/28、上 24 下 12；h2 18/26、上 20 下 8；h3 16/24、上 16 下 4；三者都用 600。

---

## 6. 间距、控件尺寸与布局

### 6.1 间距

- `--space-1..5` = 4 / 8 / 12 / 16 / 24（与今天相同）；新增 `--space-6` 32、`--space-7` 48。
- `--gutter` = 24（≥1200）/ 20（768–1199）/ 16（<768）。

### 6.2 控件尺寸

命中区按 `pointer: coarse` 判定，不按宽度（详见 §8 与 ui-spec §3.4）。

| 令牌 | `pointer: fine` | `pointer: coarse` |
|---|---|---|
| `--control-h`（文字按钮、输入框） | 32px | 44px 可见 |
| `--control-h-sm` | 28px | 36px 可见，`::after` 补到 44 |
| `--control-h-lg` | 36px | 48px |
| `--touch` | 44px | 44px |
| `--seg-item` | 26px | 26px 可见，`::after` 补到 44，最小宽 44 |
| `--icon-btn` | 32px | 32px 可见，`::after` 为 44×44 |

### 6.3 布局令牌

| 令牌 | 值 |
|---|---|
| `--sidebar` | 248（≥1024） |
| `--sidebar-narrow` | 216（768–1023） |
| `--sidebar-collapsed` | 48 |
| `--header-h` | 48 |
| `--tabs-h` | 36 |
| `--top-mobile` | 52（不变） |
| `--phone-nav-h` | 56 |
| `--composer-row` | 56（手机收起态） |
| `--index` | 272（≥1200）/ 240（1024–1199） |
| `--aside` | 300 |
| `--read-measure` | 720 |
| `--read-gutter` | 32 / 24 / 16 |
| `--safe-bottom`、`--workbench-*` | 不变 |

---

## 7. 圆角、层级、焦点与动效

### 7.1 圆角

- `--radius-xs` 4：行内 code、徽标；
- `--radius-sm` 6：按钮、输入框、分段项、菜单项；旧的 `--radius` 直接改为 6；
- `--radius-md` 8：卡片、菜单、分段轨道、代码块；
- `--radius-lg` 12：composer、对话框、结束条；
- `--radius-xl` 16：sheet 顶角；
- `--radius-pill` 999。

### 7.2 层级

| 层级 | 做法 | 阴影 |
|---|---|---|
| 0 级 | 色阶区分；同色相邻时加 1px `--divider` | 无 |
| 1 级 | surface + 1px `--border` | 无 |
| 2 级 | raised + 1px `--border` + `--shadow-2` | 深：`0 8px 24px rgb(0 0 0/.40), 0 1px 2px rgb(0 0 0/.30)`；浅：`0 8px 24px rgb(31 30 27/.10), 0 1px 3px rgb(31 30 27/.08)` |
| 3 级 | 对话框、sheet，放在 `--scrim` 之上 | `--shadow-3`。深：`0 24px 64px rgb(0 0 0/.55)`；浅：`0 24px 64px rgb(31 30 27/.18)` |

只有浮层（菜单、弹出层、对话框、sheet）带阴影，共两级 `--shadow-2` / `--shadow-3`。D-052 第 8 条原文为「不新增 z-index / elevation / 阴影 / disabled token」，本系统只放开其中的**阴影**部分（D-053 第 8 条），z-index 与 disabled token 的口径不变。所有层级都不使用 backdrop-filter。

### 7.3 焦点

- 通用：`--focus-ring: 2px solid var(--focus)`，offset 2px，只在 `:focus-visible` 时显示；
- 滚动容器里的行：offset 改为 -2px；
- 文本输入：不画 outline，改为边框变成 `--focus`，并加 `0 0 0 1px var(--focus)`；composer 用 `:focus-within` 实现同样效果。

### 7.4 动效

- 时长：`--dur-1` 100ms，`--dur-2` 160ms，`--dur-3` 240ms；
- 缓动：`--ease-out: cubic-bezier(.2,0,0,1)`，`--ease-in: cubic-bezier(.3,0,1,1)`；
- 只动 opacity、transform 和颜色；流式内容没有入场动画，transcript 行不做入场；模式切换即时生效；`will-change` 只在动画进行中设置；
- reduced-motion：用 `[data-motion="essential"]` 豁免需要保留的元素，其余一律不动。

---

## 8. 基础控件（`ui.module.css`，类名保留）

### 8.1 按钮

高 `--control-h`，左右内边距 12px（coarse 下 16px），`--text-ui` 500，圆角 6px。每个视图区域最多一个主按钮。

| 类 | 样式 |
|---|---|
| `.btnPrimary` | `--primary-fill` 底，`--on-primary` 字 |
| `.btn` | surface 底，1px `--border`，`--fg-body` 字；hover 时边框换成 `--border-control` |
| `.btnGhost` | 透明底，`--fg-muted` 字；hover 为 `--bg-hover` |
| `.btnDanger` | 透明底，1px `--danger-border`，`--danger-fg` 字；hover 底为 `--danger-bg`。不做红色填充按钮 |
| 禁用 | opacity .45 |

**`.iconBtn`**：可见 32×32，字形 16px。coarse 下加 `::after` 热区 44×44，相邻 iconBtn 间距 ≥ 12px（32 + 12 = 44，热区不重叠）。

### 8.2 分段 `.seg` / `.segItem`

- 轨道：1px `--border`，2px 内边距，`--bg-inset` 底，8px 圆角，总高 32px；
- 项：可见 26px 高，左右 10px，`--text-meta` 500，`--fg-muted` 字；
- 选中（`aria-checked`、`aria-selected` 或 `aria-pressed` 为 true）：`--bg-raised` 底，`--fg-strong` 字，外加 `inset 0 0 0 1px var(--border-control)`，字重不变；
- coarse 下：每项 `min-width: 44px`，`::after { inset: -9px 0 }` 把竖向热区补到 44；热区只向上下扩，相邻项不重叠；
- ARIA：筛选用 `radiogroup/radio`，视图切换用 `tablist/tab`。

### 8.3 输入框

高 `--control-h`，textarea 最小 88px；`--text-input` 字号；`--bg-input` 底；1px `--border-control`；圆角 6px；`--fg-strong` 字；占位文字 `--fg-faint`。

校验失败（`aria-invalid`）：边框换成 `--danger-fg`，并显示一行 12px 的 ⚠ 提示。

### 8.4 菜单与 sheet

- 菜单容器：`--bg-raised` 底，1px `--border`，8px 圆角，`--shadow-2`，内边距 4px，最小宽 200px；菜单项高 `--control-h`；勾选状态用 16px 的 ✓ 列表示；
- `.sheet`：`--bg-raised` 底，顶角 16px，底部留出 `--safe-bottom`，顶部有一条 36×4 的把手。

### 8.5 芯片与徽标

**芯片 `.chip`**：可见 24px，左右 8px，12px/500，1px `--border`，`--fg-muted` 字。

- `.chipOn`：`--bg-selected` 底，`--border-control` 边框，`--fg-strong` 字；
- coarse 下：`::after` 把竖向热区补到 44，`min-width: 44px`。

**徽标 `.badge`**：20px 高，12px/500。

- attention、danger、success、neutral 四种都是「状态底 + 状态字」；`.pill` 等同于 neutral；
- unknown 用虚线边框，并且必须带文字。

### 8.6 StateDot（8px）

| 状态 | 画法 |
|---|---|
| working | `--link` 实心点 |
| starting | `--fg-faint`，dim 脉冲 |
| idle | `--fg-muted` 空心环 |
| exited | `--fg-faint` 小方块 |
| blocked | `--attention-fg` 的 ⚠ |
| unknown | 虚线环 |

### 8.7 正文 `.prose` / `.md`

- 正文 `--text-read`，段间距 12px；
- 引用：3px `--border-control` 竖线，`--fg-muted` 字；
- 表格：14px/1.5，外面包一层横向滚动；
- 行内 code：0.875em，`--bg-inset` 底。

---

## 9. 旧 → 新令牌迁移

别名放在 `tokens.css` 末尾，过渡期内保留，最后一个界面任务合入后删除。

| 旧令牌 | 新令牌 | 例外 |
|---|---|---|
| `--canvas` | `--bg-nav` | `html` 改用 `--bg-canvas`（修 iOS 回弹时露出的色带） |
| `--ink` | `--bg-canvas` | |
| `--ink-0` | `--bg-inset` | |
| `--ink-1` | `--bg-surface` | 用作文字色的地方改为 `--fg-strong` |
| `--ink-2` | `--bg-surface` | modal、popover 改为 `--bg-raised` |
| `--line` | `--border` | 表面内部的线改为 `--divider` |
| `--paper` | `--fg-body` | 标题改为 `--fg-strong` |
| `--mute` | `--fg-muted` | |
| `--dust` | `--attention-fg` | 作为主按钮填充的地方改为 `--primary-fill`；计数徽标保持 attention |
| `--on-dust` | `--on-attention` | 主按钮上的字改为 `--on-primary` |
| `--cold` | `--link` | 焦点改为 `--focus` |
| `--ok` | `--success-fg` | idle、在线改为 `--fg-muted` |
| `--danger` | `--danger-border` | |
| `--danger-strong` | `--danger-fg` | |
| `--warn` | `--attention-fg` | |
| `--info` | `--link` | |
| `--diff-*` | `--diff-*-bg` | |
| `--font` | `--font-ui` | |
| `--mono` | `--font-mono` | |
| `--radius` | `--radius-sm` | |
| `--text-label`（11px） | `--text-meta` | 只有徽标数字用 `--text-2xs` |
| `--text-aux` | `--text-meta` | |
| `--rail` | `--sidebar-collapsed` | |
| `--list` | `--index` | |
| `--top`（56） | `--header-h`（48） | 旧名保留旧值 |
| `--bar`（64） | `--phone-nav-h`（56） | 旧名保留旧值 |

**模式选择器** `[data-theme="night"|"ledger"] …` 不迁移到 `data-appearance`，按顺序处理：

1. 先把这些分支里的取值原样提成 `--tok-*` 和 `--ember-*` 令牌，按深浅两态分别写进 `tokens.css`；
2. 所在文件的 owner 删掉分支、改用令牌；
3. 迁移完成前，由过渡镜像 `data-theme` 保持这些分支继续生效。

**护栏**：重做过的模块首行标 `/* @tokens strict */`。tokenGuard 在这些模块里禁止：十六进制颜色、旧 token 名、`[data-theme`、`[data-appearance`、`prefers-color-scheme`、小于 12px 的字号字面量（登记过的图形站点除外）、`transition: all`、`backdrop-filter`。
