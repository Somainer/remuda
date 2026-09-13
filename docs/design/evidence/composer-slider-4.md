# Composer effort slider · real Claude levels, inline New Session field, ultracode toggle

Fourth pass, from two pieces of feedback on the merged slider 3:

1. 「Claude 的档位也不是这些啊」— the Claude table still read `default / think / think-hard / ultracode`, which are not what `claude --help` exposes. The real levels are `low / medium / high / xhigh / max`; `ultracode` is not a level at all, it is the workflow-orchestration switch.
2. 「滑块放在这里也太丑了，不和谐，可能需要 redesign 一下」— a bordered Codex-style card floated in the New Session sheet with its helper text stranded at the right. Chosen redesign was **layout A: no card**, the slider becomes an ordinary form field sharing the sheet's column and muted helper slot.

Mock UI (`VITE_MOCK=1`, Playwright chromium / iPhone 13 against the local vite port) plus one live `remuda-hub` run. Night Corral (`night`) and Ledger (`ledger`). Shots are the effort element only — no personal paths, no hostnames.

Spec: `docs/design/ui-spec.md` Composer / New Session. Implementation: `web/src/features/session/effort.ts` (single tier source), `web/src/features/session/EffortSlider.tsx`, `session.module.css`, `web/src/pages/NewSessionPage.tsx`.

## 1 · The Claude table is the five real levels

From `claude --help`, in order:

| index | name | description |
|---|---|---|
| 0 | `low` | 最省 · 最快 |
| 1 | `medium` | 日常档 |
| 2 | `high` | 默认档（device default） |
| 3 | `xhigh` | 跨文件 · 长任务 |
| 4 | `max` | 最高档 · 慢且贵（ember） |

Codex (`low/medium/high/ultra`), grok (`quick/standard/max`) and agy tables are unchanged; cross-harness mapping stays ratio-based by index.

**Legacy stored values normalize by name**, so nothing saved under the old picker is misread: `default → low`, `think → high`, `think-hard → xhigh`, `ultracode → xhigh` with the ultracode toggle on, anything unknown → `high`. Both the New Session remembered-prefs path (`normalizeClaudeName`) and the Hub instance read path (`effortFromRecord`) go through the same normalizer.

## 2 · ultracode is a toggle, not a tier

`ultracode` is a Claude-only boolean (`EffortSelection.ultracode`). Turning it on forces the level to `xhigh` (`CLAUDE_ULTRACODE_INDEX = 3`), locks the track (`aria-disabled`, removed from tab order, arrows/drags ignored), and plays the ember — Claude runs xhigh with multi-agent workflow orchestration. The chip is a small outlined `ultracode` pill at the far right of the header row (popover) or the label row (inline), with `aria-pressed` and the tooltip 「ultracode · 锁定 xhigh，启用多代理工作流编排」. Toggling off returns to a plain, non-ember xhigh with an unlocked track.

**Ember rule:** the top table row (`max`) **or** Claude ultracode. Plain `xhigh` is deliberately not ember, in the track, the collapsed chip, the session list and the settings chips.

**Wire compatibility.** The Hub stores an opaque `{name, index}` and the running Claude CLI does not yet receive a flag for this axis. Until the x-p1-proto work lands `EffortSelection = {name: low|medium|high|xhigh|max, ultracode: bool}` and applies it with `--effort`, the web sends the native level names (`low…max`) and uses the legacy string `"ultracode"` as the sentinel for the locked state (`effortWireName`), then normalizes it back on read. **The visual selection therefore reaches the Hub record today, but the actual `--effort` application to the Claude process lands with x-p1-proto** — no `crates/` changes are in this pass.

## 3 · Layout A: the slider becomes a form field (New Session)

No card, no frame, no popover chrome:

- **Label row** — `effort` in muted mono at the left; the current level name plus its short Chinese description beside it in muted text; the ultracode chip at the far right.
- **Track** — the same dotted pill mechanics as the composer card, spanning the form column edge-to-edge. Verified at 1440: the pill's x/width match the permission row within 1px.
- **Tick labels** — the five level names under their stops (`low medium high xhigh max`), the active one in paper colour and amber when ember. The full names still fit at 400px (stop gap ≈ 70px vs ≤ 42px label), so no cryptic shortening is needed any more; the knob stays draggable and ←/→/Home/End work at every width.
- **Helper** — 「写进 InstanceSpec，会话内可再改」 sits in the same muted mono slot under the field as every other field's helper, never floated to the right.

