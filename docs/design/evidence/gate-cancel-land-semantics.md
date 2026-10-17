# Gate cancel vs. land semantics (c-ma-initiator r11 item 1)

## The race

A lane land runs verify then pushes `main`. A `gate.cancel` can arrive at any
point. A single pre-flight authority read does not fence the push: by the time
`merge --gate --land` reached the CAS, the cancel may have committed, the Node
process restarted (losing its local cancel records), or the dispatch frame may
have been paused across a carrier session. Force-with-lease only guards the git
ref, and the Hub's terminal-result guard only drops the reply *after* the push
already happened.

## Two-phase run with an explicit land grant

`gate.run` on the lane Node is split into two child processes with a Hub
fence at the boundary:

1. **Verify** — `merge --gate --no-push` (never advances/pushes main).
2. **Land grant** — at the verify→land boundary the Node calls the
   Node→Hub `gate.landGrant` RPC for exactly
   `(jobId, generation, hostId, sessionEpoch, mergeSha)`. The Hub writer
   transitions `Running → Landing`, serialized in the **same** writer path
   as `gate.cancel`:
   - the conditional update matches **only** a `running` row (a `canceling`
     row can never be granted) at the exact generation/host **and the
     authenticated carrier session the dispatch was bound to**; any other
     state or a session mismatch → refuse.
   - the already-`Landing` idempotent re-answer requires the same
     generation/host/**session**, so a paused frame on a replacement session
     after a reconnect is refused even when the generation still matches.
   - the grant records the authorized `mergeSha` and the session epoch on
     the Landing row.
3. **Land** — only a granted run spawns `merge --land` (the CAS push).

## Cancel outcomes

- **Cancel before the grant** — honored: no land child, nothing pushed;
  the job settles canceled/failed.
- **Cancel after the grant (row Landing)** — the push runs to completion.
  `POST /gate/jobs/{id}/cancel` returns a 409
  "landing in progress; cancel cannot stop the granted push" instead of
  pretending the cancel succeeded. The job's terminal state is `landed`
  (the honest outcome) or the land's own failure.
- **Carrier drop / Node restart** — `gate.authorize` (pre-execute) and
  `gate.landGrant` (pre-push) fail closed on a missing/dead/replacement
  carrier session. Records are generation-scoped and never age out; a
  paused frame delivered to a replacement session does not match its
  session (the Hub compares the socket's own authenticated session against
  the one recorded at dispatch, not a client-claimed epoch). The durable
  daemon controller registers the authorizer per session, includes a unique
  per-boot id in its epoch, and revokes in-flight waits on
  takeover/disconnect; the WSS transport revokes run authority for the whole
  reconnect window and rebinds after the new hello.
- **Landing is in-flight, and a Hub restart settles it without re-running** —
  until terminal, `landing` still holds the lane checkout and the project's
  land slot. After a restart the Hub probes the lane read-only
  (`gate.resolveLanding`): the granted merge already on the remote base →
  `landed`; otherwise `failed` with an explicit indeterminate reason. The
  push is never blindly re-issued.

There is **no Node-local allow floor**: the generation-scoped cancel
tombstone is a fast refuse, but the decision to execute or land is the Hub's
alone at both irreversible boundaries. A Node with no registered carrier
authorizer (a fresh process that never attached, or one whose session ended)
refuses — missing authority is permission. Tests obtain an allow only by
installing a test-only authorizer (`cfg(test)`/`test-faults`), unreachable
from a production `DevNode`.

## Attempt-qualified land evidence and its upgrade (r34/r35)

A granted land attempt's durable evidence lives in the Node's `gate-land`
directory and is bound to the attempt's FULL identity — job, attempt
generation AND the exact granted merge sha:

- live fence `land-<job>-g<gen>.json` (written before the held land child is
  released);
- death certificate `dead-<job>-g<gen>.json` (written only after the kernel
  reported the whole group ESRCH).

A finalizer opens ONLY its own attempt's immutable fence path; it never
discovers an attempt through the recyclable process-group id, so a detached
older finalizer cannot certify or unlink a newer attempt after pgid reuse.
The Hub's `gate.resolveLanding` probe pins `(generation, mergeSha)` from its
own job row (`attempts` + granted `mergeSha`); any mismatch, a missing
authority, or absent evidence reads `unknown` — never a generic "dead".

### Upgrading pre-r34 evidence and the operator route

Pre-r34 Nodes wrote single files `land-<job>.json` / `dead-<job>.json` with
no merge sha. On startup a legacy fence whose whole group is verified ESRCH is
explicitly migrated to `dead-<job>-g<gen>.json`, stamped
`"provenance":"legacy-fence"` and carrying NO merge; a legacy certificate is
migrated the same way (`"legacy-cert"`) on the first generation-pinned probe.
The missing merge is bound only through the Hub's authoritative grant data in
the probe — an unqualified certificate is never accepted as generic death
evidence.

A legacy record that cannot be bound to an exact validated attempt (missing or
unparseable generation/leader identity, unreadable file, ambiguous name) is
kept on disk and the Landing remains outcome-unknown. Its explicit recovery
route is the operator-only Hub endpoint

`POST /v1/projects/{project}/gate/jobs/{jobId}/resolve`

with `{"resolution":"failed"|"requeue", "expectedAttempts",
"expectedSessionEpoch", "expectedMergeSha", "rationale"}` (Human/Bot operator;
non-empty rationale). Unlike `/resolve-evidence`, this legacy route lets the
explicit operator decision stand when the read-only probe can only answer
unknown. The evidence-uncertain variant is deliberately excluded there and
must use `/resolve-evidence`, which fails closed on anything but an
established `landed`/`absent`.
