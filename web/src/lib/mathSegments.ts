/**
 * Pre-markdown math preparation (c-math round 3).
 *
 * Design (c-math round-3 brief):
 *  - Code is owned by the PARSER. `codeRanges()` runs the same remark pipeline
 *    the message is rendered with, minus math, and reports `code`,
 *    `inlineCode` and `html` source ranges; the scanner skips them. Fenced,
 *    tilde, indented and span code — present and future — are all covered.
 *  - One linear analysis. A single forward walk (outside code) records bracket
 *    spans, single `$` and `$$` runs; pairing walks those arrays once with no
 *    backtracking. Every source char is examined O(1) times.
 *  - In-place translation only: `\(x\)` -> `$x$`, `\[x\]` -> `$$x$$`.
 *    Delimiters are replaced two chars for two, so no newlines, blank lines
 *    or fences are injected; list/quote markers are never touched. A display
 *    `\[ … \]` pair may span several lines within one paragraph (no blank
 *    line), like `$$`; an inline `\( … \)` always stays single-line. Whether
 *    a same-line `$$…$$` renders as a block is decided by the provenance
 *    plugin from its `$$` delimiter.
 *  - Every unescaped `$` that is not a delimiter of an ACCEPTED pair is
 *    emitted as `\$`, so remark-math can never pair differently from this
 *    scanner.
 *  - A genuinely unclosed DISPLAY opener whose closer search reaches the END
 *    of the source (a trailing `$$` run with no later run, or an unclosed
 *    `\[` with nothing after its paragraph) is returned as `literalTail`,
 *    which MarkdownText renders as a plain React text node — exact source,
 *    never fed back through markdown. That is the streaming case: a closer
 *    may still arrive with the next append, so the half-formula must not be
 *    half-parsed. A `\[` rejected earlier — its paragraph ends on a blank
 *    line or is interrupted by a fenced block BEFORE EOF — is not a
 *    streaming half-formula: only the two-char opener is emitted literal
 *    (`\\[`, rendered exactly as `\[`) and the scan keeps rendering after
 *    it, so later paragraphs/lists/code never lose markdown.
 *
 * Single-$ acceptance follows pandoc's mathInline grammar: the FIRST later
 * unescaped `$` within the same paragraph decides — accepted only when it is
 * preceded by a non-space and followed by a non-digit; otherwise the opener
 * is literal and the next `$` is retried as an opener.
 */

import { codeMask, codeRanges, type OffsetRange } from "./mathCodeRanges";

/** Stand-in for a literal `$` inside a bracket-translated formula body. */
export const MATH_DOLLAR = "\uE000";

const SPACE_RE = /[ \t\r\n\f\v]/;
const isSpace = (c: string | undefined): boolean => c !== undefined && SPACE_RE.test(c);
const isDigit = (c: string | undefined): boolean => c !== undefined && c >= "0" && c <= "9";

function backslashes(s: string, j: number): number {
  let k = j;
  while (k > 0 && s[k - 1] === "\\") k -= 1;
  return j - k;
}

interface Run {
  index: number;
  size: number;
  line: number;
}
interface Single {
  index: number;
  paragraph: number;
}
interface Bracket {
  from: number;
  to: number;
  display: boolean;
  bodyFrom: number;
  bodyTo: number;
}

export interface PreparedMath {
  markdown: string;
  literalTail: string | null;
}

/**
 * End of the paragraph containing `from`: the index of the first `\n` that,
 * together with the next line, forms a blank-line separator (`\n[ \t\r]*\n`),
 * or the end of the source. Mirrors the `$$` pairing rule in pairRuns().
 */
function paragraphEnd(s: string, from: number): number {
  const n = s.length;
  for (let k = from; k < n; k += 1) {
    if (s[k] !== "\n") continue;
    let j = k + 1;
    while (j < n && (s[j] === " " || s[j] === "\t" || s[j] === "\r")) j += 1;
    if (s[j] === "\n") return k;
  }
  return n;
}

/** Maps each index to a paragraph id (incremented at every blank line). */
function paragraphOfFn(s: string): (i: number) => number {
  const starts: number[] = [0];
  for (let i = 0; i < s.length - 1; i += 1) {
    if (s[i] === "\n") {
      let j = i + 1;
      while (j < s.length && (s[j] === " " || s[j] === "\t" || s[j] === "\r")) j += 1;
      if (s[j] === "\n") starts.push(j + 1);
    }
  }
  return (i: number) => {
    let lo = 0;
    let hi = starts.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (starts[mid]! <= i) lo = mid + 1;
      else hi = mid;
    }
    return lo - 1;
  };
}

interface UnclosedDisplay {
  index: number;
  /** True when the closer search ran to EOF (the streaming tail case). */
  eof: boolean;
}

interface Collected {
  singles: Single[];
  runs: Run[];
  brackets: Bracket[];
  unclosed: UnclosedDisplay[];
}

