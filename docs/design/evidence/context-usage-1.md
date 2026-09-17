# Context usage chip popover — correction 1

Date: 2026-09-17. Scope: the composer's context chip (`web/src/features/session/Composer.tsx`)
previously painted an empty ring and the literal `○ —`; hovering it revealed nothing.
The owner asked for the live context fill, session token totals, and input/output TPM
on hover.

## What the chip now shows

- **Ring + label**: live `contextPct` (0–100), driven by the Hub per-session rollup.
- **Hover/tap popover** (`ContextUsagePopover.tsx`):
  - `上下文 <used>/<window> (<pct>%)` plus a thin fill bar;
  - 本会话 tokens: 入 / 出 / 缓存读 / 缓存写;
  - 回合数;
  - TPM 入/出 for the last **60 s** (sum) and **5 min** (per-minute average = window sum / 5);
  - 最近一回合 relative time.
- Touch widths (390 px) render the same card as a fixed bottom **sheet** with an explicit
  `×` close affordance; desktop opens on hover (click-pinned), closes on pointer-leave /
  outside pointerdown / Escape / `×`. Both light and dark themes use the existing
  `--cold / --line / --ink-2 / --paper / --mute` tokens.

## Data model — Hub-computed additive rollup

The Node usage adapters (`remuda-driver/src/usage/*`) already append protocol
`UsagePayload` observations (`inputTokens`, `outputTokens`, `cacheReadTokens`,
`cacheWriteTokens`, …, every counter wrapped in `Knowledge`). The Hub already persisted
them into `usage_events` (`usage_store.rs`). What was missing was a per-session
projection. Added `InstanceUsageRollup`, computed inside `load_instance` from the
durable table (recomputed on read, so TPM windows never go stale; nothing redundant is
stored on the instance row):

| Wire field (camelCase) | Meaning |
| --- | --- |
| `contextUsedTokens` | Last turn's fresh input + cache read + cache creation — what the next request carries |
| `contextWindowTokens` | `[1m]` tag → model catalog → harness-kind fallback (claude/codex/agy 200k, grok 128k) |
| `contextPct` | rounded, clamped 0–100; null until both sides known |
| `sessionInputTokens` / `sessionOutputTokens` | additive sums of uncached input / output |
| `cacheReadTokens` / `cacheCreationTokens` | additive sums of cache read / write |
| `turns` | folded usage observations |
| `tpmIn60s` / `tpmOut60s` | sums over events with `observed_at >= now-60s` |
| `tpmIn5m` / `tpmOut5m` | window sum / 5 |
| `lastTurnAt` | `MAX(observed_at)` |

Field naming: the `c-livephrase` worktree was already merged into `main` at integration
time and shipped no token rollup (`LiveStatusStrip` renders phase/elapsed/health only),
so these names are the canonical first cut; the live strip consumes nothing from them.

**Unknown is never zero.** SQL `SUM` returns NULL when no folded row reported a column;
the rollup keeps `Option`/`null` end to end, and the popover renders `—` with a tooltip
naming the channel that would supply the value (e.g. Codex/Grok adapters that omit the
cache breakdown; no input-bearing turn inside the TPM window).

**Hydration gotcha (web).** `mergeInstanceSnapshots` deliberately keeps the in-memory
instance when its follow-bumped `durableSeq` is ≥ the polled row's, so a Hub-computed
field on the polled object never replaces the retained one (the same reason
`effortEffective` has its own hydration map). The rollup therefore lives in a separate
`HubStore` map — `hydrateUsageRollups()` folds every polled instance's rollup, newest
turn count wins — and `SessionPage` reads `hubStore.usageRollupOf(id)` rather than
`instance.usageRollup`. Without this, chip and popover stayed blank
(`data-has-popover="0"`) even though the raw `/v1/instances` payload carried the rollup.

### Recorded sequence

The Rust rollup test replays three real `message.usage` frames captured from a Claude
Code 2.1.x transcript on the dev host:

| turn | input | cache_read | cache_creation | output |
| --- | --- | --- | --- | --- |
| 1 | 4 794 | 29 496 | 0 | 260 |
| 2 | 1 839 | 33 592 | 0 | 185 |
| 3 | 1 223 | 34 616 | 0 | 144 |

Turn 1/2 sit at −500 s/−200 s/−5 s to exercise both TPM windows; turn 3's last-request
context is `1223 + 34616 = 35 839` → **18%** of 200k; the 60 s window contains only
turn 3 (`tpmIn60s = 1223`), the 5 min average divides turns 2+3 by five.

## Verification

- `cargo test -p remuda-hub`: rollup fold on the recorded sequence, unknown-channel
  (Grok output-only) shape, no-events → no rollup, window resolution order; existing
  usage dedupe/budget tests unchanged.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `pnpm typecheck` / `pnpm lint` / `pnpm test`: clean (962 unit tests; added
  `contextUsage.test.ts`, `ContextUsagePopover.test.tsx`, chip cases in `Composer.test.tsx`,
  and `store.usage.test.ts` covering the seq-merge hydration path).
- Hub e2e `tests/e2e/ux-usage.hub.spec.ts`: the fake Node appends a real protocol usage
  observation on the `usage:<in>,<out>,<read>,<write>` sentinel (`-` = unreported).
  Asserts ring %, bar width, headline, session cells, both TPM rows, the output-only
  turn flipping context to `—`, and the 390 px sheet + close. Backend rollup arithmetic
  was independently confirmed over three live API turns (17% → 18% → context unknown,
  totals 4 794/260 → 6 633/445 → 6 633/865).
