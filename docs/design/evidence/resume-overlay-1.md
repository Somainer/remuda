# Claude gateway resume: provider delivery evidence

Date: 2026-09-15. Baseline failure is **VERIFIED** against
`3f680613b38a4fed304fdfc8f97db3073b7438c5`; native continuation is **VERIFIED** with fix
`9c012fd8eaa318082d88a8f7b00645a4808165b6`. Native and browser cleanup are **VERIFIED**.
The original full browser run **FAILED** one existing timing check; the coordinator-requested
post-merge full run later **PASSED** (37 passed, 14 intentional skips).
Validation revisions and gate results are recorded below.

Fresh `claude-pty` creation worked, but resuming its stopped session failed before a native process
or TTY bridge existed. Resume already re-resolved the public provider overlay; Hub forwarding
omitted its RPC-only provider token.

## Isolated reproduction

Both native probes used task-owned `remuda dev` on macOS: Hub `127.0.0.1:58570`, Node
`127.0.0.1:58577`, scratch `/tmp/remuda-c-resumefix/`, and Claude `2.1.270`.
`REMUDA_CLAUDE_CONFIG_DIR` selected a task-owned shared native configuration directory, retaining
the stopped transcript across runs.

Authenticated `POST /v1/providers` configured `kind: gateway` with `scope: host:<host>`. The
operator's relay settings were read into process memory and the endpoint/token sent only to that
API; the settings file was not copied into the repository or probe artifacts. Provider equality
checks logged only booleans. Endpoint values, credentials, machine names, native session IDs, and
instance IDs are omitted from this document.

The live demo and deployment surfaces were outside the probe. No tunnel tools or macOS System
Settings changes were used. The added browser regression uses a fake Node and fake harness,
with no real Claude or relay request.

The API sequence was:

1. Create the host-scoped profile, then `POST /v1/instances` with `claude-pty`,
   `delegation: gateway`, and that profile.
2. Wait for `running` and a driver-reported native session ID; close through the
   instance command API and wait for the parent to reach `exited`.
3. `POST /v1/instances/<parent>/resume` with `{"mode":"terminal"}`; inspect the
   child, command, journal, launch directory, native transcript, and TTY stream.

## Before: baseline failure VERIFIED

| Observation | Baseline result |
| --- | --- |
| Fresh create | `claude-pty`, `running`, native session ID known, 28 journal events |
| Fresh settings | `settings.json`, mode `0600`; token/base URL matched configured profile |
| Parent close | `exited`, 32 journal events |
| Resume HTTP | New child, `replayed: false`, child `requested`, command `queued` |
| Node RPC | `-32603`, exact error below |
| Child materialization | One journal event, empty launch directory, zero binary TTY frames |
| SQLite | Resume public `providerOverlay` present; provider token absent from instance specs and command payloads |

```json
{
  "code": -32603,
  "message": "driver error: driver operation failed: gateway delegation requires a settings overlay"
}
```

The `queued` command and `requested` child were Hub state, not native acceptance. The empty launch
directory and missing TTY output corroborated pre-launch failure. The failed child's close command
later settled through the Node's existing unknown-instance close API handling; no direct database
repair was used.

## Root cause and design choice

`resume_instance` in `crates/remuda-hub/src/http.rs` builds the child spec using
`InstanceRecord::spec_for_resume`, then calls `providers::resolve_and_attach` against the parent's
host. This already refreshed public metadata and enforced profile selection rules, as the baseline
SQLite inspection confirmed.

`forward_if_online` cloned the persisted command for the authenticated Node RPC, but its credential
branch matched only `instance.create`. The resumed child therefore missed both `providerAuthToken`
and its own agent credential. Node `resolve_claude_overlay` exhausted these sources:

| Source | Required inputs | Baseline resume |
| --- | --- | --- |
| Delivered overlay | `request.provider_overlay` + `request.provider_auth_token` | Public overlay present; token missing |
| Generated gateway overlay | `profile.base_url` + environment-backed `profile.secret_ref` | Factory inputs unavailable |
| Explicit settings file | `request.settings_overlay_path` | Missing |

The fix sends `instance.resume` through create's credential branch. The public overlay stays
durable; the current vault token is added only to the authenticated RPC, together with the new
child's instance-scoped agent credential.

This follows [providers.md](../providers.md), D-021 host-scoped secret release, and D-026
new-child/same-host continuity in [decisions.md](../decisions.md). D-027 is attachment transport,
not the provider launch contract. [providers-3.md](./providers-3.md) documents the existing profile
editing/save path. A fresh broker release honors credential rotation and current host scope, and
works after the parent's launch directory is removed. Reusing its settings file would retain stale
secrets and bypass those checks. Native authentication still carries no Hub provider token or
overlay.

The Node fallback error now names missing inputs for all three sources using field names and fixed
diagnostic text only, without values or paths. When factory inputs exist but generation fails, it
reports unavailable environment credentials or a settings-write failure.

