## Drivers and kinds

`--driver pty` (alias of `generic-pty`) attaches a real terminal: screen
reads, `keys`, and interactive TUIs all work. `--driver claude-print` (the
default) is headless — no PTY, so no screen and no keys. **For coordinator
work you almost always want `--driver pty`.**

Each kind launches with its yolo preset appended when absent:

| `--kind` | binary | yolo argv |
| --- | --- | --- |
| `claude` | `claude` | `--dangerously-skip-permissions` |
| `codex` | `codex` | `--dangerously-bypass-approvals-and-sandbox` |
| `grok` | `grok` | `--always-approve` |
| `agy` | `agy` | `--dangerously-skip-permissions` |
| `gemini` | `gemini` | `--yolo` |

Placement: `--host hst_…` **or** `--labels region=sg` (mutually exclusive);
neither means "any online host". Check `remuda instance list` for what exists.
