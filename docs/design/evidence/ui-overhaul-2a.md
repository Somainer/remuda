# UI 改版 UO-2a：桌面布局骨架（2026-09-24）

工作树 `wt/c-uo2a/b-uo2a-md`。本轮做布局骨架：
- 侧栏：主导航、项目区、底部“新建会话 / 管理”，可用 ⌘/Ctrl+B 折叠为图标栏；
- PageHeader；
- `/sessions` 的索引列；
- 手机底栏 PhoneNav；
- 工作台快捷键 `useWorkbenchKeys`。

Tabs 跟随 Task 属于 UO-2b，不在本轮范围。

## 1. 复核命令

```
pnpm --dir web typecheck
pnpm --dir web lint
pnpm --dir web test -- --run
# 验收
pnpm --dir web exec playwright test session-structured ux-filters
pnpm --dir web exec playwright test -c playwright.hub.config.ts ux-keys ux-quickfind
# 截图（写入 ui-overhaul/，只存 390 与 1440）
REMUDA_EVIDENCE=1 pnpm --dir web exec playwright test -c playwright.hub.config.ts uo2a-evidence
# 性能
HUB_E2E_PERF=1 pnpm --dir web exec playwright test -c playwright.perf.config.ts --project=chromium
```

## 2. 截图

所有截图在 `ui-overhaul/` 下，命名为 `UO-2a-<surface>-<mode>-<width>.png`，共 16 张。
由 `web/tests/e2e/uo2a-evidence.hub.spec.ts` 在假 Node 上生成。

| surface | 宽度 | 内容 |
| --- | --- | --- |
| sessions | 390 / 1440 | `/sessions`。1440 为侧栏 + 索引列 + 列表；390 显示手机底栏（断言可见） |
| session | 390 / 1440 | `/s/:id`（claude-print 会话，停在待批准卡）。390 断言没有 `nav[aria-label="手机底栏"]` |
| approvals | 390 / 1440 | `/approvals` |
| admin-menu | 1440 | 展开的“管理”菜单，断言其中“集群”链接指向 `/fleet` |
| collapsed | 1440 | 折叠后的侧栏（`data-collapsed="true"`） |

## 3. 验收逐项

| 项 | 证据 | 结果 |
| --- | --- | --- |
| `ux-keys.hub` 未改动通过 | 整文件单跑 5/5 | 通过 |
| `ux-quickfind` | 与 `ux-keys` 同批 8 过 1 失败，失败项为 `ux-keys:360`（见 §4） | 通过 |
| `session-structured.spec:19`、`ux-filters.spec:96` 未改动通过 | `:19` 是第 8 行用例里的断言（主导航或手机底栏可见）；`:96` 是 `selectSpace` 里点“主导航”首个链接的一步。chromium 下两者所在用例全部通过。两文件其余失败项在 main 上同样失败，`ux-filters:288` 见 UO-1 §4 | 通过 |
| `/fleet` 可从“管理”菜单进入 | `uo2a-evidence` 1440 断言菜单项 href；`Shell.test.tsx` | 通过 |
| 390 下 `/s/:id` 无手机底栏 | `uo2a-evidence` 390 断言计数为 0 | 通过 |
| 1440 / 768 无横向滚动的工具栏 | 在 `/sessions`、会话页、`/approvals`、`/board`、`/settings`、`/hosts` 的深浅两种模式下，检查所有 header、nav、toolbar、tablist 类元素。`overflow-x` 为 auto/scroll 且内容溢出的只有会话页的 Space 标签条（`features/spaces`，打开的会话标签多于宽度），它是标签条，不是工具栏，不在本轮文件内，归 UO-2b。文档本身无横向溢出 | 通过（附注） |
| 深 / 浅 | §2 截图 | 通过 |
| 性能：hub tick 上的 `commit:Shell` 不增加 | §5 | 通过 |

## 4. 已知失败

以下各项在 main（44ed3cd9）上用同样命令都复现过，不是本轮引入：
- `ux-code.hub.spec.ts:194`
- `ux-files.hub.spec.ts:175`
- `turn-end.hub.spec.ts:271`
- `ux-modelpick.hub.spec.ts:114`：在 `ux-mobile-new` 之后跑会失败，单跑通过
- mobile-webkit 下 `mobile-qa` 的两条 `boundingBox` 用例，两侧都会偶发
- `EffortSlider` 单测一条

`ux-keys.hub.spec.ts:360` 在分批时偶发，整文件单跑通过。原因：该用例在 `beforeEach` 已登录后再次调用 `login()`，已登录时 `/login` 会重定向，导致 `login-submit` 在点击前脱离 DOM。

## 5. 性能（chromium，`commit:Shell` 的 React Profiler 计数与总时长）

| 场景 | main | HEAD |
| --- | --- | --- |
| A 长 transcript 流式 + 滚动 | 1561 次 / 5547ms | 1495 次 / 5653ms |
| C 100 条待处理交互 + 收件箱滚动 | 14 次 / 52ms | 15、14、13 次 / 74、67、61ms |

提交次数没有增加。C 的总时长略高，main 只有一次采样；新侧栏多渲染项目区，单次提交稍重。
