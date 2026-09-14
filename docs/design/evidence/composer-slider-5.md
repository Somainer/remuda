# Composer effort slider · ultracode is the slider's top stop (six stops)

Fifth pass, from user feedback with a Claude Desktop screenshot (one slider, Faster → Smarter, its rightmost stop `ultracode`):

> 「和 Claude Desktop 一样，我希望 ultracode 是在 max 右边的档位，而不是一个单独的按钮然后切到 xhigh，我知道实现是这样，但我还是希望用同一个 slider，不要独立按钮」

Pass 4 modelled ultracode as a standalone toggle chip that forced and locked the track on `xhigh`. That interaction is gone: ultracode is now the **sixth stop on the one slider**, immediately to the right of `max`. The underlying implementation shape is unchanged.

Night Corral (`night`) and Ledger (`ledger`). Shots are composer/effort-element crops only — no personal paths, no hostnames. Static frames (`animations: "disabled"`), so ember drift is captured as a still field.

Implementation: `web/src/features/session/effort.ts` (tier table + stop model), `web/src/features/session/EffortSlider.tsx`, `session.module.css`, `web/src/pages/NewSessionPage.tsx`.

## 1 · One slider, six stops

Stop order for Claude:

| stop index | stop name | wire selection | ember |
|---|---|---|---|
| 0 | `low` | `{name: "low", ultracode: false}` | — |
| 1 | `medium` | `{name: "medium", ultracode: false}` | — |
| 2 | `high` (default) | `{name: "high", ultracode: false}` | — |
| 3 | `xhigh` | `{name: "xhigh", ultracode: false}` | — |
| 4 | `max` | `{name: "max", ultracode: false}` | amber |
| 5 | `ultracode` | **`{name: "xhigh", ultracode: true}`** | denser amber |

The five real `claude --effort` tiers (`low medium high xhigh max`) are unchanged; the sixth stop is presentation-only (`effortStops()` / `EffortStop`). Selecting it maps to the D-028 §9.1 wire shape `{name: "xhigh", ultracode: true}` — the wire format and the Rust/API client are untouched. The slider is **never locked** any more: every stop is a normal stop, so ←/→ move across all six and Home/End jump to `low`/`ultracode`. Dragging snaps across six intervals.

**Reverse mapping:** a record `{xhigh, ultracode: true}` renders at stop 5 (`effortStopIndex`), `{xhigh, false}` at stop 3. Legacy names still normalize by name: `default → low`, `think → high`, `think-hard → xhigh`, `ultracode` (old sentinel) → the ultracode stop. Unit tests round-trip all six stops selection → stop position → selection, including the reload path.

**Display:** every surface that names the current stop shows **`ultracode`** while the flag is set — popover title, New Session label row, collapsed composer chip (which now reads `ultracode`, not the tier name) — while the form's `data-effort` attribute keeps carrying the `ultracode` wire sentinel. `codex / grok / agy` tables, stops and cross-harness mapping are byte-for-byte unchanged; the flag is Claude-only and drops when switching harnesses.

## 2 · The standalone chip is removed everywhere

The `ultracode` toggle chip (popover header, inline label row), its lock semantics (`aria-disabled` + removed tab order while on) and the old list-view exclusion are deleted. Ultracode now appears:

- as the sixth dot and tick on the pill,
- as the sixth row of the chevron tier/model list (marked ember, `data-ultracode="1"`, tooltip 「ultracode · 锁定 xhigh，启用多代理工作流编排」),
- nowhere else.

## 3 · Ember on max, denser on ultracode

