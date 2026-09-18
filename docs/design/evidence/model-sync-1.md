# Model sync — in-session `/model` switching and the real model list (model-sync-1)

Sibling of [`effort-sync-2.md`](./effort-sync-2.md). The structured composer's
model popover previously (a) did nothing on the PTY carrier —
`claude-pty` returned `CapabilityUnsupported("claude-pty has no runtime model
command")` — and (b) showed a static builtin guess (`opus`/`sonnet`/`haiku`/
`auto`) even on a gateway-configured claude whose real `/model` picker lists
the gateway's discovered models. This is the measured contract that closes
both gaps.

Measured on the installed **Claude Code 2.1.272** binary against this host's
gateway relay (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`,
`ANTHROPIC_MODEL=model_hub/es1_orange_o48[1m]`), with a throwaway scoped
`CLAUDE_CONFIG_DIR`; earlier 2.1.221 verdicts were re-measured from
`~/.claude/projects/…` transcripts. Live probe:
`crates/remuda-driver/examples/model_probe.rs`
(`PROBE_DIR=/tmp/remuda-c-modelsync-probe1 cargo run -p remuda-driver
--example model_probe`). Verbatim fixtures:
`crates/remuda-driver/tests/fixtures/model-21272/`.

## 1. How it is reproduced

The scoped config dir is onboarding-pre-seeded (same gates the driver seeds)
plus the workspace trust dialog pre-accepted
(`projects[cwd].hasTrustDialogAccepted=true`), then claude boots with the
gateway env mirrored into its `settings.json`. The probe types, all while idle:

| phase | typed | expected |
| --- | --- | --- |
| base | `reply …: ok` | baseline assistant `message.model` |
| A | `/model sonnet` ⏎ | alias resolves to the pinned concrete id |
| B | `/model model_hub/es1_orange_o50` ⏎ | a gateway id, this one not pinned |
| C | `/model bogus-xyz-123` ⏎ | unknown id error |
| D | `/model haiku` then **Esc** | dismissed dialog/picker |
| E | bare `/model` | the picker (real list) |

## 2. What the transcript actually writes (the read-back contract)

### 2.1 A valid id applies with NO confirmation dialog on 2.1.272

Unlike `/effort` (whose cached-conversation switch shows a
`Change effort level?` dialog gated on the screen), `/model <id>` on 2.1.272
applies immediately after the submit CR. The slash record and its verdict
share **one millisecond timestamp** (`delta = 0 ms` in every capture: sonnet,
o50, bogus, kept):

```jsonc
// type:"user" record
{"message":{"content":"<command-name>/model</command-name>\n  …\n  <command-args>model_hub/es1_orange_o50</command-args>"}}
// same timestamp, type:"user" record
{"message":{"content":"<local-command-stdout>Set model to `model_hub/es1_orange_o50` and saved as your default for new sessions\x1b[2m\x1b[22m\n\x1b[2m     ANTHROPIC_MODEL is set to \x1b[22m`model_hub/es1_orange_o48[1m]`\x1b[22m — new sessions use that while it is set\x1b[22m</local-command-stdout>"}}
```

The driver keeps the screen gate anyway (`Switch model?` / `Yes, switch to …`
strings are present in the CLI bundle and 2.1.221 confirmed the switch
through such a dialog), so a cached-conversation confirmation on an older or
future build still gets its confirming CR; the **verdict**, not the dialog, is
the acceptance authority — identical discipline to effort-sync-2.

### 2.2 Verdict vocabulary

| outcome | exact `<local-command-stdout>` text | record type |
| --- | --- | --- |
| accept, 2.1.272 | `` Set model to `<resolved>` and saved as your default for new sessions `` (plus an optional dim second line: `ANTHROPIC_MODEL is set to \`<id>\` — new sessions use that while it is set`) | `user` |
| accept, 2.1.221 | `Set model to \x1b[1m<name>\x1b[22m and saved as your default for new sessions` | `user` |
| unknown id | `Model '<id>' not found` | **`system`** (`subtype:"local_command"`, top-level `content`) |
| picker/dialog dismissed | `` Kept model as `<id>` `` | **`system`** |

Two parse hazards the mapper handles:

1. The resolved id is bold/dim SGR-wrapped and, on 2.1.272, backticked. Only
   the **first line** is parsed — the dim `ANTHROPIC_MODEL` hint on line 2
   itself carries a backticked id and must not be mistaken for the resolved id.
