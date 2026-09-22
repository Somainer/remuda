# askq-pretooluse-1 · auto 模式下 AskUserQuestion 是 PreToolUse，hook 答复形状

日期：2026-09-23 · `claude` **2.1.277**（devbox）
范围：c-askq —— auto permission mode 下 `AskUserQuestion` 的 hook 事件序列、
手机卡片缺卡根因、以及 `PreToolUse` 裁决回填 `updatedInput.answers` 是否被 harness 接受
方法：真 `claude` 跑在真 PTY（`pty.fork`，`TERM=xterm-256color`），throwaway settings
overlay 只注册 `PreToolUse` / `PermissionRequest` / `PostToolUse` 三个录制钩子；
录制脚本把 stdin 逐字存盘。API 走本机配置的 relay 路由模型。全部工作目录在
`/tmp/remuda-askq/`（录制后已删），不写用户配置。

> 本页每条机器结论都来自下列两轮录制之一；没跑到的标 **[U]**。

---

## 0 一句话结论

auto 模式里 `AskUserQuestion` 触发的是 **`PreToolUse` hook**（带
`tool_use_id`，`permission_mode: "auto"`），而**不是** `PermissionRequest`。
它用的是 PreToolUse 自己的裁决词汇：

```json
{"hookSpecificOutput":{"hookEventName":"PreToolUse",
  "permissionDecision":"allow",
  "updatedInput":{ …原 tool_input…,
    "answers":{"<问题原文>":"<label>"}}}}
```

实机验证该形状被接受：问题在 TUI 完全不渲染，transcript 直接写入
`Your questions have been answered: "Tea or coffee?"="Tea"`。Remuda 之前只在
`PermissionRequest` 分支铸问题卡，所以所有者最常用的 auto 模式永远收不到卡
（journal 实测：该次会话只有 PreToolUse、没有 PermissionRequest）。

---

## 1 环境

| 项 | 值 |
|---|---|
| claude | 2.1.277（`claude --version`） |
| 载体 | 真 PTY（`pty.fork`），throwaway `--settings` overlay |
| 权限模式 | `--permission-mode auto`（TUI 状态栏：`auto mode on`） |
| 钩子 | `PreToolUse`/`PermissionRequest`/`PostToolUse`，matcher `AskUserQuestion` |
| 模型 | relay 路由模型（非本机直连 Anthropic），`-p` 在本网络不认网关模型名，故走交互 PTY |

去敏录制件：`crates/remuda-signal/fixtures/askuser/pretooluse-auto.json`、
`posttooluse-auto.json`；同轮 transcript 四段（user 提问 / thinking / tool_use /
tool_result）：`crates/remuda-testing/fixtures/claude/claude-askuser-auto.jsonl`。

---

## 2 auto 模式事件序列：只有 PreToolUse

### 2.1 [V] 钩子回答时：PreToolUse 一个事件

第一轮：PreToolUse 钩子立即用 §3 的形状回答（allow + answers）。存盘事件只有两个：

1. `PreToolUse`（toolUseId `call_j1wmn…`）
2. `PostToolUse`（`tool_input.answers` 与 `tool_response.answers` 都在）

**没有 `PermissionRequest`。** PreToolUse 载荷（去敏后，逐字段形状见 fixture）：

```json
{
  "hook_event_name": "PreToolUse",
  "permission_mode": "auto",
  "tool_name": "AskUserQuestion",
  "tool_use_id": "call_j1wmn7g2i5ipikwjxd7uqdtx",
  "tool_input": {"questions": [
    {"question": "Tea or coffee?", "header": "Beverage", "multiSelect": false,
     "options": [
       {"label": "Tea", "description": "A brewed leaf-based drink, served hot or iced."},
       {"label": "Coffee", "description": "A brewed bean-based drink containing caffeine."}]}]}
}
```

与 `PermissionRequest` 同族载荷的差别只有：事件名、`tool_use_id` **存在**
（PermissionRequest 上实测仍没有，见 §4）、没有 `permission_suggestions`；
`tool_input.questions[]` 形状逐字一致。

### 2.2 [V] 钩子不表态时：PermissionRequest 随后补来（同一 call）

第二轮：两个钩子都只录制、不输出（harness 读作 `{}` 无意见）。同一 tool call
先后触发两个事件：

| 顺序 | 事件 | 与前一个的间隔 | `tool_use_id` | `permission_mode` |
|---|---|---|---|---|
| 1 | `PreToolUse` | — | 有 | `auto` |
| 2 | `PermissionRequest` | ~53 ms | 无 | `auto` |

两个事件的 `tool_input` **逐字段相同**（JSON 相等）。即：auto 模式并非不发
PermissionRequest，而是 PreToolUse 先裁决；只有 PreToolUse 不表态（`{}` / 超时 /
拒绝？）时，harness 才把同一 call 再作为 PermissionRequest 抛出。**Remuda 把
PreToolUse park 住等手机答复期间，配对事件不会来；它只在第一个钩子快速返回
无意见时出现。** 这就是去重需求的真实来源：同一个 toolUseId（对
PermissionRequest 来说是同一份 questions 内容）只能出一张卡。

---

## 3 [V] PreToolUse 裁决形状：permissionDecision + updatedInput

PreToolUse 钩子输出：

```json
{"hookSpecificOutput":{"hookEventName":"PreToolUse",
  "permissionDecision":"allow",
  "updatedInput":{"questions":[ …同上… ],
                  "answers":{"Tea or coffee?":"Tea"}}}}
```

