# stdio-objects-1: brief attachments reach a worker over the ssh-stdio carrier

Date: 2026-09-17. Branch: `wt/c-stdioobjects/object-pull-over-carrier`.

## Failure this closes

2026-09-16, the first real self-hosted dispatch to a Mac Hub that enrolled
the Node over `remuda ssh node … --hub http://127.0.0.1:18080` provisioned
fine but the send died on the Node:

> `invalid request: this Node has no Hub attachment source; send without
> attachments` (`crates/remuda-node/src/runtime.rs`, `AttachmentLoader`).

The only channel a ssh-stdio Node has is the JSON bridge onto
`GET /v1/node`; it can make no HTTP `GET /v1/objects/{id}`. The fix adds
the Node→Hub `object.pull` RPC (protocol.md §7.4, D-027a reserved case):
small objects inline, larger objects as preceding `object.chunk`
notifications, authorized by the socket's host token **and** the staging
instance binding. The ssh bridge forwards both frame kinds unchanged; no
binary tunnel was added because the NDJSON bridge carries JSON only.

## Topology (all local, loopback only)

- Hub: `hub_e2e` example on `http://127.0.0.1:57480`, attachment cap
  default 64 KiB fixture value (brief is 217 bytes).
- A fake `ssh` shim (scratch `/tmp/remuda-mq-objpull/bin/ssh`) execs a
  locally-built `remuda node --stdio` with its own scratch HOME and data
  dir — byte-identical to the production
  `remuda ssh node <alias> --hub … --node-data-dir …` path in
  `remuda-ssh/src/cli.rs`, minus real OpenSSH.
- Node enrolled as `hst_01a0ab60-976a-73c8-a16b-94553073e9fd`, carrier
  `ssh-stdio`, online; workspace
  `wsp_01a0ab60-9781-7306-9df9-fc4bdfbe6e48` registered automatically.
- Driver: `claude-print` pointed at the bundled `fake-claude` harness
  playing `ok.jsonl` (final text `OK`), never a real model.

## Sequence

1. `remuda ssh node localnode --hub http://127.0.0.1:57480 …` — hello
   accepted, bridge held: `"mode":"hub-enroll"`, `"carrier":"ssh-stdio"`.
2. Project created with the stdio host as its `build` member and port
   block `58700-58729`.
3. `remuda dispatch --brief brief.md --project prj_… --host hst_…9fd
   --driver claude-print --model fake` returned
   `wkr_01a0ab63-0b02-70c7-bf91-f0a12ff11823` /
   `ins_01a0ab63-0a5d-735f-8416-4055e55ca8fb` /
   brief object `obj_01a0ab63-0aef-72b8-af65-39a323ada952`,
   `state.working`.
4. The Node pulled the object over the carrier and materialized it
   before the send reached the driver:
   `…/instances/ins_…a8fb/attachments/brief.md`.
   `diff` against the dispatched file is clean,
   `sha256 = 3d7c5db1ae326bbe49eb4063a585f17270d6e983ed1640bef8818f9394702818`.
5. The mirrored user journal event (seq 9) carries the file block, the
   `file://…/attachments/brief.md` resource block naming the pulled
   object id, and the coordinator note text.
6. The fake harness completed the turn: assistant final (seq 12) text
   `OK`, command settled.
7. Worker retire removed the attachment directory, worktree and target
   dir (existing co-dispatch cleanup, unchanged by this work).

Curated journal seq-9 payload (scratch path only):

```json
{
  "role": "user",
  "blocks": [
    {"type": "file", "objectId": "obj_01a0ab63-0aef-72b8-af65-39a323ada952",
     "mediaType": "text/markdown", "name": "brief.md", "size": 217},
    {"type": "resource",
     "uri": "file:///tmp/remuda-mq-objpull/data3/instances/ins_01a0ab63-0a5d-735f-8416-4055e55ca8fb/attachments/brief.md",
     "objectId": "obj_01a0ab63-0aef-72b8-af65-39a323ada952"},
    {"type": "text",
     "text": "Your coordinator brief is delivered as the attached file brief.md. … Reply on one line: DONE <sha> or BLOCKED <reason>."}
  ]
}
```

Before the fix the same topology failed at step 4 with
`NATIVE_PROTOCOL_ERROR: invalid request: this Node has no Hub attachment
source; send without attachments`, and no `attachments/` directory was
ever created.

## Automated coverage

- `remuda-node` `carrier_objects::tests` (fake carrier): inline small
  object, out-of-order `object.chunk` reassembly, SHA-256 mismatch
  refusal, deadline without a reply, Hub RPC error, unrelated-frame
  pass-through.
- `remuda-hub/tests/object_pull.rs` (real node WebSocket, the same
  frames the ssh bridge forwards): authorized inline pull; 900 KiB
  object streamed as chunks and reassembled with matching sha256;
  cross-host and wrong-instance pulls refused (`-32001`); unknown object
  `-32601`; pull past a smaller configured attachment cap rejected with
  `RESOURCE_LIMIT`.
- Existing `remuda-node/tests/attachments.rs` and
  `crates/remuda-hub/tests/objects.rs` (HTTP path) still pass; the WSS
  runtime keeps HTTP first and only falls back to the carrier once per
  connection when the Hub HTTP origin dial fails.
