# Evidence: self-host-1 — the first M1 cycle driven only by `remuda` verbs

Date: 2026-09-17/18 (local time; the Hub timestamps below are UTC).
Design: [coordinator-hierarchy.md](../coordinator-hierarchy.md) §1.1 goal 6 and
§8.2 (M1 acceptance); [roadmap-2026-09.md](../roadmap-2026-09.md) §R0.
Decision: [D-036](../decisions.md). Prior surface evidence:
[coordinator-5a-dispatch.md](./coordinator-5a-dispatch.md),
[coordinator-5b-watch.md](./coordinator-5b-watch.md),
[gate-lane-1.md](./gate-lane-1.md).

What this records: the coordinator ran a **real** docs task — refresh the
README Status section — from dispatch to a pushed `origin/main`, using
`remuda dispatch / instance read / instance keys / worker answer / watch /
gate / retire / report` and nothing else on the loop path. The one exception is
the final push, which had to happen from the home host (§4 gap 1). The cycle
log is the coordinator's timestamped command/output journal; outputs below are
copied from it and redacted (no tokens, no home paths, no usernames, no real
machine names — the remote is its registry label `devbox-sg`).

## Fixtures

| Thing | Value |
|---|---|
| Project | `prj_01a0aa58-1ead-77c0-a11b-221d43efaecb` (`remuda`), homeHost = the Mac, `defaultBaseBranch: main`, `branchPattern: wt/{worker}/{topic}`, `permissionPosture: bypass`, `defaultEffort: high` |
| Remote host | `hst_01a0a896-c963-74c2-bedd-14da447ff68c`, label `devbox-sg`, carrier **ssh-stdio**, `latencyClass: remote`, `maxInstances: 8`, portBlocks `57000-57999` / `58400-58999`, disk budget 200 GB |
| Workspace | `wsp_01a0aa57-7bc7-76ef-83f6-d4b941c985d2`, role `build` |
| Gate lane | `sg-lane1` on the same remote host (project `revision 7` after `remuda project set --gate`) |
| Provider profile | `pvp_01a0adb4-1b89-7139-8331-ab2ca822c32a` — `"devbox-sg native"`, `kind: native`, `scope: host:hst_01a0a896…`, `defaultModel: ark/seed-evolving[1m]`, models = the host's own gateway ids (`ark/seed-evolving[1m]`, `model_hub/es1_orange_o50[1m]`) — **no stored secret** |
| Node binary | built from `main` `4ae9cda0` (`c-dispatchfix`), re-enrolled with a single-use Hub enroll token; the demo Hub on the same commit |
| Worker | `wkr_01a0b025-d121-76a1-99cd-aaaf73542284` `readme2`, instance `ins_01a0b025-d01c-73da-bfb5-c30370bdc6ff` |
| Gate job | `gjb_01a0b030-70a4-71aa-b324-88cab07cb2dc` |

Each iteration of the day started with the same three operator steps (not part
of the loop, and not scripted agent work): refresh the local demo to
`origin/main`, rebuild the remote Node binary from the home clone, re-enrol
`devbox-sg`, then `remuda project set --gate` for lane `sg-lane1`. The 23:55
re-enrolment also had to kill one orphaned remote `remuda node --stdio` left
over from an earlier bridge.

```text
### 2026-09-17 23:55:49 re-enrol devbox-sg (native carrier launcher)
killed orphan remote node --stdio
commit=4ae9cda06ec9d1c08dd076206d93987846a2fbed
enroll token minted (64 bytes)
bridge pid <pid> → /tmp/remuda-live/ssh-node18.log
"ok":true
"mode":"hub-enroll"
holding ssh-stdio bridge until stdin EOF or disconnect
hst_01a09576 local-development online= False outbound-wss 2026-09-12T11:56:28.449Z
hst_01a096a5 local-development online= True  outbound-wss 2026-09-17T15:56:10.450Z
hst_01a0a896 devbox-sg          online= True  ssh-stdio    2026-09-17T15:56:06.744Z
```

## 1. The accepted cycle, in order

