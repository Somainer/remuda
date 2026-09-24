/**
 * Pre-markdown math preparation (c-math round 2, D-053 addendum 15).
 *
 * remark-math/micromark owns ALL document structure: `$`/`$$` inside
 * blockquotes, lists, indented code (`    $$x$$` stays a code block), code
 * spans and paragraphs parse correctly natively. This module never segments
 * the document or relocates blocks; it performs only the four transforms
 * remark-math cannot:
 *
 *  1. Pandoc single-`$` guard. micromark accepts `$5 和 $10` (an opener may
 *     touch a space and there is no digit-after-close rule). A linear sweep
 *     escapes the opener `$` pandoc rejects so micromark leaves it literal;
 *     closed pairs are left for micromark.
 *  2. Bracket delimiters micromark does not know: `\(…\)` → `$…$` (inline)
 *     and `\[…\]` → a `$$` flow fence, with the opener line's blockquote/list
 *     prefix replicated so the formula stays INSIDE its container.
 *  3. Display fence vs blank line. micromark's `$$` flow fence spans blank
 *     lines; pandoc ends a span at a blank line. When the next `$$` run is
 *     only reachable across a blank line, the opener is escaped (rendered
 *     literal) and that later run is reconsidered as an opener — so a later,
 *     valid formula still renders. Only a `$$`/`\[` with NO closer before EOF
 *     swallows its tail (the streaming half-formula case).
 *  4. Streaming half-formula: an unclosed display opener's whole tail is
 *     escaped literal, so delimiters/backslashes/asterisks render intact
 *     until the closing delimiter arrives on a later append.
 *
 * Complexity: a constant number of forward scans; the single-dollar close
 * search and the display-run pairing each advance a frontier that never
 * moves back. Overall O(n) — `"$1".repeat(50_000)` does not re-scan the tail
 * for every rejected opener.
 */

/**
 * Stand-in for a literal `$` inside a rewritten BRACKET formula body. Dollar
 * math needs no token (micromark owns it), but a body reached via
 * `\(…\)`/`\[…\]` may contain a real `$` that micromark would mistake for a
 * closer; the renderer restores it before KaTeX runs, so it never reaches
 * the DOM or the copied TeX.
 */
export const MATH_DOLLAR = "\uE000";

const SPACE_RE = /[ \t\n\r\f\v]/;
const isSpace = (c: string | undefined): boolean => c !== undefined && SPACE_RE.test(c);
const isDigit = (c: string | undefined): boolean => c !== undefined && c >= "0" && c <= "9";

interface Bracket {
  from: number;
  bodyFrom: number;
  bodyTo: number;
  to: number;
  display: boolean;
}

interface Analysis {
  /** 1 inside fenced/inline code. */
  code: Uint8Array;
  /** Unescaped single `$` indices (not part of a `$$+` run), outside code. */
  singleDollars: number[];
  /** Start indices of maximal `$$+` runs outside code. */
  displayRuns: { index: number; size: number }[];
  /** Matched `$$…$$` pairs (no blank line between). */
  displayPairs: { open: number; openSize: number; close: number; closeSize: number }[];
  /** Closed bracket spans in source order. */
  brackets: Bracket[];
  /** Unclosed `\[` opener, or -1. */
  unclosedBracket: number;
  /** `$$` openers to render literal (paired across a blank line). */
  deadDisplayOpeners: Set<number>;
  /** Start of the unclosed-display literal tail, or -1. */
  literalTail: number;
  /** Start index of every blank line (\n[ \t\r]*\n), sorted. */
  blankStarts: number[];
}

/** Backslash run length immediately before index j. */
function backslashes(s: string, j: number): number {
  let k = j;
  while (k > 0 && s[k - 1] === "\\") k -= 1;
  return j - k;
}

/** First blank-line start at/after `from` that begins before `limit`, else limit. */
function blankBoundaryFrom(blankStarts: number[], from: number, limit: number): number {
  let lo = 0;
  let hi = blankStarts.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (blankStarts[mid]! < from) lo = mid + 1;
    else hi = mid;
  }
  const found = blankStarts[lo];
  return found !== undefined && found < limit ? found : limit;
}

