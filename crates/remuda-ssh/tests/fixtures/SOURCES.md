# Fixture sources

Captured or synthesized 2026-09-12. HostName octets after the second group
are redacted to `x.x` (same rule as `docs/research/remote-topology.md`).

| File | Source |
|---|---|
| `ssh-G-devbox-sg.txt` | `ssh -G devbox-sg` (fields `user`/`hostname`/`port`/`identityfile` only; extra OpenSSH keys omitted). HostName `10.199.x.x`. |
| `ssh-G-forge-doloris.txt` | `ssh -G forge-doloris` (`user`/`hostname`/`port`/`proxyjump`/`identityfile`). User comment stripped by the parser. HostName `10.102.x.x`. |
| `ssh-config.txt` | Synthetic `Host` table from `docs/research/remote-topology.md` §A.1 plus wildcard/`Include` cases. |
| `ssh-config.d/extra` | Included from `ssh-config.txt`; one extra alias. |
| `fake-node-stdio.py` | Local stdio peer for length-prefixed JSON (not captured from a live Node). |
