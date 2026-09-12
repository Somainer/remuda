# SSH host onboarding 1

Date: 2026-09-13 (Asia/Shanghai). Agent: c-sshhost.

The browser added the real remote SSH host, saw it online with CLI inventory,
created a remote Codex instance, received **PONG**, stopped the instance, and
removed the host. The PONG used the gateway explicitly authorized by the user.
All remote hostnames below are redacted as `<sg-host>`.

## Runtime and browser

- Own development Hub `127.0.0.1:58680`, Node `127.0.0.1:58687`.
- `REMUDA_COOKIE_SECURE=0`; random access code in a private local file.
- Local data and workspace under `/tmp/remuda-c-sshhost/`.
- Chrome through the project's Playwright dependency, viewport **390 × 844**,
  mobile and touch enabled. This is mobile browser emulation, not a physical phone.
- Browser used the access-code login, Hosts “添加主机” form, new-session form,
  session Stop button, and host detail “移除主机” button.
- Remote: Linux x86_64; `remuda` absent on PATH. A locally built static
  `remuda-node-stdio` artifact was supplied through `REMUDA_SSH_UPLOAD_BINARY`.
  The Hub uploaded it into its private `/tmp/remuda-ssh-<hostId>/remuda`, verified
  SHA-256, checked `remuda 0.1.0`, and started its native stdio runtime.
- Existing remote user configuration was read only. Runtime, workspace, Herdr
  socket, Codex state and temporary gateway configuration were under that
  `/tmp/remuda-ssh-<hostId>/` directory. No remote service was installed.

The form submitted this request (target redacted):

```json
{
  "target": "<sg-host>",
  "label": "<sg-host>",
  "labels": ["egress:gateway", "region:sg"],
  "remuda_binary_policy": "upload_if_missing"
}
```

POST returned 201 with `state: connecting`. Polling `GET /v1/hosts` then returned
`state: online`, `online: true`, `transport: ssh-stdio`, `lastError: null`, labels
`egress=gateway` and `region=sg`, Node 0.1.0, Claude 2.1.221, Codex 0.147.0, and
Herdr 0.8.2. Grok was absent. Inventory's Codex login probe initially reported
logged out; that probe is not proof of whether a subsequently configured custom
gateway can answer a request.

![Mobile host row showing online inventory](./ssh-host-1-online.png)

This image is the actual browser host-row crop. Shell prompts, SSH addresses,
private filesystem paths and credentials are excluded.

## Authentication and model discovery

The first browser-created remote Codex instance reached its login screen. The
remote had no Codex credential file and no Grok executable. As requested, this
authentication block was recorded and local Codex credentials were not migrated.
The instance was stopped and the host was removed through the browser.

The user then authorized the gateway URL and key in a local Claude
settings overlay, and requested model discovery instead of
Haiku. The remote had no file at the same relative path. The cited local file's
URL and key were sent over SSH stdin to the remote probe; neither was printed.
Both `/models` and `/v1/models` returned HTTP 200 with these model IDs:

```text
claude-fable-5.1
claude-gpt-5.6-sol
claude-gpt-6-astra
claude-grok-4.6
claude-opus-4-8
claude-opus-5
```

Only `claude-gpt-5.6-sol` was inference-tested. Model-list membership alone does
not establish that every listed model supports Codex's Responses requests.

For this acceptance only, the referenced URL and key were written to a mode-0600
Codex config in the host's temporary `CODEX_HOME`. It used a custom provider,
`wire_api = "responses"`, WebSockets disabled, and request/stream retries set to
zero. No local Codex OAuth file was copied and no permanent remote login/config
was changed. This setup is acceptance configuration, not the provider-profile UI
owned by x-prov.

## Remote PONG, stop and removal

The second host was added from the same browser form:

```text
hostId:     hst_01a096da-09a4-74ea-8bb1-5e87be81e512
instanceId: ins_01a096db-2c60-75f9-8d00-15fe238f655d
kind:       codex
driver:     generic-pty
model:      claude-gpt-5.6-sol
prompt:     Reply with exactly PONG and do not use tools.
```

