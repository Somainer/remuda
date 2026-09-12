## MCP equivalents

| CLI | MCP tool |
| --- | --- |
| `worktree create` | `remuda_worktree_create` |
| `merge <branch> --gate` / `--dry-run` | `remuda_merge` (`branch`, `gate` / `dryRun`, `web`, `noPush`, `repo`, `targetDir`) |
| `instance create` | `remuda_instance_create` |
| `instance list` | `remuda_instance_list` |
| `instance send` | `remuda_instance_send` (`text` or `file`) |
| `instance wait` | `remuda_instance_wait` (`until`, `timeoutMs`) |
| `instance read` | `remuda_instance_read` (`lines`, `source`) |
| `instance keys` | `remuda_instance_keys` (`keys: []`) |
| `instance stop` / `rm` | `remuda_instance_stop` (`scope`) / `remuda_instance_rm` |
| `fleet run` / `fleet send` | `remuda_fleet_run` / `remuda_fleet_send` |

Arguments are camelCase (`timeoutMs`, `promptFile`, `workspaceId`,
`afterSeq`). Claude Code exposes them as `mcp__remuda__<tool>`. Paths in
`file` / `promptFile` are read by the `remuda mcp` process, so they must
exist on the machine running it.

`remuda mcp` resolves the Hub itself — `--hub` / `REMUDA_HUB` /
`$REMUDA_DATA_DIR/dev-hub/listen` / `./data/dev-hub/listen` / `:18080` when a
`bootstrap-token` or `access-code` file exists, else `:8080`. Token:
`REMUDA_TOKEN`, else `REMUDA_BOOTSTRAP_TOKEN` or the dev access-code file.
After `remuda dev` the committed config works unedited; it holds no URL or
secret. Never put a token in a prompt. See `docs/design/remuda-mcp.md`.
