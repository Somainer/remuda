# Fixture documentation

A synthetic corpus for the docs-contract checker. It is deliberately small
and deliberately broken in known places.

- [the guide](docs/guide.md)
- [heading anchor](docs/guide.md#setup-steps)
- [Chinese note](<docs/设计 笔记.md>)
- [note in a directory with a space](<docs/space dir/notes.md>)
- [nested Chinese path](<docs/目录 空格/说明.md>)
- [design entry](docs/design/home.md)
- [missing document](docs/missing.md)
- [missing heading](docs/guide.md#no-such-heading)
- [notes with a title](<docs/space dir/notes.md> "Notes")
- [balanced parentheses](docs/a_(b).md)
- [the space note][space-note]
- [guide-ref]

A footnote marker.[^1]

[^1]: Footnotes are prose, not links.

[space-note]: <docs/space dir/notes.md>
[guide-ref]: docs/guide.md#setup-steps
[unused]: nowhere.md

Commands that exist: `remuda demo run --fast`, `remuda demo run --quick`,
`remuda demo run --type structured`, `remuda demo run --timeout-ms 5`,
`remuda demo --verbose run`, `remuda demo inspect`, `remuda help demo`,
`remuda --data-dir D demo list`, `just build`, `just check`, `just c`,
`just -f ci.just check`, `pnpm --dir web test`.

Commands that do not: `remuda demo nope`, `remuda demo run --slow`,
`just nope`, `pnpm --dir web nope`.
