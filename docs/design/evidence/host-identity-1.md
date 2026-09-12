# Host identity 1

Date: 2026-09-13. Isolated `remuda dev` on hub `127.0.0.1:48080` and node `127.0.0.1:48787` (not the live demo ports). Cookie Secure off. Access code was a local test file (value not recorded). Data dir was a throwaway worktree folder (not committed).

Screenshot: [host-identity-1-hosts.png](./host-identity-1-hosts.png) (Hosts page after two process restarts).

## Restart

`remuda dev` was started, then stopped and started twice against the same data dir. Hub `/v1/hosts` and the Hosts page showed **one** `local-development` row staying online. The persisted `hostId` did not change:

`hst_01a09691-44bb-75cd-a617-a4a87b67fa45`

`enrollment.json` in the Node data dir kept that `hostId` plus a host token (token omitted here). A cold start takes ~15s because inventory probes `claude`/`codex`/`grok`/`agy` `--version` before `node.hello`.

## Inventory

`GET /v1/hosts` after the second restart returned one item with stable `hostId`/`id`, `online: true`, `transport: outbound-wss`, and installed CLI inventory (paths redacted):

- claude 2.1.269
- codex-cli 0.154.0
- grok 1.0.30
- agy 1.2.2

No duplicate offline `local-development` rows.

## UI

The Hosts page lists that single online host first, with the four CLI versions on the row. Stale offline hosts older than 30 minutes are hidden behind a “显示过期” toggle (not needed on this clean data dir). The new-session host picker uses the same online-first order and shows the host’s CLI summary instead of a hardcoded Claude version.

## Tests

- Hub: `bootstrap_reannounce_updates_the_same_host`, `duplicate_label_hosts_merge_on_reopen`, `hello_reannounce_same_host_id_updates_one_row`
- Node: `wss_reannounce_keeps_a_single_host_row`, `compose_reuses_the_persisted_host_identity`, inventory probes for claude/codex/grok/agy
- CLI: `development_enrollment_reuses_the_same_host_id`