2. Rejects/dismissals are `system` records, not `user` records (effort's
   reject path was a `user` record on 2.1.272). The mapper now handles
   `type:"system" subtype:"local_command"` records carrying the same
   `<command-name>/model` / `<local-command-stdout>` markup at top-level
   `content`.

### 2.3 An alias resolves to a concrete id — the verdict carries the truth

`/model sonnet` in the pinned env did **not** switch to anything named
"sonnet"; the verdict read
`` Set model to `model_hub/es1_orange_o48[1m]` `` — the alias resolved to the
id the `ANTHROPIC_DEFAULT_SONNET_MODEL`/pinned env points at. The picker must
mark the **resolved** id current; the typed word is only `requested`. The
follow-up assistant record corroborates this at `message.model`:

- baseline assistant record: `message.model = "claude-opus-4-8"` (no `--model`
  pin; the top-level record has no `model` field — it lives at
  `message.model`, unlike effort which is a top-level `effort` field);
- gateway sessions record the gateway id there (`ark/seed-evolving` on the
  live relay).

`/model model_hub/es1_orange_o50` (an id the env does not pin) read back
verbatim as `model_hub/es1_orange_o50`, and the next prompt's
`message.model` is that id.

### 2.4 End-to-end timing

| path | measured |
| --- | --- |
| scoped-config composer boot | 504 ms |
| baseline prompt → first `message.model` record | 3.0 s after submit (gateway round-trip) |
| submit CR → `/model` verdict on screen | ~1.5 s (transcript flush + 75 ms poll; the slash/verdict records themselves share a timestamp) |
| verdict bytes mapped → bridge settle (node integration) | **0 ms**, fold budget 50 ms (`remuda-node/tests/model_sync.rs`) |

The model read-back window is bounded at 10 s (same as effort); a switch whose
verdict never lands degrades with `model-degraded:<id>:no-readback-within-window`.

## 3. The real model list (discovery)

On a gateway-configured host Claude Code writes
`~/.claude/cache/gateway-models.json` (and the equivalent path under a scoped
`CLAUDE_CONFIG_DIR`):

```json
{ "baseUrl": "https://…", "fetchedAt": 1789235514200,
  "models": [ {"id":"ark/60b-0614c","display_name":"60b-0614c","description":""}, … ] }
```

22 discovered ids on this host, including `ark/*`, `auto_model/*`,
`model_hub/es1_orange_o47|o48|o50` and their `[1m]` long-context spellings.
(The scoped probe config fetched the changelog into `cache/` but the models
cache lands on the relay's own refresh cadence, so discovery falls back to the
operator's conventional `~/.claude` cache for the scoped dir — the same relay
serves both.)

### 3.1 Scoped vs host fallback, re-measured 2026-09-18 (c-modelpick)

The promotion-time fallback above was the root of the demo-day failure where
the picker showed gateway ids the session's own terminal `/model` did not
list. Measured on the live shell-pty instance:

- the instance's own cache under its scoped native home
  (`$nativeHome/cache/gateway-models.json`) is written **a beat after the
  promotion-time resolve**; when it lands it holds **11 ids** —
  `claude-fable-5-1`, `claude-opus-5`, `claude-sonnet-5`, `claude-fable-5`
  and the dated 4-x ids — with **no** `[1m]` spellings and no `claude-grok-*`;
- the operator's host fallback cache holds **75 ids** and ends with
  `grok-4.6`, `claude-grok-4.6`, `claude-fable-5[1m]`,
  `claude-fable-5-1[1m]`;
- `ANTHROPIC_BASE_URL` and
  `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` are present and identical in
  both environments — the same base URL answers two catalogs, one scoped to
  the session credentials and one to the operator's. So the env being correct
  does not make the host-fallback list the session's own.

