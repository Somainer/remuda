# 手机优先 UI · 任务 2：`/m` 路由树、PhoneShell、重定向层、manifest

2026-09-19 · `wt/c-mshell/b-mshell-md` · mobile-ui 实施计划 §(C) 任务 2（c-mshell）
规格权威：[ui-spec.md](../ui-spec.md) §1.2 / §1.3 / §4.7，[decisions.md](../decisions.md) D-049；基线 `origin/main` @ 83ad6d8e（c-sessionchrome 已落地）。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/lib/mobileRoute.ts` | 新增。纯函数 `resolveLanding(pathname, search, compact)`：重定向层的唯一决策点 |
| `web/src/lib/mobileRoute.test.ts` | 新增。落点解析、query 原样保留、`/s/:id*` 两侧不改写、桌面反向重定向 |
| `web/src/lib/nav.ts` | 新增 `PHONE_NAV`（会话 / 收件箱(n) / 新建 / 更多）；`PRIMARY_NAV` / `MORE_NAV` 不动 |
| `web/src/app/PhoneShell.tsx`、`phoneShell.module.css` | 新增。compact 专用手机壳：一条内容列 + 一条底栏 |
| `web/src/app/router.tsx` | 新增 `/m`、`/m/inbox` 两条路由与 `ViewportGate`（`resolveLanding` + `<Navigate replace>`）；`/` 改为按视口落点。桌面路由零改动 |
| `web/public/manifest.webmanifest` | `start_url: /sessions` → `/`；SW 未改（`sw.src.js` 的 `SHELL` 已含 `/` 与 `/index.html`） |
| `web/tests/e2e/m-shell.hub.spec.ts` | 新增 hub 规格（390 与 1440） |
| `web/tests/e2e/hub-auth.ts`、`web/tests/e2e/ux-question.hub.spec.ts` | 共享 `login()` 兼容 compact 落 `/m`；审批证据用例在拉宽回桌面后重新进入 `/approvals` |
| `docs/design/evidence/mobile-ui-2.md` + 两张 PNG | 本文件与证据截图 |

未触碰：`Shell.tsx` / `Shell.module.css`（任务 5 拥有）、`SessionPage.tsx`、`Composer.tsx`、`ToolCard.tsx`、`Transcript.tsx`、`session.module.css`、`SessionList.tsx`、`ApprovalsPage.tsx`、`ui-spec.md`、`playwright.hub.config.ts`、`sw.src.js`。

## 2. 重定向层（验收 1 / D-049 §1.2）

决策全部在纯函数里，路由侧只做声明：

```
compact:  /sessions          → /m<search>
          /approvals<search> → /m/inbox<search>   （search 逐字节保留，不 parse、不重建）
