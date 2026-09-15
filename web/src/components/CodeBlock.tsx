import { useEffect, useRef, useState } from "react";
import { Check, Copy, WrapText } from "lucide-react";
import { clipboardIo } from "../lib/clipboard";
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

  return (
    <div className={css.root} data-testid="code-block" data-wrap={wrap ? "on" : "off"}>
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
