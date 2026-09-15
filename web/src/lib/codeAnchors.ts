/**
 * Code quote anchors (workbench-code-2, 2026-09-15): the third code-block
 * toolbar action the reference Feishu-style block had — 评论.
 *
 * Pressing 评论 quotes an assistant code block into the composer draft as a
 * `[Code #n]` token, the code analogue of `[Image #n]`. Unlike images nothing
 * is uploaded: the quote is pure text that expands in front of the prompt at
 * send time so the harness model reads the exact lines the owner means.
 *
 * Token arithmetic lives in imageAnchors.ts (shared `[<Kind> #n]` machinery);
 * this module owns the quote payload, the selection → line mapping, and the
 * quoted-block expansion text. Everything here is pure and unit-tested.
 */
import { anchorTokenFor, findAnchorsFor, removeAndRenumberFor } from "./imageAnchors";

/** Fence info tokens that look like a path rather than a language tag. */
const BARE_LANGUAGE = /^[a-zA-Z][a-zA-Z0-9+#.-]*$/;
const PATH_LIKE = /[/\\]/;

/** Parse a fence info string into a language tag and an optional path. */
export function parseFenceInfo(info: string | null | undefined): { lang: string; path?: string } {
  const tokens = (info ?? "").trim().split(/\s+/).filter(Boolean);
  if (tokens.length === 0) return { lang: "" };
  const path = tokens.find((token) => PATH_LIKE.test(token));
  const langToken = tokens.find((token) => token !== path && BARE_LANGUAGE.test(token) && !PATH_LIKE.test(token));
  const lang = (langToken ?? "").toLowerCase();
  return path ? { lang, path } : { lang: lang || tokens[0].toLowerCase() };
}

/** One quoted code block as registered on a composer draft. */
export type CodeQuote = {
  kind: "code";
  /** Assistant message identity: its 1-based ordinal in the transcript. */
  turnOrdinal: number;
  /** 0-based ordinal of the fenced block inside that message. */
  blockOrdinal: number;
  /** Language tag (lower-case, "" when the fence had none). */
  lang: string;
  /** Fence-provided path, when the info string carried one. */
  path?: string;
  /** The quoted text — whole block or the selected lines. */
  text: string;
  /** 1-based inclusive first quoted line inside the original block. */
  lineFrom: number;
  /** 1-based inclusive last quoted line inside the original block. */
  lineTo: number;
};

/** A quote living in a draft, with local React key. */
export type DraftCodeQuote = CodeQuote & { localId: string };

/** Half-open offsets inside the block's raw text. */
export type OffsetRange = { start: number; end: number };

/** 1-based inclusive line range, plus the exact text of those lines. */
export type LineQuote = { lineFrom: number; lineTo: number; text: string };

/**
 * Map a character offset range onto whole lines. A selection that starts or
 * ends mid-line quotes the whole line, the way "quote these lines" reads.
 * Returns the whole block for an empty/collapsed range or bad offsets.
 *
 * Offsets are code units, matching `Range.toString()` length in practice for
 * the BMP-only code we render; line breaks are `\n`.
 */
export function lineRangeFromOffsets(code: string, start: number, end: number): LineQuote {
  const lines = code.split("\n");
  const total = lines.length;
  if (total === 0 || code.length === 0) {
    return { lineFrom: 1, lineTo: 1, text: code };
  }
  let lo = Math.max(0, Math.min(start, end, code.length));
  let hi = Math.min(Math.max(start, end), code.length);
  if (lo === hi || hi <= 0 || lo >= code.length) {
    return { lineFrom: 1, lineTo: total, text: code };
  }
  // Move the end off a trailing newline so selecting "to the line break"
  // doesn't pull the empty line after it in.
  if (hi > lo && code[hi - 1] === "\n") hi -= 1;
  const lineAt = (offset: number): number => {
    let line = 1;
    for (let i = 0; i < offset && i < code.length; i += 1) {
      if (code[i] === "\n") line += 1;
    }
    return line;
  };
  const lineFrom = Math.min(lineAt(lo), total);
  const lineTo = Math.min(lineAt(hi - 1), total);
  return {
    lineFrom,
    lineTo,
    text: lines.slice(lineFrom - 1, lineTo).join("\n"),
  };
}

/**
 * Expand one quote into the block the model reads. Header carries the same
 * `[Code #n]` token the draft text uses, then the location in parentheses,
 * then a fresh fence tagged with the language (the path, when present, lives
 * in the header rather than on the fence, so highlighting stays valid).
 */
export function quoteExpansion(quote: CodeQuote, index: number): string {
  const location = quote.path?.trim()
    ? quote.path
    : quote.lang
      ? `${quote.lang} block`
      : "code block";
  return [
    `${anchorTokenFor("Code", index)} quoted from the assistant's message (lines ${quote.lineFrom}-${quote.lineTo} of ${location}):`,
    `\`\`\`${quote.lang}`,
    quote.text,
    "```",
  ].join("\n");
}

/**
 * Expanded prompt: every quote block, in token order, immediately before the
 * draft text (which itself keeps the `[Code #n]` tokens verbatim). Quotes
 * whose token was edited out are still sent, trailing in registration order,
 * mirroring the image-anchor "未引用 still sent" behaviour.
 */
export function expandCodeQuotes(draftText: string, quotes: readonly CodeQuote[]): string {
  if (quotes.length === 0) return draftText;
  // First token offset per index, so blocks appear in the order the draft
  // mentions them (mirrors the image manifest's token ordering).
  const firstAt = new Map<number, number>();
  for (const span of findAnchorsFor("Code", draftText)) {
    if (!firstAt.has(span.index)) firstAt.set(span.index, span.start);
  }
  const ordered = quotes
    .map((quote, position) => ({ quote, index: position + 1 }))
    .sort((a, b) => {
      const ap = firstAt.get(a.index);
      const bp = firstAt.get(b.index);
      // Referenced blocks come first in token order; unreferenced ones
      // (token edited away, still sent) trail in registration order.
      if (ap === undefined && bp === undefined) return a.index - b.index;
      if (ap === undefined) return 1;
      if (bp === undefined) return -1;
      return ap - bp;
    });
  const blocks = ordered.map(({ quote, index }) => quoteExpansion(quote, index));
  const header = blocks.join("\n\n");
  const body = draftText.trim();
  return body ? `${header}\n\n${body}` : header;
}

/** Remove one code quote's token and renumber the rest (chip × mirror). */
export function removeCodeToken(text: string, removed: number): string {
  return removeAndRenumberFor("Code", text, removed);
}

/** First-line preview for a chip, truncated to one short row. */
export function quotePreview(quote: CodeQuote, max = 48): string {
  const first = quote.text.split("\n").find((line) => line.trim().length > 0) ?? quote.text;
  const one = first.trim();
  return one.length > max ? `${one.slice(0, max - 1)}…` : one;
}
