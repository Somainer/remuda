# Hub message merge regression acceptance

Date: 2026-09-13. Base: `2f569cb6b2c7f59a603181c0a0fee9936ac3aadc`.
Regression introduced by `b2ea5af` (`fix(driver): preserve Claude print stream message identity`).

## Cause and reproduction

`crates/remuda-hub/examples/hub_e2e.rs::append_journal` emits the legacy
message payload `{ role, text }`. It does not supply `blocks`, `messageId`,
`nodeId`, `revision`, `operation`, or `status`. `web/src/lib/hubJournal.ts`
previously cast that payload directly to `Observation` for both durable reads
and live follow. The new assembler assumed the typed mutation fields existed.

The unchanged baseline failed with the exact gate command:

```sh
scripts/ci/gate.sh --web-only --web-e2e
```

On macOS with the configured Chrome channel, the build and 140 unit tests
passed; Hub e2e failed and the other five e2e tests passed. Browser evidence:

```text
TypeError: Cannot read properties of undefined (reading 'flatMap')
  blocksText -> assembleTranscript -> SessionPage
hub-live.spec.ts:31: session-page: element(s) not found
```

The component crashes when journal data arrives. Whether that happens before
the session-page assertion or before the subsequent user-message assertion
explains the two timeout locations. The shape mismatch reproduces locally;
Linux-specific ordering is not required to trigger it.

## Changes and regression coverage

- Normalize legacy message payloads at the shared read/follow boundary. Use the
  durable event identity for legacy messages, convert text to blocks, and retain
  typed protocol fields and explicit empty block arrays. Unknown operations open
  a message; legacy messages default to complete.
- Group messages by message identity, retaining node identity as an alias, then
  rebuild each group in revision order while preserving its first row position.
- Keep the highest revision. A missing-base append starts an available suffix;
  it never concatenates onto an unrelated prefix. Late history reconstructs the
  full message. Empty closes retain text, equal-revision completion wins, and
  replayed appends never duplicate text.
- Tests cover all six open/append/close arrival orders, first append/replace/close,
  missing bases, targeted suffixes, equal revisions, u64 revision precision,
  legacy read/follow replay, and a stable rendered user bubble as history arrives.
- `tests/e2e/hub-live.spec.ts` and the fake Node are unchanged. No Rust sources
  changed, so the conditional driver crate test requirement does not apply.

## Validation

- `pnpm lint`: exit 0; five warnings in unchanged files.
- `pnpm exec tsc -b`: pass (the package has no separate typecheck script).
- `scripts/ci/gate.sh --web-only --web-e2e`: install, build, unit tests and all
  six browser tests passed with the local Chrome channel after the primary fix.
- Final implementation: the same gate with `CI=1`, bundled Chromium
  `153.0.8010.12` (Playwright revision 1243), and four temporary Python CPU
  workers passed **41 unit-test files / 155 tests** and **6 e2e tests** without
  retries. The workers were terminated and waited for after the gate.
- The CI-mode run used `HUB_E2E_LISTEN=127.0.0.1:60080` and
  `HUB_E2E_WEB_PORT=60087` to isolate the harness from other local gates. These
  ports served the in-process Hub/fake Node and Vite; no `remuda dev` was started.
- All Cargo builds used the assigned agent target and `CARGO_INCREMENTAL=0`.
- `./scripts/ci/secret-scan.sh` and `git diff --check`: pass.

The browser runs were on macOS ARM64, including the CI Chromium configuration;
an Ubuntu runner was not available in this local acceptance. The original CI
assertions remain intact, and the deterministic payload crash was reproduced
before the fix.

## Real Claude-print acceptance

A standalone probe invoked the actual `ClaudePrintDriver` API, linked against
the gate's driver artifacts, with native Claude authentication, model `haiku`,
and `--max-budget-usd 0.3`. Its cwd and launch files were isolated beneath
`/tmp/remuda-driver/`. It sent one short text-only prompt, observed native
`turn_done`, and successfully closed the driver. This was a real model session.

One assistant `messageId` (`obj_01a09a16-491b-75ef-9e5c-1613f2f37cac`):

| Revision | Operation | Status | Text |
| --- | --- | --- | --- |
| 1 | open | streaming | Stable |
| 2 | append | streaming | ` streaming identity keeps every piece of this short` |
| 3 | append | streaming | ` answer together until completion.` |
| 4 | replace | complete | Full sentence |
| 5 | close | complete | Full sentence |

Final text: "Stable streaming identity keeps every piece of this short answer
together until completion." All five observations retained the same identity,
and revisions increased strictly from 1 to 5.
