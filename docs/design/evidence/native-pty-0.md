# D-028 P0 实跑证据：`remuda-screen` 抽取、终端模拟器、重绘快照

工作树 `wt/x-p0-screen/emulator-and-screen-crate`，分支自 `origin/main`。
宿主 arm64 macOS（Darwin 25.4），rustc/cargo 1.94.1，claude 2.1.270，vt100 0.16.2。
所有数字来自本机真实运行，不是估算，也不是构造的 fixture。

对应设计：[native-pty-first.md](../native-pty-first.md) §4.1 载体、§4.2 信号库、
§4.6 终端与快照、§10 规则表前置、§13 P0；风险表第 4 项（模拟器内存/CPU 未测量）。

---

## 1 模拟器选型：为什么是 vt100

§4.1 与 D-028 决策条目都点名 `wezterm-term`。本轮实测后**改选 `vt100`**，理由是
wezterm-term 不可用，而 alacritty 缺的恰好是 §4.6 最需要的那一件。

选型硬需求来自任务书与 §4.6：可配置 scrollback 行数、alt-screen（`?1049`）状态、
DECSET 跟踪（`?2004` / `?25` / 鼠标 `?1000`–`?1006`）、OSC 0/2 标题保留、
OSC 9;4 进度载荷保留、**grid→ANSI 重新渲染**。

| 需求 | `vt100` 0.16.2 | `alacritty_terminal` 0.26.0 | `wezterm-term` |
|---|---|---|---|
| crates.io 可获取 | ✅ | ✅ | ❌ **未发布**（`cargo info wezterm-term` → not found；只有第三方 fork `tattoy-wezterm-term 0.1.0-fork.5`） |
| 可配置 scrollback | ✅ `Parser::new(rows, cols, scrollback_len)` | ✅ `Config::scrolling_history`（默认 10000） | — |
| alt-screen 状态 | ✅ `Screen::alternate_screen()` | ✅ `TermMode::ALT_SCREEN` | — |
| DECSET `?2004` / `?25` | ✅ `bracketed_paste()` / `hide_cursor()` | ✅ `TermMode::BRACKETED_PASTE` 等 | — |
| 鼠标 `?1000`–`?1006` | ✅ `mouse_protocol_mode()` + `mouse_protocol_encoding()` | ✅ `TermMode` 位 | — |
| OSC 0/2 标题保留 | ✅ `Callbacks::set_window_title` | ✅ `EventListener` | — |
| OSC 9;4 进度保留 | ✅ 经 `Callbacks::unhandled_osc` 自取 | ⚠️ 需自行接 `EventListener` | — |
| **grid→ANSI 重绘** | ✅ **`state_formatted()` / `contents_formatted()` / `contents_diff()`** | ❌ **没有**（全仓无 `-> Vec<u8>` 的渲染出口；它渲染到 GPU，不渲染回字节） | — |
| 依赖数 | **3**（`itoa`、`unicode-width`、`vte`） | 11 直接依赖，含 `libc`、`parking_lot`、`polling`、`rustix`、`regex-automata`、`home`、`base64` | — |
| 额外负担 | 无 | 自带 `tty/` 与 `event_loop.rs`——我们已用 `portable-pty` 自持 PTY，这层是重复的 | — |

**结论**：

- `wezterm-term` **不在 crates.io 上**，只能走 git 依赖或第三方 fork。为一个进 P0
  默认路径的组件引入未发布依赖，代价不对等，本轮不采用。若后续确实需要它的
  能力（如更完整的 SIXEL / 图像单元），再作为独立决策评估。
- `alacritty_terminal` 状态面齐全，但**没有 grid→ANSI 渲染器**。§4.6 的快照就是
  「reset + 当前网格 + 当前模式集」这串字节，缺了它等于要自己写一个渲染器——那
  正是选库要省掉的工作。它还捆了一套 PTY 与事件循环，与 `portable-pty` 重复。
- `vt100` 三个依赖，需求逐项命中，`state_formatted()` 直接就是 §4.6 要的重绘。

