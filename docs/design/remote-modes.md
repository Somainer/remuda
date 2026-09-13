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

## macOS daemon prerequisites

Terminal access and launchd access are separate macOS privacy decisions. A Node
launched by `remuda node install` may lack permission to read `~/Documents`,
`~/Desktop`, or `~/Downloads` even when the same command works in a terminal.
Grant **Full Disk Access** to the installed Remuda executable in **System Settings
→ Privacy & Security → Full Disk Access**, then restart that Node, or register a
workspace outside those protected folders. The installer prints this guidance
for a protected `--workspace` path; it does not grant access or change privacy
settings. Keep the executable at a stable path when configuring access.

Workspace registration, instance creation and read-only Git probes use bounded
subprocess checks. A permission error or deadline returns an actionable access diagnostic
instead of waiting indefinitely in a filesystem call. The host doctor and Hosts
details show the workspace check and remediation. The path check is advisory:
an unprotected path may still reach protected data through a symlink.

Claude's user configuration is another access boundary. A skill, command or
agent symlink under the effective Claude config directory can point into a
protected folder and block native startup before the UI or `SessionStart`, even
with a workspace under `/tmp`. The macOS Claude factory checks settings access,
each skill entry’s `SKILL.md`, and command/agent Markdown discovery in one
three-second subprocess before dispatch. It does not traverse unrelated skill
fixtures or examples. Host doctor checks workspace and configuration access
before collecting inventory; a blocker produces a partial report. Its config
check covers the daemon’s default/inherited config, while instance creation also
checks an explicit per-instance `claudeConfigDir`. Fix the reported access or
explicitly choose an accessible `claudeConfigDir`; Remuda does not remove skills,
copy credentials, or skip native configuration automatically. A real native
startup dialog retains the D-022 queued-input behavior.

Generated launchd units use `ProcessType=Interactive`: the daemon serves
user-requested terminal work over WSS rather than XPC activities. The previous
Background policy throttled native children and startup binary hashing. This
scheduling change does not grant filesystem access or replace the privacy
prerequisite. Existing installations adopt the unit change when reinstalled.
See [daemon Claude evidence](./evidence/daemon-claude-1.md) for the isolated
before/after probes and their limits.

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
