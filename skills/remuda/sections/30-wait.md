## wait conditions

| `--until` | Met when |
| --- | --- |
| `line:<regex>` | A journal/screen line matches (see matcher below) |
| `idle` | Ready for input — **not** Hub lifecycle `requested` |
| `blocked` | Approval prompt, question, or interaction event |
| `done` | Run terminal, or lifecycle `closed`/`failed`/`terminated` |

`--timeout` is **milliseconds**: default 30000, hard max 300000 (5 min). A
longer job needs a wait loop, not a bigger number.

**The matcher is bullet-tolerant.** TUIs render output as `• DONE`, so before
applying your regex the matcher strips leading whitespace and *one* list
marker (`•`, `●`, `◆`, `▸`, `▪`, `-`, `*`, `>`). `line:(?m)^DONE` therefore
matches `• DONE` and indented `DONE`. `matchedLine` reports the raw line.
It also ignores the brief's own echo (lines containing `for example`,
`further input`, `tui bullet`) so your instructions can't satisfy the wait.