/** One O(n) walk: mark code, collect dollars, pair bracket/display spans. */
function analyze(input: string): Analysis {
  const n = input.length;
  const code = new Uint8Array(n);
  const singleDollars: number[] = [];
  const displayRuns: { index: number; size: number }[] = [];
  const brackets: Bracket[] = [];
  const deadDisplayOpeners = new Set<number>();
  const blankStarts: number[] = [];
  for (let p = 0; ; ) {
    const nl = input.indexOf("\n", p);
    if (nl === -1) break;
    let q = nl + 1;
    while (q < n && (input[q] === " " || input[q] === "\t" || input[q] === "\r")) q += 1;
    if (input[q] === "\n") blankStarts.push(nl);
    p = nl + 1;
  }

  const markCode = (from: number, to: number) => {
    for (let k = from; k < to; k += 1) code[k] = 1;
  };

  // Top-level indented code blocks: a line beginning with ≥4 spaces (no tab,
  // no quote/list marker) that starts the document/follows a blank line or
  // continues an indented-code run. Math there (e.g. `    $$x$$`) is code.
  let lineStart = 0;
  let inIndentedCode = false;
  let prevWasBlank = true;
  for (let p = 0; p <= n; p += 1) {
    if (p === n || input[p] === "\n") {
      const line = input.slice(lineStart, p);
      const blank = /^[ \t]*$/.test(line);
      const indented = /^ {4,}(?![-*+][\t ]|\d+[.)][\t ])\S/.test(line);
      if (inIndentedCode && blank) {
        // blank line belongs to the code block; keep marking
      } else if (indented && (prevWasBlank || inIndentedCode)) {
        inIndentedCode = true;
        markCode(lineStart, p);
      } else if (!blank) {
        inIndentedCode = false;
      }
      prevWasBlank = blank;
      lineStart = p + 1;
    }
  }

  let unclosedBracket = -1;
  let i = 0;
  while (i < n) {
    const ch = input[i]!;

    // Fenced code block (line start, ≤3 spaces, ≥3 backticks/tildes).
    if ((i === 0 || input[i - 1] === "\n") && (ch === "`" || ch === "~")) {
      let j = i;
      while (j - i < 3 && input[j] === " ") j += 1;
      const marker = input[j];
      if (marker === "`" || marker === "~") {
        let run = 0;
        while (input[j + run] === marker) run += 1;
        if (run >= 3) {
          let p = j + run;
          while (p < n && input[p] !== "\n") p += 1;
          p += 1;
          let end = n;
          let foundClose = false;
          while (p < n && !foundClose) {
            let q = p;
            while (q - p < 3 && input[q] === " ") q += 1;
            if (input[q] === marker) {
              let cr = 0;
              while (input[q + cr] === marker) cr += 1;
              if (cr >= run) {
                let t = q + cr;
                while (t < n && (input[t] === " " || input[t] === "\t")) t += 1;
                if (t === n || input[t] === "\n" || input[t] === "\r") {
                  end = t === n ? t : t + 1;
                  foundClose = true;
                }
              }
            }
            if (!foundClose) {
              while (p < n && input[p] !== "\n") p += 1;
              p += 1;
            }
          }
          markCode(i, end);
          i = end;
          continue;
        }
      }
    }

    // Inline code span with an exact-length closing run.
    if (ch === "`") {
      let size = 0;
      while (input[i + size] === "`") size += 1;
      const needle = "`".repeat(size);
      let search = i + size;
      let end = -1;
      for (;;) {
        const close = input.indexOf(needle, search);
        if (close === -1) break;
        if (input[close + size] !== "`") {
          end = close + size;
          break;
        }
        search = close + size;
      }
      if (end !== -1) {
        markCode(i, end);
        i = end;
        continue;
      }
      i += size;
      continue;
    }

    // Bracket math opener.
    if (ch === "\\" && (input[i + 1] === "(" || input[i + 1] === "[")) {
      const display = input[i + 1] === "[";
      const closeNeedle = display ? "\\]" : "\\)";
      const boundary = display
        ? n
        : blankBoundaryFrom(blankStarts, i + 2, n);
      let close = -1;
      for (let j = i + 2; j < boundary - 1; j += 1) {
        if (input.startsWith(closeNeedle, j) && backslashes(input, j) % 2 === 0) {
          close = j;
          break;
        }
      }
      if (close === -1) {
        // Unclosed display swallows the tail; an unclosed inline \( is just
        // an escaped paren, so keep scanning for later dollars/markers.
        if (display) {
          unclosedBracket = i;
          break;
        }
        i += 2;
        continue;
      }
      brackets.push({ from: i, bodyFrom: i + 2, bodyTo: close, to: close + 2, display });
      i = close + 2;
      continue;
    }

    // Unescaped dollar outside any kind of code.
    if (ch === "$" && code[i] === 0 && backslashes(input, i) % 2 === 0) {
      if (input[i - 1] === "$" || input[i + 1] === "$") {
        // Entering/inside a maximal run; record its start once.
        if (input[i - 1] !== "$") {
          let size = 0;
          while (input[i + size] === "$") size += 1;
          displayRuns.push({ index: i, size });
          i += size;
          continue;
        }
      } else {
        singleDollars.push(i);
      }
    }

    i += 1;
  }

  // Pair display runs left-to-right. A run pairs with the next run unless a
  // blank line separates them: then BOTH runs are dead (literal) and scanning
  // continues after the would-be closer, so later formulas still render. Only
  // a genuinely dangling final run (no later run at all) swallows its tail.
  const displayPairs: { open: number; openSize: number; close: number; closeSize: number }[] = [];
  let literalTail = n;
  let k = 0;
  while (k < displayRuns.length) {
    const opener = displayRuns[k]!;
    const next = displayRuns[k + 1];
    if (!next) {
      literalTail = opener.index;
      break;
    }
    if (
      blankBoundaryFrom(blankStarts, opener.index + opener.size, next.index) !== next.index
    ) {
      deadDisplayOpeners.add(opener.index);
      deadDisplayOpeners.add(next.index);
      k += 2; // reject the pair, do not let the closer re-open
    } else {
      displayPairs.push({
        open: opener.index,
        openSize: opener.size,
        close: next.index,
        closeSize: next.size,
      });
      k += 2; // matched pair
    }
  }
  if (unclosedBracket !== -1) literalTail = Math.min(literalTail, unclosedBracket);

  return {
    code,
    singleDollars,
    displayRuns,
    displayPairs,
    brackets,
    unclosedBracket,
    deadDisplayOpeners,
    literalTail: literalTail === n ? -1 : literalTail,
    blankStarts,
  };
}