### 1.1 `remuda dispatch` → ssh-stdio Node, shell-pty, native profile, delegation none

The first attempt at 00:12:36 was refused by the Hub, correctly — the earlier
failed runs had left the branch behind (retire keeps branches, see §4 gap 3):

```text
### 2026-09-18 00:12:36 remuda dispatch readme-status --driver shell-pty \
        --model ark/seed-evolving[1m] (native profile; demo+node on 4ae9cda0)
Error: hub HTTP 400: {"error":"invalid request: worker branch
wt/readme-status/b-readme-status-md-3 already exists; refusing to provision a
second worktree","code":"BAD_REQUEST"}

### watch --once
watch: no active workers in scope
```

Re-dispatched under a fresh worker name `readme2` with the same brief
(`b-readme-status.md`, delivered as an objects-store file attachment, never
inlined):

```text
### 2026-09-18 00:14:22 remuda dispatch readme2 --driver shell-pty \
        --model ark/seed-evolving[1m] (native profile; demo+node on 4ae9cda0)
{'id': 'wkr_01a0b025-d121-76a1-99cd-aaaf73542284', 'name': 'readme2',
 'model': 'ark/seed-evolving[1m]',
 'providerProfileId': 'pvp_01a0adb4-1b89-7139-8331-ab2ca822c32a',
 'driver': 'shell-pty', 'branch': 'wt/readme2/b-readme-status-md',
 'portBlock': '57000-57009', 'state': {'state': 'working'}}
instance: ins_01a0b025-d01c-73da-bfb5-c30370bdc6ff
warnings: []
```

Everything on that row is product-assigned: the branch, the worktree
(`<home>/remuda/remuda-wt/readme2`), the per-worker cargo target dir and the
port block `57000-57009`. `driver: shell-pty` is the driver the Node reported
back, not just the requested one (D-035), and `warnings: []` means no supply
substitution happened.

### 1.2 `remuda instance read --source screen` — the worker is readable

75 s after dispatch:

```text
### 2026-09-18 00:15:41 instance ins_01a0b025-d01c-73da-bfb5-c30370bdc6ff after 75s
 driver shell-pty | model ark/seed-evolving[1m] | lifecycle running
 | delegation none | nativeSessionId ins_01a0b025-d…
screen tail: ──────────────────────────────────────────────────────────────
 Accessing workspace:
 <home>/remuda/remuda-wt/readme2
 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 ❯ No, exit
   Yes, I trust this folder
 Enter to confirm · Esc to cancel
### watch --once
NAME     STATUS   DRIVER     SHA/REASON  DETAIL
readme2  working  shell-pty  -           -
```

`delegation none` is the point of the native profile: the worker talks to the
host's own gateway ids directly, with no Remuda-side gateway overlay
(`c-gwcarry`, `e9f1d96d`). The screen is a real emulator grid, not journal
events — that is what `--source screen` buys over the earlier `claude-print`
runs, where the same read answered
`"no live screen for this session; showing journal events"`.

### 1.3 `remuda instance keys` / `remuda worker answer` — the two first-run dialogs

The freshly scoped Claude config dir produced two first-run dialogs. Both were
answered by hand through Remuda verbs (no ssh, no herdr):

```text
### 2026-09-18 00:16:25 remuda instance keys readme2 down enter   # workspace-trust dialog
{ "command": { "command": {
    "commandId": "cmd_01a0b027-afdb-70d7-9a5c-cf25b56126ea",
    "instanceId": "ins_01a0b025-d01c-73da-bfb5-c30370bdc6ff", … } } }

### screen after answering
 …/attachments/b-readme-status-595.md)
 Auto mode and the sandbox read outside the working directories without asking.
 Yes or Block settles this question; Ask again asks on the next outside read.
 …
 Allow reads outside the working directories?
 ❯ 1. Yes, keep allowing reads outside the working directories
   2. No, block reads outside the working directories from now on
   3. No, ask again next time
 Esc to cancel · Tab to amend
### watch --once
NAME     STATUS   DRIVER     SHA/REASON  DETAIL
readme2  working  shell-pty  -           -

### 2026-09-18 00:17:07 remuda worker answer readme2 1   # allow reads outside working dirs
{ "worker": "wkr_01a0b025-d121-76a1-99cd-aaaf73542284", "keys": [ … ] }

### screen
  …verify the crates actually exist on this tree before making claims.
  Listing cmd modules and crates
  ⎿  $ echo "=== cmd modules ===" && ls crates/remuda/src/cmd/ … && ls
     crates/remuda/src/cmd/{dispatch.rs,watch.rs,worker.rs,report.rs,gate.rs,…}
✢ Mulling… (running PermissionRequest hooks… 1/2 · 47s · ↓ 1.2k tokens)
  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents
### watch --once
NAME     STATUS   DRIVER     SHA/REASON  DETAIL
readme2  working  shell-pty  -           -
```

