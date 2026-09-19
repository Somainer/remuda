# mobile-ui 任务 9 · `m-voice`：第一里程碑「能说」

2026-09-20 · `wt/c-mvoice/b-mvoice-md` · mobile-ui 实施计划 §(C) 任务 9（规格 `docs/design/ui-spec.md` §4.8，D-049）

第一里程碑「能说」的最小诚实实现：**平台键盘听写是默认路径（不需要 Remuda 写任何代码，composer 只要不与它打架）；Web Speech API 只在浏览器实现了 `SpeechRecognition` 且用户在设置里显式打开时，作为一个麦克风按钮出现。** 识别结果只进输入框，任何路径都不触发发送；不录音、不上传音频、不做云转写、不新增协议字段。

## 1. 交付物

| 文件 | 变化 |
|---|---|
| `web/src/lib/speech.ts` | 新增。能力探测 `speechRecognitionSupported()`（`'webkitSpeechRecognition' in window \|\| 'SpeechRecognition' in window`）、每设备开关 `readVoiceInputEnabled()` / `writeVoiceInputEnabled()`（localStorage key `runtime.voice-input.v1`，缺省即关）、`SpeechInput` 封装（interim 累积替换 + final 提交、`start/stop/abort`、`onerror/onend`） |
| `web/src/lib/speech.test.ts` | 新增。探测（无构造器 / webkit / 标准三种）、开关默认关与持久化、封装 start/stop/abort/error/onend、interim 替换而非追加、final 累积、新会话清零 |
| `web/src/features/session/Composer.tsx` | **纯增量 +125 行、零删除零搬移**。仅在 `mobile && speechRecognitionSupported() && 开关开` 时于 textarea 旁渲染 `composer-voice` 按钮，聆听时在控制条显示一行 hint；识别文字经 `setText` + `writeDraft`（与打字完全相同的写入面）进草稿。三态按钮、队列 chip、选项 Sheet 结构、placeholder 分平台逻辑一字未动（D-042 边界） |
| `web/src/features/session/ComposerVoice.test.tsx` | 新增。任务要求的 Composer 分支测试：无能力不渲染、有能力但开关默认关不渲染、桌面永不渲染、识别短语落入 textarea 且 `onSend` 零调用 |
| `web/src/pages/SettingsPage.tsx` | 「外观与输入」分组新增「语音输入」段：不上传音频 / 不做云转写 / iPhone 用系统键盘听写的说明、逐字的 iOS Safari 限制声明、默认关且无能力时禁用的开关 |
| `web/src/pages/SettingsPage.test.tsx` | 新增该段两个用例（默认关 + iOS 文案逐字；有能力时开关可开、可关、持久化） |

未触碰：`session.module.css`（麦克风按钮复用现有 `chip` 样式、hint 复用 `controlNote`）、`ToolCard.tsx`、`ui-spec.md`、`decisions.md`、`playwright.hub.config.ts`，以及任何 `crates/` / wire 代码。终端段不渲染 composer dock（`SessionPage.tsx` 既有规则，本任务未改），所以终端段天然没有麦克风。

## 2. 验收对照

| # | 验收 | 实现 / 证据 |
|---|---|---|
| 1 | 无 `SpeechRecognition`（iOS Safari）不渲染麦克风，探测式为 `('webkitSpeechRecognition' in window) \|\| ('SpeechRecognition' in window)` | `speech.ts` 逐字该式；vitest「iOS Safari case」用例；真机浏览器里删除两个构造器后 composer 零麦克风（§4 记录 B） |
| 2 | 识别结果只写 textarea，任何路径不发送；`composing()` 守卫不被绕过 | 语音路径不合成任何键盘事件，只走 `setText`/`writeDraft`（打字的同一路径）；代码中无 `onSend`/`submitPrimary` 引用；`viewport.ts` 未改。vitest 与浏览器记录 C 均断言 `onSend` 零调用、草稿在识别后保持完整 |
| 3 | 设置页写明不上传音频、无云转写、iPhone 用系统键盘听写；开关默认关、按设备持久化 | SettingsPage 新段含三句保证与逐字「iOS Safari 没有 SpeechRecognition（WebKit 未实现）」；key 同其他本地偏好存 localStorage；两个 settings 用例 |
| 4 | 终端段不出现麦克风 | 终端段（`/tty`，含 generic-pty）不挂 composer dock；浏览器记录 A 切到 tty 后 `composer-voice` 计数为 0 |
| 5 | 桌面 testid 与布局零变化 | Composer.tsx 的改动全部在 `mobile` 分支或新增节点内，桌面 JSX 一个字符未动；浏览器记录 D：1280px 下本分支与任务基线（`d464ea47`，Composer 文件与当前 `origin/main` 同版）的 `[data-testid=composer]` outerHTML **逐字节相同（各 1877B）**，控制 bar 子元素几何（x/y/w/h）也完全相同 |

## 3. iOS 限制声明（逐字进设置页）

> iOS Safari 没有 SpeechRecognition（WebKit 未实现）。iPhone 上请用系统键盘的听写按钮；把 Remuda 加到主屏幕后的标准 PWA 里键盘听写可用。终端段不提供语音，要说话请切到结构段。

