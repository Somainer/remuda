# Composer effort slider · smaller card, flush fill, live embers

Third pass, from user feedback on the merged slider 2 card:

1. 「有点大了，小一点」 — the card and track were oversized.
2. 「slider 的头旁边的填充没满」 — a dark sliver of bare track sat between the brand fill and the knob.
3. 「ultra 档位的琥珀效果应该要有绚丽的动效」 — the top tier's ember was a still texture with one shimmer.

Mock UI (`VITE_MOCK=1`, Playwright chromium against a local vite port). Night Corral (`night`) and Ledger (`ledger`). No live Hub ports, no personal paths, no hostnames — shots are the composer element only.

Spec: `docs/design/ui-spec.md` Composer. Implementation: `web/src/features/session/EffortSlider.tsx`, `session.module.css`, native tables in `web/src/features/session/effort.ts`.

## 1 · Smaller, to the reference proportions

| | slider 2 | slider 3 | reference |
|---|---|---|---|
| card width | 400px | **300px** | ~300px |
| track height | 44px | **40px** | 40px (measured 48px @1.2×) |
| knob | 40px | **36px** | 36px (measured 54px @1.5× / 48px @1.2×) |
| tier title | 15px | **18px** | ~18px cap-to-descender 20–21px |
| model line | 12px | **13px** | ~13px, x-height band 17px |
| card padding | 12/12/14 | **8/10/10** | tight |
| head grid / icons | 28px | **26px** | — |

Measured off `ref-slider-mid.png` and `ref-slider-top.png` by scanning for the saturated pill band and the white knob run, then dividing out the shots' scale.

**Touch targets stay ≥ 44px through hit area, never visual size.** At the mobile breakpoint the pill keeps its 40px look inside a 48px `.effortHit` (4px block padding); the 26px icon buttons keep their glyph box and grow via an absolutely-positioned 44×44 `::after`; the tier-name button takes `min-height: 44px`. Verified at iPhone 13: card 300, hit 48, track 40, knob 36, reset reach 44×44, title button 44.

## 2 · The fill runs under the knob

The defect: `.effortFill` was `knobCentre` wide, so it stopped at the middle of the thumb and the track's dark surface showed through the knob's left half and beside it.

```css
/* was */ width: calc(var(--knob) + var(--pos) * (100% - var(--knob) * 2));
/* now */ width: calc(var(--knob) * 2 + var(--pos) * (100% - var(--knob) * 2));
```

`--knob` is half the knob (18px), fed in from `KNOB_INSET` in `EffortSlider.tsx` — it is both the knob's radius and the inset its centre travels within, so one number keeps pointer aim, knob position and fill width on the same scale. Width = knob centre + knob radius means the fill's right edge lands exactly on the knob's right edge; its rounded cap is the same radius as the pill and sits under the thumb, so it never reads as an end. At the first stop the fill is one whole knob wide (`--pos: 0` → `36px`), which is why no sliver appears there either. Past the last stop the cap is trimmed by `.effortClip`.

Verified two ways, both themes, all four stops:

- **Box geometry** — `fill.right === knob.right` at every stop (393 / 554 / 635 px at 1440), and `fill.left === track.left`.
- **Pixels** — screenshotting the track alone and classifying every pixel strictly inside the rounded rect, excluding the knob circle and 2px of antialiasing: **zero** unfilled-track pixels anywhere left of the knob centre, at stops 0–3, in `night` and `ledger`. The e2e keeps the box-geometry half of this as `assertFillReachesKnob`.

## 3 · Live embers on the top tier

Four stacked elements inside the fill, all `position: absolute; inset: 0`, all animated on **transform and opacity only** — no layout, no repaint, no per-spark DOM, no JS:

| layer | sparks | drift | twinkle |
|---|---|---|---|
| `.effortEmberGlow` | warm radial wash | — | `emberGlow` 4.5s |
| `.effortEmberBack` | 7 × 1–1.2px, dim | `emberDrift` 19s | `emberTwinkleSlow` 6.5s |
| `.effortEmberMid` | 7 × 1.2–1.6px | `emberDriftBack` 13s (counter) | `emberTwinkle` 4.2s |
| `.effortEmberFront` | 6 × 1.4–2px, brightest | `emberDrift` 8.5s | `emberTwinkleFast` 2.8s |

Each spark layer is 200% wide with `background-size: 50% 100%` and translates by exactly half its own width, so the pattern wraps seamlessly; three different speeds with one layer counter-drifting is what gives the field depth. Sparks are distributed across the whole tile (4%–96%) so coverage stays even as it moves. `will-change: transform, opacity` keeps each layer on the compositor.

The knob gets `.effortKnobGlow`, a soft amber halo breathing on opacity + scale (`knobBreath` 3.4s). Its base `translate(-50%, -50%)` is declared on the rule, not only in the keyframes, so it stays centred when the animation is dropped.

The collapsed trigger in the top tier gets the matching subtle glow: `emberChipGlow` 3.4s, the same period as the knob breath, replacing slider 2's `halo` — glow only, no expanding ring.

**Cheap and well-behaved.** Animation is confined to the open popover, which unmounts on close, so nothing animates behind a closed card. Under `prefers-reduced-motion: reduce` every ember animation is `none` and the still spark field plus both glows remain at fixed opacity — verified: all 8 animated nodes and the trigger chip report `animation-name: none`.

Motion confirmed live: four track screenshots 700ms apart are four distinct images.

## Screenshots

Composer-only crops, `animations: "disabled"`, so **the ember is captured as a static frame** — the drift and twinkle are not visible in a PNG. All ≤ 300 KB (largest 38 KB). Demo fixture session, no personal paths. `mid` = `think-hard` (non-top tier), `top` = `ultracode` (ember).

| | night | ledger |
|---|---|---|
| mid · 1440 | [composer-slider-3-mid-night-1440.png](./composer-slider-3-mid-night-1440.png) | [composer-slider-3-mid-ledger-1440.png](./composer-slider-3-mid-ledger-1440.png) |
| mid · 390 | [composer-slider-3-mid-night-390.png](./composer-slider-3-mid-night-390.png) | [composer-slider-3-mid-ledger-390.png](./composer-slider-3-mid-ledger-390.png) |
| top · 1440 | [composer-slider-3-top-night-1440.png](./composer-slider-3-top-night-1440.png) | [composer-slider-3-top-ledger-1440.png](./composer-slider-3-top-ledger-1440.png) |
| top · 390 | [composer-slider-3-top-night-390.png](./composer-slider-3-top-night-390.png) | [composer-slider-3-top-ledger-390.png](./composer-slider-3-top-ledger-390.png) |

## Unchanged

Snapping to harness-native tiers, `role=slider` with the tier name as `aria-valuetext`, ←/→/↑/↓/Home/End, pointer and touch drag, reset to the harness default, the `›` list view for tier descriptions and models, the `instance.configure` path, and the locked state. Every test id from slider 2 is unchanged; `effort-fill` and `effort-embers` are added.

## Tests

- `pnpm --dir web test` — 213 passed, 51 files (`effort.test.ts`, `Composer.test.tsx`).
- `pnpm --dir web exec tsc -b` — clean. `pnpm --dir web lint` — no new warnings.
- Playwright mock `composer-effort.spec.ts` — 26 passed, 2 skipped (chromium-only evidence, mobile-only touch sizing). New: fill-reaches-knob at every stop, ember layer count and distinct drift speeds, reduced-motion `animation-name: none`, touch targets ≥ 44px.
- `pnpm --dir web run test:e2e:hub` — live Hub `instance.configure` drag/keyboard, unchanged.