The brief arrived as the attachment visible on screen
(`…/attachments/b-readme-status-595.md`), and the worker's first tool call is a
`ls` of `crates/remuda/src/cmd/` — i.e. the brief was read, not executed.
Cost of the hand-holding: **2 min 45 s** of the run parked on dialogs (00:14:22
launch → 00:17:07 answered). See §4 gap 2.

### 1.4 `remuda watch --once` / `--follow` until DONE

`--follow` streams only state changes and exits 0 when every worker is done or
retired:

```text
### 2026-09-18 00:18:14 remuda watch --follow --project
NAME     STATUS   DRIVER     SHA/REASON                                DETAIL
readme2  working  shell-pty  -                                         -
readme2  done     shell-pty  30a4be32d1642562c5fff5b0139658545270a448  DONE 30a4be32d1642562c5fff5b0139658545270a448
watch: every worker is done or retired
### 2026-09-18 00:24:40 watch --follow exited rc=0
```

Task time from answered dialog to DONE: ~7.5 min; `--follow` held the terminal
for 6 min 26 s and returned rc=0 on its own.

### 1.5 `remuda gate` on lane `sg-lane1`

DONE is a claim, not a gate (§7 I1). The verification ran on the lane host's
own checkout, streamed step by step:

```text
### 2026-09-18 00:25:59 remuda gate wt/readme2/b-readme-status-md --lane sg-lane1 --web auto
repository:       ok (3 ms)
preflight:        ok (7 ms)
worktree:         ok (264 ms)
merge:            ok (296 ms)
pin-merge:        ok (3 ms)
secret-scan:      ok (11620 ms)
no-tunnel-scan:   ok (601 ms)
cargo-fmt:        ok (2605 ms)
cargo-check:      ok (34860 ms)
cargo-clippy:     ok (46079 ms)
cargo-test:       skipped (0 ms)
web-install:      skipped (0 ms)
web-build:        skipped (0 ms)
web-test:         skipped (0 ms)
web-hub-e2e:      skipped (0 ms)
gen-api-current:  skipped (0 ms)
verify-tree:      ok (145 ms)
record-gate:      ok (6 ms)
cleanup:          ok (32 ms)
verify: passed
exit=0
```

Wall clock 00:25:59 → ~00:27:39, ≈100 s, of which `cargo-check` +
`cargo-clippy` are 81 s and `secret-scan` 11.6 s. The rust/web steps a
docs-only diff does not touch are `skipped` by `scripts/ci/gate.sh`'s own
affected-path rules, not by the coordinator.

```text
### 2026-09-18 00:27:39 gate list
JOB                                       MODE    BRANCH                          STATE   FAILED_STEP  MERGE_SHA
gjb_01a0ae4f-077a-73ba-89e4-0cec27ed05e8  verify  wt/c-watchfailed/watch-report-   failed  -            125765f77a10
gjb_01a0b030-70a4-71aa-b324-88cab07cb2dc  verify  wt/readme2/b-readme-status-md    passed  -            6f7d987ace24
```

For contrast, the first branch ever verified through this lane earlier the same
day took 44 min and failed:

