# Composer effort slider

Mock UI (`VITE_MOCK=1`, Playwright chromium against `127.0.0.1:4177`). Night Corral (`night`) and Ledger (`ledger`). No live Hub ports, no personal paths, no hostnames — shots are the composer element only.

Spec: `docs/design/ui-spec.md` Composer. Implementation: `web/src/features/session/EffortSlider.tsx`, native tables in `web/src/features/session/effort.ts`.

## What landed

- Collapsed trigger stays the compact `模型+档位 ▾` chip (`model-effort-chip`, `aria-label` starts with `Select effort`).
- Expanded popover is a slider, not tier chips: lightning + large native tier name (ember amber on the top tier) + model + reset; a wide muted-brand gradient track (`--cold` → `--mute` → `--dust`, no purple) with a round knob; one-line `档名 · 说明`.
- Knob snaps to harness-native stops: claude `default/think/think-hard/ultracode`, codex `low/medium/high/ultra`, grok `quick/standard/max`.
- Keyboard ←/→/Home/End and pointer drag (44px hit). Reset returns to the harness default (`think` / `medium` / `standard`).
- Change goes through `instance.configure` (same store path as before). Locked when the table is empty or the session is not configurable (`exited` / `observed-only` / no `onEffort`).
- ~400px: popover is `width: 100%` and the track stays inside the composer.

## Screenshots

Composer-only crops (PNG ≤ 300 KB). Demo fixture session, no personal paths.

| | night | ledger |
|---|---|---|
| 1440 | [composer-slider-1-night-1440.png](./composer-slider-1-night-1440.png) | [composer-slider-1-ledger-1440.png](./composer-slider-1-ledger-1440.png) |
| 390 | [composer-slider-1-night-390.png](./composer-slider-1-night-390.png) | [composer-slider-1-ledger-390.png](./composer-slider-1-ledger-390.png) |

## data-testid

| id | where |
|---|---|
| `model-effort-chip` | collapsed trigger |
| `effort-menu` | popover |
| `effort-slider` | `role=slider`; `data-tiers` `data-name` `data-index` `data-ember` |
| `effort-knob` | round thumb |
| `effort-title` | large tier name |
| `effort-hint` | one-line hint |
| `effort-reset` | restore default |

## Tests

- `pnpm --dir web test` — `effort.test.ts` snap/keyboard, `Composer.test.tsx` pointer/keys/disabled
- Playwright mock: `composer-effort.spec.ts` (drag + keyboard + evidence shots)
- Live hub: `hub-live.spec.ts` drag/keyboard and assert `POST .../commands` `instance.configure`
