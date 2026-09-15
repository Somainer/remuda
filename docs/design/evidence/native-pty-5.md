# native-pty-5 · 审批裁决（D-028 P5）实机证据

日期：2026-09-14 · 主机 `devbox-sg` · `claude` **2.1.221**
范围：D-028 §4.4 tier A（hook 阻塞裁决）、§14 风险 1（allow 被忽略时回落按键）
方法：真实 `claude` 跑在真 PTY 里（`pty.openpty` + 120×40），throwaway settings
overlay 把 hook 指到一个录制脚本；脚本把 stdin 原样存盘，并按 env 返回裁决。
工作目录 `/tmp/remuda-p5/`，不写任何用户配置。

> 本页的每一行结论都来自下面列出的那一次运行。凡是**没跑到**的，标 **[U]**，
> 不写成结论。

---

## 0 一句话结论（先说最要紧的）

**设计文档 §3.1 记的 `{"behavior":"allow"｜"deny"}` 裸形状在 2.1.221 上是
无效的**——它被静默丢弃：审批框照样弹出、工具被拒、回复里没有任何出错迹象。
真正生效的是嵌套形状：

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
                       "decision":{"behavior":"allow"}}}
```

这正是 §14 风险 1 描述的那种失败（「点了批准但没动」），只是成因不是 confined
session，而是**形状不对**。因此 P5 的实现：**只发嵌套形状**，两种形状都**能读**
（别人的 hook 可能那么写），并且把「裸形状被忽略」写成一条**测试**而不是注释
（`crates/remuda-signal/src/decision.rs`
`a_decision_never_uses_the_key_that_is_silently_ignored`）。

---

## 1 环境与方法

| 项 | 值 |
|---|---|
| claude | 2.1.221（`claude --version`） |
| 载体 | 真 PTY，120×40，`TERM=xterm-256color` |
| overlay | `/tmp/remuda-p5/settings.json`，注册 8 个事件指向 `rec.sh` |
| 投递 | 正文与 `\r` **分两次写**（§5.2），等 1.5 s |
| 权限模式 | `--permission-mode default`（关键，见 §1.1） |
| 清理 | 每次运行前删 `captures/`，每次用新的 workdir |

`rec.sh` 把 stdin 存成 `captures/<Event>.<pid>.json`，只对
`$REMUDA_P5_EVENT` 指定的那个事件返回 `$REMUDA_P5_DECISION`，其余一律 `{}`——
否则会撞上 §5 记的那条「事件名不匹配就整条丢弃」。

### 1.1 headless 不会问：`-p` 模式下 `PermissionRequest` 根本不触发

先踩的坑，记下来省得别人再踩：

- `claude -p "...Write..."`（无 `--permission-mode`）→ 继承用户
  `defaultMode: auto`，直接放行，**只有 `PreToolUse`/`PostToolUse`**。
- `claude -p ... --permission-mode default` → **也不问**，模型直接回
  「The file write needs your permission to proceed」然后结束；
  `captures/` 里依旧没有 `PermissionRequest`。

**结论：`PermissionRequest` 只在交互式 TUI 里触发。** 录制它必须开真 PTY。

---

## 2 `PermissionRequest` 实机 payload（2.1.221）

一次 `Write` 审批，逐字段原样（仅路径为临时目录）：

```json
{
  "session_id": "fb571289-9767-4bf2-9354-131360aff059",
  "transcript_path": "/home/<user>/.claude/projects/-tmp-remuda-p5-p2/fb571289-….jsonl",
  "cwd": "/tmp/remuda-p5/p2",
  "prompt_id": "bef8e47f-8c22-4eca-b8f2-139d6ea37195",
  "permission_mode": "default",
  "effort": {"level": "xhigh"},
  "hook_event_name": "PermissionRequest",
  "tool_name": "Write",
  "tool_input": {"file_path": "/tmp/remuda-p5/p2/probe.txt", "content": "hello"},
  "permission_suggestions": [
    {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
  ]
}
```

三条**必须**记住的事实：

1. **[V] 有 `tool_input`**——这是 tier A 相对屏幕反推的全部价值：审批卡显示的是
   真实入参，不是 TUI 渲染出来的样子（§2.2）。
2. **[V] 没有 `tool_use_id`**。同一次工具调用的 `PreToolUse` **有**
   （`toolu_vrtx_013V5WcfuA7zCFZXUD4AhbeU`），`PermissionRequest` **没有**。
   → 任何拿 `tool_use_id` 当关联键的实现会**丢掉每一条审批**。Remuda 因此自己
   铸 key，并且直接复用 `InteractionId`（`request_key.native = Hook{…}`）。
3. **[V] 有 `permission_suggestions`**——这就是 allow-always 按钮的来源；
   Remuda 不解释内容，原样回传（§4）。

同屏的审批框（去 ANSI 后）：

```
Do you want to create probe.txt?
❯ 1. Yes
  2. Yes, allow all edits during this session (shift+tab)
  3. No
  Esc to cancel · Tab to amend
```

---

## 3 裁决形状：裸的无效，嵌套的有效

### 3.1 [V] 裸 `{"behavior":"allow"}` → **被忽略**

```
relay: 20:34:04 PermissionRequest replied={"behavior":"allow"}
screen: Do you want to create probe.txt?   ← 框还在
        ⎿  User rejected write to probe.txt
file:   probe.txt 不存在
```

回复里没有任何报错。**这就是「点了批准但没动」**。

### 3.2 [V] 嵌套形状 → **生效，且不弹框**

```
relay: 20:29:08 PermissionRequest replied={"hookSpecificOutput":{"hookEventName":
       "PermissionRequest","decision":{"behavior":"allow"}}}
screen: ⎿  Allowed by PermissionRequest hook
        ⎿  Wrote 1 line to probe.txt
file:   probe.txt 存在，内容 hello
```

### 3.3 为什么——二进制里的读取端

`claude.exe` 里通用 hook 输出处理器按 `hookSpecificOutput.hookEventName` 分支，
只有 `PermissionRequest` 那一支会赋值 `permissionRequestResult`：

```js
case "PermissionRequest":
  if (e.hookSpecificOutput.decision) {
    u.permissionRequestResult = e.hookSpecificOutput.decision,
    u.permissionBehavior = e.hookSpecificOutput.decision.behavior === "allow" ? "allow" : "deny",
    …decision.updatedInput → u.updatedInput
  }
```

裸的 `behavior` 键**到不了这一支**。消费端同样只看
`h.permissionRequestResult`（`runHooks` 内）。

### 3.4 [V] 事件名必须回声，否则整条丢弃

早期一次运行给**所有**事件返回了同一份 `PermissionRequest` 裁决，屏幕上直接报：

```
⎿ PreToolUse:Write hook error
⎿ Failed to run: Hook returned incorrect event name:
  expected 'PreToolUse' but got 'PermissionRequest'.
```

→ `HookDecision::to_hook_json(event)` 因此**必须**带事件名参数。

---

## 4 四种裁决，逐条实测

| # | 发出的裁决 | 屏幕 | 文件 | 结论 |
|---|---|---|---|---|
| 1 | 嵌套 `allow` | `Allowed by PermissionRequest hook`，**无审批框** | 已写 | **[V] allow 生效** |
| 2 | 嵌套 `deny` + `message` | `Error: denied by remuda p5 test` / `Denied by PermissionRequest hook` | 未写 | **[V] deny 生效，且 message 原样透出** |
| 3 | 嵌套 `allow` + `updatedPermissions` | 见下 | 两个都写 | **[V] allow-always 生效** |
| 4 | 裸 `allow` | 审批框仍在 → `User rejected` | 未写 | **[V] 被忽略**（§3.1） |

### 4.1 [V] allow-always 的判据是「第二次不再问」

提示词要求依次建两个文件。回传：

```json
{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":
 {"behavior":"allow","updatedPermissions":[
   {"type":"setMode","mode":"acceptEdits","destination":"session"}]}}}
