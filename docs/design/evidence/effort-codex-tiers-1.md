# Codex effort tiers — correction 1

Date: 2026-09-15. Scope: the owner's local Mac running codex-cli 0.154.0.
This corrects the five-tier Codex table and `ultra → xhigh` migration in
[composer-slider-5](./composer-slider-5.md).

## Picker contract

The owner supplied these names, order and descriptions verbatim from the
Codex picker. The local binary probes below independently establish config
acceptance; they do not enumerate the picker or measure model behavior.

| Wire value | Picker name | Subtitle / tooltip |
| --- | --- | --- |
| `low` | Low | Fast responses with lighter reasoning |
| `medium` | Medium | Balances speed and reasoning depth for everyday tasks |
| `high` | High | Greater reasoning depth for complex problems |
| `xhigh` | Extra high | Extra high reasoning depth for complex problems |
| `max` | Max | For difficult problems when quality matters more than speed · higher usage |
| `ultra` | Ultra | For demanding work using multiple agents · highest usage |

Default: `medium`. `minimal` remains accepted on input and maps to `low`.
Unknown stored words fall back to the harness default. Max and Ultra remain
unchanged through selection, persistence and argv. Ultra is a real Codex
level; Claude/agy/Grok reject it with `InvalidLaunchSpec`. `ultracode` remains
a Claude-only workflow flag and never becomes Codex's Ultra.

Max uses Claude max's static accent. Ultra uses the strongest ember effect.
The existing visual token `data-effort-look="ultracode"` names that shared
paint; Codex still emits `name: "ultra"`, with no ultracode workflow flag.

## Local binary probe — VERIFIED config acceptance, zero model requests

Each command ran against the installed binary with a 20-second subprocess
bound. No `codex exec`, prompt, model turn, agent session or config write was
needed. Existing user configuration was read without printing its contents.

```text
$ codex --version
codex-cli 0.154.0
exit_code: 0

$ codex -c 'model_reasoning_effort="max"' features list
apply_patch_freeform                     removed            false
apply_patch_preserve_line_endings        under development  false
[138 more feature rows omitted]
stderr: <empty>
exit_code: 0

$ codex -c 'model_reasoning_effort="ultra"' features list
apply_patch_freeform                     removed            false
apply_patch_preserve_line_endings        under development  false
[138 more feature rows omitted]
stderr: <empty>
exit_code: 0
```

Control probes establish the boundary of this check:

```text
$ codex -c 'model_reasoning_effort="invalid-remuda-effort-probe"' features list
[140 feature rows]
stderr: <empty>
exit_code: 0

$ codex -c 'model_reasoning_effort=42' features list
Error: failed to load bootstrap configuration

Caused by:
    invalid type: integer `42`, expected a string
    in `model_reasoning_effort`
exit_code: 1
```

The type-error control confirms this command loads and validates configuration.
String values are extensible, so success alone cannot establish which tiers
belong in the picker or which models support a tier. The six-tier UI contract
comes from the owner's authoritative transcription. Native inference quality,
cost and multi-agent execution were not exercised.

## Runtime and regression validation

Validation commands (repository root unless the command changes into `web`):

```sh
cargo build --locked -p remuda --bin remuda
TMPDIR=/private/tmp cargo test --locked -p remuda-protocol -p remuda-driver -p remuda-codex-wire
TMPDIR=/private/tmp cargo test --locked -p remuda-hub codex_
TMPDIR=/private/tmp cargo test --locked -p remuda-hub legacy_effort_tier_names_normalize_by_name_not_index
TMPDIR=/private/tmp cargo test --locked -p remuda-node model::tests
pnpm --dir web typecheck
pnpm --dir web lint
pnpm --dir web test
(cd web && PW_CHANNEL=chrome pnpm exec playwright test tests/e2e/composer-effort.spec.ts --project=chromium --workers=1)
(cd web && HUB_E2E_LISTEN=127.0.0.1:59180 HUB_E2E_WEB_PORT=59189 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59181 PW_CHANNEL=chrome REMUDA_EVIDENCE=1 pnpm exec playwright test -c playwright.hub.config.ts tests/e2e/effort-sync.hub.spec.ts)
```

- **PASS:** Rust protocol/driver/wire suites: 605 passed, 10 existing ignored.
- **PASS:** Hub vocabulary/persistence tests: 3 passed; Node configure/spec tests: 2 passed.
- **PASS:** web typecheck, lint (existing warnings), unit tests: 100 files / 891 tests.
- **PASS:** local Chrome composer spec: 15 passed, 1 existing mobile-only skip.
- **PASS:** local Chrome Hub effort spec: 3 passed, including both viewport widths.
- Shared JSON schema and `web/src/types/generated.ts` regenerated with `gen_types`;
  OpenAPI effort descriptions/enums updated and `web/src/lib/api.generated.ts`
  regenerated with `pnpm gen:api`.

The first Rust run hit two existing fixture prerequisites: macOS `/var` temp
aliases disagree with the fake harness's canonicalized Grok session path, and
its journal-diff test needs the `remuda` binary already built. Supplying the
canonical `/private/tmp` and building that binary made the unchanged parity
test and full suites pass. All changed Rust files pass `rustfmt --check`.
The first whole-workspace formatting check found pre-existing formatting in
`remuda-feishu/src/inbound.rs` and `remuda-ssh/tests/parse_ssh.rs`. Upstream main
corrected those files before the final rebase (base `07513199`); the final
`cargo fmt --all -- --check` passes without adding those paths to this branch.

The Hub tests use a fake Node and synthetic effort observations; they verify
Remuda request/read-back/persistence behavior, not native Codex turn completion.
The existing Claude max-to-xhigh mismatch fixture remains exercised separately.

Existing native boundary: the shell-PTY effort switch and transcript read-back
are implemented for Claude. Codex native live switching/read-back remains
unsupported; this vocabulary correction does not add a native Codex control
channel. Launch argv acceptance and synthetic Hub round-tripping are verified
separately.

## Screenshots

Captured by `web/tests/e2e/effort-sync.hub.spec.ts` in local Google Chrome.
Tracked captures require `REMUDA_EVIDENCE=1`; ordinary runs write only to
`web/test-results/evidence`. The fixtures contain synthetic session content.

| View | 390 px | 1440 px |
| --- | --- | --- |
| Max, static accent | [390](./effort-codex-tiers-1-max-390.png) | [1440](./effort-codex-tiers-1-max-1440.png) |
| Ultra, strongest effect | [390](./effort-codex-tiers-1-ultra-390.png) | [1440](./effort-codex-tiers-1-ultra-1440.png) |
| Six names and descriptions | [390](./effort-codex-tiers-1-list-390.png) | [1440](./effort-codex-tiers-1-list-1440.png) |
