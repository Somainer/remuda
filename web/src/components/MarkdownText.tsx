import { isValidElement, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import ui from "../styles/ui.module.css";
import { CodeBlock } from "./CodeBlock";

function nodeText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join("");
  if (isValidElement(node)) return nodeText((node.props as { children?: ReactNode }).children);
  return "";
}

type CodeChildProps = { className?: string; children?: ReactNode };

/**
 * react-markdown renders fenced code as <pre><code class="language-x">. Pull
 * the info string and the raw text out so CodeBlock owns the block; anything
 * else inside a pre falls back to the default element.
 */
function renderPre(children: ReactNode): ReactNode {
  const child = Array.isArray(children) ? children[0] : children;
  if (isValidElement(child) && child.type === "code") {
    const props = child.props as CodeChildProps;
    const match = /language-(\S+)/.exec(props.className ?? "");
    const info = match?.[1] ?? null;
    const code = nodeText(props.children).replace(/\n$/, "");
    return <CodeBlock code={code} info={info} />;
  }
  return <pre>{children}</pre>;
}

/** react-markdown + remark-gfm + rehype-sanitize: GFM without raw HTML; code colour is lazy on demand. */
export function MarkdownText({ text }: { text: string }) {
  return (
    <div className={ui.md}>
      <Markdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeSanitize]}
        components={{
          pre: ({ children }) => renderPre(children),
        }}
      >
        {text}
      </Markdown>
    </div>
  );
}
