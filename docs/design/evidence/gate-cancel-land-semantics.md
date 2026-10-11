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
   `(jobId, generation, hostId, sessionEpoch)`. The Hub writer transitions
   `Running → Landing`, serialized in the **same** writer path as
   `gate.cancel`:
   - row `Running`/`Canceling` at the exact generation/host/session →
     grant; `Canceling`/`Canceled` (or any mismatch) → refuse.
   - the row idempotently re-answers an already-`Landing` row.
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
  epoch. The durable daemon controller registers the authorizer per
  session and revokes in-flight waits on takeover/disconnect.

The Node's local generation-scoped cancel floor remains as a fast path
(and the only authority for a never-attached direct/dev caller); the Hub
is the authority at both irreversible boundaries.
