# Usage adapter fixtures

Provenance for `tests/usage.rs`. Token numbers in the grok/codex spike
fixtures are synthetic model-reported usage, not measured billing; see the
`codex/` and `grok/` READMEs.

| File | Origin |
| --- | --- |
| `claude-transcript-usage.jsonl` | Hand-built. Two model messages (`msg_parity_alpha`, `msg_parity_beta`) spread over five content-block records (the native transcript repeats one `message.usage` per block), one sidechain record that must be ignored. Counter totals intentionally match `claude-result-cost.json` so the transcript and print-result paths price identically. |
| `claude-result-cost.json` | Minimal `result` frame derived from the committed `remuda-claude-wire` fixture `claude-askuser.jsonl` (its real `total_cost_usd` / `usage`), for the print-retirement parity gate. |
| `grok-headless-usage.jsonl` | The `usage` and `end` frames from `remuda-testing/fixtures/grok/grok-headless-streaming-json.jsonl` (synthetic headless stream). |
| `grok-usage.json` | Hand-built per-model `usage.json` shape. The P6 spike captured the filename but not its body (`docs/design/evidence/grok-signals-1.md`); field names mirror the observed headless `end.modelUsage` object — deviation documented in `docs/design/usage-adapter.md`. |
| `grok-updates-usage.jsonl` | Hand-built ACP `turn_completed` frames carrying `update.usage`, plus one ordinary chunk frame with only `_meta.totalTokens` (no extractable usage). Frame envelope matches the committed `grok/tui-updates.jsonl` shape. |

The codex integration test reads the real `../codex/interactive-0.154.0.jsonl`
directly (10 `token_usage_record` + 10 cumulative `token_count` snapshots).
