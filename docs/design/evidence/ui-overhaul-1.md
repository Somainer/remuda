# UI 改版 UO-1：基础层（D-053，2026-09-24）

工作树 `wt/c-uo1/b-uo1-md`。本轮只落**基础层**：色彩/字号/间距 token、
外观运行时（跟随系统 / 深 / 浅）、打包字体（仅 IBM Plex Mono）、基础控件
（Button、Modal、StateDot、Icon、ConnectionIndicator、设置页外观段）与
终端调色板。页面级重排属于后续阶段，截图里未改造的表面仍是旧样式（见 §5）。

规范：[visual-system.md](../visual-system.md)（权威）、[ui-spec.md](../ui-spec.md)、
[decisions.md](../decisions.md) D-053。

## 1. 复核命令

```
pnpm --dir web typecheck
pnpm --dir web lint
pnpm --dir web test -- --run
# 对比度 / token 守卫 / 外观运行时 / 终端调色板
pnpm --dir web vitest run src/styles src/features/settings src/features/session/tty/theme.test.ts \
  src/features/session/tty/terminalFit.test.ts src/pages/SettingsPage.test.tsx
# 首帧、模式切换、减弱动效、字体晚到、触控热区
pnpm --dir web exec playwright test -c playwright.hub.config.ts theme-boot font-swap uo1-hitarea ux-chrome ux-touchhit
# 截图（写入本目录 ui-overhaul/，只存 390 与 1440；UO1_SHOT_COPY_DIR 可另收全部宽度含 768）
REMUDA_EVIDENCE=1 pnpm --dir web exec playwright test -c playwright.hub.config.ts uo1-evidence
# 性能（与 main 同机对比）
pnpm --dir web exec playwright test -c playwright.perf.config.ts
```

## 2. 截图

`ui-overhaul/UO-1-<surface>-<mode>-<width>.png`，5 个表面 × 深/浅 × 390/1440，共 20 张，
由 `web/tests/e2e/uo1-evidence.hub.spec.ts` 在假 Node 上生成（`reducedMotion: reduce`，
等 `document.fonts.ready` 后截）：

| surface | 内容 |
| --- | --- |
| sessions | 1440 为 `/sessions` 列表；390 为 `/m` 手机首页 |
| structured | claude-print 会话：表格回显与已结束的 Bash 工具卡同框（先把表格滚到视口顶部，断言二者都在视口内），下方为待批准卡（琥珀） |
| inbox | 1440 为 `/approvals`；390 为 `/m/inbox` |
| settings | 设置页外观段（跟随系统 / 深 / 浅） |
| terminal | claude-pty 会话的 xterm；浅色模式下终端仍为深色底（规范「终端始终为深色底」） |

## 3. 验收逐项

