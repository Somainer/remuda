# Intranet Hub behind the existing Caddy

Status: **HTTPS active; Hub `9dd7ec7`; persistent Mac Node online with the real
repository workspace. Shell and Claude PTY PONG/reply/stop acceptance passed;
workspace lookup and the earlier outbound WSS durable replay are verified.**
The user granted the Mac daemon's filesystem permission manually before the
second acceptance run; this run made no System Settings or TCC changes. See
[intranet-acceptance-2.md](../../docs/design/evidence/intranet-acceptance-2.md)
for the current workspace and PTY evidence,
[intranet-enroll-1.md](../../docs/design/evidence/intranet-enroll-1.md) for the
Hub upgrade, pairing, Node isolation and earlier blocked attempts, and
[intranet-hub-1.md](../../docs/design/evidence/intranet-hub-1.md) for approved
Caddy activation. No SG/bolt Node installation has been performed by these runs.

This is the only supported [D-031](../../docs/design/decisions.md) deployment
path. Run Hub on the SG host, join its existing `deploy_default` network,
and add one dedicated `remuda.<zone>` site to the existing Caddy. Use the
existing Cloudflare DNS-01 setup. The A record is private; clients need an
approved intranet route. Certificates do not provide public reachability.
公网暴露：待定；禁止隧道工具（D-031）。The historical public VPS assets remain in
[public](../public/README.md); they do not authorize public deployment.

The detailed sequence, acceptance checks and rollback are in the
[deploy runbook](../../docs/design/deploy-runbook.md). Actual execution
evidence belongs in [intranet-hub-1.md](../../docs/design/evidence/intranet-hub-1.md).

The inspected SG host is Debian 10 x86_64. Use that observed platform for
binary compatibility; the public package's Ubuntu installer is not used
here. Existing Caddy has named volumes at `/config` and `/data`, with a
separate bind mount for its main Caddyfile.

Prepare a private operator `.env` with an explicit `HUB_IMAGE` tag,
`HUB_DOMAIN=remuda.<zone>`, the chosen `DATA_DIR`, its actual
`HUB_UID` / `HUB_GID`, and `REMUDA_TRUSTED_PROXIES` as a JSON array containing only
the existing Caddy container's verified network IP. The default UID/GID
are 65532; adapt these to the host's verified mount ownership. Create the
data directory `/data00/remuda/hub` mode 0700, owned by the chosen Compose
runtime user, and preserve any existing database/bootstrap.
Do not commit real hostnames, addresses or credentials.

The preparation check is read-only with respect to running services:

```bash
docker compose --env-file .env -p remuda-intranet -f compose.hub.yml config
```

Hub publishes no host ports and uses the unique network alias
`remuda-intranet-hub` as Caddy's upstream. The external network must already exist;
do not create a replacement or recreate the gateway stack. Instantiate
`Caddyfile.snippet` as a new site include and verify its DNS provider
credential environment variable against the existing Caddy setup. Back up
the current Caddyfile. Keep the inactive operator source at
`~/astergate/deploy/Caddyfile.d/remuda.caddy`. Preparation copies the inactive
include to `/config/remuda/Caddyfile.d/remuda.caddy` inside Caddy's existing persistent
`/config` named volume; retaining that volume preserves the include across
container recreation. Do not add the active import before approval.

Stage [caddy-change.py](caddy-change.py) on the host as
`~/astergate/deploy/remuda-caddy-change.py`. Its private mode-0600 settings
file `.remuda-caddy-change.json` contains `gateway_health_url`,
`hub_health_url`, `baseline_caddy_sha256`, `baseline_started_at`,
`include_sha256` and `gateway_health_sha256` from the reviewed baseline;
it never contains the DNS token.

```bash
cd "$HOME/astergate/deploy"
python3 remuda-caddy-change.py prepare --settings .remuda-caddy-change.json
```

`prepare` verifies baseline hashes, Caddy start time and gateway readiness,
writes `Caddyfile.remuda-prepared`, copies the candidate and inactive
include into `/config`, validates with Caddy, then checks gateway health
again. It leaves the active Caddyfile unchanged and performs no reload or
restart. Preparation does not authorize activation.

After explicit approval, `apply` repeats those checks, saves
`Caddyfile.pre-remuda-approved` and recovery state,
installs the dedicated include, adds
`import /config/remuda/Caddyfile.d/*.caddy` only if absent, and changes the
global admin setting from `off` to `localhost:2019`. Keep that endpoint on
container loopback without publishing host port 2019. The gateway site and
its `/v1` route remain unchanged. The script validates the candidate config,
then runs `docker restart deploy-caddy-1` and checks the gateway response
hash plus Remuda HTTPS `/healthz` with certificate verification. That restart
may interrupt connections served by Caddy and requires explicit approval.
Preparation does not run it. The approved activation and earlier API/SIGUSR1
attempts are recorded separately in the evidence file.

The following is for use **after approval**, not during preparation:

```bash
python3 remuda-caddy-change.py apply --settings .remuda-caddy-change.json
```

Approval for `apply` includes its automatic recovery: if restart or health
verification fails after the active file changes, the script restores the
original file and restarts Caddy again, then checks gateway health. No
additional approval is requested in that failure path.

After activation is approved and verified, open
`https://remuda.<zone>/login` from the intranet. The operator stores
the first-login access code at `/data00/remuda/secrets/access-code`, mode
0600; read it privately to log in. A logged-in browser can
generate a phone pairing code in Settings; the phone uses `/login?pair`.
Subsequent Node acceptance is pending. Follow [NODES.md](../public/NODES.md)
for the outbound carrier and D-019 daemon installer, which requires a compatible
Hub upgrade. Store provider gateway configuration on the
Node that reaches it; the Hub does not proxy provider requests. The inspected
remote PATH lacks native agent CLIs and Herdr. Node enrollment can report
these missing tools; an online Node does not establish session execution
readiness.

The prepared `rollback` path restores the original global admin setting
(pre-activation `admin off`) and import state, deactivating the Remuda
include, then restarts `deploy-caddy-1` and checks gateway health. It verifies
the saved baseline hash before restoring it, allowing recovery even if Caddy
is stopped.
Rollback also contains a restart and must not run before approval. Preserve
Hub data, the `/config` and certificate volumes, and the shared
network. The standalone public VPS scripts are not intended to manage this
existing Caddy deployment.

Keep this explicit rollback command for approved recovery; it was not needed
during the successful activation:

```bash
python3 remuda-caddy-change.py rollback --settings .remuda-caddy-change.json
```
