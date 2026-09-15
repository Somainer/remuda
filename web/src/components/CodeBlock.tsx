import { useEffect, useSyncExternalStore, useRef, useState } from "react";
import { Check, Copy, MessageSquareText, WrapText } from "lucide-react";
import { clipboardIo } from "../lib/clipboard";
import { parseFenceInfo, type CodeQuote } from "../lib/codeAnchors";
import { codeQuoteTargetSnapshot, quoteCode, subscribeQuoteTarget } from "../lib/codeQuoteBus";
import { HIGHLIGHT_LIMIT, highlightCode, languageLabel, resolveLanguage } from "../lib/highlight";
import { notify } from "../lib/notify";
import css from "./codeBlock.module.css";

/** Soft-wrap is one viewer-wide preference, namespaced with the other runtime prefs. */
const WRAP_KEY = "runtime.code-wrap";
/** How long the copy button stays on its ✓ state. */
const COPIED_MS = 1500;

function readWrap(): boolean {
  try {
    return localStorage.getItem(WRAP_KEY) === "1";
  } catch {
    return false;
  }
}

function writeWrap(value: boolean): void {
  try {
    localStorage.setItem(WRAP_KEY, value ? "1" : "0");
  } catch {
    /* storage unavailable; preference just does not persist */
  }
}

type CodeBlockProps = {
  /** Raw fence body, without the trailing newline the fence syntax adds. */
  code: string;
  /** Raw fence info string (language tag and any meta). */
  info?: string | null;
};

/**
 * When the viewer has selected text inside this block's `<pre>`, map that
 * selection to whole lines; otherwise return null and the caller quotes the
 * entire block. Character offsets come from Range over the pre's text node.
 */
function selectedLineRange(
  root: HTMLElement,
): { lineFrom: number; lineTo: number; text: string } | null {
  const selection = typeof window !== "undefined" ? window.getSelection() : null;
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const range = selection.getRangeAt(0);
  const pre = root.querySelector('[data-testid="code-pre"]');
  if (!pre || !pre.contains(range.startContainer) || !pre.contains(range.endContainer)) {
    return null;
  }
  // Offset of the range endpoints against the pre's full text content via a
  // measurement Range from the pre start.
  const measure = document.createRange();
  const firstText = firstTextNode(pre);
  if (!firstText) return null;
  measure.setStart(firstText, 0);
  const startOffset = offsetWithin(measure, pre, range.startContainer, range.startOffset);
  const endOffset = offsetWithin(measure, pre, range.endContainer, range.endOffset);
  if (startOffset === null || endOffset === null || startOffset === endOffset) return null;
  const full = pre.textContent ?? "";
  const start = Math.min(startOffset, endOffset);
  const end = Math.max(startOffset, endOffset);
  const selected = full.slice(start, end);
  if (!selected.trim()) return null;
  return {
    // lineRangeFromOffsets takes the raw block + whole-block offsets; here we
    // know the selected text itself, so compute lines directly.
    ...linesOf(full, start, end),
  };
}

function firstTextNode(root: Node): Node | null {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  return walker.nextNode();
}

/** Offset of (node, offsetInNode) measured in characters from `measure`'s start. */
function offsetWithin(
  measure: Range,
  scope: Node,
  node: Node,
  offsetInNode: number,
): number | null {
  if (!scope.contains(node)) return null;
  try {
    measure.setEnd(node, offsetInNode);
    return measure.toString().length;
  } catch {
    return null;
  }
}

function linesOf(full: string, start: number, end: number): {
  lineFrom: number;
  lineTo: number;
  text: string;
} {
  let lineFrom = 1;
  for (let i = 0; i < start && i < full.length; i += 1) {
    if (full[i] === "\n") lineFrom += 1;
  }
  let lineTo = lineFrom;
  for (let i = start; i < end - 1 && i < full.length; i += 1) {
    if (full[i] === "\n") lineTo += 1;
  }
  const lines = full.split("\n");
  return { lineFrom, lineTo, text: lines.slice(lineFrom - 1, lineTo).join("\n") };
}

/**
 * Fenced-code rendering: a header-less block with a floating icon toolbar
 * (soft-wrap toggle, copy) pinned to its top-right corner. Long lines scroll
 * inside the `<pre>`; the page around it never scrolls sideways.
 *
 * Highlighting is applied here (not in a rehype plugin) purely as
 * presentation after rehype-sanitize has done its pass: unhighlighted code is
 * always escaped React text, highlighted HTML is the escaped tokeniser output
 * from `highlightCode` inserted into a container this component owns.
 */