`max` keeps the existing field: amber gradient, breathing wash, three drifting spark layers at three speeds, knob halo. The ultracode stop plays the **same field plus a fourth layer** — small, tightly spaced bright dots counter-drifting (the Desktop's dotted-glow feel), a slightly brighter wash and a tighter/faster knob halo. `data-intensity="ultra"` marks the field; under `prefers-reduced-motion` all motion still freezes while the still field remains.

## 4 · Tick labels and the 400px width

Every stop carries a label under its dot, both in the popover and inline:

- **Composer popover (~280px):** short labels always — `low med high xhigh max ultra` (`EffortStop.short`, 10px).
- **New Session inline field:** full names `low medium high xhigh max ultracode`; under the 767px breakpoint they swap (CSS-only, both spans are in the DOM) to the same short labels. First/last inline labels edge-align to the track ends, so `medium`/`ultracode` never overhang.
- The collapsed composer chip (`ultracode`, 9 chars) stays single-line at both 390 and 1440.

## Screenshots

`mid` = plain `xhigh`, `max` = the ember tier, `ultra` = the ultracode stop (denser ember).

**Inline New Session field (layout A):**

| | night | ledger |
|---|---|---|
| mid · 1440 | [composer-slider-5-new-mid-night-1440.png](./composer-slider-5-new-mid-night-1440.png) | [composer-slider-5-new-mid-ledger-1440.png](./composer-slider-5-new-mid-ledger-1440.png) |
| mid · 400 | [composer-slider-5-new-mid-night-400.png](./composer-slider-5-new-mid-night-400.png) | [composer-slider-5-new-mid-ledger-400.png](./composer-slider-5-new-mid-ledger-400.png) |
| max · 1440 | [composer-slider-5-new-max-night-1440.png](./composer-slider-5-new-max-night-1440.png) | [composer-slider-5-new-max-ledger-1440.png](./composer-slider-5-new-max-ledger-1440.png) |
| max · 400 | [composer-slider-5-new-max-night-400.png](./composer-slider-5-new-max-night-400.png) | [composer-slider-5-new-max-ledger-400.png](./composer-slider-5-new-max-ledger-400.png) |
| ultra · 1440 | [composer-slider-5-new-ultra-night-1440.png](./composer-slider-5-new-ultra-night-1440.png) | [composer-slider-5-new-ultra-ledger-1440.png](./composer-slider-5-new-ultra-ledger-1440.png) |
| ultra · 400 | [composer-slider-5-new-ultra-night-400.png](./composer-slider-5-new-ultra-night-400.png) | [composer-slider-5-new-ultra-ledger-400.png](./composer-slider-5-new-ultra-ledger-400.png) |

**Composer popover card:**

| | night | ledger |
|---|---|---|
| mid · 1440 | [composer-slider-5-mid-night-1440.png](./composer-slider-5-mid-night-1440.png) | [composer-slider-5-mid-ledger-1440.png](./composer-slider-5-mid-ledger-1440.png) |
| mid · 400 | [composer-slider-5-mid-night-400.png](./composer-slider-5-mid-night-400.png) | [composer-slider-5-mid-ledger-400.png](./composer-slider-5-mid-ledger-400.png) |
| max · 1440 | [composer-slider-5-max-night-1440.png](./composer-slider-5-max-night-1440.png) | [composer-slider-5-max-ledger-1440.png](./composer-slider-5-max-ledger-1440.png) |
| max · 400 | [composer-slider-5-max-night-400.png](./composer-slider-5-max-night-400.png) | [composer-slider-5-max-ledger-400.png](./composer-slider-5-max-ledger-400.png) |
| ultra · 1440 | [composer-slider-5-ultra-night-1440.png](./composer-slider-5-ultra-night-1440.png) | [composer-slider-5-ultra-ledger-1440.png](./composer-slider-5-ultra-ledger-1440.png) |
| ultra · 400 | [composer-slider-5-ultra-night-400.png](./composer-slider-5-ultra-night-400.png) | [composer-slider-5-ultra-ledger-400.png](./composer-slider-5-ultra-ledger-400.png) |

## Tests

- `pnpm --dir web test` — all files green (422 tests, 69 files). New coverage in `effort.test.ts`: six-stop order, all-six-stops selection ↔ stop-position round-trip, `{xhigh, ultracode:true|false}` reverse mapping, legacy name → stop, six-stop arrows/Home/End; `Composer.test.tsx` / `NewSessionPage.test.tsx` walk `xhigh → max → ultracode` on the one slider and assert the chip/title read `ultracode`.
- `pnpm --dir web exec tsc -b` — clean. `pnpm --dir web lint` — no new warnings.
- Playwright mock specs `composer-effort.spec.ts` + `new-session.spec.ts` (chromium): six-stop tiers/ticks/keyboard, drag to far end selects ultracode, persist-after-reload reverse mapping, three vs four ember drift layers (`data-intensity`), 390/1440 single-line chips and label fit. Two unrelated PTY-fixture failures (`grok pty … yolo`, `kind terminal … shell-pty`) fail identically on origin/main.
- `pnpm --dir web run test:e2e:hub` — run once against the live `remuda-hub` example; the configure spec now drags to the far-right stop and asserts the `ultracode` configure wire name, then Home → `low`. (Two unrelated suite tests flaked on timing in the full run and passed on an immediate re-run, on both the branch and origin/main.)