```text
### 2026-09-17 15:40:09 remuda gate wt/c-watchfailed/watch-report-failed-instances --web auto --lane sg-lane1
… secret-scan ok (11419 ms) … cargo-check ok (42672 ms) … cargo-clippy ok (43877 ms)
cargo-test: failed (2355363 ms) [retried]
gate failed or returned an incomplete step report
verify: failed
exit=1
```

That run had no retrievable log, which is why `c-gatelog` (`869d3d95`) was
dispatched; `remuda gate log <gjb|obj>` is present in `gate --help` from that
commit on. The branch itself later landed clean as `486131a8`.

### 1.6 Landing — done from the home host

`remuda land` was **not** used: the lane host has no push credentials for
`origin`. The coordinator pushed the lane's verified merge from the home host
instead. First attempt failed, because the merge commit the lane produced was
not reachable from any ref there:

```text
### 2026-09-18 00:28:10 land: fetch the lane's verified merge and CAS-push origin/main
        (lane host has no push credentials)
sglane/main = a56eab71
error: src refspec 6f7d987ace24 does not match any
error: failed to push some refs to origin
origin/main: a56eab71 merge: wt/c-hostfiles/read-only-host-files into main
branch deleted on origin
```

Second attempt pinned the merge object with a temporary ref on the lane host,
fetched that ref, and compare-and-swap pushed `main`:

```text
### 2026-09-18 00:29:11 land (2): pin the lane's verified merge object with a temp ref,
        fetch it, CAS-push main
object type: commit
ref refs/gate/readme2 set; parents: a56eab71 30a4be32
commit
To origin
   a56eab71..6f7d987a  6f7d987ace24 -> main
origin/main: 6f7d987a merge: wt/readme2/b-readme-status-md into main

### 2026-09-18 00:33:36 LANDED origin/main 6f7d987a
        (lane-verified merge, pushed from the home host via temp ref refs/gate/readme2)
```

The pushed sha is exactly the gate job's `mergeSha` (`6f7d987ace24`), whose
parents are the base it was verified against (`a56eab71`) and the worker's
DONE tip (`30a4be32`) — so the tree that landed is the tree that passed, even
though the push came from a different machine. `manpath: can't set the locale`
noise from the remote shell is elided above.

### 1.7 `remuda retire` and `remuda report`

```text
### 2026-09-18 00:28:20 remuda retire readme2
 state: {'state': 'retired'} | node: {'name': 'readme2', 'worktreeRemoved': True,
 'targetRemoved': True, 'reclaimedBytes': '0'}

### 2026-09-18 00:28:21 remuda report --for-owner
== owner action needed ==
  [gate] wt/c-watchfailed/watch-report-failed-instances gate failed or returned
         an incomplete step report
```

`retire` (no `--force`, the worker was already `done`) reclaimed both the
managed worktree and the target dir on the remote. `report --for-owner` raised
exactly one item — the earlier hard gate failure — and said nothing about the
clean DONE, which is the §5.3 T1 contract.

Cycle total: **dispatch 00:14:22 → landed 00:33:36 = 19 min 14 s**, of which
2 min 45 s was dialog hand-holding, ~7.5 min agent work, ~1 min 40 s gate, and
~5 min landing (including the failed first push).

## 2. What the cycle proves against §8.2 M1 acceptance

§8.2 M1 wording: *use `remuda project/task/own/brief/dispatch/watch/worker/
retire` to complete one **real** dispatch round (≥3 workers, at least one
remote), never invoking `<coord-scratch>/*.sh`; the evidence doc records every
step's command and output and walks the playbook §3 eleven steps one by one,
saying which the product now owns and which stay LLM judgement.*

