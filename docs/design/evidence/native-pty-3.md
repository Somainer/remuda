# native-pty-3 — live streaming, transcript regrouping, and message authorship

Evidence for [D-028](../decisions.md) §7 (实时流式结构化视图) and the P3
acceptance criteria: *结构视图行级增量出现*, *tool-call 与 tool-result 配对
100%*, *`queue-operation` 进 journal*. Also covers the two follow-up items the
user raised against screenshots comparing Remuda's 结构 view with Claude Code's
own TUI: **(A)** «workflow 没有被正确 parse» and **(B)** «结构化界面会把追加的
prompt 信息也额外展示了 … 1 是有重复，2 是容易让人误解是我发了这些信息».

- **When:** 2026-09-14, 07:20–07:40 UTC.
- **Where:** this worktree on devbox-sg; throwaway sessions under
  `/tmp/remuda-p3/`, each with its own cwd so it gets its own transcript.
- **Agent:** `claude` **2.1.221** — note this is *older* than the 2.1.270 that
  produced [native-pty-1](./native-pty-1.md), which matters for §1 below.
- **Method:** a `--settings` overlay registering a relay script that appends
  `{rx, event, payload}` with a microsecond receive timestamp, so hook arrival
  order and spacing are recorded rather than inferred.

Nothing here is a fixture. The fixtures added by P3
(`crates/remuda-testing/fixtures/hooks/claude-message-stream.jsonl`,
`crates/remuda-driver/tests/fixtures/claude-transcript-{skill,workflow}.jsonl`)
are these same recordings with host paths and identifiers scrubbed.

---

## 1 — `MessageDisplay` measured: three design assumptions did not hold

§7 recorded `MessageDisplay` as **[U]** and P3 was to verify it. It exists and
it streams, but not in the shape the design assumed.

### 1.1 It only streams inside a real PTY

Headless `claude -p` fires the hook **once with the whole message**:

```
07:29:15.876421  MessageDisplay  index=0  final=true   delta="1\n2\n3\n4\n5\n6"
```

The same prompt driven through an interactive PTY streams:

```
07:37:08.041540  MessageDisplay  index=0  final=false  delta="1\n2\n3\n4\n"
07:37:08.065080  MessageDisplay  index=1  final=true   delta="5\n6\n7\n8"
07:37:08.154849  Stop
```

**Consequence:** `-p` cannot be used to test streaming granularity at all. The
existing recorded fixture (`claude-hook-session.jsonl`) was captured headless
and contains a single `final: true` chunk, which is why P3 had to record a
second one. This is fine for D-028 — the whole decision is that a session *is*
a PTY — but it means "it streamed in my headless smoke test" proves nothing.

### 1.2 `index` is a chunk counter, not a content-block index

The two deltas above carry `index: 0` and `index: 1` for **one** text block.
They are append-only pieces of a single message. Reading `index` as an API
content-block index — the natural reading of §7's «`turn_id` / `message_id` /
`index` / `final`» — would scatter one paragraph across several nodes.

### 1.3 The hook and the transcript have different id spaces

The hook's `message_id` for the run above was
`1da06679-00ca-474c-8a1f-7a20a272f53d`. Searching the transcript that same
session wrote:

```
$ grep -c "1da06679-00ca-474c-8a1f-7a20a272f53d" <session>.jsonl
0
```

The transcript's assistant record uses `msg_vrtx_011Cf2wzDLA7shnQYB83ZBFR`.
`turn_id` is likewise absent. **The two channels cannot be joined by id.**

§7 says the transcript block should `replace` + `close` the streamed node. With
no shared identity that is not implementable as written, so the streamed node
is closed on `final` and the transcript's own message supersedes it visually.
Reconciling them onto one node would require positional matching, which is a
guess; two nodes where one is authoritative is honest.

### 1.4 Ordering: the hook is an echo, not a preview

