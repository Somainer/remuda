/**
 * Math tokenizer that runs BEFORE markdown (c-math, D-053 addendum 15).
 *
 * remark-math/micromark only knows `$`/`$$`, and its inline rule is looser
 * than pandoc's (it accepts a span padded at one end and has no
 * digit-after-close rule, so `$5 和 $10` parses as math). This scanner
 * implements the transcript rules directly:
 *
 * - `$$…$$` and `\[…\]` are display math; `$…$` and `\(…\)` are inline.
 * - Single `$` follows pandoc: the opener must be followed immediately by a
 *   non-space, the closer must be preceded by a non-space and must not be
 *   followed by a digit. Currency and shell variables stay text.
 * - An unclosed delimiter at EOF (the normal streaming state) is text; a
 *   formula never renders half-open.
 * - Delimiters inside code fences, code spans and link destinations are
 *   never math, and `\$` / `\\(` stay literal.
 *
 * `protectMath` rewrites the accepted spans into the `$`/`$$` form
 * remark-math parses, with every bare `$` inside a formula swapped for
 * MATH_DOLLAR so micromark cannot re-pair it; rejected `$` in prose are
 * escaped. The component swaps MATH_DOLLAR back with `restoreMathSource`
 * before KaTeX sees the source, so copy/TeX output is byte-exact.
 */

/**
 * Private-use stand-in for a real dollar sign inside a formula while the
 * text travels through the markdown/math parser (treated as ordinary text
 * data). Restored before rendering, so it must never appear visibly.
 */
export const MATH_DOLLAR = "\uE000";

export type MathSegmentKind = "prose" | "verbatim" | "inlineMath" | "displayMath";

export interface MathSegment {
  kind: MathSegmentKind;
  /** Raw source of the segment: prose, code or the formula without delimiters. */
  value: string;
}

const SPACE_RE = /[ \t\n\r\f\v]/;
const isSpace = (c: string | undefined): boolean => c !== undefined && SPACE_RE.test(c);
const isDigit = (c: string | undefined): boolean => c !== undefined && c >= "0" && c <= "9";

function runLength(s: string, i: number, ch: string): number {
  let j = i;
  while (j < s.length && s[j] === ch) j += 1;
  return j - i;
}

/** Index of the next unescaped needle; a run of backslashes protects the
 * character after it only when the run is odd. `needle` must not itself
 * start with a backslash (see closingCommand for the \(…\) forms). */
function indexOfUnescaped(s: string, needle: string, from: number): number {
  let i = from;
  while (i <= s.length - needle.length) {
    if (s.startsWith(needle, i) && precedingBackslashes(s, i) % 2 === 0) return i;
    i += 1;
  }
  return -1;
}

/** Length of the backslash run immediately before index `j`. */
function precedingBackslashes(s: string, j: number): number {
  let k = j;
  while (k > 0 && s[k - 1] === "\\") k -= 1;
  return j - k;
}

/** A blank line inside the candidate body ends the span pandoc-style. */
function hasBlankLine(s: string, from: number, to: number): boolean {
  return /\n[ \t\r]*\n/.test(s.slice(from, to));
}

/**
 * Find a closing `\)` / `\]` command (needle INCLUDES its backslash). The
 * close is real only when the backslashes immediately in front of it form an
 * even run: `\)` closes, `\\)` is an escaped backslash followed by `)`,
 * `\\\)` closes again. Blank-line validity is the caller's decision (it
 * distinguishes an unterminated opener from one voided by a blank line).
 */
function indexOfClosingCommand(s: string, needle: string, from: number): number {
  let i = from;
  while (i <= s.length - needle.length) {
    if (s.startsWith(needle, i) && precedingBackslashes(s, i) % 2 === 0) return i;
    i += 1;
  }
  return -1;
}

/**
 * If `start` opens a fenced code block (line start, at most 3 spaces indent,
 * at least 3 backticks/tildes), return the index just past the closing line
 * (or EOF for an unterminated fence — CommonMark consumes the rest as code,
 * which is also the right streaming behaviour).
 */
