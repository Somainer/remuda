# Intranet enrollment 1 — Hub upgraded, Mac daemon and shell verified

Status: **BLOCKED native-claude-sessionstart**. Hub upgrade, API pairing,
Mac daemon enrollment, shell prompt/reply and WSS durable replay passed. Claude
PTY never reached SessionStart readiness or produced a reply within the bounded
acceptance windows.
Target (redacted): `https://remuda.<zone>`. Live checks ran on 2026-09-13 from the
Mac on the intranet. This record distinguishes the initial compatibility blocker
from the later, separately approved Hub-only upgrade and Mac installation.

The worktree started on `wt/c-deploy/intranet-enroll` from fetched `origin/main`
at `c121ae33e02fa7699002c6f9394f5197729869a1`. The native Mac binary used for
enrollment is `2f1e58026cb1afb0f661f05a3d0fff09a3fb22e9`, including the Herdr
isolation fix below. The Hub upgrade used pinned main
`9dd7ec7b59cd43ea325c2bbe2404210ffe31ff2c`.

The existing local demo was not stopped or modified. No Node installation or
native CLI installation was performed on `<sg-host>` or `<bolt-host>`.

## Initial API pairing and compatibility blocker

The operator access code came from the existing private local deployment state.
Requests used certificate-verified HTTPS, `curl -q --noproxy '*'`, the exact
Hub Origin header and private cookie jars. Request bodies, response tokens,
pairing codes and cookie values were retained only in private operator files
(mode 0600 inside a mode-0700 directory). They are not part of this evidence.

| Request | Result |
| --- | --- |
| `HEAD /login` | HTTP 200; `text/html; charset=utf-8`; no browser used |
| `POST /v1/login` with access code and operator device name | HTTP 200; device credential and session cookie returned |
| `POST /v1/devices/pair-code` with operator cookie | HTTP 200; temporary pairing code returned |
| `POST /v1/devices/pair` in a separate cookie jar | HTTP 200; paired Mac operator device and session cookie returned |
| `GET /v1/devices` using only the paired cookie jar | HTTP 200; response contains the paired device ID |
| Repeat `POST /v1/devices/pair` with the consumed code | HTTP 401; code reuse rejected |
| `POST /v1/hosts/enroll-token` with the authenticated paired cookie | **HTTP 405**, no JSON enrollment token |

These initial checks ran at approximately 07:34:46 UTC / 15:34:46 Asia/Shanghai.
All seven HTTPS requests returned curl certificate verification result 0.
The login and pairing responses both set `remuda_device` with `Secure`,
`HttpOnly` and `SameSite=Strict`. The authenticated cookie session is verified;
this is API acceptance, not a browser/PWA interaction test.

## Approved Hub upgrade — completed

The initial Hub image, `remuda-hub:e3a4133`, ran commit
`e3a41335513526e018e90815a6bfae6341f58ccd`, which predates D-018 and its
`POST /v1/hosts/enroll-token` route. The initial HTTP 405 was therefore a real
compatibility blocker. The operator subsequently approved a Hub-only upgrade;
the access code was never substituted for a Node enrollment token.

The upgrade ran from 07:54:29 to 07:54:42 UTC. The new Hub became healthy and its
embedded version reported `0.1.0`, target `x86_64-unknown-linux-musl`, wire major
1, schema major 0 and the pinned commit below. Web assets were built with
`pnpm install --frozen-lockfile` and `pnpm build` inside `web/`; the static binary
was built with `cargo zigbuild --locked --release -p remuda --target
x86_64-unknown-linux-musl` and `REMUDA_GIT_SHA` pinned to that commit.

