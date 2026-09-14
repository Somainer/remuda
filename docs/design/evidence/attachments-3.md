# 图片锚点：prompt 里的 `[Image #n]`（2026-09-15）

工作树 `wt/ux-imgref/image-anchors`（从 `origin/main` `9df9099`），2026-09-15。
本轮是 **实现 + 自动化证据**：文字插入/重编号/删除是纯函数单测，投递链路是
Rust 单测与 hub-backed Playwright，全部可重跑，没有真机 claude / codex / grok
会话（三家实机表现见 §6 的诚实标注）。

用户原话（owner nit）：

> 图片我希望在 prompt 里面有锚点，类似于 claude 做的，有 [Image #x] 这种
> placeholder，能让模型知道我放多张图片时知道是哪一张。

对标行为是 Claude Code 自己的：每张粘贴的图片在光标处插入一个带编号的
placeholder token，模型按同一顺序收到图片，可以用编号引用。

复核命令：

```
pnpm --dir web vitest run src/lib/imageAnchors.test.ts          # 锚点纯函数 23 项
pnpm --dir web vitest run src/lib/attachments.test.ts           # manifest 排序
pnpm --dir web vitest run src/features/session/ComposerAttachments.test.tsx
pnpm --dir web vitest run src/features/session/AnchorText.test.tsx
cargo test -p remuda-driver --lib attachment                    # 编号提及行
cargo test -p remuda-hub --lib attachment                       # anchor 排序
cargo test -p remuda --locked --test mcp_schema_golden          # 工具契约 golden
cargo test -p remuda --locked --test mcp_attachment             # list 带 index
pnpm --dir web run test:e2e:hub ux-imgref                       # 端到端（假 node）
```

---

## 1 交互契约（composer）

1. **粘贴 / 拖入 / 选择（含拍照）图片**：每张图片在 textarea **光标处**插入
   `[Image #n]`，`n` 是该图片在本 draft 附件中的 1-based 位次。光标落在 token
   之后，可以接着打字。
2. **词内插入自动补空格**：光标两侧紧贴非空白字符时各补一个空格
   （`hel|lo` → `hel [Image #1] lo`）；本来就在空格处或行首行尾不重复补。
3. **多图一次粘贴**：按文件顺序连续插入，`[Image #1] [Image #2]`。
4. **chip 与 token 同号**：每个待发 chip 缩略图左上角有数字角标，等于 token 编号。
5. **删 chip**：从正文移除它的 token（出现几次删几次），其余 token 与 chip
   **整体重编号**（删掉 #1，原 #2 变 #1）。删除时吃掉自己那一侧的一个相邻空格，
   词与词之间删干净后保留一个空格。
6. **改正文删掉 token 不卸载图片**：chip 变成虚线框并标注
   「未引用（仍会发送）」——图片仍然随消息发出，manifest 里仍在。
7. **不做拖拽重排**（本期明确不做）。
8. **IME 安全**：所有插入只走 React 受控值 + `setSelectionRange`，
   `compositionstart/compositionend` 期间 Enter 本来就被 `composing()` 护栏拦掉，
   图片粘贴是独立的 paste 事件、不触碰组字缓冲区。纯函数按 JS code unit 处理，
   CJK 字符不会被劈开。

纯函数在 `web/src/lib/imageAnchors.ts`（parse / insert / renumber / remove，
零 DOM 依赖），23 个单测覆盖：往返、光标位置、词内补空格、`#1` 不被 `#12`
误匹配（正则 `\d+` 贪婪）、重复 token、token 删除后的空格折叠、CJK。

## 2 投递契约（manifest）

prompt 正文**逐字发送**，token 就在文本里，不剥离不重写。附件 manifest
（`instance.send` 的 `attachments[]`）每个元素新增纯增量字段：

| 字段 | 含义 |
|---|---|
| `index` | 1-based 锚点编号，等于 chip 位次与 `[Image #n]` |
| `objectId` | D-027 的 `obj_…` |
| `mediaType` | Hub magic-byte 嗅探结果 |
| `name` / `size` | 不变 |

数组顺序：**按 token 首次出现排序**（`refsOf(attachments, tokenText)`）；
token 被删掉的「未引用」附件仍发送，排在被引用项之后、保持自己的 `index`。
老客户端不带 `index` 时 Hub 用数组位次（1-based）兜底。

链路（全部 additive）：

1. **协议** `remuda-protocol::hubnode::AttachmentRef.index: Option<u32>`
   （`#[serde(default)]`，老 Node 忽略即降级）；
   `MediaBlock.anchor: Option<u32>` 把编号带到 driver 的内容块。
2. **Hub** `validate_send_attachments` 解析并**回写自己的权威 metadata**
   （mediaType/name/size 照旧来自对象行），同时把 `(objectId, index)` 落到
   objects 表新列 `anchor INTEGER`（`ensure_column` 增量迁移，老库自动加列）。
3. **MCP** `remuda_attachments_list` 的每个 item 现在带
   `index`，列表按 `anchor` 排序——agent 看到的顺序就是 prompt 里的 token 顺序，
   `[Image #2]` 可以直接解析成「列表里 index=2 的 objectId」再调
   `remuda_attachment`。没有 anchor 行（只暂存未被编号 send 消费）由 MCP 进程
   顺延补号。
4. **Node** materializer 保留 `index` 到 `MaterializedAttachment`，
   `prompt_input` 写进每张图片 image block 的 `anchor`。

Hub 侧排序单测（`remuda-hub` `attachment::tests`）：两张图先传 a 后传 b，
send 的 token 顺序是 b=#1、a=#2，`live_objects` 必须返回 `[b#1, a#2]`，
**manifest 的 token 顺序压过上传顺序**。

## 3 driver / harness 分层（诚实标注）

prompt 文本已经逐字带着 token，任何 harness 都「看得到」编号；字节投递沿用
attachments-2 的分层，本轮只把编号补到各层：

| harness | 首轮拿图 | 编号怎么到模型 | 实机验证 |
|---|---|---|---|
| **claude（claude-print）** | base64 image content block，图片在前、文本在后（现有行为） | 文本块逐字带 `[Image #n]`；图片块顺序 = manifest 顺序 = token 顺序 | **未实测**（print 退役条件仍挂；本轮无真机） |
| **claude / codex / grok（PTY 族）** | 追加绝对路径提及 | 有编号时每行改为 `附件 #2: obj_… (image/jpeg) /data/…/obj_….jpg`：同一行同时给出 **MCP 解析指引**（objectId）与**本地路径兜底**，行序按 `#1, #2, …`；并加一行说明「按正文 [Image #n] 编号，可用 remuda_attachments_list 解析」 | **未实测**（attachments-2 §7 的未验证项原样保留） |
| **MCP 工具（三家通用）** | `remuda_attachment` 返回 image content block | `remuda_attachments_list` 每项 `{index, objectId, mediaType, size, expiresAt}`，工具描述明确写「to answer about `[Image #2]`, call remuda_attachment with the objectId listed at index 2」 | 假 Hub + 真 `remuda mcp` 子进程集成测试通过 |
| **grok** | 无读图能力 | 仍发路径行 + 既有 journal note；编号至少让 note/路径与 token 对得上 | **未实测**，且不改变「grok 看不到图」的事实 |

路径提及的新格式只在至少一张图带 anchor 时启用，不带 anchor 的旧调用方逐字
保持 `请读取下面这个本地图片文件（附件）： / 附件: <path>`（单测钉死）。

工具 schema 是冻结契约，golden
`crates/remuda/tests/mcp_golden/attachment-tools.json` 已随描述更新，
`mcp_schema_golden` 钉死。

## 4 渲染

- 新组件 `web/src/features/session/AnchorText.tsx`（+ 独立 CSS module
  `AnchorText.module.css`，未碰 `styles/ui.module.css`）：把正文切片，
  token 渲染成内联缩略图小 chip，链接到附件预览（`blob:`），chip 上带
  `[Image #n]` 文案；找不到附件的 token 渲染成淡化纯文本 token，**绝不吞字**。
- **本地乐观气泡已接入**（`Transcript.tsx` 仅一处分支：`node.local` 走
  AnchorText，journaled user 消息仍走旧 `<p>`，并留
  `TODO(batch E, r-ux-imgref)`——等 Hub 把附件回声上 journal 后由批次 E
  在同一组件上收口）。
- 已发送气泡的缩略图角标也带编号（`SentAttachments` 的 `data-index`）。
- store 改动只有锚点映射一行：send 时把 manifest 的 `index` 配到本地预览上。

## 5 端到端证据（假 node，无真模型）

`web/tests/e2e/ux-imgref.hub.spec.ts`（hub config 的 `testMatch` 已按协调规则
以数组第二项加入 `/\.hub\.spec\.ts$/`，未改既有正则）：

1. **两图 → token 1/2 → 删第一张 → 重编号 → 发送**：
   paste 两个真实 PNG（1×1 红 + canvas 生成的 2×2 蓝，保证 re-encode 后字节
   不同、绕过 Hub 的 digest 去重），断言 textarea 值为 `[Image #1] [Image #2]`、
   两个 chip 角标 1/2、两次 `/v1/objects` 200；点第一个 chip 的 × 后剩一个
   chip、正文变 `[Image #1]`、角标 1。
2. **假 harness 收到有序 manifest**：假 node（`hub_e2e` example）现在把每条
   manifest 追加成 `[attachment-refs: #<index> <objectId> <mediaType>]` 回声行，
   e2e 断言只出现一条 `#1 obj_ … image/png`、没有 `#2`，且回声正文逐字是
   `echo: [Image #1] [attachments: image/png]`。
3. **未引用仍发送**：删掉正文 token → chip 标 `未引用（仍会发送）` →
   发送后回声 `[attachments: image/png,image/png]`，sent 缩略图仍为两张。

假 node 陷阱按既有约定处理（见 `hub-e2e-fake-node-traps` 记忆）：
`beforeAll` 用 `PATCH /v1/hosts/{id} {"maxInstances": 24}` 抬高上限
（串行套件到本 spec 时通常已占 7/8 槽，且 force-DELETE 要先 close、对假 node
每条要数秒，来不及在创建超时内回收；与 ux-quickfind 同一解法），
`afterAll` 恢复 8；每个测试后并发 `DELETE /v1/instances/{id}?force=1`
（`force=1` 是 u8，`force=true` 会 400）；创建后走 API 回答那个会锁住
composer 的 approval。

## 6 未验证 / 留给后续

1. **三家 harness 实机**：attachments-2 §7 的「MCP 附件三家验证」仍未满足；
   本轮只改了字符串与顺序，没有真 claude / codex / grok 会话。print 退役
   parity gate 不因此推进。
2. **journal 回声附件**：Hub 不把 attachments 回声到 journal，所以远端回放的
   user 消息还没有内联缩略图（本地气泡有）；批次 E 收口点已留 TODO。
3. **拖拽重排、跨 draft 撤销/重做 token**：本期不做。
4. 一个对象被多次 send 时 `anchor` 列以最近一次 manifest 为准；同一 instance
   的连续多轮会话各自按当轮 token 编号，模型侧以当轮 prompt + list 解析
   （list 是「当前会话暂存集」，语义与 attachments-2 一致）。

## 7 改动清单

| 文件 | 改动 |
|---|---|
| `web/src/lib/imageAnchors.ts`（新）+ `.test.ts` | token parse/insert/renumber/remove 纯函数，23 测 |
| `web/src/lib/attachments.ts` + 测试 | `AttachmentRef.index`；`refsOf(chips, text)` 按 token 排序 |
| `web/src/features/session/useAttachments.ts` | add 返回锚点编号；remove 返回被删编号；同步 listRef |
| `web/src/features/session/Composer.tsx` | paste/drop/pick 光标处插 token；删 chip 重编号；未引用集合（只碰附件/粘贴路径） |
| `web/src/features/session/AttachmentChips.tsx` + `.module.css` | 数字角标、未引用态、sent 角标 |
| `web/src/features/session/AnchorText.tsx`（新）+ CSS + 测试 | 内联缩略图 chip 渲染 |
| `web/src/features/session/Transcript.tsx` | 本地气泡接入 AnchorText + TODO(batch E) |
| `web/src/lib/store.ts` | send 时 index→预览映射（唯一一处附件相关改动） |
| `web/tests/e2e/ux-imgref.hub.spec.ts`（新） | 端到端两条用例 |
| `web/playwright.hub.config.ts` | testMatch 数组第二项 `/\.hub\.spec\.ts$/` |
| `crates/remuda-protocol/src/hubnode.rs` | `AttachmentRef.index` |
| `crates/remuda-protocol/src/observation.rs` | `MediaBlock.anchor`；schema/TS 重生成 |
| `crates/remuda-node/src/attachments.rs` | `MaterializedAttachment.index` 透传 |
| `crates/remuda-node/src/native.rs` | image block 带 anchor + 测试断言 |
| `crates/remuda-hub/src/objects.rs` | 校验 index、回写、落 anchor |
| `crates/remuda-hub/src/store.rs` | objects.anchor 列/迁移/`tag_object_anchors` |
| `crates/remuda-hub/src/attachments.rs` | list 按 anchor 排序 + 输出 index + 排序单测 |
| `crates/remuda-hub/openapi/openapi.json` + `web/src/lib/api.generated.ts` | AttachmentRef `index`（gen:api 重生成） |
| `crates/remuda/src/cmd/mcp/attachment.rs` | list 带 index/补号；工具描述；测试 |
| `crates/remuda/src/cmd/test_hub.rs` | 假 Hub fixture 带 index |
| `crates/remuda/tests/mcp_golden/attachment-tools.json` | 契约 golden 随描述更新 |
| `crates/remuda/tests/mcp_attachment.rs` | list index 断言 |
| `crates/remuda-driver/src/attachment.rs` | 编号提及行格式 + 单测 |
| `crates/remuda-driver/src/{claude_pty,claude_print}.rs` | 测试 fixture 补字段 |
| `crates/remuda-hub/examples/hub_e2e.rs` | 假 node 回声 `[attachment-refs: #n …]` |
