# Remuda deploy runbook (zero → phone HTTPS)

This is the M1-pre edge path: build Linux artifacts, put Hub behind the
existing Caddy on `devbox-sg-host`, publish that hostname through a
Cloudflare Tunnel, and run Node on `devbox-sg`. `remuda hub` / `remuda
node` are still bootstrap placeholders; this document is the wiring, not a
claim that the PWA already talks to a live Hub.

Do not install services or edit remote Caddy/compose while only running the
binary smoke in `/tmp/remuda-smoke/`.

## 0. Inventory (verified 2026-09-12)

| Role | SSH alias | OS | Kernel | libc | Notes |
| --- | --- | --- | --- | --- | --- |
| Hub host | `devbox-sg-host` | Debian 10 | 5.4.143.bsk.8-amd64 | glibc 2.28 | Docker 26.x, Caddy v2 on `:80`/`:443`, compose project `deploy` |
| First Node | `devbox-sg` | Ubuntu 20.04.5 | same kernel | glibc 2.31 | Container; systemd as PID 1; has docker of its own (not the host daemon) |
| CN canary | `devbox-small` | Debian 10 | same kernel | glibc 2.28 | glibc floor; GitHub egress is poor; Cloudflare 443 works |

Docker network on the Hub host (read-only `docker network ls`):

```
NAME             DRIVER    SCOPE
deploy_default   bridge    local
```

That is the compose project `deploy` default network. Remuda Hub joins it as
an **external** network. Do not create a second `deploy_default`. Do not
publish Hub ports on the host.

AsterGate already occupies `/v1` on the existing HTTPS hostname. Remuda uses
a **new hostname** (`remuda.example.com` below — replace with a name in the
same DNS zone). Cookie domain and path stay isolated.

## 1. Cross-compile

On macOS, use Zig 0.13.x plus `cargo-zigbuild` (reproducible, no QEMU):

```bash
# zig 0.13 from https://ziglang.org/download/ on PATH
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl x86_64-unknown-linux-gnu
just linux-musl    # static; smoke default
just linux-gnu     # cargo-zigbuild target x86_64-unknown-linux-gnu.2.28
```

Outputs:

- `target/x86_64-unknown-linux-musl/release/remuda` — statically linked
- `target/x86_64-unknown-linux-gnu/release/remuda` — dynamic glibc, max
  `GLIBC_2.28`

### glibc 2.28

Debian 10 is the Node floor (`plan-phase0.md` §13.6). Compatibility is not
the target triple; it is “the binary runs on that host”.

1. Prefer musl static. Promotion requires `file` = statically linked, `ldd`
   = not a dynamic executable, and a real `./remuda version` on the host.
2. Fallback: `cargo zigbuild --target x86_64-unknown-linux-gnu.2.28`. Check
   `readelf --dyn-syms` for no `GLIBC_2.29` or newer, then run the binary on
   Debian 10.
3. Hub images use bookworm/distroless and are **not** the Node glibc story.

This session: musl static ran on Ubuntu 20.04 (glibc 2.31) and Debian 10
(glibc 2.28). The gnu.2.28 artifact ran on Debian 10; dyn-syms were
`GLIBC_2.2.5` … `GLIBC_2.28` (highest 2.28). No OpenSSL/libgcc on the
current clap-only binary; later `rusqlite` bundled SQLite must re-run this
gate.

## 2. Hub image

From the repo root (after pinning `FROM` digests — see `deploy/Dockerfile`):

```bash
docker build -f deploy/Dockerfile \
  --build-arg REMUDA_GIT_SHA="$(git rev-parse HEAD)" \
  -t ghcr.io/somainer/remuda:0.1.0 .
```

Stages: pnpm `web/` → `cargo build --locked --release --bin remuda` →
`gcr.io/distroless/cc-debian12:nonroot`. Runs as UID 65532. Distroless has
no shell; do not add `HEALTHCHECK CMD-SHELL`. Probe HTTP/`remuda doctor`
once those exist.

Tag-triggered CI draft: `.github/workflows/release.yml` (musl artifact +
push to `ghcr.io/somainer/remuda`). No extra registry secret; GHCR uses
`GITHUB_TOKEN`. This workflow has not been executed here.

## 3. Secrets (create before compose up)

All files mode `0400`, directory `0700`, owner root or the compose operator.
Never put values in git, image layers, Caddy env that Hub can read, or
argv.

| Secret | Path on Hub host | Consumer | How to inject |
| --- | --- | --- | --- |
| Hub master key | `/data00/remuda/secrets/master-key` | Hub, migrate | `umask 077; openssl rand -out … 32` |
| Web password | `/data00/remuda/secrets/web-password` | Hub auth | operator-chosen; Hub stores a KDF, not this file’s echo in logs |
| Lark app secret | `/data00/remuda/secrets/lark-app-secret` | Hub dispatcher (M2) | from the Feishu app; optional until M2 |
| Node token | `/etc/remuda/secrets/node-token` on the Node | that Node only | Hub enroll returns it once; 256-bit |
| Cloudflare DNS token | existing Caddy env `CF_API_TOKEN` | Caddy only | already present; **do not** copy into Remuda |
| Tunnel credential | `/etc/cloudflared/<uuid>.json` | cloudflared only | `cloudflared tunnel create remuda` |

