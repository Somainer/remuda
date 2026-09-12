# M1 deploy package (prepared locally, operator-run)

> **Superseded by D-020.** The intranet Hub + Tunnel rollout below is not
> applicable and must not be executed. Use [deploy/public](../public/README.md)
> for the retained public VPS variant; the current supported path is the
> [intranet runbook](../../docs/design/deploy-runbook.md). The musl artifact,
> `Dockerfile` and separate Hub/Caddy building blocks remain reusable; build web and the
> binary before copying the binary into the image context.

This directory is a **package**, not a rollout. Nothing here starts services
on `devbox-sg-host` or `devbox-sg`. Artifacts are built on the
operator laptop; copy-paste the blocks below on the hosts when you choose.

Placeholders: `remuda.example.com` (public Hub hostname), `<SHA>` (git
commit used for the image/binary). Replace both before running.

Artifacts (gitignored binaries; checksums in `deploy/out/SHA256SUMS`):

- `deploy/out/remuda-hub-<SHA>.tar` — `linux/amd64` Hub image, tag `remuda-hub:m1`
- `deploy/out/remuda-linux-musl-<SHA>` — static Node binary for the SG host

Build them from the repository root (macOS):

```bash
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
SHA=$(git rev-parse --short=12 HEAD)
mkdir -p deploy/out

# Web assets (must run inside web/ so .npmrc pins registry.npmjs.org)
( cd web && pnpm install --frozen-lockfile && pnpm build )

# Hub image from that musl binary (avoids qemu rustc/cc segfaults)
cp "${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/remuda" \
  deploy/out/remuda-linux-musl
cp NOTICE LICENSE deploy/out/
docker buildx build --builder colima --platform linux/amd64 \
  -f deploy/m1/Dockerfile \
  -t "remuda-hub:${SHA}" -t remuda-hub:m1 \
  --output "type=docker,dest=deploy/out/remuda-hub-${SHA}.tar" \
  deploy/out

# Node musl binary
just linux-musl
cp "${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/remuda" \
  "deploy/out/remuda-linux-musl-${SHA}"
chmod +x "deploy/out/remuda-linux-musl-${SHA}"
( cd deploy/out && shasum -a 256 remuda-hub-${SHA}.tar remuda-linux-musl-${SHA} > SHA256SUMS )
```

Read-only preflight (this laptop → SSH):

```bash
./deploy/m1/preflight.sh
```

---

## A. Hub host (`devbox-sg-host`)

SSH as the compose operator. Work in `~/astergate/deploy` so Remuda joins
the existing `deploy` project network `deploy_default`.

### A1. Load the image

```bash
scp -o BatchMode=yes deploy/out/remuda-hub-<SHA>.tar devbox-sg-host:/tmp/remuda-hub-<SHA>.tar
ssh -o BatchMode=yes devbox-sg-host 'docker load -i /tmp/remuda-hub-<SHA>.tar && docker tag remuda-hub:<SHA> remuda-hub:m1 && rm -f /tmp/remuda-hub-<SHA>.tar && docker image inspect remuda-hub:m1 --format "{{.Os}}/{{.Architecture}} {{.Id}}"'
```

Expect `linux/amd64`. Do not `docker run` yet.

### A2. Data dir + compose files

```bash
ssh -o BatchMode=yes devbox-sg-host 'sudo mkdir -p /data00/remuda/hub /data00/remuda/secrets ~/astergate/deploy/Caddyfile.d
sudo chown 65532:65532 /data00/remuda/hub
sudo chmod 0700 /data00/remuda/hub /data00/remuda/secrets'
scp -o BatchMode=yes deploy/compose.hub.yml devbox-sg-host:~/astergate/deploy/compose.hub.yml
scp -o BatchMode=yes deploy/m1/hub.toml.example devbox-sg-host:~/astergate/deploy/hub.toml
```

Edit `hub.toml` `allowedOrigins` to `https://remuda.example.com`.

```bash
ssh -o BatchMode=yes devbox-sg-host 'cd ~/astergate/deploy && docker compose -f compose.hub.yml config'
ssh -o BatchMode=yes devbox-sg-host 'cd ~/astergate/deploy && docker compose -f compose.hub.yml up -d'
```

No `ports:` on the Hub service. Caddy on `deploy_default` reaches `remuda-hub:8080`.

### A3. Caddy include (do not hand-edit the AsterGate site)

```bash
scp -o BatchMode=yes deploy/Caddyfile.snippet devbox-sg-host:~/astergate/deploy/Caddyfile.d/remuda.caddy
```

On the host, replace `remuda.example.com` in that include. If the live
Caddyfile does **not** already import extras, add **one** line only:

```
import Caddyfile.d/*.caddy
```