The create API returned this hostId and command state `accepted`. At
`2026-09-12T18:22:35.555Z`, Hub journal sequence **12** contained an independent
assistant line **`• PONG`**, followed by sequence 13 with Herdr state `idle`.
This is screen-derived evidence, not a structured Codex turn-completion event.
The proof distinguishes the answer from the word PONG in the submitted prompt.

[Selected, redacted journal evidence](./ssh-host-1-pong.json) records the host,
instance, sequence, source and answer excerpt. The raw terminal also warned that
the gateway model lacked Codex metadata; it did not prevent the PONG response.

Browser Stop returned `accepted`, and the session changed to `exited`. Browser
removal returned **204**; the remote host row count became **0**. Both acceptance
host directories, including temporary gateway material, were then removed from
the remote. API removal itself cancels supervision and preserves remote files;
the evidence cleanup explicitly removed only this task's two temporary roots
and terminated the two isolated Herdr servers and their remaining login shells.
Ownership was checked against their temporary paths in process arguments, cwd
and environment; other remote sessions were not selected.

## Supervision and regression evidence

The Hub owns SSH directly. It already owns placement, the durable registry and
Node command dispatch; putting a bastion in the local Node would add a second
supervisor and a dependency on that Node's lifetime. The existing SSH carrier
and Hub Node dispatcher are reused. No Hub bootstrap token is written remotely.

- Target syntax and labels are validated; an API caller cannot supply an SSH
  executable, arbitrary SSH flags or a local upload path. SSH uses the Hub user's
  existing SSH configuration, noninteractive authentication and keepalives.
- `require_installed` reports a missing binary. `upload_if_missing` uploads a
  private compatible artifact. An incompatible installed release is not replaced.
  Overall preflight is bounded to 180 seconds; hello is bounded to 45 seconds.
- The persisted SSH row contains target, policy and last error. Reconnect delay
  grows from 1 to 30 seconds. Status is connecting/online/offline; the last error
  remains visible during another attempt and clears after successful hello.
- Fake SSH tests execute quoted remote commands locally. Hub tests cover auth,
  validation, duplicate-target rejection, remote worktree RPC, disconnect/error/
  reconnect, Hub restart, and deletion without reappearance. Two hosts with the
  same label and reported hostname retain separate identities after restart.
- The Node test uploads and executes the real compiled `remuda-node-stdio` binary,
  dispatches a native fake-Claude request, mirrors its journal, rejects deleting
  an active host (409), closes the instance and removes the host (204). A fixture
  user credential is not imported into the managed Codex home.
- A carrier regression interrupts a receive between JSON fragments with an
  outbound send and verifies that the completed inbound JSON is preserved.

Validation used only the four touched crates, `CARGO_INCREMENTAL=0` and the
agent's assigned target directory:

```text
cargo fmt --all
cargo build -p remuda-hub -p remuda-node -p remuda-ssh -p remuda --locked
cargo test -p remuda-hub -p remuda-node -p remuda-ssh -p remuda --locked
  205 passed; 0 failed; 1 pre-existing live-SSH test ignored
cargo test -p remuda-hub --test ssh_hosts --locked
  2 passed after the duplicate-display-name regression was added
cargo test -p remuda-node --test ssh_host --locked
  1 passed after the no-credential-import assertion was added
cargo clippy -p remuda-hub -p remuda-node -p remuda-ssh -p remuda --all-targets --locked -- -D warnings
pnpm gen:api
pnpm build
pnpm test
  35 files, 98 tests passed after rebasing onto the terminal-view changes
pnpm lint
  exit 0; two existing NewSessionPage set-state-in-effect warnings
```

The web commands ran in `web/`. The generated OpenAPI TypeScript is committed.

## Remaining integration boundary

Missing Herdr is detected at Hub placement for a managed SSH `generic-pty`
request. It returns 422 with a clear Herdr/no-herdr-driver reason before dispatch.
`claude-print` remains usable without Herdr. This branch does **not** claim an
automatic Codex/Grok fallback to `shell-pty`: that driver and its raw terminal
transport belong to x-term and are absent from this branch's base implementation.
The production fallback needs that implementation and launch/tty integration;
returning a successful empty shell would not prove that the requested worker ran.

