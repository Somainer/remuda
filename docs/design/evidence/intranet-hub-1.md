# Intranet Hub 1 — staged, awaiting Caddy restart approval

Status: **BLOCKED awaiting-caddy-restart-approval**. Target URL (redacted):
`https://remuda.<zone>`. Checked on 2026-09-13 Asia/Shanghai.

The latest coordinator instruction supersedes the earlier rollout instruction:
keep the Hub internally reachable, restore the active Caddy configuration, stage
a reviewable import/admin change, and wait for explicit restart approval.
**No restart or recreation of `deploy-caddy-1` was executed.** `apply` and
`rollback` below have not been executed against the live container.

## Executed and independently checked

| Check | Result |
| --- | --- |
| SSH | Kerberos/GSSAPI connection succeeded; no authentication bypass |
| Host OS | Debian 10 x86_64 observed, differing from the supplied Ubuntu description |
| Existing gateway | HTTPS `/health/ready` returned 200 before/after every Caddy change and at final staging |
| DNS | Created the dedicated DNS-only A record in the existing zone; Mac resolution matches the host's private IPv4; no public/tunnel route |
| DNS credential | Reused existing Caddy `CF_API_TOKEN` for the authorized DNS operation; value never printed or committed, never passed to Hub |
| Hub image | Imported static musl linux/amd64 image; version reports the commit below |
| Hub container | `remuda-intranet-remuda-hub-1` is healthy; project `remuda-intranet`; external network `deploy_default`; no published Hub ports |
| Internal routing | `docker exec deploy-caddy-1 wget -S -qO- http://remuda-hub:8080/healthz` returned HTTP 200 and `{"ok":true}` |
| Proxy configuration | Hub receives HTTPS public origin, Secure cookies enabled, and an exact JSON allowlist containing the inspected Caddy peer IP |
| Data | `/data00/remuda/hub` is 0700 and owned by the configured Compose runtime/operator user |
| Access code | `/data00/remuda/secrets/access-code` is 0600 and matches the private Hub bootstrap file; value not printed |
| Caddy baseline | Original Caddyfile restored byte-for-byte; `admin off` remains; no active Remuda import |
| Caddy lifecycle | Final container start time equals the initial inspection; no restart/recreation |
| Restart package | `prepare` executed successfully on the host; candidate passed `caddy validate`; active file unchanged |

Gateway readiness body SHA-256, unchanged throughout:
`753df501bf8ea6b776ec3e620d66f4b95d69345b546eb3ab2157df501bf4450e`.

Active Caddyfile baseline/final SHA-256:
`e8269d4937a623a37796d79b2e83ec947a724ca054839fac09a5eea2e37f4f6f`.

## Artifact identity

- Base main when this build started: `16f625402c88b654698969869315f670143c157b`.
- Built/runtime commit: `e3a41335513526e018e90815a6bfae6341f58ccd` (tested proxy and maintenance additions;
  subsequently merged into main). This is the deployed binary's exact identity,
  not a claim that it tracks later main updates.
- Image tag: `remuda-hub:e3a4133`.
- Image ID: `sha256:65e047927de0e48f08448933821039c6c8b7c12e706d5281807936902885977a`.
- musl binary SHA-256: `709679df608ce29813b87d52ed48612833ffbcdef191f59cb83165dcd8e15b4f`.
- Image tar SHA-256: `609c7b2421c5be66372bd763a988cc9909c4c8859b1ed548873c3bf9f641974d`.
- Staged restart script SHA-256: `d57c22f42c90880990b9248604a0589265013871327115ec01d595ca68ee9ea1`.

Build used web assets built inside `web/` with `pnpm install --frozen-lockfile`
and `pnpm build`, `cargo zigbuild --locked --release -p remuda --target
x86_64-unknown-linux-musl`, and `deploy/m1/Dockerfile` through the Colima buildx
builder with `--platform linux/amd64`. The binary is statically linked and stripped.
No real model invocation was used for validation.

## Staged files and approval boundary

In `~/astergate/deploy` on `<sg-host>`:

- `compose.hub.yml` plus private `.env.remuda-intranet`: running Hub only.
- `Caddyfile.d/remuda.caddy`: operator source for the dedicated DNS-01 site.
- Existing persistent volume copy: `/config/remuda/Caddyfile.d/remuda.caddy`.
- `remuda-caddy-change.py`: reviewed [apply/rollback implementation](../../../deploy/intranet/caddy-change.py).
- `.remuda-caddy-change.json` (0600): actual health URLs, baseline hashes,
  container start time and include hash; contains no DNS token.
