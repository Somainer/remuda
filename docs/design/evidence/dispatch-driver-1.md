# Dispatch driver fidelity: why `claude-pty` on the roster ran `claude-print`

Date: 2026-09-17. Scope: `remuda dispatch --harness claude` to an `ssh-stdio` Node
running with `REMUDA_PTY_CARRIER=native`.
Design references: [D-028 / D-028a](../decisions.md),
[native PTY first §5.1](../native-pty-first.md), [coordinator hierarchy §2.4](../coordinator-hierarchy.md).

## Symptom

The roster row and the Hub instance record both said `claude-pty`. The Node ran
`claude-print`: the journal's `source.driverKind` was `claude-print`, and the
process was a direct child of `remuda node --stdio` with argv
`claude -p --input-format stream-json`. No journal entry anywhere recorded a
driver change. `claude-print` is never a legitimate default (D-028: print exits
after one turn, so a worker on it cannot be nudged, steered, or watched).

## Root cause: a silent `unwrap_or` on a spec deserialization failure

The downgrade was **not** a policy decision. Nothing chose `claude-print`. The
Node's create path failed to parse the Hub's spec and fell back to a
hardcoded default request whose `driver` field is `DriverKind::ClaudePrint`.

Two independent defects compose:

### 1. The Hub sends explicit JSON `null`s for absent optional fields

`worker_launch_spec` (`crates/remuda-hub/src/workers.rs`) builds the spec with
`json!`, so an absent value becomes `Value::Null`, not an absent key:

```rust
let mut spec = json!({
    "kind": harness,
    "driver": driver,
    "model": model,                       // Option<String> -> null when None
    "providerProfileId": provider_profile_id,
    "delegation": delegation,
    ...
    "prompt": null,                       // always null on the dispatch path
    ...
});
```

`prompt` is *unconditionally* `null` here — dispatch never sends an inline
prompt, because the brief travels as an object attachment. `model` and
`providerProfileId` are `null` whenever supply admission did not pin one.

### 2. `serde(default)` does not apply to an explicit `null`

`CreateInstanceRequest` (`crates/remuda-node/src/model.rs`) declares:

```rust
#[serde(default)]
pub prompt: String,
#[serde(default = "default_model")]
pub model: String,
```

A `#[serde(default)]` fills in a **missing** key. An explicit `null` is a
present value of the wrong type, so deserialization fails with
`invalid type: null, expected a string`. Verified directly against the real
spec shape:

```
PROBE ERR invalid type: null, expected a string
```

### 3. The failure is swallowed and replaced with a `claude-print` default

`dispatch_create` (`crates/remuda-node/src/transport/hubnode_codec.rs`) discards
the parse error entirely:

```rust
let mut request: CreateInstanceRequest = match serde_json::from_value(spec_for_request) {
    Ok(request) => request,
    Err(_) => CreateInstanceRequest {          // <-- the whole Hub spec is dropped
        kind: AgentKind::Claude,
        driver: DriverKind::ClaudePrint,       // <-- the observed downgrade
        model: "fake".into(),
        ...
    },
};
request.apply_spec_launch_fields(&spec);
```

`apply_spec_launch_fields` then re-reads a handful of launch fields off the raw
spec (`binaryPath`, `delegation`, `providerProfileId`, `effort`, `tui`, …) — but
**not `kind` and not `driver`**. So the requested `claude-pty` was never
recovered, and the hardcoded `claude-print` survived to
`validate_kind_driver`, which accepts `(Claude, ClaudePrint)`, and then to
`registry.build`, which happily constructed a print driver.

The `Err(_)` arm exists so an old or partial Hub spec still launches something.
That tolerance is what turned a fully-formed, valid dispatch spec into a
silently different product.

**Answer to "where and why":** in the Node stdio runtime, at the
`dispatch_create` spec-deserialization fallback — not in the registry (whose
`build` already fails closed when a kind is unregistered) and not in the Hub
spec's driver choice (which correctly said `claude-pty`). The herdr session
ownership was never involved.

## Second defect: `driver_for` could not pick `shell-pty` at all

Independent of the downgrade, the Hub's default was wrong for this host.
`driver_for` only ever looked at whether herdr was advertised:

```rust
let has_herdr = host.herdr.as_ref().is_some_and(|value| !value.is_null());
if has_herdr { "claude-pty" } else { "claude-print" }
```

