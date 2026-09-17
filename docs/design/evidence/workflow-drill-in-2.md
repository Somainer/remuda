# 真实会话的 Workflow 钻取与分组（c-wfdrill2）

- Date: 2026-09-17
- Harness: 本机 macOS（Darwin 25.4.0, arm64），claude **2.1.274**（原生安装器路径）
- Where: 本任务 worktree 自己的 scratch dev server（`remuda dev`，独立 data dir 与端口 18790/18791），
  carrier 为 `WssLink::connect_runtime`（出站 WSS，正是缺陷 A 所在的那条链路）
- Flags: `REMUDA_PTY_CARRIER=native REMUDA_PTY_HOOKS=1 REMUDA_PTY_EMULATOR=1`
- 前置: `workflow-drill-in-1.md`（c-wfdrill，只在 hub e2e fake Node 上验过）

c-wfdrill 只在 hub e2e fixture 上证明过。放到**真实 remuda 启动的 shell-pty 会话 + 本地 WSS
carrier** 上，三处缺陷同时出现：每个 workflow 成员行恒显示「启动中」，每个子代理调用都被平铺在
一张独立的 Workflow 卡之外。三处都已修复并在本机真实会话上复验。

---

## A. Carrier 分发缺口：catch-all 伪造成功

`runtime_wss.rs` / `runtime_link.rs` 都以 `_ => Ok(json!({"ok": true}))` 结尾。任何没有人写过
arm 的方法都被报告为成功：

- `subagent.transcript` → Hub 得到 `{"ok":true}`，`subagentApi.ts` 读到 `rec.available === undefined`，
  于是**所有**成员行都落到「启动中」。每台 WSS 主机都中招，无论是否已绑定。
- `workspace.scm.*` 同样被吞掉 → files/changes 在这条 carrier 上是死的。

修的不是“再加两个 arm”（`native-carrier-4.md` §1 已经这样补过一次 `tty.screen`），而是把两处
catch-all 都委派给 codec 的分发表 `transport::hubnode::dispatch_frame`；该表的未知方法路径按
**帧型**回答：request 型（有 id，有人在等）返回 `NodeError::InvalidRequest`，notification 型
（无 id，结果没人读）保持宽容。stdio 保留白名单，只补上 subagent 一行。

`subagent.transcript` 现在还会说明**为什么**不可读（`transcript-unbound` /
`agent-transcript-pending`），web 据此区分“宿主没绑定这个会话”和“子代理自己的文件还没落盘”。

### 真实会话取证（scratch dev server，出站 WSS）

```text
GET /v1/instances/<ins>/subagents/a3034c099ad2e0b2c/transcript
available: True | reason: None
meta: { "agentId": "a3034c099ad2e0b2c", "runId": "wf_eab52bbe-1f7",
        "prompt": "Return exactly the single word: beta. …",
        "model": "claude-opus-5", "tokens": 20847, "calls": 1,
        "latestTool": "StructuredOutput",
        "startedAt": "…T15:46:23.716Z", "endedAt": "…T15:46:28.642Z" }
events: 25   kinds: lifecycle 12, opaque 8, message 1, effort 1, model 1,
                    tool_call 1, tool_result 1
```

第二个成员（`aaa04f9e3074922df`, `echo:alpha`）同样 `available: true` / 25 events。

同一条 carrier 上的 SCM 读：

```text
GET /v1/hosts/<hst>/workspaces/<wsp>/changes
availability: ok | scm: git | branch: {"state":"known","value":"main"}
entries: [ {"path":"README.md","xy":" M","kind":"modified","sizeBytes":72,…},
           {"path":"scratch.txt","xy":"??","kind":"untracked","sizeBytes":10,…} ]
```

`availability` / `entries` / `available` / `reason` 都是 catch-all 无法伪造的字段。

### 回归测试

- `runtime_wss.rs`：`the_wss_runtime_answers_drill_in_and_scm_reads_itself`（断言
  `available:false` + `reason:"transcript-unbound"`，以及真实工作树的 `entries`），
  `an_unknown_method_fails_a_request_and_is_tolerated_as_a_notification`。
- `tests/wss.rs`：`subagent_drill_in_reaches_the_node_over_the_wss_carrier`，走 Hub HTTP →
  出站 WSS → Node 的完整路由。
- `subagentApi.test.ts`：`fetch` 以 200 返回 `{ok:true}` 时**必须抛错**，不得渲染成「启动中」。

---

## B. 原生安装器路径下不促进（promotion），native session / transcript 永不绑定

本机的 claude 就是这种安装：

```text
<home>/.local/bin/claude -> <home>/.local/share/claude/versions/2.1.274
```

实测该会话的前台进程组一行（已脱敏）：

```text
PGID   PID    ARGS
69569  69569  <home>/.local/share/claude/versions/2.1.274 --settings <instance dir>/launch/settings.json
```

