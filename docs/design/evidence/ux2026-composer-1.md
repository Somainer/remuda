# ux2026 composer 手机收一行 + 去 window.confirm（c-composer / P0-3）

- 日期：2026-09-19
- 任务：workbench-ux plan §(B) 4 `c-composer`
- 规格：`docs/design/ui-spec.md` §2.2「compact composer 边界（D-042）」（条款 4，分支 `wt/c-uxspec/b-uxspec-md`，D-042 见 `docs/design/decisions.md`）
- 截图全部来自 fake-node 合成 fixture（`e2e-fake-node` / `remuda-e2e` space），无真实主机、路径或用户名。

## 落地内容（逐条对验收）

1. **手机 control bar 收成「一个触发器 + 输入框 + 主按钮」**。触发器文案形如 `询问 · ?`（权限 label/native 词 + effort 档名；effort 未回读时诚实显示 `?`，桌面同口径），testid `model-effort-chip` 并带 `data-options-trigger="1"`、`data-permission`、`data-permission-danger`；档位词仍在 `model-effort-chip-label`。
2. **danger 模式在收起态就可见、可听**：bypass 启动的会话，触发器边框/文字直接用 danger 色（`data-permission-danger="1"`，span `composer-trigger-permission` 显示「绕过全部」）；其 `aria-label` 同时含权限词与「（危险）」标记和 effort 档，不需打开 sheet。见下方第 4 张截图。
3. **选项 sheet**（`composer-options-sheet`，`variant=sheet`），按调度计划 §C 默认 4 放五样：附件（文件/拍照/粘贴）、harness 只读芯片、**context 用量**（在 sheet 内点上下文芯片再出 `ContextUsagePopover` 的手机底部 sheet）、权限选择器（`permission-menu` / `permission-option-*` testid 不变）、effort 滑杆（`effort-menu`/`effort-slider`，复用同一张 popoverCard）。**协调裁决（handback 9）：context 不再留在收起条**；收起触发器只显示权限词 + effort 档。收起条的触摸目标用居中 `::after` 扩到 `var(--touch)`（≥44px，D-038），可视高度仍 32px。
4. **三态与诚实标注留在 sheet 外（D-028a）**：发送/排队/打断按钮、`composer-queued-row` / `composer-queue-status`、`composer-cap-note` 一律在收起条上；spec 断言它们在 sheet DOM 内 count=0。
5. **从 sheet 发起的操作先关 sheet**：附件按钮先 `setOptionsOpen(false)` 再聚焦/滚动输入框，避免焦点穿过 `aria-modal` scrim；选权限/拖滑杆/关闭都把焦点交回收起触发器（`useFocusTrap` returnFocusRef）。
6. **placeholder 分平台**：手机「输入提示词…」；桌面保留完整快捷键说明。
7. **两处 `window.confirm` 换成 `Sheet`**（`composer-confirm`）：插队与 Esc 打断；桌面 `variant=popover`、手机 `sheet`；复用 `useFocusTrap`（焦点圈定、Esc=取消、确认后返回焦点到触发器）。Sheet 不像原生 confirm 那样阻塞线程，所以确认时**重新校验实时回合状态**（refs），且回合离开 working/busy 时自动撤掉对话框——回合在用户阅读确认时结束，不会再对 idle 实例补发 steer/cancel。命令路径本身不变（仍是 `onSend(..., "steer")` 与 `onInterrupt`/`instance.cancel`）。e2e 注册 `page.on("dialog")` 哨兵，断言全程无原生对话框。
8. **桌面零变化**：桌面 DOM、testid、快捷键（Enter 排队 / ⌘⌃+Enter 插队 / Esc 打断）、AnchoredPopover 布局全部保留；`session.module.css` 未改；既有 `composer-effort.spec.ts` 桌面项目断言不变，仅为其 mobile-webkit 项目补了 D-042 分叉。

## 截图（390×844，fake node，Night Corral）

| # | 状态 | 文件 |
|---|---|---|
| 1 | 收起态：单行 `询问 · ?` 触发器（无 context 段）+ 发送；placeholder 无桌面快捷键 | [ux2026-composer-1-collapsed-390.png](./ux2026-composer-1-collapsed-390.png) |
| 2 | 选项 sheet：附件 / 载体 / 上下文 / 权限选择器 / effort 滑杆 | [ux2026-composer-1-sheet-390.png](./ux2026-composer-1-sheet-390.png) |
| 3 | 插队 Sheet 确认（取消 / 打断并发送，danger） | [ux2026-composer-1-steer-confirm-390.png](./ux2026-composer-1-steer-confirm-390.png) |
| 4 | bypass 启动：收起触发器直接 danger 样式显示「绕过全部」；打断/排队三态在 sheet 外 | [ux2026-composer-1-danger-trigger-390.png](./ux2026-composer-1-danger-trigger-390.png) |

> 注：新会话首次 effort 未回读时触发器档位显示 `?`（与桌面 chip 完全一致的诚实口径，`isEffortUnknown`），回读后显示真实档名。

## e2e 断言摘要（`ux-composer-mobile.hub.spec.ts`，3 case）