- `Caddyfile.remuda-prepared`: validated candidate for review. It changes only
  global `admin off` to `admin localhost:2019` and adds
  `import /config/remuda/Caddyfile.d/*.caddy`; the gateway site block is unchanged.

`prepare` writes only staged files and validates the candidate; it does not edit
the active Caddyfile, reload or restart Caddy. The active import remains absent.
Configuration fragments contain an environment-variable reference, not the token.
Their 0644 mode allows the capability-restricted Caddy process to read the staged
operator-owned files. The settings, access code and secrets remain private.

After explicit approval, exact apply command:

```bash
ssh -o BatchMode=yes -o GSSAPIAuthentication=yes '<sg-host>' \
  'cd ~/astergate/deploy && python3 remuda-caddy-change.py apply --settings .remuda-caddy-change.json'
```

The approved apply saves `Caddyfile.pre-remuda-approved`, applies the reviewed
admin/import delta, restarts Caddy, then verifies the unchanged gateway readiness
response and certificate-verified Hub HTTPS health. It automatically restores the
baseline and restarts again if that attempt fails. Approval for apply therefore
includes this automatic rollback restart.

Explicit rollback command (also restarts Caddy; only after authorization):

```bash
ssh -o BatchMode=yes -o GSSAPIAuthentication=yes '<sg-host>' \
  'cd ~/astergate/deploy && python3 remuda-caddy-change.py rollback --settings .remuda-caddy-change.json'
```

Rollback restores the original admin option and removes the added import by
restoring the reviewed baseline. It preserves the Hub data, DNS record, staged
include, shared Docker network and Caddy volumes. Subsequent unrelated Caddyfile
edits cause refusal instead of being overwritten. A failed candidate leaving
Caddy stopped does not prevent restoration/restart of the baseline.

## Earlier attempts and recovery

1. Initial Hub startup failed because the image's default working directory was
   inaccessible to the operator UID. Adding `working_dir: /data` fixed it; only the
   new Hub container was recreated. Its health now passes.
2. `caddy reload` could not reach the admin API because the existing configuration
   has `admin off`. The attempted config change was restored. Gateway readiness
   stayed 200 with the same body hash.
3. Before the latest coordinator instruction arrived, a validated include was
   applied using Caddy's documented `SIGUSR1` reload for its verified file-based
   `caddy run` invocation. The process was not restarted. Staging permissions were
   corrected after validation rejected an unreadable fragment; those failures
   restored the baseline before proceeding.
4. On receipt of the staged-only instruction, the original Caddyfile was restored
   and reloaded through the same signal. Its exact hash and unchanged start time
   were verified, and the final gateway checks passed. The approval script's
   `prepare` then validated the inactive candidate without activating it.

Signal behavior was checked against [Caddy's official signal documentation](https://caddyserver.com/docs/command-line#signals).
No successful final external Hub HTTPS/browser/Node acceptance is claimed from
these transient probes.

## Validation and work deferred until approval

Passed: Rust build and Clippy (`-p remuda -p remuda-hub`, all-target Clippy with
`-D warnings`), crate tests (199 passing tests in the recorded full crate
run), focused maintenance retest, web build, Compose config checks, ShellCheck,
and the secret scan. Five restart-package tests cover the minimal candidate,
preparation without restart, refusal of subsequent operator edits, rollback when
a failed candidate leaves Caddy stopped, and recovery after a partial active-file
write. The live `prepare` path and internal Hub health were also checked.

Pending: explicit Caddy restart approval; final valid-certificate Hub HTTPS
acceptance; headless-browser login; Mac outbound Node enrollment and Hosts-page
verification; SG user-service Node installation and live inventory verification.
A private headless-browser acceptance script and Mac binary were prepared but
Node services and browser acceptance were not executed. The inspected daemon
installer was not merged at build time; the current carrier uses `--hub-url` and
persisted host credentials. Missing remote native CLIs/Herdr are an inventory
limitation, not proof of runtime execution capability.

The public VPS variant remains under `deploy/public`; its Compose and shell
checks passed. Its internal-CA CI smoke job was authored but not run locally after
the priority change. No public VPS or tunnel software was deployed.
