# D-019 remote daemon acceptance

Date: 2026-09-13 (Asia/Shanghai; timestamps below are UTC).

The real remote run proves that a detached Node and its accepted Claude work
survive a killed SSH bridge. The Hub reconnects using its previous durable
watermark and receives the completion records. D-018's public enrollment endpoint
is pending; this report does not claim a real one-shot WSS enrollment against it.

## Environment and boundaries

- Hub: this branch's `remuda dev`, Hub `127.0.0.1:59180`, in-process development
  Node `127.0.0.1:59187`, isolated local `/tmp/remuda-c-daemon-acceptance` data.
- Remote: `<sg-host>`, Linux x86_64, Claude Code 2.1.221. No existing `remuda` on
  PATH. Uploaded the current `x86_64-unknown-linux-musl` Remuda binary through the
  browser's “upload temporary copy if missing” binary policy.
  Acceptance binary SHA-256:
  `08488937206f8985f568607bd8f63a832dd56f607f52fd2c245cecd61b9f7349`.
- Browser: Orca embedded browser, logged in to this isolated Hub. Added the SSH
  host with label `remote-daemon-acceptance`, then created `kind=claude`,
  `driver=claude-print` instances from the new-session form. The model was
  `model_hub/es1_orange_o50[1m]`, with a configured USD 0.30 budget.
- Remote writes were restricted to the managed `/tmp/remuda-ssh-<host-id>` tree,
  `/tmp/remuda-c-daemon-acceptance`, and the installed user service unit.
  Gateway environment/model settings were copied only within the remote into
  the temporary HOME. No OAuth migration, credential output, or transfer of
  remote credentials to the local machine was used.
- A local SSH wrapper isolated the remote HOME and could temporarily reject
  **bridge** reconnects while still allowing independent daemon status probes.
  This made it possible to observe the DONE file while the Hub remained offline.

## Detached fallback and first interruption

The remote user systemd bus was unavailable. Bootstrap reported
`Failed to connect to bus: No such file or directory`, then successfully started
the detached fallback. The installed unit belonged to the managed data directory.

```text
daemon PID 2467147
PPID 1; SID 2467147; PGID 2467147; TTY ?
node.sock mode 0600
node status: {"durable":true,"running":true,"socket":"/tmp/remuda-ssh-<host-id>/node.sock"}
```

The daemon outlived the bootstrap SSH session. Sending SIGHUP to that exact PID
also left it alive, with `node status` returning exit status 0 and the same PID.

First instance: `ins_01a097b3-bff7-7285-93ae-130f6f90015f`.

| UTC timestamp | Observation |
| --- | --- |
| 22:19:01 | Claude's Bash command wrote STARTED, then began `sleep 120`. |
| 22:19:25.188 | Killed Hub-side SSH PID 36850 with SIGKILL. Hub durable sequence was 16; instance lifecycle was running. |
| During the gap | Hub state was `offline-alive`; instance remained running/disconnected, durable sequence 16. Daemon answered independent status probes and survived SIGHUP. |
| 22:21:01 | Remote command wrote `REMOTE_DAEMON_DONE` and this timestamp into DONE. |
| 22:21:03.213 | Read DONE over a separate SSH probe while the Hub was still `offline-alive`, sequence 16. Daemon PID remained 2467147. |
| 22:21:32.344 | Released the bridge reconnect hold. |
| 22:22:06 | New bridge attached; Hub replay advanced through sequence 27. |

`GET /v1/instances/<instance-id>/journal?afterSeq=16` returned precisely sequences
17–27. The records retained their original Node timestamps:

| Sequence | Original UTC time | Replayed evidence |
| --- | --- | --- |
| 21 | 22:21:01.548 | `workflow.run`, state `completed` |
| 22 | 22:21:01.566 | Bash `tool_result`, outcome `succeeded` |
| 25 | 22:21:06.911 | Read result containing `REMOTE_DAEMON_DONE` and `22:21:01Z` |
| 26 | 22:21:07.064 | Native turn result reported an error |
| 27 | 22:21:07.064 | Reported usage USD 0.41857 |

The work and file read completed while disconnected. The model's reported spend
exceeded the configured USD 0.30 cap before a final assistant reply, so this run
reconciled as failed/idle/connected. This is not presented as a successful model
turn. A second brief uses one Bash call, including DONE output, to avoid the extra
read turn.

## Second interruption and browser replay

Second instance: `ins_01a097b8-5b8c-770f-a6f4-240ed74822b8`. It ran one Bash call:

```sh
date -u +%FT%TZ > STARTED2
sleep 120
echo REMOTE_DAEMON_DONE_2 | tee DONE2
date -u +%FT%TZ >> DONE2
```