已知取舍，如实记录：

- `vt100` 的 `Callbacks` 不解析 OSC 9;4，落到 `unhandled_osc`。本仓在
  `emulator.rs` 的 `OscSink` 里自取，payload 原样保留，解释权留给 §10 规则表。
- `vt100` **预分配**每一行（见 §3），内存因此由列宽决定，这直接约束了 scrollback 上限。
- `vt100` 无 SIXEL / 图像单元。三家 harness 的 TUI 目前都不用，需要时再评估。

---

## 2 A/B：字节 ring 快照 vs 合成重绘

`crates/remuda-driver/examples/screen_ab.rs` 在同一条 `shell-pty` 上跑两遍同一个
命令，一遍 `emulator=false`（字节 ring），一遍 `emulator=true`（合成重绘），各自取
attach 快照。

### 2.1 活的 `claude` 会话（claude 2.1.270，120×40）

同一条命令 `claude`，起好等 14 s 让 TUI 画完，然后取 attach 快照：

```
$ AB_CWD=… AB_SETTLE=14 screen_ab claude
[raw-ring] source=raw-ring bytes=3364 alt_screen=false
[repaint]  source=repaint  bytes=1790 alt_screen=true
```

**`alt_screen` 就是第一条结论**：claude 的 TUI 跑在 `?1049` 里，而 ring 路径报的是
`false`——它不是判断错，是**根本无从判断**，这正是 §4.6 要 Node 上报而不是让 web
猜的原因。web 侧因此会在 ring 路径上保持旧行为（滚轮劫持照旧），在重绘路径上把滚轮
交还给 TUI。

两份快照的**开头**：

```
ring     ^[[33m"passthrough/ark/seed-evolving" isn't described by this version's …
repaint  ^[[!p^[[?1049h^[[?25h^[[m^[[H^[[J
         ^[[38;2;215;119;87m▐▛███▙  ^[[39;49;1mClaude Code^[[C^[[38;2;153;153;153;22mv2.1.270
```

ring 从一句启动警告的**中间**开始——那只是 256 KiB 窗口恰好落在的位置。
重绘从软 reset 开始，进 alt buffer，然后画当前屏。

两份快照里的 **DECSET 计数**：

| 序列 | ring | repaint |
|---|---:|---:|
| `?1049h` | 1 | 1 |
| `?2004h` | 2 | 1 |
| `?25l`（藏光标） | 6 | 0 |
| `?25h`（显光标） | 6 | 1 |
| `?1000h` / `?1006h` | 1 / 1 | 经 `?1003h` / `?1006h` 各 1 |

ring 重放了 **6 次藏光标和 6 次显光标**：客户端最终是什么状态，取决于这 12 条在
截断窗口里的先后——「重放过期 DECSET」在实测里就是这个样子。重绘对每个模式只断言
一次当前值，且**模式在最后**（尾部是
`^[[m^[>^[[?1l^[[?2004h^[[?1003h^[[?1006h`），所以绘制过程不会把它们冲掉。

### 2.2 超出 ring 的长会话：重绘 vs 256 KiB 截断

6000 行构建输出（~400 KiB，超过 256 KiB ring）后进 alt-screen 画一帧：

```
[raw-ring] source=raw-ring bytes=262144 alt_screen=false
[repaint]  source=repaint  bytes=78     alt_screen=true
```

**262144 → 78 字节，约 3360×**。ring 被顶满，开头是
`one^M^M` ——上一行 `done` 被切掉了前三个字符。两份快照都含最终帧，但 ring 要客户端
先重放 256 KiB 的历史构建输出才能到达它，而屏幕上早就只剩那一帧了。

这条同时说明**为什么 scrollback 归客户端管**：重绘只有 78 字节，因为屏幕上只有一
行字；浏览器自己的 4000 行 scrollback 与字节 ring 都没变，用户往上滚仍然能看到构建
历史（§4.6「换成真字节后 scrollback 自然成立」）。重绘换掉的只是**重连那一刻画什么**。

