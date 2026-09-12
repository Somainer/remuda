import { isValidElement, useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import { clipboardIo } from "../lib/clipboard";
import ui from "../styles/ui.module.css";
import { Button } from "./Button";

const COLLAPSE_LINES = 8;

function nodeText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join("");
  if (isValidElement(node)) return nodeText((node.props as { children?: ReactNode }).children);
  return "";
}

function CodeBlock({ children }: { children?: ReactNode }) {
  const text = nodeText(children);
  const lines = text.split("\n").length;
  const [expanded, setExpanded] = useState(false);
  const collapsible = lines > COLLAPSE_LINES;
  const collapsed = collapsible && !expanded;
  return (
    <div data-testid="code-block">
      <div className={ui.codeHead}>
        {collapsible ? (
          <Button variant="ghost" data-testid="code-expand" onClick={() => setExpanded((v) => !v)}>
            {expanded ? "收起" : "展开"}
          </Button>
        ) : null}
        <Button
          variant="ghost"
          data-testid="code-copy"
          onClick={() => {
            void clipboardIo.write(text);
          }}
        >
          复制
        </Button>
      </div>
      <pre data-testid="code-pre" style={collapsed ? { maxHeight: 160, overflow: "hidden" } : undefined}>
        {children}
      </pre>
    </div>
  );
}

/** react-markdown + remark-gfm + rehype-sanitize: GFM without raw HTML; Shiki deferred (WASM cost on PWA shell). */
export function MarkdownText({ text }: { text: string }) {
  return (
    <div className={ui.md}>
      <Markdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeSanitize]}
        components={{
          pre: ({ children }) => <CodeBlock>{children}</CodeBlock>,
        }}
      >
        {text}
      </Markdown>
    </div>
  );
}
