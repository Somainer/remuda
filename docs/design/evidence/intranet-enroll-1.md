# Intranet enrollment 1 — device paired, Node enrollment blocked

Status: **BLOCKED deployed-hub-missing-D-018-enrollment**. Target (redacted):
`https://remuda.<zone>`. Live API checks ran on 2026-09-13 at approximately
07:34:46 UTC / 15:34:46 Asia/Shanghai, from the Mac on the intranet.

The worktree started on `wt/c-deploy/intranet-enroll` from fetched `origin/main`
at `c121ae33e02fa7699002c6f9394f5197729869a1`. This milestone performed device
pairing and checked the requested enrollment endpoint. The endpoint returned
HTTP 405, so no one-shot Node credential was obtained and no new daemon was
installed. The existing local demo was not stopped or modified. No persistent
installation, Hub upgrade, Caddy change or Node installation was performed on
`<sg-host>` or `<bolt-host>`.

## Live device pairing and web entry

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

All seven HTTPS requests returned curl certificate verification result 0.
The login and pairing responses both set `remuda_device` with `Secure`,
`HttpOnly` and `SameSite=Strict`. The authenticated cookie session is verified;
this is API acceptance, not a browser/PWA interaction test.

## Enrollment compatibility blocker

The Hub artifact last verified during activation is `remuda-hub:e3a4133`, exact
runtime commit `e3a41335513526e018e90815a6bfae6341f58ccd`. That source predates
D-018 and does not register `/v1/hosts/enroll-token`. Current source registers
that POST route in `crates/remuda-hub/src/hosts.rs` and implements token minting
in `crates/remuda-hub/src/http.rs`. The observed 405 is consistent with the older
Hub's missing POST route; successful device pairing does not establish the new
bootstrap/enrollment separation or access-code TTL policy.

The next prerequisite is an approved Hub upgrade to a release containing D-018,
D-019 and the current compatible Hub/Node protocol, preserving the existing data
and proxy configuration. This run did not perform that persistent SG change.
The access code was not substituted for a Node enrollment token.

## Mac daemon isolation fix

Source review found a shared default Herdr session name, `remuda-node`.
Daemon entry points already placed sockets below their data directory, but the
native default still exposed the shared name to Herdr's named-session shutdown
fallback. Other native entry points also defaulted to shared socket discovery.

`NativeDriverConfig::new` now defaults to sockets under `<node-data>/herdr`
and a stable `remuda-node-<digest>` session name derived from the data-directory
path. Distinct data directories therefore get distinct direct sockets and named
fallback sockets. An explicit `REMUDA_HERDR_SESSION` remains an operator override.
No running process was restarted to adopt these defaults.

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

## Remaining Mac acceptance after the Hub prerequisite

1. With the persisted paired-device cookie, mint a fresh one-shot enrollment token
   through `POST /v1/hosts/enroll-token`; verify the response's `token`,
   `enrollTokenId` and `expiresAt` fields without logging their secret values.
2. Use a stable private data directory separate from `/tmp/remuda-live`, for example
   `$HOME/.local/share/remuda-intranet-mac`, and a native Mac binary containing the
   isolation fix. Install with the existing D-019 interface:

   ```bash
   remuda --data-dir "$HOME/.local/share/remuda-intranet-mac" node install \
     --launchd --hub 'https://remuda.<zone>' --enroll-token '<one-shot>'
   ```

3. Verify the launchd service, the private `node/host-id` and `node/host-token`,
   removal of the consumed enrollment file, online Hub host state and inventory.
4. Create `shell-pty` and `claude-pty` instances on that exact host. Verify a
   prompt/reply through the Hub and stop both; use an isolated workspace and the
   permitted small model/budget for the real Claude probe.
5. Interrupt only this daemon's outbound WSS connection once, keeping the demo
   and Caddy untouched. Record the prior durable journal sequence, the Hub hello
   resume watermark, replay after that watermark and the same persisted host ID.
   Merely observing online again is insufficient proof of watermark recovery.

These steps have not run. No host-online, PTY prompt/reply, stop or watermark
resume success is claimed by this record.

## SG / bolt Node installation plan — not executed

After the Hub upgrade prerequisite, inspect each target's actual OS/architecture,
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
fixtures; no real model invocation or target-host installation was performed.
The demo's two read-only HTTP responses also remained byte-for-byte unchanged.
These local results do not replace the blocked live Node acceptance above.
