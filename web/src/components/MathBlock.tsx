import {
  useCallback,
  useEffect,
  useRef,
  useSyncExternalStore,
  type ClipboardEvent,
  type ReactNode,
} from "react";
import { restoreMathSource } from "../lib/mathSegments";
import { getMathState, loadMath, renderMath, subscribeMath } from "../lib/mathRender";
import css from "./math.module.css";

export interface MathExpressionProps {
  /** TeX source as it reached the markdown node (dollar tokens restored). */
  source: string;
  display: boolean;
}

/**
 * One math node. KaTeX is a lazy chunk: until it loads, the TeX source shows
 * in a neutral style (never blank, never raw after paint). A parse error
 * shows the source in the danger role instead of crashing the message —
 * throwOnError is also false inside the engine.
 *
 * The rendered HTML is KaTeX-generated with trust:false; rehype-sanitize
 * never sees it (the markdown `code` override renders this component), and
 * no raw message HTML is involved.
 */
export function MathExpression({ source: tokenSource, display }: MathExpressionProps) {
  const engine = useSyncExternalStore(subscribeMath, getMathState);
  useEffect(() => {
    void loadMath();
  }, []);
  // The tokenizer hid real dollars from micromark behind MATH_DOLLAR; bring
  // them back for both the engine and the clipboard.
  const source = restoreMathSource(tokenSource);

  const rootRef = useRef<HTMLElement | null>(null);
  const attach = useCallback((el: HTMLElement | null) => {
    rootRef.current = el;
  }, []);

  // Copying a selection wholly inside the formula yields its TeX source; a
  // selection that reaches into surrounding prose keeps the browser default
  // (rendered text) so mixed copies are not replaced by the formula alone.
  const onCopy = useCallback(
    (event: ClipboardEvent<HTMLElement>) => {
      const root = rootRef.current;
      const selection = typeof window !== "undefined" ? window.getSelection() : null;
      if (!root || !selection || selection.isCollapsed || selection.rangeCount === 0) return;
      const range = selection.getRangeAt(0);
      if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) return;
      event.clipboardData.setData("text/plain", source);
      event.preventDefault();
    },
    [source],
  );

  const placeholderClass = display
    ? `${css.placeholder} ${css.displayPlaceholder}`
    : css.placeholder;

  let body: ReactNode;
  if (engine.status === "ready") {
    const result = renderMath(engine.katex, source, display);
    if (!result.ok) {
      body = (
        <span
          className={`${placeholderClass} ${css.error}`}
          data-testid="math-error"
          data-state="error"
          title={result.error}
          role="img"
          aria-label={`数学公式解析失败：${result.error ?? ""}`}
        >
          {source}
        </span>
      );
    } else if (display) {
      body = (
        <div
          ref={attach}
          className={css.displayScroll}
          data-testid="math-display"
          data-state="ready"
          onCopy={onCopy}
          dangerouslySetInnerHTML={{ __html: result.html }}
        />
      );
    } else {
      body = (
        <span
          ref={attach}
          className={css.inline}
          data-testid="math-inline"
          data-state="ready"
          onCopy={onCopy}
          dangerouslySetInnerHTML={{ __html: result.html }}
        />
      );
    }
  } else {
    // Loading: neutral TeX source. The engine caches no chunk-load failure,
    // so a later remount retries and swaps this placeholder for the formula.
    body = (
      <span
        ref={display ? undefined : attach}
        className={placeholderClass}
        data-testid="math-loading"
        data-state={engine.status === "error" ? "load-error" : "loading"}
      >
        {source}
      </span>
    );
  }

  return body;
}