## After: native continuity VERIFIED

The fixed binary used the same task data and stopped transcript. Host reconnect took approximately
60 seconds: an early resume returned `409 HOST_OFFLINE`; after the host came online, resume was
accepted with `replayed: false`.

| Observation | After-fix result |
| --- | --- |
| Child lifecycle | `running`, 15 journal events, original native session ID reported |
| New launch settings | Present, mode `0600`; token/base URL matched the configured profile by boolean checks |
| Hub follow stream | One binary TTY frame, 5,919 bytes, during an eight-second capture |
| Parent | Remained `exited`, 34 journal events |
| Recall request | API accepted; native transcript appended the question and exact expected reply below |
| Final child close | Close accepted, then child observed `exited`, 28 journal events |
| Provider-token redaction | API responses and SQLite specs/commands contained no provider token, as on baseline |

The task-owned native transcript contained this synthetic exchange:

```text
Before stop:
user       Remember this exact phrase: AMBER-RESUME-571. Reply with just OK.
assistant  OK

After resume:
user       What exact phrase did I ask you to remember? Reply with just the phrase.
assistant  AMBER-RESUME-571
```

The shared native session ID and transcript reply establish conversation continuity and completion
of the recall turn. This conclusion uses native transcript evidence, beyond the API's acceptance and
the attached TTY.

## Regression coverage

- Hub `resume_redelivers_current_gateway_profile_and_host_scoped_secret` captures
  create and both resume modes, checking rotated token/URL/model/headers. Moving
  scope rejects a new resume before allocation or RPC delivery. HTTP and SQLite
  specs/commands remain token-free. The existing native-resume test checks absent
  provider fields; its fake Node aborts on fixture drop.
- Node `gateway_resume_materializes_delivered_overlay_like_create` removes the
  old launch directory, then verifies matching new settings with mode `0600`.
  Missing delivery fields and wrong host scope fail without writing settings.
- Browser `gateway Claude PTY: stop and resume delivers the provider and attaches
  a new TTY`, now in `web/tests/e2e/resume-overlay.hub.spec.ts`, checks lifecycle
  progress and attachment. Its fake Node rejects missing overlay/token;
  fake-harness bytes establish UI attachment only.

## Revision and browser validation

The branch was fetched and rebased onto `origin/main` at
`9df90998deb2bf2e71fe6082b2dbdabb61ce8fb7`. The checked Rust diff against native-probe
revision `9c012fd` was empty. The resume fix became `b8c5d93`; the separate web import
repair became `0567560` (originally `603fc68`).

The new browser test moved from shared `hub-live.spec.ts` into the dedicated
`resume-overlay.hub.spec.ts`; the shared file now matches main. Main lacked a suffix
matcher, so commit `5c986be` adds `/\.hub\.spec\.ts$/` as a second `testMatch` array
pattern while preserving the existing regex.

The full `test:e2e:hub` suite ran **once before rebase**: **30 passed, 1 failed,
1 skipped**. The skipped case required a real Node. The new resume case passed in
6.6 seconds. The failure was the existing promoted-Claude fake-harness test's
1.5-second `working` assertion:

| Captured boundary | Time (UTC) |
| --- | --- |
| Browser assertion began | `17:25:52.162` |
| Node `UserPromptSubmit` event | `17:25:52.371` |
| Hub persisted event | `17:25:52.382` |
| Assertion deadline | `17:25:53.663` |
| Browser received event | `17:25:53.753` |
| UI displayed `working` | `17:25:53.860` |

These observations do not distinguish transport delay from browser scheduling.
No code or assertion timeout was changed to address this timing failure.
After rebase, this focused command used the same isolated ports:

```sh
HUB_E2E_LISTEN=127.0.0.1:58580 HUB_E2E_WEB_PORT=58589 \
HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:58581 \
pnpm --dir web exec playwright test -c playwright.hub.config.ts \
  promoted-claude-hub-live.spec.ts resume-overlay.hub.spec.ts \
  --output=test-results/resume-overlay-focused
```

Both tests passed in 1.6 minutes: promoted Claude in 23.4 seconds and resume in
8.2 seconds. The unchanged focused rerun did not reproduce the timing failure;
it does not turn the earlier full-suite result into a full-suite pass.

## Gates