| 项 | 证据 | 结果 |
| --- | --- | --- |
| 首帧外观 | `theme-boot.hub.spec.ts`「appearance applied at boot」：存储 light/ledger 在 `/`、`/sessions`、`/approvals`、`/m` 首个应用帧即为浅色画布，之后属性不再翻转；存储 dark 压过浅色系统；system/未存/非法值跟随系统且不写属性 | 通过 |
| 写入被拒回滚 | `appearance.test.ts:136`（写入被拒时抛错且不动 DOM）、`SettingsPage.test.tsx`「save states and rollback」（`runtime.theme.v1` 写入抛错时单选回到上次提交值） | 通过 |
| 终端对比度 | `tty/theme.test.ts` 按 WCAG 计算前景对终端底与选区的对比度，ANSI 16–255 交给 xterm 标准色立方；`terminalFit.test.ts:13` 未改动且通过 | 通过 |
| 减弱动效 | `theme-boot.hub.spec.ts`「reduced motion」：深/浅各一遍，`/sessions`、`/approvals`、`/settings` 上悬停并聚焦前 6 个按钮，`document.getAnimations()` 中运行的、不在 `[data-motion=essential]` 内的动画为空。反向对照：改为 `no-preference` 后在 `/settings` 捕获到 `transition:color`，确认断言有效 | 通过 |
| 模式切换无过渡 | `appearance.ts` 在每次切换（含跟随系统时的系统翻转）给 `<html>` 挂 `data-mode-switch` 两帧，`tokens.css` 在此期间关闭所有 transition。单测 `appearance.test.ts`「holds transitions off for two frames…」；e2e `theme-boot.hub.spec.ts`「mode switch」在 `no-preference` 下悬停一个分段项再切 dark→light / light→dark，下一帧运行中的 `CSSTransition` 为空 | 通过 |
| 焦点环 | `tokens.css` 的「文本输入以边框表示焦点」只作用于文本类 `input`/`textarea`/`select`，checkbox、radio、range 等回到全局 `--focus` 焦点环。本轮拥有文件中其余 `outline: none` 仅 `settings.module.css` 的分组容器（`tabIndex=-1` 的程序化焦点目标，不可 Tab 到） | 通过 |
| 被拒后焦点 | `SettingsPage.test.tsx`「returns focus to the committed radio…」：写入抛错时方向键选中被回滚，焦点回到已提交项（`tabIndex=0` 且为 `activeElement`）；「steps through two rapid arrow keys…」：首次保存未完成时连按两次右键，依次落到深色、浅色，焦点不回跳 | 通过 |
| 阅读标题 | `ui.module.css` `.md`/`.prose`：h1 20/28、上 24 下 12；h2 18/26、20/8；h3 16/24、16/4；均 600 | 通过 |
| 紧凑行高度 | body 行高用 `--lh-ui-ratio`（19/13，<768 为 22/15）而非固定 `--lh-ui`：`--text-ui` 文字仍恰为 19/22px，只改字号的小字（芯片、标记）行盒按比例缩小，与 main 的 `1.45` 行为一致。固定 19px 曾让 Provider 模型行的 10px 芯片撑到 23px（行高 36，`providers-discovery.spec.ts:179` 要求 < 34），并让 390 下会话行 headline 里 10px 的启动来源标记撑到 24px（行高 22，`ux-nextstep.hub.spec.ts` 单行断言）；两条现均通过 | 通过 |
| 粗指针热区互不重叠 | chip 与分段控件的 44px 只在纵向延伸，并以 `margin-block` 在布局中预留，换行后相邻行不共享热区；chip 的 `::after` 从 padding 盒外扩 1px 边框，热区为完整 44px。`uo1-hitarea.hub.spec.ts`（`hasTouch`）：390 下原始事件筛选 chip 实际换成多行，每个 chip 44px 热区四角 `elementFromPoint` 都落在自身；设置页外观分段同样。`ux-touchhit` / `ux-chrome` 的既有热区断言通过 | 通过 |
| 无未映射颜色 / 无 `--dust` 填充 | `tokenGuard.test.ts` 扫描本轮拥有的 CSS/TSX | 通过 |
| 对比度 token | `tokens.contrast.test.ts`：深浅两套正文/次要/状态色对各自表面 | 通过 |
| 字体晚到 | `font-swap.hub.spec.ts`：woff2 延迟 800ms，每条先做一次无换字体对照（字体已缓存）再做换字体恢复。两组输入相同：首次离开后快照 `runtime.reading.v1.<id>` 整条记录，每组进入前写回，init 脚本记录会话页文档启动时读到的记录并断言两组都与快照逐字节相同（对照组本身会改写该记录，实测两条用例都改写了锚点或偏移）。断言：换字体恢复距保存位置不超过对照的误差 + 4px（换字体不引入额外偏移）、字体换入前后位移 ≤ 4px、行不重叠。短会话：`saved=249.09 control=339.09 beforeSwap=339.09 settled=339.09`（对照误差 90，换字体 90，额外偏移 0）。长会话（2400 行突发 + 代码回复 + 24 行，超过 Hub 2000 行尾窗，断言 `fromSeq>1` 且 `reachedAfterSeq=false`，即只重放有界窗口）：`saved=249.39 control=224.81 beforeSwap=224.81 settled=224.81`（对照误差 24.6，换字体 24.6，额外偏移 0；紧凑行高修正后重测）。修正前两条连续各跑 2 遍数值一致；390 置底会话换字后仍置底 | 通过 |
| 截图 1440/768/390 | 仓库内 20 张见 §2（390 与 1440）；768 深/浅另行交付评审目录，不入库 | 通过 |
| 44px 断言迁移 | `ux-chrome.hub.spec.ts` 按 `touch` 分 `hasTouch` 上下文、只在触控下断言热区；`ux-settings.spec.ts` 的「44px touch targets」在 `hasTouch` describe 内；`ux-filters.spec.ts` 手机筛选面板一条移入 `hasTouch` describe。后者仍失败，原因见 §4 | 已迁移 |
| 冒烟集 | m-chrome、m-home、m-inbox、turn-end、session-virtual.hub、journal-window、ux-touchhit、font-swap、theme-boot：28 过 / 2 跳过 / 1 失败 / 1 未跑 | 见 §4 |
| `test:e2e:terminal` | 本机未跑，需排到开发机。`remuda dev` 的终端会话是 Herdr pane，启动时还会 `reconcile_herdr`；本机唯一的 Herdr 是承载协调者与各 worker 会话的共享服务器，在其上建/收 pane 有干扰现有会话的风险（`--no-herdr-orphan-sweep` 只关清扫，不关对账与建 pane）。另起私有 socket 的 Herdr 属于在本机起新的系统服务，本轮未获授权。开发机复现：隔离 data dir、`remuda dev --hub-listen 127.0.0.1:58280 --access-code-file <file> --web-origin http://127.0.0.1:58290 --workspace-root <tmp>`，再 `VITE_ACCESS_CODE=<code> pnpm --dir web test:e2e:terminal` | 未验证（待开发机） |
| 性能 | §6 | 见 §6 |

