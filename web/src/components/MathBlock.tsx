import {
  useCallback,
  useEffect,
  useRef,
  useState,
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

/** Faces that must be ready before the placeholder is allowed to swap. */
const REQUIRED_FONTS = ['16px "KaTeX_Main"', '16px "KaTeX_Math"'];

export function MathExpression({ source: tokenSource, display }: MathExpressionProps) {
  const engine = useSyncExternalStore(subscribeMath, getMathState);
  // The placeholder is removed only once the KaTeX faces are ready; a font
  // failure still proceeds (the @font-face uses font-display: swap), it never
  // swaps to invisible glyphs on a timer.
  const [fontsReady, setFontsReady] = useState(false);
  useEffect(() => {
    void loadMath();
  }, []);
  const source = restoreMathSource(tokenSource);

  useEffect(() => {
    if (engine.status !== "ready") {
      setFontsReady(false);
      return;
    }
    let cancelled = false;
    const fonts = typeof document !== "undefined" ? document.fonts : undefined;
    if (!fonts?.load) {
      setFontsReady(true);
      return;
    }
    // Resolve on either success or failure — never an invisible timed swap.
    void Promise.all(REQUIRED_FONTS.map((spec) => fonts.load(spec)))
      .catch(() => undefined)
      .then(() => {
        if (!cancelled) setFontsReady(true);
      });
    return () => {
      cancelled = true;
    };
  }, [engine.status]);

  const rootRef = useRef<HTMLElement | null>(null);
  const attach = useCallback((el: HTMLElement | null) => {
    rootRef.current = el;
  }, []);

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
    : `${css.placeholder} ${css.inlinePlaceholder}`;

  let body: ReactNode;
  if (engine.status === "ready" && fontsReady) {
    const result = renderMath(engine.katex, source, display);
    if (!result.ok) {
      const tooLarge = result.error === "too-large";
      body = (
        <span
          className={`${placeholderClass} ${tooLarge ? "" : css.error}`}
          data-testid={tooLarge ? "math-skip" : "math-error"}
          data-state={tooLarge ? "too-large" : "error"}
          title={tooLarge ? "公式过长，已按源码显示" : result.error}
          role="img"
          aria-label={
            tooLarge ? "数学公式过长，显示源码" : `数学公式解析失败：${result.error ?? ""}`
          }
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
      // Plain inline KaTeX: no height cap, so ordinary formulas sit on the
      // text baseline and explicitly tall ones (\dfrac/matrices) may grow the
      // line (round-3 ruling J).
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
    const state = engine.status === "error" ? "load-error" : "loading";
    body = (
      <span
        ref={display ? undefined : attach}
        className={placeholderClass}
        data-testid="math-loading"
        data-state={state}
        aria-label={engine.status === "error" ? "数学排版加载失败，将重试" : "数学公式排版中"}
      >
        {source}
      </span>
    );
  }

  return body;
}
