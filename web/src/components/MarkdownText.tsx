import { createContext, useContext, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeSanitize from "rehype-sanitize";
import ui from "../styles/ui.module.css";
import css from "./codeBlock.module.css";
import mentionCss from "./fileMention.module.css";
import { CodeBlock } from "./CodeBlock";
import { MathExpression } from "./MathBlock";
import { protectMath } from "../lib/mathSegments";

function nodeText(node: ReactNode): string {
  if (node == null || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(nodeText).join("");
  return "";
}

/**
 * The exact line the driver prepends to a harness prompt for a non-image
 * attachment (D-027b): `[File #n] <name> (<mime>, <size>) saved at <path>`.
 * It is quoted back in structured transcripts and renders collapsed.
 */
const FILE_MENTION_RE =
  /^\[File #(\d+)\] (.+) \(([^,()]+), ([^)]+)\) saved at (.+)$/;

export type FileMention = { index: number; name: string; mime: string; size: string; path: string };

/** Parse one expansion line, or null when it is not one. */
export function parseFileMention(line: string): FileMention | null {
  const match = FILE_MENTION_RE.exec(line.trim());
  if (!match) return null;
  return {
    index: Number(match[1]),
    name: match[2],
    mime: match[3],
    size: match[4],
    path: match[5],
  };
}

function FileMentionRow({ mention }: { mention: FileMention }) {
  return (
    <details className={mentionCss.mention} data-testid="file-mention" data-index={mention.index}>
      <summary className={mentionCss.summary}>
        <span aria-hidden>📎</span>
        <span className={mentionCss.name} title={mention.name}>
          {`[File #${mention.index}] ${mention.name}`}
        </span>
        <span className={mentionCss.meta}>{`${mention.mime} · ${mention.size}`}</span>
      </summary>
      <p className={mentionCss.path}>{`saved at ${mention.path}`}</p>
    </details>
  );
}

/**
 * Markdown renders a fenced block as `<pre><code>`, inline code as a bare
 * `<code>` with no `<pre>` ancestor. react-markdown v9 gives the code override
 * no `inline` flag, so the pre override marks its subtree: any code inside is
 * a fence.
 */
const FenceContext = createContext(false);

/**
 * Stable `pre` override. This must be a module-level component: react-markdown
 * renders overrides by their function identity, and an inline arrow rebuilt on
 * every Markdown render makes React remount the whole `<pre>` subtree (FencedCode
 * and CodeBlock included) on each transcript re-render. The remount restarted
 * the CodeBlock highlighter effect, detaching the block node mid-assertion in
 * the evidence screenshot loops ("Element is not attached to the DOM").
 *
 * No `<pre>` element is emitted: fenced code renders CodeBlock (which owns its
 * pre) and display math renders MathExpression.
 */
function FenceMarkdownPre({ children }: { children?: ReactNode }) {
  return <FenceContext.Provider value={true}>{children}</FenceContext.Provider>;
}

type CodeProps = {
  className?: string;
  children?: ReactNode;
  /** micromark keeps the fence meta (`a.ts` in ` ```ts a.ts `) on the hast node. */
  node?: { data?: { meta?: string | null } };
};

/** react-markdown's code override: inline stays a bare <code>, fences become CodeBlock. */
function FencedCode(rawProps: unknown) {
  const props = rawProps as CodeProps;
  const fenced = useContext(FenceContext);
  const isMath = /(?:^|\s)language-math(?:\s|$)/.test(props.className ?? "");
  if (isMath) {
    // mdast-util-math compiles flow math to `<pre><code class="language-math …">`
    // (fence context set) and inline math to bare `<code class="language-math">`.
    return <MathExpression source={nodeText(props.children)} display={fenced} />;
  }
  if (!fenced) {
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
 * Split text into markdown segments and collapsed D-027b file-mention lines.
 *
 * The driver's `[File #n] … saved at …` expansion can be quoted back inside a
 * structured message; it is a file reference, not prose the user typed, so it
 * renders as a collapsed row exactly like the other attachment anchors.
 */
function renderWithFileMentions(text: string): ReactNode {
  const lines = text.split("\n");
  const out: ReactNode[] = [];
  let markdown: string[] = [];
  const flush = (key: number) => {
    if (markdown.length === 0) return;
    out.push(
      <Markdown
        key={`md-${key}`}
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={[rehypeSanitize]}
        components={{
          pre: FenceMarkdownPre,
          code: FencedCode,
        }}
      >
        {protectMath(markdown.join("\n"))}
      </Markdown>,
    );
    markdown = [];
  };
  lines.forEach((line, index) => {
    const mention = parseFileMention(line);
    if (mention) {
      flush(index);
      out.push(<FileMentionRow key={`file-${index}`} mention={mention} />);
    } else {
      markdown.push(line);
    }
  });
  flush(lines.length);
  return out;
}

/**
 * react-markdown + remark-gfm + rehype-sanitize: GFM without raw HTML; code
 * colour is lazy on demand. Fenced code renders through CodeBlock (wrap /
 * copy / 评论 toolbar); inline code keeps the default element.
 */
export function MarkdownText({ text }: { text: string }) {
  return (
    <div className={`${ui.md} ${css.mdSurface}`}>{renderWithFileMentions(text)}</div>
  );
}
