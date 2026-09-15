# 评论：把代码块锚定进 prompt（workbench-code-2）

工作树 `wt/ux-comment/codeblock-quote-anchor`，2026-09-15。
接续 ux-code（代码块工具栏的 换行 / 复制）与 ux-imgref（`[Image #n]` 图片
锚点）。参考的飞书文档风格代码块有三个图标：换行、复制、**评论**。本轮交付
第三个：**评论 = 就这段代码回复 agent**。

对标 `[Image #n]` 的同构行为：按下后在 composer 草稿光标处插入
`[Code #n]` 锚点 token，模型能确切知道 owner 在说哪个代码块的哪几行。
纯文本，无上传、无 Hub/Rust 协议改动（fake harness 只多了一个会返回代码块的
触发语，见 §4）。

复核命令：

```
pnpm --dir web vitest run src/lib/codeAnchors.test.ts       # 锚点/选区/展开 21 项
pnpm --dir web vitest run src/lib/imageAnchors.test.ts      # 共享 token 机制
pnpm --dir web vitest run src/components/CodeBlock.test.tsx # 按钮显隐与引用载荷
pnpm --dir web test                                          # 全量 809 项
./node_modules/.bin/playwright test -c playwright.hub.config.ts tests/e2e/ux-comment.hub.spec.ts
```

## 1 交互契约

1. **第三个工具栏按钮**：代码块悬浮工具栏在 换行、复制 之后多一个评论图标
   （lucide `MessageSquareText`，15px，同 30px/触摸 44px 命中区、同 tooltip
   与 aria 规范：`title="评论"`，
   `aria-label="评论：把这段代码引用到输入框"`）。
2. **只在会话 transcript 里出现**：按钮由「是否有已挂载的 session composer」
   决定（`codeQuoteBus` + `useSyncExternalStore`）。docs、预览、mock 页面、
   MarkdownText 单测里没有 composer，按钮不渲染。Transcript.tsx /
   assemble.ts（批次 C 所有）与 SessionPage **零改动**。
3. **按下**：在 composer 草稿光标处插入 `[Code #n]`（词内自动补空格，复用
   imageAnchors 的插入实现），焦点移到 composer；触摸视口下 composer
   `scrollIntoView({ block: "center" })` 滚动到可视区。
4. **编号**：`n` = 该 draft 里代码引用的第 n 个，与图片编号空间**相互独立**
   （同一条 draft 里可以同时有 `[Image #1]` 与 `[Code #1]`）。
5. **选区→行**：按下时如果查看者在该块 `<pre>` 内有非折叠文本选区，只引用
   选中的行（按整行扩张，选区落在行中间也算整行，选区结束在换行上不吞下一
   空行）；否则引用整块。选区跨高亮产生的多个 text node 时用 Range 度量
   字符偏移，再换算成 1-based `lineFrom/lineTo`。
6. **chip**：composer 上方出现引用 chip：编号角标、`</>` 字形、标题（path
   优先，否则 `<lang> block`，再否则 `code block`）、一行正文预览；删除 chip
   与删除 token 双向镜像（删 chip → 去掉 token 并重编号；改正文删掉 token →
   chip 变虚线 + 「未引用（仍会发送）」，与图片 chip 完全一致）。
7. **不做键盘快捷键**（本期明确跳过）。

## 2 锚点实现（复用，不发明第二套语法）

`web/src/lib/imageAnchors.ts` 泛化为 `AnchorKind = "Image" | "Code"`：
`anchorTokenFor / anchorRegex / findAnchorsFor / referencedIndicesFor /
insertAnchorFor / insertAnchorsFor / renumberAnchorsFor / removeAndRenumberFor`，
旧的无后缀函数全部保留为 `"Image"` 特化，ux-imgref 的 23 项测试原样通过。
新增 `findAllAnchors` 给统一渲染器切片。

新文件：

- `web/src/lib/codeAnchors.ts`（纯函数）：
  - `parseFenceInfo`：`ts src/app.ts` → `{lang:"ts", path:"src/app.ts"}`，
    纯路径（`scripts/run.sh`）→ 无 lang + path；
  - `lineRangeFromOffsets`：字符偏移→整行（单测覆盖折叠选区、跨多行、反向
    选区、结尾换行、空块）；
  - `CodeQuote` 载荷 `{kind, turnOrdinal, blockOrdinal, lang, path?, text,
    lineFrom, lineTo}`。身份在点击时**从 DOM 推导**（该 section 在
    `[data-testid=message]` 列表里的 1-based 位次 = turnOrdinal；section 内
    `[data-testid=code-block]` 位次 = blockOrdinal），所以不需要批次 C 的
    node 模型给 props；
  - `quoteExpansion` + `expandCodeQuotes`：发送时展开（§3）；
  - `quotePreview`：chip 一行预览。
- `web/src/lib/codeQuoteBus.ts`：模块级单监听总线
  （`listenForCodeQuotes / quoteCode / subscribeQuoteTarget`），composer
  挂载即注册，CodeBlock 无需 props 穿透 MarkdownText。
