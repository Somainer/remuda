# Fixture sources

These files are **synthetic**. They were not captured from a live `lark-cli event consume` session. Field names and types come from read-only schema queries on 2026-09-12 (`lark-cli` 1.0.76):

- `lark-cli event schema im.message.receive_v1 --json` (`jq_root_path: "."`)
- `lark-cli event schema card.action.trigger --json` (`jq_root_path: "."`)
- `lark-cli im +messages-send --help` / `+messages-reply --help` (`--idempotency-key`, `--reply-in-thread`, `--file`)

JSONL lines may be prefixed with `#` comments (skipped by tests).

| File | Notes |
| --- | --- |
| `im-message-*.jsonl` | Flattened IM events. `.content` is pre-rendered text (do not `fromjson`). |
| `im-message-second-prompt.jsonl` | Same p2p `session_key` as `im-message-p2p.jsonl`, new `message_id` (resume/send). |
| `card-action-*.jsonl` | `action_value` / `form_value` are JSON strings as in the resolved schema. |
| `interaction-*.json` | Adapted from `crates/remuda-protocol/tests/fixtures/interaction.json` (`protocol.md` §2.6 / §5.4). |
| `fake-lark-cli.sh` | Local test double. Does not contact Feishu or change `lark-cli` config. |
