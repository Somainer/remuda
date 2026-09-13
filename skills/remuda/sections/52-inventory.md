## Host Claude login inventory

`remuda doctor` and host `cli[]` report whether Claude is installed and
whether a native login or API gateway is configured. `auth` is
`gateway-native`, `logged_in`, `logged_out`, or `unknown`. Token and URL
values are never reported.

## Provider scoping (D-021)

Providers created in Remuda are **universal**: Hub stores the encrypted
token and every Node receives the profile through SecretBroker at launch.
A remote host can also have **host-scoped** profiles or bind to **native**
(its own CLI login, no Hub key). Hosts page: 自动 / 原生登录 / 指定
provider. New Session follows the host unless you pick 原生 or 网关.

## Workspace registration (D-023)

Human and Bot operators can list, add, or remove project directories on a running
Node using `GET/POST/DELETE /v1/hosts/{hostId}/workspaces`. Mutation bodies are
`{"path":"/home/dev/projects/example"}`. Agent credentials receive HTTP 403.
The Node validates absolute, existing, canonical directories against its
`workspace_roots` allowlist (default: its user's home directory), and rejects
registration inside another workspace's worktree directory. CLI `--workspace`
entries merge into `workspaces.json` in the Node data directory. Configure
`[node] workspace` (legacy primary), `workspaces` (additional roots), and
`workspace_roots` (allowlist); `REMUDA_WORKSPACE_ROOTS` accepts a JSON array.
Both `remuda node` and `remuda dev` accept repeated `--workspace` and
`--workspace-root` flags. Startup roots use the same validation as registration.

Mutation success means the Node prepared and committed the change and the Hub
persisted its acknowledged registry snapshot. Refresh the list after a lost
connection before retrying. Removing a workspace leaves files and running
sessions intact; new sessions must use a remaining registered workspace.
New Session selects a workspace and accepts an optional relative subpath. API
cwd values beginning with `~` or `$HOME` expand against the Node user's home;
containment within a registered workspace still applies.
