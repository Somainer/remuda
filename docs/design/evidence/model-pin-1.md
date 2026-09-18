# A `--model` pin must reach the process, or the launch must refuse

A dispatch that names a model must put that model in front of the process, on
every claude launch path. If it cannot, the launch must fail loudly rather than
run on something else while every record claims the pin was honoured.

Worker `c-modelpin`, 2026-09-18. Scratch dev server only: a `remuda dev` Node
with the fake harness from `crates/remuda-testing`, on an isolated temp root. No
host, user or path names appear below; every model id is either a synthetic
stand-in or a host-generic gateway id.

Implementation: branch `wt/c-modelpin/b-modelpin-md`; the full implementation
through the round-3 source-scoping and explicit-pin fixes is at
`46e92c6fa99c16c8f9b489f2607f7d112ba18b4a` (this evidence doc lands in the
commits immediately after). The argv/overlay channel work and reporting are in
the earlier commits; the post-launch read-back gate, the alias-aware comparison,
the Hub launch-source scope, and the explicit-`model_pin` arming follow on the
same branch.

---

## 1. The launch, the argv, and the transcript

One native `shell-pty` launch, kind `claude`, delegation `none` (native profile
— the exact shape of the 2026-09-18 demo), with an explicit pin, against a
seeded host settings layer that names a *different* model:

| Input | Value |
|---|---|
| driver / kind | `shell-pty` / `claude` |
| delegation | `none` (native profile) |
| requested pin | `model_hub/es1_orange_o50[1m]` |
| host `settings.json` `model` | `model_hub/es1_orange_o48[1m]` |
| host `settings.json` `env.ANTHROPIC_MODEL` | `model_hub/es1_orange_o48[1m]` |

### 1.1 The argv the recipe builds

From `materialize_shell_pty_agent`, the pinned spec above
(`cargo test -p remuda-driver --test materializer`):

```
ARGV=["--model", "model_hub/es1_orange_o50[1m]"]
MODEL_REQUESTED=model_hub/es1_orange_o50[1m]
```

The token is present and byte-identical to the request, `[1m]` suffix included.
Before this change the same spec produced an argv with **no model token at all**
while `MODEL_REQUESTED` already read exactly as it does here — which is why the
audit record and the Hub roster row both testified to a pin that reached
nothing.

### 1.2 The merged overlay the CLI was handed

Captured from the live launch (`model_pin_evidence.rs`, the 0600 instance
overlay the PTY passed as `--settings`):

```
model key           = "model_hub/es1_orange_o50[1m]"
env.ANTHROPIC_MODEL = null
```

The pin owns the `model` key, and the host's `ANTHROPIC_MODEL` — which outranks
that key inside Claude Code — is gone. The host's endpoint and credential
variables are deliberately left in place: under `none` the host still owns where
the session talks and as whom.

### 1.3 What the process itself recorded

The fake harness parses `--model` (`crates/remuda-testing/src/flags.rs:64`) and
echoes it into the records it writes, so its transcript is evidence about the
process rather than about our intent:

```
assistant record: message.model = model_hub/es1_orange_o50[1m]
instance lifecycle = Ready
last_error = None
```

The pinned id is what answered, and the launch was not refused.

### 1.4 The negative control (argv channel)

The argv/overlay half was confirmed able to fail: the end-to-end test was re-run
with both launch channels deliberately neutered (preset `model_flag` set to
`None`, the overlay pin skipped). With no `--model` argv and no overlay pin the
harness never produced a transcript naming the pinned id, so the wait for one
timed out:

```
panicked at crates/remuda-node/tests/model_pin_launch.rs:
  no harness transcript with a model under <scratch>/claude-home
test result: FAILED. 0 passed; 1 failed
```

(The fake harness's own default is `claude-opus-5`, which is also why an id
assertion alone would not have been enough; the timeout is the honest failure.)
The read-back gate has its own negative control — the §3.4 mismatch case, which
fails if a substitution is not stopped.

---

## 2. Audit: 2026-09-17/18 evidence that reported a pin which was not effective

The pin each doc records was a *request*, not an observation. But the rows below
are not all the same class — the launch profile decides whether the argv hole
even applied — so they are split:

| Document | Lines | Class |
|---|---|---|
| `docs/design/evidence/dispatch-onboarding-1.md` | 8–9 | **native shell-pty** (`--driver shell-pty`, native profile, delegation none): the argv hole applied directly. The dispatch `--model ark/seed-evolving[1m]` reached no model token. Any claim about which model answered must be re-verified. |
| `docs/design/evidence/self-host-1.md` | 28, 63, 78, 80 | **native shell-pty**, same class: profile `defaultModel: ark/seed-evolving[1m]`; two dispatches `--model ark/seed-evolving[1m]`; the roster row at :80 echoes `'model': 'ark/seed-evolving[1m]'`. That row is the clearest instance of the failure — the Hub wrote the *request* into it. |
| `docs/design/evidence/dispatch-onboarding-1.md` | 74 | **gateway profile, different class — NOT affected by this bug.** That run used the real binary via a *gateway* provider profile. The generated gateway overlay carried the model in its `model` key, and the gateway strip ran (it was gated on delegation `gateway`/`direct`). The argv was still absent, but the overlay channel the gateway path relied on was present. No re-verification owed on the model. |

