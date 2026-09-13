# D-023 workspace registration acceptance

The feature was exercised through an isolated `remuda dev` Hub on
`127.0.0.1:59780` and native Node on `127.0.0.1:59787`. All session probes used
`kind=terminal`, `driver=shell-pty`, and `/bin/sh`; no model request was made.
Local paths below are normalized to `$RUN` and `$HOME`. Access codes and device
credentials are omitted.

## Before and after the reported failure

A retained pre-feature executable reproduced the original failure through
`POST /v1/instances` with `cwd: "~"`:

```text
HTTP 400 BAD_REQUEST
cwd $RUN/baseline-root/~ is not a directory
```

The baseline executable's SHA-256 was
`1c3985209dbbd7664690018f47f2775a75d254749b9eedf0b5ef052ce0aab5f4`.
It was an existing local executable, not an independently rebuilt historical
commit. The matching literal-child resolution is also present in the initial
branch base `9dd7ec7`, `crates/remuda-node/src/worktree.rs`.

The feature executable returned this response for the same request while HOME
itself was outside the registered project roots:

```text
HTTP 400 BAD_REQUEST
cwd $HOME/ is outside registered workspaces: $RUN/baseline-root.
Register an absolute project path in Hosts > Add directory or
POST /v1/hosts/<hostId>/workspaces.
```

This proves expansion uses the Node's HOME while preserving containment.
Registration remains an explicit action; expansion does not authorize a new
root. Unit tests also cover `$HOME`, `$HOME/subpath`, and unsupported `~user`
without executing a shell expansion.

## Real API and native-process evidence

The initial native run used host
`hst_01a099f4-c414-767e-9dbe-8d3fe2fb97b7` and registered project
`wsp_01a099f5-f30c-777a-b962-632d3a727e24`.

| Check | Observed result |
|---|---|
| GET host workspaces | Startup project advertised at registry revision 1 |
| POST relative path | HTTP 400; absolute path required |
| POST `/etc` outside default HOME boundary | HTTP 400; error identifies `workspace_roots` |
| POST existing `$RUN/project` | HTTP 200 after Node prepare/commit; revision 2, canonical root and stable workspace ID returned |
| Repeat POST same canonical root | One membership, same ID and revision; successful idempotent registration |
| Create using registered ID and no cwd | Native terminal `ins_01a099f5-f4c3-7206-8ed4-de6c931767a1` started in the selected project |
| Native cwd | Sent `pwd -P > .cwd-proof-register` through `tty.write`; the shell-created file contained canonical `$RUN/project` |
| Close | Observed the instance reach `lifecycle=exited` |
| Durable Node membership | `node/workspaces.json` held revision 2, two memberships, and two settled registration receipts |
| Durable Hub outcome | Both successful registration commands were `settled/clear`; revision 2 was persisted in `workspace_journal` |
| Rejected registrations | Hub retained explicit rejection audit events and did not mark the rejected requests accepted |

HTTP acceptance of terminal input was not used as native completion evidence:
the shell-created cwd file and the final instance lifecycle supplied that proof.

## Restart and unregister

The same data directory was reopened with only the original startup workspace
flag. `GET /v1/hosts/{hostId}/workspaces` still returned revision 2 and the same
API-added project ID. Session
`ins_01a099fc-3790-733c-b819-746056fb0291` launched with
`cwd: "~/<home-relative project>"`; its native `pwd -P` proof again matched the
canonical project directory, and close reached `exited`.

DELETE then settled command `cmd_01a099fc-3ac2-7512-85a4-917fc383f2f6` at
revision 3. Only the original startup project remained. New create requests
using either the deleted workspace ID or its absolute cwd returned HTTP 400;
the first identified an unregistered ID and the second listed the remaining
root plus registration guidance. Node membership and Hub `workspace_journal`
agreed on revision 3.

