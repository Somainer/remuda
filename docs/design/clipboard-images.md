# 剪贴板与图片直通（Clipboard & Image Passthrough）调研与设计

## 1 结论

- **今天完全不通**：从浏览器到 agent 没有任何图片路径。Composer 是纯 `<textarea>`，Hub↔Node 的 `instance.send` 在类型层就只有 `prompt: String`，所有 driver 都会把非 Text block 丢弃。上传端点（`object.prepare/write/commit`）只有类型和文档，没有一行 handler。
- **最短闭环只要三件事**：Hub 侧一个对象暂存端点、Node 侧回拉落盘、driver 侧按能力投递（Claude print 用 base64 block，PTY 类用绝对路径提及）。**不需要**改 Hub↔Node 的帧结构、不需要 channel 2、不需要实现完整 `object.*` RPC。
- **传输通道必须绕开三道上限**：`maxJsonFrameBytes` 1 MiB、prompt 64 KiB（`crates/remuda-node/src/runtime.rs` 的 `validate_text`）、TTY input 4096 B（`web/src/features/session/tty/client.ts:39`）。因此附件字节只走 HTTP，命令帧里只带 metadata。
- **agent 能力天然分层**，设计必须容纳降级：Claude print 支持 base64 image block（VERIFIED），Claude PTY 靠 Read tool 读路径（VERIFIED），Codex 只有启动期 `-i` 或 app-server `LocalImage{path}`（VERIFIED），Grok 没有任何远端图片入口（VERIFIED）。
- **浏览器侧 iOS 是主要风险面**：`paste` 事件是 Baseline 且无需权限，但 iOS 存在 `types` 声明 image 而 `files` 为空的情况，必须同时提供 file picker 和显式「粘贴图片」按钮兜底。
- **MVP 约 5 人日**（详见 §7）。唯一可能推翻通道选择的未决项：Node 是否持有可用的 Hub HTTP base URL + token（§8 第 1 条），**开工第一件事验证**。

## 2 现状（Remuda 今天的路径）

### 2.1 文本链路（唯一存在的链路）

| 环节 | 位置 | 事实 |
|---|---|---|
| 输入框 | `web/src/features/session/Composer.tsx:48,84-90` | `onSend: (text: string)`；纯 textarea，无 `onPaste`、无 drop、无 `<input type=file>` |
| 草稿 | `web/src/lib/drafts.ts` | 按 instance 存 localStorage |
| 剪贴板 | `web/src/lib/clipboard.ts:1-3` | 全站**只写不读**（`writeText`） |
| 发送 | `web/src/lib/store.ts:415-440` → `web/src/lib/api.ts:937-939` | `command(instanceId, "instance.send", { prompt })`，wire 字段是扁平 string |
| HTTP | `crates/remuda-hub/src/http.rs:404-461` | `post_command`：origin 校验、device/caller 解析、`agent_scope::authorize_command`、`queue_command`、`forward_if_online`。**无 size 校验、无 content-type 分支** |
| Node 解码 | `crates/remuda-node/src/transport/hubnode_codec.rs:381-386`、`transport/wss/runtime_wss.rs:301-313` | 两处都用 `InstanceSendParams::prompt_text()`（`crates/remuda-protocol/src/hubnode.rs:815`，VERIFIED）压成一个 `String` |
| Node runtime | `crates/remuda-node/src/runtime.rs:1008-1022`、`:1149-1156` | `DriverRequest::Send { prompt, origin }`，`validate_text` 上限 **64 KiB** |
| 重新包装 | `crates/remuda-node/src/native.rs:426,503-510` | 把 String 重新包成 `PromptInput{ blocks: [Text] }` |

即：**blocks 在类型系统里存在，但由 Node 从 string 合成；Hub↔Node 线上没有结构化 block。**

### 2.2 driver 侧一律丢弃非文本

