# Host file search and cross-host agent messaging — host-search-1

Captured with `REMUDA_EVIDENCE=1` against the Hub e2e harness
(`cargo run -p remuda-hub --example hub_e2e`) on a loopback port pair, with
**two enrolled fake Nodes** (host A and host B) and the real `remuda` CLI
built from this branch. Both fake Nodes answer `host.files.list` /
`host.files.read` / `host.files.search` against real seeded directories and
the search walker uses the same `ignore` / `glob` / `regex` crates and caps
as `remuda-node::files`. No live model is involved anywhere.

Tokens, host ids, instance ids and hostnames are redacted
(`<access-code>`, `hst_a` / `hst_b`, `ins_a1` / `ins_a2` / `ins_b1`).

Instances for the messaging half were created through the real operator
API: `ins_a1` on host A scoped to both hosts (`scope.hostIds=[hst_a,
hst_b]`), `ins_a2` on host A scoped to host A alone, `ins_b1` on host B.
The agents used ordinary per-instance MCP device tokens minted with
`POST /v1/instances/{id}/mcp-token`.

## 1. Name search

```text
$ remuda host files search e2e-fake-node wsp_e2e alpha --path host-search \
    --hub http://127.0.0.1:<port> --bootstrap-token <access-code>
host-search/alpha.txt
```

Name mode matches the entry name; one workspace-relative path per line.

## 2. Content search (`path:line:snippet`)

```text
$ remuda host files search e2e-fake-node wsp_e2e needle --content --path host-search \
    --hub http://127.0.0.1:<port> --bootstrap-token <access-code>
host-search/alpha.txt:1:the hostsearch needle lands here
host-search/sub/beta.rs:1:fn needle() -> u8 { 1 }
```

The seeded tree also contains `ignored.ignored` (matched by `*.log`-style
`.gitignore` — here `*.ignored`) and a NUL-byte `binary.bin`; neither is
returned: gitignored entries and binary files are skipped exactly like a
default ripgrep invocation. The case-sensitive literal `needle` also
correctly misses the seeded `NEEDLE in caps` line; the regex
`(?i)needle` matches it (node unit `search_name_content_regex_and_glob_modes`).

## 3. Truncated by cap

Three same-pattern names (`match-a.txt`, `match-b.txt`, `match-c.txt`)
exist; the caller asked for one hit:

```text
$ remuda host files search e2e-fake-node wsp_e2e match --path host-search --max-results 1 \
    --hub http://127.0.0.1:<port> --bootstrap-token <access-code>
host-search/match-a.txt
truncated: max-results cap hit after 1 match(es) across 3 file(s); narrow the query or raise --max-results
```

The Node reply carries `truncated:true` and
`truncatedReason:"max-results"`. The other construction bounds are
exercised deterministically in node unit `search_caps_truncate_with_reasons`:
`per-file-bytes` (a file over 1 MiB is skipped and truncates),
`total-bytes` (the walk stops past 10 MiB scanned) and `time` (an expired
deadline stops at the first entry boundary).

## 4. Rejected traversal

```text
$ remuda host files search e2e-fake-node wsp_e2e needle --content --path ../remuda-e2e-second \
    --hub http://127.0.0.1:<port> --bootstrap-token <access-code>
Error: hub HTTP 400: {"error":"host file path ../remuda-e2e-second escapes the workspace: '..' is not allowed","code":"BAD_REQUEST"}
exit: 1
```

The `..` component is refused lexically on the Node before any filesystem
call; the symlink-escape case is covered by node unit
`search_rejects_traversal_and_symlink_escape`.

## 5. Cross-host send: MCP tool and CLI, journaled on both sides

An agent instance on host A messaged an instance on host B twice — once
through the MCP server (`remuda mcp`, NDJSON framing) and once through the
CLI — both with the instance's own Agent token and **no human Interaction**:

```text
$ printf '%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remuda_instance_send",
      "arguments":{"instanceId":"ins_b1","text":"cross-host hello from the MCP tool on node A"}}}' \
  | REMUDA_HUB=http://127.0.0.1:<port> REMUDA_TOKEN=<agent-a1-token> remuda mcp
{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"command\":{\"commandId\":\"cmd_…\",
  \"instanceId\":\"ins_b1\",\"hostId\":\"hst_b\",\"operation\":\"instance.send\",\"state\":\"queued\",
  \"forwarded\":true …}}]}}

$ remuda instance send ins_b1 --text "cross-host hello from the remuda CLI on node A" \
    --hub http://127.0.0.1:<port> --token <agent-a1-token>
{ "command": { "operation": "instance.send", "state": "queued", "forwarded": true, … } }
```

Sender-side journal (`GET /v1/instances/ins_a1/journal` with the agent
token), outbound records naming the target instance and host:

```json
{ "seq": 6, "direction": "outbound",
  "text": "cross-host hello from the MCP tool on node A",
  "relatedIds": { "instanceId": "ins_b1", "hostId": "hst_b" } }
{ "seq": 7, "direction": "outbound",
  "text": "cross-host hello from the remuda CLI on node A",
  "relatedIds": { "instanceId": "ins_b1", "hostId": "hst_b" } }
```

Receiver-side journal (`GET /v1/instances/ins_b1/journal`), inbound records
naming the sender and its host:

```json
{ "seq": 12, "direction": "inbound",
  "text": "cross-host hello from the MCP tool on node A",
  "relatedIds": { "instanceId": "ins_a1", "hostId": "hst_a" } }
{ "seq": 16, "direction": "inbound",
  "text": "cross-host hello from the remuda CLI on node A",
  "relatedIds": { "instanceId": "ins_a1", "hostId": "hst_a" } }
```

(The receiving Node additionally journals the delivered command prompt, so
the inbound seqs are interleaved with the fake Node's own message rows.)

The out-of-scope sibling keeps the one-shot human Interaction. The
narrowly-scoped `ins_a2` (`scope.hostIds=[hst_a]`) sending to host B:

```text
$ remuda instance send ins_b1 --text "this must be refused" \
    --hub http://127.0.0.1:<port> --token <agent-a2-token>
Error: hub HTTP 409: {"error":"human approval required; answer interaction int_…, then retry with approvalId",
 "code":"HUMAN_APPROVAL_REQUIRED","interactionId":"int_…"}
exit: 1
```

Fleet `all` stays banned from Agent origin (`400` in the same setup), and an
Agent can never answer the resulting Interaction.

## 6. Secret scan

```text
$ ./scripts/ci/secret-scan.sh
secret-scan: pass
```

## 7. Automated coverage recorded for this slice

- Node unit tests (`remuda-node` `files::tests`, 11): containment success,
  `..` rejection, external-symlink rejection, irregular-file rejection,
  oversize-read rejection, scratch acceptance/refusal, unknown
  workspace/method rejection (the host-files-1 set), plus name/content
  literal + regex modes, glob narrowing, gitignore and binary skipping, and
  each truncation cap (`max-results`, `per-file-bytes`, `total-bytes`,
  `time`) with `truncated` set.
- Hub route tests (`tests/host_files.rs`, 7): Human 200 for list/read/search
  through a scripted Node, Agent origin 403 on all three routes, unknown host
  404 and disconnected host 409 on search as well, Node-side validation
  errors surfacing as a clean 400 (empty query, bad mode, unknown workspace),
  plus the host-token staging and oversize checks from host-files-1.
- Cross-host messaging (`tests/cross_host_messaging.rs`, 1): two fake Nodes;
  an agent scoped to both hosts sends host A → host B with 200 and no
  approval, both journals carry the outbound/inbound records with target and
  host ids, a host-A-only scope gets 409 `HUMAN_APPROVAL_REQUIRED`, and fleet
  `all` is 400 for Agent origin.
- Hub e2e example: now enrolls a second Node (`e2e-node-b`, separate seeded
  roots) and answers `host.files.search` with the same real walker used in
  the evidence runs.