iPhone 上「能说」= 系统键盘麦克风：听写产生的是普通 `input` 事件，手机 composer 本来就是 Enter 换行、按钮发送（ui-spec §4.2），听写中途没有任何自动发送路径；Remuda 不承诺离线听写。这条路径不需要 Remuda 代码，本任务也没有为它加代码——只保证不抢焦点、不在听写期间重排（语音按钮点击后**不**对 textarea 调 `focus()`，避免唤起软键盘；`visualViewport` 接入是既有实现）。

## 4. 手工验证记录（真实浏览器）

**环境**：Google Chrome for Testing 153.0.8010.12（Playwright Chromium 修订 1243），headless，移动模拟 390×844 / touch / isMobile；被测应用为 mock 后端的 Vite dev server（`VITE_MOCK=1`，回环地址，不连任何主机）。桌面对照为同一二进制在 1280×800 下跑任务基线 `d464ea47` 的临时 worktree，两台 dev server 仅本机端口不同。

**关于音频路径的诚实说明**：该 Linux 沙箱没有麦克风、也没有到浏览器厂商语音服务的网络通路，因此**没有**经过真实的拾音 → 厂商识别；验证方式是在 document-start 注入一个与浏览器同形的 `SpeechRecognition`（同时占住标准与 webkit 两个构造器名），由它按真实事件面（`onresult` 的 `resultIndex` / `results[i].isFinal` / `results[i][0].transcript`，以及 `onend`）吐出 interim 与 final 事件。被测代码（`speech.ts` + Composer）走的是与真浏览器完全相同的事件路径；能力探测本身在未注入的真实 Chromium 上验证（两个构造器均存在）。

- **A. 真实 Chromium，移动视口**：原生探测为 `{std:true, webkit:true}`；设置页开关可用、无「不支持」提示；开关默认关；打开后刷新仍为开（按设备持久化）。新建会话切到结构段，麦克风出现；点一次进入聆听（`data-listening=1` + hint「文字只进输入框，不会自动发送」），再点一次回到静止（`data-listening=0`）。切到终端段：`composer-voice` 数量 = 0。
- **B. 模拟 iOS Safari（删除两个构造器），localStorage 开关已置 1**：设置页开关禁用并显示不支持说明；结构段 composer 零麦克风——能力缺失时开关开着也不画按钮。
- **C. 同形识别器注入，移动视口**：点麦克风后经真实事件面依次投递 interim「总结」→ interim「总结一下今天的改动，不要发送」→ final 同名结果。
  - **口授内容**：`总结一下今天的改动，不要发送`
  - **结果**：textarea 最终值逐字等于该短语（interim 被替换、未重复追加）。
  - **没有任何东西被发送**：识别结束后草稿原样保留，发送按钮未被识别路径调用；mock 后端下发送只有显式点「发送」这一个出口——手工点击该按钮后草稿才清空（证明清空来自显式发送，而非听写）。
- **D. 桌面零差异**：1280×800 下本分支与基线 `d464ea47` 各新建一个会话、切结构段，字体就绪后抓取 composer DOM 与控制条子元素几何：outerHTML 逐字节相同（1877B），8 个直接子元素的 x/y/宽/高全部一致；两侧都没有 `composer-voice`。

上述检查由一个**未提交**的临时脚本驱动（留在任务 scratch 目录，不属于仓库），连续运行三次均为 20/20 通过。

未做 iPhone 真机验证：沙箱内无 iOS 设备，且键盘听写路径按规格不需要 Remuda 代码；真机只可能验证系统键盘行为本身，列为后续人工项，不在本任务闸口内。

## 5. 自动化测试

- `pnpm --dir web test`：**131 个文件 / 1246 个用例全部通过**；本任务新增 21 例：`speech.test.ts` 14 例、`ComposerVoice.test.tsx` 5 例、`SettingsPage.test.tsx` 2 例。
- `pnpm --dir web typecheck`：通过。
- `pnpm --dir web lint`：仅存量 warning（`Transcript.tsx` / `SessionPage.tsx` 等既有文件），新增文件无新增告警。
- hub e2e：未新增语音 e2e。`SpeechRecognition` 在 Chromium/headless 下不稳定且需要拾音与厂商服务，计划明确它**不是闸口断言**；能力缺失分支由单测与 §4-B 的浏览器手工记录覆盖。

## 6. 边界与不做项

- 不录音、不上传音频、不做云端转写；不新增任何协议/wire 字段（报告 §5「明确不做」）。音频是否离开设备由用户所用浏览器决定，不经过 Remuda 的 Hub / Node——设置页如实写明这一点。
- 听写中途绝不自动发送；语音不接入 `submitPrimary` / `onHold` / steer 任何一条发送路径，也不改 `composing()` 守卫。
- 终端段不提供语音（ui-spec §4.2/§4.8：不向 PTY 灌候选与标点）。
- 不改三态按钮、队列 chip、「尚未验证」标注、选项 Sheet（D-042），不改 `session.module.css`、`ToolCard.tsx`、规格文档与 hub playwright 配置。
- 桌面 composer DOM 与几何零变化（§4-D）。
