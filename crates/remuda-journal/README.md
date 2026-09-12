# remuda-journal

Append-only observation log for a Remuda Instance.

## Two authorities

| Authority | Owns | Does not own |
| --- | --- | --- |
| **Native session** | Resume, process identity, Claude/Codex/Grok session files | What other devices have already observed |
| **Journal** | Observed facts: `seq`, `eventId`, `rawRef`, projections | Native resume. Replaying the journal does **not** recreate a native session |

A Node may tail `~/.claude/projects/<enc>/<sid>.jsonl` and `subagents/workflows/wf_*/journal.jsonl` into this crate. Those files stay the resume source. This crate assigns a monotonic `seq` starting at 1 that never resets on process restart.

## Layout

```
<data_dir>/
  journal.sqlite          # instance watermark, event index, interactions, source cursors
  journal/<instanceId>.jsonl
  blobs/<sha256-hex>      # raw native bytes, content-addressed
```

`append` writes raw bytes to the blob store first, then the JSONL line and SQLite index, then broadcasts. `follow` registers the live subscriber **before** reading history so there are no holes.

## Sources

`Source` maps an external record to one or more [`Envelope`] values (`kind`, `completeness`, `source`, `raw`). This crate implements:

- `ClaudeJsonlTailer` — byte-offset tail with generation bump on truncate/prefix rewrite
- `WorkflowJournalTailer` — `wf_*/journal.jsonl` plus `*.meta.json`

stream-json stdout and Herdr adapters belong in the driver crate; they use the same `Source` trait.

## Projections

`TranscriptProjection`, `InteractionProjection` (first-writer-wins), and `StatusProjection` are pure folds. Full replay, per-item append, and prepend backfill of the same seqs must match.
