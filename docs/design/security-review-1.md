# Security review 1 — Hub, driver, SSH, Feishu, secret-scan

Date: 2026-09-12  
Scope: committed code in `crates/remuda-hub`, `crates/remuda-driver`, `crates/remuda-ssh`, `crates/remuda-feishu`, `scripts/ci/secret-scan.sh` (+ `secret-scan.py`).  
Method: adversarial read of auth, cookies, Origin/Host, Node tokens, journal mirror, launch materializer, SSH argv, Feishu allowlists, scanner coverage.  
Line numbers refer to the tree at review time (before follow-up `fix(*)` commits).

Severity:

- **P0** — exploitable with the current API without extra bugs; integrity or authz break.
- **P1** — realistic exploit path or secret handling defect; should not ship as-is.
- **P2** — defense-in-depth, single-user residual, or config footgun.

---

## P0

### H1. Node `journal.append` / `ensure_instance` does not bind instance → host

- **Where:** `crates/remuda-hub/src/store.rs:448-471` (`ensure_instance` returns any existing row), `crates/remuda-hub/src/store.rs:625-680` (`append_journal` only checks instance exists), `crates/remuda-hub/src/ws.rs:326-356`.
- **Exploit:** Node A authenticates. Node B (or A after learning an `ins_…` id from Hub HTTP) sends `journal.append` with Host B’s `instanceId`. `ensure_instance` short-circuits on the existing row and **does not compare `host_id`**. Hub appends into another host’s journal; `/v1/follow` and `/v1/instances/:id/journal` then show attacker-controlled observations (fake tool results, fake approvals, fake completion).
- **Fix:** Reject when `existing.host_id !=` the authenticated Node’s host. Pass `host_id` into `append_journal` and fail closed. Same check on `tty.frame` (`ws.rs:357-371`) and `mark_settled` (`ws.rs:372-384`), which currently take a client-supplied `instanceId` / `commandId` with no host match.

### S1. SSH `Host` alias is passed as a raw `ssh` argv token

- **Where:** `crates/remuda-ssh/src/target.rs:35-41` (only rejects whitespace/NUL), `crates/remuda-ssh/src/client.rs:152-160` (`cmd.arg(&self.alias)` before `--`).
- **Exploit:** Alias `-oProxyCommand=touch /tmp/pwn` (or `-e`, `--help` with extra `-o`) is parsed as an OpenSSH **option**, not a destination. `ssh -G` and `ssh -T … <alias> -- …` then run the attacker’s `ProxyCommand` locally as the Hub/Node user.
- **Fix:** Allow only a single OpenSSH-host token: `[A-Za-z0-9][A-Za-z0-9._-]*`, no leading `-`, no `=`. Validate in `resolve_with` and `SshClient::command`.

---

## P1

### H2. Bootstrap token compared with `!=` (timing)

- **Where:** `crates/remuda-hub/src/http.rs:88` (`POST /v1/login`), `crates/remuda-hub/src/store.rs:301` (Node enroll).
- **Exploit:** Device and Node bootstrap secrets are high-value. Byte-at-a-time timing on the plaintext compare leaks them over a fast LAN (loopback Hub in `remuda dev` still matters if another local process can hit the port).
- **Fix:** Constant-time equality of the raw bytes (length included in the accumulator). Device/host tokens already use Argon2id.

### H3. Bootstrap file created world-readable then chmod’d

- **Where:** `crates/remuda-hub/src/auth.rs:47-51` (`std::fs::write` then `set_permissions(0o600)`).
- **Exploit:** Default umask `022` creates `bootstrap-token` as `0644` for a window. Any local user can read the enroll secret and mint device cookies or extra Nodes (H1 then applies).
- **Fix:** `OpenOptions` with `mode(0o600)` on create; `fsync`. Apply `0600` to `hub.sqlite` after open (`store.rs:713`).

### H4. Device cookie is `SameSite=Lax`, not `Strict`

- **Where:** `crates/remuda-hub/src/auth.rs:73-78`.
- **Exploit:** Lax sends the cookie on top-level GET navigations from other sites. Combined with `origin_allowed` treating **missing Origin as allow** (`auth.rs:105-112`), a non-browser or odd client that echoes the cookie without Origin can CSRF mutating POSTs. Browser cross-site POST is mostly mitigated by Lax; Strict removes the GET navigation case for a same-origin PWA.
- **Fix:** `SameSite=Strict`. Keep missing Origin allowed only for Bearer/Node (no device cookie). Optional: reject `http://` Origins when `cookie_secure` is true.

### H5. Bootstrap secret is immortal

- **Where:** `crates/remuda-hub/src/store.rs:301-328`; README enrollment.
- **Exploit:** After the first Node enrolls, the same bootstrap still enrolls **unlimited additional hosts**. Leak of `data-dir/bootstrap-token` (or `REMUDA_BOOTSTRAP_TOKEN`) is a standing join token, not a one-time secret.
- **Fix:** Owner decision (rotate / one-shot / count limit). Not patched here — product policy.

### D1. Bot origin still allows `dontAsk`

- **Where:** `crates/remuda-driver/src/materializer.rs:435-439` (only `BypassPermissions`), `crates/remuda-driver/src/claude_print.rs:1519-1527` (same).
- **Exploit:** Dispatcher/bot `InstanceSpec` with `permission_mode: dontAsk` materializes `--permission-mode dontAsk --permission-prompts none`. Tools run without Interaction. `bot-dispatcher.md` / plan M2: bot must never bypass human approval. `dontAsk` is the M0 debt path (`TD-M0-PERM-01`), not a bot feature.
- **Fix:** Reject `DontAsk` and `BypassPermissions` when `origin` is `Bot` or `Agent`.