| event | time |
|---|---|
| transcript assistant record written | 07:37:07.993 |
| `MessageDisplay` chunk 0 | 07:37:08.041 (+48 ms) |
| `MessageDisplay` chunk 1 | 07:37:08.065 (+72 ms) |

The deltas arrive **after** the transcript record, not before. The fold
therefore assumes no ordering between the two channels.

---

## 2 — Latency: first text well inside the 300 ms budget

The P3 requirement is «first text visible < 300 ms after the hook delta». The
segment Remuda owns is relay payload → journal message, measured by
`crates/remuda-node/tests/hook_latency.rs` on the recorded payloads:

```
$ cargo test -p remuda-node --test hook_latency -- --nocapture
first text after hook delta: 0 ms — "1\n2\n3\n4\n"
test a_hook_delta_becomes_readable_text_well_inside_the_budget ... ok
```

Sub-millisecond, because the fold is in-process arithmetic on an already
decoded payload. The wall-clock number a user perceives is dominated by what
is *outside* this segment, and the recording bounds that too: the two chunks
were 23 ms apart, and `Stop` followed the last one by 90 ms. The budget is not
at risk from the fold; the test exists to catch a regression that makes it so.

Honest scope: this measures the Node-side path, not the browser paint. The
journal → web hop is the existing follow/WebSocket path that P0–P2 already
shipped and P3 does not change.

---

## 3 — Transcript regrouping: two more assumptions that did not hold

§7 item 1 says to buffer by `(requestId, message.id)` and reassemble by
`apiBlockIndex`. On 2.1.221:

```
$ jq -c 'select(.type=="assistant") | keys' <session>.jsonl | head -1
["attributionSkill","cwd","effort","entrypoint","gitBranch","isSidechain",
 "message","parentUuid","sessionId","timestamp","type","userType","uuid","version"]
```

**Neither `requestId` nor `apiBlockIndex` exists.** (The older recorded fixture
`claude-transcript-tools.jsonl` does have `apiBlockIndex`, so the field is real
but not universal.) The grouping key therefore degrades to `message.id` alone,
file order carries block order, and `apiBlockIndex` is honoured when present.

Second: **`stop_reason` is set on every record of a group**, not only the last:

```
{"n":8,"type":"assistant","mid":"msg_…kH1wd","stop":"tool_use","b":["text"]}
{"n":9,"type":"assistant","mid":"msg_…kH1wd","stop":"tool_use","b":["tool_use"]}
```

So «在 `stop_reason` 出现时一次性发出» cannot mean "flush on the first record
carrying one". A group is flushed when **superseded** — a different
`message.id`, a `user` record, or the end-of-batch `flush()` the tailers call.
That last one matters: without it the closing message of a finished turn sits
buffered until the *next* record arrives, which at end of turn may be minutes.

Regrouping is verified by
`crates/remuda-driver/tests/transcript_origin.rs::records_sharing_a_message_id_become_one_assistant_message`
against the recorded skill session, whose `thinking` and `text` records share
one `message.id`.

The link from a result back to its call uses `sourceToolUseID`.
`parentToolUseId`, which the mapper previously read, occurs **zero times** in
any recorded transcript.

---

## 4 — Parity: both carriers, one conversation

`crates/remuda-driver/tests/transcript_parity.rs` declares one scenario and
renders it the way each carrier actually writes it — whole `assistant` frames
for stdout, one content block per record for the transcript — then compares
the conversation each produces:

```
$ cargo test -p remuda-driver --test transcript_parity
test both_carriers_yield_the_same_conversation ... ok
test the_shared_conversation_is_the_scenario_as_written ... ok
test the_tool_result_links_to_its_call_on_both_sides ... ok
```

Writing this found a real defect: `review::map_stdout_json` builds a fresh
mapper per call, so native ids never correlate and the print side produced an
**unlinked** tool result. `review::StdoutMapper` now keeps state across frames
the way the live reader does.

`remuda journal diff` against the whitelist stays green on the existing 3-turn
fixtures:

```
$ remuda journal diff …/print-3turn.json …/pty-3turn.json
parity ok: 7 whitelisted difference(s), 0 blocking
```

