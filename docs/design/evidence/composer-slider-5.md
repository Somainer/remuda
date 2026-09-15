# Composer effort slider · pass 6 — the three-level look and the real Codex/Grok vocabularies

> **Correction (2026-09-15):** The Codex picker conclusion and `ultra → xhigh` migration below were wrong. The owner transcribed codex-cli 0.154.0 on this Mac: Low, Medium, High, Extra high, Max, Ultra. Current implementation and local acceptance evidence: [effort-codex-tiers-1](./effort-codex-tiers-1.md). Earlier binary observations and screenshots are historical.

Sixth pass, from two owner-reported defects after the six-stop slider
([composer-slider-4](./composer-slider-4.md), [composer-slider-5](./composer-slider-5.md)):

> 1. 「max 和 ultracode 的特效要区分，只有 ultracode 才有最强的特效」
> 2. 「codex 的档位是不全的」— the table was `low/medium/high/ultra`, and
>    `ultra` would have been passed straight into
>    `-c model_reasoning_effort="ultra"`.

Mock UI (`VITE_MOCK=1`, Playwright chromium / iPhone 13 against the local vite
port) plus the installed Codex CLI on this host and upstream Grok source.
Night Corral (`night`) and Ledger (`ledger`). Shots are composer/effort-element
crops only — no personal paths, no hostnames. Static frames
(`animations: "disabled"` / `prefers-reduced-motion: reduce`), so drift and
twinkle are captured as a still ember field and the reduced-motion rules are
what the evidence shows.

## 1 · The three-level visual ladder

| look | stops | treatment |
| --- | --- | --- |
| `plain` | claude `low/medium/high`; every non-top stop of the other tables; agy | the cold brand fill, muted tick, plain thumb |
| `top` | claude `xhigh` **and** `max`; codex `max` and grok `xhigh` | restrained static accent: cold→dust gradient, dust tick/title, one thin dust thumb ring. No glow, no spark field, no animation |
| `ultracode` | the Claude `ultracode` stop and Codex `ultra` tier | full amber ember: four drifting spark layers + breathing wash + brighter halo, its own `--ember-ultra` label colour and its own thumb state |

The distinction is asserted without screenshots through
`data-effort-look="plain|top|ultracode"` on the slider root, the panel, the
title and every list row (e2e:
`the fill reaches the knob at every stop; the ember field exists on ultracode alone`,
`codex … vocabulary 1:1`, `grok session lists the native grok effort table`).
`data-ember="1"` is now strictly equivalent to `data-effort-look="ultracode"`.

### Composer popover card

| | night | ledger |
|---|---|---|
| plain (`high`) | [plain-night-1440](./composer-slider-5-plain-night-1440.png) | [plain-ledger-1440](./composer-slider-5-plain-ledger-1440.png) |
| top (`max`, restrained accent) | [top-night-1440](./composer-slider-5-top-night-1440.png) | [top-ledger-1440](./composer-slider-5-top-ledger-1440.png) |
| ultracode (full ember) | [ultra-night-1440](./composer-slider-5-ultra-night-1440.png) | [ultra-ledger-1440](./composer-slider-5-ultra-ledger-1440.png) |

The same three frames at **768**:
[plain-night-768](./composer-slider-5-plain-night-768.png) ·
[top-night-768](./composer-slider-5-top-night-768.png) ·
[ultra-night-768](./composer-slider-5-ultra-night-768.png) ·
[plain-ledger-768](./composer-slider-5-plain-ledger-768.png) ·
[top-ledger-768](./composer-slider-5-top-ledger-768.png) ·
[ultra-ledger-768](./composer-slider-5-ultra-ledger-768.png)

and at **390** (short tick labels; the hit area keeps ≥44px):
[plain-night-390](./composer-slider-5-plain-night-390.png) ·
[top-night-390](./composer-slider-5-top-night-390.png) ·
[ultra-night-390](./composer-slider-5-ultra-night-390.png) ·
[plain-ledger-390](./composer-slider-5-plain-ledger-390.png) ·
[top-ledger-390](./composer-slider-5-top-ledger-390.png) ·
[ultra-ledger-390](./composer-slider-5-ultra-ledger-390.png)

### Inline New Session field (layout A)

| | night | ledger |
|---|---|---|
| plain | [new-plain-night-1440](./composer-slider-5-new-plain-night-1440.png) | [new-plain-ledger-1440](./composer-slider-5-new-plain-ledger-1440.png) |
| top (`max`) | [new-top-night-1440](./composer-slider-5-new-top-night-1440.png) | [new-top-ledger-1440](./composer-slider-5-new-top-ledger-1440.png) |
| ultracode | [new-ultra-night-1440](./composer-slider-5-new-ultra-night-1440.png) | [new-ultra-ledger-1440](./composer-slider-5-new-ultra-ledger-1440.png) |

768 and 390 variants sit alongside with the same `-768`/`-390` suffixes.

Accessibility and geometry, unchanged and re-checked:
`prefers-reduced-motion` keeps the **static strongest state** (all ember
animations `none`, field and thumb ring still painted — e2e
`reduced motion freezes the ultracode ember but keeps the strongest static state`),
the pill stays 40px tall with a 36px knob, and the touch hit area stays
≥44px through padding, never the visual size
(`touch targets stay >= 44px while the pill stays 40px` on iPhone 13).

## 2 · Codex vocabulary — what the binary actually accepts

Installed binary: **codex-cli 0.147.0** (`codex --version`, npm
`@openai/codex` under `~/.nvm/.../@openai/codex`). Evidence gathered on this
host, all in throwaway `/tmp/remuda-r-effort2/` homes:

1. **There is no `--effort` flag.** Both `codex --effort xhigh --version` and
   `codex exec --effort xhigh` fail at clap parse with
   `error: unexpected argument '--effort' found`. The old materializer emitted
   `--effort <name>` for every agent kind, which is a startup crash for codex;
   it now emits the `-c` overlay instead.
2. **`-c model_reasoning_effort=…` is the channel** — the exact argv shape
   `remuda-codex-wire` already builds: `-c model_reasoning_effort="low"`.
3. **The parsed enum** (`codex-rs/protocol/src/openai_models.rs`, tag
   `rust-v0.147.0`, `ReasoningEffort::from_str`):
   `none · minimal · low · medium · high · xhigh · max · ultra · Custom(…)`,
   with `#[default] Medium`. The app-server protocol models the value as a
   free-form string (`v2.ReasoningEffort` is "a non-empty reasoning effort
   value advertised by the model") — `thread/start` admitted even `bogus`, so
   the closed set has to be enforced by us, which it now is in both the web
   layer and `remuda-codex-wire` (`InvalidReasoningEffort`).
4. **Per-model advertising** from the catalog embedded in the pinned binary
   (its `supported_reasoning_levels` JSON):

   | model | default | advertised levels |
   | --- | --- | --- |
   | gpt-5.6-sol | low | low medium high xhigh max ultra |
   | gpt-5.6-terra | medium | low medium high xhigh max ultra |
   | gpt-5.6-luna | medium | low medium high xhigh max |
   | gpt-5.5 / 5.4 / 5.4-mini / 5.2 | medium | low medium high xhigh |

   The TUI source agrees (`reasoning_shortcuts.rs`): *"Raising never silently
   crosses into Max or Ultra; those efforts require the explicit
   advanced-reasoning picker."*

The earlier conclusion to offer `minimal/low/medium/high/xhigh` was wrong.
The corrected picker is **Low · Medium · High · Extra high · Max · Ultra**,
with wire values `low/medium/high/xhigh/max/ultra` and default `medium`.
`minimal` is accepted only as a compatibility alias for `low`; `max` and
`ultra` pass unchanged. Unknown stored words fall back to `medium`.
See the [current six-tier table and evidence](./effort-codex-tiers-1.md).

## 3 · Grok vocabulary — quick/standard/max was invented

The earlier `quick/standard/max` table dates from slider pass 1 and was never
verified against a grok binary; the grok 1.0.30 binary from the
[grok-signals spike](./grok-signals-1.md) is no longer installed on this host.
The CLI is **xai-org/grok-build** (a Claude Code fork), so its public source
is the vocabulary authority:

- the pager CLI struct (`crates/codegen/…/src/app/cli.rs`):
  `--reasoning-effort <EFFORT>` with `visible_alias = "effort"`.
- the sampling-types crate's `ReasoningEffort::from_str` parses
  `none · minimal · low · medium · high(default) · xhigh · max` and rejects
  anything else (`invalid reasoning effort …`).
- the pager slash commands (`…/slash/commands/effort_levels.rs`) and the
  user guide (`/effort`): the built-in menu is exactly
  **xhigh · high · medium · low**; *"`none`/`minimal` are still accepted by
  `ReasoningEffort::from_str` for power users."*

The grok slider is therefore **low · medium · high · xhigh** (menu order,
default `medium`), the driver emits the canonical
`--reasoning-effort <word>`, and the driver rejects `quick/standard/max/ultra`
in hand-written `spec.args`. The legacy stored words migrate by name:
`quick → low`, `standard → medium`, `max → xhigh`; unknown → `medium`.

## 4 · Wire mapping, 1:1, with typed rejection

- `remuda-protocol`: `EffortName` carries legacy `Minimal` and real `Ultra`;
  the harness-aware `normalize_legacy_effort` preserves Codex `ultra` and maps
  legacy Codex `minimal` to `low` (round-trip tests in `remuda-protocol/tests/wire.rs`).
- `remuda-driver/src/effort.rs` (new): one mapper per kind —
  claude `--effort <low..max|ultracode>`, agy `--effort <low..max>`, codex
  `-c model_reasoning_effort="<low|medium|high|xhigh|max|ultra>"`, grok
  `--reasoning-effort <low|medium|high|xhigh>` — returning
  `DriverError::InvalidLaunchSpec` on out-of-vocabulary values;
  `materialize_shell_pty_agent` calls it per kind, and
  `ensure_no_effort_in_extras` stops `spec.args` from smuggling a duplicate
  (codex's flag allowlist no longer contains `effort` at all; grok's adds the
  real `reasoning-effort`). Unit tests: `remuda-driver/src/effort.rs`,
  `flags.rs`, `tests/materializer.rs`.
- `remuda-codex-wire`: `SpawnSpec.argv()` now rejects a
  `reasoning_effort` outside `REASONING_EFFORTS` with the typed
  `WireError::InvalidReasoningEffort` before spawn.
- Web: `effortWireName` only emits current native words (plus the claude
  `ultracode` sentinel), the Hub openapi enum includes `ultra`, and both
  generated TS files were regenerated (`just gen-types`, `pnpm gen:api`).

## Tests

- `pnpm --dir web test`: 76 files / 535 tests green, incl. the new
  `EffortSlider.test.tsx` look-ladder suite and rewritten `effort.test.ts`
  (tables, defaults, migration, closed wire vocabulary, looks, round-trips).
- Playwright mock specs: codex enumeration 1:1 from the New Session sheet,
  grok table + top-row-not-ember, the full plain/top/ultracode walk, reduced
  motion. Three host-UI flakes (`grok pty … readonly`, terminal shell-pty
  view, mobile install-bar intercept) reproduce identically on the unmodified
  tree and are unrelated to the slider.
- Cargo: `remuda-protocol`, `remuda-driver`, `remuda-codex-wire` test suites
  green.