Formatting, Clippy, the complete requested Rust test suite, web tests, and the focused
browser rerun all passed after the final rebase. The full browser suite ran once before
rebase; its timing failure is recorded separately above and below.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **VERIFIED** |
| `cargo clippy -p remuda-hub -p remuda-node -p remuda --all-targets --locked -- -D warnings` | **VERIFIED** |
| Focused regressions | **VERIFIED**: 3 Node tests and 2 Hub tests passed |
| `cargo test -p remuda-hub -p remuda-node -p remuda --locked` | **VERIFIED**: 599 passed; one helper marked ignored is re-executed by its passing parent test under both carrier configurations |
| `pnpm --dir web test` | **VERIFIED after rebase**: 645 tests passed across 81 files in 45.78 seconds |
| `HUB_E2E_LISTEN=127.0.0.1:58580 HUB_E2E_WEB_PORT=58589 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:58581 pnpm --dir web run test:e2e:hub` | **FAILED before rebase**: 30 passed, 1 existing timing failure, 1 skipped; new resume case passed |
| Focused browser rerun after rebase, command above | **VERIFIED**: 2 passed; earlier timing failure did not reproduce |
| After-run HTTP/SQLite provider-token check | **VERIFIED** |
| `./scripts/ci/secret-scan.sh` | **VERIFIED** |

The web gate exposed a baseline macOS collision between `QuickFind.tsx` and `quickFind.ts`.
Separate commit `0567560` makes two component imports explicitly end in `.tsx`: 13 focused
tests passed, followed by the pre-rebase 616-test/80-file run and the post-rebase run above.

## Native and browser cleanup VERIFIED

After the recall, the child reached `exited` with 28 journal events; the parent remained `exited`
with 34. The owned dev process received SIGINT. Only the owned Herdr PID was terminated, after
checking its process identity. All four recorded dev/Herdr PIDs were then absent, and ports `58570`
and `58577` were closed.

`/tmp/remuda-c-resumefix/` was removed and verified absent, including its vault, generated settings,
native transcripts, and logs. The owned probe helper and sample artifacts were also removed from
`target`.

For both browser runs, task-port process IDs were recorded and confirmed absent after exit.
Hub `58580`, web `58589`, and fake upstream `58581` were all closed. Recorded task data paths
were removed, as were six untracked screenshots produced by existing Spaces tests.

## Coordinator-requested merge validation

After the completed checks above, the coordinator requested merging `origin/main`
at `b3f5fd99f3a04a4fbac67a1d19c687e3f5129e6f`. The sole conflict was
`web/playwright.hub.config.ts`: both sides had functionally identical matcher
arrays. Resolution preserved main's comments, the explicit existing regex union,
and the separate `/\.hub\.spec\.ts$/` suffix matcher.

The `quickFindSearch` rename had not landed: main still had `QuickFind.tsx` and
`quickFind.ts`. A controlled `tsc -b` comparison **VERIFIED** why the two explicit
`.tsx` component imports remain. Current imports passed; temporarily removing
both suffixes produced exit code 2, TS1149/TS1261 casing errors, and missing
`QuickFind`/`QuickFindTrigger` exports. Both imports were restored. No raw logs,
local paths, or runtime identifiers are reproduced here.

The following completed checks apply to the newly merged tree, separately from
the earlier rebase results. The full hub suite ran exactly once for this merge:

| Post-merge check | Result |
| --- | --- |
| `pnpm --dir web exec tsc -b` (no `typecheck` package script is defined) | **VERIFIED** |
| `pnpm --dir web lint` | **VERIFIED**: exit 0; warnings, no errors |
| `pnpm --dir web test` | **VERIFIED**: 675 passed across 84 files |
| `cargo test -p remuda-hub -p remuda-node --locked` | **VERIFIED**: 400 passed; one inventory helper marked ignored is re-executed by its passing parent |
| Focused `resume-overlay.hub.spec.ts` browser regression | **VERIFIED**: 1 passed in 42.3 seconds (test body: 7.0 seconds) |
| Full hub browser suite, one run | **VERIFIED**: 37 passed, 14 intentional skips, no failures in 4.1 minutes |
| Post-merge browser processes/listeners/data cleanup | **VERIFIED** |
| `./scripts/ci/secret-scan.sh` | **VERIFIED** |

The focused and full browser commands both used `HUB_E2E_LISTEN=127.0.0.1:58580`,
`HUB_E2E_WEB_PORT=58589`, and `HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:58581`:

```sh
pnpm --dir web exec playwright test -c playwright.hub.config.ts \
  resume-overlay.hub.spec.ts --output=test-results/resume-overlay-merge-focused
pnpm --dir web run test:e2e:hub --output=test-results/resume-overlay-merge-full
```

The full run passed the unchanged promoted-Claude timing test (19.3 seconds), the
resume test (6.2 seconds), and all three new transcript tests. Thirteen mock-backed
new-session cases were skipped by their existing Hub-mode guard; the remaining
skip requires an external operator-owned `remuda dev`. The executed suite used
only the fake Node and fake harness.

Both runs' recorded Hub/Vite PIDs were absent afterward, all three assigned ports
were closed, and the identified Hub scratch directories were removed. Six untracked
Spaces screenshots generated by the full run were also removed. The promoted
fixture completed its own Node shutdown and scratch-directory cleanup.