/** Single forward walk outside code. */
function collect(s: string, code: Uint8Array, blocks: OffsetRange[]): Collected {
  const n = s.length;
  const singles: Single[] = [];
  const runs: Run[] = [];
  const brackets: Bracket[] = [];
  const unclosed: UnclosedDisplay[] = [];
  let line = 0;
  let parenScan = 0;
  let bracketScan = 0;
  // End of the line/paragraph the cursor is on. Each is recomputed only when
  // the cursor crosses the cached value, so a long line of unmatched openers
  // costs O(L) total, not O(N x L) (round-4 fix 1).
  const firstNl = s.indexOf("\n");
  let le = firstNl === -1 ? n : firstNl;
  let pe = -1;
  // Pointer into the sorted block-code ranges; advanced with the cursor.
  let blockPtr = 0;

  for (let i = 0; i < n; i += 1) {
    const ch = s[i]!;
    if (ch === "\n") line += 1;
    if (code[i] === 1) continue;

    if (ch === "\\" && (s[i + 1] === "(" || s[i + 1] === "[") && backslashes(s, i) % 2 === 0) {
      const display = s[i + 1] === "[";
      const needle = display ? "\\]" : "\\)";
      if (i >= le) {
        const nl = s.indexOf("\n", i);
        le = nl === -1 ? n : nl;
      }
      // Inline \(…\) never crosses a newline. Display \[…\] may span lines
      // within the same paragraph (no blank line), like $$ (round-4 fix 2).
      let limit = le;
      if (display) {
        if (pe === -1 || i >= pe) pe = paragraphEnd(s, i);
        limit = pe;
        // A fenced code block can interrupt the paragraph with no blank line.
        // Pairing across it would swallow the fence into math, so cap the
        // closer search at the block and leave the opener unclosed.
        while (blockPtr < blocks.length && blocks[blockPtr]!.end <= i) blockPtr += 1;
        if (blockPtr < blocks.length) {
          const blockStart = blocks[blockPtr]!.start;
          if (blockStart > i && blockStart < limit) limit = blockStart;
        }
      }
      const scan = display ? bracketScan : parenScan;
      let close = -1;
      for (let j = Math.max(scan, i + 2); j < limit - 1; j += 1) {
        if (s.startsWith(needle, j) && code[j] === 0 && backslashes(s, j) % 2 === 0) {
          close = j;
          break;
        }
      }
      if (close !== -1) {
        brackets.push({ from: i, to: close + 2, display, bodyFrom: i + 2, bodyTo: close });
        if (display) bracketScan = close + 2;
        else parenScan = close + 2;
        i = close + 1;
        continue;
      }
      // No closer in range. A later opener on that same range cannot close
      // either (its range is a subset), so park the scan cursor at the limit —
      // an unclosed `\(`/`\[` never rescans the tail (design B).
      if (display) bracketScan = limit;
      else parenScan = limit;
      // EOF-reaching failure = streaming half-formula (literal tail). A
      // failure capped by a blank line / fenced block = rejected opener:
      // only `\[` goes literal, rendering resumes after it.
      if (display) unclosed.push({ index: i, eof: limit === n });
      continue;
    }

    if (ch === "$" && backslashes(s, i) % 2 === 0) {
      if (s[i + 1] === "$") {
        if (s[i - 1] !== "$") {
          let size = 0;
          while (s[i + size] === "$") size += 1;
          if (size >= 2) runs.push({ index: i, size, line });
          i += size - 1;
        }
      } else {
        singles.push({ index: i, paragraph: 0 });
      }
    }
  }
  return { singles, runs, brackets, unclosed };
}

/**
 * Accepted single-$ delimiters (pandoc mathInline). In one linear sweep each
 * opener's fate is decided by the very next single-$ in the same paragraph:
 * non-space edge and non-digit after make a pair; otherwise the opener is
 * literal and the next `$` is retried.
 */
function pairSingles(s: string, singles: Single[]): Set<number> {
  const accepted = new Set<number>();
  for (let k = 0; k < singles.length; k += 1) {
    const a = singles[k]!;
    const b = singles[k + 1];
    if (!b || b.paragraph !== a.paragraph) continue;
    const opener = a.index;
    const closer = b.index;
    if (isSpace(s[opener + 1])) continue;
    if (isSpace(s[closer - 1])) continue;
    if (isDigit(s[closer + 1])) continue;
    accepted.add(opener);
    accepted.add(closer);
    k += 1;
  }
  return accepted;
}

/**
 * Pair `$$` runs. Same-line runs bind; on later lines a run binds with the
 * next only across a single newline (no blank line). A run that cannot bind
 * is killed (escaped) and the next run is reconsidered; a final run with no
 * later run is the streaming literal tail.
 */
function pairRuns(s: string, runs: Run[]): { accepted: Set<number>; tail: number } {
  const accepted = new Set<number>();
  let k = 0;
  while (k < runs.length) {
    const op = runs[k]!;
    const next = runs[k + 1];
    if (!next) return { accepted, tail: op.index };
    if (next.line === op.line || !/\n[ \t\r]*\n/.test(s.slice(op.index + op.size, next.index))) {
      accepted.add(op.index);
      accepted.add(next.index);
      k += 2;
    } else {
      k += 1; // kill only this opener; reconsider the would-be closer
    }
  }
  return { accepted, tail: -1 };
}

