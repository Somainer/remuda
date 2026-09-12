# Reclaim 1 — Node / Herdr resource reclamation

Date: 2026-09-13 (Asia/Shanghai). Agent: c-cleanup. Branch:
`wt/c-cleanup/resource-reclaim`. Integration base: `bf6af35` (main). The
initial cycles started before rebasing; the restart and clean shutdown used the
rebased implementation and preserved the upstream dev identity migration.

## Result

An isolated real `remuda dev` on Hub **58780** / Node **58787** returned its
Herdr session to **0 panes, 0 tabs, 0 workspaces, 0 durable ownership records**
after stop cycles and after Node shutdown. Restart adopted the recorded live
pane and removed an intentionally orphaned workspace plus the unused root shell.
SIGTERM with both a live instance and an already-ended driver worker exited **0**.
The shutdown log contained none of:

```text
driver shutdown did not settle
instance driver task is unavailable
one or more drivers did not complete shutdown
```

The dedicated session was `remuda-c-cleanup`, using Herdr **0.9.0** and Claude
Code **2.1.269**. Only that session was stopped/deleted after verification. The
assigned listeners were free after shutdown. No task prompts were submitted;
CLI startup used `haiku` and `--max-budget-usd 0.3`, followed by shutdown controls.

## Live observations

Counts below came from `session.snapshot` on the explicit named Herdr socket and
`SELECT count(*) FROM pty_resources` in the isolated Node's `node.sqlite`.
Create/close commands were awaited through `GET /v1/commands/{id}`; completed
closes were also checked through `GET /v1/instances/{id}`.

| Checkpoint | Panes | Tabs | Workspaces | Store resources | Outcome |
| --- | ---: | ---: | ---: | ---: | --- |
| First generic-pty start | 2 | 1 | 1 | 1 | Create settled completed |
| First generic-pty stop | 0 | 0 | 0 | 0 | Close completed; instance exited |
| First claude-pty start attempt | 0 | 0 | 0 | 0 | Create rejected with `agent_pane_busy`; partial launch reclaimed |
| Close after that worker ended | 0 | 0 | 0 | 0 | Close completed; instance exited |
| Second generic-pty start | 2 | 1 | 1 | 1 | Create settled completed |
| Second generic-pty stop | 0 | 0 | 0 | 0 | Close completed; instance exited |
| Next claude-pty start | 2 | 1 | 1 | 1 | Create settled completed |
| claude-pty stop | 0 | 0 | 0 | 0 | Close completed; instance exited |
| Known live generic-pty plus orphan, before SIGKILL | 3 | 2 | 2 | 1 | Orphan had no store record |
| Restart, before new commands | 1 | 1 | 1 | 1 | Known pane adopted; orphan and root shell removed |
| Stop adopted instance | 0 | 0 | 0 | 0 | Existing instance close completed |
| Before final SIGTERM | 2 | 1 | 1 | 1 | One live generic-pty; another worker had already rejected `--bare` |
| After final SIGTERM | 0 | 0 | 0 | 0 | Dev process exit code 0 |

The first claude-pty rejection is a real shell-readiness race in `agent.start`,
not a successful native launch. Its cleanup and subsequent close succeeded.
No change to that startup readiness policy is claimed here.

Restart logged:

```text
2026-09-12T18:12:12.223655Z INFO adopted live Herdr pane without replay
```

The original instance identity remained `ins_01a096cd-d38f-73e4-be1f-137db5998a22`.
It was not recreated and no old prompt was replayed. After final shutdown, both
`ins_01a096d3-0cbf-716f-a806-e775de82abfc` (live at shutdown) and
`ins_01a096d3-0ca9-71b7-8a8c-318b131756ed` (already-ended worker) were durably
`exited`. Their `exit` observations retained `code: null` and `signal: null` with
observed timestamps; the Herdr API did not supply a native process exit code.
Carrier removal does not establish native task success or rollback of tool work.

## Implementation and configuration

- `generic-pty` and `claude-pty` register the created workspace/root tab before
  `agent.start`, then update the record with the agent pane. Allocation and
  registration finish even if Node shutdown cancels the worker waiting for the
  creation response. Records include the exact socket, session, instance,
  workspace, tab, pane, and a unique workspace label to reject reused handles.
- Close sends Ctrl-C twice, allows up to one second for graceful exit, and sends
  the shell `exit` builtin only after agent absence and foreground shell identity
  agree. It then closes the whole tab/workspace and verifies removal. Socket
  operations have two-second deadlines. Native observation pumps are stopped;
  a `carrier_closed` observation and Node `exited` entity record the exit.
  Native homes, login files, and transcripts are retained.