| Artifact | Verified identity |
| --- | --- |
| Build commit | `9dd7ec7b59cd43ea325c2bbe2404210ffe31ff2c` |
| Image tag | `remuda-hub:9dd7ec7` |
| musl binary SHA-256 | `fed2c8e0a96ce1695c3b121174258497c21e83eb06e871c909d053981a7e8b0a` |
| Image archive SHA-256 | `bce65157f6654bd27c1badceea0aee631228487b84cd12b389ce61acd8bd119d` |
| Image ID | `sha256:7330426f01ff8212d4ee9df49bf6524a6e986fda379941343318068935313fb6` |
| Pre-upgrade SQLite backup SHA-256 | `d9ea58a77bd1df8949312a34215b465c74bd1293b50777f15224c772a16b108d` |

The intended backup directory under the Hub's data-volume parent was unwritable
for the operator, so the upgrade backup was stored in a private directory under
operator `$HOME` (`$HOME/<hub-backup-dir>`). The migration added the
`enroll_tokens` table. The upgrade result verified unchanged Hub mounts and
environment, unchanged Compose configuration, unchanged Caddy container ID and
start time, unchanged Caddyfile and unchanged gateway response hash. This
upgrade did not restart or reconfigure Caddy.

Certificate-verified `https://remuda.<zone>/healthz` returned HTTP 200 with
`{"ok":true}`. This version has no HTTP build-info endpoint: the serving container's
`remuda version --json` provided the exact build commit, and the served `/` body
matched the pinned build's `web/dist/index.html` byte-for-byte (SHA-256
`cff68c2a3738244eb3e7074129ed028a2247332fd788e60d0935c50127a55ffb`).


## API pairing and enrollment after upgrade — completed

All nine post-upgrade requests returned curl exit 0 and TLS certificate
verification result 0. The preexisting paired cookie still authenticated
`GET /v1/devices` with HTTP 200; fresh login and pairing were then exercised.

| Request | Result |
| --- | --- |
| `HEAD /login` and `GET /` | HTTP 200 |
| `POST /v1/login` | HTTP 200; secure session cookie returned |
| `POST /v1/devices/pair-code` | HTTP 200; temporary code returned |
| `POST /v1/devices/pair` with a separate cookie jar | HTTP 200; paired device and cookie returned |
| `GET /v1/devices` using the new paired cookie | HTTP 200 |
| Repeat `POST /v1/devices/pair` with the consumed code | HTTP 401; reuse rejected |
| `POST /v1/hosts/enroll-token` using the paired cookie | HTTP 200; nonempty `token`, `enrollTokenId` and `expiresAt` returned |

Both fresh login and pairing cookies retained `Secure`, `HttpOnly` and
`SameSite=Strict`. Token values, cookie values and pairing codes remain in
private operator files only. These checks establish API pairing and enrollment
compatibility; no browser/PWA interaction is claimed.

## Mac daemon isolation fix

Source review found a shared default Herdr session name, `remuda-node`.
Daemon entry points already placed sockets below their data directory, but the
native default still exposed the shared name to Herdr's named-session shutdown
fallback. Other native entry points also defaulted to shared socket discovery.

`NativeDriverConfig::new` now defaults to sockets under `<node-data>/herdr`
and a stable `remuda-node-<digest>` session name derived from the data-directory
path. Distinct data directories therefore get distinct direct sockets and named
fallback sockets. An explicit `REMUDA_HERDR_SESSION` remains an operator override.
The newly installed Mac daemon uses the binary containing these defaults; the
existing demo was not restarted to adopt them.

The daemon's own Unix socket, lock, SQLite state, host identity and credentials
are already rooted in its data directory. The daemon does not publish a TCP
listener. The launchd user-agent label remains `com.remuda.node`; the installer
refuses to overwrite a unit belonging to another data directory. This guard and
the Herdr defaults must both be checked before installing on a host with existing
services.

The regression checks cover stable names, distinct directories, separation from
the legacy/default session, the explicit override, and two isolated fake Herdr
servers: sweeping one Node's orphan workspace leaves the other Node's workspace
and panes intact. These fixture tests are separate from live enrollment acceptance.