desktop:  /m 与 /m/*         → /sessions
两侧都不重定向：/s/:id*（含 /tty /structured /files /events /agents/…）、
              /sessions/new、/login、/pair、/settings
start_url "/"：compact → /m，桌面 → /sessions
```

- **query 保留**：`resolveLanding` 直接拼接 `location.search`；vitest 覆盖 `?focus=abc&kind=approval`、percent-encoding、排序不变（计划风险 E8）。
- **深链**：推送/飞书的 `/approvals?focus=…` 在 compact 落 `/m/inbox?focus=…`，e2e 用真实 id 走了一遍；`/s/:id` 在两套壳下是同一条路由，永不重定向。
- 共享路由用 `<Navigate replace>`（与规格一致），视口实时变化时 `useWorkbenchViewport` 的 media-query 监听会驱动重渲染，拉宽窗口即从 `/m*` 弹回 `/sessions`（1440 e2e 同款路径）。

## 3. PhoneShell 与底栏（验收 2 / 3）

- **底栏四项**：会话 · 收件箱(n) · 新建 · 更多，`nav[aria-label="手机底栏"]`，只挂在 `/m*` 外壳；共享的 `/s/:id*` 仍在 `Shell` 下渲染（任务 5 负责把 Shell 的底栏从会话路由摘掉，本任务不碰）。
- **热区**：每项 `min-width/min-height: var(--touch)`（44px），底栏 `min-height: calc(var(--bar) + var(--safe-bottom))` 且 `padding-bottom: var(--safe-bottom)`。e2e 以几何（`getBoundingClientRect`，宽高均 ≥ 44、整体在 390 视口内）为契约。
- **badge**：`hub.interactions.filter(i => i.state === "pending").length`，与 `Shell.tsx:176` 同一口径；角标 testid `phone-inbox-badge`，假节点新建会话产生 1 条 pending approval，e2e 断言为 1。
- **visibilitychange 等价行为**（验收 3）：PhoneShell 复刻 Shell 的 effect——回前台时 `/s/:id` 走 `hubStore.catchup(id)`，其余（含 `/m`、`/m/inbox`）走 `hubStore.refresh()`；e2e 在 `/m` 上派发 `visibilitychange` 并断言随后发生 `/v1/instances` 拉取。legacy `hubStore.toast` 桥接到 `toastAdapter` 的行为也一并复刻，通知面（`ShellNotify`、`InstallBar`）复用同一组件，不复制。
- 视口高度一律走 `useWorkbenchViewport`（E1），壳自身不做 innerHeight 计算。
- **新建**：在 `/m` 首页带当前 space（`workbench.newHref`，与桌面侧栏 ＋ 一致），其他位置走裸 `/sessions/new`。
- **更多**：打开与 Shell 同款的 `MORE_NAV` 菜单（主机 / 项目 / Provider / Bot / 设置），导航后自动收起。

## 4. manifest / SW（验收 4）

`start_url: "/"`。`web/sw.src.js` 的 `SHELL = ["/", "/index.html", "/manifest.webmanifest", …]` 已预缓存 `/`，首屏命中不变，SW 零改动。只有一份 manifest、一份 SW。

## 5. 过渡期信息架构（验收 5，显式说明）

任务 3（m-home）与任务 4（m-inbox）落地前：

- `/m` 在 PhoneShell 内渲染**既有 `SessionsPage` 列表**（含与今天 compact `/sessions` 相同的 spaces chips 横条与 SpaceTabs，复用 `SpacesMobile` / `SpaceTabs`，未复制）；
- `/m/inbox` 渲染**既有 `ApprovalsPage` 全部内容**（kind 分段、host/workspace 筛选、审批/提问卡）。

即「旧列表换了新壳」，**没有新的信息架构**：无分组、无 context 环、无两档收件箱、无权限横幅。手机首页搜索框顶栏与 Jump To 入口同样属任务 3/7，本任务不含。

## 6. 截图

合成数据：fake node（`e2e-fake-node`，工作区均为 `/tmp/remuda-e2e*` 夹具路径），无个人路径、用户名或主机名。Night Corral 主题，`prefers-reduced-motion`。

| 390×844 compact | 1440×900 desktop |
|---|---|
| [mobile-ui-2-home-390.png](./mobile-ui-2-home-390.png)：登录后落 `/m`，四项底栏，收件箱(1) 角标（1 条 pending approval），内容为既有会话列表 | [mobile-ui-2-desktop-1440.png](./mobile-ui-2-desktop-1440.png)：同一份合成数据，桌面仍是 `/sessions` 与桌面侧栏（零改动） |

## 7. 测试与验证

- `pnpm --dir web test`：vitest 全绿（131 文件 / 1241 用例，含新增 `mobileRoute.test.ts`）。
- `pnpm --dir web typecheck`、`pnpm --dir web lint`：通过（lint 的 warning 均为既有文件、非本任务文件）。
- 新 hub 规格连跑 **3 次全绿**（显式命令行，锁槽 + 指定端口，bundled chromium 因该机无 Google Chrome；`$E2E_LOCK` 是本批派工约定的共享 `e2e.lock` 槽路径，不在此写死本机绝对路径）：

```bash
flock "$E2E_LOCK" bash -c '
  export HUB_E2E_LISTEN=127.0.0.1:59160 HUB_E2E_WEB_PORT=59169 \
         HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59161 VITE_NO_WATCH=1 PW_CHANNEL=chromium
  ./node_modules/.bin/playwright test -c playwright.hub.config.ts m-shell.hub.spec.ts'
```

- **完整 web hub e2e 套件跑了 1 次**（闸口只跑 hub 配置，且 `web/src/app/**` 不在自动开闸清单内——计划 (C) 公共约定 E4，合并时需显式 `--web-e2e`）：

```bash
flock "$E2E_LOCK" bash -c '
  export HUB_E2E_LISTEN=127.0.0.1:59160 HUB_E2E_WEB_PORT=59169 \
         HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59161 VITE_NO_WATCH=1 PW_CHANNEL=chromium
  ./node_modules/.bin/playwright test -c playwright.hub.config.ts'
```

结果：**129 passed / 18 skipped / 2 failed**，失败两条见 §8（本机缺 CJK 字体导致的环境问题，与本 diff 无关；补字体后两条规格各自全绿）。

- `bash scripts/ci/secret-scan.sh`、`bash scripts/ci/no-tunnel-scan.sh`：通过。
- 闸口几何与 DOM 断言均在 chromium hub 规格内（390×844，hasTouch/isMobile 模拟手机）；webkit iPhone 项目不进闸（E3），本任务未声称真机/iOS 验证。

合并开闸命令（显式 `--web-e2e`）：`./scripts/ci/gate.sh --web --web-e2e`（web 步骤受影响文件清单不含 `web/src/app/**`，不显式开则 hub e2e 被跳过）。

## 8. 运行记录

完整 hub 套件（`playwright test -c playwright.hub.config.ts`，1 worker，153 用例，2026-09-19，端口 59160/59169/59161 + `$E2E_LOCK` 槽，bundled chromium / `PW_CHANNEL=chromium`）：

- **129 passed，18 skipped（REMUDA_EVIDENCE 未置位的证据用例），2 failed，4 did not run（serial 连带跳过）。**
- 两条失败：`ux-chrome.hub.spec.ts:225`（390px 热区几何）与 `ux-touchhit.hub.spec.ts:251`（390px header 44px 角点），报错同为 `seg-tty 的 44px 热区右上角解析到 seg-structured`。两者都在共享的 `/s/:id` 路由上断言 `ViewSwitch`，**本任务 diff 不触碰该路由、`SessionPage`、`session.module.css` 或 `ViewSwitch`**。
- **根因 = 运行环境缺 CJK 字形**：该机字体目录只有 DejaVu（22 个 face，无任何 CJK 字体），`终端`/`结构` 回退为等宽 `.notdef`，两个字形的视觉宽度从设计假设的 ≥43.5px 缩到约 24px，使 30px 段的相邻 44px `::after` 热区必然重叠——探针因此在两条段之间解析到邻居。几何探针是按字形渲染宽度设计的（`session.module.css:2731` 的 640px 规则注释明写「Keep each segment >=44px visual」）。
- **验证**：在用户字体目录放入一个 CJK 字体（不改动仓库与系统 fontconfig）后单跑这两条规格：**6 passed**（ux-chrome 2 + ux-touchhit 4，全绿）。截图（§6）在同一字体环境下拍摄，字形正常。按协调人指示，闸机主机缺 CJK 字形不属本任务缺陷，不在代码中处理；合并闸机若同样缺字体，需要在主机字体层补齐 CJK face（与本代码无关）。
- 除上述两条环境性失败外，其余 129 条全绿，包括全部 compact 会话路由规格（ux-composer-mobile / ux-files / ux-keys / ux-code / ux-usage / ux-toolfold / ux-wfdrill / ux-workflow-card / ux-attach-files / ux-live-view / ux-livephrase / ux-question 等）与本任务 m-shell 规格。

## 9. 对既有规格的兼容性说明

- `hub-auth.ts` 的 `login()` 是全部 hub 规格共用助手：登录后落点正则从 `/sessions` 放宽为 `/(sessions|m)`，随后仍断言 `session-list` 可见（两个壳下都是同一个 `SessionList`）。桌面规格行为不变。
- `ux-question.hub.spec.ts` 的证据用例在 390 帧会把 `/approvals` 带到 `/m/inbox`，拉宽回 1440 后 `/m/inbox` 又弹回 `/sessions`；该用例现在在桌面宽度重新 `goto("/approvals")` 再操作卡片，这是 D-049 重定向的预期交互，不是被测功能变化。
- 其余 compact hub 规格（ux-touchhit / ux-keys / ux-chrome / ux-composer-mobile / ux-files / ux-code / ux-usage 等）的会话内断言全部在共享的 `/s/:id*` 路由上，该路由两侧都不重定向；完整套件回归通过（§7）。
