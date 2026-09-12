# Protocol audit 1 — protocol.md vs implementation

Date: 2026-09-13
Scope: [protocol.md](protocol.md) against `crates/remuda-protocol/src/{hubnode,entities,enums,launch,capabilities,interaction,observation,rpc,error}.rs`,
`crates/remuda-hub/openapi/openapi.json`, `crates/remuda-node/src/{model,server}.rs` + `transport/*`,
`crates/remuda/src/cmd/{instance,fleet,mcp}.rs`, and (for §3/§4 claims) `crates/remuda-driver/`.
Method: per-method / per-entity / per-field diff of the spec tables against the Rust types and the
generated `schema/protocol.schema.json`; test coverage read from `crates/*/tests/` and in-file `mod tests`.

`docs/research` is not available on this host; only committed `docs/design/*.md` and code were used.

## 0. Summary

The **wire type layer is a faithful implementation of the spec**. Every §7.2 method, every §9.1 error
code, every §2 entity field and all 15 Observation kinds exist in `remuda-protocol` with matching wire
spellings. The generated artefacts are **current** (`cargo run -p remuda-protocol --example gen_types -- --check`
exits 0 — schema and TS are not stale).

Drift is concentrated in three places, and it is almost entirely **implementation lagging the doc**, not
the doc being wrong:

1. **The runtime layers implement a much smaller surface than the wire types describe.** The Node dev
   server answers ~22 of 46 methods; the Hub↔Node link (`hubnode.rs`) is a separate 14-method M1 wire;
   the Hub REST API leaves `CommandRequest.operation` as an unconstrained `string`.
2. **The Driver trait (§3.1) diverges structurally** from the documented interface, and 3 of 7
   DriverKinds have no `Driver` impl at all.
3. **Capability snapshots are a static transcription of the §3.3 matrix**, not the four-condition
   evidence gate §3.2 requires.

Doc-side errors found: **4**, all small and all fixed in this change (§5 below). Notably the spec's own
§12 self-assessment is accurate — the counts it claims (15 kinds, 46 methods, 47 error codes, 24 M0
codes) are all exactly right, and every §12.1 field claim verified.

Legend for the tables: **Doc** = specified in protocol.md · **Impl** = a Rust type/handler exists ·
**Test** = covered by a named test · **Drift** = name/shape/semantic divergence.

---

## 1. Wire basics, IDs, native mapping (§1)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| `Json`/`U64`/`Timestamp`/`Id`/`Digest` | ✓ | ✓ `scalar.rs` | ✓ `wire.rs::counters_and_identity_brands_preserve_wire_precision` | none — `U64` is a decimal string as specified |
| `Knowledge<T>` known/unknown/not-applicable | ✓ | ✓ tag `state`, kebab-case | ✓ `wire.rs::missing_nullable_fields_are_not_treated_as_known_absence` | none |
| `EntityMeta` (id/revision/createdAt/updatedAt) | ✓ | ✓ flattened into 6 entities | ✓ | none |
| `ActorRef` | ✓ | ✓ | ✓ | `type` field is Rust `actor_type` renamed — wire correct |
| ID prefixes `hst_`…`epoch_` | ✓ | ✓ per-entity Rust brands | ✓ `wire.rs::counters_and_identity_brands…` | none |
| Uniqueness keys (§1.2) | ✓ | ✗ type-level only | — | **gap:** `(principalId,commandId)`, `(hostId,journalId,seq)`, Interaction 5-tuple are not enforced by any store; §12 already scopes this to Node/journal work |
| `NativeRef` + kind branches | ✓ | ✓ `native.rs` | ✓ `m0.rs::background_job_and_herdr_identity_do_not_require_a_known_claude_session` | none — `claudeBg.jobId` is its own branch as specified |
| `ProcessRef`, `NativeRequestKey` | ✓ | ✓ | ✓ `wire.rs::interaction_answer_and_source_cursor_variants_keep_native_values` | none — RPC `1` vs `"1"` preserved via `valueType` |

## 2. Core entities (§2)

Field-level diff of each documented table against `protocol.schema.json`:

| Entity | Doc fields | Impl fields | Test | Drift |
| --- | --- | --- | --- | --- |
| `Host` (§2.1) | 13 + meta | 17 | ✓ `wire.rs::round_trip` | **exact match** |
| `Workspace` (§2.2) | 12 + meta | 16 | ✓ | **exact match** |
| `WorktreeRecord` (§2.2) | 12 | 12 | ✓ `worktree.rs` (hub) | **exact match**; no `revision`/timestamps by design (not an `EntityMeta` entity) |
| `Instance` (§2.3) | 20 + meta | 25 | ✓ `wire.rs::entities_preserve_independent_state_dimensions` | **1 undocumented field: `lastError`** (optional string, set on `lifecycle=failed`). Only Rust field in the crate with no `protocol.md §` cite. Documented in §2.3 by this change. |
| `Run` (§2.4) | 21 + meta | 25 | ✓ | **exact match** |
| `Command` (§2.5) | 20 + meta | 24 | ✓ `m0.rs::unknown_recovery_states_and_m0_error_codes_are_fixed` | **exact match**. Carries both meta `id` and `commandId` as §2.5 requires |
| `Interaction` (§2.6) | 16 + meta | 20 | ✓ `interactions.rs` (node) | **exact match** |

Three-state Command model (§2.5): `CommandState = queued|accepted|settled` only — no fourth state.
`resolution` (clear/unknown/reconciling), `dispatch` (4 values incl. `intent-durable`) and `authority`
(hub-inbox/node-ledger) are separate orthogonal fields exactly as specified. `m0.rs` asserts this.

State enums all match: `InstanceLifecycle` 9 values, `RunState` 9, `InteractionState` 7,
`Connectivity` 3, `Activity` 4 (as `Knowledge<Activity>`).

## 3. Driver interface and capabilities (§3)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| `DriverKind` 7 values | ✓ | ✓ enum complete | ✓ | none at the enum level |
| `Driver.capabilities/start/send/cancel/close/resume/attach/respondInteraction` | ✓ | ✓ | ✓ | **signature drift** — see below |
| `Driver.observations()` | ✓ | ✗ | — | **replaced** by an `mpsc::Receiver` inside the returned `RunHandle` |
| `CallContext` param on every method | ✓ | ✗ | — | struct exists, **never passed**; fence/generation enforcement is not at this layer |
| `DriverAck{dispatch,nativeIds,evidenceRawIds}` | ✓ | ✓ | ✓ | none (impl `dispatch` has 4 values incl. `intent-durable`; doc §3.1 lists 3) |
| `DriverInput` prompt/steer/model-switch | ✓ | ✓ `launch.rs` | ✓ | none |
| `DriverRecord` raw/native-exit/transport-state | ✓ | ✗ **absent** | — | **gap** — stream carries mapped `Observation`; exit becomes a synthesized lifecycle observation, transport epochs unmodelled |
| `AttachRef` / `ResumeRef` wrappers | ✓ | ✗ at driver layer | — | `attach`/`resume` take a bare `NativeRef`; **`mode` and `allowWake:false` are not expressible**, so §3.1's `ATTACH_WOULD_WAKE` contract is not structurally enforced (bg driver checks it ad hoc — `claude_pty_review.rs::bg_attach_before_dispatch_would_wake`) |
| `CapabilityName` 15 values | ✓ | ✓ all 15 evaluated | ✓ | none |
| `CapabilitySnapshot` incl. `adapterTransport` | ✓ | ✓ | ✓ `m0.rs::bg_carrier_and_capability_evidence_preserve_the_selected_transport` | **exact field match** |
| `DriverDescriptor` | ✓ | ✓ type exists | — | type present; **not produced** by any driver (no inventory path) |
| §3.2 four-condition `supported` gate | ✓ | ✗ | — | **gap** — `capabilities.rs` is a static transcription of the §3.3 matrix: `S*` → `Supported` unconditionally, evidence always `type=source` pointing at protocol.md, `nativeProtocolVersion` always unknown, `settingsRevision`/`providerProfileRevision` hard-coded `1` |

Driver implementations present: `ClaudePrintDriver`, `ClaudePtyDriver`, `ClaudeBgDriver`,
`GenericPtyDriver` (+ test-only `FakeDriver`). **`codex-appserver`, `grok-acp`, `agy-print` have no
`Driver` impl** — the materializer builds their argv and they run through the generic PTY preset, so
none of the §5.7 structured Codex/ACP/agy mappings are reachable.