/**
 * Single `$` openers pandoc rejects but micromark would accept. The close
 * frontier never moves backwards → O(len(singles)). An opener with no later
 * `$` at all before the paragraph boundary is inert in micromark too and is
 * left alone (a lone trailing `$`).
 */
function rejectedOpeners(
  input: string,
  singles: number[],
  limit: number,
  blankStarts: number[],
): Set<number> {
  const rejected = new Set<number>();
  let frontier = 0;
  let k = 0;
  while (k < singles.length) {
    const opener = singles[k]!;
    if (opener >= limit) break;
    const boundary = blankBoundaryFrom(blankStarts, opener + 1, limit);
    // A later candidate dollar is what could make micromark open a span.
    const firstLater = lowerBound(singles, opener + 1);
    if (firstLater >= singles.length || singles[firstLater]! >= boundary) {
      k += 1; // inert: nothing can close it
      continue;
    }
    if (isSpace(input[opener + 1])) {
      rejected.add(opener);
      k += 1;
      frontier = Math.max(frontier, k);
      continue;
    }
    let closeK = -1;
    let j = Math.max(k + 1, frontier);
    for (; j < singles.length; j += 1) {
      const cand = singles[j]!;
      if (cand >= boundary) break;
      if (!isSpace(input[cand - 1]) && !isDigit(input[cand + 1])) {
        closeK = j;
        break;
      }
    }
    if (closeK !== -1) {
      k = closeK + 1;
      frontier = k;
    } else {
      // A candidate exists but fails pandoc's edge/digit rules: micromark
      // would still pair it, so escape this opener.
      rejected.add(opener);
      frontier = Math.max(frontier, lowerBound(singles, boundary));
      k += 1;
    }
  }
  return rejected;
}

/** Index of the first value ≥ target (binary search). */
function lowerBound(arr: number[], target: number): number {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (arr[mid]! < target) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * Escape markdown-control characters so a tail renders literally. A
 * backslash is doubled and the character right after it is emitted raw (it is
 * already protected); standalone `* _ $ ` [` that would otherwise transform
 * are backslash-escaped.
 */
function escapeLiteralTail(s: string): string {
  let out = "";
  let protectedNext = false;
  for (const ch of s) {
    if (ch === "\\") {
      out += "\\\\";
      protectedNext = true;
      continue;
    }
    if (!protectedNext && (ch === "*" || ch === "_" || ch === "$" || ch === "`" || ch === "[")) {
      out += "\\";
    }
    out += ch;
    protectedNext = false;
  }
  return out;
}

/** Whitespace-only container prefix (`> `, `- `, `1. ` …) on index's line. */
function linePrefix(input: string, i: number): string {
  let lineStart = i;
  while (lineStart > 0 && input[lineStart - 1] !== "\n") lineStart -= 1;
  const prefix = input.slice(lineStart, i);
  if (!/^\s*(?:>[\t ]*)*(?:[-*+]|\d+[.)])[\t ]+(?:>[\t ]*)?$/.test(prefix)) {
    return /^[\s>]+$/.test(prefix) ? prefix : "";
  }
  return prefix;
}

