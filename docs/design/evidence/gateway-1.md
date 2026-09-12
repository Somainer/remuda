# Gateway 1

Date: 2026-09-13. Isolated `remuda dev` on hub `127.0.0.1:58080` and node `127.0.0.1:58787` (not the live demo ports). Cookie Secure off. Access code was a local test file (value not recorded). Data dir was a throwaway folder (not committed). Overlay path was `~/.claude/settings.relay.json` (contents not logged).

Screenshots:

- [gateway-1-gateway.png](./gateway-1-gateway.png) — New Session `kind=claude`, `driver=claude-print`, Provider=gateway, permission=bypassPermissions, overlay `~/.claude/settings.relay.json`
- [gateway-1-none.png](./gateway-1-none.png) — same form with Provider=none and inherited host login (`haiku`, `--max-budget-usd 0.3`)

## Placement

`GET /v1/hosts` showed one online `local-development` row with label `egress=gateway`. `remuda dev` now advertises that label by default, so gateway placement no longer fails with `placement unsatisfiable`.

## Gateway

Headless Chrome against the Hub web UI logged in, opened New Session, selected gateway + yolo, set the overlay path, and sent `你好，你是什么模型`.

The Session header showed `claude-print · gateway · gateway · running` (delegation and providerProfileId persisted on `GET /v1/instances`). The assistant replied in Chinese identifying itself as Claude / Claude Code. Journal usage was non-zero (in 2 / out 83). This is not the previous `Not logged in · Please run /login` failure.

A first attempt used the form default model `passthrough/auto` and the gateway returned `400 requested model is not available`. The successful run used the overlay's configured model.

## None

The same UI path with Provider=none, no overlay, model `haiku`, budget `0.3` inherited the host Claude login. The Session header showed `none · none`. The assistant replied as Claude Haiku 4.5 (`claude-haiku-4-5-20251001`) with non-zero usage (in 10 / out 206).

## Tests

- Driver: `gateway_overlay_config_dir_and_budget_are_emitted`, `missing_overlay_fails_closed_without_logging_contents`, `pty_and_bg_recipes_accept_user_overlay`
- Node: `wss_create_preserves_gateway_delegation_overlay_and_budget` (fake-claude), `create_params_preserve_gateway_overlay_config_dir_and_budget`
- Hub: `create_instance_persists_delegation_and_provider_profile`
- CLI: `remuda_dev_help_lists_label`, default `egress=gateway` on `remuda dev`