The implementation also has regressions for deleted/unmounted extra projects
remaining removable after restart, root symlink drift, tightened allowlist
admission, and delayed unregister commands racing replacement registrations.
SSH bootstrap selects Node HOME explicitly instead of implicitly treating its
private transport `/tmp` directory as a permitted project. The real uploaded
Node/SSH transport regression passed after that correction.

## Reproduction configuration

Create separate existing directories under the Node user's HOME so the default
allowlist applies. Prepare a private access-code file without printing it.
`$RUN` is a temporary acceptance directory, separate from the project root being
registered.

```sh
mkdir -p "$RUN/baseline-root" "$RUN/project" "$RUN/e2e-project"
REMUDA_ALLOWED_ORIGINS='["http://127.0.0.1:59789","http://localhost:59789"]' \
SHELL=/bin/sh "$CARGO_TARGET_DIR/debug/remuda" \
  --data-dir "$RUN/live" dev \
  --workspace "$RUN/baseline-root" \
  --hub-listen 127.0.0.1:59780 --port 59787 \
  --web-origin http://127.0.0.1:59789 \
  --access-code-file "$RUN/access-code"
```

Pair a Human device with `POST /v1/login`, then use its token or cookie:

1. `GET /v1/hosts/{hostId}/workspaces`.
2. `POST /v1/hosts/{hostId}/workspaces` with `{"path":"<absolute project>"}`.
3. `POST /v1/instances` with the returned `workspaceId`, the host ID,
   `kind:"terminal"`, and `driver:"shell-pty"`; omit cwd to exercise selection.
4. Inspect native cwd and close the instance. Restart the same dev command with
   the same data dir, leaving only the original startup `--workspace` flag.
5. Confirm the API-added project's ID is unchanged, create another terminal with
   its `~/<home-relative project>` cwd, and close it.
6. `DELETE /v1/hosts/{hostId}/workspaces` with the canonical project path; verify
   subsequent new-session admission refuses its old workspace ID and cwd.

Startup flags are merged on every start. Removing a root from the registry does
not remove a still-configured startup flag: that flag re-registers it on the
next start. Unregister does not delete project files or stop existing sessions.
For projects elsewhere, configure `[node].workspace_roots`,
`REMUDA_WORKSPACE_ROOTS` (JSON array), or repeated `--workspace-root` explicitly.
The same allowlist applies to startup and API registration.

## Browser acceptance

Ran the required command against the owned native dev process, using Chrome:

```sh
cd web
HUB_E2E_EXTERNAL=1 \
VITE_HUB_URL=http://127.0.0.1:59780 \
HUB_E2E_WEB_PORT=59789 \
HUB_E2E_ACCESS_CODE_FILE="$RUN/access-code" \
HUB_E2E_WORKSPACE="$RUN/e2e-project" \
pnpm run test:e2e:hub
```

Final result against the rebuilt executable after rebase: **6 passed, 1 skipped
in 54.6 seconds**. The skipped test is the explicitly fake-Node
model echo/approval scenario; the replacement external test uses the real
native shell. The native workspace flow passed these checks:

- Register an existing absolute directory through New Session's add action and
  select the returned canonical workspace ID.
- Observe a second browser tab receive registration through `host.updated`.
  Its `/v1/hosts` HTTP responses are frozen to an old snapshot, proving the live
  event is required and stale polling cannot undo it.
- Create a `shell-pty` session with an empty subpath and verify a shell-generated
  `WORKSPACE_REG_CWD=$PWD` marker contains the registered canonical root.
- Reload with a device cookie, close the session, and wait for `exited`.
- Remove the directory from Hosts and observe its picker option disappear live
  in the other tab; reload New Session and confirm it remains absent.

The browser-created session was
`ins_01a09a1b-9967-74b7-8073-dbba162ca0d3`, with workspace
`wsp_01a09a1b-846b-770c-91a3-36eb7041d6f1`. Follow-up API inspection confirmed
`terminal` / `shell-pty`, `lifecycle=exited`, host revision 15, and the removed
e2e project absent from the registry. All native sessions created in that
project across the repeated browser runs were exited.