Read-only demo probes during this milestone returned HTTP 200 from its web root
on loopback port 18080 and HTTP 401 from the protected endpoint on port 18787.
The 401 is recorded as an authentication boundary, not a successful health probe.
No demo login, session creation, terminal input, service signal or data-directory
mutation was performed.

## Mac daemon installation and shell acceptance — completed

The native Mac binary reported commit
`2f1e58026cb1afb0f661f05a3d0fff09a3fb22e9`, target `aarch64-apple-darwin`, wire
major 1 and schema major 0. Its SHA-256 was
`2d6387340c21e30118a7d9e8867e0c213e69de5bb9580a59c829769a4aefa6d6`.
Installation used a stable private data directory and isolated workspace,
separate from the existing demo, through the D-019 interface:

```bash
remuda --data-dir "$HOME/.local/share/remuda-intranet-mac" node install \
  --launchd --hub 'https://remuda.<zone>' --enroll-token '<one-shot>'
```

The install exited 0. The daemon reported running in daemon mode, and its host
ID matched both the persisted `node/host-id` and the Hub host record. The Hub
reported that exact host online with transport `outbound-wss` and no last error.
The operator checked `node/host-token` and `node.sock` modes as 0600, absence of
the consumed `node/enroll-token`, and absence of the enrollment token from the
launchd plist. The host inventory reported Herdr 0.9.0 and installed Claude
2.1.270, Codex, Grok and Agy CLIs; inventory alone does not establish model
execution readiness.

The isolated `shell-pty` instance was created at 07:59:32 UTC. Its acceptance
summary captured 59 TTY frames / 861 bytes and verified the expected reply
marker while explicitly excluding the echoed command. A subsequent stop left
the instance lifecycle `exited` at 08:00:32 UTC with durable sequence 10. This
establishes a Hub-mediated shell prompt/reply and stop on the enrolled Mac.

The demo's two read-only HTTP results and body hashes matched the pre-install
baseline. No demo login, instance creation, terminal input, signal or data
mutation was performed as part of these checks.

## Claude PTY startup — blocked

Three native Claude attempts used `claude-pty`, model `haiku`, delegation `none`,
permission mode `default`, a maximum budget of USD 0.30 per attempt and the isolated
registered workspace. The first debug-binary attempt had a 70-second request
window, leaving only about 38 seconds after Claude became the foreground process;
that short window alone was inconclusive. A second allowed 180 seconds. The final
attempt used the optimized release binary and also allowed 180 seconds.

In the release attempt, Herdr detected a Claude foreground process at
08:22:58.667925 UTC. The process had `TERM=xterm-256color`, `COLORTERM=truecolor`,
its normal home directory and the Node-scoped XDG configuration directory. The
current launch produced neither `session-meta.json.raw` nor `session-meta.json`.
The Hub journal remained at sequence 8 with the user input queued; the TTY stream
contained the launch command but no Claude UI, assistant reply or error. The
acceptance capture saw 31 frames / 4,833 bytes and no expected reply marker. The
instance was stopped through the Hub and its final lifecycle was `exited`.

Two additional diagnostic creates requested `--debug-file` using separate-value
and equals syntax. Launch validation rejected them before Claude started (the
flag is not on the launch allowlist); they were stopped and are not model runs.
No allowlist or SessionStart readiness guard was bypassed. This evidence does not
identify the native startup cause or claim that authentication inventory proves
execution. Further investigation must establish why this Herdr-launched Claude
process does not reach the injected hook; queued API acceptance is insufficient.

The installed optimized Mac daemon remains online and usable for the verified
shell path. All acceptance instances are stopped. Caddy, the production gateway
site and the original local demo were not changed during this investigation.

## Outbound WSS interruption and durable replay — passed

A local bridge takeover on this daemon's mode-0600 `node.sock` revoked its WSS
controller. The real Hub observed this exact host offline. The daemon process,
its host identity and the isolated shell instance remained alive. The test
completed the local bridge hello without supplying credentials or durability
acknowledgements, then submitted the shell builtin `:` into the already verified
idle shell. This generated durable command/input events while the Hub was offline;
it is a local-controller command, not a claim that the disconnected Hub delivered
that command or that shell output is durable.