This Node advertises `capabilities.driverInventory[] = [{kind: "shell-pty",
launchable: true, reasonCode: "carrier-native"}]` (it runs with
`REMUDA_PTY_CARRIER=native`), which `driver_for` never read — so the native,
screen-readable carrier could never be the default. And the `else` arm made
`claude-print` a default, which D-028 forbids outright.

## Fixes

1. **Hub `driver_for`** reads `capabilities.driverInventory` first and prefers
   `shell-pty` when the Node reports it launchable; then `claude-pty` when herdr
   is advertised; and `claude-print` is **never** a default — a host with
   neither is refused as unsatisfiable with a reason, rather than downgraded.
2. **Hub explicit `--driver`** is honoured verbatim or refused (HTTP 409, non-zero
   CLI exit) with a reason naming what the host can actually launch. It is never
   silently replaced.
3. **Node** no longer swallows the spec parse error for the fields that decide
   *which product runs*. `kind` and `driver` are recovered from the raw spec even
   on the tolerant path, and an unparseable/unconstructable requested driver
   fails `instance.create` with a reason code instead of downgrading. Any
   retained downgrade must journal a `driver_downgraded` lifecycle event carrying
   `from`, `to` and `reason`.
4. **Roster + instance record** take the driver from the Node's create result, so
   they name the driver that actually ran; `remuda watch` prints it.

## Also fixed on the same dispatch path

- **Native-login profiles at launch.** A provider profile of kind `native` holds
  no secret by design (`secret_name: None`). But `provider_resolve::apply_to_spec`
  classified delegation by `kind == "direct"` and sent everything else to
  `"gateway"`, so a native profile referenced by its real `pvp_…` id got
  `delegation: "gateway"`. `with_launch_secret` then saw a non-`none` delegation
  and a real profile id — its guard only matched the *literal* strings `"none"` /
  `"native"`, not a native-kind row — fetched the record and called `load_secret`,
  producing HTTP 400 `provider profile has no stored auth token`. Native profiles
  now resolve to `delegation: none` end to end, so the CLI on the host uses its
  own login and no `providerAuthToken` reaches the Node.
- **Rollback on failed dispatch.** That 400 landed *after* `worker.provision` had
  created the worktree and target dir on the Node, and before any roster row
  existed — so `remuda retire` had nothing to reclaim and cleanup was manual.
  Dispatch now calls `worker.remove` for what it provisioned when any later step
  fails.

## Evidence: a real dispatch on this host

Captured on the devbox with `REMUDA_PTY_CARRIER=native`, real Hub, real Node,
real Claude binary (`2.1.274`). Reproduce with `remuda dev --hub-listen … --listen …`
plus a project, then the commands below.

### 1. What the host reports it can launch

The Hub's own host view, i.e. what `driver_for` now reads:

```
herdr advertised: true
driverInventory: [{"kind": "shell-pty", "launchable": true, "reasonCode": "carrier-native"}]
```

### 2. `dispatch` with no `--driver` takes the native carrier

```
$ remuda dispatch --project $PRJ --brief brief.md --name evnat2
  roster/instance driver = shell-pty
```

Before this change the same command produced `claude-pty` on the roster (the
herdr arm, since `driver_for` never read the inventory) and `claude-print` on
the Node.

### 3. The instance record and the journal agree with the roster

```
instance.driver = shell-pty
source.driverKind histogram: {'shell-pty': 4}
```

The instance row and every journal event carry the driver that actually ran —
the `driver_downgraded` diagnostic is emitted only when they differ, and here
they do not.

### 4. `remuda watch` prints it

```
NAME    STATUS   DRIVER     SHA/REASON  DETAIL
evnat2  working  shell-pty  -           -
```

The DRIVER column is new: a worker on the wrong carrier misbehaves in ways the
status column cannot explain (a print worker looks idle forever, because print
exits after one turn).

### 5. The screen is readable through `remuda instance read --source screen`

```
source: screen | screenSource: raw-ring | cols x rows: 80 x 24
 | ────────────────────────────────────────────────────────────────
 | Accessingworkspace:
 | /tmp/…/remuda-wt/evnat2
 | Quicksafetycheck:Isthisprojectyoucreatedoroneyoutrust?(Likeyour
 | owncode,awell-knownopensourceproject,orworkfromyourteam).Ifnot,
 | takeamomenttoreviewwhat'sinthisfolderfirst.
 | ClaudeCode'llbeabletoread,edit,andexecutefileshere.
 | Securityguide
 | ❯No,exit
 | Yes,Itrustthisfolder
 | Entertoconfirm·Esctocancel
```

