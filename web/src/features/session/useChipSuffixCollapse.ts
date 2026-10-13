import { useLayoutEffect, useState } from "react";

/**
 * c-effortui r2 item 9: the 132 px-class phone trigger must keep the
 * permission word and the effort tier intact and drop ONLY the "· ultracode"
 * suffix (the ember sparks remain as the dot marker) when the full content
 * does not fit. Detection is measurement-based, not a breakpoint — the same
 * 390 px phone renders more or less text depending on the words.
 *
 * Two-pass measure: set data-measuring (the CSS force-shows the suffix while
 * measuring), compare the button's full content width (`scrollWidth`) against
 * its box (`clientWidth`), then report overflow. Re-runs on resize and
 * whenever the words change. In a layout-less environment (jsdom) both widths
 * are 0 so the hook reports `false`; tests stub the widths and fire resize.
 */
export function useChipSuffixCollapse(
  ref: React.RefObject<HTMLElement | null>,
  deps: readonly unknown[],
): boolean {
  const [collapsed, setCollapsed] = useState(false);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => {
      el.dataset.measuring = "1";
      const overflow = el.scrollWidth - el.clientWidth > 1;
      delete el.dataset.measuring;
      setCollapsed(overflow);
    };
    measure();
    let observer: ResizeObserver | undefined;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(measure);
      observer.observe(el);
    }
    window.addEventListener("resize", measure);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", measure);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return collapsed;
}
