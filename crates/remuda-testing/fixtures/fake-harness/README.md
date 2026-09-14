# fake-harness fixtures

Files used by `fake-harness` (D-028 §12 item 7 / P7) and its integration tests
(`crates/remuda-testing/tests/fake_harness.rs`).

## `scenarios/`

Declarative turn scripts (JSON or YAML). The schema is documented in
`docs/design/testing-fake-harness.md`; fields:

| file | purpose |
|---|---|
| `ok.json` | thinking + streamed text, no tools; quits after one turn |
| `approval.json` | one `approval: ask` Bash tool with the native dialog |
| `slow.json` | two sequential long tools; used by steer/queue/interrupt tests |
| `hooks.json` | `approval: hook` and `hook_required` turns |
| `demo.yaml` | YAML-format example (same schema) |

## `hooks/`

| path | purpose |
|---|---|
| `hook.sh` | tiny POSIX shell hook: logs stdin + identity env to `$FAKE_HARNESS_HOOK_LOG`; blocking permission decision driven by `FAKE_HARNESS_HOOK_DECISION` / `FAKE_HARNESS_HOOK_WAIT_FILE` / `FAKE_HARNESS_HOOK_STYLE` (`claude` \| `codex` \| `grok`) |
| `claude-settings.json` | Claude `--settings` overlay registering every event |
| `codex-hooks.json` | codex `$CODEX_HOME/hooks.json` shape |
| `grok-hooks/probe.json` | grok `$GROK_HOME/hooks/*.json` (includes a deliberately ignored `PermissionRequest`, like the real binary) |

The command strings reference `$FAKE_HARNESS_FIXTURE_DIR`; tests export it
pointed at this `hooks/` directory before spawning the binary.

## `golden/`

Checked-in plaintext screen renderings produced by
`fake_harness::screen::render_grid`, named `<kind>-<state>-<cols>x<rows>.txt`.
Regenerate with `UPDATE_GOLDEN=1 cargo test -p remuda-testing --test fake_harness`.
These are deterministic grids (trailing spaces significant); they are not raw
PTY captures — the binary emits them over cursor positioning plus OSC/DECSET.
