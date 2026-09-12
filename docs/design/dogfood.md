# D0 · 用 Remuda 迭代 Remuda（对标 herdr 多 agent 工作流）

目标：一个 Claude Code coordinator 会话通过 `remuda`（CLI 或 MCP）完成今天用 herdr 做的全部动作。

## 对标清单（herdr 动作 → Remuda 能力）

状态以 dogfood-3 合入后的 main（[`30e4abe`](https://github.com/Somainer/remuda/commit/30e4abe)）为准。证据：[`dogfood-1-report.md`](./dogfood-1-report.md) / [`dogfood-2-report.md`](./dogfood-2-report.md) / [`dogfood-3-report.md`](./dogfood-3-report.md)，stream-json 在 [`evidence/`](./evidence/)。

| 现在用 herdr 的动作 | Remuda 等价 | 状态 |
|---|---|---|
| `herdr tab create --cwd X --label L` + `agent start <name> --kind K -- <yolo flags>` | `remuda instance create --kind K --driver pty --cwd X --name L [--worktree W] --prompt-file brief.md`；每 kind 的 yolo 预设（claude `--dangerously-skip-permissions`、codex `--dangerously-bypass-approvals-and-sandbox`、grok `--always-approve`、agy `--dangerously-skip-permissions`） | **完成。** generic-pty + yolo 预设；create-time brief 在 control ready 后送达（[`c95cd25`](https://github.com/Somainer/remuda/commit/c95cd25) / dogfood-2）。dogfood-3 起 codex+grok pty 于 worktree cwd。 |
| `agent prompt <name> "…"` | `remuda instance send <id\|name> "…"` / `--file` | **完成。** 排队直到 pane live，经 herdr `agent.prompt`（dogfood-2）。dogfood-3 grok 跟发一次 send 后 wait 成功。 |
| `agent wait <name> --until idle\|blocked --timeout` | `remuda instance wait <id> --until idle\|done\|blocked\|line:<regex> --timeout` | **完成。** `idle`/`done`/`blocked`/`line:` 均接线。验收路径是 `line:(?m)^DONE`：TUI 子弹线（`• DONE`、缩进 `DONE`）在 [`379ac23`](https://github.com/Somainer/remuda/commit/379ac23) / dogfood-3 命中。 |
| `agent read <name> --lines N` | `remuda instance read <id> --lines N [--source screen\|journal]` | **完成。** generic-pty 把 bounded screen 快照写入 journal（`nativeName=screen`，screen-derived）；dogfood-2/3 `source=screen` 返回 pane 文本，无 journal-fallback。 |
| `agent send-keys <name> enter` | `remuda instance keys <id> enter` | **部分。** Hub→Node `tty.write` 已合入 [`a4f0485`](https://github.com/Somainer/remuda/commit/a4f0485)；D0 验收未跑 keys。 |
| `agent list` | `remuda instance list [--host]` | **完成。** CLI/MCP 存在；D0 会话未调用。 |
| 群发暂停/恢复 | `remuda fleet send --all "PAUSE…"` / `remuda fleet keys --all esc` | **完成。** Hub `POST /v1/fleet/broadcast` 在服务端选中运行中 Instance 并扇出 `instance.send` / `tty.write`，返回 per-instance 结果与 `accepted`/`failed`/`skipped` 汇总；`--all` 与 `--labels`/`--host`/`--kind` 可叠加（取交集），`--idempotency-key` 重放不重发。CLI `fleet send` / `fleet keys`、MCP `remuda_fleet_send` / `remuda_fleet_keys`、Web Fleet 页「群发」框同一条路径。测试：Hub 双 fake Node 扇出、CLI 单测、web 单测。D0 live 验收仍未跑。 |
| 任务书文件 + `DONE <sha>` 约定 | `remuda instance wait --until 'line:(?m)^DONE'` 或 read 解析 | **完成。** dogfood-3 wait `reason=condition-met`（codex `matchedLine="• DONE"`；grok 跟发后命中缩进 `DONE`）。worker 文件 `dogfood-codex.txt` / `dogfood-grok.txt`。 |
| `git worktree add ../remuda-wt/<agent> -b wt/…` | `remuda worktree create <name> [--base main]` 并可在 create 时 `--worktree` | **完成。** dogfood-1 起 MCP `remuda_worktree_create` 可用（reuse-on-repeat，catalog 在 git-common-dir）。 |
| coordinator 验证合并（coord-verify/merge 脚本） | `remuda merge <branch> --gate` / MCP `remuda_merge` | **已实现（post-D0）**。隔离 merge、共享 gate、CAS 更新 main、push；临时 Git 仓库 + stub gate 验证，不计入历史 dogfood-3 live 验收。 |
| herdr skill（教 coordinator 用 CLI） | `skills/remuda/SKILL.md`（Claude Code skill） | **完成**（D0-4 已在 main）。dogfood 会话走 `--mcp-config` + allowed tool list，未加载 skill。 |

## 验收

原目标：一次真实的 `claude -p --mcp-config remuda-mcp.json` 会话：创建 worktree → 起一个 codex（pty）和一个 grok（pty）实例 → 各发任务书 → wait until 看到 `DONE` → 停止实例。

脚本：[`scripts/demo/dogfood-1.sh`](../../scripts/demo/dogfood-1.sh)。与 coordinator demo 并存时用 `REMUDA_DOGFOOD_HUB_LISTEN=127.0.0.1:28080` / `REMUDA_DOGFOOD_NODE_LISTEN=127.0.0.1:28787`。

## D0 验收结果

**通过**（main [`30e4abe`](https://github.com/Somainer/remuda/commit/30e4abe)）。coordinator JSON：`codexDone=true`，`grokDone=true`。

| 轮次 | 报告 | 证据 | 结果 |
|---|---|---|---|
| dogfood-1 | [`dogfood-1-report.md`](./dogfood-1-report.md) | [`evidence/dogfood-1.jsonl`](./evidence/dogfood-1.jsonl) | MCP 路径打通；create-time prompt `control unavailable`；无 DONE 文件；wait 超时 |
| dogfood-2 | [`dogfood-2-report.md`](./dogfood-2-report.md) | [`evidence/dogfood-2.jsonl`](./evidence/dogfood-2.jsonl) | 两份 DONE 文件写出；screen journal 可用；`^DONE` 未匹配 TUI `• DONE` |
| dogfood-3 | [`dogfood-3-report.md`](./dogfood-3-report.md) | [`evidence/dogfood-3.jsonl`](./evidence/dogfood-3.jsonl) | wait `condition-met`；`codexDone=true` / `grokDone=true` |

Caveats（dogfood-3 观察到、验收仍算通过）：

- Grok 第一次 `line:(?m)^DONE` 超时，需要一次显式 `remuda_instance_send`「Print DONE as its own line now.」后再 wait 才 `condition-met`。Codex 第一次 wait 即命中 `• DONE`。
- `instance stop` 不回收 herdr workspace / pane；重复 dogfood 会在 `remuda-node` session 里留下 tab。
- Node 退出时日志会出现 `driver shutdown did not settle`（stop 已发出，herdr 侧未在超时内收干净）。

未纳入本轮验收、仍待做：`instance keys` 的 live 跑通、herdr workspace 在 stop 时回收、shutdown settle。`remuda merge --gate` 的后续实现见下节。

## Coordinator 验证合并

```bash
remuda merge wt/reviewer/work --dry-run --json
remuda merge wt/reviewer/work --gate --json
# 仅更新本地 main；强制追加 web 验证；指定 gate 独占的构建缓存
remuda merge wt/reviewer/work --gate --no-push --web --target-dir target-coordinator
```

CLI 与 MCP `remuda_merge` 共用同一实现。MCP 参数为 `branch`、`gate: true`（或 `dryRun: true`）、`web`、`noPush`、`repo`、`targetDir`；在 MCP server 所在机器执行 Git 操作，不经过 Hub。CLI `--repo` 默认当前目录所在的 Git checkout。

执行顺序：

1. `git fetch origin`，检查源分支的 worktree：有 staged changes 或 merge/rebase 未完成即拒绝。rebase 已 detached HEAD 时仍按 rebase 的 `head-name` 识别源分支。未暂存和未跟踪的 worker 文件不参与合并。
2. 固定本地 `main` 的 expected SHA 和源分支 SHA，在 `<repo>/data/tmp/merge-*/worktree` 创建 detached worktree，按这两个快照执行 `git merge --no-ff --no-edit`。冲突返回文件列表，不改源 worktree。
3. 执行 [`scripts/ci/gate.sh`](../../scripts/ci/gate.sh) 的唯一 gate 定义：`./scripts/ci/secret-scan.sh` → `cargo fmt --all --check` → `cargo check --workspace --all-targets --locked` → `cargo clippy --workspace --all-targets --locked -- -D warnings` → `cargo test --workspace --locked`。仅 test 失败时自动重试一次，输出 `retried`，JSON 同时给出 `attempts: 2`、`retried: true`。任一步最终失败即停止，后续 gate 步骤标为 `skipped`。
4. `--web` 或最终 merge 相对 expected main 改动了 `web/` 时，在临时 worktree 的 `web/` 内顺序执行 `pnpm install --frozen-lockfile` → `pnpm build` → `pnpm test`。
5. 确认 gate 没有修改 merge HEAD / tracked content，执行 `git update-ref refs/heads/main <merged> <expected>`。成功后 push 到 `origin main`，除非 `--no-push`；push 的 refspec 固定为 `<merged>:refs/heads/main`，只发送已验证的 commit，不 force-push。
6. 成功、gate 失败、冲突、CAS 失败、push 失败均清理本次临时 worktree；cleanup 失败会明确报告。保留构建缓存以供下一次 gate 复用。

命令为 gate 强制设置 `CARGO_INCREMENTAL=0`，`CARGO_TARGET_DIR` 默认 `<repo>/target-gate`，不继承 worker 的 Cargo target。`--target-dir` 可设绝对路径或相对 repo 的路径；并行 coordinator 应各自指定独占缓存。

`--dry-run` 只检查本地源 worktree / refs、按本地分支差异选择 web 步骤、输出计划；不 fetch、不创建目录 / worktree、不运行 gate、不更新 ref、不 push。它不证明没有 merge conflict 或 gate 能通过，真正执行时会重新 fetch 并固定 SHA。

`--json` 的 stdout 是一个 JSON object，日志走 stderr。字段包括 `status`、`exitCode`、`expectedMain`、`source`、`merged`、`mainUpdated`、`pushed`、`web`、`targetDir`、`conflicts` 和 `steps`；每步给出 `name/status/durationMs/attempts/retried`，失败步骤附 `error`。MCP 同时返回相同的 `structuredContent` 和 JSON text，非零 `exitCode` 标为 `isError: true`。

| Exit code | 含义 | main / origin 行为 |
|---|---|---|
| 0 | 成功，或 dry-run 计划生成成功 | 正常执行按 `mainUpdated/pushed` 判断；dry-run 均为 false |
| 1 | preflight、gate、操作或 cleanup 失败 | push 失败时本地 main 可能已更新，必须检查 `mainUpdated/pushed`，不自动回滚 |
| 2 | merge conflict | 返回 `conflicts`，不更新 main / origin |
| 3 | CAS lost | 其他操作已改变 main；不覆盖、不 push；重新检查后再执行 |

CAS 只更新 ref，其他 checkout（包括已 checkout main 的目录）的 index / 文件不会被重置。Coordinator 在刷新该目录前应检查自己的本地修改。远端 main 不会被强制覆盖；fetch 不隐式 fast-forward 本地 main，push 被拒绝时保留本地已验证 commit 供对账。

CI 的 Rust 和 web jobs 分别调用 `gate.sh` 与 `gate.sh --web-only`；原有 web lint / OpenAPI freshness、secret scanner self-test、stub acceptance 和可选 audit 仍保留。`gate.sh --list [--web]` 输出步骤计划，`--report <file>` 写入逐步 JSONL。仅测试使用 `REMUDA_MERGE_GATE_COMMAND=<executable>` 替换各 gate 命令，该 executable 接收 step name，仍走相同顺序 / 重试 / cwd / 环境；结果 `gateOverride: true` 明确标识这种 stub 验证，真实 coordinator / CI 应不设置此变量。
