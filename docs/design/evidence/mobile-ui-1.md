# 手机优先 UI · 规格与 ADR 修订（D-049）

2026-09-19 · `wt/c-mspec/b-mspec-md` · 任务 c-mspec（mobile-ui 实施计划 §(C) 任务 1，docs-only）

本任务把手机优先架构写进规格，使后续实施任务（m-shell / m-home / m-inbox / m-sessionfold / m-keybar / m-jumpto / m-push / m-voice）不再与 `ui-spec.md` 打架——这是报告 §11.7 教训的第二次应用（第一次是 [ux2026-spec-1.md](./ux2026-spec-1.md) 的 D-038…D-042）。**不实施任何代码**：`web/` 与 `crates/` 零改动，格式与边界同 c-uxspec（见该证据 §5）。

派工计划本身**不在仓库里**，本文件按名字（mobile-ui 计划）引用它、不建链接；每条决策的**理由**指向 [workbench-ux-improvement-2026-09.md](../workbench-ux-improvement-2026-09.md)（下称「报告」）的具体小节。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `docs/design/ui-spec.md` | v0.2.2 changelog；§1.2 路由表（`/m`、`/m/inbox` 两行 + `/` 改按视口落点 + `/sessions`、`/approvals` 行补 compact 重定向）+ 表后重定向层表；§1.3 手机线框重画为两套铬 + 「会话路由 compact 不渲染底部导航栏」规则；§4.5 补 badge 与权限横幅位置、PWA 关闭态；§4.6 `start_url` 改 `/`；新增 §4.7、§4.8 |
| `docs/design/decisions.md` | 顶部索引表追加 D-049 一行（D-048 之后） |
| `docs/design/evidence/mobile-ui-1.md` | 本文件 |

## 2. `ui-spec.md` 逐条：旧文 → 新文

### 2.1 changelog（新增 v0.2.2 段）

| | |
|---|---|
| **旧文** | 最新一段是 v0.2.1（D-038…D-042 的六处修订），没有手机路由树、铬预算、语音、badge 的任何记录 |
| **新文** | 顶部新增 v0.2.2 段，逐处列出本批改动（§1.2 / §1.3 / §4.5 / §4.6 / §4.7 / §4.8）并点名 D-049，依据列出报告 §5 / §6 / §7.1 / §9 / §10-19 / §10-23 / §11.2 / §11.3 / §11.4 / §11.5。与 v0.2.1 同样声明「只增补、不改产品方向」 |

### 2.2 §1.2 路由表与重定向层

| | |
|---|---|
| **旧文** | `/` 行 = 「重定向 `/sessions`」；表中无 `/m*`；`/sessions` 行只有 query 说明；`/approvals` 行备注「手机底栏一等入口」；表后只有一句深链说明（推送进 `/s/:id` 或 `/approvals?focus=`） |
| **新文** | `/` 改为「按视口落点：compact → `/m`，其余 → `/sessions`」；新增 `/m`（手机会话 home）与 `/m/inbox`（手机收件箱）两行并注明桌面访问 `<Navigate replace>` 回 `/sessions`；`/sessions` 行注明 compact → `/m`，`/approvals` 行注明 compact → `/m/inbox` 且 **query 原样保留**；`/s/:id*` 各行与 `/sessions/new` 行显式注明「不重定向」。表后新增一张 5 行重定向表（compact / 桌面 × 命中路径）与一段深链说明 |
| **为何这样写** | 计划 §(B) B.1 把「受限 `/m` + 重定向规则」列为必须进 ADR 的最重要边界；写成表格让 query 保留与「`/s/:id` 永不重定向」成为可逐条验收的明文，而不是散在叙述里 |

### 2.3 §1.3 手机线框与「会话路由无底栏」规则

