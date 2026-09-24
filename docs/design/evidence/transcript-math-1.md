# Transcript math — LaTeX 公式渲染（KaTeX）

Owner 请求（2026-09-24，附截图）:

> 「现在 latex 公式没有渲染」

当时 `$$…$$` 在 transcript 里原样显示，还被 markdown 破坏
（下划线变强调、反斜杠丢失）。D-053 addendum 第 15 条把数学排版移入
范围；规则全文见 `docs/design/decisions.md` D-053 文末，机械取值见
`docs/design/visual-system.md` §10。

浏览器验收全部跑在 `crates/remuda-hub/examples/hub_e2e.rs` 的
**fake node** 上（`playwright.hub.config.ts`）：合成助手消息由 node 的
`echo:` 反射产生，未调用真实模型。截图为合成 fixture，无真实运行内容、
主机名或个人路径。

## 改了什么

### 1. 先于 markdown 的数学扫描 — `lib/mathSegments.ts`

- `$$…$$`、`\[…\]` 为 display，`$…$`、`\(…\)` 为 inline；数学段在
  markdown 解析前切出，公式内的下划线、星号、反斜杠永远不会被 markdown
  处理。
- 单 `$` 走 pandoc 口径（规则经 pandoc 3.5 源码
  `src/Text/Pandoc/Parsing/Math.hs` 核实）：开 `$` 后紧跟非空白；闭
  `$` 前为非空白、其后不能是数字。`花了 $5 和 $10` 与
  `$HOME and $PATH`（闭 `$` 前是空白）为文本；`$PATH:$HOME` 与
  pandoc 一致仍是数学（`:`、`H` 都满足闭合规则），单测明确钉住该口径。
- 代码 span、fenced code block（含流式未闭合 fence）、链接 `](…)`
  目标内永不解析数学；`\$` 为字面美元符；空行使定界符作废（pandoc
  `notFollowedBy blankline`）。
- 流式安全：消息末尾未闭合的 `$$` / `\[` 使其后整段保持纯文本，闭合
  定界符到达后才整体成为公式，绝不渲染半个公式。
- 切出的公式重写成 remark-math 的 `$`/`$$` 语法，公式内真实 `$` 换为
  私有区记号过 parser，渲染前还原；被拒的 prose `$` 转义为字面量。

### 2. KaTeX 懒加载与渲染 — `lib/mathRender.ts` + `components/MathBlock.tsx`

- KaTeX JS、CSS、woff2 字面全是异步 chunk：第一条数学节点挂载时才请求；
  无数学的 transcript 零 KaTeX 请求（e2e 断言）。
- chunk 到达前显示 TeX 源码占位（中性 `--fg-muted`；display 为 inset
  块），不留白；到达后原位替换，虚拟行由 Transcript 既有的 per-row
  ResizeObserver 重新测量，未改 Transcript.tsx。
- 渲染选项固定 `output: "htmlAndMathml"`（MathML 供屏幕阅读器）、
  `throwOnError:false`、`trust:false`、`strict:"ignore"`。坏公式显示
  `--danger-fg` 源码与错误 title，不炸消息。
- KaTeX 输出经 React 组件注入，不经过 rehype-sanitize，也无消息原始
  HTML 透传。
- 选区完全位于公式内时复制得到 TeX 源码；延伸到公式外保持浏览器默认。
- display 超宽只在公式块内横向滚动（`overflow-x:auto` +
  `min-width:0`），页面永不横滚；inline 字号 1.1em，普通公式不撑高
  28px 阅读行；公式只用 currentColor，深浅两态均为阅读墨色。
- KaTeX 样式表 `components/mathKatex.css` 由上游 `katex.css` 删除
  ttf/woff `src`（woff2 only，20 面、约 292 KB）并改写字体 URL 生成，
  文件头记录了重新生成命令。

## 测试

- 单测（vitest，44 例）：`lib/mathSegments.test.ts`（24 例：定界符、
  pandoc 货币/空白/数字规则、shell 变量、代码与链接目标、空行、流式未
  闭合、跨段配对回归）、`lib/mathRender.test.ts`（4 例：
  htmlAndMathml、坏公式、引擎异常不抛出、`trust:false` 下
  href/url/html 指令不产生锚点/属性）、
  `components/MathBlock.test.tsx`（6 例：占位→KaTeX、display 块、
  currentColor、坏公式、内外选区复制）、`MarkdownText.test.tsx`
  math 段（10 例端到端管线）。
- hub e2e `tests/e2e/math-render.hub.spec.ts`（fake node）：owner 的
  softmax 公式 + inline + 货币句 + 坏公式 + 超宽公式，深浅两态：
  KaTeX 输出在位、原始 TeX/定界符在助手消息中不可见（MathML 树除外）、
  货币保持文本、坏公式显示源码、页面无横向溢出而超宽公式块内滚动；
  另立一例断言无数学的会话不请求任何 KaTeX 资源。

## 截图（合成 fixture，`REMUDA_EVIDENCE=1` 生成）

- `transcript-math-1-dark-1440.png` / `transcript-math-1-light-1440.png`
- `transcript-math-1-dark-390.png` / `transcript-math-1-light-390.png`

## 体积影响（`vite build`）

| | 改前 | 改后 | Δ |
|---|---|---|---|
| 主 chunk `index-*.js` | 1501.63 kB（gzip 444.60 kB） | 1512.84 kB（gzip 448.07 kB） | +11.21 kB（gzip **+3.47 kB**） |
| 主 CSS | 211.05 kB（gzip 38.89 kB） | 211.72 kB（gzip 39.03 kB） | +0.67 kB（gzip +0.14 kB） |
| KaTeX JS chunk | — | 259.16 kB（gzip 77.74 kB） | 懒加载，不进主 chunk |
| KaTeX CSS chunk | — | 27.26 kB（gzip 7.49 kB） | 懒加载 |
| KaTeX 字体 | — | 20 面 woff2，合计约 292 KB | 懒加载，长缓存 |

主 chunk 的 gzip 增量 3.39 kB 来自 remark-math/mdast/micromark 三个
小型解析扩展（静态注册，MarkdownText 保持同步渲染）；KaTeX 引擎、样式
与字体全部在首个数学节点出现时才拉取。