These docs are not necessarily wrong about their own subjects — onboarding
dialogs and self-host bring-up — but any statement in the two **native** rows
about *which model answered* rested on a channel that did not exist.

### 2.1 It was recorded once and not acted on

`docs/design/evidence/gateway-carryover-1.md:40`, written 2026-09-17, states it
plainly (translated): "`materialize_shell_pty_agent` **does not emit a `--model`
argv**, so for shell-pty the overlay's `model` key is the only channel for the
requested model — the host's top-level `model` must also be stripped, not just
the env."

That is this bug, in the repository, a day before the demo. The env strip it
asked for was implemented; the argv hole it named was not, and the strip was
gated on `provider_authoritative`, so it never ran for the native profile the
demo used. A finding written down but left unfixed reads, a day later, exactly
like a finding nobody had.

A matching comment survived at `crates/remuda-driver/src/launch/user.rs:234-237`
— "`model` is the only channel a shell-pty launch has (its materializer emits no
`--model` argv)" — correct, and still describing an unwired channel.

### 2.2 Unaffected

`docs/design/evidence/model-sync-1.md` is **not** affected. It measured the
installed Claude Code 2.1.272 binary directly against this host's gateway
(`model-sync-1.md:11`), not a Remuda dispatch, so no launch path of ours sat
between its request and its observation. Its `message.model` readings stand.

---

## 3. The post-launch read-back gate

Brief step 4 asked for the first observed effective model to be compared against
the pin, with a mismatch stopping the launch and ending the worker Blocked. That
is now implemented — but the comparison is **not** string equality, and the gate
judges a real read-back rather than the driver's own launch assertion. This
section records why.

### 3.1 Why equality on `message.model` is unsound

Measurements on this host's real gateway and its `cache/gateway-models.json`:

| pin (requested) | observed `message.model` | same after normalising? |
|---|---|---|
| `model_hub/es1_orange_o50[1m]` | `claude-opus-5` | no — **and this launch was correct** |
| `model_hub/es1_orange_o48[1m]` | `claude-opus-4-8` | no — this is the demo bug |
| `ark/seed-evolving[1m]` | `ark/seed-evolving` | yes |

Row 1 is a correctly pinned session (it is, in fact, the session this doc was
written in): the gateway resolves a `model_hub/…` catalog id to an upstream
vendor name, so the transcript records something that is not the pin and never
will be. Rows 1 and 2 are **indistinguishable by `message.model`** — neither
`claude-opus-5` nor `claude-opus-4-8` is in the catalog, while both `model_hub/…`
pins are. A byte/trimmed/namespace-equality gate would refuse nearly every
correct gateway launch *and still miss* the substitution.

The codebase already says so: `ModelTracker::note_stdout`
(`crates/remuda-protocol/src/model.rs`) settles an awaiting pin on the verdict
regardless of id equality — "the requested alias resolved to this concrete id" —
and assistant records "corroborate the verdict but never resolve a live switch".

### 3.2 The alias-aware comparison

`remuda_protocol::compare_model_pin(pin, observed, catalog)` (mirrored in TS by
`web/src/features/session/modelEffective.ts::compareModelPin`) returns one of:

- **Honoured** — equal, or equal apart from a `[1m]` context suffix
  (`ark/seed-evolving[1m]` ↔ `ark/seed-evolving`);
- **Mismatch** — a different id **in the pin's own vocabulary**: two namespaced
  ids that differ (`model_hub/…o50[1m]` vs `model_hub/…o48[1m]`), or two bare
  aliases that differ (`sonnet` vs `haiku`). This is decidable because both the
  pin and a `/model` verdict speak the catalog namespace;
- **Unresolvable** — the pin is namespaced and the observation is an upstream
  vendor name. Reported silently, **never** refused or flagged as a divergence,
  because a correct launch and a substituted one are identical there.

A catalog hit upgrades an un-namespaced observation (`es1_orange_o48` listed in
the discovered catalog) to comparable, hence `Mismatch`.

### 3.3 What the gate judges — and the unfalsifiable snapshot

The Node pump folds `model` observations through a `ModelPinGate`. The driver
emits two launch-sourced model observations, and confusing them is the trap the
first implementation fell into:

- the **launch snapshot** (`take_launch_model_snapshot`) is emitted *before the
  process speaks*; its `effective.id` is the requested pin itself and it carries
  no `raw` transcript spelling. It is a prediction. Judging it would settle the
  gate on our own request and make the check unfalsifiable — which is exactly
  why the first attempt (a pre-process channel assertion) could never fire once
  the argv fix populated the channel.
- the **first genuine read-back** — an assistant record's `message.model`, or a
  launch-settled `/model` verdict — carries the native spelling in `raw`. The
  gate judges only this (`source = Launch` **and** `raw.is_some()`).

