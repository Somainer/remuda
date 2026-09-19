# Codex Computer Use 4 — a screenshot survives from tool result to the web card

Date: 2026-09-19.
Scope: `c-cua-media` — [D-045](../decisions.md) §6.2 / [codex-cua.md](../codex-cua.md) §6.
Sibling evidence: [codex-cua-1](codex-cua-1.md) (the skill and its unverified
live-probe surface). Nothing here is a live desktop drive: every byte is a
fixed synthetic 1x1 PNG the in-process fake Node stages.

## What was broken

Every tool-result producer emitted text only:

- the transcript tailer (`remuda-journal` `tool_result`) filtered content to
  `text` items and dropped the rest;
- the live `claude-print` user-frame mapper JSON-`to_string()`ed an array
  content into the result text — which also inlined any image's base64;
- the driver adapters' `tool_result_payload` took one `text: Option<String>`;
- the web joined the blocks as text and rendered nothing else.

A computer-use tool returning a screenshot therefore made an empty/garbled
card, and one live path risked writing base64 into the journal row.

## The contract implemented

- No new observation kind or card family: the bytes live in the Hub object
  store and the block is the existing `ContentBlock::Image` + `MediaBlock`
  (`objectId`/`mediaType`/`name`/`size`).
- One pure seam (`remuda-protocol::tool_result_content_blocks`) folds native
  content: text stays byte-identical, image items base64-decode and stage via
  an injected `ToolMediaStager`. Missing data, undecodable base64, oversize
  or staging failure degrade to a text block naming the media type and byte
  count — never a dropped result, a panic, or inline bytes.
- The Node reuses the durable host-token staging route
  (`POST /v1/hosts/{id}/files/objects`) with an opt-in `mediaType=image/png`
  the Hub validates against magic bytes; a claim the bytes do not back is a
  400, so the object can never be octet-stream masquerading as an image.
  `GET /v1/objects/{id}` then serves it inline for the card.
- The synchronous mapper seam folds from inside the Node's tokio runtime;
  staging is handed to a dedicated one-worker runtime on its own OS thread
  over a job channel (a `Handle::block_on` from a worker would panic).
- The web `MCP` card renders bounded thumbnails (`max-height: 180px` pinned
  in CSS, `loading="lazy"`, alt = block name); click opens the object route
  in a new tab — no lightbox, no auto-expand. `resultText()` is unchanged;
  images come from the new `resultMedia()`.

## Card

`codex-cua-4-tool-card-thumbnail.png` (REMUDA_EVIDENCE=1, synthetic PNG only):
the MCP card for `mcp__codex-computer-use__get_app_state`, showing the
`codex-computer-use/get_app_state` identity, the `window state captured`
text half and the 180px-bounded thumbnail linked to `/v1/objects/obj_…`.

## Journal row of the mixed result

`codex-cua-4-journal-row.json` is the stored `tool_result` row
(`GET /v1/instances/{id}/journal`, seq 7 in the captured run). The blocks
are exactly

```json
[
  { "type": "text", "text": "window state captured" },
  {
    "type": "image",
    "objectId": "obj_01a0b84c-bd0f-73d1-8f05-01bbe48084dd",
    "mediaType": "image/png",
    "name": "screen-1.png",
    "size": 70
  }
]
```

— an object reference, no base64. The e2e asserts the fixed PNG alphabet
prefix `iVBORw0KGgo` is absent from the journal response and that
`GET /v1/objects/{objectId}` answers 200 `image/png` inline with the PNG
signature.

## Run output

- `cua-media.hub.spec.ts` — `1 passed` under the assigned ports
  59080/59089/59081 (flock `/tmp/remuda-local-e2e.lock`, shared Playwright
  server `ws://127.0.0.1:3177/`).
- Full hub suite run before DONE: `117 passed, 16 skipped, 0 failed`
  (17.2 min, serial, 2026-09-19).

## Unit coverage

- `remuda-protocol/src/tool_media.rs` — text/image/mixed folding, missing
  stager, malformed/over-size/unstageable fallbacks.
- `remuda-journal/tests/tool_media.rs` — the transcript mapper produces an
  object reference and never serialises the payload.
- `remuda-driver/tests/tool_media.rs` — the live mapper stages, degrades
  without a stager, and joins text arrays instead of JSON-stringifying them.
- `remuda-hub/tests/host_files.rs` — typed staging serves inline as
  `image/png`; mismatched/non-image claims are 400.
- `remuda-node/src/files.rs` — the staging bridge works when driven from
  inside a tokio worker (regression for the `block_on` deadlock/panic and
  the drop-order channel hang).
- `toolPresenters.test.ts` / `ToolCard.test.tsx` — `resultMedia()`, the
  thumbnail/link/alt contract, and no media row for text-only results.
