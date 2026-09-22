# UI 升级 · 任务 14（c-ltboot）：启动时应用主题选择，首帧生效

2026-09-22 · `wt/c-ltboot/b-ltboot-md` · D-053（浅色主题纳入本轮迭代）中最小、最独立的一块；修一个真实缺陷。

## 1. 缺陷

唯一写 `document.documentElement.dataset.theme` 的地方是 `SettingsPage.tsx` 的
`useEffect`，只在设置页挂载时执行。于是：选了浅色 → 刷新 → 只要不再打开设置页，
**任何其它路由仍是深色**。选择写进了 `localStorage`，但开机时没人读它。

另有一处埋雷：`features/settings/prefs.ts` 的 `DeviceSettings.theme` 把每次读写
硬性强制回固定的旧值，选择器只能另起一个 `runtime.theme.v1` 键绕开它——两套存储并存。

## 2. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/features/settings/theme.ts` | 新增（单一来源）：`THEME_KEY` / `readTheme` / `applyTheme` / `writeTheme` / `ThemeChoice` |
| `web/src/features/settings/theme.test.ts` | 新增 vitest：缺失 / 合法 / 非法存储值三个分支，外加 storage 抛异常、幂等、写失败回滚 |
| `web/src/main.tsx` | React 挂根前执行一次 `applyTheme(readTheme())` |
| `web/src/pages/SettingsPage.tsx` | 删除本地主题副本，改 import 共享模块；保留挂载 effect 并注释说明启动应用之后它只是幂等修复 |
| `web/src/features/settings/prefs.ts` | 删除 `DeviceSettings.theme` 字段及其全部强制回落（类型、默认值、读、写） |
| `web/src/features/settings/prefs.test.ts` | 去掉对已删字段的断言，新增旧 blob 遗留字段在读/写后被清除的迁移用例 |
| `web/src/features/settings/index.ts` | barrel 导出主题 API |
| `web/tests/e2e/theme-boot.hub.spec.ts` | 新增 hub 规格：四条路由 reload 后首帧 `data-theme` 断言（纯前端，无 `binariesReady` 门） |
| 本文 + 两张 PNG | 证据 |

未触碰：任何 `.css` / `.module.css`（任务 17 独占）、`tty/theme.ts`（任务 18 独占）、
任何 wire/协议文件。回滚单元：本任务为单个 commit，可独立 `git revert`。

## 3. 单一来源与默认值

- `runtime.theme.v1` 的赋值（`setItem`）只存在于 `features/settings/theme.ts` 一处。
- `DeviceSettings.theme` 被**整体删除**而不是保留一个不起作用的字段；旧存储 blob 里
  的遗留键在下次 `writeDeviceSettings` 时自然清除（有单测锁定）。
- 无 storage、`getItem` 抛异常、或值不在已知集合内，`readTheme()` 一律回落 `"night"`。
- `index.html` 仍内联静态 `data-theme="night"` 作为脚本执行前的兜底，防首帧闪烁。

## 4. “首帧”是怎么测的

`theme-boot.hub.spec.ts` 用 `addInitScript`（在任何页面脚本之前）做两件事：

1. 预置 `localStorage["runtime.theme.v1"] = "ledger"`；
2. 从 `<html>` 元素出现起挂 `MutationObserver` 记录每次 `data-theme` 变化，并逐帧
   轮询 `#root` 何时第一次出现 React 提交的内容，记录该帧将绘制的值
   （`app-first-frame`）。

对 `/`、`/sessions`、`/approvals`、`/m` 逐条全新文档加载（非客户端导航）断言：

- 页面脚本运行前属性是解析器给的 `night`（基线，排除“默认恰好相等”）；
- 存在一次 `night → ledger` 变化，且其序位严格早于 `app-first-frame`；
- 加载完成后属性仍是 `ledger`；
- 全程不进入 `/settings`，设置页组件计数为 0。

反向用例：非法值（`"midnight"`）时从不出现 ledger 变化，首帧与稳定后都是 `night`
（该用例无需登录，落在 /login 也无所谓——主题在路由与鉴权门之前已应用）。

> 注：dev server 下 deferred module 与 `DOMContentLoaded` 的先后不稳定，规格没有
> 采用 DCL 采样，而是直接比较“属性变化序位 vs 首个含应用内容的绘制帧”，语义更贴近
> 验收要求的“首帧成立”。另外该注入时机下 `document.documentElement` 可能尚为
> `null`，探针在首帧 rAF 内重试挂载（开发过程中实测到的坑）。

## 5. 证据截图

`REMUDA_EVIDENCE=1` 时由同规格的 evidence 用例产出，只截 Remuda 自身渲染、
仅 390 与 1440 两个宽度；默认运行跳过，不产生 PNG（闸门脏树检查安全）：

- `ui-upgrade-14-sessions-1440.png`：预置 ledger 后全新加载 `/sessions`，1440 桌面工作区首帧即浅色。
- `ui-upgrade-14-home-390.png`：390 手机宽度全新加载 `/m`，手机首页首帧即浅色。

## 6. 验证记录

- `pnpm --dir web test`：157 文件 / 1605 用例全绿（含新增 theme/prefs 用例）。
- `pnpm --dir web typecheck`：通过。
- `pnpm --dir web lint`：通过（exit 0；剩余 warning 均为既有文件）。
- `theme-boot.hub.spec.ts`：在派工端口与锁槽下通过（见下方运行记录）。
- mock 回归：`ux-settings.spec.ts`（含主题保存/失败回滚/reload 保持用例）单独跑全绿。

<!-- RUNLOG -->
- 2026-09-22，派工端口 59260/59269/59261 + 锁槽 `locks/e2e.lock-ltboot`，bundled Chromium（`PW_CHANNEL=chromium`）：
  - 默认运行：`theme-boot.hub.spec.ts` **2 passed / 1 skipped**（28.5s；skipped 为 evidence 截图用例），不产生 PNG。
  - `REMUDA_EVIDENCE=1`：**3 passed**（31.2s），产出上方两张 PNG（390×844、1440×900）。
- 全量 web hub e2e 套件（派工端口/锁槽，bundled Chromium，1 worker）：**180 passed / 1 failed（28.6m）**。
  唯一失败是 `ux-files.hub.spec.ts` 的 “row 9: content changed … manual refresh”：
  假节点全程刷 `node did not durably accept command`，90 s 内按钮不可点击。该用例与主题零耦合
  （spec 无 theme 引用，按钮与已可见的提示文案在同一个 JSX 条件块内），且在**干净 main
  `be3c48d8` 检出**（本任务文件一个都不存在）、同机同端口同锁槽下单独重跑**同样失败**
  ——判定为既有的假节点时序 flake，非本改动引入；闸门自带 1 次重试通常可吸收。