| | |
|---|---|
| **旧文** | 手机只有一张线框：「顶：标题/连接/铃铛 + 唯一一列 + 会话页 transcript/底 composer + 底栏 会话│审批·n│＋│更多」——首页预会话页共用同一条底栏，与会话页底部的 composer 叠成两条 |
| **新文** | 线框重画为**两套铬**：首页级屏（`/m`、`/m/inbox`、`/settings`）保留 64px app 底栏；会话路由（`/s/:id*`）只有 52px 顶栏 + 正文 + composer/键盘条，**无 app 底栏**。线框下新增规则段：会话路由 compact 不渲染 app 底栏（`Shell.tsx` 的 `.bar`，今天在 `:310`）也不渲染 SpaceTabs 行（`:306`）；回列表靠 `SessionPage.tsx:239` 返回键（44px 热区，D-039）；切空间/切 tab 靠 D-040 的单芯片抽屉（`spaces-drawer-open`）与 Jump To，能力不降级 |
| **处理方式** | 旧的「映射规则」表与 Paseo 段落全部保留；新规则是**在手机小节内增补**，桌面三栏线框一个字没动。底栏文案「收件箱(n)」与 §1.1 表的「审批」之差在 §4.7 显式钉了口径（compact 底栏以 §4.7 为准） |

### 2.4 §4.5 badge、权限位置、PWA 关闭态

| | |
|---|---|
| **旧文** | 载荷 = `title, body, tag, data.url`；没有应用角标；没有权限申请位置的规定（实现里只藏在设置 → 通知）；没有 PWA 关闭态行为；iOS 只有一句「必须加到主屏幕」 |
| **新文** | 载荷增加**可选**整数 `badge`；新增三段：(a) **badge**——SW 在 push 事件调 `setAppBadge`/`clearAppBadge`，站内 pending 归零清零，无 Badging API 什么都不做（不用通知条数冒充），字段缺失行为逐字节一致、不新增事件类型；(b) **权限位置**——绝不在启动时弹，只在 `/m/inbox` 横幅（报告 §11.3 实测位置）与设置两处由手势触发，iOS 未加主屏时文案换「先加到主屏幕」；(c) **PWA 关闭态**——SW 唤醒 → `showNotification` → `notificationclick` 聚焦/`openWindow`，深链落点不变，Hub 的 follow 抑制规则（`alerts.rs:159-160`）不改 |
| **处理方式** | 旧的事件集合（interaction/持续 waiting/失败退出、不对 `--bg` idle 发完成）与 VAPID 订阅段落原样保留；badge 明确写成**一个可选 Rust 字段**（计划 D8），不给旧客户端混搭留坑 |

### 2.5 §4.6 `start_url`

| | |
|---|---|
| **旧文** | `- start_url: /sessions。` |
| **新文** | `- start_url: /（D-049，2026-09-19 从 /sessions 改）：落点由 §1.2 重定向层按视口判定——手机装的 PWA 落 /m，桌面装的落 /sessions；只有一份 manifest、一份 SW（SW 预缓存壳清单已含 /，sw.src.js:9）。` |
| **处理方式** | 计划归属清单点名的是 §4.5，但 §4.6 这行不改就会与 D-049 直接矛盾；同文件内的一致性修订，在本表与 D-049 里都留了记录 |

### 2.6 §4.7 手机优先路由树与铬预算（新增整节）

| | |
|---|---|
| **旧文** | §4 只有 §4.1–§4.6（软键盘 / 中文输入 / 触控 / 前后台 / Web Push / PWA 安装），**没有**手机路由树、没有可测量的铬预算、没有截断优先级 |
| **新文** | 新增整节，五块：(1) **受限 `/m` 树**（只含 home / inbox / Jump To sheet / phone 底栏的一张表）+ 「会话本体永远是共享 `/s/:id`、不分叉」+ 引报告 §9 SOTA 一句话；(2) 指向 §1.2 的重定向与深链三连；(3) **可测量铬预算**——最多一条顶栏（`--top-mobile`，`tokens.css:79`）+ 一条底栏（`--bar`，`tokens.css:77`，+ `--safe-bottom`，`:80`）；会话路由无 app 底栏/无 SpaceTabs 行；**正文（`session-body`，`SessionPage.tsx:416`，终端段测 xterm 容器）composer 收起、无软键盘时 ≥ 60% 视口高**，并给出 390×844 的余量算式（结构 ≈ 75%、终端 ≈ 71%，60% 是下限不是目标）；(4) **截断优先级**：标题 → space 芯片退首字母 → 状态文字（只留点）；**`终端\|结构` 分段（`SessionPage.tsx:258`）与 Stop（`:329`）永不截断、永不进 ⋯**（D-040）；(5) 依据清单逐小节点名报告，并钉 M1/M2 边界（M1 = home、会话、收件箱、新建、登录、语音；键盘条/Jump To/badge 真机排 M2；git 五 tab 在 M2 后） |
| **为何这些数字是「规格」而不是实施细节** | 报告 §2.1 的观察是「垂直铬叠五层，正文只剩中间一小块」；没有可测量下限，每个实施单都会自行其是。60% + 两条栏 + 截断顺序让任务 5（m-sessionfold）的几何验收直接引用本节，不需要再发明口径 |

