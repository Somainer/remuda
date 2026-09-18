# A `--model` pin must reach the process, or the launch must refuse

A dispatch that names a model must put that model in front of the process, on
every claude launch path. If it cannot, the launch must fail loudly rather than
run on something else while every record claims the pin was honoured.

Worker `c-modelpin`, 2026-09-18. Scratch dev server only: a `remuda dev` Node
with the fake harness from `crates/remuda-testing`, on an isolated temp root. No
host, user or path names appear below; every model id is either a synthetic
stand-in or a host-generic gateway id.

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

### 1.4 The negative control

The same end-to-end test was re-run with both channels deliberately neutered
(preset `model_flag` set to `None`, the overlay pin skipped) to confirm it can
actually fail:

```
panicked at crates/remuda-node/tests/model_pin_launch.rs:199:
  no harness transcript with a model under <scratch>/claude-home
test result: FAILED. 0 passed; 1 failed
```

With no `--model` argv and no overlay pin the harness never produced a
transcript naming the pinned id at all, so the wait for one timed out — the
assertion never got as far as comparing ids. (The fake harness's own default is
`claude-opus-5`, which is also why the id assertion alone would not have been
enough; the timeout is the honest failure here.)

With the fix restored the same test passes in 66 s. A test that cannot fail
proves nothing, so this run is part of the evidence.

---

## 2. Audit: 2026-09-17/18 evidence that reported a pin which was not effective

Every dispatch below ran on driver `shell-pty` with a native profile, which is
exactly the path that emitted no `--model` argv and left the host's `model` key
in place. The *pin* each doc records was therefore a request, not an observation,
and the conclusion each doc drew about the model in effect **must be
re-verified**:

| Document | Lines | What it recorded |
|---|---|---|
| `docs/design/evidence/dispatch-onboarding-1.md` | 8–9, 74 | dispatch `--driver shell-pty --model ark/seed-evolving[1m]` (native profile, delegation none), and a gateway profile pin at :74 |
| `docs/design/evidence/self-host-1.md` | 28, 63, 78, 80 | profile `defaultModel: ark/seed-evolving[1m]`; two dispatches `--model ark/seed-evolving[1m]`; a roster row echoing `'model': 'ark/seed-evolving[1m]'` |

Note the roster row at `self-host-1.md:80` is the clearest instance of the
failure mode: the row named the pin because the Hub wrote the *request* into it,
which is the reporting hole §5 of this brief closes.

These are not necessarily wrong about their own subjects — onboarding dialogs and
self-host bring-up — but any statement in them about *which model answered* rests
on a channel that did not exist.

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

## 3. Why the refusal is on the channel, not on `message.model`

Brief step 4 asked for the first observed effective model (the transcript's first
assistant `message.model`) to be compared against the pin, with a mismatch
stopping the launch. **Measured, that comparison is unsound**, so the refusal was
built on the launch channel instead. The measurements, taken on this host's real
gateway and its `cache/gateway-models.json`:

| pin (requested) | observed `message.model` | same after normalising? |
|---|---|---|
| `model_hub/es1_orange_o50[1m]` | `claude-opus-5` | no — **and this launch was correct** |
| `model_hub/es1_orange_o48[1m]` | `claude-opus-4-8` | no — this is the demo bug |
| `ark/seed-evolving[1m]` | `ark/seed-evolving` | yes |

Row 1 is a correctly pinned session: the gateway resolves a `model_hub/…` catalog
id to an upstream name, so the transcript records something that is not the pin
and never will be. An equality gate — byte, trimmed, or namespace-normalised —
would refuse that launch.

Worse, rows 1 and 2 are **indistinguishable by that channel**. Catalog membership
cannot separate them either: neither `claude-opus-5` nor `claude-opus-4-8` is in
`gateway-models.json`, while both `model_hub/es1_orange_o50[1m]` and
`model_hub/es1_orange_o48[1m]` are. So the proposed gate would refuse nearly
every correct gateway launch *and still* miss the substitution it was written to
catch.

The codebase already encodes this. `ModelTracker::note_stdout`
(`crates/remuda-protocol/src/model.rs:204-209`) settles an awaiting pin on the
verdict regardless of id equality, commented "the requested alias resolved to
this concrete id", and `:239-241` notes that assistant records "corroborate the
verdict but never resolve a live switch".

### 3.1 What is decidable

Whether the pin reached the process at all — which is precisely the 2026-09-18
failure. A pin with no `--model` argv and no settings `model` key cannot be in
effect, whatever any later transcript says. That check is local, deterministic,
needs no network or catalog, and cannot false-positive on a gateway resolution.

`model_pin_channel_error` (`crates/remuda-node/src/native.rs`) reports through
`startup_error`, so the runtime's existing path tears the process down and fails
the instance rather than leaving it running on the wrong model. A launch with no
pin cannot mismatch and is never refused.

The two ids are still both *reported* (§5 of the brief): `WorkerRoster.model` is
the request, `WorkerRoster.model_effective` the observation when they diverge,
and `remuda watch` renders `requested>observed`. A divergence is a report, not by
itself a fault — which is the honest position given the table above.

---

## 4. Tests

| Test | Covers |
|---|---|
| `remuda-driver/tests/materializer.rs::shell_pty_agent` | the pin on argv verbatim for claude-print / claude-pty / shell-pty; no pin ⇒ no token; no `--model` grafted onto codex/grok/agy |
| `remuda-driver/tests/model_pin_overlay.rs` | host `model` + `ANTHROPIC_MODEL` evicted under delegation `none`; endpoint/credential kept; blank pin is not a pin; case-insensitive env match |
| `remuda-node/src/native.rs::tests::model_pin` | the channel refusal: no channel refuses naming the pin; argv or overlay accepted; a suffix-stripped value is not the pin; a host-model overlay still refuses; no pin never refuses |
| `remuda-node/tests/model_pin_launch.rs` | end to end through a real PTY: the pinned id is what the harness received, and the host's default did not answer |
| `remuda/src/cmd/watch.rs::tests` | the MODEL cell: effective id, `requested>observed` on divergence, `-` when unknown |
| `web/src/features/session/SessionList.test.tsx` | the row label: requested until read back, observed after, divergence marked |
| `remuda-node/tests/model_pin_evidence.rs` | `#[ignore]` capture tool that produced §1.2–1.3 |