验证点：

1. **被接受**：TUI 没有停在问题表单；transcript 的 tool_result 为
   `Your questions have been answered: "Tea or coffee?"="Tea". You can now continue
   with these answers in mind.`，随后的 PostToolUse 原样带回 `answers`。
2. **键名位置**：`updatedInput` 与 `permissionDecision` 都在 `hookSpecificOutput`
   这一层，**不是** `hookSpecificOutput.decision` 里面。
3. harness 自带钩子文档（2.1.277 二进制内嵌字符串）原文：

   > - `permissionDecision` - "allow", "deny", or "ask" (PreToolUse only)
   > - `permissionDecisionReason` - Reason for the permission decision (PreToolUse only)
   > - `updatedInput` - Modified tool input (PreToolUse only)
   > - `decision` - "block" for PostToolUse/Stop/UserPromptSubmit hooks (**deprecated
   >   for PreToolUse, use hookSpecificOutput.permissionDecision instead**)

   因此 deny/超时在 PreToolUse 上必须写成
   `{"permissionDecision":"deny","permissionDecisionReason":"…"}`，沿用
   PermissionRequest 的 `decision.behavior` 会被丢。
4. answers 键仍是**问题原文**；单选 label 字符串、多选 label 数组 —— 与
   `ask-user-question-1.md` §3 的 PermissionRequest 约定完全相同，回填/校验逻辑
   （`question::question_decision`）原样复用，无需第二套映射。

**[U]** `permissionDecision: "ask"` 未测；Remuda 不产生它（让 harness 回退到
自己的表单不是手机卡片的语义）。

---

## 4 与 2.1.272 结论的差异

| | `ask-user-question-1.md`（2.1.272，default） | 本页（2.1.277，auto） |
|---|---|---|
| 问题走哪个钩子 | `PermissionRequest` | `PreToolUse`（不表态时再补 PermissionRequest） |
| 裁决键 | `decision.behavior: allow` | `permissionDecision: allow` |
| answers 位置 | `decision.updatedInput.answers` | `hookSpecificOutput.updatedInput.answers` |
| deny | `decision: {behavior:"deny",message}` | `permissionDecision:"deny"` + `permissionDecisionReason` |
| questions/answers 语义 | 问题原文做键、label/id 同形 | 完全一致，直接复用 |

---

## 5 落到实现的对应关系

| 证据 | 代码 | 测试 |
|---|---|---|
| §2.1 auto 只有 PreToolUse | `event::hooks_block`（载荷形状判阻塞）、`question::question_request_from_event` | `event::tests::an_auto_mode_askuserquestion_pretooluse_blocks_by_payload`、`bus::…an_auto_mode_pretooluse_opens_a_question_card` |
| §2.2 配对事件、同一份 questions | `bus::QuestionCall` + `question_fingerprint`，首事件开卡、次事件旁挂 | `bus::paired_pretooluse_and_permission_request_open_one_card`、`…in_the_other_order_also_open_one_card` |
| §3 PreToolUse 裁决形状 | `decision::HookDecision::to_hook_json("PreToolUse")` | `decision::tests::a_pretooluse_allow_uses_the_permission_decision_keys_with_updated_input` 等 4 项 |
| §3.1 answers 回到 hook | 复用 `question::question_decision` | `bus::…an_auto_mode_question_answer_reaches_the_hook_in_pretooluse_shape`、`question::an_auto_mode_card_answer_round_trips…` |
| §3.3 deny/超时形状 | 继电器 `cmd::hook::decide` 按事件名序列化 | `remuda/cmd::hook::a_timed_out_auto_mode_question_denies_in_pretooluse_shape`、`…ordinary_pretooluse_timeout_stays_no_opinion` |
| 继电器阻塞预算 | `cmd::hook::wait_for`（问题型 PreToolUse 给 BLOCKING_WAIT） | `…an_auto_mode_askuserquestion_pretooluse_gets_the_blocking_budget` |
| §5 TUI 自答收卡（新路径） | 复用 `bus::close_terminal_answered`，同时撤配对钩 | `bus::a_terminal_answer_closes_the_auto_mode_card_and_both_hooks`、`…on_a_lone_pretooluse_card…` |
| 真实录制形状 | `fixtures/askuser/pretooluse-auto.json`、`posttooluse-auto.json`、`fixtures/claude/claude-askuser-auto.jsonl` | include_str! 直读 |

超时策略沿用既有 question 语义（blocking wait 与 broker TTL 对齐；harness 自身
每钩 600 s 的上限照旧），本任务不改。

## 6 复现

```bash
# throwaway overlay（/tmp 下临时目录，录完即删）：PreToolUse 钩子读 stdin 里的
# tool_input.questions，选每题第一个 label 组 answers，用 §3 的形状输出；
# 其余钩子只录制。
claude --permission-mode auto --settings /tmp/remuda-askq/settings.json
# 真 PTY 里输入："You MUST call the AskUserQuestion tool exactly once.
#  Ask a single two-option question: tea or coffee. Header Beverage. …"
# 检查：captures/ 下只有 PreToolUse + PostToolUse；transcript 的 tool_result
# 含 "Your questions have been answered"。
# 第二轮让钩子输出空串（{} 无意见）：captures/ 下为 PreToolUse 然后
# PermissionRequest，两者 tool_input JSON 相等。
```