/**
 * Build a remark-math `$$` flow fence for a display body, honoring the
 * container prefix on the opener's line. Quote prefixes (`> `) repeat on
 * continuation lines; a list marker (`- `, `1. `) is replaced by spaces of
 * the same width so the body and close fence are the SAME item's
 * continuation, not three list items.
 */
function continuationPrefix(prefix: string): string {
  return prefix.replace(/[-*+]|\d+[.)]/, (m) => " ".repeat(m.length));
}

function displayFence(prefix: string, body: string): string {
  const trimmed = body.trim();
  if (prefix === "") return `\n\n$$\n${trimmed}\n$$\n\n`;
  const cont = continuationPrefix(prefix);
  const inner = trimmed.split("\n").map((l) => `${cont}${l}`).join("\n");
  return `\n${prefix}$$\n${inner}\n${cont}$$\n`;
}

/** Replacement text for a closed bracket span. */
function bracketReplacement(input: string, b: Bracket): string {
  const token = (s: string) => s.replace(/\$/g, MATH_DOLLAR);
  if (!b.display) return `$${token(input.slice(b.bodyFrom, b.bodyTo))}$`;
  return displayFence(linePrefix(input, b.from), token(input.slice(b.bodyFrom, b.bodyTo)));
}

/**
 * Replacement for a matched `$$…$$` pair. When the pair already occupies
 * separate lines micromark parses a flow fence itself, so it is copied
 * verbatim (container markers and all). A same-line pair (`$$x$$`, which
 * micromark would treat as inline) is rewritten to a prefix-aware fence.
 */
function dollarPairReplacement(
  input: string,
  pair: { open: number; openSize: number; close: number; closeSize: number },
): { text: string; to: number } | null {
  const bodyStart = pair.open + pair.openSize;
  const bodyEnd = pair.close;
  const between = input.slice(bodyStart, bodyEnd);
  if (between.includes("\n")) return null; // already a multi-line flow fence
  const prefix = linePrefix(input, pair.open);
  const body = between.trim().replace(/\$/g, MATH_DOLLAR);
  return { text: displayFence(prefix, body), to: pair.close + pair.closeSize };
}

/**
 * Prepare markdown source for remark-math. Linear; safe on every streaming
 * append.
 */
export function protectMath(input: string): string {
  const n = input.length;
  if (n === 0) return input;
  const a = analyze(input);
  const limit = a.literalTail === -1 ? n : a.literalTail;

  if (
    a.singleDollars.length === 0 &&
    a.brackets.length === 0 &&
    a.displayPairs.length === 0 &&
    a.deadDisplayOpeners.size === 0 &&
    limit === n
  ) {
    return input;
  }

  const rejected =
    a.singleDollars.length > 0
      ? rejectedOpeners(input, a.singleDollars, limit, a.blankStarts)
      : new Set<number>();
  const bracketAt = new Map<number, Bracket>();
  for (const b of a.brackets) if (b.from < limit) bracketAt.set(b.from, b);
  const pairAt = new Map<number, (typeof a.displayPairs)[number]>();
  for (const p of a.displayPairs) if (p.open < limit) pairAt.set(p.open, p);

  let out = "";
  let i = 0;
  while (i < limit) {
    if (a.code[i] === 1) {
      out += input[i]!;
      i += 1;
      continue;
    }
    const pair = pairAt.get(i);
    if (pair) {
      const repl = dollarPairReplacement(input, pair);
      if (repl) {
        // The opener's container prefix (`> `, `- `, …) was already emitted
        // as we walked the line; drop it so the prefix-aware fence replaces
        // the whole marker line instead of leaving an empty container line.
        const pfx = linePrefix(input, pair.open);
        if (pfx !== "" && out.endsWith(pfx)) out = out.slice(0, out.length - pfx.length);
        out += repl.text;
        i = repl.to;
        continue;
      }
    }
    const bracket = bracketAt.get(i);
    if (bracket) {
      out += bracketReplacement(input, bracket);
      i = bracket.to;
      continue;
    }
    if (rejected.has(i)) {
      out += "\\$";
      i += 1;
      continue;
    }
    if (a.deadDisplayOpeners.has(i)) {
      const run = a.displayRuns.find((r) => r.index === i)!;
      out += "\\$".repeat(run.size);
      i += run.size;
      continue;
    }
    out += input[i]!;
    i += 1;
  }

  if (a.literalTail !== -1) out += escapeLiteralTail(input.slice(a.literalTail));
  return out;
}

/** Restore the literal `$` tokens carried through a bracket-formula body. */
export function restoreMathSource(source: string): string {
  return source.replace(/\uE000/g, "$");
}