- `crates/remuda-driver/src/claude_print.rs:504` 调用 `prompt_text(&prompt.blocks)`（定义在 `:1452`，VERIFIED），只保留 Text，否则报 "prompt has no text blocks"；最终发 `UserContent::Text(text)`，尽管 `crates/remuda-claude-wire/src/types.rs:188-193` 已有 `UserContent::Blocks(Vec<Value>)`。
- `claude_pty.rs:997,1009-1022`、`generic_pty.rs:634-645`、`shell_pty.rs:221-237` 同样的 text-only 过滤。
- 类型早已就位：`crates/remuda-protocol/src/observation.rs:143`（`MediaBlock{object_id, media_type, name}`）、`:185/188/191`（Image/Audio/File）、`:229`（`PromptInput.blocks`）—— 均 VERIFIED。
- 前端渲染同样丢弃：`web/src/lib/api.ts:1268-1278` `observationText()`。

### 2.3 上传与对象存储：不存在

- Hub 全部 30 条路由中**无 multipart、无 `/v1/objects`**；`grep DefaultBodyLimit crates/` 只命中 `crates/remuda-node/src/server.rs:106`（Node 自己的 1 MiB），**Hub 全局没有 body 上限**（VERIFIED）。
- `object.prepare/write/commit/stat/read` 仅有枚举与类型（`crates/remuda-protocol/src/rpc.rs:1247-1258`、`enums.rs:818-820`）和 `docs/design/protocol.md:1285-1291` 的描述，**handler 数为 0**（VERIFIED：在 hub/node 源码中 grep 无命中）。
- 存在的是 journal 内部内容寻址 blob：`crates/remuda-journal/src/blob.rs:12-80`，落在 `<data_dir>/blobs/<digest>`，铸 `Id::new("obj")`，按 digest 去重，返回 `RawRef`。**仅从 journal 追加原生输出可达；无读回 HTTP 路由，无任何删除/GC 路径**（VERIFIED）。
- 二进制 channel 2 `ObjectChunk` 已定义（`crates/remuda-protocol/src/binary.rs:19,175`，VERIFIED），但 Hub 只收发 channel 1/3（`ws.rs:823-827`、`:980`）。

### 2.4 终端粘贴

xterm.js 原生粘贴触发 `onData`（`TerminalView.tsx:183-189`）→ `tty/client.ts:267-272` 入队、8 ms 批处理 → `flushInput` 按 `MAX_TTY_INPUT = 4096`（`client.ts:39`，VERIFIED）切块，每块一帧 channel 3。客户端从不自发 `ESC[200~`，也不做任何过滤。Hub 侧 `crates/remuda-hub/src/ws.rs:980 handle_follow_input`（VERIFIED，注意不是 979）拒绝 >4096 的载荷，但**不做任何写权限检查、无 writer lease**，与 `docs/design/remote-terminal.md:29,91` 的设计不符。新路径不得复制此模式。

### 2.5 一张图片上传会继承的约束

鉴权：`require_origin` + device cookie（`http.rs:409-410`）；`crates/remuda-hub/src/agent_scope.rs:153-184` 限定 agent-origin 只能 `instance.send|cancel|close|tty.write|instance.keys`，且多种情况需 approval。限流目前只覆盖 auth（`rate_limit.rs:15-20`）。
尺寸：prompt 64 KiB、JSON 帧 1 MiB、`maxBinaryChunkBytes=65536`、`maxTtyInputBytes=4096`。`docs/design/protocol.md:753` 明确禁止把 base64 图片塞进消息。
落盘先例：`crates/remuda-node/src/native.rs:171-178` 的 `<data_dir>/instances/<ins_…>/launch/`，目录 0700、原子 tmp+rename（`crates/remuda-driver/src/profile.rs:563-600`）；`data_dir` 由 `REMUDA_DATA_DIR` / `XDG_DATA_HOME` 决定（`crates/remuda-node/src/enroll.rs:51-62`）。这些目录**同样没有清理逻辑**。

## 3 各 agent CLI 的图片输入能力矩阵