Reload Caddy, do not recreate it:

```bash
ssh -o BatchMode=yes devbox-sg-host 'docker exec deploy-caddy-1 caddy reload --config /etc/caddy/Caddyfile'
```

### A4. cloudflared token → local Caddy :443

No inbound security-group change. Configure the public hostname in the
Cloudflare Zero Trust dashboard (token-based tunnels do not need a local
`config.yml`):

- Public hostname: `remuda.example.com`
- Service: `https://127.0.0.1:443`
- Origin TLS: SNI / HTTP Host `remuda.example.com`
- Catch-all remains 404

On a trusted laptop:

```bash
cloudflared tunnel create remuda-m1
cloudflared tunnel token remuda-m1 > remuda-m1.token
chmod 0400 remuda-m1.token
scp -o BatchMode=yes remuda-m1.token devbox-sg-host:/tmp/remuda-m1.token
```

On the Hub host:

```bash
sudo mkdir -p /etc/cloudflared
sudo install -m 0400 /tmp/remuda-m1.token /etc/cloudflared/remuda-m1.token
rm -f /tmp/remuda-m1.token
sudo install -m 0644 ~/path/to/checkout/deploy/m1/cloudflared.service /etc/systemd/system/cloudflared-remuda.service
# if cloudflared is not at /usr/local/bin/cloudflared, edit ExecStart
sudo systemctl daemon-reload
sudo systemctl enable --now cloudflared-remuda.service
```

CNAME `remuda.example.com` to the tunnel. Do not publish a public A record
for the private Hub IP.

### A5. Bootstrap token retrieval

Hub writes `/data/bootstrap-token` (0600) on first start when none is
configured. From the Hub host:

```bash
sudo cat /data00/remuda/hub/bootstrap-token
# or, if the container already mounted /data:
# docker exec remuda-hub cat /data/bootstrap-token   # distroless: use the host path
```

Use that value only as `REMUDA_BOOTSTRAP_TOKEN` / `--bootstrap-token` for
`POST /v1/login` and first Node enroll. Do not put it in compose YAML, Caddy,
or cloudflared env.

---

## B. SG Node (`devbox-sg`)

Outbound WSS only. No listener, no extra inbound ports.

### B1. Copy the musl binary

```bash
scp -o BatchMode=yes deploy/out/remuda-linux-musl-<SHA> devbox-sg:/tmp/remuda
ssh -o BatchMode=yes devbox-sg 'sudo mkdir -p /opt/remuda /etc/remuda/secrets /var/lib/remuda
sudo install -m 0755 /tmp/remuda /opt/remuda/remuda
rm -f /tmp/remuda
/opt/remuda/remuda version'
```

Expect `target=x86_64-unknown-linux-musl`.

### B2. Host token + unit

First enroll may present the Hub **bootstrap** secret; afterwards store the
host token only on the Node:

```bash
# on sg-host, copy bootstrap once (operator paste), then:
ssh -o BatchMode=yes devbox-sg 'sudo install -m 0400 /dev/stdin /etc/remuda/secrets/node-token'
# paste token, EOF

scp -o BatchMode=yes deploy/m1/node.toml.example devbox-sg:/tmp/node.toml
scp -o BatchMode=yes deploy/m1/node.service devbox-sg:/tmp/remuda-node.service
ssh -o BatchMode=yes devbox-sg 'sudo sed -i "s/remuda.example.com/remuda.example.com/" /tmp/node.toml /tmp/remuda-node.service
sudo install -m 0644 /tmp/node.toml /etc/remuda/node.toml
sudo install -m 0644 /tmp/remuda-node.service /etc/systemd/system/remuda-node.service
sudo systemctl daemon-reload
sudo systemctl enable --now remuda-node.service'
```

If systemd is absent, use `deploy/node-nohup.sh` with:

```bash
export REMUDA_BIN=/opt/remuda/remuda
export REMUDA_NODE_CONFIG=/etc/remuda/node.toml
export REMUDA_NODE_TOKEN_FILE=/etc/remuda/secrets/node-token
# node.toml must contain hub_url = "wss://remuda.example.com/v1/node"
./deploy/node-nohup.sh start
```

Equivalent argv (do not put the token on the command line):

```bash
/opt/remuda/remuda node \
  --hub-url wss://remuda.example.com/v1/node \
  --host-token-file /etc/remuda/secrets/node-token \
  --label region=sg
```

---

## C. What this package does not do

- Does not `compose up`, `systemctl start`, or `caddy reload` from the laptop.
- Does not open host ports or add Hub `ports:`.
- Does not touch the AsterGate server block or `/v1` on that hostname.
- Does not commit image tarballs or Node binaries.
