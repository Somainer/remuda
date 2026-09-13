import { useEffect, useState } from "react";

/** Copied algorithm from herdrx displayPreferences.ts; not imported. */
export const COMPACT_WORKBENCH_QUERY =
  "(max-width: 767px), (pointer: coarse) and (max-width: 1023px) and (max-height: 600px)";

/**
 * Whether the device is driven by touch rather than a mouse + hardware
 * keyboard. `COMPACT_WORKBENCH_QUERY` is a *layout* question and also matches
 * a narrow desktop window; features that trade the keyboard for an on-screen
 * dock must key off this instead, or a narrow window silently loses its mouse.
 */
export const COARSE_POINTER_QUERY = "(pointer: coarse) and (hover: none)";

export function useWorkbenchViewport() {
  const [mobile, setMobile] = useState(() =>
    typeof window === "undefined" ? false : window.matchMedia(COMPACT_WORKBENCH_QUERY).matches,
  );
  const [coarsePointer, setCoarsePointer] = useState(() =>
    typeof window === "undefined" ? false : window.matchMedia(COARSE_POINTER_QUERY).matches,
  );
  const [height, setHeight] = useState(() => (typeof window === "undefined" ? 800 : window.innerHeight));
  const [offsetTop, setOffsetTop] = useState(0);

  useEffect(() => {
    const media = window.matchMedia(COMPACT_WORKBENCH_QUERY);
    const coarse = window.matchMedia(COARSE_POINTER_QUERY);
    const apply = (nextHeight: number, nextOffset: number) => {
      document.documentElement.style.setProperty("--workbench-height", `${nextHeight}px`);
      setHeight(nextHeight);
      setOffsetTop(nextOffset);
    };
    const update = () => {
      setMobile(media.matches);
      setCoarsePointer(coarse.matches);
      const viewport = window.visualViewport;
      if (!viewport) {
        apply(window.innerHeight, 0);
        return;
      }
      if (viewport.scale !== 1) {
        apply(window.innerHeight, 0);
        return;
      }
      window.scrollTo(0, 0);
      apply(viewport.height, viewport.offsetTop || 0);
    };
    update();
    media.addEventListener("change", update);
    coarse.addEventListener("change", update);
    window.addEventListener("resize", update);
    window.visualViewport?.addEventListener("resize", update);
    window.visualViewport?.addEventListener("scroll", update);
    return () => {
      media.removeEventListener("change", update);
      coarse.removeEventListener("change", update);
      window.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("scroll", update);
      document.documentElement.style.removeProperty("--workbench-height");
    };
  }, []);

  return { mobile, coarsePointer, height, offsetTop };
}

export function composing(event: { nativeEvent: { isComposing?: boolean; keyCode?: number }; key: string }): boolean {
  return Boolean(event.nativeEvent.isComposing) || event.key === "Process" || event.nativeEvent.keyCode === 229;
}