| CLI | 通道 | 结论 | 证据等级 |
|---|---|---|---|
| Claude print `--input-format stream-json` | `{"type":"image","source":{"type":"base64","media_type","data"}}` 内容块 | **可用** | VERIFIED（实跑 claude 2.1.270，64×64 红色 PNG，返回 "Red"）；官方 streaming-vs-single-mode 文档同形状，并写明 single-message 模式不支持图片附件（VERIFIED，WebFetch） |
| 同上，`source.type:"file"` / path | 无报错但模型答「图片无法处理」；bundle 内归一化逻辑把非 base64 source 降级为文本 | **不可用** | VERIFIED（实跑 + `strings`）；`url`/`file_id` source UNVERIFIED |
| Claude print 尺寸 | bundle 内含图片字节预算 512000 并在超出时重压 | 参考值 | VERIFIED（字符串）；API 侧上限 UNVERIFIED |
| Claude TUI Ctrl+V | keybinding `chat:imagePaste`，在**运行 claude 的机器**上 shell 出 `osascript`/`pbpaste`/`xclip`/`wl-paste`；有 SSH 分支提示 "You're SSH'd; try scp?" | **对 Remuda 无用** | VERIFIED（strings） |
| Claude TUI 拖拽 | 提示语被 `isRelevant: !isSSH()` 门控，仅本地终端 | 不可用 | VERIFIED |
| Claude 文本里提路径 | Read tool 接受 `png,jpg,jpeg,gif,webp`(+pdf)，结果以 `tool_result` 内 image block 返回 | **远端唯一可靠通路**（一次 tool round-trip） | VERIFIED（strings） |
| Codex `-i/--image <FILE>...` | `codex` 与 `codex exec` 都有；**仅初始 prompt**，无会话中途 flag | 部分可用 | VERIFIED（`--help`，0.154.0） |
| Codex JSON 协议 | `UserInput::LocalImage{path, detail}`（serde tag `local_image`/`localImage`），本仓库已建模于 `crates/remuda-codex-wire/src/types.rs:451`（VERIFIED） | **驱动 app-server 即可按远端路径给图** | VERIFIED |
| Codex TUI Ctrl+V | 同样是本地剪贴板 | 不可用 | VERIFIED（strings） |
| Codex 识别散文中的路径 | 未见证据 | 假设**不识别**，必须显式指令 | UNVERIFIED |
| Grok 1.0.30 | 全部 flag 中无任何 image 项；`-p/--single` 仅文本，无 `--input-format`；`grok wrap` 的 OSC 52 仅文本 | **无远端图片入口** | VERIFIED（`--help`） |
| Grok TUI 剪贴板 | 二进制含 `grok-clipboard-probe.*`、`pbpaste`、`<<<IMAGE>>>` 标记与 "Image input" 提示；读 `SSH_CONNECTION` | 本地剪贴板可用；远端需先写宿主 pasteboard | VERIFIED（strings） |
| agy 1.2.2 | 有 `--input-format stream-json`，无 image flag；二进制含 `image/png`/`input_image` 字符串 | 接受与否 **UNVERIFIED** | 部分 |
| MCP `get_attachment` | Claude 把 MCP `ImageContent` 转成 base64 image block（VERIFIED，bundle）；某条转换路径有 130000 b64 字符上限。Codex 解析 `ImageContent` 结构（VERIFIED 字符串），是否转发 UNVERIFIED；Grok UNVERIFIED | 可作 v2 的可移植兜底 | 混合 |

## 4 浏览器剪贴板能力与限制

1. **`paste` 事件（主路径，全平台可用）**：Baseline 自 2015，任意元素可监听，无权限、无用户手势问题（粘贴本身即手势）。图片通过 `clipboardData.items[i].getAsFile()`（过滤 `type.startsWith("image/")`）**以及** `clipboardData.files` 两路读取。WebKit 自 Safari 11.1 / iOS 11.3 起把 pasteboard 图片暴露为 file 并自动把 TIFF 转 PNG。
   **iOS 已知缺口**：部分来源 `types` 含 `image/*` 但 `files` 为空（社区报告，PARTLY VERIFIED）。因此必须同时遍历 `items` 与 `files`，两者皆空时引导用户走按钮或 file picker。
   **只在真的消费了图片时** `preventDefault()`，否则会破坏 iOS 的光标与纯文本粘贴。
