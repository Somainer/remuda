# Protocol wire goldens

Source: `docs/design/protocol.md`, every fenced JSON example. `manifest.json` records the section, JSON block ordinal, frame ordinal, and Rust decoding category for each file. The test extracts the current document and compares every value, rejecting missing, extra, or changed examples. NDJSON examples are split into one JSON file per frame.

These files preserve the specification examples, including synthetic native IDs, settings, permission frames, and placeholder digests. No live model was called to produce them. JSON does not support comments, so provenance lives in this file, the manifest, and the test module's header instead of altering wire payloads.

`native-json` entries are native Claude settings/control/hook examples, outside Remuda's normalized RPC types. They must round-trip losslessly as strict JSON and must fail Remuda RpcRequest decoding. Other categories round-trip through their concrete Rust type and validate against its generated schema.