## 4. 已知失败（非 UO-1 引入；前两条在 main 源码上复现过，mobile-webkit 两条按原因归类，本轮未在 main 上重跑）

- `turn-end.hub.spec.ts:271`：`live-decided-by` 为 `hook`，期望 `screen`（node 侧竞态）。
- `ux-filters.spec.ts:288`（手机筛选面板 44px，已放进 `hasTouch`）：`ViewportGate` 在宽度 < 768
  时把 `/sessions` 交给手机首页 `/m`，筛选按钮不存在，点击超时。与热区无关，面板在 390 下
  已不可达，需要产品决定该用例改到哪个表面。同因：`ux-filters-evidence.spec.ts:103`（390 一帧）、
  mobile-webkit 下 `ux-settings.spec.ts:145`（深链后期望 `session-list`）。mobile-webkit 下
  `ux-filters.spec.ts:145`「Tab stays inside the open panel」为 WebKit 的 Tab 默认不聚焦按钮。
- `EffortSlider` 单测一条：合并前即失败（紧凑行高修正后的一次全量单测中通过，时有时无）。
- `ux-code.hub.spec.ts:194`（fenced code 工具栏）：`jump-latest` 一直不稳定，点击超时；在 main 的 `web/src` 上同样失败。
- 分批跑时偶发、单独重跑通过或去掉本轮改动后同样偶发：`ux-files.hub.spec.ts:175`（去掉紧凑行高修正 3 次失败 1 次，保留时 2 次失败 1 次）、`ux-livephrase.hub.spec.ts:116`（单独重跑通过）、`ux-keys.hub.spec.ts:360`（期望恰好 9 行会话，依赖同文件前一条及其他 spec 留下的会话；整文件单跑 5/5 通过）。
- Transcript 位置恢复精度：`saved` 与 `control` 相差约 95px（上方行高不一的场景可达 1318px），
  来自 `Transcript.tsx` 以估算行高补未测量行，与字体无关；font-swap 因此以无换字对照为基准。
- Transcript 长窗口滚动跳行（新发现，基线问题）：2000 行尾窗里从底部向上按像素滚动（`scrollTop`
  或真实滚轮都一样），越过一个高行后一次跳过约 100 行，随后又回跳，代码回复被整段跨过。
  长会话 font-swap 因此改用正文搜索按行号跳到代码块；滚动本身需另开任务修。

## 5. 未改造表面（后续阶段）

截图中仍可见旧样式：会话头部等单色等宽按钮（如 “Compact”）、带边框的输入框、390 下已选
space 芯片 “changed” 的琥珀描边、红色 “打断” 按钮。这些不在本轮拥有文件内，按规范应在
后续阶段改为中性控件；琥珀/红色语义目前被它们越界使用。

## 6. 性能（chromium，与 main 同机，后台负载 load avg 8–12）

A（长 transcript）与 C（交互洪峰）：HEAD 与 main 均 0 个长任务。

B（终端洪峰），每轮长任务总时长（ms）：

| 数据集 | HEAD | main |
| --- | --- | --- |
| 合并 main 前，各 9 轮 | 137, 124, 112, 0, 118, 0, 0, 67, 0（合计 558） | 121, 0, 124, 0, 0, 0, 117, 0, 0（合计 362） |
| 合并 main 后，各 6 轮 | 0 个长任务 | 1 轮 3 个，最长 117，TBT 79（约 229） |

B 的长任务在两侧都是偶发的；CPU profile 显示反复出现的约 120ms 任务是 xterm WebGL
首帧上传（`texImage2D` / `bufferData` / `renderRows`），main 同样存在。第一组数据下 HEAD
总量高于 main 的 110%，第二组相反；在当前负载下两组都不足以区分，如实并列。
`profileFlags.ts` 未改动。