function fencedCodeEnd(s: string, start: number): number | null {
  if (start !== 0 && s[start - 1] !== "\n") return null;
  let j = start;
  while (j - start < 3 && s[j] === " ") j += 1;
  const marker = s[j];
  if (marker !== "`" && marker !== "~") return null;
  const size = runLength(s, j, marker);
  if (size < 3) return null;
  // Skip the info string to the next line.
  let p = j + size;
  while (p < s.length && s[p] !== "\n") p += 1;
  p += 1;
  while (p < s.length) {
    let q = p;
    while (q - p < 3 && s[q] === " ") q += 1;
    if (s[q] === marker) {
      const closeRun = runLength(s, q, marker);
      if (closeRun >= size) {
        let t = q + closeRun;
        while (t < s.length && (s[t] === " " || s[t] === "\t")) t += 1;
        if (t === s.length || s[t] === "\n" || s[t] === "\r") {
          return t === s.length ? t : t + 1;
        }
      }
    }
    while (p < s.length && s[p] !== "\n") p += 1;
    p += 1;
  }
  return s.length;
}

/**
 * For an opening `](`, return the index just after the matching closing
 * paren of a link destination (balanced parens, backslash escapes, optional
 * `<…>` wrapping). Newlines are not legal in destinations.
 */
function linkDestinationEnd(s: string, afterParen: number): number | null {
  let j = afterParen;
  if (s[j] === "<") {
    const gt = s.indexOf(">", j + 1);
    const nl = s.indexOf("\n", j + 1);
    if (gt === -1 || (nl !== -1 && nl < gt)) return null;
    j = gt + 1;
  }
  let depth = 1;
  while (j < s.length) {
    const ch = s[j];
    if (ch === "\n") return null;
    if (ch === "\\") {
      j += 2;
      continue;
    }
    if (ch === "(") depth += 1;
    else if (ch === ")") {
      depth -= 1;
      if (depth === 0) return j + 1;
    }
    j += 1;
  }
  return null;
}

/**
 * Find the closing `$` of a pandoc inline span opened at `open`.
 *
 * Pandoc's body grammar consumes a space run only when a `$` does not
 * follow it (`many1 spaceChar <* notFollowedBy (char '$')`): a `$` reached
 * straight after whitespace makes the WHOLE span fail — it is not skipped in
 * favour of a later `$` (otherwise `花了 $5 … $$x$$` would pair the currency
 * `$` with the second `$` of the display opener). An odd run of backslashes
 * (`\$`) is an escaped literal and the search continues past it.
 */
function inlineDollarClose(s: string, open: number): number | null {
  if (open + 1 >= s.length || isSpace(s[open + 1])) return null;
  for (let j = open + 2; j < s.length; j += 1) {
    if (s[j] !== "$") continue;
    const run = precedingBackslashes(s, j);
    if (run % 2 === 1) continue;
    if (isSpace(s[j - run - 1])) return null;
    if (isDigit(s[j + 1])) continue;
    // A blank line between opener and closer ends the paragraph: no span.
    if (hasBlankLine(s, open + 1, j)) return null;
    return j;
  }
  return null;
}

