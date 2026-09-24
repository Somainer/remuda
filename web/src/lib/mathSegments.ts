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
 *  - In-place translation only: `\(x\)` -> `$x$`, `\[x\]` -> `$$x$$` on the
 *    same line. No newlines, blank lines or fences are injected; list/quote
 *    markers are never touched. Whether a same-line `$$…$$` renders as a
 *    block is decided by the provenance plugin from its `$$` delimiter.
 *  - Every unescaped `$` that is not a delimiter of an ACCEPTED pair is
 *    emitted as `\$`, so remark-math can never pair differently from this
 *    scanner.
 *  - A genuinely unclosed DISPLAY opener (a trailing `$$` run with no later
 *    run, or an unclosed `\[`) is returned as `literalTail`, which
 *    MarkdownText renders as a plain React text node — exact source, never
 *    fed back through markdown.
 *
 * Single-$ acceptance follows pandoc's mathInline grammar: the FIRST later
 * unescaped `$` within the same paragraph decides — accepted only when it is
 * preceded by a non-space and followed by a non-digit; otherwise the opener
 * is literal and the next `$` is retried as an opener.
 */

import { codeMask, codeRanges } from "./mathCodeRanges";

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

function lineEnd(s: string, from: number): number {
  const nl = s.indexOf("\n", from);
  return nl === -1 ? s.length : nl;
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

interface Collected {
  singles: Single[];
  runs: Run[];
  brackets: Bracket[];
  unclosedDisplay: number;
}

/** Single forward walk outside code. */
function collect(
  s: string,
  code: Uint8Array,
  paragraphOf: (i: number) => number,
): Collected {
  const n = s.length;
  const singles: Single[] = [];
  const runs: Run[] = [];
  const brackets: Bracket[] = [];
  let unclosedDisplay = -1;
  let line = 0;
  let parenScan = 0;
  let bracketScan = 0;

  for (let i = 0; i < n; i += 1) {
    const ch = s[i]!;
    if (ch === "\n") line += 1;
    if (code[i] === 1) continue;

    if (ch === "\\" && (s[i + 1] === "(" || s[i + 1] === "[") && backslashes(s, i) % 2 === 0) {
      const display = s[i + 1] === "[";
      const needle = display ? "\\]" : "\\)";
      const scan = display ? bracketScan : parenScan;
      const le = lineEnd(s, i);
      let close = -1;
      for (let j = Math.max(scan, i + 2); j < le - 1; j += 1) {
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
      // No closer on the rest of this line. A later opener on the same line
      // cannot close either (its range is a subset), so advance the cursor to
      // the line end — an unclosed `\(`/`\[` never rescans the tail (design B).
      if (display) bracketScan = le;
      else parenScan = le;
      if (display && unclosedDisplay === -1) unclosedDisplay = i;
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
        singles.push({ index: i, paragraph: paragraphOf(i) });
      }
    }
  }
  return { singles, runs, brackets, unclosedDisplay };
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

  const code = codeMask(input, codeRanges(input));
  const paragraphOf = paragraphOfFn(input);
  const { singles, runs, brackets, unclosedDisplay } = collect(input, code, paragraphOf);
  const acceptedSingles = pairSingles(input, singles);
  const { accepted: acceptedRuns, tail: runTail } = pairRuns(input, runs);

  const bracketByFrom = new Map<number, Bracket>();
  for (const b of brackets) bracketByFrom.set(b.from, b);
  const runAt = new Map<number, Run>();
  for (const r of runs) runAt.set(r.index, r);

  const tailStart =
    runTail === -1 ? unclosedDisplay : Math.min(runTail, unclosedDisplay === -1 ? runTail : unclosedDisplay);
  const limit = tailStart === -1 ? input.length : tailStart;

  let out = "";
  let i = 0;
  while (i < limit) {
    if (code[i] === 1) {
      out += input[i]!;
      i += 1;
      continue;
    }
    const bracket = bracketByFrom.get(i);
    if (bracket) {
      const token = (x: string) => x.replace(/\$/g, MATH_DOLLAR);
      const body = token(input.slice(bracket.bodyFrom, bracket.bodyTo));
      out += bracket.display ? `$$${body}$$` : `$${body}$`;
      i = bracket.to;
      continue;
    }
    if (input[i] === "$" && backslashes(input, i) % 2 === 0) {
      const run = runAt.get(i);
      if (run) {
        out += acceptedRuns.has(i) ? "$".repeat(run.size) : "\\$".repeat(run.size);
        i += run.size;
        continue;
      }
      out += acceptedSingles.has(i) ? "$" : "\\$";
      i += 1;
      continue;
    }
    out += input[i]!;
    i += 1;
  }

  return { markdown: out, literalTail: tailStart === -1 ? null : input.slice(tailStart) };
}

/** Restore the literal `$` tokens carried through a bracket-formula body. */
export function restoreMathSource(source: string): string {
  return source.replace(/\uE000/g, "$");
}