The real acceptance used installed Herdr. The SSH supervision, browser flow,
remote placement and gateway PONG are verified independently of that open
fallback dependency. Inventory is collected on connection; periodic SSH-Node
inventory refresh is not added here.

## Rebase integration (2026-09-13)

The branch was rebased to current main with provider profiles, Hub security
hardening, resource cleanup, and PTY streaming retained. OpenAPI keeps every
pre-existing main path, operation and schema value together with the SSH
add/remove schemas; TypeScript was regenerated from that merged document.

The SSH supervisor no longer authenticates a reserved host with the bootstrap
secret. Each connection receives a fresh Hub-internal credential whose hash is
stored against that managed host; the credential stays in Hub memory and is not
sent to the remote. The existing WSS restriction against bootstrap reuse remains
in force. The fake-SSH integration now checks that a bootstrap-authenticated WSS
client cannot claim the registered SSH host while supervision remains online.

Startup retains the host-lost reaper. SSH hosts with old connection-time
inventory receive a fresh disconnect grace period at Hub restart, while a
persisted expired disconnect still settles instances as host-lost. Inventory and
hello clear offline_since without prematurely marking a connecting or retired
SSH host online. The stdio CLI also honors the main branch's orphan-sweep opt-out.

The earlier real-remote acceptance above is historical evidence; no remote model
or credential operation was repeated for this rebase. Main now includes the
shell-pty driver; the earlier automatic-fallback limitation is not a new blocker
for resolving and validating this branch's merge.

Validation after resolving against main `c009320`:

```text
cargo fmt --all
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p remuda-hub -p remuda-node --locked
  110 passed; 0 failed; 0 ignored
(cd web && pnpm build && pnpm test)
  36 files, 100 tests passed
./scripts/ci/secret-scan.sh
  PASS
```

The final main update changed only the Feishu crate. Hub and Node source-tree
hashes matched their tested versions; workspace check and clippy were rerun
against the final base. The existing c-sshhost target cache was reused with
`CARGO_INCREMENTAL=0`: the initial shared-cache attempt had resolved stale SSH
metadata while another agent was building there. No new target directory was
created. Main's source worktree was not edited.


## Feature-router integration (2026-09-13)

Rebased onto main's `c424751` feature-router composition. The SSH module now
owns its HTTP routes and supervisor lifetime. `lib.rs` adds exactly one module
declaration and one composition line; every existing main line remains unchanged.
The host, instance and TTY feature routers, authentication middleware and host-lost
reaper stay in main's composition. OpenAPI retains main's automatic source-module
discovery, with explicit checks for the two SSH operations.

The route-owned supervisor restores persisted registrations asynchronously and
cancels its restore task and SSH connections when the server is dropped, including
failed listener binding. Supervising tasks hold ordinary Hub state without owning
their supervisor, avoiding an ownership cycle. The fake-SSH tests now verify child
process termination on graceful shutdown, server drop and host removal, and check
that failed binding leaves no running SSH child.

Main's indexed token lookup also requires SSH credential rotation to update the
token prefix together with its hash. Both values now change in the same SQL update;
retiring a managed host clears both. The reconnect regression detected stale-prefix
authentication failures before this fix. The bootstrap credential still cannot
claim an existing SSH registration, and no credential is sent to the remote.

This integration does not repeat the historical remote model acceptance above.


Validation after the router rebase (`c424751`) and final driver/Feishu refresh
(`8539a0c`):

```text
cargo fmt --all
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p remuda-hub -p remuda-node --locked
  125 passed; 0 failed; 0 ignored
(cd web && pnpm gen:api && pnpm build && pnpm test)
  39 files, 119 tests passed
./scripts/ci/secret-scan.sh
  PASS
```

The existing `target` cache was used for the complete requested Rust checks with
`CARGO_INCREMENTAL=0`; no new target directory or source-worktree edit was needed.

The complete Rust checks were rerun after the final main refresh and again passed
all 125 Hub/Node tests. The web source tree remained identical to the version
with 119 passing tests; generated API types remained unchanged.