| M1 clause | Status in this cycle |
|---|---|
| Real task, not a fixture | **Yes.** A README Status refresh that landed on `origin/main` as `6f7d987a`; no synthetic repo, no fake harness, no stubbed gate. |
| `remuda project` | **Yes.** `project set --gate` configured lane `sg-lane1`; the project row carries members/hosts/portBlocks/placement/posture (revision 7). |
| `remuda brief` | **Yes.** Brief delivered as an objects-store `text/markdown` attachment; visible on the worker's screen as `…/attachments/b-readme-status-595.md`. |
| `remuda dispatch` | **Yes**, to a remote **ssh-stdio** Node, `--driver shell-pty`, native provider profile, `delegation none`, no supply substitution (`warnings: []`). |
| `remuda watch` | **Yes.** `--once` between every step, `--follow` to the DONE sha, self-exit rc=0. |
| `remuda worker` | **Yes.** `worker answer` for a dialog; `instance keys` for the other. |
| `remuda retire` | **Yes.** Worktree + target dir reclaimed on the remote. |
| `remuda report` | **Yes.** `--for-owner` surfaced only the owner-actionable gate failure. |
| gate → land | **gate yes, land no.** Verification ran on the lane host through the Hub queue (`gjb_01a0b030…`, `verify: passed`); the push had to be done from the home host (§4 gap 1). `remuda land` exists and its `--help` is in the log, but it was not exercised against the real origin. |
| ≥3 workers, at least one remote | **Partially, and not in one clean batch.** The accepted end-to-end cycle ran **one** worker (`readme2`). Three workers were dispatched on the same remote host earlier in the day (`pty-reap`, `readme-status`, `pin-refuse`) and all three failed their first turn for the reasons in §4 gap 3; the ≥3-worker fan-out itself is proven in [coordinator-5a-dispatch.md](./coordinator-5a-dispatch.md) / [coordinator-5b-watch.md](./coordinator-5b-watch.md) on synthetic fixtures. A ≥3-worker *real* batch on the fixed build is not yet on record. |
| `<coord-scratch>/*.sh` never invoked | **On the loop path, yes** — dispatch/watch/answer/gate/retire/report were all Remuda verbs, and no script was scp'd or ssh-run for them. **Not yet globally**: three interim scripts still run around the loop (§4 gap 4), and the landing push was hand-run git on the home host. |
| Playbook §3, eleven steps | **Owned by the product now:** pick host/model and admit (dispatch placement + supply), allocate branch/worktree/target/ports, deliver the brief as a file, launch the carrier, poll the screen, classify done/blocked, answer a prompt, verify on a lane, retire and reclaim, report to the owner. **Still LLM judgement:** writing the brief (scope, acceptance, redaction rules), reading a failed gate and deciding hand-back vs. re-dispatch, and deciding *when* a task is worth dispatching at all. |

`remuda task` / `remuda own` were not used in this cycle — the task was owner
intent relayed straight into a brief, so no ledger row and no ownership claim
were needed. That part of §8.2's verb list is covered by
[coord-task-1.md](./coord-task-1.md) and stays unexercised here.

## 3. Verdict

M1 goal 6 — *"a human coordinator completes one dispatch→watch→gate→land→retire
round using only `remuda` verbs"* — is met **with one exception**: `land`. The
loop is real, remote, and script-free on its own path; landing is still a push
from the home host. Accepted as such in [D-036](../decisions.md).

## 4. Honest gaps

### Gap 1 — `land` needs the home host

The lane host verifies but cannot push: it holds no credential for `origin`.
So the last hop of every landing is a human-run `git fetch` + CAS `push` on the
home host, and the first attempt failed outright (`src refspec … does not match
any`) because the lane's merge commit was a loose object reachable from no ref.
The workaround that worked — pin `refs/gate/<worker>` on the lane host, fetch
that ref, CAS-push — is exactly the product shape that is missing.

Two honest options, pick one:

1. **Hub/home-host push.** `land` keeps its verify on the lane, but the
   compare-and-swap push is performed by the Hub or the project's home host
   against the lane's `mergeSha`. This requires the lane Node to publish the
   verified merge under a fetchable ref (what the manual fix did by hand) and
   the Hub to fetch it before the CAS, so the pushed object is still the
   verified one.
2. **Credential the lane host.** Give the lane host a scoped push credential
   for the project remote, and `remuda land` works as documented today
   ([gate-lane-1.md](./gate-lane-1.md) proved that path against a scratch bare
   repo, where the lane host *did* have write access).