- `web/src/features/session/useCodeQuotes.ts`：镜像 useAttachments 的同步
  listRef/编号契约；无上传。
- `web/src/components/CodeBlock.tsx`：按钮 + 选区度量（DOM 只读，不碰
  MarkdownText）。
- chip 复用 `AttachmentChips.module.css`（新字形类），内联 token 复用
  `AnchorText`（扩展支持 `[Code #n]` 的 `inline-code-anchor`）+
  `AnchorText.module.css`。没有新增全局 CSS，未碰 ui.module.css。

## 3 发送展开（harness 读到的文本）

prompt 草稿里的 token **逐字保留**，发送瞬间在正文**之前**展开引用块。
多块按 token 在草稿中首次出现排序（与图片 manifest 的 token 排序同一规则）；
token 被删掉的引用仍发送，排在被引用项之后。

展开格式（编号、位置、围栏）：

````
[Code #1] quoted from the assistant's message (lines 3-4 of src/app.ts):
```ts
const x = f();
return x;
```

[Code #2] quoted from the assistant's message (lines 1-1 of python block):
```python
print("hi")
```

[Code #2] after [Code #1]?   ← owner 的草稿正文逐字
````

无 lang/path 时位置写 `code block`；path 只出现在头行，围栏 tag 始终是
语言（保证 markdown 高亮合法）。结构化 transcript 里，草稿气泡仍通过
AnchorText 渲染，token 显示为内联 `</> [Code #n]` chip，带 "quoted" 语义；
回声的展开块本身是普通 assistant 围栏，照常走 CodeBlock 渲染。

## 4 端到端证据（假 node，无真模型）

`web/tests/e2e/ux-comment.hub.spec.ts`（2 例，`.hub.spec.ts` 后缀自动匹配，
未改 playwright.hub.config.ts；fake-node maxInstances 8 按既有约定
beforeAll 抬到 24、afterAll 恢复 8）：

1. fake harness 对包含 `show me code` 的 prompt（create 或 send 都会触发，
   example 里的 `code_comment_reply` 共享于两个分支）回复一条带
   ```` ```ts src/math.ts ```` 围栏的 assistant 消息；
2. **评论 → chip → 发送 → 展开到达 harness**：断言块上有第三个按钮、点击后
   chip 角标为 1、标题 `src/math.ts`、textarea 出现 `[Code #1]`；删 chip
   token 消失；再次引用后发送，fake node 回声（user 消息逐字 journal）包含
   `[Code #1] quoted from the assistant's message (lines 1-3 of
   src/math.ts):`、围栏原文与正文问题；
3. **未引用仍发送**：删掉 token → chip `data-unreferenced=1` +
   「未引用（仍会发送）」→ 发送后回声仍含展开块。

截图：`REMUDA_EVIDENCE=1` 时写
`docs/design/evidence/{workbench-code-2-1440-night.png,
workbench-code-2-390-night.png}`（本环境未开启；常规产物落在
`web/test-results/evidence`，不进树）。

## 5 未做 / 留后续

- 代码块内键盘快捷键（明确跳过）。
- 评论历史的富文本锚定（鼠标点具体行号跳转）——目前是行号区间 + 围栏。
- journaled 回声消息的代码 chip 内联：和图片锚点同一限制，Hub 不回声附件/
引用结构，远端回放仍是展开后的纯文本（模型侧完整，人侧靠 user 消息全文）。
- 跨消息同块多次评论的去重：允许（每次是独立引用，可不同行区间）。

## 6 改动清单

| 文件 | 改动 |
|---|---|
| `web/src/lib/imageAnchors.ts` | AnchorKind 泛型 + Code 一族纯函数（Image 行为不变） |
| `web/src/lib/codeAnchors.ts`（新）+ `.test.ts` | fence 解析、选区行映射、载荷、展开文本、预览 |
| `web/src/lib/codeQuoteBus.ts`（新） | transcript↔composer 单监听总线 |
| `web/src/features/session/useCodeQuotes.ts`（新） | draft 代码引用 store（镜像 useAttachments 契约） |
| `web/src/components/CodeBlock.tsx` + `.test.tsx`（新） | 评论按钮、会话内显隐、选区→行、引用载荷 |
| `web/src/components/codeBlock.module.css` | 无改动（复用按钮样式） |
| `web/src/features/session/AttachmentChips.tsx` + `.module.css` | CodeQuoteChips + 字形样式 |
| `web/src/features/session/AnchorText.tsx` + `.module.css` | 内联 `[Code #n]` chip |
| `web/src/features/session/Composer.tsx` | 仅插入/focus/remove/展开钩子（与图片同路径） |
| `web/tests/e2e/ux-comment.hub.spec.ts`（新） | 端到端 2 例 |
| `crates/remuda-hub/examples/hub_e2e.rs` | `show me code` 触发返回围栏块（fake harness） |