## 4. InstanceSpec and materializer (§4)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| `InstanceSpec` 19 fields | ✓ | ✓ | ✓ | **exact match**. Note `host` (not `hostId`) is the documented and implemented spelling |
| `PermissionMode` 5 kinds | ✓ | ✓ tagged `kind` | ✓ | none; Codex `sandbox`/`permissions` mutual exclusion enforced by an untagged union with `deny_unknown_fields` |
| `EnvBinding` literal/credential/host-env | ✓ | ✓ | ✓ | none |
| `CarrierSpec` stdio/pty/claude-bg | ✓ | ✓ hand-written deser | ✓ `m0.rs`, golden `13`/`14` | none; stdio rejects extra fields |
| `ProviderSelection` 9 fields | ✓ | ✓ type | — | type matches; **not produced** by the materializer, which flattens provider data into its own `RecipeProvider` (no `endpointId`/`ingress`/`credentialVersion`/`modelResolved`/`selectionReason`) |
| `MaterializedLaunch` | ✓ (explicitly non-wire) | ✗ as such | ✓ `generated.rs` asserts it is **absent** from the schema | **intentional** — impl uses `LaunchRecipe`, which stores an env *allowlist* (names only) rather than resolved values. Stronger than the doc's `Record<string,string>`; noted in §12 |
| §4.2 prohibited flags/env → `NATIVE_FEATURE_DISABLED` | ✓ | ✓ | ✓ `materializer.rs::prohibited_flags_are_rejected`, `flags.rs::banned_flags_are_token_matches_not_substrings` | **none — fully implemented** as a token allowlist (not substring), covering `--bare`, `--safe-mode`, `--no-session-persistence`, `--continue`, `CLAUDE_CODE_SIMPLE`, `CLAUDE_CODE_SAFE_MODE`, plus empty `--setting-sources`; re-checked at argv build and again in each Claude driver |
| §4.3 claude-print argv recipe | ✓ | ✓ | ✓ `claude_print_review.rs::start_completes_initialize_before_user_and_keeps_stdin_open` | **matches flag-for-flag**. `--permission-prompt-tool stdio` correctly emitted only with `--permission-prompts host`; resume swaps `--session-id` for `--resume` and never `--continue` (`resume_reapplies_settings_and_model`) |
| §4.1 materialization ordering | ✓ 10 steps | ~6 of 10 | partial | **gaps:** worktree + writer-lease verification absent; provider selection has no weighted-healthy/rotation logic; `requiredCapabilities` never validated; manifest/intent persistence not done here (Node's job per §12). Not consumed from the spec: `binaryRef`, `settingsOverlay.objectRef`, `worktree`, `requiredCapabilities`, `completionScope`, `parent` |

## 5. Observation envelope and payloads (§5)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| `ObservationKind` 15 values | ✓ | ✓ | ✓ `wire.rs::every_observation_family_has_the_specified_discriminant` | **exact match**, incl. mixed spellings (`tool_call`, `interaction.requested`, `raw_tty`) |
| `Observation` envelope 16 fields | ✓ | ✓ | ✓ | **exact match**. `kind`+`payload` injected by flatten |
| `ObservationSource`, `SourceCursor` 5 variants | ✓ | ✓ | ✓ | none |
| `RawRef`, `Completeness` 4 values | ✓ | ✓ | ✓ | none |
| `NodeMutation` open/append/replace/close | ✓ | ✓ flattened into 4 payloads | ✓ | none |
| Message/Thought/ToolCall/ToolResult | ✓ | ✓ | ✓ | none |
| `workflow.run/phase/member` | ✓ | ✓ | ✓ | none |
| `interaction.requested/answered/expired` | ✓ | ✓ | ✓ | none |
| `lifecycle` entity+native | ✓ | ✓ `entityType`+`entity` union | ✓ | none — §12.1's "Rust union binding" claim verified |
| `usage`, `artifact`, `raw_tty` ×3, `opaque` | ✓ | ✓ | ✓ | `ArtifactPayload` wire key is `type` (Rust `actor_type`) — wire correct |
| `RegistryEvent` separate envelope | ✓ | ✓ `JournalEvent` union | ✓ `wire.rs::event_batches_require_one_contiguous_journal_without_duplicate_ids` | none — bad events with `instanceId` cannot fall back to the registry branch |
| §5.6/§5.7 native mapping tables (Claude/Codex/Grok/agy/PTY) | ✓ very detailed | partial | ✓ for Claude (`remuda-claude-wire`), ACP/Codex wire crates exist | **largest doc-ahead-of-impl area.** Claude print/pty/bg mappings are real and fixture-tested; Codex/Grok/agy mappings have wire crates (`remuda-codex-wire`, `remuda-acp-wire`) but **no driver consuming them** (§3) |
| §5.8 settlement rules | ✓ | partial | ✓ `claude_print_review.rs::first_workflow_result_does_not_complete_the_run`, `workflow_emits_two_results` | the critical "first result ≠ task done" rule **is** implemented and tested for claude-print |

`seq` monotonicity, `durableSeq` watermarks and batch contiguity are typed and tested at the wire level
(`EventsBatch::validate`), and exercised end-to-end by `remuda-journal/tests/journal.rs` and
`remuda-node/tests/restart.rs`.

## 6. Interaction broker (§6)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| `InteractionRequest` 4 kinds / `InteractionAnswer` 4 kinds | ✓ | ✓ | ✓ golden `10-question-request.json` | **exact match** |
| `DecisionOption`, `QuestionField` | ✓ | ✓ | ✓ | none; `nativeValueRef` kept server-side as specified |
| §6.1 `interaction.respond` param set | ✓ | ✓ `InteractionRespondParams` | ✓ | none — all of `{interactionId,requestVersion,processGeneration,runGeneration,connectionEpoch,answer}` present, `commandId` hoisted to the envelope per §7.2 |
| §6.1 CAS single-answer arbitration | ✓ | ✓ Node | ✓ `remuda-node/tests/interactions.rs` | implemented at the Node; `INTERACTION_ALREADY_ANSWERED` path exists |
| §6.2 can_use_tool / PermissionRequest hook / TUI | ✓ | ✓ print + pty | ✓ `claude_print_process.rs::approval_allow_and_deny`, `askuser_answers_question`, `claude_pty_review.rs::blocked_interaction_is_screen_derived_and_not_answerable` | matches; screen-derived prompts correctly `answerable:false` |
| §6.3 dual-responder conflict → `approvalAuthority=unknown` | ✓ | ✓ field exists, set by materializer | partial | field and `CONTROL_UNAVAILABLE` exist; **no detection of a third-party competing hook** in a real settings tree |
| §6.4 `interactionDeadlineMs`/`hookDeadlineMs`/`controlWriteTimeoutMs` | ✓ | ✗ | — | **gap** — not present as Node config keys |

## 7. Hub ↔ Node protocol (§7)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| §7.2 method list | 46 | **46 in `MethodName`** | ✓ `wire.rs::protocol_examples_and_all_methods_round_trip` | **exact set match** — programmatic diff of the §7.2 table vs the enum: zero in either direction |
| Params/result structs for all 46 | ✓ | ✓ | ✓ | none found; `CommandEnvelope{commandId,payload,expected?,expiresAt?}` matches §7.2 |
| `instance.open_terminal` allowWake:true literal | ✓ | ✓ `BoolLiteral<true>` | ✓ `m0.rs::opening_background_terminal_requires_a_distinct_explicit_wake_command`, golden `15` | none |
| `tty.attach` rejects launch fields | ✓ | ✓ `deny_unknown_fields` | ✓ | none |
| §7.3 snapshot+follow, `Snapshot{scope}` | ✓ | ✓ instance/registry variants | ✓ | none — §12.1 claim verified |
| §7.4 32-byte binary header | ✓ | ✓ `binary.rs` + `node/src/tty.rs` | ✓ `m0.rs::binary_header_preserves_uuid_big_endian_offset_and_split_utf8`, `binary_ingress_rejects_invalid_lengths_versions_ids_and_ranges`, `tty.rs::frame_has_protocol_v1_header` | header codec **fully correct**; but see carrier gap below |
| §7.4 `TransportLimits` defaults | ✓ | ✓ type | partial | Node advertises different values than the §7.4 defaults (`maxInFlightRpc=64` vs 128, `maxEventsPerBatch=128` vs 256, `maxWaitMs=30000` vs 60000). §7.4 calls these "建议" defaults returned in hello, so this is legal, not drift |
| §7.5 recovery algorithm | ✓ 6 steps | partial | ✓ `remuda-node/tests/restart.rs` | restart/watermark reconciliation exists; full native-pending replay accounting does not |

**Two distinct wires exist.** `rpc.rs` is the full 46-method §7 protocol. `hubnode.rs` is a separate,
smaller **M1 operational wire** (14 methods: `node.auth`, `node.hello`, `node.heartbeat`,
`instance.create/send/cancel/respond`, `interaction.respond`, `journal.append`, `tty.frame`,
`tty.write`, `instance.keys`, + `runtime.*` aliases) that Hub and Node actually speak today. Its module
doc says so explicitly. This is a real, deliberate staging split — but **§7.2 does not mention it**, so a
reader takes the 46-method table for the live Hub↔Node contract. Documented by this change in §7.2.

Runtime coverage of the 46 methods:

| Layer | Handles | Notably absent |
| --- | --- | --- |
| Node dev server (`server.rs`, `/v1/rpc` + `/v1/client`) | ~22: `runtime.hello`, `host.*`, `workspace.*`, `worktree.create/list`, `instance.list/get/create/send/cancel/close`, `command.get`, `events.*`, `tty.attach/detach/resize/write` | `interaction.respond` (**`interaction.list` is a hardcoded empty stub**), `run.*`, `workflow.wait`, `reconcile.instance`, `object.*`, `driver.*`, `host.report`, `instance.attach/resume/fork/configure/open_terminal`, `command.list` |
| Hub↔Node transport | the 14 `hubnode.rs` methods | everything else |

Two transport-level issues worth recording as tasks:

- `runtime_wss::dispatch_hub` falls through to `{"ok":true}` for **any** unrecognized method, unlike
  `hubnode_codec::dispatch_method` and `server.rs::dispatch_rpc`, which reject unknown methods. §7.1 and
  §9.2 both require unknown control messages to *not* become success.
- The 32-byte TTY framing is implemented and unit-tested, but is used **only by the local dev server on
  a static fixture**. The Hub WSS carrier is JSON-only (`recv_ws` parses `Message::Binary` as JSON), and
  inbound `tty.frame` is a no-op ack. So §7.4's binary multiplexing is not live over the Hub link.

## 8. MCP / CLI control plane (§8)

| Doc tool (§8.1) | Impl tool | Test | Drift |
| --- | --- | --- | --- |
| `runtime_instance_create` | `remuda_instance_create` | ✓ `mcp.rs::tools_call_create_against_mock_hub` | **name prefix `remuda_`, not `runtime_`**; inputs differ (`host`/`labels`/`worktree`/`promptFile` vs documented `providerProfileId`/`permissionPresetId`/`worktreeMode`) |
| `runtime_instance_send` | `remuda_instance_send` | ✓ | same prefix drift; accepts `text`/`file`/`input` alias |
| `runtime_instance_wait` | `remuda_instance_wait` | ✓ | prefix; `until`/`condition` incl. non-spec `idle`/`done`/`blocked`/`line:<regex>` |
| `runtime_instance_read` | `remuda_instance_read` | ✓ | prefix; `lines`/`limit`/`source` |
| `runtime_instance_stop` | `remuda_instance_stop` | ✓ | prefix; `scope` run/instance as documented |
| — | `remuda_instance_list`, `remuda_instance_keys`, `remuda_instance_rm`, `remuda_worktree_create`, `remuda_fleet_run`, `remuda_fleet_send` | ✓ `tools_call_list_keys_and_fleet_send_against_mock_hub`, `tools_call_fleet_run_is_error_when_hub_404` | **6 undocumented tools** |

The documented §8.1 CLI shape (`instance create --spec <file> --command-id <id> --json`) does not match
the real CLI, which is `remuda instance create --host/--labels/--prompt …` with `wait` conditions
`idle|done|blocked|line:<re>`. §8.1's `--spec` file form does not exist. The MCP capability object
(`principalId,instanceId,allowedHosts,…,maxChildren,maxDepth,expiresAt`) is not implemented — there is
no depth/concurrency limiting, so §8.2's `RESOURCE_LIMIT`-on-depth behaviour is absent.

Hub REST (`openapi.json`) exposes commands via `POST /v1/instances/{id}/commands` and
`POST /v1/fleet/{id}/commands`, but **`CommandRequest.operation` is typed `{"type":"string"}` with no
enum** — the accepted vocabulary (`instance.send`, `instance.cancel`, `instance.close`, `tty.write`)
is undocumented in the schema. `InteractionAnswerRequest.answer` is likewise a free-form object rather
than the §5.4 `InteractionAnswer` union. `InstanceRecord.lifecycle`
(`requested|starting|running|closing|exited|failed`) does not match protocol `InstanceLifecycle`
(`…preparing|ready|…|unknown|reconciling`), and the CLI's wait logic matches yet other strings
(`ready`, `idle`, `closed`, `terminated`, `creating`).

## 9. Errors and compatibility (§9)

| Item | Doc | Impl | Test | Drift |
| --- | --- | --- | --- | --- |
| §9.1 error table | 47 codes | **47** | ✓ `wire.rs::error_codes_match_the_specification_table` | **exact match** — programmatic diff of code string *and* rpcCode: zero differences, contiguous −32000…−32046 |
| `RuntimeError{code,rpcCode,message,retry,execution,details}` | ✓ | ✓ | ✓ | none |
| `retry` 5 / `execution` 5 values | ✓ | ✓ | ✓ | none |
| `RpcError.data` shape | ✓ | ✓ `From<RuntimeError>` | ✓ | none |
| §12.2 `M0_REQUIRED_ERROR_CODES` 24 | ✓ | ✓ **24, same order** | ✓ `m0.rs::unknown_recovery_states_and_m0_error_codes_are_fixed` | **exact match** |
| §9.2 unknown reason vocabulary | ✓ 9 codes | free-form `String` | — | reasons are unconstrained strings; the stable-lowercase-code rule is convention, not type-enforced |

## 10. Generated artefacts (§12)

`cargo run -p remuda-protocol --example gen_types -- --check` → **exit 0, both files current**. Schema
generation is **not** out of date. `tests/generated.rs` enforces this in CI
(`generated_files_are_current_without_writing_to_the_workspace`), plus `generator_rejects_unreviewed_schema_keywords`
and `generator_rejects_missing_definitions`. All 16 §12.3 golden JSON frames round-trip
(`wire_golden.rs::every_specification_json_frame_has_one_lossless_golden`).

Every §12 count claim verified: 15 Observation kinds, 46 methods, 47 error codes, 24 M0 codes,
`open_terminal` with `allowWake:true`, read-only `tty.attach` rejecting launch fields. Every §12.1
bullet verified against the schema.

---

## 11. Doc-side corrections applied in this change

Implementation was right in all four cases; protocol.md was fixed.

1. **§12.2 `Instance.connectivity` listed `reconnecting`** — the real third value is `reconciling`,
   matching §2.3's own table and `Connectivity` in `enums.rs`. §12.2 was the only place with the wrong
   spelling.
2. **§2.3 was missing `lastError`** — the one implemented Instance field with no doc row. Added as an
   optional diagnostic, explicitly not a state and not evidence of a Run outcome.
3. **§7.2 did not mention the M1 `hubnode` wire** — added a note that the 46-method table is the target
   §7 protocol and that Hub↔Node today speaks the smaller operational subset in `hubnode.rs`, so the
   table is not a claim about what is currently dispatchable.
4. **§12 implementation table overstated driver/MCP coverage** — updated to record that only
   claude-print/pty/bg + generic-pty have `Driver` impls, that capability snapshots are static matrix
   transcriptions rather than probed evidence, and that the MCP tools ship as `remuda_*`.

No Rust code, no `openapi.json`, and no generated artefact was modified.

---

## 12. Implementation gaps as concrete tasks

Ordered roughly by how much each one weakens a guarantee the spec makes. Each is a doc-vs-code gap
found above; none are style preferences.

### P0 — a spec safety rule is currently unenforceable

1. **Reject unknown methods in `runtime_wss::dispatch_hub`.** Replace the `{"ok":true}` catch-all with a
   `-32601`/`NATIVE_PROTOCOL_ERROR` response. §7.1 and §9.2 both forbid unknown control messages from
   becoming success; today a typo'd or future Hub method silently reports OK.
   *Files:* `crates/remuda-node/src/transport/wss/runtime_wss.rs`. *Test:* unknown method → error, and
   the response is not `ok`.

2. **Enforce the §1.2 uniqueness keys in the Node ledger.** `(principalId,commandId)` with digest
   comparison → `COMMAND_ID_CONFLICT`, `(hostId,journalId,seq)`, and the Interaction 5-tuple
   `(instanceId,processGeneration,connectionEpoch,nativeRequestKey,requestVersion)`. Without these the
   §2.5 idempotent-retry contract ("same id + same digest returns the original record") is untested at
   the store layer.
   *Files:* `crates/remuda-node/src/store.rs`, `crates/remuda-journal`. *Test:* same id+digest returns
   the original; differing digest is rejected; duplicate seq is refused.

3. **Give `attach`/`resume` typed refs so `allowWake:false` is structural.** Introduce `AttachRef`
   (`{nativeRef, processRef, mode, allowWake:false}`) and `ResumeRef` at the `Driver` trait boundary
   instead of a bare `NativeRef`. Today only the bg driver checks wake ad hoc, so a new driver can
   silently wake a native job and never return `ATTACH_WOULD_WAKE`.
   *Files:* `crates/remuda-driver/src/driver.rs` + the 4 impls.

### P1 — documented surface not reachable

4. **Implement `interaction.respond` / `interaction.list` on the Node's JSON-RPC surface.**
   `interaction.list` currently returns a hardcoded empty page and `interaction.respond` is absent from
   `dispatch_rpc`, so §6's broker is unreachable over `/v1/rpc` and `/v1/client` even though the Node
   implements the CAS internally.
   *Files:* `crates/remuda-node/src/server.rs`. *Test:* extend `remuda-node/tests/local_api.rs`.

5. **Constrain `CommandRequest.operation` in the OpenAPI to the `CommandOperation` enum**, and type
   `InteractionAnswerRequest.answer` as the §5.4 `InteractionAnswer` union. Also reconcile
   `InstanceRecord.lifecycle`/`activity` with protocol `InstanceLifecycle`/`Activity`.
   *Files:* `crates/remuda-hub/openapi/openapi.json` (+ the handler and `remuda-hub/tests/openapi.rs`).
   Deliberately **not** changed in this docs-only pass.

6. **Carry TTY bytes over the Hub link using the existing 32-byte framing.** The codec is correct and
   tested but only feeds a local static fixture; `recv_ws` treats binary frames as JSON and `tty.frame`
   is a no-op ack. Until this lands, §7.4's multiplexed terminal is not deliverable to the Web UI.
   *Files:* `crates/remuda-node/src/transport/wss.rs`, `crates/remuda-hub/src/ws.rs`.

7. **Make capability snapshots evidence-based, not a static matrix.** Enforce §3.2's four conditions and
   emit real `fixture`/`native-negotiation` evidence with digests; stop hard-coding
   `settingsRevision`/`providerProfileRevision` to `1` and populate `nativeProtocolVersion` from the
   actual handshake. Today every `S*` cell reports `supported` regardless of the build, which is exactly
   the failure §3.2 is written to prevent.
   *Files:* `crates/remuda-driver/src/capabilities.rs`.

8. **Produce `DriverDescriptor` inventory and wire `driver.list` / `driver.capabilities`.** The types
   exist; nothing emits them, so `Host.driverInventory` can never be populated from a real host.

### P2 — completeness against the spec

9. **Add real `Driver` impls for `codex-appserver`, `grok-acp`, `agy-print`.** The wire crates
   (`remuda-codex-wire`, `remuda-acp-wire`) and the §5.7 mapping tables exist, but with only a
   generic-PTY fallback none of the structured Codex/ACP/agy observations, approvals or turn settlement
   are reachable.

10. **Emit `ProviderSelection` from the materializer** (`endpointId`, `ingress`, `credentialVersion`,
    `modelResolved`, `selectionReason`) and implement §4.4 weighted-healthy endpoint choice. `Run` has
    the field; nothing fills it, so §4.4 rotation rules are unimplementable.

11. **Complete the §4.1 materialization order:** worktree + writer-lease verification, validation of
    `requiredCapabilities` against the capability snapshot before dispatch (§5.8 requires refusing a
    `completion-task` send when the capability is unmet), and consumption of `binaryRef` and the private
    `settingsOverlay.objectRef`.

12. **Add `interactionDeadlineMs` / `hookDeadlineMs` / `controlWriteTimeoutMs` as Node config** recorded
    in the LaunchManifest, with the §6.4 rule that the hook helper deadline precedes the native hook
    timeout.

13. **Reconcile the §8.1 MCP/CLI contract with reality** — either rename the tools to `runtime_*` and
    adopt the documented input schemas, or (preferred, since `remuda_*` matches the product name) keep
    the names and rewrite §8.1 around the real tool set, documenting the 6 extra tools and the real CLI
    verbs. Then implement the §8.1 capability object (`allowedHosts`, `maxChildren`, `maxDepth`,
    `expiresAt`) so §8.2's depth/concurrency `RESOURCE_LIMIT` is enforceable.

14. **Constrain §9.2 unknown reasons to a typed vocabulary** (`not-emitted`, `native-ack-missing`, …)
    rather than free-form `String`, so the "unknown is never a success state" rule is checkable.
