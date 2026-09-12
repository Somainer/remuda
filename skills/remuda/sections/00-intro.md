---
name: remuda
description: "Dispatch and drive coding agents (claude, codex, grok, agy, gemini) on Remuda Hub/Node hosts from a coordinator session. Use when the task is to run work in isolated git worktrees on other agents — create worktree, create pty instances, deliver task briefs, wait for DONE, read screens, stop — via the remuda CLI or the remuda_* MCP tools. Not for ordinary local shell work."
---

# Remuda

Remuda runs native coding-agent CLIs on Hub-connected Nodes. One worktree per
agent, one PTY instance per worktree, a task brief in, a `DONE <sha>` line out.

Two interchangeable surfaces. Use whichever this session has:

- **CLI** `remuda instance|worktree|fleet|merge …` when you can run shell.
- **MCP** `mcp__remuda__remuda_*` when launched with
  `--mcp-config docs/design/remuda-mcp.json --strict-mcp-config`.

The installed binary is the authority for flags — `remuda instance --help`,
`remuda merge --help`. Bare `remuda` has no default subcommand; don't run it
for discovery.