2. **`navigator.clipboard.read()`（次路径，绑手势）**：Baseline 2024，仅 secure context，常见类型为 text/HTML/`image/png`。Chromium 需 `clipboard-read` 权限与 transient activation；WebKit 仅在同源写入时静默放行，否则弹平台「粘贴」确认，且期间点击页面他处会 reject。**只挂在显式「粘贴图片」按钮上**，绝不在 mount/focus 时调用。
3. **file input 兜底（手机主路径）**：`<input type="file" accept="image/*" multiple>`。`capture="user|environment"` 只是「请求」相机且非 Baseline。建议给两个入口：不带 `capture` 的选择器（Photos/Files/iCloud 全覆盖）+ 可选的相机入口。桌面再加 `dragover.preventDefault()` / `drop` 取 `dataTransfer.files`，约 15 行。
4. **Share Target（仅 Android）**：manifest 里 `share_target`（POST + multipart），由 service worker 的 `fetch` 拦截。MDN 标注 limited availability：Android Chrome 支持，**iOS Safari 不支持**。Remuda 当前没有 service worker，真正的成本在这里而不是 manifest。
5. **格式与尺寸**：iOS 常常已把 HEIC 转 JPEG，但 HEIC 仍会漏出，而非 Safari 引擎根本无法解码，所以转码必须在端上做：`createImageBitmap(file,{imageOrientation:"from-image"})` → `OffscreenCanvas` → `toBlob("image/jpeg",0.85)`。`createImageBitmap` 在 iOS Safari 能否解 HEIC 属 UNVERIFIED，需 feature-detect 并回退到 `<img>`+`createObjectURL`。canvas 重编码顺带剥掉 EXIF（含 GPS）与 ICC，是最省事的脱敏手段，因此**对所有图片（含 PNG 截图）无条件执行**。长边压到 ~1568 px、单图 ~5 MB 以内。
6. **CSP**：Hub 现有 `img-src 'self' data: blob:; worker-src 'self' blob:; form-action 'self'`（`crates/remuda-hub/src/web.rs:18`），blob 预览与 share-target POST 均已放行，**无需放宽**。`frame-ancestors 'none'` 使 `Permissions-Policy: clipboard-read` 无关紧要。
7. **复制选区出去**：`writeText` 必须在 tap/click handler 内**同步**调用，任何 `await` 都会丢掉 iOS 的 transient activation；需要延迟数据时用接受 Promise 的 `ClipboardItem`。保留 `document.execCommand("copy")` 兜底。`TerminalView.tsx:243` 已暴露 `term.hasSelection()`。

## 5 推荐设计

### 5.1 数据流（MVP）

```
paste / drop / file-picker
 → web: createImageBitmap({imageOrientation:"from-image"}) → OffscreenCanvas
        → 长边 ≤1568 → toBlob(jpeg|png)（剥 EXIF/GPS，顺带 HEIC 转码）→ sha256
 → POST /v1/objects   ← {objectId:"obj_…", mediaType, bytes, digest, name, expiresAt}
 → composer chip（blob: 本地预览）
 → POST /v1/instances/{id}/commands
     {operation:"instance.send",
      payload:{prompt:"看下这个报错",
               attachments:[{objectId, mediaType, name}]}}
 → Hub: require_origin + device cookie → queue_command / forward_if_online（原路径不变）
 → Node runtime: DriverRequest::Send{prompt, attachments, origin}
     → GET {hub}/v1/objects/{obj}
     → <data_dir>/instances/<ins_…>/attachments/<obj>.<ext>（文件 0600、目录 0700，不进 workspace）
     → 组装 PromptInput.blocks = [Image(MediaBlock), Text]，并记 object_id → local_path
 → driver 按能力投递
```

materialize 在 dispatch 前**同步**完成；失败（磁盘满 / 404 / 已过期）整条 send 失败并给出可读错误，**不静默降级为纯文本**（否则 agent 答非所问）。Node 离线时对象留在 Hub、命令排队，TTL 取 **24 h**（必须大于典型离线时长）。

### 5.2 API / 协议增量

