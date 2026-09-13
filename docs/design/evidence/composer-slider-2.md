# Composer effort slider · Codex-style card

Second pass. The first slider (`composer-slider-1.md`) was a thin full-width track inside a tall popover that also listed models — this replaces it with a compact card whose pill *is* the control, matching the Codex effort slider adapted to Night Corral.

Mock UI (`VITE_MOCK=1`, Playwright chromium against a local vite port). Night Corral (`night`) and Ledger (`ledger`). No live Hub ports, no personal paths, no hostnames — shots are the composer element only.

Spec: `docs/design/ui-spec.md` Composer. Implementation: `web/src/features/session/EffortSlider.tsx`, native tables in `web/src/features/session/effort.ts`.

## What changed against slider 1

| | slider 1 | slider 2 |
|---|---|---|
| trigger | `opus think ▾` (model + tier) | `think ▾` — tier only, fixed min-width so the bar never reflows |
| popover | full-width panel, model list + slider + footer | ~400px card, pill only |
| track | 8px hairline, gradient across the whole width | 44px rounded pill, solid brand fill left / neutral right |
| stops | none visible | a dot marker per tier, visible on both sides of the knob |
| knob | 18px amber dot | 40px white circle with a soft shadow |
| top tier | animated gradient + pulsing halo | ember amber gradient + still ember star texture, one 5s shimmer |
| models | listed above the slider, always | behind the `›` chevron, with the tier descriptions |

## Layout

Row 1 is a 3-column grid — lightning (left) · tier name in brand colour + `›` (centre) · reset (right) — so the centred tier name stays centred regardless of icon widths. Row 2 is the muted model name. Then the pill.

The knob centre travels between `22px` and `width - 22px` (half a knob in from each edge), so the knob never overflows the pill at either end; `effortIndexFromClientX` is fed that inset range, which keeps pointer aim and knob position on the same scale.

The pill's paint sits in an inner `overflow: hidden` layer; the knob is a sibling outside it, so its drop shadow is not clipped.

Ember texture is nine `radial-gradient` dots at varying radius and opacity — no sprites, no per-dot animation. The only motion is a 5s opacity shimmer on that one layer, dropped under `prefers-reduced-motion: reduce`.

## Screenshots

Composer-only crops (PNG ≤ 300 KB). Demo fixture session, no personal paths. `mid` = `think-hard` (a non-top tier), `top` = `ultracode` (ember).

| | night | ledger |
|---|---|---|
| mid · 1440 | [composer-slider-2-mid-night-1440.png](./composer-slider-2-mid-night-1440.png) | [composer-slider-2-mid-ledger-1440.png](./composer-slider-2-mid-ledger-1440.png) |
| mid · 390 | [composer-slider-2-mid-night-390.png](./composer-slider-2-mid-night-390.png) | [composer-slider-2-mid-ledger-390.png](./composer-slider-2-mid-ledger-390.png) |
| top · 1440 | [composer-slider-2-top-night-1440.png](./composer-slider-2-top-night-1440.png) | [composer-slider-2-top-ledger-1440.png](./composer-slider-2-top-ledger-1440.png) |
| top · 390 | [composer-slider-2-top-night-390.png](./composer-slider-2-top-night-390.png) | [composer-slider-2-top-ledger-390.png](./composer-slider-2-top-ledger-390.png) |

## Behaviour kept from slider 1

- Snaps to harness-native tiers: claude `default/think/think-hard/ultracode`, codex `low/medium/high/ultra`, grok `quick/standard/max`. No generic fast/standard/deep.
- Keyboard ←/→/↑/↓/Home/End, pointer and touch drag, 44px hit area. Reset returns to the harness default (`think` / `medium` / `standard`).
- Change goes through `instance.configure` (journal + persist) — same store path, unchanged.
- Locked when the table is empty or the session is not configurable (`exited` / `observed-only` / no `onEffort`).
- ~400px: the card is `min(400px, 100%)` and the pill stays inside the composer.

## data-testid

| id | where |
|---|---|
| `model-effort-chip` | collapsed trigger, tier name only |
| `effort-menu` | card popover |
| `effort-slider-panel` | card body; `data-view` = `slider` \| `list` |
| `effort-slider` | `role=slider`; `data-tiers` `data-name` `data-index` `data-ember`; `aria-valuetext` = tier name |
| `effort-track` | the pill |
| `effort-knob` | 40px white thumb |
| `effort-title` | tier name (`data-ember`) |
| `effort-model` | model line |
| `effort-open-list` | tier name + `›`, opens the list view |
| `effort-list` / `effort-list-back` | list view body / back to the pill |
| `effort-tier-<name>` | a tier row in the list view (`data-selected`, `data-ember`) |
| `model-option-<short>` | a model row in the list view |
| `effort-reset` | restore default |

`effort-slider` and `effort-knob` are unchanged from slider 1, so the live-hub e2e keeps working.

## Tests

- `pnpm --dir web test` — `effort.test.ts` snap/keyboard, `Composer.test.tsx` pointer/keys/list view/ember/disabled (213 passed)
- Playwright mock `composer-effort.spec.ts` — pill geometry (≥40px tall, ≥34px knob, knob inside the pill), card ≤ 400px, list round-trip, drag + keyboard, evidence shots. 21 passed on `chromium` + `mobile-webkit`, 1 skipped (evidence shots are chromium-only).
- Live hub `hub-live.spec.ts` — "effort slider drag and keyboard send instance.configure", unchanged
