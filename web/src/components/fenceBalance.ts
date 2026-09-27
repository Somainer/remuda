/**
 * Close an unterminated code fence in streaming markdown, at render time only.
 *
 * While an assistant reply streams, the text often stops inside a ``` block.
 * Rendered as-is, everything after the opening fence flips between prose and
 * code on every batch, and the row jumps. Appending the missing closing fence
 * keeps the partial block a code block until the real one arrives. The stored
 * text is never changed; callers apply this only while the message streams.
 *
 * The decision is the shipped markdown parser's, not a hand scan: the text is
 * parsed with the same remark plugins MarkdownText renders with, and a closer
 * is appended only when the last block (through quotes and list items) is a
 * fenced `code` node that runs to the end of the text without its closing
 * fence. HTML blocks, indented code, inline code spans and fences whose
 * container already ended are the parser's business and are left alone.
 */
import Markdown, { type Options } from "react-markdown";
import remarkGfm from "remark-gfm";

type Point = { offset?: number };
type MdNode = {
  type: string;
  children?: MdNode[];
  position?: { start: Point; end: Point };
};

/** Containers whose last child is still "the last block" of the document. */
const CONTAINERS = new Set(["blockquote", "list", "listItem"]);

const OPENER = /^(`{3,}|~{3,})/;

/**
 * The mdast of `text` as MarkdownText parses it. react-markdown's `Markdown`
 * is synchronous and uses no hooks; the capture plugin keeps the tree and
 * hands an empty root on so nothing past parsing does real work.
 */
function parse(text: string): MdNode | null {
  let tree: MdNode | null = null;
  const capture = () => (root: MdNode) => {
    tree = root;
    return { type: "root", children: [] };
  };
  const plugins = [remarkGfm, capture] as NonNullable<Options["remarkPlugins"]>;
  Markdown({ children: text, remarkPlugins: plugins });
  return tree;
}

function lastBlock(root: MdNode): MdNode | null {
  let node = root.children?.at(-1) ?? null;
  while (node && CONTAINERS.has(node.type)) node = node.children?.at(-1) ?? null;
  return node;
}

/** Strip container prefixes (`>` and indent) from one source line. */
function content(line: string): string {
  return line.replace(/^(?:\s*>)*\s*/, "");
}

export function balanceFences(text: string): string {
  const root = parse(text);
  const code = root && lastBlock(root);
  const start = code?.position?.start.offset;
  const end = code?.position?.end.offset;
  if (!code || code.type !== "code" || start === undefined || end === undefined) return text;
  // Only a block that runs to the end of the text can be the open one.
  if (text.slice(end).trim() !== "") return text;
  // A fenced block starts on its fence run; indented code starts on its
  // indent and has no fence to close.
  const opener = OPENER.exec(text.slice(start));
  if (!opener) return text;
  const run = opener[1];
  const lines = text.slice(start, end).split("\n");
  if (lines.length > 1) {
    const closer = new RegExp(`^\\${run[0]}{${run.length},}\\s*$`);
    if (closer.test(content(lines[lines.length - 1]))) return text;
  }
  // The closer repeats the opener line's container prefix: quote markers stay,
  // list markers become their content indent.
  const lineStart = text.lastIndexOf("\n", start - 1) + 1;
  const prefix = text.slice(lineStart, start).replace(/[^>\s]/g, " ");
  const close = prefix + run;
  return text.endsWith("\n") ? `${text}${close}` : `${text}\n${close}`;
}
