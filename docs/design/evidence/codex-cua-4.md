# Codex Computer Use 4 — a screenshot survives from tool result to the web card

Date: 2026-09-19.
Scope: `c-cua-media` — [D-045](../decisions.md) §6.2 / [codex-cua.md](../codex-cua.md) §6.
Sibling evidence: [codex-cua-1](codex-cua-1.md) (the skill and its unverified
live-probe surface). Nothing here is a live desktop drive: every byte is a
fixed synthetic 240×240 PNG the in-process fake Node folds through the real
mapper.

## What was broken

Every tool-result producer emitted text only:

- the transcript tailer (`remuda-journal` `tool_result`) filtered content to
  `text` items and dropped the rest;
- the live `claude-print` user-frame mapper JSON-`to_string()`ed an array
  content into the result text — which also inlined any image's base64;
- Claude mirrors the native content array into the `toolUseResult` sidecar,
  and nothing scrubbed it, so even after the blocks were folded the
  screenshot bytes still rode into `structured_result` (and the web card's
  JSON fallback) and out again on every replay;
- the driver adapters' `tool_result_payload` took one `text: Option<String>`;
- the web joined the blocks as text and rendered nothing else.

A computer-use tool returning a screenshot therefore made an empty/garbled
card, and oversized image frames risked the hub frame cap disconnecting the
Node.

## The contract implemented

- No new observation kind or card family: the bytes live in the Hub object
  store and the block is the existing `ContentBlock::Image` + `MediaBlock`
  (`objectId`/`mediaType`/`name`/`size`).
- One pure seam (`remuda-protocol::fold_tool_result`) folds native content:
  text stays byte-identical; image items are size-checked against the base64
  length *before* decoding, decoded, and staged via an injected
  `ToolMediaStager`. At most 8 images per result. Missing data, undecodable
  base64, oversize or staging failure degrade to a fixed text block naming
  the media type and byte count — never a dropped result, a panic, or inline
  bytes. Non-text/non-image items (`resource`, `resource_link`, …) get an
  opaque note rather than vanishing; object-shaped content keeps the
  driver's serialised fallback.
- `sanitize_tool_result_sidecar` rewrites the mirrored `toolUseResult`
  image data to the object id (or the fixed note), idempotently, in both the
  journal mapper and the live driver.
- The async producers pre-fold on `spawn_blocking` before taking the mapper
  lock (live stdout, pty transcript pump); the `subagent.transcript` RPC
  folds on a blocking thread with the connected stager. Staging parks on a
  bounded (95 s) dedicated thread, never on a tokio worker.
- The scrub is unconditional for any image object carrying bytes
  (`data`, `source.data`, `file.base64`), so a string-content result with
  an image-bearing mirrored sidecar cannot keep base64; media-type claims
  are validated before entering the journal or the staging URL, the
  pre-fold marker is a per-process nonce, and PNG headers over 8192 px /
  40 Mpx degrade.
- The Node reuses the durable host-token staging route
  (`POST /v1/hosts/{id}/files/objects`) with an opt-in, length-capped,
  charset-validated `mediaType`; the Hub accepts it only for a sniffed image
  (`image/jpg` matches JPEG magic) and serves `GET /v1/objects/{id}` inline
  with `nosniff`. On a failed HTTP-origin derivation the stager is cleared
  so an old connection's token never survives reconnect.
- The web `MCP` card renders bounded thumbnails (CSS `max-height: 180px`,
  reserved box, `loading="lazy"`, alt = block name); click opens the object
  route in a new tab — no lightbox, no auto-expand. Duplicate screenshots
  key by objectId plus index. `resultText()` is unchanged; images come from
  the new `resultMedia()`.

## Card

`codex-cua-4-tool-card-thumbnail.png` (REMUDA_EVIDENCE=1, synthetic PNG only):
the MCP card for `mcp__codex-computer-use__get_app_state`, with the
`codex-computer-use/get_app_state` identity, the `window state captured`
text half, and the 240×240 fixture rendered inside the 180px-bounded
thumbnail linked to `/v1/objects/obj_…`.

## Journal row of the mixed result

`codex-cua-4-journal-row.json` is the stored `tool_result` row. The blocks
are text plus one image reference; the mirrored `structuredResult` carries
the same object id where the base64 used to be. The e2e asserts the fixed
PNG alphabet prefix `iVBORw0KGgo` is absent from the whole journal response
and that `GET /v1/objects/{objectId}` answers 200 `image/png` inline with
the PNG signature.

## Run output

- `cua-media.hub.spec.ts` — `1 passed` (sentinel build + run) under the
  assigned ports 59080/59089/59081 (flock `/tmp/remuda-local-e2e.lock`,
  shared Playwright server `ws://127.0.0.1:3177/`).
- Full hub suite (serial, same ports), final post-review tree rebased onto
  main with c-toolfold/c-sessionchrome: `129 passed, 16 skipped, 2 failed`
  (19.8 min); the cua-media spec passed within the run. Both failures were
  load-sensitive tests in files this branch does not touch
  (`ux-code.hub.spec.ts` evidence screenshot detached under load;
  `ux-grok-partials.hub.spec.ts` needing 2 distinct pre-final samples), and
  both files passed 3/3 when run alone immediately after (1.3 min).

## Known follow-up (not fixed here)

The transcript tailer attaches the raw JSONL line as a blob with no TTL
(`remuda-journal` `envelope` raw bytes / hub store blob retention). Once
subagent screenshots are staged on that tail (this change), a screenshot-
bearing session retains megabyte-scale raw lines for the object lifetime.
The workflow journal tailer does not map tool results at all, so workflow-
member screenshots fold only through the `subagent.transcript` RPC path
(the earlier MapContext stager on `WorkflowProducer` was dead wiring and was
removed). Filed as a follow-up rather than widened in this task.

## Unit coverage

- `remuda-protocol/src/tool_media.rs` — text/image/mixed folding, encoded-
  length pre-check, per-result cap, per-process nonce marker (forgery
  rejected), unknown-item notes, missing stager and
  malformed/over-size/over-dimension/unstageable fallbacks, sanitised media
  claims, and unconditional sidecar scrubbing (unmatched image, excess
  images, `file.base64`, idempotency).
- `remuda-journal/tests/tool_media.rs` — the transcript mapper produces an
  object reference, scrubs the mirrored sidecar, and never serialises the
  payload into the *whole* `ToolResultPayload`.
- `remuda-driver/tests/tool_media.rs` — the live mapper stages, degrades
  without a stager, joins text arrays instead of JSON-stringifying them,
  and scrubs its sidecar.
- `remuda-hub/tests/host_files.rs` — typed staging serves inline as
  `image/png`; `image/jpg` matches JPEG bytes; mismatched/over-long/non-image
  claims are 400 without echoing the raw claim.
- `remuda-node/tests/subagent_transcript.rs` — a sidechain screenshot stages
  to an object reference through `read_for_store(Some(stager))` with no
  base64 in the read result.
- `remuda-node/src/files.rs` — the staging bridge works when driven from
  inside a tokio worker (regression for `block_on`-on-a-worker and the
  drop-order channel hang).
- `toolPresenters.test.ts` / `ToolCard.test.tsx` — `resultMedia()`, the
  thumbnail/link/alt contract, and no media row for text-only results.