`promote::detect` 按 basename 匹配 `AGENT_TABLE`，而这里的 basename 是 `2.1.274`。于是
`detect` 返回 `None` → 没有 `agent_promoted` → `PromotedHooks.foreground_pid` 始终为空 →
hook 的 `SessionStart`（带真实 session id 与 transcriptPath）一直停在 `pending` 里 →
`nativeRef.sessionId` 保持等于 instance id、`transcript` 永远 unknown → `claude --resume` 无从谈起。

任何文件名启发式都补不全这一类：grok 下载在 `<home>/.grok/downloads/grok-<ver>-macos-aarch64`，
而 `binary.rs` 的 ETXTBSY 兜底还会把二进制**复制到一个全新的名字**再 pin。因此改为：把
launch 已经知道的两件事——agent kind 与 pin 的绝对路径（`BinaryPin.abs_path`，即复制后的那个
路径）——作为 `LaunchAlias` 传进 `detect`，按**精确路径**先于名字表匹配。一次修好 claude /
grok / codex / agy，且天然跟随 pin 的复制。

检测仍可能迟到，所以 poller 增加一条兜底：**pid 在本 PTY 自己的前台进程组里**的
authenticated `SessionStart` 可以自行促进并绑定。pid 检查就是那道闸——嵌套或无关的 claude
共用同一个 hook socket，但拿不到这个实例。

### 真实会话取证（同一会话的 journal，seq 已保留）

```text
 8 lifecycle agent_detected     {"kind":"claude","pid":"69569"}
 9 lifecycle agent_promoted     {"kind":"claude","mode":"promoted","pid":"69569",
                                 "promotedAt":"…T15:42:55.281Z"}
10 lifecycle transcript_unbound {}
14 lifecycle SessionStart       nativeId=known "667cd550-…-df4b23bedd21"
                                 relatedIds.transcriptPath=<home>/.claude/projects/…/667cd550-….jsonl
26 lifecycle transcript_bound   {"sessionId":"667cd550-…","source":"hook",
                                 "transcriptPath":"<home>/.claude/projects/…/667cd550-….jsonl"}
```

实例视图：

```text
lifecycle running | mode promoted | kind claude
nativeSessionId       = 667cd550-4c42-4820-8800-df4b23bedd21     ← 不再等于 instance id
nativeTranscriptPath  = <home>/.claude/projects/-private-tmp-…-work/667cd550-….jsonl  ← 文件存在
```

### 回归测试

`the_same_agent_promotes_under_every_spelling_of_its_executable` 增加三行：原生安装器路径
（无 alias 时 `None`，有 alias 时促进且仍水合 transcript）、grok 下载路径、以及一个反例
（无关的 `tools/versions/<semver>` 路径在带 claude alias 时仍然 `None`——alias 是精确匹配，
不是形状匹配）。

---

## C. hook 节点 id 挂在一个一次性 instance id 下，分组永远匹配不上

`promote_ctx` 之前 `instance_id: InstanceId::new()`，而 `remuda-signal` 把它烘进
`LiveState.scope`：每个 hook 工具节点都是 `Id::derive("obj", <一次性 id>, <tool_use_id>)`。
Node 的 `WorkflowProducer` 却用**真实** instance id 派生 `workflow.run.toolCallId`。两者永远
不可能命名同一个节点；store 的 append 只重写信封身份，不重写 payload 里的派生 id，所以下游
无从修复：web 的 `mountWorkflow` 找不到那一行，卡片退化成独立节点，子代理行全部留在顶层。

修法：`ShellPtyOptions`（以及同样 mint 的 `ClaudePtyOptions` / `ClaudeBgOptions`）带上 Node
指定的 instance，在 `native.rs` 的 `DriverKind::ShellPty` arm 里**无条件**设置——不能放进
`pty_hooks` 块，否则一个关掉 hook 的 promoted shell 仍旧拿到随机 scope。transcript 重放
（`TranscriptMapper`）用的是同一个 `ctx.instance_id`，因此一并归位。另有 debug_assert 校验
instance 目录末段等于该 instance id。

### 真实会话取证

同一会话的 journal 里：

```text
tool_call 节点（4 个）
  obj_67a2…f08be  Skill              agent: not-applicable
  obj_5341…06098  Workflow           agent: not-applicable
  obj_4eee…bec5f2 StructuredOutput   agent: a3034c099ad2e0b2c
  obj_2b6a…3b40c3 StructuredOutput   agent: aaa04f9e3074922df

workflow.run.toolCallId = obj_5341…06098
  → 解析到 journal 里真实存在的 tool call: True  ('Workflow')
```

把这份真实 journal 喂给**未改动的** web assembler（`assembleTranscript`）：