- Node shutdown gates mutations, cancels/joins workers, closes retained drivers,
  and sweeps the durable resource ledger. A finished worker or a dropped
  observation receiver does not make successful cleanup fail. Failed cleanup
  retains ownership for reconciliation. The existing composition shutdown
  deadline still bounds the whole operation. Stdio Nodes handle EOF, SIGINT,
  and SIGTERM through the same cleanup path (ten-second shutdown deadline).
- Node startup adopts matching live panes with a fresh control worker, marks
  unsettled commands unknown, and removes the other resources in its isolated
  session. This is carrier/control adoption; it does not replay prompts,
  approvals, native start, or semantic resume.
- `REMUDA_HERDR_SESSION` selects the isolated session (default `remuda-node`).
  `REMUDA_HERDR_ORPHAN_SWEEP=0`, or `remuda dev/node
  --no-herdr-orphan-sweep`, preserves unknown panes for manual recovery. Known
  live panes are still adopted. Shutdown still reclaims recorded ownership.
- Hub records `offline_since`, clears it on hello/heartbeat, and checks once per
  second for expired offline hosts. The default grace is **600000 ms** (ten
  minutes), configurable as `[hub] host_lost_grace_ms` or
  `REMUDA_HOST_LOST_GRACE_MS`. Expired instances become `exited` with
  `lastError: "host-lost"`. The default `GET /v1/instances` hides those rows;
  `GET /v1/instances?includeHistory=true`, direct instance lookup, and the stored
  journal preserve history. This is a Hub liveness projection, not a fabricated
  Node journal event or native completion.

Shared touches are limited to startup/shutdown wiring in the composition
entrypoints, the Herdr client's `workspace.close` method and fake equivalent,
Hub liveness/list configuration, the standalone stdio binary runtime drain,
and the Node dependency on `remuda-herdr`.
Claude background terminal attachments use the same ownership tracker so those
Node-created workspaces are included in cleanup.

## Reproduction and validation

`MAIN_REPO` and `WORKTREE` below denote the checkouts; these placeholders avoid
recording personal paths. Tests use the agent-specific build cache with
incremental compilation disabled.

```sh
cd "$WORKTREE"
export CARGO_TARGET_DIR="$MAIN_REPO/target-c-cleanup"
export CARGO_INCREMENTAL=0
cargo fmt --all
cargo build -p remuda -p remuda-node -p remuda-testing
cargo test -p remuda-herdr -p remuda-testing -p remuda-driver \
  -p remuda-node -p remuda-hub -p remuda
cargo clippy -p remuda-herdr -p remuda-testing -p remuda-driver \
  -p remuda-node -p remuda-hub -p remuda -- -D warnings
./scripts/ci/secret-scan.sh
```

The six-crate run passed **279 tests**, with **0 failures** and **10 ignored**
opt-in tests. Focused driver, Hub, and Node reruns covered the final adjustments.
The Node suite additionally includes `sigterm_with_open_stdin_exits_cleanly`.
A standalone stdio subprocess smoke received `node.hello`, held stdin open,
sent SIGTERM, and observed exit code **0**. The binary bounds Tokio runtime
shutdown to one second after driver cleanup so its blocking stdin read cannot
hold the process open.
The automated checks include:

- fake-herdr stop tests for both PTY drivers assert zero panes, tabs, workspaces,
  an exit observation, and idempotent close;
- restart reload of the durable ledger, live-pane adoption, explicit follow-up
  control, orphan removal, and shutdown cleanup;
- orphan-sweep opt-out, failed/partial ownership without a driver task, and
  protection against workspace ID reuse;
- panic supervision followed by successful shutdown of the ended worker;
- Hub grace, reconnect, idempotence, and history preservation, plus a real
  WebSocket-disconnect/HTTP-list test with an accelerated **20 ms** grace.
  This verifies the configurable path without claiming a ten-minute live wait.

The real dev invocation was equivalent to:

```sh
reclaim_run=/tmp/remuda-c-cleanup-live
# Create a random access-code file with mode 0600 before starting.
REMUDA_HERDR_SESSION=remuda-c-cleanup REMUDA_COOKIE_SECURE=0 RUST_LOG=info \
  "$CARGO_TARGET_DIR/debug/remuda" --data-dir "$reclaim_run/data" dev \
  --hub-listen 127.0.0.1:58780 --listen 127.0.0.1:58787 \
  --workspace "$reclaim_run/workspace" \
  --access-code-file "$reclaim_run/access-code"
```

Local raw evidence is retained under `/tmp/remuda-c-cleanup-live/`:
`dev.log`, `restart.log`, and `evidence.jsonl`; build/test/lint logs use
`/tmp/remuda-c-cleanup-*.log`. Raw logs and access credentials are not committed.