The five pairing/authentication tests also passed. Repeating the suite against
the persistent dev data directory exposed a fixture name collision; the pairing
test now uses a unique device name, matches that exact device, and logs it out
after its reload assertions. A later run reached a live native shell but failed
to click xterm's hidden helper textarea outside the viewport. The test focuses
the named Terminal input and asserts focus before sending real keyboard input;
the shell-generated cwd marker remains the completion check. The failed
attempt's session was closed and its registration removed before repeating.
The first browser attempt
identified an acceptance configuration error: `--web-origin` configures the
Node, while the Hub also requires `REMUDA_ALLOWED_ORIGINS` for the separate
Vite origin. After restarting with the configuration above, the complete suite
passed. Vite emitted proxy/WebSocket `EPIPE` diagnostics during the run; all
live update, terminal, closure, and authentication assertions passed. Socket
teardown is a possible explanation, not independently proven for each diagnostic.

## Final validation

Validation was repeated after integrating `origin/main` at `2f569cb`, including
the newly merged Claude stream identity changes.
The new main-branch Herdr isolation test explicitly authorizes its own temporary
fixture directory; production isolation behavior is unchanged by this feature.

| Required check | Result |
|---|---|
| `cargo fmt --all` and formatting check | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo test --workspace --locked` | PASS: 741 passed, 13 existing ignored tests |
| `cargo build -p remuda --locked` | PASS |
| `pnpm test` | PASS: 151 tests in 45 files |
| `pnpm build` | PASS |
| `pnpm lint` | PASS: four existing warnings, no errors |
| `pnpm run test:e2e:hub` against owned native dev | PASS: 6 passed, 1 intentional fake-Node skip |
| `pnpm gen:api` | PASS; generated API client current |
| `./scripts/gen-skill.sh --check` | PASS |
| `./scripts/ci/secret-scan.sh` | PASS |
| `git diff --check` | PASS |

The Rust suite includes 14 dedicated Node registry/expansion/race tests,
Hub operator-route and persistence integration, agent 403 coverage, operational
Hub↔Node wire round trips, and selected-project worktree dispatch. The schema
catalog check excludes these operational types alongside the existing `hubnode`
frames; REST types are described in OpenAPI and the regenerated web API client.
Cargo dependency manifests/lockfile and the pnpm lockfile were unchanged.

## Coordinator rebase onto `68684d5`

The integration with the daemon access changes retains bounded workspace
probes before registry canonicalization and cwd admission. Probe failures keep
their Full Disk Access guidance; failed probes are not followed by an unbounded
containment retry. Worktree sibling checks distinguish absent directories from
access failures and resolve symlink boundaries in a killable subprocess.

Host diagnostics now inspect the current registered roots, including multiple
roots and an empty registry, while retaining guarded configuration preflight.
Unregistering the initial workspace removes it from diagnostics. Installer
guidance covers additional `--workspace` flags as well as the primary directory.
Main's temporary-directory fixtures explicitly permit their own roots and use
canonical paths. The web retains both workspace management and host diagnostics,
plus main's message normalization changes.

The earlier native-run evidence above remains tied to its stated executable and
base. This coordinator rebase uses the requested Rust and web validation gates.

| Rebase check | Result |
|---|---|
| `cargo fmt --all` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo test -p remuda-node -p remuda-hub --locked` | PASS: 244 tests |
| `cargo test -p remuda --test node_install_cli --locked` | PASS: additional-workspace installer guidance |
| `pnpm test` | PASS: 169 tests in 46 files |
| `pnpm build` and `pnpm lint` | PASS; four existing lint warnings |
| `./scripts/ci/secret-scan.sh` | PASS |

The initial rebased commit exposed an unused cwd resolver under Clippy. Omitted
cwd requests now use that shared resolver, preserving the final bounded access
check; explicit cwd requests retain HOME expansion and registry containment.
The results above include this correction.
