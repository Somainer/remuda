# Node onboarding: outbound WSS to the Hub

The SG host, devboxes and laptops are Nodes. Each Node runs as the user who
owns its workspaces, native CLI login state and Herdr sessions. Use the current
intranet Hub while working on the intranet, or the future public VPS Hub when
that variant is deployed. The Hub never needs an inbound connection to a Node
or an intranet provider gateway. See [D-020](../../docs/design/deploy-public.md).

## Primary mode: persistent daemon + one-shot enrollment (D-019 / D-018)

Once the c-daemon installer and D-018 enrollment changes are included in
the chosen release, obtain a fresh one-shot enrollment token from the
authenticated Hub operator and run this on each Node, as its runtime user:

```bash
remuda node install --hub 'https://<hub-host>' --enroll-token '<one-shot>'
```

This is the intended primary command. The installer manages a `systemd
--user` service on Linux or a launchd user agent on macOS and stores the
host identity and durable credential locally. A one-shot token is for
enrollment, not for the service's restart command; do not share it between
Nodes. Use the release's `remuda node install --help` for service paths and
management commands. These installer commands are not present in the base
CLI used to prepare this package; the manual carrier below is the interim
path until those changes land. Linux user services need the administrator's
chosen linger policy to survive logout; a macOS user agent runs in its
user's login session. Service supervision cannot keep a sleeping laptop online.

## Interim: existing `remuda node` carrier

Install the musl binary matching the Node's CPU architecture (or a native
macOS build). Native CLIs and Herdr are required for the corresponding
session drivers; the Node may enroll first and report them as missing.
The current SG inspection found neither on the remote PATH. Validate the
inventory and install the required tools before claiming session readiness.
Prepare a private
Node data directory and a mode-0600 token file through the operator's
credential handoff. Keep the data directory stable across restarts:

```bash
umask 077
mkdir -p "$HOME/.local/share/remuda"
chmod 0700 "$HOME/.local/share/remuda"
export REMUDA_DATA_DIR="$HOME/.local/share/remuda"
remuda node \
  --hub-url 'wss://<hub-host>/node/v1/connect' \
  --host-token-file "$HOME/.config/remuda/enroll-token" \
  --label region=sg
```

The current carrier flags are `--hub-url` and `--host-token-file`; the
shorter conceptual `remuda node --hub … --token …` is not implemented by
this CLI. The token file must already exist and contain a credential
accepted by the selected Hub version. On pre-D-018 builds only, first
enrollment uses the Hub bootstrap secret; it is a broader credential and
is not the new one-shot enrollment mechanism.

On successful first enrollment the current carrier writes its host token
to `$REMUDA_DATA_DIR/node/host-token`. For every subsequent start, change
`--host-token-file` to that file and remove the temporary enrollment file.
The host identity is persisted under the same Node data directory. Do not
re-copy a bootstrap/enrollment token over an established Node credential.
Run the manual command under the host's existing supervisor if unattended
restarts are needed; the command alone is a foreground process.

## Firewall, NAT and recovery

`<hub-host>` is `remuda.<zone>` for the current SG intranet deployment;
it resolves to an intranet address and requires the Node and browser to
have an approved route into that network. DNS-01 HTTPS does not make this
Hub reachable from an ordinary cellular network. The future public VPS
variant uses a publicly reachable `<hub-host>`.

Allow Node outbound TCP 443 to `<hub-host>`, DNS to the host's configured
resolver and return traffic for established connections. Allow each Node's
required provider/registry egress separately; intranet gateway access stays
on the relevant intranet route. No public Node listener, NAT port-forward,
Hub-to-Node SSH or tunnel software is needed. A TLS-inspecting network must
provide an approved trust chain and support WebSocket upgrades; keep TLS
verification enabled.

The current WSS carrier sends heartbeats every 15 seconds. After a
disconnect it retries with exponential backoff (1-second base, 30-second
base cap, jitter), authenticates again using the durable host token and
resumes journal synchronization. A process supervisor covers process exits
and startup failures; retries within a running carrier cover lost network
connections. NAT mappings are recreated by the next outbound connection.
Commands pending at disconnection are not automatically replayed. Inspect
instance/journal state before resubmitting work whose outcome is unknown.

## Provider profile per host

Configure providers on the Node that can reach them. A Node-local
`remuda.toml` can describe the host's profile with secret references:

```toml
[provider_profiles.intranet]
endpoint = "https://<gateway-host>/v1"
models = ["<model>"]
secret_refs = { api_key = "file:./secrets/provider-key" }
```

`<gateway-host>` is a placeholder for the gateway reachable from this Node;
its value may differ on SG, other devboxes and laptops. Relative secret
paths resolve beside the TOML file. Keep the secret file private and supply
this config through `remuda --config <node-config> node …`.

Current implementation boundary: this TOML profile is validated, but the
Node carrier does not register it with the runtime profile API yet. Merely
adding the section does not switch a native CLI to the gateway. For current
Claude gateway launches, prepare a private settings overlay on that Node
containing its gateway configuration, then set launch options
`delegation: "gateway"` and `settingsOverlayPath` to that Node-local path.
The Node expands `~` and passes the overlay to the native CLI. For
`generic-pty`, configure the native CLI itself under the service user;
that driver does not apply the Claude overlay helper. Native subscription
login remains a separate host-local choice. Never copy provider secrets
or private gateway configuration into the public Hub's `.env`.

After onboarding, check that the Hub lists the expected host identity and
CLI inventory, then verify a disconnect/reconnect without creating a
duplicate host. Test provider access from the Node and inspect the chosen
native CLI configuration; Hub reachability does not prove gateway access.
