/**
 * Provenance marker for genuine math nodes (c-math round 3, design F).
 *
 * We must distinguish real math from a fenced code block whose info string is
 * `math` / `mathdisplay` / `mathinline` (those are CODE and must render on the
 * CodeBlock path without loading KaTeX). react-markdown gives both a `<code>`
 * element, so this remark plugin stamps every mdast math node with
 * `node.data.hProperties.dataMath`:
 *  - a flow `math` node is always `display`;
 *  - an `inlineMath` node is `display` only when its source delimiter was
 *    `$$` (a same-line `$$x$$` / in-list `\[x\]` the scanner left in place),
 *    otherwise `inline`.
 *
 * A fence cannot produce this property, so the `code` component override can
 * trust it. The property survives rehype-sanitize because MarkdownText
 * extends the default schema to allow `dataMath` on `<code>`.
 */

import type { Plugin } from "unified";

type MathLike = {
  type: string;
  value?: string;
  data?: { hProperties?: Record<string, unknown> };
  position?: { start: { offset?: number } };
  children?: MathLike[];
};

const stamp = (node: MathLike, source: string | undefined): void => {
  let kind: "display" | "inline";
  if (node.type === "math") {
    kind = "display";
  } else {
    const start = node.position?.start.offset;
    kind =
      source !== undefined && start !== undefined && source.startsWith("$$", start)
        ? "display"
        : "inline";
  }
  node.data ??= {};
  node.data.hProperties ??= {};
  node.data.hProperties.dataMath = kind;
};

const walk = (node: MathLike, source: string | undefined): void => {
  if (node.type === "math" || node.type === "inlineMath") stamp(node, source);
  for (const child of node.children ?? []) walk(child, source);
};

/**
 * Remark plugin. react-markdown provides the transformed source as the VFile
 * `value`. Exported as a plain attacher; MarkdownText casts it to its own
 * plugin-list element type (the unified Plugin generics conflict across the
 * transitive/unified copies).
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export const remarkMathProvenance: Plugin = function () {
  return (tree: any, file: any): void => {
    const source = typeof file?.value === "string" ? file.value : undefined;
    walk(tree as MathLike, source);
  };
};
