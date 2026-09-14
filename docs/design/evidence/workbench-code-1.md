# Workbench code — 代码块：图标工具栏 + 语法高亮

Owner nits（verbatim intent）:

> 1. 「codeblock 的按钮比较难看…按钮布局要重新设计一下，能用图标的尽量用图标」
> 2. 「codeblock 没有代码语法高亮」

基线 `origin/main` = `9df9099`；分支
`wt/ux-code/codeblock-toolbar-and-highlight`。

浏览器验收全部跑在 `crates/remuda-hub/examples/hub_e2e.rs` 的
**fake node** 上（`playwright.hub.config.ts`），合成助手消息由 node 的
`echo:` 反射产生，未调用真实模型。截图为合成 fixture，无真实运行内容、
主机名或个人路径。

## 改了什么

### 1. 工具栏重做 — `components/CodeBlock.tsx` + `codeBlock.module.css`

- 无标题栏：代码块自身带边框与底色，工具栏是块内右上角的一枚圆角浮层，
  默认低对比（`opacity: 0`），**悬停整块或键盘聚焦按钮时显现**，140 ms
  淡入；触屏（`@media (pointer: coarse)`）没有 hover，常显。
- 两个**纯图标**按钮（`lucide-react`，代码库已有依赖）：
  - `WrapText` — **换行**：切换长行软换行；`aria-pressed` 表示当前态，
    激活时按钮有底色；偏好在本机按 viewer 持久化于
    `localStorage` 的 `runtime.code-wrap`（与 `runtime.*` 其它偏好同
    命名空间）。
  - `Copy` / 成功后变 `Check` — **复制**：复制围栏原文；成功后按钮
    变绿勾约 1.5 s 后还原，并经批次 C 的 `notify()` info 通道
    （`role="status"` live region）播报「代码 · 已复制」；剪贴板被
    浏览器拒绝时发 blocking 通知而不是静默失败。
- 语言已知时工具栏左侧显示语言名（TypeScript / Rust / Python / JSON /
  Bash）；未知语言显示 info 串原文（如 `elixir`），只是不着色。
- 两个按钮都有 `aria-label` 与 `title`（中文）、可见 focus ring；
  指针设备上 30 px，触屏命中区 **44 × 44**（390 px e2e 实测 ≥ 44）。
- **横向滚动只发生在 `<pre>` 内部**：长行默认不折行，
  `overflow-x: auto` 在 pre 上。MarkdownText 根节点补 `min-width: 0`
  （assistant 行是 column flex，默认 min-width:auto 会被代码 max-content
  撑大），所以页面（含 390 px 手机）永不横向溢出。

旧的「展开 / 收起 + 复制」文字按钮（`ui.codeHead` 内联在
`MarkdownText.tsx` 里的局部 CodeBlock）整体删除；围栏外的行内
`` `code` `` 不动。

### 2. 语法高亮 — `lib/highlight.ts`

- **highlight.js 11 core**（+8.0 kB gzip）通过 `import()` 动态加载，
  不进主 chunk；语法按需懒注册，每个语法独立 chunk：

  | chunk | gzip |
  |---|---|
  | core | 8.04 kB |
  | typescript | 3.08 kB |
  | javascript | 2.61 kB |
  | python | 1.48 kB |
  | bash | 1.56 kB |
  | rust | 1.44 kB |
  | json | 0.41 kB |

- fence info 串映射：`ts/tsx/mts/cts → typescript`、
  `js/jsx/mjs/cjs → javascript`、`rs → rust`、`py/py3/python3 → python`、
  `sh/shell/zsh → bash`，`json` 直连；fence meta（```ts {1,2} ```）忽略。
  未知语言一律原文渲染（仍显示语言标签）。
- **超过 10 000 字符不高亮**：整块原样渲染，工具栏给出「未高亮·过长」
  注记（title 有完整中文说明）。