### 2.7 §4.8 语音输入（新增整节）

| | |
|---|---|
| **旧文** | 全规格无语音条目；全库也无任何语音代码。「能说」（报告 §5-P0）没有定义 |
| **新文** | 新增整节、五条钉死：(1) 默认路径 = **平台键盘自带听写**，不录音、不上传、不云转写、不新增协议字段；(2) **先成文再发送**，听写中途绝不自动发送，`composing()`（`viewport.ts:67-68`）守卫对听写 input 同样生效；(3) **Web Speech API 仅增强、默认关**，能力探测不存在就不渲染麦克风按钮，结果只写进输入框；(4) **iOS Safari 没有 `SpeechRecognition`——这句话必须原样写进设置页文案**，iPhone 的「能说」就是系统键盘听写，不承诺离线听写；(5) **终端段不提供语音**（PTY 会吃下候选词与标点）。依据点报告 §5-P0 与 §7.4 |
| **处理方式** | 计划 D3 选 (a) 平台听写；规格把「为什么不是 Web Speech 为主」也写明（iOS Safari 无该能力，以它为主路径等于让 iPhone 拿不到功能），任务 9（m-voice）照此实现即可 |

## 3. `decisions.md` D-049 与计划 D1–D8 的对应

| 计划默认（§(D)） | D-049 内的落点 |
|---|---|
| D1 架构 = 方案 2 的受限形态 | D-049 (1)(2)：`/m` 只含 home/inbox/Jump To/底栏；会话本体共享 `/s/:id` 不分叉；拒绝独立客户端的理由（§9 SOTA + §5 明确不做）写进依据 |
| D2 互补不取代 | D-049 (2)：`/m` 只取代 `/sessions` 与 `/approvals` 的**手机入口**（重定向），`/s/:id` 与共享页的 compact 继续由 D-040/D-041/D-042 负责，桌面零改动 |
| D3 语音 = 平台听写 | D-049 (7) 与 ui-spec §4.8 五条 |
| D4 通道不变 + 横幅/设置两处 | D-049 (6) 与 ui-spec §4.5；明确「不在启动时弹」 |
| D5 手机 Inbox 取代审批入口、桌面保留 | D-049 (3)：compact `/approvals?focus=` → `/m/inbox?focus=`，query 保留；桌面 `/approvals` 原样（反向是 `/m*` → `/sessions`） |
| D6 M1 范围 | D-049 (8)：M1 = home/会话/收件箱/新建/登录/语音；键盘条、Jump To、badge 真机为 M2 |
| D7 `start_url: /` | D-049 (4) + ui-spec §4.6 |
| D8 badge 动 Rust、只加一个可选字段 | D-049 (5) + ui-spec §4.5（缺省逐字节一致、不新增事件类型、不支持就什么都不做） |

另记入 D-049 的两条计划内取舍：**隧道/Kill 端口旁栏整体不抄**（报告 §11.5 + D-031，不只是「优先级靠后」）；会话路由去掉 app 底栏后的「回家」路径（44px 返回键 + M2 的 Jump To，计划风险 E9），实测反感时只回滚该条、不回滚整棵 `/m` 树。

## 4. 与 D-038…D-042 的逐条无冲突对照