export function CodeBlock({ code, info = null }: CodeBlockProps) {
  const [wrap, setWrap] = useState(readWrap);
  const [copied, setCopied] = useState(false);
  const [html, setHtml] = useState<string | null>(null);
  const copyTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  // 评论 exists only inside a session transcript whose composer is mounted.
  const quotable = useSyncExternalStore(
    subscribeQuoteTarget,
    codeQuoteTargetSnapshot,
    () => false,
  );

  const label = languageLabel(info);
  const tooLarge = label !== "" && code.length >= HIGHLIGHT_LIMIT;

  useEffect(() => {
    let cancelled = false;
    void highlightCode(info, code).then((result) => {
      if (!cancelled) setHtml(result);
    });
    return () => {
      cancelled = true;
    };
  }, [info, code]);

  useEffect(() => {
    return () => {
      if (copyTimer.current) clearTimeout(copyTimer.current);
    };
  }, []);

  const toggleWrap = () => {
    setWrap((value) => {
      const next = !value;
      writeWrap(next);
      return next;
    });
  };

  const copy = async () => {
    try {
      await clipboardIo.write(code);
    } catch {
      notify({
        subject: "代码",
        stage: "复制失败",
        reason: "浏览器拒绝了剪贴板访问",
        severity: "blocking",
        key: "code-copy-failed",
      });
      return;
    }
    setCopied(true);
    if (copyTimer.current) clearTimeout(copyTimer.current);
    copyTimer.current = setTimeout(() => setCopied(false), COPIED_MS);
    // The C1 info channel: a short role=status announcement for screen
    // readers; the visual ✓ on the button itself is silent.
    notify({ subject: "代码", stage: "已复制", severity: "info", key: "code-copied" });
  };

  const wrapLabel = wrap ? "取消换行" : "换行";
  const copyLabel = copied ? "已复制" : "复制";

  // Quote the block (or the lines selected inside it) into the composer.
  // Identity is read from the DOM: which assistant message is this, and which
  // fenced block inside it. Batch C owns the transcript node model, so the
  // button derives position instead of taking a prop through MarkdownText.
  const comment = () => {
    const root = rootRef.current;
    if (!root) return;
    const message = root.closest('section[data-testid="message"]');
    const transcript = message?.parentElement;
    let turnOrdinal = 1;
    let blockOrdinal = 0;
    if (message && transcript) {
      const messages = Array.from(
        transcript.querySelectorAll('section[data-testid="message"]'),
      );
      const position = messages.indexOf(message as Element) + 1;
      turnOrdinal = position > 0 ? position : 1;
      blockOrdinal = Array.from(message.querySelectorAll('[data-testid="code-block"]')).indexOf(
        root,
      );
      if (blockOrdinal < 0) blockOrdinal = 0;
    }
    // A line selection inside the pre quotes just those lines; otherwise the
    // whole block.
    const range =
      selectedLineRange(root) ??
      (() => {
        const total = code.split("\n").length;
        return { lineFrom: 1, lineTo: total, text: code };
      })();
    const { lang, path } = parseFenceInfo(info);
    const payload: CodeQuote = {
      kind: "code",
      turnOrdinal,
      blockOrdinal,
      lang,
      ...(path ? { path } : {}),
      text: range.text,
      lineFrom: range.lineFrom,
      lineTo: range.lineTo,
    };
    quoteCode(payload);
  };

  return (
    <div ref={rootRef} className={css.root} data-testid="code-block" data-wrap={wrap ? "on" : "off"}>
      <div className={css.toolbar} data-testid="code-toolbar">
        {label ? (
          <span className={css.label} data-testid="code-lang">
            {label}
          </span>
        ) : null}
        {tooLarge ? (
          <span className={css.note} data-testid="code-note" title={`超过 ${HIGHLIGHT_LIMIT} 字符，已关闭高亮`}>
            未高亮·过长
          </span>
        ) : null}
        {quotable ? (
          <button
            type="button"
            className={css.button}
            data-testid="code-comment"
            aria-label="评论：把这段代码引用到输入框"
            title="评论"
            onClick={comment}
          >
            <MessageSquareText size={15} strokeWidth={2} aria-hidden="true" focusable="false" />
          </button>
        ) : null}
        <button
          type="button"
          className={css.button}
          data-testid="code-wrap"
          data-active={wrap}
          aria-pressed={wrap}
          aria-label={wrapLabel}
          title={wrapLabel}
          onClick={toggleWrap}
        >
          <WrapText size={15} strokeWidth={2} aria-hidden="true" focusable="false" />
        </button>
        <button
          type="button"
          className={css.button}
          data-testid="code-copy"
          data-copied={copied}
          aria-label={copyLabel}
          title={copyLabel}
          onClick={() => void copy()}
        >
          {copied ? (
            <Check size={15} strokeWidth={2} aria-hidden="true" focusable="false" />
          ) : (
            <Copy size={15} strokeWidth={2} aria-hidden="true" focusable="false" />
          )}
        </button>
      </div>
      <pre className={`${css.pre} ${wrap ? css.preWrap : ""}`} data-testid="code-pre">
        {html !== null ? (
          <code
            className={`language-${resolveLanguage(info) ?? ""}`.trim()}
            data-testid="code-code"
            dangerouslySetInnerHTML={{ __html: html }}
          />
        ) : (
          <code data-testid="code-code" className={info ? `language-${info.trim().split(/\s+/, 1)[0]!.toLowerCase()}` : undefined}>
            {code}
          </code>
        )}
      </pre>
    </div>
  );
}