Compose maps the first three as Docker secrets (`*_FILE` env). Data dir
`/data00/remuda/hub` must be writable by UID 65532, mode `0700`:

```bash
sudo mkdir -p /data00/remuda/hub /data00/remuda/secrets
sudo chown 65532:65532 /data00/remuda/hub
sudo chmod 0700 /data00/remuda/hub /data00/remuda/secrets
```

Rotate: write a new file, `compose up -d` (or restart the migrate+hub pair),
keep the previous master key readable until dual-key read is implemented.
Tunnel JSON leak → rotate the tunnel; it is independent of the Hub master
key.

## 4. Compose + Caddy + Tunnel

On `devbox-sg-host`, from a checkout of `deploy/`:

```bash
# 1. Placeholder hub.toml until remuda hub reads config (create empty ok).
install -m 0644 /dev/null ./hub.toml

# 2. Join existing network; no ports.
docker compose -f compose.hub.yml config
docker compose -f compose.hub.yml up -d
```

Append `deploy/Caddyfile.snippet` to `~/astergate/deploy/Caddyfile` as a
**new site block**. Reload Caddy only (`docker exec deploy-caddy-1 caddy
reload --config /etc/caddy/Caddyfile`). Do not change the AsterGate site.

Follow `deploy/cloudflared.md`: named tunnel, origin `https://127.0.0.1:443`
with `originServerName remuda.example.com`, catch-all `http_status:404`.
CNAME the hostname to `<tunnel-id>.cfargotunnel.com`. Do not publish a
public A record for the private Hub IP. Do not open extra inbound ports.

Phone check: cellular network (not corp VPN) loads `https://remuda.example.com`,
WebSocket upgrade works, SSE does not buffer. Cloudflare 200 is not Hub
health.

## 5. Node

Same UID as Herdr (`remuda-agent`). Binary from the musl artifact unless
musl fails that host.

- systemd host: `deploy/node.service` (requires a matching Herdr unit from
  `plan-phase0.md` §13.4). `ProtectHome=read-only`; workspace whitelist
  replaces `/workspaces`.
- No systemd (or a canary in a container): `deploy/node-nohup.sh start`.
  One PID file; `stop` to clean. Not a production supervisor.

Node only dials outbound WSS to the Hub hostname. Production Node has no
listener.

## 6. Smoke results (redacted)

Cross-compile host: macOS `aarch64-apple-darwin`, rustc 1.94.1,
zig 0.13.0, cargo-zigbuild 0.23.4. Artifact SHA-256 is of the file copied
to `/tmp/remuda-smoke/remuda`. Remote files deleted after the run.

| Host | OS / kernel / libc | Artifact | `./remuda version` | `./remuda dev --help` | Cleanup |
| --- | --- | --- | --- | --- | --- |
| `devbox-sg` | Ubuntu 20.04.5 / 5.4.143.bsk.8-amd64 / glibc 2.31 | musl static, ELF x86-64, `ldd`: not a dynamic executable, sha256 `86a4781998a0e12551d3c09a78d2a4b3431d9f2e143187733f755d2912151fd3` | `remuda 0.1.0` `target=x86_64-unknown-linux-musl` `wire=1` `schema=0` | clap help for `dev` | `/tmp/remuda-smoke` removed |
| `devbox-small` | Debian 10 / 5.4.143.bsk.8-amd64 / glibc 2.28 | same musl binary / same digest | same | same | removed |
| `devbox-small` (gnu gate) | Debian 10 / glibc 2.28 | `x86_64-unknown-linux-gnu.2.28` PIE, `ldd`: libpthread/libc/libdl, dyn-syms highest `GLIBC_2.28`, sha256 `29c911261197c90109743f24b168f24f974db529d7c6a78c43c56a0e46d3692d` | `target=x86_64-unknown-linux-gnu` | same | removed |

`devbox-sg-host` was used only for `docker network ls` (confirmed
`deploy_default`). No binary was left there. No compose/Caddy/cloudflared
process was started in this smoke.

Commit in the version line was `b5f8c97…-dirty` because the tree had
unrelated in-progress crates at build time; release builds must pass
`REMUDA_GIT_SHA` from a clean tag.

## 7. Rollback

1. Stop taking new Hub work; Nodes keep local journals.
2. `docker compose -f compose.hub.yml down` (Remuda services only).
3. Revert the Remuda Caddy site block; reload Caddy. Leave AsterGate as-is.
4. Point the Tunnel hostname away or delete only the Remuda ingress rule.
5. Restore `/data00/remuda/hub` from backup into a **new** directory; never
   overlay a live DB.

## 8. Still out of scope for this wiring

- Real `remuda hub` listen / auth / migrate (M1).
- Node enrollment and outbound WSS (M1).
- CN as a production Node (egress and Cloudflare reachability differ by
  host; `devbox` CN cannot reach Cloudflare).
- Pinning container `FROM` digests (M3-05 / `scripts/ci/container.sh`).