### 2.3 自动化断言（`crates/remuda-node/tests/tty_repaint.rs`，真实 PTY）

四条，全部对真 PTY 跑：

| 场景 | 断言 | 意义 |
|---|---|---|
| flag off，重绘两帧的进程 | 快照里**仍有** `first frame` | ring 行为逐字节不变，这是 P0 的「零行为变更」 |
| flag on，同一进程 | 快照里**没有** `first frame`，以 `ESC[!p` 开头 | 重绘只带当前屏，不带造出它的历史（§4.6） |
| flag on，`?1049` 全屏 TUI | `altScreen == true`；快照**不含** alt 之前的 scrollback；含 `ESC[?1049h` | 只快照 alt 网格，并先把客户端切进 alt buffer |
| flag on，主屏 | `altScreen == false` | 不误报 |

`available_from` 的处理也在断言里：ring 切片仍是 `next_offset - len`（它确实占据
那段 offset），合成重绘是 `next_offset`（它不占 offset 空间，下一个 live 字节就是
客户端没见过的第一个）。

### 2.4 签名漂移：模拟器真正修掉的东西

`crates/remuda-driver/tests/screen_golden.rs` 里
`the_emulator_forgets_a_dialog_the_tui_painted_over_but_the_byte_tail_does_not`
把差异钉成测试而不是藏起来：

```
bytes = ESC[2J ESC[H "Is this a project you created or one you trust?"
        ESC[2J ESC[H "❯ ready for a prompt"

字节尾匹配器   → Blocked   （对话框已经被覆盖，但字节还在）
模拟器网格匹配 → Idle      （屏幕上只有 composer）
```

这就是 §4.1 说的「光标移动被丢弃导致的签名漂移」。**本期模拟器默认关，所以线上
仍是左边那个结果**——修复随 flag 一起到来，不随抽取到来。

---

## 3 内存 / CPU 基准与 scrollback 上限

### 3.1 成本的形状

`vt100` 的 `Grid::allocate_rows()` 在建表时就为每一行 `Row::new(cols)` 分配满
`Vec<Cell>`，且 `Cell` 是定长 32 字节（`assert!(size_of::<Cell>() == 32)`）。因此
一个满载模拟器的常驻内存约为：

```
(rows + scrollback) × cols × 32 B   （主网格 + 其 scrollback）
+ rows × cols × 32 B                （alt 网格，scrollback_len = 0）
```

**与内容无关，只与列宽有关。** 这决定了上限必须按列宽算，不能只按行数拍。

### 3.2 实测（N = 32 个模拟器，各自灌满 scrollback，rows = 40）

RSS 增量 ÷ N，`cargo build --release`：

| cols | scrollback | 每模拟器 | N=32 合计 | 灌满耗时/模拟器 |
|---:|---:|---:|---:|---:|
| 80 | 500 | 1.45 MiB | 46 MiB | 0.48 ms |
| 80 | 1000 | 2.77 MiB | 88 MiB | 0.61 ms |
| 80 | 2000 | 5.42 MiB | 173 MiB | 1.20 ms |
| 120 | 500 | 2.15 MiB | 69 MiB | 0.45 ms |
| 120 | 1000 | 4.13 MiB | 132 MiB | 0.85 ms |
| 120 | 2000 | 8.08 MiB | 259 MiB | 1.61 ms |
| 200 | 500 | 3.80 MiB | 122 MiB | 0.70 ms |
| **200** | **1000** | **7.31 MiB** | **234 MiB** | **1.39 ms** |
| 200 | 2000 | 14.31 MiB | 458 MiB | 2.49 ms |
| 400 | 500 | 7.57 MiB | 242 MiB | 1.31 ms |
| 400 | 1000 | 14.55 MiB | 466 MiB | 2.43 ms |
| 400 | 2000 | 28.54 MiB | 913 MiB | 7.19 ms |

解析成本：灌满 1000 行 200 列 ≈ **1.4 ms**，即一次「`cat` 一个大文件」级别的突发
在 PTY 读路径上花约一毫秒。模拟器坐在读路径上，这个量级可以接受。