- **`POST /v1/objects`**：raw body + `Content-Type`，header 带 `X-Remuda-Instance-Id`、`X-Remuda-Object-Purpose: input`。不引 multipart 依赖；按 digest 幂等去重。
- **`GET /v1/objects/{objectId}`**：供 Node 回拉，未来供 UI 回显。`DELETE` 同路径（v2）。
- Hub 挂 `DefaultBodyLimit(6 MiB)`——这是全仓库第一处全局 body 上限。
- 协议增量：`InstanceSendParams` 增 `attachments: Vec<AttachmentRef>{objectId, mediaType, name}`，`prompt: String` 与 `prompt_text()` 保持不变，新增 `attachments()`。这是纯增量，老 Node 忽略字段即降级为纯文本，`hubnode_codec.rs` 与 `runtime_wss.rs` 的压扁逻辑不必改。
- **刻意不做（留 v2）**：channel 2 `ObjectChunk`、`object.prepare/write/commit` handler、上行 `blocks: Vec<ContentBlock>`。

### 5.3 各 driver 投递

| driver | MVP | v2 |
|---|---|---|
| `claude_print` | `claude_print.rs:504` 的 `prompt_text()` 换成 `prompt_content()`，发 `UserContent::Blocks([{type:"image",source:{type:"base64",…}}, {type:"text",…}])`。**必须 base64**。>3.5 MiB 退化为路径提及 | 多图 token 上限提示 |
| `claude_pty` | 文本后追加 `附件: /abs/path.png`，靠 Read tool 拾取 | 叠加远端 pasteboard |
| `generic_pty`（codex） | 路径提及，且文本**必须显式**写「读取 /abs/path.png」 | 走 `codex app-server` 的 `UserInput::LocalImage{path}` |
| `generic_pty`（grok） | 路径提及 + UI 一次性徽标「该 agent 不支持读图」 | 远端 pasteboard 或 MCP `get_attachment(objectId)` |
| `shell_pty` | 只插入 shell-quote 后的路径，不发 block | — |

`claude_pty.rs:1009`、`generic_pty.rs:643`、`shell_pty.rs:221` 三处 text-only 过滤统一改为「Text 原样 + Media 转路径行」，抽成共享 helper。

v2 的 `crates/remuda-driver/src/clipboard.rs`：macOS `osascript` 写 `«class PNGf»`、X11 `xclip -selection clipboard -t image/png -i`、Wayland `wl-copy --type image/png`；探测结果进 host capability。**OSC 52 不可替代**：它只改终端模拟器的剪贴板且限纯文本，而 agent 读的是宿主 pasteboard。

### 5.4 安全

- 鉴权：`require_origin` + device cookie，仅 owner device 可上传；agent-origin 一律 403（MVP），v2 再把 `object.attach` 纳入 `agent_scope` 的 approval 类别。`GET /v1/objects/{id}` 双重绑定 account + 该对象绑定的 instance；Node 只能读它所托管 instance 的对象。
- 尺寸：单文件 ≤5 MiB、单条消息 ≤4 个附件、单 instance 暂存 ≤64 MiB，超限返回 `RESOURCE_LIMIT`（chip 保留可重试）。滑动窗口限流留 v2。
- MIME：allowlist `image/png|jpeg|webp|gif`（PDF 默认关闭）；**以服务端 magic bytes 嗅探为准**，与 `Content-Type` 不符即拒。**丢弃原始文件名**，落盘名固定 `<obj_id>.<ext-from-sniff>`，结构性消除路径穿越。
- 响应头：`Content-Disposition: attachment` + `X-Content-Type-Options: nosniff`。
- EXIF：浏览器侧无条件 canvas 重编码；Hub 不做二次解码（不引图片库＝不引 CVE 面）。
- 前端：`drafts.ts` 只存 `objectId`，字节绝不写 localStorage；send/discard 时 `revokeObjectURL`；剪贴板 `text/html` 丢弃、只取 `text/plain`。
- 不复制坏模式：`ws.rs:980` 缺写权限校验，新路由每次显式校验 caller；v2 做终端粘贴前必须先补上该处的写权限 + writer lease。
- 审计：`attachment.uploaded{objectId,digest,bytes,mediaType,instanceId,deviceId}` 与 `object.materialized{path}`。

### 5.5 清理