### F1. Card callback omits `chat_id` → chat allowlist skipped

- **Where:** `crates/remuda-feishu/src/inbound.rs:537-545`.
- **Exploit:** If `chat_allowlist` is non-empty but the consume line has empty/`null` `chat_id`, only `operator_id ∈ owner_open_ids` is checked. A click (or injected consume line) from an owner in a **non-allowlisted** group is admitted. Schema usually has `chat_id`; fail closed anyway.
- **Fix:** Non-empty allowlist + missing/empty `chat_id` → `DropReason::ChatNotAllowed`.

### F2. `@bot` detection uses substring `name.contains`

- **Where:** `crates/remuda-feishu/src/inbound.rs:596-610`.
- **Exploit:** `bot_name = "Remuda"` matches mention display name `"Remuda's intern"` / `"notRemuda"`. Group messages that `@` a similarly named user are treated as addressed to the bot and become commands.
- **Fix:** Exact mention `name` (and exact `@Name` token in content).

### F3. Card update `token` is in `Debug`

- **Where:** `crates/remuda-feishu/src/inbound.rs:75-80` (`#[derive(Debug)]` on `CardAction`).
- **Exploit:** `token` is valid 30 minutes / 2 card updates. `ConsumeEvent` / tracing `:?` dumps the secret into logs; a log reader can `card/update` without a second click.
- **Fix:** Custom `Debug` that redacts `token`.

### C1. `secret-scan` needles are too few

- **Where:** `scripts/ci/secret-scan.py:64-87`; wrapper `scripts/ci/secret-scan.sh`.
- **Exploit:** Scanner misses `REMUDA_BOOTSTRAP_TOKEN=`, `xai-`, `ghp_`, `github_pat_`, `OPENAI_API_KEY=`, PEM private keys, Feishu `app_secret`. Those can land in git while CI is green. `sk-` / `agk_` / `Bearer ` / `ANTHROPIC_AUTH_TOKEN=` / `api_key` only.
- **Fix:** Add those patterns with the same concatenation trick so the scanner file does not match itself. Keep dummy/placeholder allowlist.

---

## P2

| ID | Where | Note |
| --- | --- | --- |
| H6 | `auth.rs:105-112` | Missing `Origin` allowed so Node/curl work. With `SameSite=Strict` this is residual. Prefer requiring Origin whenever a device cookie is present on POST. |
| H7 | `store.rs:234-266`, `269-300` | Linear Argon2 verify over all devices/hosts — DoS as the table grows; also first-match wins. |
| H8 | `ws.rs:476-527` | Follow snapshot is any `instanceId` after device auth. Fine for single-owner Hub; not tenant isolation. |
| H9 | `auth.rs:13-20` | Argon2id 8 MiB / 1 pass is light for online guessing if sqlite leaks. |
| H10 | `http.rs:81-101` | No login rate limit; bootstrap is still guessable if short/dev. |
| H11 | `config.rs` listen default `127.0.0.1` | Good; production bind + reverse proxy must not strip `Origin`. |
| D2 | `flags.rs:50` `mcp-config` / `add-dir` | Extra argv can point Claude at attacker-controlled MCP or extra trees. Keep allowlist tight; owners should not take these from bot text. |
| D3 | `recipe.rs` `secret_ref` | Recipe stores `env:NAME` / `file:PATH` spellings, not values. Do not log full `InstanceSpec.env` literals at spawn. |
| S2 | `bootstrap.rs:179-184` | Remote path charset allows spaces/metacharacters; mitigated by `sh_single_quote`. Leading `-` on the remote binary name is a remote-exec quirk, not local ssh injection. |
| F4 | `tickets.rs` | `answer_card` does not re-check `operator_id`; depends on `admit()`. Hub must not call tickets without admit. |
| F5 | `consume.rs:57` `extra_env` | Test hook. Never pass process env through from Hub config blobs. |
| C2 | `secret-scan.py` | No git-history scan; `MAX_BYTES` skip; `.lock` skipped. |

---

## What looks correct

- Device/host **verifiers** are Argon2id PHC strings; Hub does not store raw device/node tokens after enroll (`store.rs` hosts/devices).
- Node hello before other methods (`ws.rs:184-192`).
- Command forward intent is sticky (`mark_forward_intent`); reconnect does not auto-resend (tested).
- Materializer writes overlays `0600`, launch dir `0700`; argv redacts settings paths; banned flags are token matches (`flags.rs`).
- `SecretRef` requires `env:` / `file:` / `helper:` schemes.
- SSH remote argv is `ssh … alias -- arg1 arg2` (no shell) except bootstrap `sh -c` with single-quoting.
- Feishu empty `owner_open_ids` fails closed (`is_owner` is `any`).
- IM idempotency uses `message_id`, not `event_id`.

---

## Patch plan (this review)

| Finding | Patch |
| --- | --- |
| H1–H4 | Already on `main` as of `d85c77c` (`secret_eq`, `0600` bootstrap/sqlite, `SameSite=Strict`, instance/journal/command host bind, `tty.frame` host check). Not re-committed here. |
| H5 | leave to owners |
| D1 | `fix(remuda-driver)` — `dontAsk` gated with bypass for `LaunchOrigin::Bot` |
| S1 | `remuda-ssh` was not committed at review time; `validate_alias` is in the local crate tree for the owner. Not committed here. |
| F1 F2 F3 | `fix(remuda-feishu)` |
| C1 | `fix(scripts)` `scripts/ci/secret-scan.py` |