```

relay 日志（关键在于 `PermissionRequest` **只出现一次**）：

```
20:36:23 PreToolUse          ← 第一个 Write
20:36:24 PermissionRequest   ← 唯一一次审批
20:36:24 PostToolUse
20:36:29 PreToolUse          ← 第二个 Write：没有 PermissionRequest
20:36:29 PostToolUse
20:36:33 Stop
```

底栏从 `⏸ manual mode on` 变成 `⏵⏵ accept edits on`，两个文件都建成。
**把 suggestion 原样回传就够了**，Remuda 不需要理解它——这也是为什么
`PermissionSuggestion` 存的是 `serde_json::Value`。

---

## 5 超时：谁先放弃，以及为什么超时必须是 deny

### 5.1 [V] hook 挂起 75 s，claude 一直等，晚到的 allow 仍然生效

`REMUDA_P5_SLEEP=75`：

```
20:37:45 PreToolUse
20:39:00 PermissionRequest replied={…allow…}   ← 75 s 后才回
screen:  审批框在这 75 s 里一直显示
file:    probe.txt 已写
```

**两条结论，都对实现有约束：**

1. **[V] 审批框与「hook 还在等」是并存的**。所以**不能**用「屏幕上有框」当作
   「hook 没生效」的判据——那会在每一次慢审批上重复作答（hook 一次、按键一次）。
   回落只能由 hook 路径**自证未生效**来触发（`Outcome::Abandoned` /
   `TimedOut`），这就是 `hook_answer::needs_fallback` 的全部理由。
2. 晚到的裁决**仍然会被应用**，所以「Remuda 放弃等待」不等于「agent 放弃等待」。

### 5.2 claude 自己的 hook 超时是 600 000 ms

二进制里 `var hh=600000`，作为 `executePermissionRequestHooks` 的默认
`timeoutMs`；hook 注册项支持 `timeout`（秒）覆盖。
Remuda 的 `BLOCKING_WAIT` 对齐 broker TTL = **900 s** > 600 s，
→ **通常是 claude 先不等**。因此超时不能只是「不回答」：
`remuda hook emit` 在**够到了 Node 但没等到裁决**时主动打印 deny（fail closed），
够不到 Node 时才打印 `{}`（把决定权留给 agent 自己的框）。
两者不能混为一谈——后者若也 deny，Node 一停整机所有工具调用都会被拒。

**[U]** 未验证：claude 侧 600 s 真正触发后的行为（跑满 10 分钟未做）。

---

## 6 Elicitation

**[U] 未取得实机 payload。** 本轮没有构造出会触发 `Elicitation` 的 MCP 服务器。
二进制里该事件的读取端与 `PermissionRequest` 同族、形状已确认：

```js
case "Elicitation":
  if (e.hookSpecificOutput.action) {
    u.elicitationResponse = {action: …, content: …},
    action === "decline" → blockingError
  }