/** Split markdown source into prose / verbatim code / math segments. */
export function splitMathSegments(input: string): MathSegment[] {
  const segments: MathSegment[] = [];
  const n = input.length;
  let textStart = 0;

  const flush = (end: number) => {
    if (end > textStart) segments.push({ kind: "prose", value: input.slice(textStart, end) });
  };
  const verbatimTo = (end: number, at: number) => {
    flush(at);
    segments.push({ kind: "verbatim", value: input.slice(at, end) });
    textStart = end;
  };
  const mathTo = (
    kind: "inlineMath" | "displayMath",
    delimStart: number,
    bodyStart: number,
    bodyEnd: number,
    spanEnd: number,
  ) => {
    flush(delimStart);
    segments.push({ kind, value: input.slice(bodyStart, bodyEnd) });
    textStart = spanEnd;
  };

  let i = 0;
  while (i < n) {
    // 1. Fenced code block: the whole region (fences included) is verbatim.
    const fence = fencedCodeEnd(input, i);
    if (fence !== null) {
      verbatimTo(fence, i);
      i = fence;
      continue;
    }

    // 2. Code span: a matching run of backticks (may cross a line). The
    // close run must be exact (not the first N backticks of a longer run).
    if (input[i] === "`") {
      const size = runLength(input, i, "`");
      const needle = "`".repeat(size);
      let search = i + size;
      let consumed = false;
      for (;;) {
        const close = input.indexOf(needle, search);
        if (close === -1) break;
        if (input[close + size] !== "`") {
          const end = close + size;
          verbatimTo(end, i);
          i = end;
          consumed = true;
          break;
        }
        search = close + size;
      }
      if (consumed) continue;
      i += size;
      continue;
    }

    // 3. Link destination: `](url)` is verbatim (URLs often carry dollars).
    if (input[i] === "]" && input[i + 1] === "(") {
      const end = linkDestinationEnd(input, i + 2);
      if (end !== null) {
        verbatimTo(end, i);
        i = end;
        continue;
      }
    }

    // 4. Escaped character, and the \(…\) / \[…\] forms.
    if (input[i] === "\\") {
      const next = input[i + 1];
      if (next === "(" || next === "[") {
        const needle = next === "(" ? "\\)" : "\\]";
        const close = indexOfClosingCommand(input, needle, i + 2);
        if (close !== -1 && !hasBlankLine(input, i + 2, close)) {
          const end = close + 2;
          mathTo(next === "(" ? "inlineMath" : "displayMath", i, i + 2, close, end);
          i = end;
          continue;
        }
        if (close === -1 && next === "[") {
          // Unclosed display bracket: the streaming half-formula state —
          // the opener and everything after it stays plain text until \]
          // arrives. A close beyond a blank line voids the opener instead.
          flush(n);
          return segments;
        }
      }
      i += 2;
      continue;
    }

    // 5. Dollar forms.
    if (input[i] === "$") {
      if (input[i + 1] === "$") {
        const close = indexOfUnescaped(input, "$$", i + 2);
        if (close !== -1 && !hasBlankLine(input, i + 2, close)) {
          const end = close + 2;
          mathTo("displayMath", i, i + 2, close, end);
          i = end;
          continue;
        }
        if (close === -1) {
          // Unclosed $$ during streaming: no half formula — the rest is text.
          // A close beyond a blank line voids the opener; scanning continues.
          flush(n);
          return segments;
        }
        i += 2;
        continue;
      }
      const close = inlineDollarClose(input, i);
      if (close !== null) {
        const end = close + 1;
        mathTo("inlineMath", i, i + 1, close, end);
        i = end;
        continue;
      }
    }

    i += 1;
  }
  flush(n);
  return segments;
}

const EDGE_SPACE_RE = /^[ \t\r\n]+|[ \t\r\n]+$/g;

/** Formula body on its way through micromark: real dollars become tokens. */
function tokenizeBody(body: string): string {
  return body.replace(EDGE_SPACE_RE, "").replace(/\$/g, MATH_DOLLAR);
}

/** Escape a `$` that prose leaves literal, unless a backslash already does. */
function escapeProseDollars(s: string): string {
  let out = "";
  for (let i = 0; i < s.length; i += 1) {
    if (s[i] !== "$") {
      out += s[i];
      continue;
    }
    let backslashes = 0;
    for (let k = i - 1; k >= 0 && s[k] === "\\"; k -= 1) backslashes += 1;
    out += backslashes % 2 === 1 ? "$" : "\\$";
  }
  return out;
}

/**
 * Rewrite the math spans of `input` into remark-math `$`/`$$` syntax while
 * leaving code and rejected dollars as literal text. Display math is always
 * emitted on its own lines so it parses as a flow block.
 */
export function protectMath(input: string): string {
  let out = "";
  for (const seg of splitMathSegments(input)) {
    if (seg.kind === "prose") out += escapeProseDollars(seg.value);
    else if (seg.kind === "verbatim") out += seg.value;
    else if (seg.kind === "inlineMath") out += `$${tokenizeBody(seg.value)}$`;
    else out += `\n\n$$\n${tokenizeBody(seg.value)}\n$$\n\n`;
  }
  return out;
}

/** Restore the real TeX source after the markdown round-trip. */
export function restoreMathSource(source: string): string {
  return source.replace(/\uE000/g, "$");
}
