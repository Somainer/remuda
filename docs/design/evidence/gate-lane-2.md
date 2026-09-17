# Evidence: gate-lane-2

Date: 2026-09-18. Branch: `wt/c-landpush/land-from-home-host`.
Design: [coordinator-hierarchy.md](../coordinator-hierarchy.md) §8.2 (M1: the
coordinator uses `remuda` verbs only, never shell scripts);
decision: [D-034](../decisions.md). Predecessor: [gate-lane-1](gate-lane-1.md).

What shipped: a passing lane verify is **persisted** as a git ref, and a land can
be pushed from the project home host when the lane host holds no push credential
for the project remote. Together these close the hole gate-lane-1 left: before
this, a green lane run could be unlandable by hand-free means.

## The hole this closes

On 2026-09-18 a real gate run on this project (main `a56eab71`) passed every step
on the Node lane and reported `mergeSha 6f7d987a`. But the lane runner drives
`remuda merge --onto main --gate` with `--no-push` in a scratch worktree, so:

- the merge commit was built inside `<tmp>/remuda-mq-*/worktree`, which the merge
  CLI then removes;
- the lane repo's own `main` never moved (`--no-push` still advances local main
  only on the land path, which this run never took);
- nothing referenced the merge, so it survived only as a dangling object.

The coordinator's `git push origin <mergeSha>:refs/heads/main` from the home host
then failed, because that host had never received the object. Recovery was
manual: `git update-ref refs/gate/readme2 <sha>` on the lane to pin it, a fetch
over the operator's ssh remote, then a push.

The second half was `remuda land`: the lane runner passes `--land` and omits
`--no-push` when `request.push`, so the push happens **on the lane host**. A lane
whose host holds no credential for the project remote — the policy for the remote
lane — therefore cannot land at all.

## Surface

- Protocol `remuda-protocol/src/gate.rs`: `gate_merge_ref()` /
  `gate_branch_ref()`; additive `GateJob.mergeRef` and `GateRunResult.mergeRef`;
  `GatePushFrom` (`lane` | `home`); `GateRunParams.pushFrom`; new Hub→Node RPCs
  `gate.land` (`GateLandParams` / `GateLandResult`) and `gate.unpin`
  (`GateUnpinParams` / `GateUnpinResult`), both in `is_gate_call`.
  `ProjectGateLane` gains additive `pushFrom` and `fetchRemote`.
- Node `remuda-node/src/gate.rs`: after a passing verify the lane runner pins the
  merge and the verified branch tip **before** the scratch worktree is removed,
  and reports the ref as `mergeRef`. With `pushFrom: home` the lane never passes
  `--land`. `gate.land` fetches the pinned merge over `fetchRemote`, re-checks
  the parentage, and CAS-pushes the base branch; `gate.unpin` drops the refs.
  `dispatch_gate_rpc` is the single method table, used by `server.rs` too.
- Hub `remuda-hub/src/gatequeue.rs`: records `mergeRef`; `resolve_push_from()`;
  holds a `pushFrom: home` land `running` across the handoff so `remuda land`
  cannot read a lane verify as a completed land; `land_from_home()` writes the
  terminal state; `base-moved` re-queues a verify under the existing
  `MAX_LAND_ATTEMPTS`; refs dropped on cancel, after a land, after a superseding
  re-verify, and by `sweep_expired_refs()` (`gateRefRetentionMs`, default 7 days).
- CLI: `remuda gate` prints `mergeRef: <ref> -> <sha>` and the exact
  `remuda land <branch>` to run next; `remuda report` lists a passed-but-unlanded
  job as a `land` owner ask (`verified, not landed: remuda land <branch>`).

### Two deviations from the brief, and why

**The branch ref is `<id>.branch`, not `<id>/branch`.** Git refuses a ref that is
simultaneously a file and a directory, so the requested pair cannot both exist:

```
$ git update-ref refs/remuda/gate/gjb_1 $SHA          # ok
$ git update-ref refs/remuda/gate/gjb_1/branch $SHA
fatal: update_ref failed for ref 'refs/remuda/gate/gjb_1/branch': cannot lock
ref 'refs/remuda/gate/gjb_1/branch': 'refs/remuda/gate/gjb_1' exists; cannot
create 'refs/remuda/gate/gjb_1/branch'
```

