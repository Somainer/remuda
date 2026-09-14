# `fake-harness`：确定性 TUI 替身（D-028 P7）

- 决策：[`native-pty-first.md`](./native-pty-first.md) D-028 §12 item 7 / 阶段 P7
- 实现：`crates/remuda-testing/src/fake_harness/`，二进制 `crates/remuda-testing/src/bin/fake-harness.rs`
- 夹具：`crates/remuda-testing/fixtures/fake-harness/`
- 测试：`crates/remuda-testing/tests/fake_harness.rs`、`tests/golden_screens.rs`

`fake-harness` 是一个脚本驱动的二进制，后续 P2（AgentPty）、P3（流式）、
P4（steer/queue）、P5（审批）、P6（parity gate）都把它**放进真实 PTY**
而不是 mock。它在 Remuda 实际观测的层面模仿三个真实客户端：屏幕方言、
OSC/DECSET 字节、磁盘 artifact、钩子 stdin/stdout、以及逐键输入语义。
模型本身从不执行——工具只是“运行”一段可配置的墙上时间。

## 1 为什么需要它

P7 parity gate 要把 `print(fake)` 与 `agent-pty(fake)` 的事件流做 diff；
没有一个确定性的对端，队列边界、审批回落、Esc 中断这些只在真实 TTY
里出现的时序就无法反复验证。`fake-claude` 是 stream-json（无 TUI、无
排队、无原生审批），覆盖不了这一层；`fake-harness` 正是它在 native PTY
方向的后继，`§12` 明确两者都保留到 print 退役。

## 2 运行

```sh
fake-harness --kind claude|codex|grok [选项]
```

| flag | 含义 |
|---|---|
| `--script <file>` | 场景文件（`.json`，或 `.yaml`/`.yml`）；缺省内置 RUN_TOOL/SLOW 场景 |
| `--settings <file>` | claude 风格 `--settings` 钩子 overlay |
| `--setting-sources <list>` | 接受生产 shim 的参数；fake 仍只读取 `--settings`，不加载宿主配置 |
| `--home <dir>` | harness home：claude 项目目录 / codex `CODEX_HOME` / grok `GROK_HOME` 的落点 |
| `--cwd <dir>` | 会话上报的工作目录（默认真实 cwd，取 canonicalize） |
| `--session-id <id>` | 固定会话 id（默认 `00000000-…-001`） |
| `--resume <id>` | 续写既有 artifact 集（不重写 session 头） |
| `--model <label>` | 模型标签 |
| `--trust-directory` | 启动先显示首次信任目录对话框 |
| `--no-alt-screen` | 不进 alt screen（对应 codex/grok 的 `--no-alt-screen`） |
| `--cols/--rows` | 取不到 winsize 时的回退尺寸 |
| `--epoch-ms <i64>` | 钉死确定性时钟起点（Unix 毫秒，测试用） |
| `--events-out <file>` | 语义事件 JSONL（测试断言通道，见 §7） |

环境变量 `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GROK_HOME` 仅在未传
`--home` 时作为 home 回退。退出：`/exit`（claude、grok）、`/quit`
（codex）、grok 双击 Ctrl+Q、EOF、或收到 SIGTERM/SIGHUP/SIGINT。

## 3 场景格式

JSON（默认）或 YAML（按扩展名）。未知字段直接报错，避免拼写错误静默
失效：

```json
{
  "turns": [
    {
      "match_prefix": "SLOW",
      "thinking": "先想一下。",
      "think_chunks": 2,
      "text": "SPIKE_COMPLETE SLOW",
      "chunks": 3,
      "chunk_delay_ms": 8,
      "tools": [
        {
          "name": "Bash",
          "input": { "command": "sleep 20", "timeout": 60000 },
          "approval": "auto",
          "duration_ms": 700,
          "exit_code": 0
        }
      ],
      "stop_reason": "end_turn",
      "usage": { "input_tokens": 100, "output_tokens": 20 }
    }
  ],
  "quit_after_turns": 1
}
```

- `match`（精确）/ `match_prefix`（前缀）选轮；都不设即 catch-all，
  按声明顺序消费一次。提交的 prompt 谁都不匹配时走内置 echo 轮
  （回复 `SPIKE_COMPLETE <prompt>`）。
- `input` 可以是字符串，自动包成 `{"command": "…"}`。
- `approval`：`auto`（默认，直接跑）、`ask`（原生屏幕对话框，等按键）、
  `hook`（阻塞等 PermissionRequest/PreToolUse 裁决，无钩子回落到屏幕）、
  `hook_required`（无钩子裁决即拒绝）。