The catalog therefore records which cache answered
(`payload.catalog.cache.scope = "scoped-config-dir" | "host-fallback"`, plus
that file's `baseUrl`/`fetchedAt`) and whether the discovery gate env was
present (`discoveryEnv`). Both drivers (`shell_pty` promotion and
`claude_pty`) re-resolve for up to 60 s after launch; when the scoped cache
lands they emit a catalog-only model observation that replaces the
frozen-at-promotion answer. The picker shows a one-line warning on a
host-fallback list, a missing discovery env, or discovery never answering, so
a list the terminal would reject is never presented as the session's own.

An id the session's own discovery did not list is still switchable: the
picker's verbatim input types `/model <id>` and the verdict decides, exactly
as the configure path already did. The verdict observation carries
`selectionPath: "listed" | "typed"` so the recorded path is visible on the
current-model row.

Resolution precedence (`remuda-driver/src/model_discovery.rs`,
`parse_gateway_models_json` is protocol-shared so node tests share it):

1. `gateway-discovery` — `<config dir>/cache/gateway-models.json`, then the
   host's conventional `~/.claude/cache/…`; non-empty ids only.
2. `settings` — `settings.json` `model`, `modelSettings` keys, and
   `env.ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`, plus
   the child process env the driver actually launched with.
3. `builtin` — `opus`/`sonnet`/`haiku`/`auto`, used only when nothing is known.

The list rides a launch-time `model` observation as
`payload.catalog: {models, source, observedAt}` (Hub projects it onto the
instance spec as `modelCatalog`); later model edges omit it. The web picker
uses the discovered ids as its list and appends builtin aliases **only** in
the `builtin`/unknown case.

## 4. The fix (end to end, mirroring effort-sync-2)

- **protocol** — `model.rs`: `ModelTracker`, `parse_model_stdout`,
  `slash_model_args`, `ObservedModel`, `EffectiveModel`, `ModelCatalogInfo` /
  `ModelListSource`; a new `ObservationPayload::Model` (`kind:"model"`).
- **driver** — `model.rs` (`ModelBridge`/`ModelQueue`/`perform_model_switch`,
  body + CR, optional 2.1.221 dialog gate, 10 s bounded read-back,
  `model-applied`/`model-queued`/`model-degraded:<id>:<reason>` lifecycles)
  and `model_discovery.rs`; wired into both `claude-pty` and `shell-pty`
  (including the promoted-terminal hydrator). `claude-print` stays
  unsupported for the interactive switch (its `set_model` control-request
  path is unchanged). The transcript mapper settles the bridge from the
  verdict, handles both `user` and `system` verdict records, and emits the
  launch snapshot carrying the catalog.
- **node** — a model configure no longer gets a synthetic "applied" lifecycle;
  the driver reports its own verdict lifecycle, exactly like effort.
- **hub** — `kind:"model"` observations project `modelEffective` and
  `modelCatalog` onto the instance spec; the fake node
  (`examples/hub_e2e.rs`) gained a `/model` verdict echo and
  `__queued__:`/`__notfound__:`/`__resolve__:` sentinels.
- **web** — store gains `modelEffective`/`modelCatalogs`/`modelPending`,
  pending/queued/degrade reducers (revert + Chinese toast on reject), a
  no-configure terminal fold, and `modelListOf`; the picker renders the
  discovered list and marks the **resolved** id current (with a
  请求/实际 mismatch line when an alias resolves elsewhere).

## 5. Tests and timings

- Driver unit tests on recorded fixtures/recorded stdout strings + mocked PTY
  I/O; `remuda-driver/tests/model_transcript.rs` replays the verbatim
  2.1.272 walk (accept) and reject (bogus `system` + dismissed `system`)
  through the real mapper, including bridge settle/reject.
- `remuda-node/tests/model_sync.rs`: both directions, reject reason, and the
  ≤50 ms in-process fold budget.
- Discovery resolution unit tests (gateway cache wins → settings/env →
  builtin; scoped→host cache fallback; env-hint id isolation is parser-tested).
- Web: `store.model.test.ts` (catalog snapshot, terminal fold with no
  configure, pending settle on the resolved id, not-found revert+toast) and
  `tests/e2e/ux-modelsync.hub.spec.ts` through the fake harness (picker shows
  the gateway list; select posts configure; alias mismatch; terminal fold;
  not-found; queued).

### Before / after

| | before | after |
| --- | --- | --- |
| idle model switch → chip/picker | PTY: `CapabilityUnsupported`, nothing happened | `/model <id>` ⏎, verdict proves it; ~1.5 s end to end |
| model list | static `opus/sonnet/haiku/auto` guess | gateway discovery cache → settings/env → builtin, with provenance |
| alias pick (`sonnet`) | selected the literal word | marked the resolved concrete id; mismatch shown on divergence |
| terminal `/model` | never synced the structured picker | slash-attributed edge moves the picker, no configure ping-pong |
| bogus id | — | `model-degraded:<id>:not-found`, picker reverts with a toast |
| switching while working | — | queued at the next idle, 排队中 tag |

## Files touched (evidence)

- Probe: `crates/remuda-driver/examples/model_probe.rs`
- Capture: `/tmp/remuda-c-modelsync-probe1/{transcript.jsonl,timing.json}`
  (scratch; verbatim records committed under
  `crates/remuda-driver/tests/fixtures/model-21272/`).
- Host discovery cache: `~/.claude/cache/gateway-models.json` (host-local,
  not committed).
