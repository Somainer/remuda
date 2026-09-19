# ux2026 composer 手机收一行 + 去 window.confirm（c-composer / P0-3）

- 日期：2026-09-19
- 任务：workbench-ux plan §(B) 4 `c-composer`
- 规格：`docs/design/ui-spec.md` §2.2「compact composer 边界（D-042）」（条款 4，分支 `wt/c-uxspec/b-uxspec-md`，D-042 见 `docs/design/decisions.md`）
- 代码：`web/src/features/session/Composer.tsx`、`ComposerOptions.tsx`（新）、`composerOptions.module.css`（新）
- 测试：`web/tests/e2e/ux-composer-mobile.hub.spec.ts`（新，fake-node，390×844 `hasTouch`）；单测 `Composer.test.tsx` / `ComposerSteerQueue.test.tsx` / `ComposerAttachments.test.tsx`
- 截图全部来自 fake-node 合成 fixture（`e2e-fake-node` / `remuda-e2e` space），无真实主机、路径或用户名。

## 落地内容（逐条对验收）

1. **手机 control bar 收成「一个触发器 + 输入框 + 主按钮」**。触发器文案形如 `询问 · ?`（权限 label/native 词 + effort 档名；effort 未回读时诚实显示 `?`，桌面同口径），testid `model-effort-chip` 并带 `data-options-trigger="1"`、`data-permission`、`data-permission-danger`；档位词仍在 `model-effort-chip-label`。
2. **danger 模式在收起态就可见**：bypass 启动的会话，触发器边框/文字直接用 danger 色（`data-permission-danger="1"`，span `composer-trigger-permission` 显示「绕过全部」），不需要打开 sheet。见下方第 3 张截图。
3. **选项 sheet**（`composer-options-sheet`，`variant=sheet`）：附件（文件/拍照/粘贴）、harness 只读芯片、context 用量环（保留在收起条上作为 fused 段，点开仍是 `ContextUsagePopover`）、权限选择器（`permission-menu` / `permission-option-*` testid 不变）、effort 滑杆（`effort-menu`/`effort-slider`，复用同一张 popoverCard）。
4. **三态与诚实标注留在 sheet 外（D-028a）**：发送/排队/打断按钮、`composer-queued-row` / `composer-queue-status`、`composer-cap-note` 一律在收起条上；spec 断言它们在 sheet DOM 内 count=0。
5. **placeholder 分平台**：手机「输入提示词…」；桌面保留完整快捷键说明。
6. **两处 `window.confirm` 换成 `Sheet`**（`composer-confirm`）：插队与 Esc 打断；桌面 `variant=popover`、手机 `sheet`；复用 `useFocusTrap`（焦点圈定、Esc=取消、确认后返回焦点到触发器）。确认后的命令路径不变（仍是 `onSend(..., "steer")` 与 `onInterrupt`/`instance.cancel`）。e2e 注册 `page.on("dialog")` 哨兵，断言全程无原生对话框。
7. **桌面零变化**：桌面 DOM、testid、快捷键（Enter 排队 / ⌘⌃+Enter 插队 / Esc 打断）、popover 布局全部保留；既有 `composer-effort.spec.ts` 桌面项目断言不变，仅为其 mobile-webkit 项目补了 D-042 分叉。

## 截图（390×844，fake node，Night Corral）

| 状态 | 文件 |
|---|---|
| 收起态：单行 `询问 · ?` 触发器 + context 环 + 发送；输入框 placeholder 无桌面快捷键 | [ux2026-composer-1-collapsed-390.png](./ux2026-composer-1-collapsed-390.png) |
| 选项 sheet：附件 / 载体 / 权限选择器 / effort 滑杆 | [ux2026-composer-1-sheet-390.png](./ux2026-composer-1-sheet-390.png) |
| 插队 Sheet 确认（取消 / 打断并发送，danger） | [ux2026-composer-1-steer-confirm-390.png](./ux2026-composer-1-steer-confirm-390.png) |
| bypass 启动：收起触发器直接 danger 样式显示「绕过全部」；打断/排队三态在 sheet 外 | [ux2026-composer-1-danger-trigger-390.png](./ux2026-composer-1-danger-trigger-390.png) |

> 注：新会话首次 effort 未回读时触发器档位显示 `?`（与桌面 chip 完全一致的诚实口径，`isEffortUnknown`），回读后显示真实档名。

## e2e 断言摘要（`ux-composer-mobile.hub.spec.ts`，3 case）

1. 收起条 `data-collapsed="1"`；触发器带 `data-permission=manual`、`data-permission-danger=0`，权限词与档名同在；sheet 内含附件/harness/权限/effort，三态与 cap-note count=0；排队后 `composer-queue-status` 与逐行 chip 仍在 sheet 外。
2. 插队：Sheet 取消时草稿保留、无 `mode=steer` POST；确认后 fake-node 收到 `instance.send mode=steer` 且出「已打断」。Esc 打断：Sheet 内 Esc=取消（无 `instance.cancel`），确认后 fake-node 收到 `instance.cancel`；全程 `page.on("dialog")` 哨兵为空。
3. bypass 启动（New Session 勾选「绕过全部」+ yolo ack）：未打开 sheet 时触发器即为 `data-permission=bypassPermissions` / `data-permission-danger=1` / 文案「绕过全部」。

## 兼容性迁移

旧的 `page.once("dialog", …)` 原生确认在三处 hub spec 改为点 Sheet 的 `composer-confirm-ok`：`ux-steer.hub.spec.ts`（按钮 + ⌃Enter 两处）、`hub-live.spec.ts`（插队 + Esc 打断）、`promoted-claude.hub.spec.ts`（可见「打断」按钮本就不经 confirm，删除冗余 dialog 监听）。可见「打断」按钮始终是直达动作，只有插队与 Esc 走确认——与改动前语义一致。
