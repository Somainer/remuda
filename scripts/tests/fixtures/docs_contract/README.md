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

Commands that exist: `remuda demo run --fast`, `remuda demo run --quick`,
`remuda --data-dir D demo list`, `just build`, `just check`,
`pnpm --dir web test`.

Commands that do not: `remuda demo nope`, `remuda demo run --slow`,
`just nope`, `pnpm --dir web nope`.