It is event-driven and **fail-open**: it judges the launch read-back once; if no
genuine read-back ever arrives (no assistant message, no verdict), there is no
evidence of a substitution, so the launch continues rather than being killed on a
guess. Later human `/model` and Remuda `configure` switches (`source =
Slash`/`Remuda`) are out of scope and can never refuse a launch.

The gate arms from the recipe's **explicit pin only** (`RecipeProvider.model_pin`,
which is `spec.model_id` and nothing else), not from `model_requested`. On an
unpinned launch `model_requested` still carries the profile's default model, so
arming from it would make an unpinned launch refuseable if launch attribution
ever changed. `pin_from_recipe` is the single arming path.

### 3.3.1 The Hub half is source-scoped too

The watch pass derives the `Blocked` worker from the projected `modelEffective`,
and must apply the **same** source gate: it only treats a record with
`effective.source == "launch"` as a possible substitution. The instance
projection is source-agnostic, so an operator switching the session after launch
(a human `/model`, or a Hub `instance.configure`) overwrites `modelEffective`
while `WorkerRoster.model` keeps the dispatch pin. Without the source filter the
next watch pass would end an intentionally reconfigured worker `Blocked` naming
two ids nobody substituted, and keep it Blocked. `slash`/`remuda` observations
change neither worker state nor the effective-model column.


On a `Mismatch` the pump journals the contradicting observation, sends
`DriverRequest::Close` to stop the process, and records
`model-mismatch: requested <pin> but <observed> answered`. The Hub watch pass
then ends the worker **Blocked** with that reason naming both ids
(`WorkerState` has no `Failed` arm). The instance itself is marked failed by the
Node; the worker is Blocked by the Hub.

### 3.4 The read-back end to end

`model_pin_launch.rs` runs two cases through a real PTY. The fake harness gained
`FAKE_HARNESS_REPORT_MODEL` (`crates/remuda-testing/src/fake_harness/engine.rs`)
so it can stand in for a harness that ignores `--model`: `--model` is still
parsed and reaches argv, but the session *reports* a different id — separating
"the pin was sent" from "the pin was honoured".

- **honoured** — pinned `…o50[1m]`, harness reports `…o50[1m]`: instance Ready,
  no `model-mismatch`;
- **mismatch** — pinned `…o50[1m]`, harness reports `…o48[1m]` (the host
  default, same namespace): the launch is stopped, the instance is Failed with a
  `model-mismatch` error naming both ids, and the worker ends Blocked.

A correct gateway launch (`…o50[1m]` answered by `claude-opus-5`) is not
reported as diverged in the watch table or the web strip — only a real mismatch
is.

---

## 4. Tests

| Test | Covers |
|---|---|
| `remuda-driver/tests/materializer.rs::shell_pty_agent` | the pin on argv verbatim for claude-print / claude-pty / shell-pty; no pin ⇒ no token; no `--model` grafted onto codex/grok/agy |
| `remuda-driver/tests/model_pin_overlay.rs` | host `model` + `ANTHROPIC_MODEL` evicted under delegation `none`; endpoint/credential kept; blank pin is not a pin; case-insensitive env match |
| `remuda-protocol/src/model.rs::tests` | `compare_model_pin`: suffix-equal Honoured, same-namespace Mismatch, upstream resolution Unresolvable, catalog hit, bare alias, absent pin |
| `remuda-node/src/runtime.rs::tests::model_pin_gate` | the read-back gate: synthetic snapshot never judged, launch read-back mismatch refuses naming both ids, gateway resolution passes, later switches out of scope, no read-back fails open, no pin never refuses, **an unpinned/blank pin arms no gate despite a populated `model_requested`** |
| `remuda-node/tests/model_pin_launch.rs` | end to end through a real PTY, **honoured** and **mismatch** cases: the pinned id is received; a substituted launch is stopped Failed, both ids named |
| `remuda-hub/tests/watch.rs` | a launch-sourced mismatch ends the worker Blocked naming both ids and sets `modelEffective`; a gateway→upstream resolution is neither blocked nor recorded; **a `slash`/`remuda` switch after an honoured launch keeps the worker Working across repeated passes** |
| `remuda-hub/src/store.rs::tests` | a `model` journal event projects the observed id to `modelEffective` |
| `remuda-driver/tests/materializer.rs` | `spec.model_id` becomes `RecipeProvider.model_pin` verbatim; an unpinned launch has `model_pin = None` even when `model_requested` is populated |
| `web/.../EffortSlider.test.tsx` | the slider's requested-vs-actual note uses the alias rule: gateway resolution and suffix variants not flagged, real same-namespace mismatch flagged, pending not flagged |
| `remuda/src/cmd/watch.rs::tests` | the MODEL cell: observed-first `observed ⇐ requested` on divergence, `-` when unknown, truncation keeps the observed id |
| `web/src/features/session/SessionList.test.tsx` | the row label: requested until read back; real substitution marked diverged; gateway resolution not marked |
| `remuda-node/tests/model_pin_evidence.rs` | `#[ignore]` capture tool that produced §1.2–1.3 |