- 着色在 CodeBlock 内、**rehype-sanitize 之后**作为纯表现层应用：未着色
  路径永远是被 React 转义的文本；着色路径插入的是 highlight.js 已转义的
  token HTML（`&lt;`/`&gt;`/`&#x27;`），单测覆盖
  `<img onerror>` / `<script>` 围栏体在两条路径上都不能注入标记。
- token 配色是 `codeBlock.module.css` 里按主题定义的 **CSS 变量**
  （`--tok-*`，night 与 ledger 各一套，全部取自既有令牌的色相区间），
  `.hljs-*` 类映射到变量；没有 vendor 任何 highlight.js 主题文件。
- tokenizer 永不抛到 UI：`ignoreIllegals` + 任何失败回退纯文本，且组件
  对异步结果带取消标记，流式重渲染不会把旧结果写到新代码上。

### 3. 测试

- 单测（vitest，19 例，`highlight.test.ts` + `MarkdownText.test.tsx`）：
  复制原文 / ✓ 瞬态 / `notify` info 播报、换行偏好持久化与重挂载回放、
  aria-label/title/pressed、ts/rust/py/json/bash 五语言出 token span、
  未知语言纯文本但保留标签、10k 上限与注记、恶意围栏体两条路径均不能
  注入标记、行内 code 无工具栏。
- hub e2e `tests/e2e/ux-code.hub.spec.ts`（fake node，合成
  TypeScript + Bash 围栏消息）：
  - 工具栏默认透明、hover 后不透明；两按钮名称/title 正确；
  - 长行 pre 内 `scrollWidth > clientWidth`，同时
    `documentElement.scrollWidth ≤ innerWidth`；
  - 复制写真实 CDP 虚拟剪贴板（授权 `clipboard-read/write`）并出现
    「已复制」态、约 1.5 s 还原；
  - 换行切换后 pre 内溢出归零且页面仍无横向滚动，
    `runtime.code-wrap=1` 落盘；
  - 390 px 触屏：工具栏常显，两按钮命中盒 ≥ 44 px，默认长行内滚、
    切换换行后页面依旧不横向溢出。

## 截图（合成 fixture）

桌面 1440 宽（默认 night 与 ledger 各一）、手机 390 宽（双主题各一），
消息为合成 TypeScript / Bash 围栏：

- `workbench-code-1-night-1440.png` / `workbench-code-1-ledger-1440.png`
- `workbench-code-1-night-390.png` / `workbench-code-1-ledger-390.png`

## 体积影响（`vite build`，gzip）

| | 改前 | 改后 | Δ |
|---|---|---|---|
| 主 chunk `index-*.js` | 345.44 kB | 348.29 kB | **+2.85 kB** |
| 主 CSS chunk | 114.72 kB raw | 114.86 kB raw | 本模块规则 gzip 后约 +0.32 kB |
| highlight core / 语法 | — | 8.04 + 0.41~3.08 kB/语法 | 全部懒加载，不进主 chunk |

主 chunk 的增量来自 CodeBlock 组件本体、语言注册表条目和
`lucide-react` 的三个图标（tree-shake 后）；只有真出现围栏代码时才拉
core，出现某种语言时才拉对应语法 chunk。

> 注：上表是本分支相对基线 `9df9099` 的净增量。合并最新 `origin/main`
> （批次 E/F 等已进入）后主 chunk 为 353.86 kB gzip，差异来自其它批次；
> 懒加载的 highlight core/语法 chunk 与上表一致，仍不在主 chunk。

## 验收对照（owner nits）

- 图标化、块内右上圆角浮层、低对比 hover/focus 显现、中文 tooltip、
  无文字按钮 — 见截图与 e2e。
- 44 px 触屏命中、可见焦点环、键盘可达 — 390 px e2e 实测命中盒。
- 语法高亮、双主题 — 五个语法单测 + 双主题截图。
- 长行只在块内横滚，页面永不横滚 — 1440 与 390 两条 e2e 都断言
  `documentElement.scrollWidth ≤ window.innerWidth`。
