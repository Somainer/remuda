# remuda-ssh

OpenSSH transport for Remuda nodes. The crate shells out to the system `ssh`
binary so `~/.ssh/config` aliases, ProxyJump, and ssh-agent behave the same as
an interactive login. Do not replace this with russh.

## Frame alignment with remuda-node

`remuda-node` `StdioCarrier` speaks **NDJSON** (`CarrierKind::StdioNdjson`,
`node.hello` first line). This crate's [`NodeTransport`] matches that:

| Carrier | Framing |
|---|---|
| SSH stdio (`ssh <alias> -- <bin> node --stdio`) | one UTF-8 JSON line, cap 1 MiB (`protocol.md` §7.4 `maxJsonFrameBytes`) |
| WSS | one WebSocket text (or binary) message per JSON value |

If the remote binary is older than `node --stdio`, `remuda ssh node` falls
back to `remuda version` and prints a `version-fallback` object.

## Commands (via `remuda`)

```
remuda ssh list
remuda ssh probe <alias>
remuda ssh bootstrap <alias> [--local target/x86_64-unknown-linux-musl/release/remuda] [--remote ~/.local/bin/remuda]
remuda ssh node <alias>
```

`bootstrap` skips the copy when the remote sha256 already matches. Production
default dest is `~/.local/bin/remuda`. Live tests write only
`/tmp/remuda-ssh-test/` and delete it afterwards.

SSH flags always include `-o BatchMode=yes -o ServerAliveInterval=15
-o ServerAliveCountMax=3`. ControlMaster is optional:
`-o ControlMaster=auto -o ControlPath=<runtime dir>/cm-%C -o ControlPersist=60`.

## Tests

```
cargo test -p remuda-ssh
cargo test -p remuda-ssh -- --ignored --nocapture   # devbox-sg, once
```

The ignored test probes `devbox-sg`, bootstraps the musl binary into
`/tmp/remuda-ssh-test/`, reads `node --stdio` `node.hello` (or `version` if
the artifact is older), and removes that directory.