Hub 侧 24 h TTL，MVP 采**惰性删除**（读到 `expires_at < now` 即 404，上传时顺带扫同 instance 的过期项），不写 cron。Node 侧在 instance close 时递归删 `attachments/`，另加 7 天 sweeper——顺手把现存无人清理的 `instances/<id>/launch/` 一并纳入。journal blob 的引用计数与真 GC 留 v2。

## 6 手机 UX

- Composer 加 `onPaste`：同时遍历 `clipboardData.items[].getAsFile()` 和 `.files`；仅在真消费图片时 `preventDefault()`。
- 三个入口：📎 `<input type="file" accept="image/*" multiple>`（**不带 capture**）、📷 `capture="environment"`、以及「粘贴图片」按钮走 `navigator.clipboard.read()`（只在用户手势内）。第三个专治 iOS 的空 FileList 与键盘无粘贴按钮两种情况。
- 桌面：`dragover` + `drop` 取 `dataTransfer.files`。
- textarea 上方缩略图 chip：上传中转圈 / 失败标红可重试 / 长按删除；全部 commit 前发送按钮置灰。发送后的气泡下方用本地 metadata 展示 chip（服务端回读回显留 v2）。
- 失败引导：iOS FileList 为空 → 提示用粘贴按钮；HEIC 解码失败 → 原字节上传被 Hub 拒 → 文案「请用相册导出 JPEG 后再试」。
- v2：Android `share_target`（manifest POST multipart + 新 service worker 拦 `/share` → 暂存 IndexedDB → 跳 `/?share=1` → instance 选择器）；iOS 无此能力，引导「拷贝 → 粘贴」。

## 7 分阶段实施与工作量

**MVP ≤5 人日**（明确砍掉：GC 定时任务、DELETE 路由、缩略图参数、服务端回显、远端 pasteboard、终端粘贴、share target）

| 项 | 人日 |
|---|---|
| Hub `POST /v1/objects` + `GET /v1/objects/{id}` + 元数据（owner_account / instance_id / purpose 硬编码 `input` / expires_at）+ 硬上限 + 审计 | 1.5 |
| protocol `attachments` 增量 + 两处 codec + `runtime.rs` materialize / 落盘 / close 清理 | 1.5 |
| `claude_print` 的 `prompt_content()` + 三个 PTY 路径 helper | 1.0 |
| Web composer（paste / drop / picker / camera / 重编码 / chip） | 1.0 |
| 真机冒烟（iOS Safari、Android Chrome） | 0.5 |

**v2 ≈12 人日，可并行切片**：① channel 2 `ObjectChunk` + `object.prepare/write/commit` handler（解决 Node 无 Hub HTTP 出口 / LAN 直传）2.5d；② 上行 `blocks: Vec<ContentBlock>` + journal 存 user Image block + `observationText()` 返 blocks + MessageList 缩略图 + 回显 3d；③ blob 引用计数 + 真 GC + 滑动窗口限流 1.5d；④ `clipboard.rs` 远端 pasteboard + capability 徽标 1.5d；⑤ 终端粘贴（按 `bracketedPasteMode` 包 `ESC[200~…ESC[201~`、按 4096 边界切块）/ OSC 52 捕获（`registerOscHandler(52)`，≤8 KiB + 可见 toast）+ `ws.rs:980` lease 修复 2d；⑥ share_target service worker 1.5d。Codex app-server `LocalImage`、`max_blocks` / `max_attach_bytes` 配置、agent approval 类别按需插入。

## 8 未决问题