### 3.3 选定的上限

**`DEFAULT_SCROLLBACK_LINES = 1000`，外加 `MAX_COLS = 400` / `MAX_ROWS = 200` 的
尺寸钳制。** 预算：单实例 8 MiB，整队 256 MiB（`maxInstances` 设计上限 32）。

- 1000 行 @ 200 列：7.7 MiB/实例，32 个 **247 MiB** —— 在 256 MiB 内，余量不大。
- 2000 行 @ 200 列：32 个 **472 MiB** —— 超出，故**否决**了设计文档举例的 2000。

`crates/remuda-screen/tests/budget.rs` 把这几条钉成断言，其中一条专门断言
「2000 行仍然超预算」，以便有人调高上限时**立刻失败**而不是悄悄超支。模型带
1.15 的开销系数，是对上表实测/计算比值（1.01–1.13）取的上界，所以断言算的是进程
真实分配量，而不是理想的 cell 算术。

### 3.4 一个如实记录的缺口

**32 个 400 列模拟器 = 494 MiB，放不进 256 MiB 预算。** 行数上限治不了这个：成本
按 cell 算，行数不知道行有多宽。

本期判断为可接受并**写成断言而非注释**
（`the_absolute_worst_case_fleet_is_over_budget_and_that_is_recorded_not_hidden`）：
`maxInstances` 默认是 8（即便 400 列也只有 124 MiB，另有一条断言守住这个**用户真
能触达**的情形），而 32 路并发 400 列会话不是真实配置。真要治，应当改成**字节预算**
——scrollback 由列宽反推，使乘积恒定。那比 P0 该做的范围大，故记录在此，不在本期
猜着改。

---

## 4 零行为变更的证据

P0 的验收是「抽取零行为变更」。三层证据：

1. **原有单测原样通过**：`promote.rs` 与 `pty_interaction.rs` 里所有既有测试一行
   未改，直接跑在委托适配器上（`cargo test -p remuda-driver --lib`，105 passed）。
2. **golden 对拍**：`tests/screen_golden.rs` 把每个有记录来源的屏幕形状同时喂给
   driver 适配器、`remuda-screen` 网格 API、以及真模拟器，断言三者结论一致。
3. **快照 A/B**：§2.2 的第一条断言就是「flag off 时快照仍是原来的 ring」。

适配器保持的边界也是逐字的：`screen_status` 仍取最后 ~8 KiB **字节**，
`detect_from_screen` 仍取最后 4096 **字符**——两个不同的口径，因为原代码就是两个
不同的口径，统一它们会改变宽字符屏幕上的判定。

---

## 5 本期交付与未交付

**交付**（§13 P0）：

- `crates/remuda-screen`：`ansi` / `grid` / `signature` / `dialog` / `emulator`，
  零 herdr 依赖，输入是渲染网格。
- `REMUDA_PTY_EMULATOR` 开关（默认关），PTY 全部输出同时喂 ring 与模拟器。
- attach 快照在开关打开时改为合成重绘；任何模拟器异常（锁中毒、空重绘）回落
  ring 并**打 warn 日志**，`PtySnapshot.source` 带出处。
- `TtyAttachResult.altScreen` 一个可选字段（D-016 线格式其余不变），Hub 以
  `tty.mode` 转发，web 据此关掉本地滚轮劫持，鼠标上报路径不动。
- 内存/CPU 基准与上限守卫。

**未交付，留给后续期**：

- §10 的 TOML 规则表引擎。本期只把匹配器搬进一个地方并给了 `flat()` /
  `bottom_non_empty_lines()` 这些 region 原语；规则仍是硬编码短语，P7 再换表。
- grok / agy 的屏幕签名覆盖率仍然为零（风险表第 12 项），本期未动。
- `?1049` 之外的 alt-screen 细节（如 `?47` / `?1047`）未处理：三家 harness 都用
  `?1049`，需要时再补。
