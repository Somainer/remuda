# Claude print stream append evidence

Recorded 2026-09-13 with isolated `remuda dev` services on Hub
`127.0.0.1:59880` and Node `127.0.0.1:59887`. The baseline binary was built from
`63c63bbf5158d6bc7dfed94bc18fa898c4bb39fc` before editing the adapter. This is a
real Claude Code 2.1.270 `claude-print` stream-json run using `--model haiku`
and `--max-budget-usd 0.3`.

## Regression and intended model

`git log -S 'event.uuid.clone().unwrap_or_else(|| "stream".into())' --
crates/remuda-driver/src/claude_print.rs` traces the per-event UUID mapping to
`184357e2a7868e99af261996f252d55378eef5f8` (`feat(remuda-driver): claude-print driver over stream-json`). Each
delta called the message helper, which allocated a new message/node ID and
emitted `open`, revision 1. A later assistant snapshot allocated another ID.
This behavior originates in the initial print mapper; it is not evidence of a
recent change to the web renderer. Codex/Grok do not share this print mapper;
their wire adapters are separate. The Claude transcript mapper in
`remuda-journal` is also separate from this stdout path.

The fix follows the existing per-block model: one stable message/node ID for
each native text block, an initial `open`, subsequent `append` operations with
increasing revisions, and `close` with the completed content. Thinking and
tool blocks retain their corresponding protocol observation types and stable
identities. Existing journals are not rewritten or heuristically joined.
One durable event per delta is permitted by the task; stable append operations
provide the required merge semantics without time-based coalescing.

## Reproduction

Build and preserve the baseline executable before changing sources. Use a
private random access-code file with mode 0600, an isolated data/workspace
directory under `/tmp/remuda-driver/`, and the following shape of invocation:

```sh
REMUDA_CLAUDE_BIN=/tmp/remuda-driver/x-frag-before/claude-capture \
REMUDA_CLAUDE_INHERIT_DEFAULT_CONFIG=1 \
/tmp/remuda-driver/x-frag-before/remuda-baseline \
  --data-dir /tmp/remuda-driver/x-frag-before/data dev \
  --no-herdr-orphan-sweep \
  --hub-listen 127.0.0.1:59880 --listen 127.0.0.1:59887 \
  --workspace /tmp/remuda-driver/x-frag-before/workspace \
  --access-code-file /tmp/remuda-driver/x-frag-before/access-code
```

The capture executable forwards to the installed real Claude executable. It
inherits stdin, tees stdout bytes to a private local capture, forwards signals,
and propagates the child exit status. Version probes and non-stream metadata
are excluded from the fixture. The Hub exchanges the bootstrap code at
`POST /v1/login`; the returned device token stays in a mode-0600 local file and
is used only for authenticated requests. No credentials are included here.

Create through `POST /v1/instances`, selecting the local advertised host:

```json
{"kind":"claude","driver":"claude-print","model":"haiku","maxBudgetUsd":0.3,"cwd":"/tmp/remuda-driver/x-frag-before/workspace","name":"print-stream-before","prompt":"请只用一句简短中文解释为什么流式回复应当合并成一个消息，不要使用工具。"}
```

Read `GET /v1/instances/{id}/journal?limit=1000` after the native `turn_done`
observation. Command acceptance alone is not used as completion evidence.

## Before: durable journal fragments

The baseline instance was
`ins_01a099f8-c464-7650-b394-2a1af0beddd1`. Its completed turn had
`durableSeq="40"`, 40 journal events, and 11 assistant `message` observations
with **11 distinct message IDs and node IDs**. Ten were `open/streaming`, and
one was `open/complete`; every revision was 1. The text fragments occupied
durable sequences 22–31; the complete text snapshot was sequence 32.

Selected exact payload fields from the Hub journal:

```jsonl
{"seq":22,"messageId":"obj_01a099f8-dd1d-77bc-a5ae-338a483e372f","nodeId":"obj_01a099f8-dd1d-77bc-a5ae-338a483e372f","operation":"open","revision":"1","targetBlock":0,"status":"streaming","blocks":[{"text":"流式","type":"text"}]}
{"seq":23,"messageId":"obj_01a099f8-dd1d-77bc-a5ae-338cb6532c43","nodeId":"obj_01a099f8-dd1d-77bc-a5ae-338cb6532c43","operation":"open","revision":"1","targetBlock":0,"status":"streaming","blocks":[{"text":"回复合并成一个消","type":"text"}]}
{"seq":24,"messageId":"obj_01a099f8-dd1d-77bc-a5ae-338e2bda8a80","nodeId":"obj_01a099f8-dd1d-77bc-a5ae-338e2bda8a80","operation":"open","revision":"1","targetBlock":0,"status":"streaming","blocks":[{"text":"息可","type":"text"}]}
{"seq":32,"messageId":"obj_01a099f8-dde1-77e1-893e-f8ef5bef90b8","nodeId":"obj_01a099f8-dde1-77e1-893e-f8ef5bef90b8","operation":"open","revision":"1","targetBlock":null,"status":"complete","blocks":[{"text":"流式回复合并成一个消息可以避免频繁的通知打断，让用户一次性看到完整的响应流程。","type":"text"}]}
```

Native `turn_done` was durable sequence 39, followed by usage at sequence 40.
After graceful dev shutdown, reading the Hub SQLite journal confirmed
`COUNT(*)=40` and `MAX(seq)=40`. The owned dev process and its capture/Claude
children exited, and both ports were released.

## Recorded wire ordering

The sanitized fixture preserves the actual relevant record order from this
session:

