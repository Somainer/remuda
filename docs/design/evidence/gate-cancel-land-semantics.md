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
