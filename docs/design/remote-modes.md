# Remote Node modes (D-019)

A remote Node owns its instances and SQLite journal independently of any Hub
connection. SSH is a bootstrap or transport mechanism; losing SSH must not stop
the daemon or its children. This supersedes the connection-bound SSH launch in
D-013. `remuda dev` continues to compose its Node in process.

| Mode | Deployment | Disconnect behavior |
| --- | --- | --- |
| Primary | Persistent `remuda node daemon`, outbound WSS to a publicly reachable Hub | Instances keep running; exponential backoff reconnects and replays the journal after the Hub's durable watermark. |
| Fallback | The same daemon, with Hub-supervised `ssh <sg-host> remuda node bridge` forwarding NDJSON over its private Unix socket | Only the bridge exits. The next bridge reattaches to the existing daemon and resumes from the Hub's acknowledged sequence. |
| Development only | `remuda node --stdio` | Ephemeral and non-durable as a process: peer loss shuts down drivers. Its persisted journal does not make its running instances survive. |

## Operation

`remuda node install --hub wss://hub.example/v1/node --enroll-token TOKEN`
installs a user service. Use `--systemd-user` on Linux or `--launchd` on macOS.
An authenticated device obtains `TOKEN` from D-018's
`POST /v1/hosts/enroll-token`; the device pairing access code is not a Node
credential. Successful enrollment exchanges the one-shot token for a private
host token used on reconnect. Service units contain file references, not tokens.

The D-018 mint endpoint is pending on the current base of this branch. The daemon
already accepts its one-shot bearer and persists the returned host token; the
mock WSS tests exercise that exchange. Once D-018 lands, the operator/bootstrap
caller must obtain the token from the authenticated device endpoint above and
verify the real exchange. The SSH acceptance does not use a device access code
as a Node enrollment credential.

`remuda node daemon` is the foreground service process. `remuda node run --daemon`
starts it detached when no user service manager is available. `remuda node status`
probes the local daemon without taking control. `remuda node bridge` starts a
missing daemon on demand; `--no-start` refuses that behavior. `remuda node
uninstall` disables and removes the user service.

The socket lives under the Node data directory with mode `0600`. A bridge's local
hello requests control explicitly; a later controller must request takeover.
Takeover fences the previous controller without stopping accepted instance work.
The Hub-facing hello identifies the connection as a bridge to a persistent daemon.

## Recovery contract

The Node sends journal records using their original `journal.append` sequence.
The Hub returns per-instance durable watermarks in the hello response. A new
connection subscribes before replay, sends only records after those watermarks,
and then continues live delivery. Transport writes are not acknowledgments;
duplicate records after a lost acknowledgment are safe. Commands with unknown
outcomes are reconciled through instance state and command identity, not blindly
reissued.

The Hub records a disconnected bridge as offline but potentially alive, removes
it from placement, and probes the daemon again. Only failed daemon reachability
for the configured host-lost grace period permits the synthetic `host-lost`
projection. On reconnect, the daemon's instance snapshot and replayed journal
reconcile the Hub view. A daemon/process failure and a bridge failure are distinct.

An unavailable user service manager is reported explicitly. SSH bootstrap can use
the detached daemon fallback in its private `/tmp/remuda-*` directory; this does
not claim login/logout or reboot persistence from a service that was not enabled.
See [acceptance evidence](./evidence/remote-daemon-1.md) for the tested environment.
