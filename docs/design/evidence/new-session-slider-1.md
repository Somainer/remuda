# New Session effort · the composer's slider, inline

User feedback on the merged composer slider: 「这里的 effort 怎么不是滑块」 — the New
Session sheet still picked effort with the old tier chips (`default` /
`think` / `think-hard` / `ultracode`) while the composer had the Codex-style
pill. Two pickers for one value, and the newer one only on one of the two
surfaces that set it.

Mock UI (`VITE_MOCK=1`, Playwright chromium against a local vite port). Night
Corral (`night`) and Ledger (`ledger`). No live Hub ports, no personal paths,
no hostnames — the shots are the effort card only.

Implementation: `web/src/features/session/EffortSlider.tsx`,
`session.module.css`, `web/src/pages/NewSessionPage.tsx`. Previous pass:
[composer-slider-3.md](./composer-slider-3.md).

## 1 · One slider, two mounts

`EffortSlider` gained two props rather than being copied:

| prop | default | New Session |
|---|---|---|
| `variant` | `"popover"` — the composer's menu frames the card | `"inline"` — the card frames itself |
| `idPrefix` | `"effort"` | `"new-session-effort"` |
| `footer` | `EFFORT_MENU_FOOTER` | 「写进 InstanceSpec，会话内可再改」 |

`idPrefix` renames every `data-testid` the card emits (`<prefix>-slider`,
`-track`, `-knob`, `-fill`, `-embers`, `-title`, `-model`, `-reset`,
`-open-list`, `-list`, `-tier-<name>`, `-slider-panel`), so the two mounts are
addressable apart and can coexist. The composer passes neither prop, so
**every composer test id is unchanged** and its behaviour is untouched — the
26 assertions in `composer-effort.spec.ts` still pass against it.

There is no second tier table, no second keyboard map, no second snapping
rule: `effort.ts` remains the only source, and the page no longer imports
`effortTable` / `isEmberTier` at all.

`.effortInline` is the only new CSS — the `.effortCard` body is shared:

```css
.effortInline {
  width: min(400px, 100%);   /* the sheet's column; the popover keeps 300px */
  border: 1px solid var(--line);
  border-radius: var(--radius);
  background: var(--ink-2);
  padding: 8px 12px 12px;
}
```

Everything else comes along unchanged: the compact head (bolt · tier name ·
reset), the hint line under it, the dotted pill with the fill running **under**
the knob to its far edge, the top tier's amber ember field, the knob's
breathing halo, `prefers-reduced-motion`, the ≥ 44px touch targets, and the
`›` list view for tier descriptions.

## 2 · Snapping to the harness above

The harness buttons (`claude` / `codex` / `grok` / `agy` / `terminal`) sit
directly above the card, and the page already mapped the selection through
`mapEffort` on switch. The card is mounted with `key={activeKind}`, so a
switch remounts it: the pill re-snaps onto the new native table **and** any
half-finished pointer draft inside the card is dropped with it, instead of the
knob staying where the last drag left it on a table that no longer has that
stop.

| from | to | why |
|---|---|---|
| claude `think` (1/4) | codex `medium` (1/4) | same length, renamed in place |
| claude `think` (1/4) | grok `standard` (1/3) | 1/3 along → nearest stop |
| claude `ultracode` (top) | grok `max` (top) | top tier always lands on top |
| grok `quick` (first) | claude `default` (first) | and the ember drops |
| any | `terminal` | no effort axis → the whole card unmounts |

`agy` has a single tier, so its pill sits at the ember end — the existing
`effortRatio` rule, unchanged.

## 3 · The value still goes into InstanceSpec

Untouched. `sessionEffort` is still `effort.kind === activeKind ? effort :
mapEffort(...)`, and `hubStore.create` still receives `effortIndex` /
`effortName` from it, alongside `rememberNewSessionSuccess`. The
「写进 InstanceSpec，会话内可再改」 hint stays beside the card (and repeats as
the card's own footer in the list view). The fieldset carries
`data-harness` / `data-effort` so a test can read the committed pair without
reaching into the card.

## Screenshots

Effort-card crops, `animations: "disabled"`, so **the ember is captured as a
static frame** — the drift and twinkle are not visible in a PNG. All ≤ 300 KB
(largest 26 KB). `mid` = `think-hard` (non-top tier), `top` = `ultracode`
(ember).

| | night | ledger |
|---|---|---|
| mid · 1440 | [new-session-slider-1-mid-night-1440.png](./new-session-slider-1-mid-night-1440.png) | [new-session-slider-1-mid-ledger-1440.png](./new-session-slider-1-mid-ledger-1440.png) |
| mid · 390 | [new-session-slider-1-mid-night-390.png](./new-session-slider-1-mid-night-390.png) | [new-session-slider-1-mid-ledger-390.png](./new-session-slider-1-mid-ledger-390.png) |
| top · 1440 | [new-session-slider-1-top-night-1440.png](./new-session-slider-1-top-night-1440.png) | [new-session-slider-1-top-ledger-1440.png](./new-session-slider-1-top-ledger-1440.png) |
| top · 390 | [new-session-slider-1-top-night-390.png](./new-session-slider-1-top-night-390.png) | [new-session-slider-1-top-ledger-390.png](./new-session-slider-1-top-ledger-390.png) |

The card in the whole sheet: [composer-1-new-session-1440.png](./composer-1-new-session-1440.png)
and [composer-1-new-session-390.png](./composer-1-new-session-390.png), both
re-captured. At 390 the sheet is narrower than 400px, so the card takes the
column and the hint wraps under it; at 1440 the hint sits beside it.

**Evidence capture is opt-in.** Both `new-session.spec.ts` and
`composer-effort.spec.ts` now write to the gitignored `web/test-results/`
unless `REMUDA_EVIDENCE=1`, so a default `pnpm test:e2e` no longer rewrites
tracked PNGs. (Previously both wrote straight into `docs/design/evidence/`.)

## Tests

- `pnpm --dir web test` — **288 passed, 59 files**. `NewSessionPage.test.tsx`
  adds five: the slider replaces the chips; the harness switch re-snaps
  (claude→codex→grok, names, indices, `data-tiers`); the top tier stays the
  top tier across tables and the draft is dropped with the remount; and the
  tier the slider is left on reaches `hubStore.create` as
  `effortIndex`/`effortName`, before and after a harness switch.
- `pnpm --dir web exec tsc -b` — clean. `pnpm --dir web lint` — no new warnings.
- Playwright mock, `new-session.spec.ts` — 5 chromium / 4 mobile-webkit. New:
  the inline card's role/tiers/pill geometry and fill-reaches-knob; setting
  the tier **by keyboard** (`Home`, `ArrowRight`×2, `End`, `ArrowLeft`) and
  asserting the **created** instance carries it (`composer[data-effort]` =
  `think-hard`, `data-effort-index` = `2`); the harness re-snap end to end,
  including `terminal` unmounting the card.
- `composer-effort.spec.ts` — 13 chromium passed, 1 skipped (mobile-only touch
  sizing); its two New Session assertions moved from the chips to the slider.
- `pnpm --dir web run test:e2e:hub` — live Hub `instance.configure` drag and
  keyboard, unchanged.
