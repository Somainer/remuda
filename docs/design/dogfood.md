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
| 群发暂停/恢复 | `remuda fleet send --all "PAUSE…"` | **完成。** CLI `--all` / `--labels` 与 MCP `remuda_fleet_send` 存在；D0 验收未跑。 |
| 任务书文件 + `DONE <sha>` 约定 | `remuda instance wait --until 'line:(?m)^DONE'` 或 read 解析 | **完成。** dogfood-3 wait `reason=condition-met`（codex `matchedLine="• DONE"`；grok 跟发后命中缩进 `DONE`）。worker 文件 `dogfood-codex.txt` / `dogfood-grok.txt`。 |
| `git worktree add ../remuda-wt/<agent> -b wt/…` | `remuda worktree create <name> [--base main]` 并可在 create 时 `--worktree` | **完成。** dogfood-1 起 MCP `remuda_worktree_create` 可用（reuse-on-repeat，catalog 在 git-common-dir）。 |
| coordinator 验证合并（coord-verify/merge 脚本） | `remuda merge <branch> --gate` 或保留脚本 | **后置**（post-D0）。现网仍用 coordinator 脚本 gate-merge。 |
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

未纳入本轮验收、仍待做：`instance keys` 的 live 跑通、herdr workspace 在 stop 时回收、shutdown settle、`remuda merge --gate`。