**The whitelist was not modified.** The existing `granularity` rules already
cover the record-level vs token-level difference P3 introduces, which is the
outcome §12.1 asks for — a new exemption would have meant the two paths were
diverging rather than converging.

---

## 5 — Authorship: which "user" records the user never wrote

The user's report was that the 结构 view showed text they had not sent, some of
it twice. Recording a session that invokes a skill shows why — every one of
these is `role: "user"`:

| record | evidence | classified |
|---|---|---|
| `<command-message>p3probe</command-message>\n<command-name>/p3probe</command-name>` | text shape | `injected-skill` |
| `Base directory for this skill: …\n# P3 probe reference…` | `isMeta: true` | `injected-skill` |
| `<task-notification><task-id>wc5tri90t</task-id>…` | `origin.kind` | `hook-context` |
| tool results | `sourceToolUseID` / `tool_result` block | `tool-result` |
| the human's actual prompt | `promptSource: "typed"` | `human` |

Claude sometimes states authorship itself. Surveying ~41k local user records:

```
origin.kind:   40601 «none» · 393 human · 259 task-notification · 2 coordinator · 1 peer
promptSource:  40580 «none» · 387 typed · 269 system · 16 sdk · 6 queued
```

Present on ~1.5% — too sparse to rely on, but where present it is the harness
speaking directly, so it **outranks** the heuristics. Structural evidence
(`isCompactSummary`, `sourceToolUseID`, a `tool_result` block) outranks both: a
tool result carrying `promptSource: "typed"` is still a tool result.

Classifying the same 41k records by the shipped rules leaves **no residue** —
every record lands in a category, and the `plain` bucket contains only genuine
prompts and `[Request interrupted by user]` notices.

Two deliberate choices:

- **Unrecognised ⇒ `human`.** Showing one row too many is recoverable;
  silently swallowing something the user said is not.
- **Injections are kept, not dropped.** They explain why the agent answered as
  it did. The web collapses them to a muted 「系统注入 · kind · n 字」 row and
  hiding them entirely is the opt-in (`runtime.transcript-injected`).

Duplicates: a prompt is recorded twice when enqueued and then delivered, both
records sharing a `promptId`. Deduping on `promptId` **alone** would be wrong —
several distinct prompts can share one (measured: `156a4be6-…` covers four
different texts) — so the key is `(promptId, text)`, keeping the first.

---

## 6 — Tool presenters

A recorded `Workflow` call carries exactly one input, the script source:

```json
{"type":"tool_use","name":"Workflow","input":{"script":"export const meta = { name: 'p3-demo', description: 'P3 transcript shape probe', phases: [{ title: 'Only' }] }\n…"}}
```

There is no title field — which is why the card rendered empty. The title is
parsed out of the `export const meta` literal with a tolerant regex plus brace
matching (`phases: [{…}]` closes a brace before the literal ends), falling back
to the `description` input and never throwing: a blank card is recoverable, an
exception takes the transcript row down with it.

`TaskOutput` now reads 「等待任务 w7dujkwfx，最长 10 分钟」 instead of raw JSON,
and unknown tools get a key/value summary with the raw payload behind a 原始
toggle rather than as the default. Covered by
`web/src/features/session/toolPresenters.test.ts` (22 cases).

---

## 7 — What is not covered

- **Browser paint timing.** §2 measures the Node-side fold, not the rendered
  frame. The hub e2e asserts that streamed deltas render as one growing bubble,
  but not with a millisecond budget.
- **grok / codex streaming.** §7's other two columns are P6.
- **`replace` reconciliation onto the streamed node** — not implementable as
  written (§1.3); the transcript message supersedes rather than replaces.
- The hub e2e suite on this host fails ~4 specs on an unmodified tree under
  load (>120 on 64 cores, ~100 leaked processes from other worktrees).
  Verified by stashing the diff and re-running: the same specs fail at
  baseline, and the P3 specs pass when run individually.
