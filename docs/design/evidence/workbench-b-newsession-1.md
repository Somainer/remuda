# Workbench batch B · New Session：来源返回、草稿隔离、首层词汇、幂等提交、390px 软键盘

- 日期：2026-09-14
- 状态：已实施（合成 fixture 证据；hub-live e2e 使用 fake Hub + fake Node）
- 对应需求：[workbench-ux-exploration.md](../workbench-ux-exploration.md) §5 P0-2、§7 验收；[workbench-ux-plan.md](../workbench-ux-plan.md) §1 行 B、§2 overlay 契约、§4 风险 3
- 实施：`web/src/lib/newSessionDraft.ts`、`web/src/pages/NewSessionPage.tsx`、`NewSessionPage.module.css`、`web/tests/e2e/new-session.spec.ts`

## 1 · 改了什么

1. **来源返回。** 新建页记住它是从哪里打开的：从会话页、会话列表或主机页在应用内打开时，Escape / ✕ / 取消都 `navigate(-1)` 回到原处，而不是固定回 `/sessions`。直接打开 `/sessions/new` 深链时回退到该空间的列表，并先选中对应空间。
2. **共享弹层契约。** 自有的遮罩+form 删除，整页消费批次 A 的 `components/Sheet.tsx` + `useFocusTrap`：`role="dialog"`、`aria-modal`、初始焦点在提示词、Tab 圈定、Escape、关闭后焦点回到触发器。桌面为居中对话框，手机为底部 sheet。
3. **草稿。** 独立版本键 `runtime.draft.new.v1.<authSubject>.<hostId>.<workspaceId>`：
   - `authSubject` 是 Hub 颁发的 device id；拿不到可靠登录身份时草稿只活在当前 tab 的内存里，**绝不写 localStorage**；
   - 主体 + 主机 + 工作目录三段隔离，换账户或换目录不串草稿；
   - 只存正文与非敏感选项（kind / model / 权限 / 模型来源 / 目录模式 / 子路径 / worktree 名 / effort），allowlist 在写入和读取两侧同时生效；认证值、settings overlay 路径、`CLAUDE_CONFIG_DIR`、预算、自定义可执行文件、argv、临时文件内容、附件字节一律不落盘；
   - 不触碰 Composer 在用的 `runtime.draft.<instanceId>` 命名空间，回退不破坏既有草稿；
   - Escape / 取消保留可恢复草稿；显式「丢弃草稿」才清除；创建确认后草稿随会话"花掉"。
4. **首层用户词汇。** 首层是「要做什么 / 工作目录 / 执行 agent / 权限 / 模型来源」。承载方式（driver）矩阵、launch prefill、settings overlay、config dir、预算、特殊参数、可执行文件全部收进「高级设置」。终端本身是技术表面，其唯一承载 `shell-pty` 在首层如实命名（外部终端 e2e 依赖该 testid）。effort 滑块布局 A 视觉不变，仅把 helper 里的实现名 `InstanceSpec` 换成用户说法「会话开始后仍可在会话内调整」。
5. **幂等提交。** 每次提交生成客户端请求 id（`creq_…`，Crypto UUID，降级时随机串），随表单展示：
   - 明确拒绝（4xx，除 408）：错误留在发生位置，输入与草稿保留，可修正后再提交一次；
   - **ACK 未知**（断连 / 超时 / 5xx）：请求可能已在主机创建会话，页面进入常驻 `状态待确认`（P0-3 词汇，本批 C1 的 commandStatus 尚未在本分支，故使用本地常量），主按钮禁用、不自动重发、不猜测实例归属，只提供「刷新状态」（只读 `instanceList` 调和）与「返回列表」；
   - 重复 requestSubmit 由 ref 门禁拦截，同一次尝试不可能产生第二个 POST。
6. **纯键盘。** 初始焦点在提示词；Tab 在弹层内循环；最后一站是「开始」，Enter 提交；滑块本就支持 ←/→/Home/End。hub-live e2e 用键盘完成整次创建。
7. **390px 软键盘。** sheet 与 footer 使用 visual viewport 高度（`--workbench-height`，viewport.ts 已随软键盘更新）；底部 sheet 固定在可视区底边，键盘弹起时 footer 连同主操作上移，不被遮挡。

## 2 · 截图（合成 fixture）

`VITE_MOCK=1` 的示例主机/目录/会话数据；Playwright chromium，`animations: "disabled"`；无个人路径、无主机名（fixture 自带的 `devbox-sg`/`sfe-root` 是 mock 数据）。

### 首层：要做什么 · 工作目录 · 执行 agent · 权限 · effort · 模型来源

| | night | ledger |
| --- | --- | --- |
| 1440 | [workbench-b-newsession-layer1-night-1440.png](./workbench-b-newsession-layer1-night-1440.png) | [workbench-b-newsession-layer1-ledger-1440.png](./workbench-b-newsession-layer1-ledger-1440.png) |
| 390 | [workbench-b-newsession-layer1-night-390.png](./workbench-b-newsession-layer1-night-390.png) | [workbench-b-newsession-layer1-ledger-390.png](./workbench-b-newsession-layer1-ledger-390.png) |

首层看不到 driver / shell-pty / carrier / InstanceSpec 等实现词；effort 仍是无边框 form field，与权限行同列等宽（composer-slider-4 布局 A）。