This is the whole point of preferring `shell-pty`: a live, scrollable screen.
A `claude-print` worker has none.

### 6. A refusal is a refusal on a stdio Node, not a downgrade

Driving a real `remuda node --stdio` with `REMUDA_PTY_CARRIER` off (the exact
configuration that used to end in `claude-print`), its inventory reports:

```
driverInventory: [{"kind": "shell-pty", "launchable": false, "reasonCode": "carrier-not-enabled"}]
```

and the creates are answered with reason codes:

```
instance.create driver=claude-teleport → REFUSED: unsupported-driver: this node has no driver claude-teleport
instance.create kind=cursor            → REFUSED: unsupported-kind: this node cannot launch kind cursor
instance.create driver=codex-appserver → REFUSED: kind and driver do not describe the same native product
```

No `accepted` reply, and no substituted driver.

### 7. Explicit `--driver` is honoured or refused

On a host reporting `carrier-not-enabled`:

```
$ remuda dispatch … --name evref4 --driver shell-pty
Error: hub HTTP 409: {"error":"host … does not report shell-pty as launchable; set
REMUDA_PTY_CARRIER=native on the Node or dispatch without --driver"}
$ echo $?
1
```

The same host with no `--driver` picks `claude-pty` (herdr is advertised), never
print — and `--driver claude-print` still works when an operator names it:

```
evdef3  working  claude-pty    -  -
evpr3   working  claude-print  -  -
```

(Note: the refusal's JSON `code` string reads `COMMAND_ID_CONFLICT` because the
Hub maps every `Conflict` variant to that discriminant in `error.rs`. The HTTP
status, 409, and the CLI's non-zero exit are correct; the code string is a
pre-existing mislabel, not part of this change.)

## Addendum (owner ruling, 2026-09-17): print is explicit-only

The owner tightened the rule the same day: `claude-print` is useless as a worker
carrier — a session ends after one turn and needs a manual resume — so it is
removed from **every** default and fallback path, not just `driver_for`. That is
recorded as **D-035** and implemented as:

- **Hub.** `driver_for` prefers `shell-pty` when the inventory reports it
  launchable, then `claude-pty` when herdr is advertised, and refuses with a
  reason when neither exists. `POST /v1/instances` no longer falls back to
  `claude-print`: an omitted driver defaults to `claude-pty` (multi-turn, and not
  a shell driver, so the existing agent-approval gate for such a request is
  unchanged). The dispatch path was also carrying a hardcoded `claude-pty` in its
  *placement probe*, which rejected every host without herdr before `driver_for`
  ever ran — it now probes `shell-pty`, which carries no herdr requirement.
- **Node.** A requested `claude-pty` / `shell-pty` that cannot be constructed
  refuses `instance.create` with a reason code (`unsupported-driver` /
  `unsupported-kind`), before the command is durably accepted. It never
  downgrades. Evidence §6 above is exactly this.
- **Print stays selectable, labelled.** `--driver claude-print` and the web
  picker's explicit choice still work; the picker label now reads as a diagnostic
  carrier. The web New Session default already followed `driverInventory`
  (`shell-pty` when launchable, else `claude-pty`); its persisted `prefs.driver`
  no longer seeds `claude-print`.
- **Docs.** `docs/design/protocol.md` (the `DriverKind` block) and
  `docs/design/native-pty-first.md` (the kind/driver matrix) carry the D-035
  ordering note.

## What this does not do

- `shell-pty` selection on the `POST /v1/instances` path (New Session) is left to
  the caller: that path falls back to `claude-pty` rather than resolving against
  the chosen host, because resolving it there would collide with the
  agent-approval gate in `agent_scope.rs` (a `shell-pty` create for an agent
  caller requires human approval, which the create path cannot request before
  placement has chosen a host). The web New Session picker already derives its
  default from `driverInventory`, so first-party callers name the carrier.
- `driver_kind_for_args` in `http.rs` still falls back to the Claude arg
  allowlist for an unknown driver. That widens which flags are *validated*
  locally; the Node re-validates with the real driver, and it never chooses a
  carrier.