1. `message_start` with one native assistant message ID.
2. Thinking block start at native index 0, three deltas, then an `assistant`
   snapshot containing only the thinking block, then block stop.
3. Text block start at native index 1, ten deltas, then an `assistant` snapshot
   containing only the text block, then block stop.
4. `message_delta`, `message_stop`, then `result`.

Both snapshots use the same native message ID. A snapshot content array has
local index 0 even when the stream's block index is 1, and the snapshot arrives
**before** `content_block_stop`. The mapper therefore must not assume that a
snapshot array index is the native block index, or reopen a block on its final
snapshot. Private metadata is removed; opaque thinking signatures are replaced
with a fixed redacted marker.

## After: one durable streaming message

The fixed executable was copied from the rebuilt agent target before running
the same prompt, model, budget, and owned ports with a separate
`/tmp/remuda-driver/x-frag-after/` data/workspace root. The resulting instance
was `ins_01a099fd-9aaa-77db-bd26-7859c5d0eb7b`.

The completed turn had `durableSeq="44"` and 13 assistant `message`
observations with **one distinct message ID and node ID**. They were one
`open`, ten `append`, one complete snapshot `replace`, and one `close`.
Revisions increased exactly from 1 through 13; all targeted block 0 of this
per-block message. Claude's native text block index was 1 because thinking
occupied native index 0.

Selected exact payload fields from the fixed Hub journal:

```jsonl
{"seq":24,"messageId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","nodeId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","operation":"open","revision":"1","targetBlock":0,"status":"streaming","blocks":[{"text":"合","type":"text"}]}
{"seq":25,"messageId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","nodeId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","operation":"append","revision":"2","targetBlock":0,"status":"streaming","blocks":[{"text":"并流式回复成一条","type":"text"}]}
{"seq":26,"messageId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","nodeId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","operation":"append","revision":"3","targetBlock":0,"status":"streaming","blocks":[{"text":"消息能","type":"text"}]}
{"seq":35,"messageId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","nodeId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","operation":"replace","revision":"12","targetBlock":0,"status":"complete","blocks":[{"text":"合并流式回复成一条消息能避免频繁的消息通知和对话碎片化，让用户专注地阅读完整信息，提供更好的交互体验。","type":"text"}]}
{"seq":36,"messageId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","nodeId":"obj_01a099fd-bc80-77b4-8ed7-d0bf522ad580","operation":"close","revision":"13","targetBlock":0,"status":"complete","blocks":[{"text":"合并流式回复成一条消息能避免频繁的消息通知和对话碎片化，让用户专注地阅读完整信息，提供更好的交互体验。","type":"text"}]}
```

The two `thought` observations also shared one stable
`thoughtId=nodeId=obj_01a099fd-b477-71c4-961a-d2b364de43f7`: `open`, revision 1,
at sequence 22, followed by `close`, revision 2, at sequence 23. The native
thinking deltas contained no text in this run, so it validates snapshot/stop
identity rather than incremental thought text; fixture tests cover nonempty
thinking deltas and tool blocks.

| Measurement | Baseline | Fixed |
| --- | ---: | ---: |
| Assistant text journal operations | 11 | 13 |
| Distinct assistant message/node identities | 11 | 1 |
| Assistant `open` operations | 11 | 1 |
| Streaming `append` operations | 0 | 10 |
| Highest assistant revision | 1 | 13 |

The model returned different short sentences, so total event counts and delta
counts are not a performance comparison. The verified change is stable
identity and mergeable operations throughout a streamed response.

## Persistence and scope

A second authenticated Hub journal read was byte-equivalent after JSON
decoding to the first completed-turn read. An independent fold of
`open/append/replace/close` reconstructed exactly the final complete text, with
no duplicate snapshot text and revisions 1–13.

After graceful dev shutdown, the Hub's persisted SQLite `journal` table
contained 44 events with maximum durable sequence 44, including the same 13
assistant operations and exactly one assistant message ID. The owned dev,
capture-wrapper, and Claude process IDs no longer existed; ports 59880 and
59887 were released. This validates durable storage, rather than only an
in-memory follow stream or an accepted HTTP command.

The live acceptance above covers the actual driver → Node → Hub durable
journal path. The web merge behavior is covered by its projection tests.
The Hub journal now offers mergeable identities and operations to consumers.
MCP `read` still returns raw observations; the separate journal and Feishu
projections were not changed or separately live validated in this task. No real tool was requested by the short prompt;
tool-use/tool-result behavior is covered by driver fixtures.

## Repository checks

All implementation checks passed on the complete worktree change:

- `cargo fmt --all`, followed by `cargo fmt --all -- --check`.
- `cargo build -p remuda --locked` (includes the changed driver and live dev executable).
- `cargo test -p remuda-driver --locked claude_print::stream -- --nocapture`: 11 regression tests passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- `cargo test --workspace --locked`: 723 passed, 0 failed, 13 ignored across unit, integration, and doc-test summaries. Ignored opt-in live tests were not counted as passes; the two real sessions above were run separately.
- In `web/`, `pnpm test`: 41 files / 140 tests passed; `pnpm build` passed with the existing bundle-size warning; `pnpm lint` exited 0 with five existing warnings in untouched files.
- `./scripts/ci/secret-scan.sh` and `git diff --check` passed.

Cargo used the assigned agent target directory and `CARGO_INCREMENTAL=0`.
The only implementation changes are in the Claude print driver and the web
transcript assembler; protocol schemas, Hub persistence, dependencies, and
historical journals are unchanged.