export function prepareMath(input: string): PreparedMath {
  if (input.length === 0) return { markdown: input, literalTail: null };

  const ranges = codeRanges(input);
  const code = codeMask(input, ranges);
  const blocks = ranges.filter((range) => range.block);
  const { singles, runs, brackets, unclosed } = collect(input, code, blocks);
  // Paragraph ids are only needed for single-$ pairing; skip the extra pass on
  // messages without one.
  if (singles.length > 0) {
    const paragraphOf = paragraphOfFn(input);
    for (const single of singles) single.paragraph = paragraphOf(single.index);
  }
  const acceptedSingles = pairSingles(input, singles);
  const { accepted: acceptedRuns, tail: runTail } = pairRuns(input, runs);

  const bracketByFrom = new Map<number, Bracket>();
  for (const b of brackets) bracketByFrom.set(b.from, b);
  const runAt = new Map<number, Run>();
  for (const r of runs) runAt.set(r.index, r);
  // Openers whose range ends before EOF (blank line / fenced block) are merely
  // rejected: render the two delimiter chars literal and continue with the
  // rest of the message.
  const rejectedDisplayAt = new Set<number>();
  let eofDisplay = -1;
  for (const u of unclosed) {
    if (u.eof) {
      if (eofDisplay === -1 || u.index < eofDisplay) eofDisplay = u.index;
    } else {
      rejectedDisplayAt.add(u.index);
    }
  }

  // Only an opener whose closer search reaches EOF starts the streaming
  // literal tail (alongside a final unpaired `$$` run).
  const tailStart =
    runTail === -1 ? eofDisplay : Math.min(runTail, eofDisplay === -1 ? runTail : eofDisplay);
  const limit = tailStart === -1 ? input.length : tailStart;

  // Fast path: no accepted math, no code/HTML, no tail — the only edit is
  // escaping unescaped `$`. A single regex pass handles it without a million
  // output segments (covers both currency prose and the `$1$1…` adversarial
  // line). Backslash escapes (incl. `\$`) are consumed verbatim.
  if (
    brackets.length === 0 &&
    rejectedDisplayAt.size === 0 &&
    acceptedSingles.size === 0 &&
    acceptedRuns.size === 0 &&
    ranges.length === 0 &&
    tailStart === -1 &&
    input.indexOf("$") !== -1
  ) {
    // No backslash escapes in the source: every `$` is unescaped — a native
    // global string replace (no per-match callback) handles the `$1$1…` line.
    if (!/\\/.test(input)) return { markdown: input.replace(/\$/g, "\\$"), literalTail: null };
    const escaped = input.replace(/\\[\s\S]?|\${1,}/g, (m) =>
      m[0] === "\\" ? m : "\\$".repeat(m.length),
    );
    return { markdown: escaped, literalTail: null };
  }

  // Verbatim ranges are emitted as slices joined with the replacements; an
  // unchanged message is one O(1) slice, never a char-by-char string build.
  const segments: string[] = [];
  let segStart = 0;
  const flushVerbatim = (end: number): void => {
    if (end > segStart) segments.push(input.slice(segStart, end));
  };
  let i = 0;
  while (i < limit) {
    if (code[i] === 1) {
      i += 1;
      continue;
    }
    const bracket = bracketByFrom.get(i);
    if (bracket) {
      flushVerbatim(i);
      const token = (x: string) => x.replace(/\$/g, MATH_DOLLAR);
      const body = token(input.slice(bracket.bodyFrom, bracket.bodyTo));
      segments.push(bracket.display ? `$$${body}$$` : `$${body}$`);
      i = bracket.to;
      segStart = i;
      continue;
    }
    if (rejectedDisplayAt.has(i)) {
      // Keep the rejected opener as EXACT visible source and fully inert:
      // `\\` renders a literal backslash and `\[` a literal bracket. Both
      // must be escaped — escaping only the backslash leaves a live `[`,
      // which turns `\[label](url)` into a real link. Rendering resumes on
      // the next char.
      flushVerbatim(i);
      segments.push("\\\\\\[");
      i += 2;
      segStart = i;
      continue;
    }
    if (input[i] === "$" && backslashes(input, i) % 2 === 0) {
      const run = runAt.get(i);
      flushVerbatim(i);
      if (run) {
        segments.push(acceptedRuns.has(i) ? "$".repeat(run.size) : "\\$".repeat(run.size));
        i += run.size;
      } else {
        segments.push(acceptedSingles.has(i) ? "$" : "\\$");
        i += 1;
      }
      segStart = i;
      continue;
    }
    i += 1;
  }
  flushVerbatim(limit);
  const out = segments.join("");

  return { markdown: out, literalTail: tailStart === -1 ? null : input.slice(tailStart) };
}

/** Restore the literal `$` tokens carried through a bracket-formula body. */
export function restoreMathSource(source: string): string {
  return source.replace(/\uE000/g, "$");
}