| | 顶层 tool 行 | Workflow 卡挂载 | Workflow 行下嵌套的子代理行 |
|---|---|---|---|
| 修复前的形状（把 `run.toolCallId` 改成 journal 里不存在的节点，即旧派生的效果） | **4**：Skill, Workflow, StructuredOutput, StructuredOutput | false | 0 |
| 修复后（真实 journal 原样） | **2**：Skill, Workflow | true（members: 2） | **2** |

修复后的 subagent 桶：

```json
[{"agentId":"a3034c099ad2e0b2c","kind":"workflow-member","tools":["StructuredOutput"]},
 {"agentId":"aaa04f9e3074922df","kind":"workflow-member","tools":["StructuredOutput"]}]
```

### 回归测试

- driver：`promote_ctx_is_scoped_to_the_instance_the_node_named`。
- `tests/workflow_producer.rs`：bus 改为经**生产缝** `shell_pty::hook_bus_context`（内部就是
  `promote_ctx`）构造，再断言 `run_snapshots[0].tool_call_id` 等于同一次 hook fold 为
  `toolu_vrtx_01WF` 产出的 tool call 节点。把 `promote_ctx` 换回 `InstanceId::new()` 后该断言
  立即失败（实测：两个不同的 `obj_…`）。
- `assemble.test.ts`：pin 住“`workflow.run` 指向不存在的 tool call 时**什么都不折叠**”——
  卡片留在顶层，成员自己的工具行也留在顶层，绝不因为一个 id 解析失败就把所有子代理行塞进一张卡。

---

## 真实会话是怎么跑的

```text
remuda dev（自己的 data dir + 端口 18790/18791，workspace = /private/tmp/…/work）
remuda instance create --kind claude --driver shell-pty --cwd <scratch work dir>
  → 会话促进为 claude，本机原生登录；/model 选 Opus 5 (1M)（按 s「仅本会话」，不改操作者默认）
  → prompt：用 workflow 并行跑两个只回一个词的 agent
  → 屏幕：✔ Completed in 13s · 2 agents · 41.6k tokens → "WFDONE alpha beta"
  → 结束后 instance stop，进程已回收（ps 无残留）
```

## 遗留 / follow-up

- 仍有**一个 promoted shell-pty 实例处于 unbound**（由验证者在本轮之前报告）。本轮的两条绑定
  路径（精确 alias + 前台进程组内的 SessionStart 兜底）覆盖的是“检测不到”和“检测迟到”；一个
  在这两条路径之外仍然 unbound 的实例需要单独取证（它的前台进程组一行 + hook binding 的 pid）。
- 本机上 `cargo test -p remuda-node -p remuda-driver -p remuda-signal -p remuda-journal`
  有 10 个测试失败，且**失败集合与未改动的 `ccd9a5dc` 逐字相同**（把这四个 crate 全部
  `git checkout ccd9a5dc --` 后重跑，`diff` 无输出），与本轮改动无关：

  | 测试 | 本机成因 |
  |---|---|
  | `workspace_scm::tests::*`（7 个）+ `gate::tests::verify_passes_and_streams_steps` | macOS 的 `$TMPDIR` 在 `/var/folders/…` 下，而 git 的 `rev-parse --show-toplevel` 回答 `/private/var/folders/…`；`git_toplevel` 要求 toplevel 与注册根**字符串相等**，于是临时仓库被判为 `unsupported` |
  | `tasktrack_node::foreground_and_background_subagent_rows_close_correctly` | 子进程用 `tempdir_in("/tmp")`，但 `test_workspace_roots!` 只允许 crate 目录与 `$TMPDIR`（`/private/var/folders/…`），于是 `/private/tmp/.tmpXXXX/workspace` 落在允许根之外 |
  | `shell_pty::lifecycle::tests::a_cooperative_group_stops_at_the_first_rung` | 负载敏感的时序 flake：本机 CPU 100%，第一档 SIGINT 之前就走到了 `Hangup`。在**未改动**的树上 4 次里失败 2 次 |
  | `adapters_parity`、`prompt_correlation`（3 个）、`live_latency::budgets_and_rule_six_over_a_real_pty` | 同样在未改动的树上失败（真实 PTY / 负载与上述 `/tmp` 归属问题） |

  这些都不在本任务的所有权范围内（`workspace_scm` 的 toplevel 比较、`test_workspace_roots`
  的根集合），因此本轮不动它们，只如实记录。本轮新增/改动的测试全部通过：
  `transport::wss::runtime_wss`（7）、`tests/wss.rs`（12 passed / 1 ignored）、
  `tests/workflow_producer.rs`（2）、`tests/subagent_transcript.rs`（2）、
  `promote.rs` 的检测表、`shell_pty::tests::promote_ctx_is_scoped_…`；
  `pnpm --dir web test` 117 files / 1013 tests 全绿；
  hub e2e `ux-wfdrill.hub.spec.ts` 3 passed（第 4 条是取证截图，默认 skip）。