Until one exists, `remuda land` must not be presented as the coordinator's
landing verb for this project.

### Gap 2 — first-run dialogs stall the worker, and `watch` calls them `working`

Two dialogs parked the worker for 2 min 45 s, both needing a human keystroke:

1. the workspace-trust dialog for the provisioned worktree
   (`Is this a project you created or one you trust?` / `Yes, I trust this
   folder`) — answered with `remuda instance keys readme2 down enter`;
2. the auto-mode question `Allow reads outside the working directories?`
   (1 keep allowing / 2 block / 3 ask again) — answered with
   `remuda worker answer readme2 1`.

Worse than the stall: `remuda watch --once` printed `working  -  -` on all
three polls while a modal dialog owned the screen. The co-watch classifier has
a `blocked{reason}` state for dialogs and it did not fire for either layout, so
an unattended coordinator would have waited on a `working` row forever.

This is a **pre-seeding** problem, not a watch-only problem: the Node should
write the persisted trust flag for a cwd it provisioned itself before launch,
and (under `permissionPosture: bypass` only) seed the outside-reads answer —
the primitives already exist in `claude_onboarding.rs`
(`seed_scoped_config`, `pre_trust_workspace`) but `native.rs` gates pre-trust
on a *registered workspace root*, which a dispatch worktree is not. Worker
`c-onboard2` is in flight on exactly this (pre-seed + classify a first-run
dialog as `blocked` with the dialog title as reason + fixtures for both
layouts); its evidence will be `dispatch-onboarding-1.md`.

### Gap 3 — the earlier attempts that failed, and the fixes that closed them

Everything before 00:12 on 2026-09-18 failed. Recorded honestly, with the
landed fix each one bought:

| Attempt | Failure | Closed by |
|---|---|---|
| 12:42 `dispatch readme-status` (no `--driver`) | Roster said `claude-pty`, but the instance spec read back `driver claude-pty model claude-fable-5.1` — the pinned model was silently replaced by a gateway-profile model. `instance read --source screen` had no screen at all: `"no live screen for this session; showing journal events (a PTY carrier serves GET /v1/instances/<id>/screen)"`. Retired `--force`. | `c-pinrefuse` **`17045bfd`** — an unknown/parked model pin is refused with a reason instead of substituted ([supply-pin-refuse-1.md](./supply-pin-refuse-1.md)). |
| 12:42–12:47 three workers (`pty-reap`, `readme-status`, `pin-refuse`) | All three failed their first turn: the **stdio Node ran `claude-print`** although the roster said `claude-pty`; the Remuda gateway profile overlay was applied on top; every turn ended `API Error: 400 requested model is not available`. `watch --once` showed all three as `working`. The print child processes stayed alive on the remote after the failed turn (3 pids, parent `remuda node --stdio`) and were only reaped by `retire --force`. | `c-dispatchfix` **`4ae9cda0`** — driver selection from the host's `driverInventory`, no silent print fallback, Node refuses an unsupported carrier with a reason code instead of degrading, roster records the *actual* driver, and failed dispatch reclaims what it provisioned (D-035, [dispatch-driver-1.md](./dispatch-driver-1.md)). Print-process reaping tracked separately from the `b-pty-reap` brief. |
| 12:50–12:51 native profile + `--driver shell-pty` | The new tokenless native profile was rejected at dispatch: `hub HTTP 400: {"error":"provider profile has no stored auth token","code":"BAD_REQUEST"}` — the token check assumed every profile carries a secret, which a host-native profile deliberately does not. | `c-gwcarry` **`e9f1d96d`** — gateway-overlay precedence: a native profile launches with the host's own credentials/model ids and the Remuda gateway overlay no longer rides along ([gateway-carryover-1.md](./gateway-carryover-1.md)). |
| 12:53 fall back to the gateway profile with `claude-opus-5` | The Hub accepted `driver shell-pty` (`instance.driver=shell-pty`) but the stdio Node launched `claude-print` again, and the gateway answered `400 requested model is not available` for that id from this host; lifecycle `failed`, while `watch --once` still said `working`. Also: the first `instance read --source screen` call in the log is a usage error (the instance id is positional, not a flag). | `c-dispatchfix` **`4ae9cda0`** for the driver half; `c-watchfailed` **`486131a8`** for the reporting half — a failed instance is reported as failed by `watch` instead of hiding under `working` ([watch-failed-1.md](./watch-failed-1.md)). |
| 15:40–16:24 first lane gate | `cargo-test failed (2355363 ms) [retried]`, `gate failed or returned an incomplete step report`, and **no retrievable log** for the failure — nothing to hand back to the worker. | `c-gatelog` **`869d3d95`** — `remuda gate log <gjb…\|obj…>` prints the bounded failure log, and `land --keep-logs` retains it on success ([gate-log-1.md](./gate-log-1.md)). |
| — (precondition for all of the above) | Before batch 6 the coordinator scp'd `remote-gate.sh` and ssh'd the lane by hand. | `co-gate` **`5b03b4b5`** — gate queue + lane runner + `remuda gate/land/gate list/gate cancel` (D-034, [gate-lane-1.md](./gate-lane-1.md)). |

