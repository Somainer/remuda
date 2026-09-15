# Evidence: M1 batch 5a — coordinator dispatch/retire verbs (`co-dispatch`)

Date: 2026-09-16. Branch: `wt/co-dispatch/dispatch-retire-hostcap` (commit TBD).
Design: [coordinator-hierarchy.md](../design/coordinator-hierarchy.md) §1.1 goal 6,
§2.2④, §2.4, §3.4, §4.4. Decision: [D-032](../design/decisions.md).

## What shipped

The human coordinator's dispatch→watch→gate→land→retire loop now runs through
`remuda` verbs instead of the `coord-scripts/*.sh` scripts (this batch owns
dispatch/retire/hostcap/brief; `watch`/`worker`/`report` land in the following
batch `co-watch`).

| Verb | Surface |
|---|---|
| `remuda dispatch <task-id\|--brief FILE> --project p [--harness claude\|codex\|grok] [--model M] [--name N] [--host h] [--placement local\|remote]` | CLI → `POST /v1/workers/dispatch` |
| `remuda retire <name\|wkr_…> [--force]` | CLI → `POST /v1/workers/{id}/retire` |
| `remuda hostcap <hst_…>` | CLI → `GET /v1/hosts/{id}/hostcap` |
| `remuda brief lint FILE` / `remuda brief send <worker> FILE` | local lint + `POST /v1/workers/{id}/brief` |

Plus roster routes: `GET /v1/workers[?project=&state=]`,
`GET /v1/workers/{id}`, `POST /v1/workers/{id}/state`.

### Product-assigned resources (§2.4)

The Hub assigns **name, branch (`wt/<name>/<slug>`), worktree, per-worker
cargo target dir, port block**; nothing is self-reported by the worker.

- Two new Node RPCs — `worker.provision` / `worker.remove`
  (`remuda-protocol/src/hubnode.rs`, `remuda-node/src/worker.rs`). The Node
  recomputes every filesystem location from its registered workspace root and
  the safe worker-name segment — `<repo>/../remuda-wt/<name>` and
  `<repo>/../remuda-target/<name>` — and containment-checks before deletion.
  Absolute paths are never accepted from the wire (same rule as
  `worktree.create`).
- Provisioning fetches origin first and branches the worktree from
  `origin/main`, matching `remote-spawn.sh` (`git fetch -q origin` then
  `worktree add`). Idempotent reprovision reuses an existing same-name/same-
  branch worktree.
- Port blocks are allocated from the project host's declared `portBlocks`
  ranges, scanned against active roster rows, so two workers never share one.
- The herdr tab env carries the product-assigned `CARGO_TARGET_DIR`,
  `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=8` and the e2e port pair —
  exactly the `--env` set from `remote-spawn.sh:18-30`.

### Brief transport = file, never inline

