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

<!-- EVIDENCE-RUN -->
