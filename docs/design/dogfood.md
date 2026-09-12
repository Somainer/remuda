# D0 · 用 Remuda 迭代 Remuda（对标 herdr 多 agent 工作流）

目标：一个 Claude Code coordinator 会话通过 `remuda`（CLI 或 MCP）完成今天用 herdr 做的全部动作。

## 对标清单（herdr 动作 → Remuda 能力）

| 现在用 herdr 的动作 | Remuda 等价 | 状态 |
|---|---|---|
| `herdr tab create --cwd X --label L` + `agent start <name> --kind K -- <yolo flags>` | `remuda instance create --kind K --driver pty --cwd X --name L [--worktree W] --prompt-file brief.md`；每 kind 的 yolo 预设（claude `--dangerously-skip-permissions`、codex `--dangerously-bypass-approvals-and-sandbox`、grok `--always-approve`、agy `--dangerously-skip-permissions`） | 待做（D0-1） |
| `agent prompt <name> "…"` | `remuda instance send <id\|name> "…"` / `--file` | CLI 存在，pty 路径待接 |
| `agent wait <name> --until idle\|blocked --timeout` | `remuda instance wait <id> --until idle\|done\|blocked --timeout` | 待补 until |
| `agent read <name> --lines N` | `remuda instance read <id> --lines N [--source screen\|journal]` | 待补 screen 源 |
| `agent send-keys <name> enter` | `remuda instance keys <id> enter` | 待做 |
| `agent list` | `remuda instance list [--host]` | 存在 |
| 群发暂停/恢复 | `remuda fleet send --all "PAUSE…"` | fleet send 存在，需 `--all` |
| 任务书文件 + `DONE <sha>` 约定 | `remuda instance wait --until-line 'DONE '` 或 read 解析 | 待做 |
| `git worktree add ../remuda-wt/<agent> -b wt/…` | `remuda worktree create <name> [--base main]` 并可在 create 时 `--worktree` | 待做（D0-2） |
| coordinator 验证合并（coord-verify/merge 脚本） | `remuda merge <branch> --gate` 或保留脚本 | 后置 |
| herdr skill（教 coordinator 用 CLI） | `skills/remuda/SKILL.md`（Claude Code skill） | 待做（D0-4） |

## 验收
一次真实的 `claude -p --mcp-config remuda-mcp.json` 会话：创建 worktree → 起一个 codex（pty）和一个 grok（pty）实例 → 各发任务书 → wait until idle → read 到 `DONE` → 停止实例；evidence 存 `docs/design/evidence/dogfood-1.jsonl`。