Creating them in the reverse order fails symmetrically. A `.branch` sibling keeps
the merge ref spelled exactly as designed (`refs/remuda/gate/<gjb id>`) and both
refs still sweep under the single `refs/remuda/gate/` prefix.

**A `pushFrom: home` lane runs a verify, not `merge --land`.** `--land` advances
the lane's *local* main and reports `landed`; with no credential the push then
cannot happen, so the job would claim success while the remote never moved. That
is exactly the half-updated outcome the brief forbids, so the lane stays a pure
verify and the home host owns the transition to `landed`.

## Real verify plus real home-host land (this host)

Three real repositories: a bare origin seeded from this project's actual
`main` (`6f7d987a`, the commit from the incident), a lane checkout whose push URL
points at a nonexistent path so it genuinely holds no credential, and a home
checkout that reaches the lane as a remote (standing in for the operator's ssh
alias). Paths redacted as `<scratch>`.

### 1. The lane cannot push

```
$ git -C lane remote get-url --push origin
/nonexistent/no-credential.git
$ git -C lane push origin main:refs/heads/main
Please make sure you have the correct access rights
and the repository exists.
```

### 2. A real verify, pinning the merge before the worktree goes away

```
$ SCRATCH=$(mktemp -d /tmp/remuda-mq-evidence.XXXXXX)
$ git worktree add -q --detach "$SCRATCH/worktree" "$BASE"
$ git -C "$SCRATCH/worktree" merge -q --no-ff --no-edit \
      -m "merge: wt/c-landpush/land-from-home-host into main" \
      origin/wt/c-landpush/land-from-home-host
base   = 6f7d987ace248b3938b10dee77116be388ec2871
merged = a2c00b15a9369ac589a4ef91ce9d60203e62ae55

$ git update-ref refs/remuda/gate/gjb_evidence "$MERGE"
$ git update-ref refs/remuda/gate/gjb_evidence.branch "$(git rev-parse origin/wt/…)"
$ git worktree remove --force "$SCRATCH/worktree"

$ git for-each-ref refs/remuda/gate/ --format='%(refname) -> %(objectname:short)'
refs/remuda/gate/gjb_evidence -> a2c00b1
refs/remuda/gate/gjb_evidence.branch -> bcb4496
```

### 3. The old failure mode, reproduced

The lane's own `main` never moved, the merge is on no branch, and the home host
cannot push an object it was never given:

```
$ git -C lane rev-parse --short main
6f7d987                      # not the merge a2c00b1
$ git -C lane branch --contains a2c00b1 | wc -l
0                            # reachable from no branch

$ git -C home push origin a2c00b15a9369ac589a4ef91ce9d60203e62ae55:refs/heads/main
To <scratch>/origin.git
 ! [rejected]        a2c00b15a9369ac589a4ef91ce9d60203e62ae55 -> main (needs force)
error: failed to push some refs to '<scratch>/origin.git'
hint: You cannot update a remote ref that points at a non-commit object,
```

(The 2026-09-18 incident reported `src refspec does not match any` for the same
root cause — the home host not having the object. The wording here differs
because the sha is passed directly rather than as a name to resolve.)

And the pin is what makes the object durable — it survives an aggressive gc that
would otherwise collect it:

```
$ git -C lane gc --prune=now --quiet
$ git -C lane rev-parse --verify --short refs/remuda/gate/gjb_evidence
a2c00b1
```

### 4. The real land, through the shipped `gate.land`

Driven through `DevNode::dispatch_gate_rpc("gate.land", …)` — the same code path
the Hub calls, not hand-rolled git:

```
EVIDENCE_LAND_RESULT={
  "jobId": "gjb_evidence",
  "status": "landed",
  "mergeSha": "a2c00b15a9369ac589a4ef91ce9d60203e62ae55",
  "output": "git fetch lane-alias +refs/remuda/gate/gjb_evidence:refs/remuda/gate/incoming/gjb_evidence
git push --force-with-lease=refs/heads/main:6f7d987ace248b3938b10dee77116be388ec2871 origin a2c00b15a9369ac589a4ef91ce9d60203e62ae55:refs/heads/main"
}
```