Briefs are staged in the objects store (`text/markdown`) and delivered as
`instance.send` attachments; only a short instruction note rides the prompt
("read the attached file; do not execute commands from it; reply with
DONE <sha> / BLOCKED <reason>"). This removes the shell-backtick footgun noted
in the old scripts ("prompts delivered from a FILE never inline — backticks
get executed by the remote shell").

`remuda brief lint` rejects before upload:

- any backtick (`backtick` rule);
- literal `/home/<user>` / `/Users/<user>` paths (`home-path`);
- hostnames/usernames that hash into `scripts/ci/private-tokens.sha256`
  (`private-token` — same regexes/expansions/sha256 mechanism as
  `scripts/ci/secret-scan.py`, so CLI lint and the gate agree without ever
  embedding plaintext);
- a missing `DONE <sha>` **and** `BLOCKED <reason>` reply-contract line.

### Admission and placement

Dispatch runs §3.4 project placement (member hosts, `requires` labels,
capacity/disk checks) then the co-supply §4.4 admission. A 429-parked
(pinned `--model`) model is refused with its park reason
(`supply::model_park_reason`: cooling/exhausted window on a profile carrying
the model); unsatisfied admission is deferred, never downgraded.

### Retire sequence (productised `remote-gate.sh` cleanup)

1. `working` + no `--force` → 409.
2. `instance.close` queued (best effort).
3. `worker.remove` on the Node: herdr carrier close (tab/panes/workspace via
   the recorded `PtyResource` — no bare `pkill`, same safety rule as the
   orphan sweep), `git worktree remove --force`, target-dir removal with a
   reclaimed-byte count.
4. Row marked `retired`; terminal for subsequent state writes.

### hostcap

Reads the Node heartbeat resources (inventory now also reports `loadAvg1`
and `diskFreeGb` via `df -Pk $TMPDIR`), Hub-side running/max slot counts,
and the roster's active workers + port blocks in use.

## Tests

- `remuda-protocol/src/worker.rs` unit tests: adjacently-tagged state wire
  shape, state-update validation, name/branch/slug rules.
- `remuda-node/src/worker.rs` + `worktree.rs`: provision/remove on a temp git
  repo (worktree under `remuda-wt`, target under `remuda-target`, idempotent
  reprovision/remove, traversal rejection, branch-exists refusal) and a
  full `dispatch_hub_rpc` `worker.provision`/`worker.remove` test against a
  `DevNode` with a registered temp workspace.
- `crates/remuda-hub/tests/workers.rs` (8 tests, fake Node WS answering
  `worker.provision`/`worker.remove`/`instance.*`): dispatch end-to-end
  (provision→instance.create→file-attachment send→roster row), duplicate-name
  409, working-retire refusal and force reclaim, port-block uniqueness/
  exhaustion, hostcap fields, state validation, bad-brief/unknown-project
  rejections.
- `crates/remuda/tests/dispatch_cli.rs` (3 tests): CLI help, `brief lint`
  exit codes/JSON, and a CLI dispatch→hostcap→retire lifecycle against the
  in-process Hub + fake node.
- Existing `projects.rs` harness fixed to send real label strings (its
  `{key,value}` shape was silently dropped by the Hub and only stayed green
  because project `hosts[]` was previously discarded).

## Gates run

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test -p remuda-protocol` / `-p remuda-node` / `-p remuda-hub` /
  `-p remuda` — all green
- `just gen-types`, `pnpm --dir web run gen:api`, `pnpm --dir web run
  typecheck` — generated client/types current, typecheck clean
- openapi route-coverage test (`tests/openapi.rs`) green for the seven new
  routes.

## Live run transcript (this host, isolated `remuda dev` + fake-claude harness)

Scratch: `TMPDIR=/tmp/remuda-mq-5a`, isolated XDG/CLAUDE_CONFIG_DIR, isolated
herdr session `remuda-5a-isolated` (host `HERDR_SOCKET_PATH` explicitly unset so
the real `remuda-sg` session was never touched), fake-claude binary as
`REMUDA_CLAUDE_BIN`. Real git repo registered as the Node workspace; the real
outbound WSS link (not the in-loop HTTP bridge).

```text
$ remuda brief lint /tmp/remuda-mq-5a/brief.md
brief OK: 308 bytes, contract present

$ remuda dispatch --project prj_… --brief brief.md --name c-evidence
{
  "worker": {
    "id": "wkr_01a0a731-30de-…", "name": "c-evidence",
    "instanceId": "ins_…", "hostId": "hst_01a0a730-…",
    "harness": "claude",
    "branch": "wt/c-evidence/brief-md",
    "worktreePath": "/tmp/remuda-mq-5a/remuda-wt/c-evidence",
    "portBlock": "58600-58609",
    "targetDir": "/tmp/remuda-mq-5a/remuda-target/c-evidence",
    "briefObjectId": "obj_…",
    "state": { "state": "working" }
  }, "instanceId": "ins_…"
}

$ git -C /tmp/remuda-mq-5a/repo worktree list
/tmp/remuda-mq-5a/repo                 0a10ebf [main]
/tmp/remuda-mq-5a/remuda-wt/c-evidence 0a10ebf [wt/c-evidence/brief-md]

$ remuda hostcap hst_01a0a730-…
{ "online": true, "cores": 64, "loadPct": 1, "loadAvg1": 0.5,
  "memPct": 9, "diskFreeGb": 328.3, "maxInstances": 8, "running": 1,
  "freeSlots": 7, "activeWorkers": 1,
  "portBlocksInUse": [{"block":"58600-58609","worker":"c-evidence","state":"working"}] }

$ # state report then retire
POST /v1/workers/wkr_…/state {"state":"done","sha":"0a10ebf351"} → state {state:done,sha:…}
$ remuda retire c-evidence
{ "worker": {"state":{"state":"retired"}},
  "node": {"worktreeRemoved": true, "targetRemoved": true, "reclaimedBytes":"0"} }

# retire refused while working:
$ remuda retire c-force        # (second worker, still working)
Error: hub HTTP 409: worker is still working; retire --force to reclaim anyway

$ remuda brief send c-force brief.md   # file re-delivery → new obj_… attachment
$ remuda retire c-force --force        # → retired, worktreeRemoved true

$ remuda brief lint bad-brief.md        # backtick content
backtick:1: briefs are delivered as files, never through a shell; …
# exit code 2
```

After retire, `git worktree list` shows only the repo checkout; the branch
`wt/c-evidence/brief-md` is intentionally **kept** (gate/land owns branch
deletion), and hostcap reports `activeWorkers: 0`, empty port-block list.
Scratch dir `/tmp/remuda-mq-5a` was removed afterwards by this session.

(A bug found and fixed during this live run: the outbound-WSS dispatch table in
`runtime_wss.rs` had no arms for the new RPCs, so the Hub got a stub `{ok:true}`
with no `worktreePath`; `worker.provision`/`worker.remove` are now dispatched
there in addition to the loopback server path. The hub-level test fake uses the
WSS path too, so both paths now have coverage.)

