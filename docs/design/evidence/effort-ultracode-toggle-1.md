# effort-ultracode-toggle-1 — 五档滑杆 + 正交 Ultracode 开关（D-056 web）

Date: 2026-10-05
Branch: `wt/c-effortui-b-md`

## 范围

D-056（decisions.md 同条）落地 web 端：Claude 滑杆从「第六档 ultracode」改成
**五个原生档位 `low/medium/high/xhigh/max` + 独立的 Ultracode `role="switch"`**。
开关在 ≥2.1.284 上与档位正交（任意档位可开，翻开关不移动滑杆）；在
2.1.203–2.1.283 上保留耦合行为（开 → 滑杆移到 xhigh 并明示）；<2.1.203
开关禁用并显示版本原因。Codex/grok 滑杆不变。

> 注：本任务只交付 web + Hub catalog 字段。Rust 驱动端 ≥2.1.284 的会话内
> 两条命令切换在并行任务（c-effortread）里；web 已按 D-056 wire 形状
> `{name,ultracode,index}` 发送，并在耦合版本上让页面只发一次。

## 单元测试（截图以 WebKit 真机 + 模拟键盘在 390px / 1440px 复核的 checklist；
## 自动化断言见 vitest，真实浏览器切换见下一节 hub 规格）

- 五档 / 开关 / 版本门：`src/features/session/effort.test.ts`（42 tests）
  - `effortStops("claude")` 恰五档，无 `ultracode` 档；
  - `claudeVersionGate`：2.1.289/2.1.284 decoupled，2.1.277/2.1.203
    coupled，2.1.202 legacy，空/未知 unknown；
  - per-model 默认：opus/sonnet=medium，fable/haiku=high，未知模型 null，
    网关 `family-日期` / `[1m]` 后缀命中。
- 开关组件：`src/features/session/EffortSlider.test.tsx`（22 tests）
  - 拖滑杆只改档位、带着当前 flag；翻开关只调 `onUltracodeChange`、滑杆不动；
  - legacy/coupled/model/workflows 四种禁用原因；Codex 六档无开关行。
- 每轴回读 + A→B provenance：`src/lib/store.effort.test.ts`（14 tests）
  - level/flag 分轴 settle；flag 未读回不清除指示；
  - 被替换请求（A）的晚到 `effort-queued` 不覆盖 B；
  - 同时间戳的旧投影不能清除新请求的 pending；
  - model 拒绝随 /model 解除，workflows 拒绝进程内保持、正向 on 解除。
- 组件 / 页面：Composer、NewSessionPage（34 tests，含 legacy `ultracode`
  偏好迁移为 {xhigh,on}、未固定草稿跟随模型默认）、driverMatrix argv 预览
  （decoupled overlay vs coupled `--effort ultracode`）。

全量：`185 files / 2121 tests passed`，`tsc -b` 0 错，oxlint 仅 warning。

## Hub e2e（fake node，gate-e2e.lock 内）

`web/tests/e2e/effort-sync.hub.spec.ts` 由 fake 节点
（`crates/remuda-hub/examples/hub_e2e.rs`）驱动：

- 档位切换 wire 为 `{name,ultracode:false,index}`，无旧别名；
- **开关在 max 打开 → wire `{name:"max",ultracode:true}`，滑杆停在 max**，
  chip 为 `max · ultracode`，ember 落在收起触发器/开关上；
- level 与 flag 分轴 settle（flag 先未知、后 on）；
- 三次连续切换（flag on → 档位 → flag off/on）各自结算；
- `ultracode-unavailable-for-model` 只禁用开关行，滑杆仍可用（失败是
  configure outcome，不结束会话）；
- queued/degraded 生命周期。

fake 节点改动：不再无条件把 `max` clamp 到 xhigh（D-056 解耦后
`{max,ultracode:true}` 必须在 max 回读 on）；clamp 改为 `__clamp__` 测试
哨兵；新增 `__ultra_refuse_model__` / `__ultra_refuse_workflows__` 哨兵，
回读 flag 与 level 分轴投影。

运行：

```sh
cd web
PW_CHANNEL=chromium ./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/effort-sync.hub.spec.ts
```

截图仅在 `REMUDA_EVIDENCE=1` 时写入本目录；本任务的 390px / 1440px 真机
（WebKit iPhone）开关页截图在 gated run 落盘后补登：

- `effort-toggle-390.png`（手机 sheet：五档 + 开关）
- `effort-toggle-1440.png`（桌面 popover：五档 + 开关）

## 真实 Claude Code ≥2.1.284 真机切换

由 owner 在 ≥2.1.284 真机上完成（自动 e2e 只打 fake 节点；web 端按
D-056 发送 `{name,ultracode}` 并按版本门渲染耦合/解耦文案）。web 侧
自测点：任意档位开开关滑杆不动；level→flag→level 三连后 chip 各自 settle。
