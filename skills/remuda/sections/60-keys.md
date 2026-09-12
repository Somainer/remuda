## keys

```bash
remuda instance keys reviewer esc
remuda instance keys reviewer down enter
```

Names are validated before any byte is written: `enter`/`return`, `tab`,
`esc`, `space`, `backspace`, `delete`, `up`/`down`/`left`/`right`, `home`,
`end`, `ctrl+<letter>`, or a single character. An unknown name fails the
whole call. Requires a tty-attach driver.