```

→ 实现按此形状发 `{hookSpecificOutput:{hookEventName:"Elicitation",action,content}}`，
并在代码与本页都标注为**未实机验证**。取得真实 payload 前不得宣称已打通。

---

## 7 落到实现的对应关系

| 证据 | 代码 | 测试 |
|---|---|---|
| §3 嵌套形状生效、裸形状无效 | `remuda-signal/src/decision.rs` `to_hook_json` | `allow_is_emitted_in_the_nested_shape_the_harness_actually_reads`、`a_decision_never_uses_the_key_that_is_silently_ignored` |
| §3.4 事件名回声 | `to_hook_json(event)` 带参 | `the_event_name_is_echoed_because_a_mismatch_drops_the_decision` |
| §2 真实 `tool_input` 进卡片 | `remuda-signal/src/approval.rs` | `the_card_shows_the_real_tool_input_and_is_carried_by_the_hook` |
| §2 无 `tool_use_id` | 自铸 key = `InteractionId` | `the_recorded_request_has_no_tool_use_id_so_nothing_may_require_one` |
| §4.1 allow-always 原样回传 | `HookDecision::Allow{updated_permissions}` | `allow_always_hands_the_suggestion_back_verbatim` |
| §5.1 框与 pending 并存 | `hook_answer::needs_fallback` | `only_a_confirmed_hook_delivery_ends_the_story` |
| §5.2 超时 deny vs 够不到 | `remuda/src/cmd/hook.rs` `decide` | `an_approval_nobody_answered_in_time_denies`、`an_absent_node_leaves_the_decision_to_the_agents_own_dialog` |

---

## 8 回落按键：一次，且必须看到框消失

回落只在 hook 路径自证未生效时武装（§5.1），并且：

- **每个 interaction 只有一次机会**，写之前就记账（D-022：ACK 丢了也不重放，
  否则回车会落到**当下**那个框上，可能是另一个问题）。
- **截断 / 歧义 / 不是审批框 → 一个键都不按**。model picker 和审批框长得像
  （§10 锚点③）。
- **写完必须观察到框消失**才算 applied，否则如实报 `not-dispatched`。
  「写出去了」不是「被接受了」。

按键是**光标相对**的（`enter` / `down,enter` / `down,down,enter`），因为带 `❯`
的菜单不接受数字键——这一点沿用 `remuda-screen` 既有实现。
另外 allow-once **绝不能**落在 `2. Yes, allow all edits during this session`
上：它同样以 Yes 开头，按下去等于悄悄授予会话级权限。

**[U]** confined session 本身未单独复现（2.1.221 二进制里没有「confined
session」字样）。§14 风险 1 的「allow 被忽略」在本轮由**形状错误**这一条真实
路径复现并覆盖，回落逻辑对两者是同一条。

---

## 9 复现

```bash
mkdir -p /tmp/remuda-p5/w && cd /tmp/remuda-p5
# rec.sh 存 stdin，按 $REMUDA_P5_EVENT 返回 $REMUDA_P5_DECISION
REMUDA_P5_EVENT=PermissionRequest \
REMUDA_P5_DECISION='{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}' \
python3 drive.py /tmp/remuda-p5/w \
  "Use the Write tool to create probe.txt containing the single word hello" \
  --settings /tmp/remuda-p5/settings.json --permission-mode default
```

`drive.py` 开 PTY、分两次写正文与回车、抓原始字节到 `pty.log`。
把 `REMUDA_P5_DECISION` 换成裸 `{"behavior":"allow"}` 即可复现 §3.1 的忽略。