- `chunks`/`think_chunks` 控制流式分块；`chunk_delay_ms` 是块间真实
  sleep，屏幕与 artifact 都按块推进。
- `quit_after_turns` 到数自动退出，方便无人值守测试。

夹具见 `fixtures/fake-harness/scenarios/`（`ok` / `approval` / `slow` /
`hooks` / `demo.yaml`）。

## 4 屏幕方言

`fake_harness::screen` 产出两类东西：

1. **进入字节** `enter()`：`ESC[?1049h` alt screen、`ESC[?2004h`
   bracketed paste、`OSC 0` 标题、`OSC 9;4` 进度；
2. **重绘字节** `repaint()`：home-clear + 逐行绝对定位 + 光标；
   `render_grid()` 是同样内容的纯文本网格（底部锚定、按列裁剪），
   golden 快照对的是它，不是 ANSI 转义。

逐方言锚点字符串全部来自实测证据：

- **claude**：`❯` 输入框、`esc to interrupt`、信任目录
  （`Is this a project you created or one you trust?` /
  `Yes, I trust this folder`，与 `promote.rs`、`pty_interaction.rs`
  的检测器一致）、`✻/⏳ … (esc to interrupt)` 工作行。
- **codex**：`›`、`Working (Ns • esc to interrupt)`、steer 提示
  `Messages to be submitted after next tool call…`、Tab 队列
  `Queued follow-up inputs`、审批
  `› 1. Yes, proceed (y)` /
  `3. No, and tell Codex what to do differently (esc)` /
  `Press enter to confirm or esc to cancel`，以及中断后的
  `Conversation interrupted - tell the model what to do differently.`。
- **grok**：`❯` 带边框 composer、footer 随状态切
  `Enter:queue | Ctrl+Enter:send now` / `Enter:send now | Ctrl+;:queue`
  /空 composer `Enter:submit`、`[stop]` chip（D-028 §10 指定的锚点，
  不用盲文 logo 当状态源）、`Press Ctrl+c to cancel the turn`、
  四选一权限框（`1 (●) Yes, and don't ask again…` …
  `4 (○) Never allow: …`）、标题 `⚠ Action Required - ⠦ - …`、
  `⠸ - Running: <tool> - grok`、`OSC 9;4;1;-1`。

golden 覆盖三种方言 × idle/working/approval/trust × 80×24 与 40×20
（`fixtures/fake-harness/golden/`）。改渲染后
`UPDATE_GOLDEN=1 cargo test -p remuda-testing --test golden_screens`
重新生成。

## 5 磁盘 artifact

写入即 append+flush，生产 tail（`TranscriptTail` / `RolloutTail` /
`SessionTail`）按同样方式读到整行。字段形状对齐被合并解析器消费的
实测会话（`docs/design/evidence/`）。

### claude → `$home/projects/<encode_project_dir(cwd)>/<session>.jsonl`

- 一条 assistant 记录只装一个 content block，带 `apiBlockIndex`；
  `text` / `thinking` / `tool_use` 分记录，共享 `message.id`，
  含 `message.usage`；
- `user` 文本与 `tool_result`（`tool_use_id` / `is_error` /
  `sourceToolAssistantUUID` / `toolUseResult`）；
- `queue-operation`：`enqueue` / `remove`（`reason:"absorbed_mid_turn"`）
  / `dequeue`；
- `attachment`：`type:"queued_command"`，其 `timestamp` 保留**入队**
  时刻，即使物理上写在 tool_result 之后（实测里就是乱序的，适配器
  必须按文件顺序读，不能按时间戳全局排序）。

### codex → `$home/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` + `session_index.jsonl`

- `session_meta`（ordinal 0）、`event_msg/task_started`、
  `turn_context`、`response_item/message`（user/assistant）、
  `reasoning`、`function_call`（模型名 `exec_command`，参数是 JSON
  字符串）、`event_msg/item_completed`（含 `CommandExecution`）、
  `function_call_output`、`token_usage_record`（顶层 envelope，
  非 event_msg）、`event_msg/task_complete`；
- `turn_aborted {reason:"interrupted"}`；Esc 后迟到的工具结果仍挂
  **旧 turn_id**（证据 A5 ordinal 86）；
