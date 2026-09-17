# Evidence: an unmatched `--model` pin is refused, never silently replaced

Date: 2026-09-17
Branch: `wt/c-pinrefuse/refuse-unknown-model-pin`
Design: `docs/design/coordinator-hierarchy.md` §4.3 (pin semantics), §4.4 step 8 (no silent downgrade).

## Why

On 2026-09-17 a coordinator ran

```
remuda dispatch --model ark/seed-evolving[1m]
```

against a Hub whose provider catalog lists `passthrough/ark/seed-evolving`
(and not the pinned id). The dispatch **succeeded**: `supply.rs` marked no
candidate `pinned` when the pin matched nothing, and the ranker launched the
list head (`claude-fable-5.1` in the incident). A pin is a hard constraint;
the request must fail instead of substituting.

This evidence replays the exact shape with synthetic ids
(`synth/seed-evolving[1m]` vs the listed `passthrough/synth/seed-evolving`).

## Setup (identical for both runs)

- `remuda hub` on loopback, fresh data dir, one enrolled **fake Node**
  (answers `worker.provision` / `instance.create` / `instance.send` and logs
  every RPC it receives; no real provisioning).
- One project with the fake host as a build member, port blocks
  `59100-59129`, one secret-less gateway profile listing two enabled models:

```json
{
  "name": "synth-passthrough",
  "kind": "gateway",
  "models": [
    {"id": "passthrough/synth/seed-evolving", "family": "synth",
     "role": "workhorse", "priority": 20,
     "fallback": ["passthrough/synth/seed-legacy"]},
    {"id": "passthrough/synth/seed-legacy", "family": "synth",
     "role": "workhorse", "priority": 10}
  ]
}
```

- `before` = a clean build of `origin/main` (`f66dce9e`); `after` = the
  branch build. Same commands, same inputs, only the binary differs.

## 1. Before: unknown pin is silently substituted

### 1a. `remuda profile probe --pin-model synth/seed-evolving[1m]` — exit 0, 200 OK

The dry-run "succeeds" and answers a model the caller never asked for:

```json
{
  "chosen": {
    "profileId": "pvp_…",
    "modelId": "passthrough/synth/seed-evolving",
    "family": "synth",
    "hostId": "hst_…",
    "fallback": ["passthrough/synth/seed-legacy"]
  },
  "ranked": [
    {"profileId": "pvp_…", "modelId": "passthrough/synth/seed-evolving",
     "priority": 20, "state": "available"},
    {"profileId": "pvp_…", "modelId": "passthrough/synth/seed-legacy",
     "priority": 10, "state": "available"}
  ],
  "rejected": [],
  "reasons": ["priority 20 wins; 100% window headroom"],
  "deferred": false,
  "deferredUntil": null
}
```

### 1b. `remuda dispatch … --model 'synth/seed-evolving[1m]'` — exit 0

```
$ remuda dispatch --project prj_… --brief brief.md --model 'synth/seed-evolving[1m]'; echo "exit=$?"
exit=0
{
  "worker": {
    "name": "w01a0",
    "model": "passthrough/synth/seed-evolving",
    "providerProfileId": "pvp_…",
    "state": { "state": "working" },
    "supplyDecision": {
      "chosen": { "modelId": "passthrough/synth/seed-evolving", … },
      "rejected": [],
      "reasons": ["priority 20 wins; 100% window headroom"]
    },
    …
  }
}
```

Raw HTTP shows the same: `POST /v1/workers/dispatch` with
`"model":"synth/seed-evolving[1m]"` → **HTTP 200**, response worker
`rawpin` with `model: passthrough/synth/seed-evolving`, state `working`.

The fake Node was told to provision and launch — the substitution had real
side effects:

```
── node RPC methods called by the Hub:
  worker.provision
  instance.create
  instance.send
  (… one triple per dispatch, three in total)
── roster:
  w01a0    model= passthrough/synth/seed-evolving
  rawpin   model= passthrough/synth/seed-evolving
  knownpin model= passthrough/synth/seed-evolving
```

This is the incident: a pin the operator forbade substitutions for produced
a running worker on the ranker's favourite.

## 2. After: unknown pin is a hard 409 refusal

### 2a. `remuda profile probe --pin-model synth/seed-evolving[1m]` — exit 1

```
$ remuda profile probe --pin-model 'synth/seed-evolving[1m]'; echo "exit=$?"
Error: hub HTTP 409: pin refused: no listed model/supply matches the pin
  - pinned model "synth/seed-evolving[1m]" is not listed by any provider profile — a pin is a hard constraint; dispatch refused, never substituted
  - did you mean "passthrough/synth/seed-evolving"?
  - did you mean "passthrough/synth/seed-legacy"?
exit=1
```

### 2b. `remuda dispatch … --model 'synth/seed-evolving[1m]'` — exit 1

Same stderr, exit code 1, no stdout. Raw HTTP:

```
$ curl -s -w '\nHTTP_STATUS:%{http_code}\n' -X POST …/v1/workers/dispatch \
    -H 'content-type: application/json' \
    -d '{"projectId":"prj_…","brief":"…","model":"synth/seed-evolving[1m]"}'
{"error":"pin refused: no listed model/supply matches the pin",
 "code":"PIN_REFUSED",
 "pin":{"harness":"claude","model":"synth/seed-evolving[1m]"},
 "reasons":[
   "pinned model \"synth/seed-evolving[1m]\" is not listed by any provider profile — a pin is a hard constraint; dispatch refused, never substituted",
   "did you mean \"passthrough/synth/seed-evolving\"?",
   "did you mean \"passthrough/synth/seed-legacy\"?"],
 "suggestions":["passthrough/synth/seed-evolving",
                "passthrough/synth/seed-legacy"]}
HTTP_STATUS:409
```

`POST /v1/supply/resolve` (the probe endpoint) returns the same 409 body for
the same input — refusal logic lives in the solver, and every admission entry
point enforces it.

### 2c. No side effects

The refusal is returned before worker-name allocation, port-block
allocation, and the Node `worker.provision` call.

```
── roster:
  knownpin model= passthrough/synth/seed-evolving
  warnpin  model= passthrough/synth/seed-legacy
── node RPC methods called by the Hub:
  worker.provision      ← known pin (step 3)
  instance.create
  instance.send
  worker.provision      ← warned pin (step 4)
  instance.create
  instance.send
```

The unknown-pin attempts created **no roster row** and caused **zero Node
RPCs**.

## 3. After: a known pin is still honoured

```
$ remuda dispatch … --name knownpin \
    --model passthrough/synth/seed-evolving; echo "exit=$?"
exit=0
```

Worker `knownpin` launches on `passthrough/synth/seed-evolving`; its
`warnings` list is empty (`[]`) — the pin matches the project workhorse.

## 4. After: pin ≠ project workhorse is a warning, not a refusal

With the project's `modelRoles.workhorse` set to
`passthrough/synth/seed-evolving`, pinning the other listed model still
dispatches (the pin wins), carrying one informational warning:

```
$ remuda dispatch … --name warnpin --model passthrough/synth/seed-legacy; echo "exit=$?"
exit=0
```

```json
"model": "passthrough/synth/seed-legacy",
"warnings": [
  "pinned model \"passthrough/synth/seed-legacy\" differs from the project \
workhorse \"passthrough/synth/seed-evolving\"; honoring the pin (informational)"
]
```

## Exit-code matrix

| Command | before | after |
| --- | --- | --- |
| `profile probe --pin-model synth/seed-evolving[1m]` | 0 | 1 |
| `dispatch --model synth/seed-evolving[1m]` | 0 | 1 |
| `dispatch --model passthrough/synth/seed-evolving` (listed) | 0 | 0 |
| `dispatch --model passthrough/synth/seed-legacy` (listed, ≠ workhorse) | 0 | 0 + warning |

## Suggestion ranking

Suggestions are the five closest *enabled listed* ids, scored on:
listed id ending in the pin (`ark/seed-evolving` →
`passthrough/ark/seed-evolving`), shared `/`-path tails, qualifier-insensitive
equal tail (`seed-evolving[1m]` ≈ `seed-evolving`), shared name tokens, and a
small Levenshtein tiebreaker. Unit-covered in
`crates/remuda-hub/src/supply.rs`:

- `unknown_model_pin_is_refused_not_substituted_with_suggestions`
- `refusal_suggestions_are_capped_at_five`
- `unknown_supply_pin_is_refused`
- `known_pin_is_honored_without_refusal`
- `workhorse_warning_is_informational_only`

Integration/CLI coverage:

- `crates/remuda-hub/tests/supply.rs::unknown_pin_refused_on_resolve_and_known_pin_honored`
- `crates/remuda-hub/tests/workers.rs::dispatch_refuses_unknown_model_pin_without_provisioning`
  (asserts HTTP 409, no `worker.provision` / `instance.create` RPC, empty
  roster)
- `crates/remuda-hub/tests/workers.rs::dispatch_honors_known_model_pin_and_provisions`
- `crates/remuda-hub/tests/workers.rs::dispatch_warns_when_honored_pin_differs_from_project_workhorse`
- `crates/remuda/tests/dispatch_cli.rs::unknown_model_pin_refuses_nonzero_with_message`
  (non-zero `remuda dispatch` and `remuda profile probe`, stderr names the
  pin and the suggestion)

## Surfaces changed

| Surface | File |
| --- | --- |
| Refusal type, suggestion scoring, pin branch in `solve()`, workhorse warning | `crates/remuda-hub/src/supply.rs` |
| `HubError::PinRefused` → HTTP 409 `PIN_REFUSED` (`pin`/`reasons`/`suggestions`) | `crates/remuda-hub/src/error.rs` |
| Dispatch admission refuses before any allocation/provisioning; warning rides `warnings` | `crates/remuda-hub/src/workers.rs` |
| Instance-create admission enforces the same refusal | `crates/remuda-hub/src/http.rs` |
| CLI renders refusal reasons on stderr for `dispatch` and `profile probe` | `crates/remuda/src/cmd/dispatch.rs`, `crates/remuda/src/cmd/profile.rs`, `crates/remuda/src/cmd/hub_client.rs` |

No protocol-schema change: the 409 carries the rejected pin and suggestions
alongside the standard error envelope, so no client regeneration was needed.