| ADR | 既有决定（一句话） | 本批新增 | 关系判定 |
|---|---|---|---|
| **D-038** | 列表行 = 点 + 标题 + 一句下一步；wire 退到 `<details>`/`title`；行内遥控进 Sheet | §4.7 表写明 `/m` home 的行口径**同 §2.1 / D-038**（点 + 标题 + 一句下一步 + context 环）；§1.2 只是给手机列表一个**新路由**，没有重述或放宽任何行内文本规则 | **引用，不改写**。手机 home 行是 D-038 的另一个消费者，禁止项（不出现三维 wire/`ins_`/driver/model）原样生效 |
| **D-039** | 命中只靠热区；视觉字形不变；`.meta` 同值；热区不得重叠 | §4.7 预算把顶栏/底栏/返回键全部写成 `var(--touch)` 热区并再次引用 §3.4；§1.3 规则段点名返回键 44px 热区 | **同向加严**。没有新增任何裸像素命中尺寸，也没有改「视觉可以略小」的口径 |
| **D-040** | `/s/:id` compact：chips 折成**单枚**当前 space 芯片入顶栏；诊断进「运行详情」；分段与 Stop 永不进 ⋯ | §1.3 + §4.7 规定会话路由 compact **既不渲染 app 底栏、也不渲染 SpaceTabs 行**；截断优先级把 space 芯片排在标题之后、状态文字之前；分段/Stop 的「永不截断、永不进 ⋯」原文照抄并补了挂载位置（`SessionPage.tsx:258` / `:329`）；space 芯片点开仍是 D-040 的同一个抽屉（`spaces-drawer-open`） | **同向增补（additive），不是改写**：D-040 解决的是「chips 行折成单芯片」，本批解决的是「单芯片之外那条 SpaceTabs 行与 app 底栏也不该占正文」；D-040 允许的抽屉、必须留的主行元素（host 芯片、cost、状态点、分段、Stop）一条不少，切空间/切 tab 能力由抽屉 + Jump To 兜底并被明文写成「能力不降级」。D-040 的文字无需修改即与本批兼容 |
| **D-041** | compact + settled 普通工具卡折一行；Workflow/error/`interaction.*` 豁免；折叠分支在 family 判定之后 | §4.7 的 75% 正文算式把「一行工具卡」作为结构段的现状前提引用；没有给工具卡加任何新折叠/展开规则 | **无交集**。本批不碰工具卡语义；正文预算在工具卡折行的现状上测量，D-041 若回滚只会让正文更短、不会让 60% 口径失效（75% 余量已吸收） |
| **D-042** | compact composer 收成选项触发器；触发器带 mode 词与档名；三态/队列 chip/诚实标注不进 sheet；桌面零变化 | §4.7 把结构段底部钉为「收起态 composer 56px」且该路由**不另加 app 底栏**（两条不叠）；§4.8 语音第 2 条要求听写结果只进同一 composer、不绕过 `composing()`、不触发发送 | **互补**。§4.7 给 D-042 的 composer 指派了「该路由唯一下条」的位置；§4.8 的语音只是 composer 的一个输入来源，三态与 sheet 边界一字未动 |

结论：五条 ADR 无一需要修订；本批是在 D-040 的单芯片规则**之上**再收两条（SpaceTabs 行、app 底栏），两者叠加后 `/s/:id` compact 的顶栏能力仍全部可达。

## 5. 引用的 file:line 核对（全部以 `git show origin/main:<file>` 验证）

规格正文出现的每一条代码 file:line 都在本批编辑前于 `origin/main`（`402c3670`）逐条验证；本分支不改代码，行号不会在本批内漂移。