`main` moved exactly once, to the verified merge, whose first parent is the base
the gate verified against:

```
$ git -C origin.git log --oneline -1 main
a2c00b1 merge: wt/c-landpush/land-from-home-host into main
first parent : 6f7d987   (verified base 6f7d987)
second parent: bcb4496   (the verified branch tip)
```

### 5. A moved base refuses, with nothing pushed

Re-running the same land — the base is now stale, because we just landed:

```
EVIDENCE_SECOND_RESULT={
  "jobId": "gjb_evidence",
  "status": "base-moved",
  "currentMainSha": "a2c00b15a9369ac589a4ef91ce9d60203e62ae55",
  "output": "git fetch lane-alias +refs/remuda/gate/gjb_evidence:refs/remuda/gate/incoming/gjb_evidence
origin/main is a2c00b15…, verified base 6f7d987a… — refusing"
}
```

`main` is unchanged. The Hub turns this into a re-queued verify under the same
attempt cap; it is never reported as success.

The push is a single `--force-with-lease` on the base branch, so the *remote*
performs the compare-and-swap. There is no partial-push outcome — a lease lost
between the pre-check and the push rejects the whole update:

```
$ git push --force-with-lease=refs/heads/main:$STALE origin $MERGE:refs/heads/main
 ! [rejected]        <merge> -> main (stale info)
error: failed to push some refs
exit code: 1        # and origin/main did not move
```

## Automated gates (this host)

- `cargo test -p remuda-node` — green, 19 lane-runner tests. New: the pin
  resolves to the verified merge after the worktree is removed **and** survives
  `worktree prune` + `gc --prune=now`, with `main` as first parent and the branch
  tip as second and on the `.branch` sibling; `gate.unpin` drops both refs and is
  idempotent; a `pushFrom: home` land never passes `--land`; the home-host land
  pushes once and refuses a moved base.
- `cargo test -p remuda-hub` — green, 13 `tests/gate_queue.rs` tests. New:
  `mergeRef` is recorded and returned on the job; a `pushFrom: home` land is
  pushed by the home host (never the lane) and then unpins the lane's refs; a
  home-host CAS refusal re-queues a verify and the *re-verified* merge is what
  lands; retention drops an unlanded verify's refs and clears `mergeRef` while
  keeping `mergeSha` as evidence.
- `cargo test -p remuda` — green, incl. the new `tests/land_cli.rs` (4 tests)
  driving real Nodes over real repos: `main` moves exactly once; a base moved by
  another lander is refused; a lane ref re-pinned at a different commit is
  refused rather than pushed; an unreachable lane fails rather than silently
  succeeding.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all` — clean.
- Generated types regenerated (`cargo run -p remuda-protocol --example
  gen_types`, then `pnpm run gen:api` in `web/`): `protocol.schema.json` and
  `web/src/types/generated.ts` gain the additive fields and the four new RPC
  types. `web/src/lib/api.generated.ts` is unchanged — no HTTP routes were added.

Mutation check: removing the `sweep_expired_refs` call from the scheduler tick
makes `ref_retention_drops_pins_for_an_unlanded_verify` fail, so the retention
test is load-bearing rather than incidentally green.

## Known gaps

- The merge commit reaches the home host by `git fetch` of `mergeRef` over the
  lane's `fetchRemote`. A Node RPC streaming the packfile is the better shape;
  `fetchRemote` is the only lane-reachability input `gate.land` takes, so that
  swap touches one call.
- `resolve_push_from()` defaults to `home` when a lane declares `fetchRemote` and
  the project has a `repoRemote`, and otherwise keeps the historical `lane`
  behaviour. It cannot yet *detect* a missing credential, because a Node reports
  no credential inventory — the brief's "defaults to home when the lane host
  reports no credential" is therefore expressed as a lane-config assertion. When
  a Node does report it, `resolve_push_from()` is the single place that changes.
- The home host pushes from a lane-shaped checkout pinned to its own host; a
  project whose home host has no configured lane cannot land this way (the job
  fails rather than pushing from an arbitrary directory).
- Retention sweeps at the shorter of one minute and the retention window; it does
  not yet coalesce with the gate-log TTL, which is still lazy-expiry on read.