| UTC timestamp | Observation |
| --- | --- |
| 22:24:02 | STARTED2 written; sleep began. |
| 22:24:35.323 | Killed replacement Hub-side bridge PID 38022 at durable sequence 10. |
| 22:26:02 | DONE2 written with `REMOTE_DAEMON_DONE_2`. |
| 22:26:21.617 | Separate SSH read confirmed DONE2 and unchanged daemon PID 2467147; Hub remained `offline-alive`, instance running/disconnected at sequence 10. Released reconnect hold. |
| 22:26:34 | New bridge replayed completion; durable sequence advanced to 22. |

The resumed journal after sequence 10 contained contiguous sequences 11–22:

- Sequence 14: workflow completed at `22:26:02.394Z`.
- Sequence 15: successful Bash result containing `REMOTE_DAEMON_DONE_2`, at
  `22:26:02.403Z`.
- Sequence 20: assistant message with phase `final`, status `complete`, and
  content `REMOTE_DAEMON_DONE_2`, at `22:26:06.113Z`.
- Sequence 21: native result again reported `error`; sequence 22 reported
  USD 0.4155587500000001, above the configured cap. The Hub reconciled
  failed/idle/connected. The normalized result does not retain its detailed
  error subtype; attributing the error to the budget cap is an inference.

The browser's session view displayed the replayed DONE result, sequence 22, and
connected status. Both acceptance tasks completed their requested file writes
and emitted completion evidence without a bridge; neither native result is
misrepresented as an error-free paid model turn. No further model runs were made.

## Cleanup

Closed both instances through the Hub, observed lifecycle `exited`, and removed
the disposable SSH host (HTTP 204). `node uninstall --systemd-user` removed the
owned user service unit; it returned a clear error because the absent user bus
could not confirm process shutdown. Sent SIGTERM to the verified daemon PID.

The final independent remote check confirmed no managed process, pidfile, or
socket remained. Removed both remote temporary trees, including the copied
gateway settings. Verified the user service unit was absent. No existing remote
workspace or credential file was modified. The local acceptance dev process and
browser tab were also stopped; ports 59180 and 59187 had no remaining listeners.

## Automated checks

Only the four touched crates were tested: `remuda`, `remuda-node`, `remuda-hub`,
and `remuda-ssh`. The combined test run passed **325 tests**, failed 0, and ignored
1 opt-in remote test. The explicit remote acceptance above supplies the live
evidence; the ignored test is not counted as passed.

The focused coverage includes:

- Native fake-Claude work completing after its bridge peer is killed, replay
  after an acknowledged watermark, and SQLite reopen verification.
- Outbound WSS reconnect delivering offline completion without a new command,
  returned host-token persistence with mode 0600, and authoritative watermarks.
- Controller takeover, no-takeover refusal, bridge/WSS fencing, and a stalled
  old controller write not blocking its replacement.
- A future sequence gap is rejected rather than acknowledged as a duplicate;
  a failed final journal append is retried from the confirmed watermark without
  waiting for another broadcast event.
- Hub bridge interruption held beyond the reaper grace period, snapshot
  reconciliation without advancing the journal watermark, and replayed
  completion; managed-host reaper guard and cross-host reconciliation isolation.
- systemd/launchd service text, escaping, private configuration, startup
  readiness, and the unavailable-manager detached fallback.

`cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`,
`git diff --check`, and `./scripts/ci/secret-scan.sh` passed for the submitted work.
The Linux musl release cross-build also passed.

The real SSH runs used the binary hash recorded above. A subsequent review
hardened the separate outbound WSS gap-acknowledgment and retry path; its focused
regression is included in the final automated checks. This does not relabel the
earlier acceptance binary as containing that later WSS-only fix.

## D-018 follow-up

The authenticated device endpoint `POST /v1/hosts/enroll-token` is **pending on
the current main base**, per the coordinator's sequencing decision. No r-sec4
authentication implementation is included in this branch.

Once that endpoint lands, the operator/bootstrap caller supplying
`remuda node install --hub URL --enroll-token TOKEN` must obtain TOKEN through
that API. The receiving call sites are `install_service` in
`crates/remuda/src/cmd/node/service.rs` and `outbound_loop` in
`crates/remuda/src/cmd/node/daemon.rs`; the WSS link persists the returned
`nodeToken` before exposing enrollment success. They already accept the intended
one-shot bearer contract and prefer the durable host token on reconnect. The
remaining integration gate is a real mint/exchange/reconnect against D-018,
including its rejection of device access codes for Node enrollment.

SSH bootstrap in this acceptance used no enroll token. The authorized additive
`ws.rs` daemon snapshot registration and `store.rs` lost-host guard are included;
reconciliation itself lives in the SSH module and cannot update another host's
instances or advance durable sequence numbers.
