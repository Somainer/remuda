# Public VPS Hub package

This is the retained public VPS variant of [D-020](../../docs/design/deploy-public.md).
The current priority is the [intranet Hub runbook](../../docs/design/deploy-runbook.md).
The public variant places Hub and Caddy on a public VPS, with Nodes dialing
outbound WSS/HTTPS and phones reaching the Hub through HTTPS. The old intranet Hub + cloudflared
package is not applicable. The commands below are for a human on a fresh
Ubuntu 24.04 VPS; preparing this package does not deploy any service.

## Build the image

Use the static musl image approach from `deploy/m1/Dockerfile`. Build the
web assets **before** the binary so its embedded web files are present.
For a Linux amd64 VPS, from a clean, validated repository checkout with
pnpm, Zig and cargo-zigbuild installed:

```bash
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
export CARGO_INCREMENTAL=0
SHA=$(git rev-parse --short=12 HEAD)
(cd web && pnpm install --frozen-lockfile && pnpm build)
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --locked --release -p remuda --target x86_64-unknown-linux-musl
mkdir -p deploy/out
cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/remuda" deploy/out/remuda-linux-musl
cp NOTICE LICENSE deploy/out/
docker buildx build --platform linux/amd64 -f deploy/m1/Dockerfile \
  -t "remuda-hub:${SHA}" \
  --output "type=docker,dest=deploy/out/remuda-hub-${SHA}.tar" deploy/out
```

Use the corresponding target and image platform for another architecture.
Transfer the image archive and this directory by the operator's approved
file-transfer procedure. Alternatively, publish the image to your registry
and set `HUB_IMAGE` to its explicit tag; this package does not assume that
a project registry image has already been published.

## Prepare and install on the VPS

Create an A record for `<hub-host>` in `<zone>` pointing directly to the
public address assigned to `<vps>`. This preflight requires A-only DNS and
rejects AAAA records until dual-stack reachability is validated separately.
Open inbound TCP 80/443 in the VPS provider firewall; both ports must be
free locally. Caddy uses HTTP-01 for automatic HTTPS and redirects HTTP.
See [Caddy automatic HTTPS](https://caddyserver.com/docs/automatic-https).

```bash
cd deploy/public
cp .env.example .env
chmod 0600 .env
# Edit .env with the values described below.
sudo ./install.sh --image-archive '<path-to-image-archive>'
# With a registry image already configured, use: sudo ./install.sh
```

The `.env` format is plain, unquoted `KEY=VALUE`; it is parsed without
executing shell code. Set `HUB_DOMAIN`, `ACME_EMAIL` and `HUB_IMAGE` to real
operator values. `DATA_DIR` defaults to `/var/lib/remuda-public` and backs
the named Hub `/data` volume; install creates it as mode 0700 for UID 65532.
`VPS_PUBLIC_IP` may be omitted if a public IPv4 is directly assigned to a
local interface; for a VPS using provider NAT, set it from the provider
console. Every DNS A answer must match that address. Keep `.env` out of git.

`install.sh` installs Docker Engine and the Compose plugin using Docker's
Ubuntu repository, installs local prerequisites, runs `preflight.sh`,
writes a mode-0600 bootstrap access code at `$DATA_DIR/bootstrap-token`, and
starts the stack. Existing bootstrap files are preserved. Docker's setup
is described in [its Ubuntu installation documentation](https://docs.docker.com/engine/install/ubuntu/).
For a fresh VPS that already has Docker and prerequisites installed,
`sudo ./preflight.sh` performs the checks independently; it expects free
80/443 and is not a health check for a running stack.

The installer prints `https://<hub-host>/login` and
`https://<hub-host>/login?pair`, never the access code in a URL. Read the
bootstrap file privately to log in on the first browser, then generate a
pairing code from that authenticated browser's Settings page for the phone.
The phone uses the pairing URL and that code. An HTTP health response does
not prove the web bundle, authentication or pairing flow is working.

## Proxy and Node configuration

Caddy alone publishes 80/443. Hub has no published ports and uses a
dedicated private Compose network. `REMUDA_PUBLIC_ORIGIN` is the HTTPS Hub
origin; `REMUDA_TRUSTED_PROXIES` trusts only Caddy's configured network IP.
If `PROXY_SUBNET` / `PROXY_IP` conflicts with another Docker network, change
both together. Do not extend this trust to arbitrary private networks.

The Caddyfile provides HSTS, security headers, gzip/zstd and preserves API
paths under `/v1` and `/node/v1`. WebSocket upgrades at `/v1/follow` and
`/node/v1/connect` use Caddy's built-in support, with streaming flush and
bounded connection/response-header timeouts. See [Caddy reverse proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy).
DNS-01 is an optional manual extension: build a Caddy image with your DNS
provider module and give only Caddy the scoped zone credential. The stock
image and this package do not install a DNS plugin. Preserve the dedicated
origin, proxy trust and HTTPS cookie behavior when customizing Caddy.

Enroll the SG host, devboxes and laptops using [NODES.md](NODES.md), which
covers the D-019 installer and interim carrier. Provider gateways and
credentials belong on each Node; the public Hub does not reach into the
intranet to call a provider.

## Backup, upgrade and restore

Set `BACKUP_RECIPIENT` to an age public recipient before backing up or
upgrading. Keep its matching private identity off the VPS and verify that
the operator can decrypt backups. `BACKUP_DIR` defaults to
`/var/backups/remuda-public`.

```bash
sudo ./backup.sh
sudo ./upgrade.sh '<registry/image:explicit-tag>'
```

Backup briefly stops the Hub, takes a consistent SQLite `.backup`, packages
the remaining private data and `.env` into an age-encrypted `.tar.gz.age`
secrets envelope, then restarts the Hub if it was running. Treat the
encrypted archive as the database and matching secrets recovery unit;
retain a copy outside the VPS. Caddy's independent certificate/config
volumes are not the Hub secrets envelope and certificates can be reissued.

Upgrade pulls the new image, backs up, stops the Hub, runs the new image's
`remuda hub --migrate`, then recreates only the Hub service. A migration failure keeps
the Hub stopped for inspection. Old-image rollback is safe only when that
image supports the migrated schema. Otherwise decrypt the backup in a
private recovery location, restore into a **new** mode-0700 data directory,
restore its matching config/secrets and choose the matching old image.
The archive contains `data/` plus `deployment.env`; the secrets envelope is
`data/secrets/secrets.json` with its matching `data/secrets/master.key`.
Preserve the failed directory, verify ownership and database integrity,
then switch the named volume backing directory before starting the stack.
Do not overlay a live database or run `docker compose down -v` to upgrade.

CI's `deploy-public` job checks Compose, scripts and `/healthz` through an
internal-certificate Caddy stack. Public ACME issuance, firewall/DNS
reachability, phone pairing and real Node reconnects remain operator checks;
this public variant has not been deployed by the current intranet-first task.
