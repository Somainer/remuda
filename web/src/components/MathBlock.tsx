import {
  useCallback,
  useEffect,
  useLayoutEffect,
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
  const [wide, setWide] = useState(false);

  const renderedInline = engine.status === "ready" && fontsReady && !display;
  useLayoutEffect(() => {
    if (!renderedInline) return;
    const el = rootRef.current;
    if (!el) return;

    // Available width = the nearest containing BLOCK's content box. Inside
    // inline markdown (`**$x$**`, `*$x$*`, `[$x$](u)`) the immediate parent
    // is an inline element whose clientWidth is 0, so inline ancestors are
    // skipped. clientWidth already excludes borders; subtract horizontal
    // padding for the content width.
    let container: HTMLElement | null = el.parentElement;
    while (container && getComputedStyle(container).display === "inline") {
      container = container.parentElement;
    }
    const availableWidth = (): number => {
      if (!container) return el.clientWidth;
      const cs = getComputedStyle(container);
      return (
        container.clientWidth -
        (Number.parseFloat(cs.paddingLeft) || 0) -
        (Number.parseFloat(cs.paddingRight) || 0)
      );
    };

    // Promotion/demotion compares the formula's INTRINSIC width (inner
    // KaTeX node, flex 0 0 auto while promoted, so it never wraps) with the
    // current box and the available block width. scrollWidth alone cannot
    // drive demotion: it is clamped to clientWidth when content fits, so it
    // is never negative, and the box is shrink-to-fit and then matches the
    // formula.
    //   - clipped box: promote whenever ink is really clipped
    //     (scrollWidth > clientWidth, even a 1px KaTeX subscript bearing).
    //   - promoted box: demote only when NOTHING is scrollable any more AND
    //     the intrinsic formula fits the available width with 8px slack
    //     (column widened / source shortened). The scrollable check keeps a
    //     bearing formula promoted (demoting would clip those pixels); the
    //     slack is the column-boundary anti-flip band.
    const DEMOTE_SLACK = 8;
    const measure = (): void => {
      const katexEl = el.querySelector<HTMLElement>(".katex");
      const nodeWidth = katexEl ? katexEl.getBoundingClientRect().width : 0;
      const intrinsic = Math.max(el.scrollWidth, nodeWidth);
      const clipped = el.scrollWidth > el.clientWidth;
      setWide((prev) => (prev ? clipped || intrinsic > availableWidth() - DEMOTE_SLACK : clipped));
    };
    measure();
    // Fonts arriving late and viewport/column resizes change the widths.
    const Observer = globalThis.ResizeObserver;
    if (!Observer) return;
    const ro = new Observer(measure);
    ro.observe(el);
    // Once the wrapper fills the clamped box its own width stops changing,
    // so a later container-only resize (e.g. column 904 -> 1000 px) fires
    // no callback on `el` and the formula would stay promoted with slack;
    // observe the available-width block too.
    if (container) ro.observe(container);
    return () => ro.disconnect();
  }, [renderedInline, source]);
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
          className={`${css.inline}${wide ? ` ${css.inlineScroll}` : ""}`}
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