The pill's scale tokens (`--pill 40px`, `--knob-size 36px`, knob face, brand colour) are shared by the framed card and the frameless form, so the two surfaces cannot drift apart. An ember lock keeps its amber while the track is disabled — the dimming reserved for genuinely unavailable controls — and under reduced motion the chip's loose sparks are hidden (the ember border/text and the track's still spark field carry the state).

## 4 · Composer popover: same compact card, five levels + chip

The approved ≤300px slider-3 card is kept; its header is now `bolt · level› · ultracode · reset` (the title takes the shrinking `1fr` track so the card never exceeds 300px), with five stops on the pill and the ember on `max` or ultracode. The collapsed chip names the locked tier `xhigh` while the composer stamps the wire name `ultracode`.

## Screenshots

Composer-only / effort-field-only crops, `animations: "disabled"` — **the ember is a static frame**, drift and twinkle are not visible in a PNG. `mid` = `xhigh` (plain, non-ember), `top` = `max` (ember). Demo fixtures, no personal paths.

**Inline New Session field (layout A):**

| | night | ledger |
|---|---|---|
| mid · 1440 | [composer-slider-4-new-mid-night-1440.png](./composer-slider-4-new-mid-night-1440.png) | [composer-slider-4-new-mid-ledger-1440.png](./composer-slider-4-new-mid-ledger-1440.png) |
| mid · 400 | [composer-slider-4-new-mid-night-400.png](./composer-slider-4-new-mid-night-400.png) | [composer-slider-4-new-mid-ledger-400.png](./composer-slider-4-new-mid-ledger-400.png) |
| top · 1440 | [composer-slider-4-new-top-night-1440.png](./composer-slider-4-new-top-night-1440.png) | [composer-slider-4-new-top-ledger-1440.png](./composer-slider-4-new-top-ledger-1440.png) |
| top · 400 | [composer-slider-4-new-top-night-400.png](./composer-slider-4-new-top-night-400.png) | [composer-slider-4-new-top-ledger-400.png](./composer-slider-4-new-top-ledger-400.png) |
| ultracode-locked · 1440 | [composer-slider-4-new-ultra-night-1440.png](./composer-slider-4-new-ultra-night-1440.png) | — |
| ultracode-locked · 400 | [composer-slider-4-new-ultra-night-400.png](./composer-slider-4-new-ultra-night-400.png) | — |

**Composer popover card:**

| | night | ledger |
|---|---|---|
| mid · 1440 | [composer-slider-4-mid-night-1440.png](./composer-slider-4-mid-night-1440.png) | [composer-slider-4-mid-ledger-1440.png](./composer-slider-4-mid-ledger-1440.png) |
| mid · 400 | [composer-slider-4-mid-night-400.png](./composer-slider-4-mid-night-400.png) | [composer-slider-4-mid-ledger-400.png](./composer-slider-4-mid-ledger-400.png) |
| top · 1440 | [composer-slider-4-top-night-1440.png](./composer-slider-4-top-night-1440.png) | [composer-slider-4-top-ledger-1440.png](./composer-slider-4-top-ledger-1440.png) |
| top · 400 | [composer-slider-4-top-night-400.png](./composer-slider-4-top-night-400.png) | [composer-slider-4-top-ledger-400.png](./composer-slider-4-top-ledger-400.png) |

## Tests

- `pnpm --dir web test` — 321 passed, 61 files. New coverage in `effort.test.ts`: five-level order/defaults, legacy name normalization, ultracode forcing xhigh and Claude-only scoping, the ember rule, wire name sentinel, five-stop snapping.
- `pnpm --dir web exec tsc -b` — clean. `pnpm --dir web lint` — no new warnings.
- Playwright mock `new-session.spec.ts` + `composer-effort.spec.ts` — chromium 22 passed / 1 skipped, mobile-webkit (iPhone 13) 20 passed / 3 skipped: layout-A framing and column alignment, five-stop keyboard/drag, ultracode lock + `ultracode` create, harness re-snap, legacy remembered tier, fill geometry, reduced motion, touch targets ≥ 44px.
- `pnpm --dir web run test:e2e:hub` — 11 passed against the live `remuda-hub` example; the drag/keyboard spec now asserts the wire names `max` (drag to far end) and `low` (Home) on the five-stop table.