Two smaller facts worth keeping: `retire` **preserves the branch**, so a
re-dispatch under the same worker name is refused (`worker branch … already
exists; refusing to provision a second worktree`) — that refusal is correct,
but it means a retried task needs a new worker name (here `readme-status` →
`readme2`). And the whole day needed six rebuild/re-enrol iterations of the
remote Node because every fix above had to be on the lane host's binary before
the next attempt could be honest.

### Gap 4 — interim scripts the coordinator still runs

Not on the loop path, but still shell:

| Script | What it does today | What retires it |
|---|---|---|
| `rgate4.sh` | Serial batch gate driven by a queue *file*, so branches can be appended while it runs: skips a branch that conflicts with `origin/main` until its worker rebases, drops a failed branch into a hand-back list, holds a local lock so two gates never overlap, and pushes the verified sha from the home host. Still used because the day's throughput is a dozen worker branches, not one. | A CLI/Hub intake for the same shape: `remuda gate` taking many branches (or a persistent project queue) with **conflict-skip** rather than a failed merge step, a hand-back list `remuda report` can read, and Gap 1's Hub-side push. The Hub gate queue already does FIFO/lane parallelism/land serialization — what is missing is multi-branch intake, the conflict pre-check, and the hand-back ledger. |
| `coord-guard.sh` | Outer retry loop for the **coordinator's own** Claude session in a herdr pane: classifies the last turn of the session transcript, re-prompts `continue` after a transient 5xx/429/overload/timeout with 2→30 min backoff, never retries a non-transient 4xx, and restarts the session with `--resume` (capped) if the process is gone. | The coordinator running **as a Remuda instance** rather than a bare pane — i.e. §2.2's T1/T2 seats with the existing `worker nudge` throttle, `worker resume` (D-026 native session resume) and `idle-api-error` classification applied to the coordinator's own session. That is the R5 prerequisite (coordinator verbs as MCP tools + skill); until then the coordinator cannot supervise itself through the product. |
| `remote-spawn.sh` | Pre-dispatch worker spawning on the remote: `git worktree add` from `origin/main`, `herdr tab create` with the full env (toolchain PATH, `CARGO_TARGET_DIR`, `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=8`, OpenSSL/rpath), `herdr agent start --kind claude -- --dangerously-skip-permissions --model <id>`, then pressing Enter on the trust dialog. Every `c-*` worker in §4 gap 3's fix table was started this way. | Gap 2 (pre-seeded dialogs) plus one more real ≥3-worker `dispatch` batch on the fixed build. `dispatch` already assigns the same worktree/target/port/env set (§1.1) and `--driver shell-pty` already gives a readable screen; the remaining reasons the script survives are the hand-pressed trust dialog and the toolchain/OpenSSL env that the remote workspace registration has to carry for a Rust build. |

Per R0's exit criterion the archive of these scripts to `coord-state` is the
remaining bookkeeping; this document is the record of which product gap each
one is still standing in for.