| Observation | Result |
| --- | --- |
| Hub journal baseline before offline input | sequence 7 |
| Hub journal while disconnected | still sequence 7 |
| Local SQLite journal after offline input | sequence 11; four new events at 8–11 |
| Local command result | settled, outcome completed |
| Full successful takeover interval | 84.486 seconds |
| After bridge EOF | same daemon PID and persisted host ID; online via outbound WSS |
| Hub journal after reconnect | sequence 11; all four offline sequence/event-ID pairs matched |
| Acceptance shell stop through Hub | lifecycle `exited` |

The replay comparison used the Node's read-only SQLite journal and the actual
Hub's `GET /v1/instances/<instance-id>/journal?afterSeq=7`. No synthetic bridge
journal frame was acknowledged as Hub durability. This proves recovery of events
past the Hub's prior watermark; no raw WSS hello frame was captured or claimed.

Two incomplete probes preceded the successful one. The first timed out after the
bridge ACK, before inventory collection returned; closing it eventually restored
WSS without restarting the daemon. The second reached offline state but used an
invalid UUIDv4 command ID, which was rejected. The completed probe used UUIDv7.
Thus there were three deliberate WSS takeovers, not one uninterrupted successful
attempt. Neither failed probe proves journal replay.

A process sample during the long takeover found the debug Node spending time in
`inventory::hash_file` / SHA-256. The inventory cache's 30-second TTL is not a
probe deadline. The operator then installed an optimized native release of the same commit
(SHA-256 `9b522e8e7cf233f37b1a8473b35313e97d2321859c14e3c12f08e46c83ce87c1`)
and restarted only the new Mac launchd daemon at 08:22:03 UTC. It was online by
08:22:16 UTC with the same persisted host ID and unchanged host-token digest.
All earlier acceptance instances were stopped before this binary replacement.
This later binary upgrade is separate from the same-PID WSS replay proof above.

## SG / bolt Node installation plan — not executed

With the Hub prerequisite now satisfied, inspect each target's actual OS/architecture,
user-service availability, writable workspace, native CLI inventory, provider
login/configuration and any existing Remuda/Herdr services. The previously
inspected SG host is Debian 10 x86_64; do not assume the public Ubuntu installer
applies. Confirm `<bolt-host>` independently before selecting its binary.

Install a compatible static Linux binary and Herdr/native CLIs only in a separately
authorized rollout. Use the operator user's dedicated private Node data directory,
a different one-shot token per host, and `remuda node install --systemd-user`
against `https://remuda.<zone>`. Review the host's linger policy if the daemon
must survive logout. Persist host identity and token in that Node's directory;
do not reuse the Hub access code or another host's credential.

Allow outbound TCP 443, DNS and established return traffic; no inbound Node port
or tunnel is required. Configure provider gateways, private overlays and native
login state on each Node that can reach them. Verify inventory and a real bounded
session before declaring execution readiness. SG inspection previously found
missing native CLIs/Herdr, so an online Node alone would not satisfy that gate.

## Local validation

Passed with `CARGO_INCREMENTAL=0` and the assigned `target-c-deploy` build cache:

```bash
cargo build -p remuda-node --locked
cargo test -p remuda-node --locked
cargo clippy -p remuda-node --all-targets --locked -- -D warnings
```

All 108 crate tests passed (83 unit and 25 integration tests), including both
new Herdr isolation regressions. Rust formatting, `git diff --check` and
`./scripts/ci/secret-scan.sh` passed. Test transports, SSH, Claude and Herdr used
fixtures; these local tests did not invoke a real model or install a target host.
The demo's two read-only HTTP responses also remained byte-for-byte unchanged.
These local results validate isolation separately from the completed Mac shell
acceptance and the blocked Claude acceptance above.
