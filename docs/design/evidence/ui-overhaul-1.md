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
# 首帧、减弱动效、字体晚到
pnpm --dir web exec playwright test -c playwright.hub.config.ts theme-boot font-swap
# 截图（写入本目录 ui-overhaul/）
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
| structured | claude-print 会话：待批准卡（琥珀）、代码块、表格、已结束工具卡 |
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
| 无未映射颜色 / 无 `--dust` 填充 | `tokenGuard.test.ts` 扫描本轮拥有的 CSS/TSX | 通过 |
| 对比度 token | `tokens.contrast.test.ts`：深浅两套正文/次要/状态色对各自表面 | 通过 |
| 字体晚到 | `font-swap.hub.spec.ts`：woff2 延迟 800ms。1440 保存位置：`saved=249.375 control=344.375 beforeSwap=348.375 settled=348.375`，字体换入前后位移 0，与无换字体对照差 4px（阈值 4px）；390 置底会话换字后仍置底 | 通过 |
| 截图 1440/768/390 | 20 张见 §2（390 与 1440）；768 在 smoke 集的 m-chrome / ux-touchhit 中覆盖布局，本轮未单独存图 | 部分 |
| 冒烟集 | m-chrome、m-home、m-inbox、turn-end、session-virtual.hub、journal-window、ux-touchhit、font-swap、theme-boot：28 过 / 2 跳过 / 1 失败 / 1 未跑 | 见 §4 |
| `test:e2e:terminal` | 9 条全部跳过：需要 `VITE_ACCESS_CODE` 与在线 dev hub，本机不具备 | 未验证 |
| 性能 | §6 | 见 §6 |

## 4. 已知失败（均在 main 源码上同样复现，非 UO-1 引入）

- `turn-end.hub.spec.ts:271`：`live-decided-by` 为 `hook`，期望 `screen`（node 侧竞态）。
- `ux-filters.hub.spec.ts:297`（手机筛选面板 44px）：390 下 `/sessions` 交给手机首页，筛选按钮不存在。
- `EffortSlider` 单测一条、若干 mock e2e：合并前即失败。
- Transcript 位置恢复精度：`saved` 与 `control` 相差约 95px（上方行高不一的场景可达 1318px），
  来自 `Transcript.tsx` 以估算行高补未测量行，与字体无关；font-swap 因此以无换字对照为基准。

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