- rollout 里**没有** steer/queue 的 enqueue 标签——投递的 provenance
  只能靠 turn_id 区分，fake 不发明记录（§“刻意不做”）。

### grok → `$home/sessions/<percent-encode(cwd)>/<id>/{updates,events}.jsonl` + `active_sessions.json`

- updates：`session/update` 的 `user_message_chunk` /
  `agent_thought_chunk` / `agent_message_chunk` / `tool_call` /
  `tool_call_update`（`rawOutput.exit_code`），以及
  `_x.ai/session/update` 的 `turn_completed`（`stop_reason`
  `end_turn`/`cancelled`）；`eventId` 跨轮单调；
- events：`turn_started`（`turn_number`/`model_id`/`yolo_mode`）、
  `phase_changed`、`first_token`、`tool_started/completed`、
  `permission_requested/resolved`、`turn_ended`（`outcome`
  completed/cancelled + `cancellation_context.trigger`）；
- `active_sessions.json` 存活期一条（真实 TUI PID，发现路径，**不是
  liveness**），关闭前先移除成 `[]`（证据 A2）。

集成测试直接用 `remuda_driver` 的三个生产解析器解析这些文件，并用
`locate_rollout_in` / `locate_session` 反查。

## 6 钩子

- **claude**：只加载 `--settings` 指向的一个 JSON overlay
  （`{"hooks":{Event:[{"hooks":[{"command","timeout"}]}]}}`）。
- **codex**：`$home/hooks.json` 同形状。
- **grok**：合并 `$home/hooks/*.json`；`PermissionRequest` 注册时被
  **静默忽略**——和真实 1.0.30 一样（证据 A3）。

命令经 `/bin/sh -c` 执行，payload JSON 走 stdin，公共字段
`session_id`/`sessionId`/`cwd`/`hook_event_name`(PascalCase)/
`hookEventName`(snake_case)/`timestamp`。grok 额外注入
`GROK_HOOK_EVENT`/`GROK_SESSION_ID`/`GROK_WORKSPACE_ROOT`/
`CLAUDE_PROJECT_DIR`。stdout 里的裁决对象被识别三种写法：

