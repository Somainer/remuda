# Composer control bar and per-harness effort

Mock UI (`VITE_MOCK=1`, Playwright against `127.0.0.1:4177`). Night Corral. No live Hub ports, no personal paths.

Spec: `docs/design/claude-design/Remuda UI Spec v0.2.dc.html` boards 1b / 1k / 1d / 1h (commit `21ca43e`). Written spec: `docs/design/ui-spec.md`.

## What landed

- Session composer has a collapsed control bar under the input: harness, model+effort, context %, permission. Hidden per harness capability table (permission is Claude-only; terminal hides model/effort/context).
- Effort tables are native: claude `default · think · think-hard · ultracode`; codex `low · medium · high · ultra`; grok `quick · standard · max`; agy a single `default`. Stored as index + native name; harness switch remaps by index so the top (ember) tier stays the top tier.
- Composer effort change sends `instance.configure` with `{ effort }` (not local-only). Settings default (claude table index) flows into New Session; New Session writes the native name into create state; the composer changes the current turn.
- Ember (amber text/border, `ember` / `halo` / `spark` keyframes) on the top tier. `prefers-reduced-motion: reduce` disables those animations.
- Approval card stays above the composer. The effort menu opens downward/inline when an approval card is present so bounding boxes do not overlap.
- New Session permission chips are `white-space: nowrap; min-width: max-content` and wrap the **row**. Four modes: 询问 / 可改文件 / 全自动 / 绕过全部. Yolo box sits below the row with an aligned checkbox.

## Screenshots

| | |
|---|---|
| Collapsed composer bar | [composer-1-bar.png](./composer-1-bar.png) |
| Effort menu (claude table + ember ultracode) | [composer-1-effort-menu.png](./composer-1-effort-menu.png) |
| Harness picker (account row, `+` for uninstalled) | [composer-1-harness-menu.png](./composer-1-harness-menu.png) |
| Approval + expanded menu, no overlap | [composer-1-approval-menu.png](./composer-1-approval-menu.png) |
| New Session 1440 | [composer-1-new-session-1440.png](./composer-1-new-session-1440.png) |
| New Session 390 (single-line chips, yolo + checkbox, native effort) | [composer-1-new-session-390.png](./composer-1-new-session-390.png) |

## data-testid contract (stable for computer-use)

| id | where |
|---|---|
| `composer` | form; attrs `data-harness` `data-effort` `data-effort-index` `data-model` |
| `composer-bar` | chip row |
| `harness-chip` | collapsed harness |
| `harness-menu` | popover; `data-placement=up\|down` |
| `harness-option-{claude,codex,grok,agy,terminal}` | row; `data-installed=0\|1` |
| `model-effort-chip` | collapsed model+effort; `data-ember=0\|1` |
| `effort-menu` | popover; header/footer copy from the spec |
| `effort-tier-{name}` | one row per native tier; `data-ember=1` on the top tier |
| `model-option-{short}` | model list inside the effort popover |
| `context-chip` | plain text percent or `—` |
| `permission-chip` | collapsed permission |
| `permission-menu` / `permission-option-{id}` | manual / acceptEdits / dontAsk / bypassPermissions |
| `approval-card` | existing; used for overlap checks |
| `new-session-perm-row` | permission chip row |
| `new-session-perm-{id}` | one chip |
| `new-session-yolo-hint` / `new-session-yolo-ack` | yolo box + checkbox |
| `new-session-effort` / `new-session-effort-{name}` | native effort row; `data-selected` `data-ember` |
| `settings-effort` / `settings-effort-{name}` | default effort (claude table) |
| `session-effort` | session list / run row native name; `data-ember` |

## Tests

- `pnpm --dir web test` (128)
- `pnpm --dir web build`
- Playwright mock: `composer-effort.spec.ts` (chromium + mobile-webkit) plus the existing session/new-session/settings suites

Idle mock sessions have no usage event, so the context chip shows `—` rather than the spec’s example `74%`. Working-session journals still drive a real percent when tokens are known.

## Computer-use (live demo)

Screenshots above are mock (`VITE_MOCK=1`). On the live demo, keep these testids: `composer-bar`, `harness-chip`, `model-effort-chip`, `context-chip`, `permission-chip`, `effort-menu`, `effort-tier-*`, `harness-menu`, `approval-card`, `new-session-perm-row`, `new-session-yolo-hint`, `settings-effort`. Open the effort menu with an approval card visible and assert the two bounding boxes do not overlap. Check New Session permission chips at 390px and 1440px for single-line labels. Do not use ports 18080/18787.
