import { createContext, useContext, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import ui from "../styles/ui.module.css";
import css from "./codeBlock.module.css";
import { CodeBlock } from "./CodeBlock";

function nodeText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join("");
  return "";
}

/**
 * Markdown renders a fenced block as `<pre><code>`, inline code as a bare
 * `<code>` with no `<pre>` ancestor. react-markdown v9 gives the code override
 * no `inline` flag, so the pre override marks its subtree: any code inside is
 * a fence.
 */
const FenceContext = createContext(false);

type CodeProps = {
  className?: string;
  children?: ReactNode;
  /** micromark keeps the fence meta (`a.ts` in ` ```ts a.ts `) on the hast node. */
  node?: { data?: { meta?: string | null } };
};

/** react-markdown's code override: inline stays a bare <code>, fences become CodeBlock. */
function FencedCode(rawProps: unknown) {
  const props = rawProps as CodeProps;
  if (!useContext(FenceContext)) {
    return <code className={props.className}>{props.children}</code>;
  }
  const match = /language-(\S+)/.exec(props.className ?? "");
  const lang = match?.[1] ?? null;
  // The info string is "lang meta" (e.g. `ts src/app.ts`); sanitize keeps the
  // meta on the hast node rather than on className.
  const meta = props.node?.data?.meta?.trim();
  const info = meta ? (lang ? `${lang} ${meta}` : meta) : lang;
  const code = nodeText(props.children).replace(/\n$/, "");
  return <CodeBlock code={code} info={info} />;
}

/**
 * react-markdown + remark-gfm + rehype-sanitize: GFM without raw HTML; code
 * colour is lazy on demand. Fenced code renders through CodeBlock (wrap /
 * copy / 评论 toolbar); inline code keeps the default element.
 */
export function MarkdownText({ text }: { text: string }) {
  return (
    <div className={`${ui.md} ${css.mdSurface}`}>
      <Markdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeSanitize]}
        components={{
          pre: ({ children }) => (
            <FenceContext.Provider value={true}>{children}</FenceContext.Provider>
          ),
          code: FencedCode,
        }}
      >
        {text}
      </Markdown>
    </div>
  );
}
