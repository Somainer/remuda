# 委托决策留痕 c-deleg3（plan-review v2，T1）

日期：2026-09-24。分支 `wt/c-deleg3/b-deleg6-md`，基线
`origin/main`。范围：print/sdk 上 **Node 侧直接铸 PlanReview**，走既有
的 Node first-answer-wins CAS + Hub 一跳路由；无 Hub-fold 生产者、无
Hub-native broker、无 plan 对象通道、无新 wire enum。

## 0. 真实条件验证

用真实交互式 Claude（≥ 2.1.277，`-p --input-format stream-json
--output-format stream-json --permission-mode plan --permission-prompts host
--permission-prompt-tool stdio`）录制两段会话，脱敏后位于
`crates/remuda-testing/fixtures/claude/`（与
`remuda-claude-wire/tests/fixtures/` 各一份）：

- `claude-exit-plan-mode-allow.jsonl`：ExitPlanMode 暂停 → host 回
  `behavior:"allow"`（无 `updatedPermissions`）→ tool_result
  "User has approved your plan"、permissionMode 切到 default → 子代理
  **真的执行了计划里的 Bash**。
- `claude-exit-plan-mode-deny.jsonl`：host 回 `behavior:"deny"` +
  `message` → 该文本作为 `is_error` 的 tool_result 原样喂给模型，子代理
  不执行计划命令。

脱敏（写入 `fixtures/SOURCES.md`，不引用被替换的原值）在**重构后的内容**
上做，而不是逐行替换：

- 流式 `input_json_delta.partial_json` 会把敏感字符串（甚至用户名本身）
  切到多行，逐行 grep 查不到但拼接后泄露。生成器按 content_block
  start/stop 边界重组每个流式 tool_use 的输入 JSON，整体替换路径后把该
  块的 delta **折叠成一个片段**（driver 只在 content_block_stop 解析拼接
  结果，折叠不影响解析），再重新拆分一致；
- 工作目录、home 配置目录、socket、计划文件路径统一到
  `/workspace/project/...` 中性占位；project memory 目录名中性化；
- **init 模型目录**（initialize 成功响应的 `models` 数组）逐条替换为
  中性的 `test-model`，仅保留 CLI 哨兵 `default`；
- 组织标签的 `msg_*` / `toolu_*` 原生 id（含 `wire_tool_inputs` JSON
  key）替换为 `msg_replay_NN` / `toolu_replay_NN`；裸 UUID 替换为确定
  性 replay UUID；非 JSON 诊断输出剔除；两份副本字节一致。

用一个片段重接校验脚本（`/tmp` 下，不入库）对每个流式块重新 join 并
grep 拼接后的内容、逐行内容与 init 目录，确认无 home 路径/用户名/组织
id/内部模型 id；校验输出见本回复（提交后协调员可用同样逻辑重跑）。

回放器：`remuda-testing` bin `fake-claude-replay`。非检查点帧按序吐到
stdout；`_dir:"in"` 检查点在 stdin 上等一帧并严格校验：

- init 检查点接受 driver 自己生成的 request id（与改写后的 init 响应
  相关联），但仍要求 control_request/initialize 形状；
- **ExitPlanMode 判定检查点对整个 permission-response 信封做校验**：顶层
  `type=control_response`、`response` 对象精确键集合、`subtype=success`、
  request id 相关、inner payload 精确键集合 + 深比对（`behavior`、精确
  `updatedInput`、deny `message`，无 `updatedPermissions` 等额外字段）；
  注入错误 subtype、改 input、加权限的负例都使回放非零退出；
- 判定检查点之前 stdin EOF 即非零退出（driver 不答不能算通过）；
- 退出状态文件原子发布（临时文件 rename），测试读不到半成品。

driver 驱动的回放测试一律开 `FAKE_CLAUDE_STRICT=1`，并通过
`FAKE_CLAUDE_RESULT_FILE` 读回回放进程的真实退出码（driver 持有子进程
句柄，测试拿不到 wait status，用状态文件做侧信道，不碰 stdout）。

## 1. 实现要点

1. **协议（加性可选字段）**：`PlanReviewRequest.plan: Option<String>`，
   `#[serde(default, deserialize_with=required_option)]`。旧读者忽略，新
   读者缺失为 `None`；零新 wire enum；`planRef` 是不解析占位 id。schema
   与 `web/src/types/generated.ts` 用 `gen_types` 重生成；手写
   `interaction.ts` 加 `plan?: string | null`。
2. **driver 铸票**：`tool_name=="ExitPlanMode"` + 该 tool_use_id 被
   mapper 以顶层（`parent_tool_use_id == null`）见过 + `input.value.plan`
   是 ≤ 32 KiB 的字符串（**空字符串也铸**，见 §3）。其余 fail-closed 回
   Approval（仅人答）。
