import { useLayoutEffect } from "react";

/**
 * Publish measured bottom-chrome height(s) as a custom property on
 * documentElement (the common ancestor of portal/sibling-rendered surfaces
 * such as the notify stack). When several elements are passed their offset
 * heights are SUMMED — an ended tty session mounts both the tty bottom chrome
 * (TerminalView's local input dock + phone key bar) AND the endedDock with its
 * Resume button, and the notify stack must clear the stack of both. A
 * ResizeObserver keeps the value current; the property is removed while no
 * element is observed and on teardown so other routes never see a stale
 * height.
 *
 * Used by SessionPage's structured dock, the tty view's bottom chrome, and the
 * ended-events endedDock.
 */
export function usePublishedElementHeight(
  els: HTMLElement | null | ReadonlyArray<HTMLElement | null>,
  property: string,
): void {
  useLayoutEffect(() => {
    const list = (Array.isArray(els) ? els : [els]).filter((el): el is HTMLElement => el !== null);
    const root = document.documentElement;
    if (list.length === 0) {
      root.style.removeProperty(property);
      return;
    }
    const publish = () => {
      const total = list.reduce((sum, el) => sum + el.offsetHeight, 0);
      root.style.setProperty(property, `${total}px`);
    };
    publish();
    let observer: ResizeObserver | undefined;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(publish);
      list.forEach((el) => observer!.observe(el));
    }
    return () => {
      observer?.disconnect();
      root.style.removeProperty(property);
    };
  }, [els, property]);
}