1. 收起条 `data-collapsed="1"`；触发器带 `data-permission=manual`、`data-permission-danger=0`，权限词与档名同在；收起条无附件/harness/context/权限菜单；触发器 `::after` 热区在可视 chip 上下 6px 边缘点命中（≥44px，`document.elementFromPoint`）；sheet 内含附件/harness/context/权限/effort，三态与 cap-note count=0；**在 sheet 内改权限（→可改文件）和 effort 滑杆，关闭后收起触发器摘要随之更新，并把焦点交回触发器**；排队后 `composer-queue-status` 与逐行 chip 仍在 sheet 外。
2. 插队：Sheet 取消时草稿保留、无 `mode=steer` POST；确认后 fake-node 收到 `instance.send mode=steer` 且出「已打断」。Esc 打断：Sheet 内 Esc=取消（无 `instance.cancel`），确认后 fake-node 收到 `instance.cancel`；全程 `page.on("dialog")` 哨兵为空。
3. bypass 启动（New Session 勾选「绕过全部」+ yolo ack）：未打开 sheet 时触发器即为 `data-permission=bypassPermissions` / `data-permission-danger=1` / 文案「绕过全部」。

## 改动文件清单与理由

| 文件 | 归属 | 改动理由 |
|---|---|---|
| `web/src/features/session/Composer.tsx` | c-composer | 手机收起/Sheet 分叉、Sheet 确认 + 实时状态重校验、context 移入 sheet |
| `web/src/features/session/ComposerOptions.tsx`（新） | c-composer | 选项 Sheet 与确认 Sheet（共享 `components/Sheet`） |
| `web/src/features/session/composerOptions.module.css`（新） | c-composer | 触发器/Sheet/确认样式与 `--touch` 热区；不改 `session.module.css` |
| `web/src/features/session/Composer.test.tsx` | c-composer | D-042 单测（触发器/sheet 拆分、danger aria、关 sheet 回焦、附件先关 sheet、回合结束撤确认、实时守卫） |
| `web/src/features/session/ComposerSteerQueue.test.tsx` | c-composer | 三态在 sheet 外 |
| `web/src/features/session/ComposerAttachments.test.tsx` | c-composer | 手机附件入口移入 sheet |
| `web/tests/e2e/ux-composer-mobile.hub.spec.ts`（新） | c-composer | 3 个 fake-node 手机用例 |
| `web/tests/e2e/ux-steer.hub.spec.ts` | c-composer 迁移 | 原生 confirm → Sheet |
| `web/tests/e2e/hub-live.spec.ts` | c-composer 迁移 | 原生 confirm → Sheet |
| `web/tests/e2e/promoted-claude.hub.spec.ts` | c-composer 迁移 | 删除冗余 confirm 监听（可见打断不经确认） |
| `web/tests/e2e/composer-effort.spec.ts` | c-composer 分叉 | mobile-webkit 项目按 D-042 打开 sheet；Grok 权限菜单改 `expect.poll` 确定性断言 |
| `web/tests/e2e/ux-modelsync.hub.spec.ts` | c-composer 最小改动 | openModelList 的防御性 Esc 可能弹出新打断 Sheet，加同款 stray-confirm 守卫；**恢复**被误删的两处 clearApprovals 与 approval-card count=0 |
| `web/tests/e2e/ux-modelpick.hub.spec.ts` | c-composer 同款守卫 | 与 ux-modelsync 相同的 stray-confirm 守卫（openModelList 同样的防御性 Esc） |
| `web/tests/e2e/ux-usage.hub.spec.ts` | c-composer 后果适配 | 390px 用例：context chip 移入选项 sheet 后，先开 sheet 再断言 chip 与明细 sheet |
| `web/tests/e2e/session-structured.spec.ts` | **handback 1 授权**（c-sessionchrome 拥有；c-toolfold 在改同文件） | 仅 107-117 行那个用例：compact 下先开选项 sheet 再断言只读 harness-chip |
| `web/tests/e2e/session-chrome-evidence.spec.ts` | **handback 1 授权** | 400px：golden 在 sheet 关闭状态截取，断言只读 harness-chip 时再临时打开 sheet；`shot()` 走 REMUDA_EVIDENCE 开关，默认运行写入 gitignored `test-results/`，不改动 tracked golden |
| `web/tests/e2e/ux-touchhit.hub.spec.ts` | **round-4 授权**（c-touchhit 拥有） | D-042 后果：390px effort 卡片移入选项 sheet，探测 reset/list-back 44px 角点前显式 `scrollIntoView({block:"center"})`（293-315 行附近，仅该 390px 用例） |

## 兼容性迁移

旧的 `page.once("dialog", …)` 原生确认在三处 hub spec 改为点 Sheet 的 `composer-confirm-ok`：`ux-steer.hub.spec.ts`（按钮 + ⌃Enter 两处）、`hub-live.spec.ts`（插队 + Esc 打断）、`promoted-claude.hub.spec.ts`（可见「打断」按钮本就不经 confirm，删除冗余 dialog 监听）。可见「打断」按钮始终是直达动作，只有插队与 Esc 走确认——与改动前语义一致。