3. **consumer 臂**：approve → Allow 原 input、`updated_permissions: None`；
   deny → feedback 逐字作为 deny message；feedback 为 null（缺失）时才
   用默认 `"plan review denied"`；纯空白 feedback 逐字转发。
4. **validate_answer（先于 CAS，任意 carrier）**：选项必须是请求实际
   offer 的；revision 必须等于**请求的** revision（非硬编码 1）；digest
   必须一致；`allow_feedback=false` 时 deny 不得带 feedback；approve 永
   不带 feedback；feedback ≤ 4096 UTF-8 字节。坏答复不消费 ticket。
5. **Hub self-exclusion（D-051 (6d)）**：list 与 answer 都对 plan-review
   去掉 self 边，child 不能列/答自己的 plan；直接父或人可答。
6. **覆盖边界**：本批只覆盖 print/sdk（唯一有真录证据的可回复原生暂
   停）。刮屏 carrier 不产生 plan-review；shell-pty hooks（T2）以真录为
   门槛，未做。

## 2. web

- ApprovalsPage 的 plan-review 卡片：有 inline `plan` 时折叠 `<pre>` 渲
  染；`plan` 为 null/缺失/空时给出「打开会话查看完整计划」提示而不是空
  卡片。
- deny 反馈 textarea：提交前按 **UTF-8 字节**校验 4096 上限（中文 3 字
  节，字符数不够），超限禁用拒绝并显示可操作错误；approve 恒发
  `feedback:null`。
- `hubStore.respond`：只有**应答 POST 本身**失败才清掉本地
  `answering`、抛错并保留草稿；POST 已 200 之后的 refresh/catchup 失败
  保持成功路径（不会让已提交的决定看起来被拒、卡片错误复活）。
- deny 反馈：textarea 字面为空才发 null；纯空白（空格/换行）按操作员所
  输逐字转发（与 driver 的 null-only 默认语义一致）。
- mock e2e 5 条（含无正文提示与 4 KiB 阻断）；typecheck/lint 干净。

## 3. 空 plan 的判定

**空字符串也铸 PlanReview。** 设计批准的谓词是「`input.plan` 是字符串且
≤ 32 KiB」，空字符串满足；此时审阅者审批的是 ExitPlanMode 工具调用本
身，UI 走「打开会话查看」提示。代码、单测、web 行为一致（不再因
`is_empty()` 退回 Approval）。

## 4. 测试

- 真录 VCR（`remuda-driver/tests/claude_plan_review.rs`，5/5）：approve
  后真实 tool_result + 计划 Bash 执行；deny+feedback 无 Bash；对 allow
  fixture 回 deny 使回放非零退出；allow/deny 正确判定回放 exit 0（strict，
  含 payload 深比对与真实退出码）；并对错误 subtype、被篡改的
  updatedInput、新增 updatedPermissions 三个负例断言回放非零退出。
- driver 单测 10 个：顶层铸票/plan-digest；nested、>32 KiB、非
  ExitPlanMode 回 Approval；**空字符串仍铸票**；approve 不放大权限；
  deny feedback 逐字/默认；validate 的 offered-option、请求 revision、
  digest、`allow_feedback=false`、approve-feedback、4096 上限规则；跨
  kind 拒绝。
- Node：plan-review 在 ClaudeControl 上 digest 不符 → InvalidRequest
  且票仍 pending、正确答案随后能赢；approve 带 feedback 拒绝且票仍
  pending。
- Hub（`delegated_decisions.rs`，13/13，plan-review 三例）：**真实
  remuda-driver `InteractionBroker` + 计数 owner** 挂在 Hub 的 Node
  transport 后面（不再用罐头应答）：父答命中真实 CAS、恰好一次 driver
  delivery；child 403 且不列/父可见，403 不触达 CAS；**人/父并发**应答
  恰一 200 一 409、恰好一次 delivery、行 answer-committed。
- protocol `wire_golden`/`generated`、claude-wire 解码探针（含两份真录）
  通过。
- web mock e2e 5/5。

## 5. 待协调员验证（未在本环境执行）

brief §8 验收 2/3 的**真实父子 gateway 轮次**（真实两个 agent：child
plan 模式、父 list 看到 plan 正文、approve 后 child 退到 default 执行、
deny+feedback 进 child transcript、`interaction.answered` 审计
answeredByLevel=1；以及人从 /approvals 作答）需要交互式 gateway 与两个
agent，留待 landing 后由协调员在 demo 环境跑。本批用真录 VCR + 真实
broker CAS 单测证明等价链路，但不替代真人验收。

## 6. 本地清单

- `cargo fmt --all` 干净；touched crates（protocol/driver/node/hub/testing/
  claude-wire）`--all-targets -D warnings` 干净。
- 无新 SQL、无新 wire enum、无新 endpoint/verb/工具。
