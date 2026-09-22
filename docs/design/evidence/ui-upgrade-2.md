# UI 升级 · 任务 2（c-tokens）：`--warn` / `--info` / `--text-xl` / `--text-13` 落地

2026-09-23 · `wt/c-tokens/b-tokens-md` · ui-upgrade 任务 2（计划 §B.1 / (C) 任务 2）。
让两个「幽灵 token」不再靠 fallback 渲染，给 18px/13px 正式名。**只加定义 + 只改被点名站点**，
不做全库转换。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/styles/tokens.css` | night/ledger 各加 `--warn`、`--info`；type scale 加 `--text-xl: 18px`、`--text-13: 13px` |
| `web/src/features/session/session.module.css` | `:1003` 琥珀色幽灵回退（`var(<warn-ghost>, #ffc107)`）→`var(--warn)`；`:1016` 天蓝色幽灵回退（fallback `#57b7ff`）→`var(--info)`；`:1612` `font-size: 18px`→`var(--text-xl)` |
| `web/src/features/session/notifications/notifications.module.css` | `:72` `var(--text-body, 13px)`→`var(--text-body)`（删掉从未成立的 13px 回退） |
| 9 处 18px 站点（8 个文件） | `font-size: 18px`→`var(--text-xl)`：bots `:152`、providers `:560`、hosts `:418,:476`、SessionList `:808`、ApprovalsPage `:332`、mobile/inbox `:118`、session `:1612`、LoginPage `:26` |
| `web/tests/e2e/ui-tokens-evidence.hub.spec.ts` | 新增证据规格（`REMUDA_EVIDENCE=1` 才截图，默认全 skip、零 PNG） |
| 本文 + 8 张 PNG | 证据 |

未触碰：feature 层 13px 站点（本任务只定义 `--text-13`）；ledger 失败色 / xterm（任务 17/18）；
`features/settings/theme.ts`（c-ltboot 独占）。回滚单元：tokens.css 与被点名站点可独立 revert。

## 2. Token 取值与设计理由

| token | night | ledger |
|---|---|---|
| `--warn` | `#ffc107` | `#7a5410` |
| `--info` | `#57b7ff` | `#1f5575` |

- **night 取原 fallback 的同值**：这两个状态色在 night 一直以「未定义 token + fallback hex」
  实际渲染（琥珀色 `#ffc107` / 天蓝色 `#57b7ff`）。定义成同一 hex 后，night 被改站点
  **渲染字节级不变**（同色同值，不是「接近」）；本任务的视觉变化只发生在 ledger。
- **ledger 取加深版暖/冷色**：`#ffc107` / `#57b7ff` 在 `--ink-2: #faf8f3` 浅面上分别只有
  1.54:1 / 2.06:1（计划审计值），降到深棕 `#7a5410` / 深蓝 `#1f5575` 后双双过 AA。
- `--text-xl` / `--text-13` 定义在共用的 `:root` type scale，两主题同值；13px 仅定义，
  全库 13px 站点按计划留给后续批次迁移。

## 3. 对比度计算（WCAG 2.1，对 `--ink-2`，两主题均 ≥ 4.5:1）

相对亮度 `L = 0.2126·R' + 0.7152·G' + 0.0722·B'`（sRGB 线性化，
`c ≤ 0.03928 → c/12.92`，否则 `((c+0.055)/1.055)^2.4`）；
对比度 `(L_lighter + 0.05) / (L_darker + 0.05)`。

用法覆盖：`:1003` `.chipEffortMismatch`（mismatch 行，`--warn`）与
`:1016` `.chipEffortPendingTag`（排队中/切换中 标签，`--info`）都是 11px 状态文本，
按本任务契约对各主题 `--ink-2` 计算。`--ink-2` 是两主题中**最浅的容器面**
（night `#1a212b`、ledger `#faf8f3`，均浅于 canvas/ink-0/ink-1），故对它的比值是
这两个状态色在本主题内的**下界**，落在更深面上只会更高。

| 主题 | token（hex, L） | 底色 `--ink-2`（L） | 比值 | AA 4.5:1 |
|---|---|---|---|---|
| night | `--warn` `#ffc107`（0.5942） | `#1a212b`（0.0148） | **9.94:1** | ✅ |
| night | `--info` `#57b7ff`（0.4311） | `#1a212b`（0.0148） | **7.42:1** | ✅ |
| ledger | `--warn` `#7a5410`（0.1052） | `#faf8f3`（0.9393） | **6.38:1** | ✅ |
| ledger | `--info` `#1f5575`（0.0807） | `#faf8f3`（0.9393） | **7.57:1** | ✅ |

对照旧 ledger fallback：`#ffc107` 1.54:1 → 6.38:1；`#57b7ff` 2.06:1 → 7.57:1。

## 4. 验收反向断言（命令与结果）

```
# 模式按 shell 引号拆写显示（两段拼接即原词），避免本文自身成为命中点
$ git grep -n -e '--am''ber'          # 零命中（exit 1，源码与文档均无该幽灵名）
$ git grep -n -F 'var(--in''fo,'      # 零命中（exit 1）
$ grep -rn "font-size: *18px" web/src --include="*.css"   # 零命中
$ grep -rn "var(--text-xl)" web/src --include="*.css" | wc -l
9                                                     # 恰好 9 处消费
$ grep -n -- "--warn:\|--info:\|--text-xl:\|--text-13:" web/src/styles/tokens.css
25:  --warn: #ffc107;  26:  --info: #57b7ff;          # night
51:  --warn: #7a5410;  52:  --info: #1f5575;          # ledger
64:  --text-xl: 18px;  66:  --text-13: 13px;
```

`git diff` 仅含 tokens.css、被点名的 9 个 18px 站点、notifications.module.css，
外加证据规格与本文/PNG。

## 5. 证据截图

四组矩阵（390 / 1440 × night / ledger），每组分 warn / info 两帧（两个状态在 UI 上
时间互斥：排队中标签会在下一次 effort 回读时结算并让回 mismatch 行，无法同框）。
由 `ui-tokens-evidence.hub.spec.ts` 在假节点会话上驱动：选 max 被假节点钳到 xhigh →
mismatch 行（`--warn`）；再发 `__queued__:xhigh` sentinel → 「排队中」标签（`--info`）。
`REMUDA_EVIDENCE=1` 才写入本目录，默认运行 4 skipped、不改写任何 PNG（闸门脏树安全）。

**warn（请求 max → 实际 xhigh）**

- `ui-upgrade-2-warn-night-390.png` / `ui-upgrade-2-warn-night-1440.png`：night 暖黄，与改前 fallback 同值。
- `ui-upgrade-2-warn-ledger-390.png` / `ui-upgrade-2-warn-ledger-1440.png`：ledger 深棕，1.54:1 → 6.38:1。

**info（排队中）**

- `ui-upgrade-2-info-night-390.png` / `ui-upgrade-2-info-night-1440.png`：night 冷蓝，与改前 fallback 同值。
- `ui-upgrade-2-info-ledger-390.png` / `ui-upgrade-2-info-ledger-1440.png`：ledger 深蓝，2.06:1 → 7.57:1。

所有截图只含 Remuda 自身渲染；`--text-xl` 为纯机械替换（计算值仍 18px），
9 处站点无视觉变化，不单独截图。

## 6. 验证记录

- `pnpm --dir web test`：157 文件 / 1605 用例全绿（无新增运行时逻辑）。
- `pnpm --dir web typecheck`：通过。
- `pnpm --dir web lint`：exit 0（剩余 warning 均为既有文件）。
- hub e2e `effort-sync.hub.spec.ts`（直接覆盖这两枚 chip 的全部状态）：6 passed。

<!-- RUNLOG -->
- 2026-09-23，派工端口 59320/59329 + 共享锁槽 `locks/e2e.lock-tokens`（worktree 外的跨库 flock），bundled Chromium（`PW_CHANNEL=chromium`）：
  - 默认运行 `ui-tokens-evidence.hub.spec.ts`：**4 skipped**，不产生 PNG。
  - `REMUDA_EVIDENCE=1`：**4 passed**（约 46s），产出上述 8 张 PNG（390×844 / 1440×900 视口的 composer 条裁切）。
  - 同锁下 `effort-sync.hub.spec.ts` + 证据规格默认模式：**6 passed / 4 skipped**（约 1m）。
