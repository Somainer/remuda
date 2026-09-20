# 手机优先 UI · 任务 6：`m-keybar` — 终端段九键键盘条

2026-09-20 · `wt/c-mkeybar/b-mkeybar-md` · mobile-ui 实施计划 §(C) 任务 6（B.3.2 九键表）
规格权威：[ui-spec.md](../ui-spec.md) §1.3 / §3.4（D-039）/ §4.7（D-049），[decisions.md](../decisions.md) D-028a / D-039 / D-049；基线 `origin/main` @ 5da06fd9。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/features/session/tty/AuxKeys.tsx` | 新增 `variant="phone"`：收起态九枚原始字节键（Esc/Tab/Ctrl/方向/PgUp/PgDn），展开态恢复完整 11 键 BAR（含 alt / ⌃C）。`stickyCtrl`/`onStickyCtrlChange` 让九键动作条与第二行共享同一个粘滞 Ctrl。BAR / STRIP 与 toolbar/bar 两个变体逐字节不变 |
| `web/src/features/session/tty/PhoneKeyBar.tsx`（新） | 九键动作条组件：Ctrl · Esc · Tab · git · 跳转 · 贴 · 史 · 结构 · 键，全部接到既有能力 |
| `web/src/features/session/tty/promptHistory.ts`（新） | 史键数据：经现有 `assembleTranscript` 投影取本实例 `origin==="human"` 的 user 消息（最新在前），外加未发送的 `lib/drafts.ts` 草稿；skill 注入体 / hook-context 由投影的 origin 过滤天然排除 |
| `web/src/features/session/tty/ttyScrollMemory.ts`（新） | git 键往返时跨 xterm 卸载/重挂记住终端缓冲行（baseY）的小存储 |
| `web/src/features/session/tty/TerminalView.tsx` | 仅移动底部区：移动分支由旧的整行 `AuxKeys` 换成 `PhoneKeyBar`；dock 的 LocalInput 加 `initialText` 注入（史键填入，靠 key 重挂）；ready 后按 `ttyScrollMemory` 用 xterm 滚动 API 恢复行；`__ttyLab` 增加 `scrollLine` / `scrollToLine`（测试/恢复用）；desktop 路径不动 |
| `web/src/features/session/tty/LocalInput.tsx` | 新增可选 `initialText`（本挂载的初始内容），提交路径与 D-028 body/Enter 分离完全不变 |
| `web/src/lib/clipboard.ts` | 新增 `probeClipboardRead()` / `readClipboard()` / `clipboardIo.read`：同步事实（无 API / 非安全上下文）+ Permissions API denied 探测；Safari 不支持 query 时乐观、读取时拒绝再兜底 |
| `web/src/features/session/tty/TerminalView.module.css` | 九键条样式：单行 44px、横向滚动、8px 托盘内边距（按键不贴视口边）、隐藏滚动条、圆角视觉 + 方形 `::after` 命中面、禁用原因行（`--text-aux`）、历史 sheet |
| `web/src/features/session/tty/phoneKeyBar.test.tsx`（新） | 11 个 vitest：九键表顺序、粘滞 Ctrl（一次性、与第二行共享）、历史只填不发、剪贴板禁用原因/粘贴/读取时拒绝、三条导航键、冻结门 |
| `web/src/features/session/tty/promptHistory.test.ts`（新） | 5 个 vitest：最新在前、非 human origin 排除、去重裁剪、草稿在前且非破坏、空实例 |
| `web/tests/e2e/m-keybar.hub.spec.ts`（新） | hub 规格：390 九键四角几何 + 第二行 testid/粘滞联动；390 git/files 往返（无触控上下文，§3.4 第二维度）；390 结构键 ↔ 顶栏 ViewSwitch URL+viewPref；1440 桌面回归 |
| `web/tests/e2e/mobile-qa.spec.ts` | 非 hub 的 mock 视觉 QA：旧移动键条断言改为「先断言九键、再开 键 行断言 11 枚 tty-key」 |
| `docs/design/evidence/mobile-ui-6.md` + `mobile-ui-6-terminal-390.png` | 本文件与 390 终端截图（仅假节点合成数据） |

未触碰：`SessionPage.tsx`、`session.module.css`、`Composer.tsx`、`Transcript.tsx`、`PhoneShell.tsx`、`QuickFind.tsx`、`playwright.hub.config.ts`、`sw.src.js`、`deploy/`。

## 2. 九键表与能力映射（验收 1，B.3.2）

顺序（DOM testid 顺序，e2e 逐位断言）：

| 位 | 键 | 接到的既有能力 |
|---|---|---|
| 1 | Ctrl | AuxKeys 同款粘滞修饰（与第二行共享，一次性） |
| 2 | Esc | 原始字节 `` 走既有 tty `send` |
| 3 | Tab | 原始字节 `\t` 同上 |
| 4 | git | 现有 `navigate('/s/:id/files')` |
| 5 | 跳转 | 导出的 `openQuickFind()`（任务 7 换成分组 sheet 前的退化路径） |
| 6 | 贴 | `lib/clipboard.readClipboard()` → 原始字节 `send` |
| 7 | 史 | 本实例历史 prompt sheet，选中只填入 LocalInput |
| 8 | 结构 | `navigate('/s/:id/structured')`，与顶栏 ViewSwitch onChange 同一路径 |
| 9 | 键 | 唤起软键盘（focus/blur 本地输入）+ 展开原始字节第二行 |

九键是动作键，不是 3×3 网格：单行横滑，高度恒定 44px + 8px 上下托盘，整条 60px 与任务 5 折叠后的旧键条同高，xterm 的 §4.7 ≥60% 正文预算不变（m-chrome 实测 0.658）。

### 跳转（验收 3）

QuickFind 的 overlay 只在 spaces drawer 里挂载（compact `/s/:id` 无侧栏），因此键先程序化点击既有 `spaces-drawer-open`（与手点返回键同一路径），再调用**导出的** `openQuickFind()`——没有复制 QuickFind 的状态或 UI。任务 7 落地分组 sheet 时只改这一处。

## 3. 44px 几何（验收 1，D-039）

- 每枚键是真实 44×44 盒子（`var(--touch)`），横向 4px 间隔，互不重叠。
- 视觉是 4px 圆角描边（1px inset outline，不用 border——border 会让 0.5px 内缩的四角命中到描边外侧），命中面用方形 `::after` 覆盖圆角，保证四角都是键自己；这是 D-039「视觉字形尺寸不变、命中靠热区」的同一手法。
- 按键不贴视口边：行用 8px 托盘内边距（旧移动 `.keys` 条本来就是 8px）。Chromium 在视口最底/最右 0.5px 对 `elementFromPoint` 返回 null（实测底、右边均如此，与按键宽度无关），贴边按键的底两角无法通过四角探针；8px 内边距让四角全部落在探针可达区。真机上这也正是安全区留白。
- 行本身隐藏滚动条（`scrollbar-width:none` + `::-webkit-scrollbar`），触摸横滑不变，且没有经典滚动条轨道压在命中角上。
- 390 e2e 用 ux-touchhit 同款四角探针（中心 ±22px、0.5px 内缩）逐键断言，并先 `scrollIntoView` 到行中央。
- 第二行展开后 11 枚 `tty-key-*` 逐一断言可见且盒 ≥44px；粘滞 Ctrl 在两行之间联动（动作行点亮 → 展开后第二行 Ctrl 同为 pressed）。
- 几何用 `hasTouch:true` 但不带 `isMobile:true` 的 context（ux-touchhit 同款）：isMobile UA 模拟在视口底边另留一条亚像素死带；compact 布局由 `COMPACT_WORKBENCH_QUERY` 宽度决定、本地输入默认由 coarsePointer 决定，与真机一致。

## 4. 贴 键与权限（验收 4）

- 首帧由同步事实决定：无 `navigator.clipboard.readText`（含非 HTTPS 安全上下文）即禁用。
- 挂载后 `probeClipboardRead()` 查 Permissions API：`denied` → 禁用；不支持 query（Safari）→ 乐观，读取手势时再判。窗口 focus / 可见性变化时重探。
- 禁用时：`disabled` **且**条下常驻一行原因（`phone-clip-reason`，`var(--text-aux)`），不只靠 hover title；点击不产生任何写入（vitest + e2e 覆盖）。
- 允许时读取剪贴板把字节经 tty 原始通道送出；剪贴板为空 toast；读取当场被拒 toast 「读取剪贴板失败：浏览器未授权」并翻成禁用+原因。永不静默失败。
- e2e 在假节点里授 `clipboard-read/write`，先断言无原因行、键可用，写入唯一 marker 后点击，`tty-raw-tail` 收到该 marker（字节进了 PTY）。

## 5. git 键往返与滚动恢复（验收 2）

- 导航只用既有路径：`navigate('/s/:id/files')`；返回用 FilesView 既有的 `files-back`（SessionPage 对 files 后回 `/s/:id/tty` 的 Esc/back 处理不变）。e2e 断言 URL `/s/:id/files$` → `/s/:id/tty$`、`files-pane` 可见、返回后 `data-tty-ready=1` 与九键条重新挂载。
- xterm 在 tty→files→tty 之间整组件卸载，SessionPage 的 `preFilesScroll` ref 抓不到它（顶栏 files 开关在此路由还折进 `⋯`）。因此 git 键点击时由 TerminalView 用 `term.buffer.active.baseY` 抓当前缓冲**行**（不是像素），新 attach 重放 Hub 屏幕快照后用 `term.scrollToLine()` 恢复；按行索引与像素舍入无关。

### 已记录的 headless 限制

在 hub 假节点 + bundled chromium（webgl/canvas 渲染器）下，xterm v6.0.0 的**程序化滚动**在本环境完全不改变 `buffer.active.baseY`：`scrollToLine` / `scrollLines` / `scrollToTop` 调用后 ydisp 不变；受信任 `mouse.wheel`、CDP `dispatchTouchEvent` 滑动同样不改变它（真实手机上的滚动路径是 `attachTerminalTouch` → `term.scrollLines`，触摸事件在桌面 chromium 下无法由自动化合成为等价的受信序列）。已排除：宽度无关、DPR 无关、renderer 无关（临时强制 DOM 渲染器亦同）、鼠标跟踪无关（`data-tty-mouse=none`）、disableStdin 无关。这是 xterm v6 虚拟滚动在该无头环境的行为；桌面 1440 规格只断言 toolbar 渲染，全仓此前也没有任何 hub 规格驱动过 xterm 回滚（`terminal-live` 的 wheel 断言属于需真实后端的非闸口配置）。

因此 hub 规格在 390 无触控上下文（ui-spec §3.4 明确要求的「390 无触控」第二维度）断言：九键条在宽度驱动的 compact 布局下挂载、贴键字节进入同一 PTY、git 走 files 往返、返回后 xterm 重挂且重放的快照仍含该 marker（同一活终端、缓冲连续）。产品代码的行级捕获/恢复逻辑在真机回滚发生时即生效；该 headless 限制不影响任何真机能力，也未触碰产品渲染器。

## 6. 史 键与 D-028a（验收 5）

- 历史来源只有计划点名的两处：journal 经**同一个** `assembleTranscript` 投影的 human user 节点 + `lib/drafts.ts` 未发送草稿；不另造派生。
- sheet 选中一条：`onFillInput` → TerminalView 用 nonce 重挂 LocalInput 并 `initialText` 注入，sheet 关闭；**绝不调用 send**。vitest 断言 `onFillInput` 恰好 1 次、`onKey` 0 次；e2e 走九键其余键证明字节通道独立。
- sheet 内常驻「选中只填入本地输入条，不会自动发送（D-028a）」提示；空历史给 `phone-history-empty`。
- 草稿读取非破坏（vitest 断言 localStorage 原值仍在）。

## 7. 结构键 ↔ ViewSwitch（验收 6）

390 e2e：tty 段点 `phone-key-view` → URL `/s/:id/structured`、`session-page[data-view=structured]`、顶栏 `view-switch[data-view=structured]`、localStorage `runtime.session-view.<id>` 为 `structured`（SessionPage 的 effect 写入）；再点顶栏 `view-switch-tty` 回 tty，viewPref 跟为 `tty`，九键条重新出现。键没有自己的状态——与顶栏是同一条 `/s/:id/<view>` 导航，URL 与 viewPref 必然一致。

## 8. 桌面零变化（验收 8）

1440 e2e：`phone-keybar` 计数 0；`tty-keybar`（toolbar 变体）仍在，11 枚 `tty-key-*` 全部可见，条高 ≤30px（桌面 28px 视觉行原样）。AuxKeys 的 BAR/STRIP、toolbar 与默认 bar 变体代码路径未改；`pnpm test`（1396 项）与既有终端规格保持绿。

## 9. 验证记录

- `pnpm --dir web test`：145 文件 / 1396 项全绿（含新增 16 项 tty 单测）。
- `pnpm --dir web typecheck`、`pnpm --dir web lint`：通过（仅有仓内既有的 fast-refresh/ref 警告，本批文件无新增）。
- `m-keybar.hub.spec.ts` 连续 **3 次** 4/4 通过（`PW_CHANNEL=chromium`，锁槽 e2e.lock-b，端口 59230/59239/59231；本机无 google-chrome，按 hub 配置注释使用 bundled chromium）。
- 相关回归一次全绿（8/8）：`ux-touchhit.hub.spec.ts`、`m-chrome.hub.spec.ts`、`ux-ttymode.hub.spec.ts`。
- **全套 hub e2e 一次**（`CI=1`，同锁槽，26.6m）：**157 passed / 19 skipped / 1 failed**。唯一失败 `ux-nextstep.hub.spec.ts:159` 两次尝试均 90s 套件负载超时（无断言失败，仓内已知 flake，coordinator 确认；另一已知 flake `grok-structural.hub.spec.ts:507` 本次通过）。本批未触碰该规格及其依赖（session 行 / approval / store）。
- `bash scripts/ci/secret-scan.sh`：通过。
- 截图 `mobile-ui-6-terminal-390.png` 由规格以 `REMUDA_EVIDENCE=1` 生成，画面仅含假节点 `e2e-fake-node` / `wsp_e2e` 合成数据；无主机名 / 用户名 / home 路径入镜。
- 闸口提示：本任务改动落在 `web/features/session/tty/**` + `web/lib/clipboard.ts` + `web/tests/e2e/m-keybar.hub.spec.ts` + 一个非闸口 mock 规格；按 [web_e2e.rs](../../../crates/remuda/src/cmd/merge/web_e2e.rs) 清单需在 `remuda merge` 显式 `--web-e2e`（与任务 5 同款说明，无闸口清单文件改动）。
