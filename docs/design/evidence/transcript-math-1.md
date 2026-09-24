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

---

## Round 2（codex + grok review，2026-09-24）

安全基线（`trust:false`、`throwOnError:false`、不放宽 sanitizer、
lib/ 新文件在范围内、主 chunk ~3.5 KB gzip 增量）获接受；按下发的 8 条
修订全部处理：

1. **工作量有界。** 扫描器重写为线性：单趟标记代码/缩进块并配对
   `$$` run 与括号定界符，单 `$` 闭合搜索的游标只前进不回退。
   `"$1".repeat(50000)` 与 100 KB 公式在 devbox 上 best-of-5 仅数十毫秒
   （单测带 <250 ms 预算与 10× 线性比断言；旧的二次实现 ~2.5e9 步）。
   KaTeX 固定 `maxSize:20em`、`maxExpand:1000`；源码超过
   `MATH_MAX_SOURCE=4000` 字符直接跳过引擎（中性 skip 态，非 danger）；
   成功 HTML 按 `(source,display)` 记忆化，流式重渲染同一条只解析一次。
   `\rule{100000em}` 被钳到 20em、宏炸弹 `\def\a{\a}\a` 被
   `maxExpand` 截断成错误、100 KB 公式走 skip——三者均有单元与 e2e。
2. **markdown 结构保持。** 不再注入顶层空行搬运块：`$`/`$$` 的容器/缩进/
   空行结构完全交给 remark-math；`    $$x$$` 是缩进代码，
   `> $$x$$`、`- $$x$$` 的公式留在引用/列表项内；同一行 `$$x$$`
   改写成带容器前缀（列表续行用等宽空格而非重复 marker）的 display
   fence；`\(…\)`/`\[…\]` 仅在 prose 中改写。
3. **` ```math ` / `~~~math` 围栏是代码。** 真正的 mdast 数学节点用专用
   class `language-mathinline`/`language-mathdisplay`（自定义
   remark-rehype handler 标注，默认 sanitizer 的 `language-*` 放行），与
   围栏的 `language-math` 区分，走 CodeBlock 路径且不加载 KaTeX。
4. **流式未闭合显示数学按字面源渲染。** 悬空 `$$`/`\[` 的整段尾巴转义后
   原样显示，定界符、反斜杠、星号、大括号齐全且无 `<em>`；e2e 钉
   `intro \[a *b* + \{c\}` 的可见文本。
5. **`$$…$$` 内空行。** 该对开/闭两个 run 都作废（字面），但扫描在闭合
   run 之后继续，后续合法公式仍渲染；只有真正悬空、EOF 前无闭合的最后
   run 才吞尾。修正了此前锁错结果的单测。
6. **失败不粘性。** 发布 `error` 后清空去重槽，下一次 `loadMath()` 从
   `loading` 重新 import；`mathLoader.test.ts` 用 importer seam 钉死
   “失败一次→再挂载渲染成功”。
7. **字体。** 引擎就绪后还要等 KaTeX_Main/KaTeX_Math 主字面
   `document.fonts.load`（2 s 兜底）才撤占位，杜绝占位消失后的 FOIT
   不可见期。
8. **inline 行高。** inline 节点为 `max-height:1.4em; overflow:visible`
   的 inline-block：`$\dfrac{1}{\dfrac{1}{x}}$` 等超高公式可见地超出但
   不撑大行盒/段落（e2e 断言行盒 ≤1.4em）。

### Round-2 测试与体积

- 数学相关单元共 59 例（mathSegments 25、mathRender 9、mathLoader 2、
  MathBlock 9、MarkdownText math 14）；全量 `pnpm test` 全绿。
- hub e2e 在原 5 例外新增 1 个 round-2 加固用例（结构/缩进/math 围栏/
  字面尾巴/规则钳制/宏炸弹/100 KB skip/inline 几何），共 6 例。
- 主 chunk（最终）：1515.79 KB（gzip 449.35 KB），相对基线
  1501.63 KB（444.60 KB）为 +14.16 KB（gzip **+4.75 KB**）；
  KaTeX JS/CSS/字体仍全部懒加载，体积不变。
