import { useLayoutEffect } from "react";

/**
 * Publish an element's measured height as a custom property on
 * documentElement (the common ancestor of portal/ sibling-rendered surfaces
 * such as the notify stack). A ResizeObserver keeps the value current; it is
 * removed while no element is observed and on teardown so other routes never
 * see a stale height.
 *
 * Used by SessionPage's structured dock and the tty view's bottom chrome
 * (local input dock + phone key bar), which both need the notify stack to
 * anchor above them.
 */
export function usePublishedElementHeight(el: HTMLElement | null, property: string): void {
  useLayoutEffect(() => {
    const root = document.documentElement;
    if (!el) {
      root.style.removeProperty(property);
      return;
    }
    const publish = () => {
      root.style.setProperty(property, `${el.offsetHeight}px`);
    };
    publish();
    let observer: ResizeObserver | undefined;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(publish);
      observer.observe(el);
    }
    return () => {
      observer?.disconnect();
      root.style.removeProperty(property);
    };
  }, [el, property]);
}
