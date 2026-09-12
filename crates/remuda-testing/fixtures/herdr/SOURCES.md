# Herdr fixtures

| Path | Source |
|---|---|
| `session-ok.jsonl` | Live capture 2026-09-12, herdr 0.9.0, isolated session `remuda-test`, cwd `/tmp/remuda-herdr`. `agent.start` claude `--model haiku --dangerously-skip-permissions --max-budget-usd 0.3` then `agent.prompt` "Reply with exactly OK". Session UUID replaced with `00000000-0000-4000-8000-000000000001`. `terminal_id` stripped. Nested `pane_read` text shortened to contain `OK` (live capture showed the prompt box; screen-read of the model reply was empty — herdr-herdrx.md §4.1). |
| `events-ok.jsonl` | Shape from `docs/research/herdr-herdrx.md` §1.2 plus the working→idle pair fake-herdr emits on `agent.prompt`. |
| `session-trust.jsonl` | Authored from herdr-herdrx.md §3.2 `agent_not_ready` / trust dialog. |
| `terminal-observe.jsonl` | `pty-driver-spike.md` §4.3 `terminal.frame` envelope; `bytes` is base64 ANSI for `OK`. |

Do not point these tests at `~/.config/herdr/herdr.sock`.