### 高级设置：承载方式矩阵与启动覆写在折叠区内

| | night | ledger |
| --- | --- | --- |
| 1440 | [workbench-b-newsession-advanced-night-1440.png](./workbench-b-newsession-advanced-night-1440.png) | [workbench-b-newsession-advanced-ledger-1440.png](./workbench-b-newsession-advanced-ledger-1440.png) |
| 390 | [workbench-b-newsession-advanced-night-390.png](./workbench-b-newsession-advanced-night-390.png) | [workbench-b-newsession-advanced-ledger-390.png](./workbench-b-newsession-advanced-ledger-390.png) |

### 390px 软键盘：主操作始终在可视区内

[workbench-b-newsession-keyboard-390.png](./workbench-b-newsession-keyboard-390.png) — e2e 通过改写 `visualViewport.height`（504px，约 iPhone 13 级键盘高度）派发与真机相同的 resize 事件；断言「开始」整体位于未被遮挡区域且保持可用。

### 未知 ACK：状态待确认，永不二次创建（fake Hub 实拍）

[workbench-b-newsession-unknown-ack-1440.png](./workbench-b-newsession-unknown-ack-1440.png) — hub-live e2e 中，`POST /v1/instances` 已被真实 fake Hub/fake Node 处理（实例已创建）后响应被替换为 504。页面停在常驻 `状态待确认`：展示客户端请求 id、只给只读调和动作，底部主操作禁用。服务端断言本次尝试只多了恰好 1 个实例。截图来自 `hub_e2e` 合成夹具（`remuda-e2e` 空间 / `e2e-fake-node`）。

## 3 · 测试

- `web/src/lib/newSessionDraft.test.ts`（19 例）：身份/主机/工作目录三段隔离；无身份仅内存且不提升到存储；hostile 字段与被篡改存储的清洗；版本/损坏数据拒绝；与 `runtime.draft.<instanceId>` 共存互不触碰；空草稿判定。
- `web/src/pages/NewSessionPage.test.tsx`（28 例）：保留全部既有创建/worktree/effort/launch 参数用例；新增来源返回（会话/列表/深链回退）、Escape 保留草稿与重开恢复、显式丢弃、首层无实现词汇、终端命名 shell-pty、ACK 未知恰好一次 create 且不能二次提交、明确 4xx 可修正后再提交。
- `web/tests/e2e/new-session.spec.ts`：
  - mock-backed：原有 30 秒路径、终端、布局 A 滑块六档/ultracode/重吸附/等宽与证据用例保留；
  - hub-live（fake Hub + fake Node，`playwright.hub.config.ts`，串行单 worker）：纯键盘创建恰好一次 POST；**未知 ACK**——`page.route` 先 `route.fetch()` 让真实 Hub/fake Node 完成创建再回 504，客户端显示 `状态待确认`、POST 恰好 1 次、刷新只读不重发，服务端 `GET /v1/instances` 只多出恰好 1 个实例；390px 软键盘几何。
  - 该 spec 同时在两个 Playwright config 下运行：hub config 在进程内设置 `REMUDA_E2E_BACKEND=hub`，spec 据此跳过不属于当前后端的 describe；hub config 的 testMatch 增加 `new-session`。

### 本机运行记录（devbox-sg，2026-09-14）

- `pnpm --dir web lint` / `tsc -b`：通过；`pnpm --dir web test`：74 文件 513 例通过。
- mock e2e（chromium via `ws://127.0.0.1:3177`）：本 spec 19 通过 / 2 失败；失败用例均为「kind terminal uses shell-pty」（chromium 与 mobile-webkit 同一原因），在未改动的 origin/main 上同样失败（已知的 mock-backed 基线问题，见本机 devbox 记录；根因在 `SessionPage` 的 baseView 选择，本批未触碰），与本批改动无关。
- hub e2e（`HUB_E2E_LISTEN=127.0.0.1:58280 HUB_E2E_WEB_PORT=58289`，`flock /tmp/remuda-agents/e2e.lock`，conda OpenSSL 3）：本批三个新用例首次运行全部通过；同轮另两个失败（hub-live 承载矩阵断言因矩阵移入高级区、spaces-hub-live 关闭按钮名）已在本批修正并复验；`providers-discovery` 的失败属于该共享机记录在案的基线 flake 集合（本机高负载），本分支未触碰该路径。

## 4 · 边界与未做

- 客户端请求 id 目前只在客户端使用：Hub 的 `POST /v1/instances` 尚无服务端幂等键契约（`CreateInstanceBody` 不接收该字段）。本批保证的是「ACK 未知绝不自动二次创建」；跨刷新的服务端去重需要单独的调和契约（计划 §3 中 C2 的 clientRequestId 工作），不在本批范围。
- 无身份时的内存草稿不跨 tab、不跨刷新——这是刻意的安全取舍。
- 未改动 EffortSlider/effort.ts/Composer/Shell/store.ts/ui.module.css；承载方式 testid 与 launch-options（特殊参数、自定义可执行文件）原样保留，仅移动位置。
- 手机真机软键盘、TalkBack/VoiceOver、输入法组合输入下的手感仍需人工验收；e2e 用 visualViewport 事件模拟等价几何。
