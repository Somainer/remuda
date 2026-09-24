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

/** Faces whose readiness gates the placeholder→formula swap. */
const REQUIRED_FONTS = ['16px "KaTeX_Main"', '16px "KaTeX_Math"'];
/** Never stall forever on a slow font fetch; after this the formula shows. */
const FONT_GRACE_MS = 2000;

export function MathExpression({ source: tokenSource, display }: MathExpressionProps) {
  const engine = useSyncExternalStore(subscribeMath, getMathState);
  const [fontsReady, setFontsReady] = useState(false);
  useEffect(() => {
    void loadMath();
  }, []);
  // The tokenizer hid real dollars from micromark behind MATH_DOLLAR; bring
  // them back for both the engine and the clipboard.
  const source = restoreMathSource(tokenSource);

  // Once the engine (and its @font-face declarations) is present, keep the
  // visible source placeholder until the main KaTeX faces are actually ready,
  // so the swap never paints an invisible formula (FOIT). A short grace
  // timeout covers environments without the Font Loading API.
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
    let timer: ReturnType<typeof setTimeout> | undefined;
    Promise.race([
      Promise.all(REQUIRED_FONTS.map((spec) => fonts.load(spec))).then(
        () => fonts.ready,
      ),
      new Promise<void>((resolve) => {
        timer = setTimeout(resolve, FONT_GRACE_MS);
      }),
    ]).then(() => {
      if (!cancelled) setFontsReady(true);
    });
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [engine.status]);

  const rootRef = useRef<HTMLElement | null>(null);
  const attach = useCallback((el: HTMLElement | null) => {
    rootRef.current = el;
  }, []);

  // Copying a selection wholly inside the formula yields its TeX source; a
  // selection that reaches into surrounding prose keeps the browser default.
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
  const engineReady = engine.status === "ready";

  let body: ReactNode;
  if (engineReady && fontsReady) {
    const result = renderMath(engine.katex, source, display);
    if (!result.ok) {
      // Parse error, oversized source, or engine throw: raw source, danger
      // role (or neutral for the size cap), message still intact.
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
    // Chunk loading, chunk failed (a later mount retries), or faces not yet
    // ready: the TeX source stays visibly in a neutral style.
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