1. ~~**（阻塞 MVP）Node 是否持有可用的 Hub HTTP base URL + token**~~ —— **已验证解决（2026-09-13，x-clip）。结论：持有，走 §5.1 的 HTTP 回拉主路径，不需要 channel 2 fallback。** 见下 §8.1 验证记录与 [decisions.md](./decisions.md) D-027。
2. Claude CLI 把 `source.type:"file"` 降级为文本——称已实测，未在本仓库留下证据。
3. CLI 单图 512 KB 内部重压阈值的确切语义与 API 侧真实上限。
4. Claude Read tool 对 png/jpg/gif/webp 转 image block——称已验证，未复核。
5. Codex PTY 是否识别散文中的路径（UNVERIFIED，MVP 按不识别处理）。
6. Grok TUI 探测 `pbpaste`/xclip 并主动提示粘贴的行为（单方面主张）。
7. agy 是否接受 image block（UNVERIFIED）。
8. PTY agent 若跑在容器或远端 worktree 中，`<data_dir>/instances/…` 是否在其可见文件系统内——可能需要「materialize 到 workspace `.remuda/attachments/` + .gitignore」开关。
9. `purpose: input|settings|answer` 仅在 protocol.md 有描述、无代码语义；MVP 硬编码 `input`。
10. 对象绑定单 instance 还是账号内可复用（跨会话转发）；远端 pasteboard 覆盖 Node 宿主用户剪贴板是否默认开启；图片是否永久进 journal（与 GC/配额冲突）——三项需在 v2 前定。
11. 本文引用的 `docs/design/protocol.md` 行号与部分 driver 行号未逐条复核；已复核项见 §2 表格中标 VERIFIED 者。

### 8.1 验证记录（2026-09-13，x-clip，开工第一件事）

**问题**：WSS-only 部署下 Node 是否持有可用的 Hub HTTP base URL + token，足以 `GET /v1/objects/{id}` 回拉附件字节。

**结论：持有。MVP 按 §5.1 原样实施 HTTP 回拉，不改用 channel 2 `ObjectChunk`。**

证据（均为本仓库源码复核，VERIFIED）：

| 事实 | 位置 |
|---|---|
| `WssConfig.url` 就是 Hub 的 `ws(s)://<authority>/v1/node`，`WssConfig.token` 是持久 host token | `crates/remuda-node/src/transport/wss.rs:52-56` |
| 生产 Node 从 `config.node.hub_url` 与 `<data_dir>/node/host-token` 填这两个字段；daemon 同理（崩溃后优先用持久 host token，不用已消费的 enroll token） | `crates/remuda/src/cmd/node.rs:188-198`、`crates/remuda/src/cmd/node/daemon.rs:103-130` |
| `remuda dev` 的进程内 Node 走 `WssConfig::loopback(hub_addr, node_token, host_id)`，同样两者齐备 | `crates/remuda/src/cmd/dev.rs:165` |
| host token 经 `node.hello` 由 Hub 回发并落盘 `0600` | `crates/remuda-node/src/enroll.rs:107-133`、`transport/wss.rs`（`nodeToken`） |
| **Hub 已经用同一个 token 认证 WS 握手**：`presented_token(headers)` 读 `Authorization: Bearer` → `Store::authenticate_host` 按 `token_prefix` 索引 + Argon2 校验 | `crates/remuda-hub/src/ws.rs:161`、`crates/remuda-hub/src/store.rs:616-660` |
| ws→http 的 scheme 换算已有现成先例 | `crates/remuda-node/src/carrier.rs:73-84` |

因此缺的不是凭据也不是地址，只是两段管道：

1. Hub 侧一个 HTTP 版的 `require_host` 提取器——复用 `presented_token` + `Store::authenticate_host` 的同一条查询，不新增凭据种类、不新增信任边界；
2. Node 侧一个 HTTP 客户端（`reqwest`，已在 workspace lock 内，`remuda-hub-client` / `remuda-feishu` 已依赖）。

两项合计远小于 channel 2 的 +2~3 天，§7 的 5 人日预算不变。

**代价与边界**（记录以便 v2 复查）：

- Node 必须能对 Hub 发起**出站 HTTPS**，而不只是出站 WSS。D-020 的部署形态（Node 主动出站连公网/内网 Hub）本就要求出站 HTTPS，所以这不是新增网络要求；但若将来出现「只开 WSS 端口、HTTP 被策略拦截」的部署，回退路径仍然是 v2 的 channel 2 `ObjectChunk`（§7 v2 ①），届时 Node 侧 materialize 接口保持不变，只换取字节的来源。
- `GET /v1/objects/{id}` 对 host 凭据放行，等于让 host token 多了一个读权限。因此该路由对 host 调用方做双重绑定：对象必须绑定在某个 instance 上，且该 instance 的 `host_id` 必须等于认证出来的 host——一台 Node 读不到别台 Node 的附件。

