/**
 * Parser-driven code ranges (c-math round 3, design A).
 *
 * The math delimiter scanner must never treat `$`/`\[` inside code as math.
 * Rather than re-implementing every code form (fences, tildes, indented
 * blocks, indented tilde fences, code spans, raw HTML), we run the SAME
 * remark pipeline the message is rendered with — but WITHOUT remark-math —
 * and take the source positions of the parser's `code`, `inlineCode` and
 * `html` nodes. Any future code construct the parser learns is covered for
 * free. The transform is cheap (parse only; no rehype/React) and runs once
 * per message.
 */
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";

export interface OffsetRange {
  start: number;
  end: number;
}

type PositionedNode = {
  type: string;
  position?: { start: { offset?: number }; end: { offset?: number } };
  children?: PositionedNode[];
};

const CODE_TYPES = new Set(["code", "inlineCode", "html"]);

const parser = unified().use(remarkParse).use(remarkGfm);

/**
 * Cheap pre-filter: could this source possibly contain code? Every code form
 * needs a backtick (fence/span), a tilde fence, an indented/tab line, or a raw
 * `<`. If none is present there is provably no `code`/`inlineCode`/`html`
 * node, so the (relatively expensive) full parse is skipped. This keeps the
 * linear adversarial paths (`$1$1…`, `\(\(…`, giant formulas) off the parser
 * entirely while every message that actually has code is parsed.
 */
function mightContainCode(markdown: string): boolean {
  if (markdown.includes("`") || markdown.includes("~") || markdown.includes("<")) return true;
  let atLineStart = true;
  let spaces = 0;
  for (let i = 0; i < markdown.length; i += 1) {
    const ch = markdown[i]!;
    if (atLineStart && ch === "\t") return true;
    if (atLineStart && ch === " ") {
      spaces += 1;
      if (spaces >= 4) return true;
      continue;
    }
    spaces = 0;
    atLineStart = ch === "\n";
  }
  return false;
}

/** Sorted, non-overlapping byte ranges that are code/raw-HTML, never math. */
export function codeRanges(markdown: string): OffsetRange[] {
  if (!mightContainCode(markdown)) return [];
  let tree: PositionedNode;
  try {
    tree = parser.parse(markdown) as unknown as PositionedNode;
  } catch {
    return [];
  }
  const ranges: OffsetRange[] = [];
  const walk = (node: PositionedNode): void => {
    if (CODE_TYPES.has(node.type)) {
      const pos = node.position;
      if (pos && typeof pos.start.offset === "number" && typeof pos.end.offset === "number") {
        ranges.push({ start: pos.start.offset, end: pos.end.offset });
      }
    }
    for (const child of node.children ?? []) walk(child);
  };
  walk(tree);
  ranges.sort((a, b) => a.start - b.start || a.end - b.end);
  return ranges;
}

/** O(1) membership mask over the source for the given ranges. */
export function codeMask(markdown: string, ranges: OffsetRange[]): Uint8Array {
  const mask = new Uint8Array(markdown.length);
  for (const { start, end } of ranges) {
    for (let k = start; k < end && k < markdown.length; k += 1) mask[k] = 1;
  }
  return mask;
}
