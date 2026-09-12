import type { ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import ui from "../styles/ui.module.css";
import { Button } from "./Button";

function CodeBlock({ children }: { children?: ReactNode }) {
  const text = String(children ?? "");
  return (
    <div>
      <div className={ui.codeHead}>
        <Button
          variant="ghost"
          onClick={() => {
            void navigator.clipboard?.writeText(text);
          }}
        >
          复制
        </Button>
      </div>
      <pre>
        <code>{children}</code>
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