| 方言 | 接受的 stdout |
|---|---|
| claude | `{"behavior":"allow"|"deny"}`，或 `permissionDecision` |
| codex | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"…"}}}` |
| grok | `{"decision":"allow"|"deny"|"ask", "reason":"…"}`（PreToolUse） |

事件：`SessionStart`（new/resume 都发，claude 包含 `transcript_path` 供生产绑定）、`UserPromptSubmit`
（claude 在**入队时**就触发，不是投递时）、`PreToolUse`、
`PermissionRequest`（阻塞，受 handler `timeout` 约束）、`PostToolUse`
（仅成功工具；被中断的工具不发，对齐证据）、`MessageDisplay`
（claude 每行：`turn_id`/`message_id`/`index`/`final`/`delta`）、
`Stop`、`SessionEnd`。codex PermissionRequest 的 stdin **没有**
`tool_use_id`、工具名归一为 `Bash`（证据 A1）。

`fixtures/fake-harness/hooks/hook.sh` 是测试用的 POSIX shell 钩子：
追加 stdin+env 到 `$FAKE_HARNESS_HOOK_LOG`，权限事件可阻塞在
`$FAKE_HARNESS_HOOK_WAIT_FILE` 出现后再按
`$FAKE_HARNESS_HOOK_DECISION` / `$FAKE_HARNESS_HOOK_STYLE` 返回裁决。

## 7 输入语义（逐读边界，和证据一致）

字节在 raw mode 下读取，`fake_harness::input::Parser` 以**一次 read**
为单位解码：

- 独立一次 `\r` 读 = Enter 提交；**body 与 `\r` 同一次 read 一律不提交**
  （paste burst，CR 被吞）——`body\r` 后必须再来一次独立 `\r`；
  测试 `claude_body_and_cr_in_one_write_does_not_submit` 钉死；
- bracketed paste `ESC[200~ … ESC[201~]`：内嵌 CR 变换行，不提交；
  可跨 read；
- 独立 `\x1b` = Esc；`\x1b[A/B` = 上下箭头；`\t`/`\x03`/`\x11`/`\x18`
  分别是 Tab/Ctrl+C/Ctrl+Q/Ctrl+X。

| 方言 | 工作中 Enter | 排队键 | 中断 |
|---|---|---|---|
| claude | 入**原生队列**，UserPromptSubmit 此刻触发；在**工具结果之后、下一工具之前**消费（`remove absorbed_mid_turn` + `queued_command`） | Tab 无效（Ctrl+X+Enter 同样入队） | **Esc**：打断请求/工具（错误结果 `User rejected tool use`），队列**存活**，dequeue 后作为新 user 记录自动续跑 |
| codex | 立即 **steer** 进当前 turn（共享 `turn_id`，无 enqueue 记录） | **Tab** 排下一轮（当前 task_complete 之后才开新 turn；idle 时 Tab 直接提交） | **Esc**：`turn_aborted reason=interrupted`，已启动的工具结果可迟到挂旧 turn；不杀“后台工具” |
| grok | 默认**排下一轮**（无原生 enqueue provenance） | Enter 即队列 | **Esc 不是中断**：turn 与草稿都保留，只提示；**第一次 Ctrl+C 只清草稿**，**双击 Ctrl+C** 才取消（`trigger=ctrl_c`） |

grok 的唯一 send-now：工作中 Enter 排队后，在**空 composer** 上再按
Enter = 取消当前 turn（`turn_ended outcome=cancelled
trigger=send_now`）并**立刻**发送队首（下一 turn 带
`redirect_kind=queued_after_cancel`）。fake 不会偷偷实现
Ctrl+Enter 物理 send-now——证据表明该组合键未单独验证。

## 8 `--resume` / `--session-id`

- `--session-id` 固定 id；不传用默认常量。
- `--resume <id>` 打开既有主文件**追加**：claude 按
  `projects/<enc cwd>/<id>.jsonl`，codex 用
  `locate_rollout_in(home,id)`，grok 用
  `sessions/<enc cwd>/<id>/`；找不到即报错退出。
- resume 不重写 `session_meta`（codex 集成测试断言全文只有一条）、
  不重建 registry 之外的初始化，grok 的 `turn_number` 从旧 events
  续号，SessionStart 以 `source:"resume"` 触发。

## 9 刻意**不**仿真的部分

这些都在 parity gate 的白名单里，fake 明确不做，避免测试给人虚假的
信心：

- 不发起任何模型/网络请求，不真正执行命令；工具只是墙上时间；
- 不实现 codex 0.154 的 hook trust gate（canonical hash /
  `hooks.state`）——`$CODEX_HOME/hooks.json` 里的命令直接执行；
- 不实现 grok 的 Claude 配置串读与 `GROK_CLAUDE_HOOKS_ENABLED` 开关；
- 不实现鼠标、富文本 markdown 渲染、完整颜色表、滚动查看器、
  `/help` 之外的斜杠命令、`AskUserQuestion` 之外的 MCP UI；
- 不实现 Ctrl+Enter 物理 send-now、Ctrl+; 队列等未实测物理组合键
  （footer 文案照实显示）；
- 不按真实客户端版本做功能差异分支；版本字符串是固定的
  2.1.270 / 0.154.0 / 1.0.30。

## 10 测试与刷新

```sh
cargo fmt --all
cargo clippy -p remuda-testing --all-targets --locked -- -D warnings
cargo test -p remuda-testing --locked
./scripts/ci/secret-scan.sh
```

- 单测：场景解析、时钟、输入 chunk、屏幕渲染、钩子裁决解析。
- `tests/golden_screens.rs`：18 张网格快照。
- `tests/fake_harness.rs`：真实 portable-pty 驱动（独立的 reader
  线程持续排空屏幕，断言只看 artifact 与 `--events-out`），20 个用例
  串行执行（`support::serial()`）以消除真实 sleep 下的调度抖动；
  事件游标只前进到命中的那一行，避免“先 wait A 时把 B 冲掉”。
- 语义事件名（`--events-out`）：`submit` / `turn_start` / `enqueue` /
  `steer` / `queue` / `boundary_deliver` / `approval_prompt` /
  `approval_decision` / `interrupt`（`by=esc|ctrl_c|send_now`）/
  `ctrl_c_clear_draft` / `esc_notice` / `turn_end` / `exit` /
  `empty_submit` / `trust_accepted`。这是测试便利通道，不属于三个
  真实 harness 的协议面。

## 11 Hub-live Playwright 命名约定

Hub-backed（fake node）的 Playwright 规格一律命名为 `*.hub.spec.ts`；`web/playwright.hub.config.ts` 的 `testMatch` 会自动收录该后缀，**新增 hub 规格时不要编辑该配置**（旧名仍在显式列表中，仅用于历史兼容）。