| 引用 | 验证到的内容（origin/main） |
|---|---|
| `tokens.css:77` | `--bar: 64px;` |
| `tokens.css:79` / `:80` | `--top-mobile: 52px;` / `--safe-bottom: env(safe-area-inset-bottom, 0px);` |
| `tokens.css:68` / `:57` | `--touch: 44px;` / `--text-aux: 12px;`（§3.4 既有引用，本批未改） |
| `web/src/app/Shell.tsx:306` | `{onSessions ? <SpaceTabs … /> : null}` |
| `web/src/app/Shell.tsx:310` | `<nav className={css.bar} aria-label="手机底栏">` |
| `web/src/pages/SessionPage.tsx:239` | `<Link className={session.back} to="/sessions" aria-label="返回">` |
| `web/src/pages/SessionPage.tsx:258` | `<ViewSwitch …` 挂载处 |
| `web/src/pages/SessionPage.tsx:329` | Stop 控件 `aria-label="Stop"` |
| `web/src/pages/SessionPage.tsx:416` | `data-testid="session-body"` |
| `web/src/lib/viewport.ts:15` | `export function useWorkbenchViewport()` |
| `web/src/lib/viewport.ts:67-68` | `composing()`：`isComposing` / `key === "Process"` / `keyCode === 229` |
| `web/src/lib/push.ts:19` | `needsHomeScreenForNotifications()` |
| `web/src/lib/push.ts:108-114` | `subscribePush()` 内 iOS 早退 + `Notification.requestPermission()` |
| `web/src/pages/SettingsPage.tsx:724` | 通知组 `GroupHeading id="notifications"`（推送开关在该组内） |
| `web/sw.src.js:9` | `SHELL` 预缓存清单含 `"/"` 与 `/index.html` |
| `web/sw.src.js:97` | `self.registration.showNotification(...)` |
| `web/sw.src.js:106-120` | `notificationclick`：聚焦既有窗口，否则 `openWindow`（`:120`） |
| `crates/remuda-hub/src/alerts.rs:159-160` | 有设备 follow 即 `push suppressed`（Paseo attention suppress） |
| `web/public/manifest.webmanifest:6` | 今天仍是 `"start_url": "/sessions"`——本批只改规格，实际改值由任务 2（m-shell）实施 |

报告小节引用（§5 / §6 / §7.1 / §9 / §10-19 / §10-23 / §11.2 / §11.3 / §11.4 / §11.5）均按入库后现行标题引用；其中 §9 的 SOTA 一句话（报告行 344）与「不要抄」清单（报告行 336–342）、§11.5 末「这块整体不抄」（报告行 461）为逐字语义引用。

## 6. 边界与不做项

- **零代码改动**：本分支不含 `web/` 或 `crates/` 的任何改动，没有 vitest / e2e；manifest 的 `start_url` 实际改值、badge 字段、重定向元素全部留给 §(C) 任务 2/5/8，规格只定口径。
- **不抄 D-031 禁止的东西**：端口/Kill 面板与第三方隧道工具在 §4.7 / D-049 里只以类别名出现（「端口/Kill 旁栏」「隧道工具」），本文件不逐字拼写 D-031 禁用的产品名，避免命中 `no-tunnel-scan.sh` 自身（同 ux2026-spec-1.md §6 的做法）。
- **不新做原生 app、不做独立 Vite 入口、不做两份 manifest/SW**（报告 §5「明确不做」）；§4.6 显式钉为一份。
- **不把 `/events` 提为一等入口、不把 hook/PTY 原文当结构对话**（报告 §7.3 / §10-21）；§4.7 的路由树清单里没有它。
- **不承诺 git 五 tab、不抄硬裁 diff 与列表行丢弃按钮**（报告 §11.4）；§4.7 只把它排在 M2 之后。
- **不抽公共 `Segmented`**（报告 §11.7 P1-8 行的 M→L 告诫仍成立）；§4.7 只要求分段永不进溢出。
- **不引入个人标识**：新增文本无主机名、无 home 路径、无用户名；新增线框不编造示例主机名。

## 7. 验证

| 检查 | 结果 |
|---|---|
| `scripts/ci/secret-scan.sh` | PASS（见本任务 DONE 前的运行输出） |
| `scripts/ci/no-tunnel-scan.sh` | PASS（新增规格文本对 D-031 禁用 token 集合零匹配） |
| 引用一致性 | D-049 编号在 changelog、§1.2、§1.3、§4.5、§4.6、§4.7、§4.8 与 decisions.md 索引行中一一对应；表 5 的代码行号全部经 `git show origin/main:<file>` 核对 |
| 旧文比对 | §2 各表「旧文」逐字取自本批修订前文件（可用 `git show 402c3670:docs/design/ui-spec.md` 复核） |
| 与 D-038…D-042 冲突核对 | 见 §4 逐行表：无一需要修订，D-040 与本批为同向增补 |
| vitest / e2e | 不适用（docs-only，无代码改动） |
