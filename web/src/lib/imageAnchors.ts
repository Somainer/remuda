/**
 * Image anchor tokens in prompt text (owner nit, 2026-09-15).
 *
 * Every staged image owns a 1-based position among the draft's attachments,
 * and a `[Image #n]` token at the caret ties that image to a *place* in the
 * prompt — the way Claude Code's pasted-image placeholder does. The prompt
 * ships verbatim: nothing here strips or rewrites tokens at send time.
 *
 * All functions are pure (no DOM, no React state), so caret arithmetic and
 * IME/CJK text can be unit-tested directly. The composer is responsible for
 * reading the textarea caret and restoring it after an insert.
 */

/** The literal token for position `index` (1-based). */
export function anchorToken(index: number): string {
  return `[Image #${index}]`;
}

/**
 * Matches a token as the user could type it. `\d+` is greedy, so `#1` never
 * matches inside `#12`.
 */
export const IMAGE_ANCHOR_RE = /\[Image #(\d+)\]/g;

/** One parsed token: its 1-based index and its half-open byte-ish offset. */
export type AnchorSpan = { index: number; start: number; end: number };

/**
 * Every token in `text`, left to right. Duplicates are kept: a chip whose
 * token appears twice stays referenced, and chip removal rewrites both.
 */
export function findAnchors(text: string): AnchorSpan[] {
  const found: AnchorSpan[] = [];
  // String#replaceAll with a global regex is fine here; run a fresh regex so
  // the function is re-entrant (a module-level global would carry lastIndex).
  text.replace(IMAGE_ANCHOR_RE, (match, digits: string, offset: number) => {
    found.push({ index: Number(digits), start: offset, end: offset + match.length });
    return match;
  });
  return found;
}

/** Distinct token indices present in the text, ascending. */
export function referencedIndices(text: string): Set<number> {
  return new Set(findAnchors(text).map((span) => span.index));
}

/** Result of inserting one or more tokens: the new text and where the caret should sit. */
export type InsertResult = { text: string; caret: number };

const isWhitespace = (ch: string | undefined): boolean => ch === undefined || /\s/.test(ch);

/**
 * Insert `[Image #index]` at `caret`, adding a space on either side only when
 * the token would touch a non-space character — "inside a word" gets spaces,
 * insertion at a gap does not. The returned caret lands immediately after the
 * token (before any trailing space this added), so typing continues naturally.
 */
export function insertAnchor(text: string, caret: number, index: number): InsertResult {
  const at = Math.max(0, Math.min(caret, text.length));
  // String#at wraps negatives to the *end* of the string; at caret 0 there
  // is simply no preceding character.
  const before = at >= 1 ? text.at(at - 1) : undefined;
  const after = text.at(at);
  const lead = isWhitespace(before) ? "" : " ";
  const tail = isWhitespace(after) ? "" : " ";
  const token = anchorToken(index);
  const inserted = lead + token + tail;
  return {
    text: text.slice(0, at) + inserted + text.slice(at),
    caret: at + lead.length + token.length,
  };
}

/**
 * Insert several tokens in one gesture (a multi-file paste or pick). They
 * arrive in attachment order, each separated from the last.
 */
export function insertAnchors(text: string, caret: number, indices: number[]): InsertResult {
  let next = text;
  let at = caret;
  for (const index of indices) {
    const result = insertAnchor(next, at, index);
    next = result.text;
    at = result.caret;
  }
  return { text: next, caret: at };
}

/**
 * Map every token index through `rename`. Returning the same number keeps a
 * token; returning another renumbers it; returning `null` deletes it.
 *
 * Deletion collapses the one adjacent space this module inserted when it
 * can, preferring the leading space, so "a [Image #1] b" becomes "a b"
 * rather than "a  b" or "a [Image #2]"-shaped leftovers.
 */
export function renumberAnchors(
  text: string,
  rename: (index: number) => number | null,
): string {
  const spans = findAnchors(text);
  // `at` without the end-wrap footgun: a negative offset is out of bounds.
  const charAt = (offset: number): string | undefined =>
    offset >= 0 && offset < text.length ? text[offset] : undefined;
  // Decide deletions first so offset arithmetic stays on the original text.
  const deletions = spans
    .map((span) => {
      const next = rename(span.index);
      if (next !== null) return null;
      // Consume one adjacent space. Prefer leading: that is the separator we
      // ourselves add on mid-word inserts.
      let start = span.start;
      let end = span.end;
      if (!isWhitespace(charAt(span.start - 1)) && !isWhitespace(charAt(span.end))) {
        // Touches words on both sides — leave a single space behind.
        return { start: span.start, end: span.end, replacement: " " };
      }
      if (/\s/.test(charAt(span.start - 1) ?? "")) {
        start -= 1;
      } else if (/\s/.test(charAt(span.end) ?? "")) {
        end += 1;
      }
      return { start, end, replacement: "" };
    })
    .filter((d): d is { start: number; end: number; replacement: string } => d !== null)
    .sort((a, b) => b.start - a.start);

  let out = text;
  for (const del of deletions) {
    out = out.slice(0, del.start) + del.replacement + out.slice(del.end);
  }
  return out.replace(IMAGE_ANCHOR_RE, (match, digits: string) => {
    const next = rename(Number(digits));
    return next === null ? match : anchorToken(next);
  });
}

/**
 * Chip `removed` was deleted from the draft: pull its token(s) out and shift
 * every higher number down by one.
 */
export function removeAndRenumber(text: string, removed: number): string {
  return renumberAnchors(text, (index) => (index === removed ? null : index > removed ? index - 1 : index));
}
