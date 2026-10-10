import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { usePublishedElementHeight } from "./usePublishedElementHeight";

/**
 * r11 item 4: the published --session-dock-h must cover EVERY bottom surface.
 * An ended tty session mounts both the tty bottom chrome and the endedDock
 * (Resume); a single-element publish hid one behind the notify stack. The hook
 * now sums one or more observed elements.
 */
function box(height: number): HTMLDivElement {
  const el = document.createElement("div");
  Object.defineProperty(el, "offsetHeight", { value: height, configurable: true });
  document.body.appendChild(el);
  return el;
}

describe("usePublishedElementHeight", () => {
  afterEach(() => {
    document.documentElement.style.removeProperty("--session-dock-h");
    document.body.innerHTML = "";
  });

  it("publishes a single element's height", () => {
    const el = box(48);
    renderHook(() => usePublishedElementHeight(el, "--session-dock-h"));
    expect(document.documentElement.style.getPropertyValue("--session-dock-h")).toBe("48px");
  });

  it("sums multiple mounted bottom surfaces (tty chrome + endedDock)", () => {
    const ttyChrome = box(60);
    const endedDock = box(40);
    renderHook(() => usePublishedElementHeight([ttyChrome, endedDock], "--session-dock-h"));
    // The notify stack bottom is this value + its 12px (--space-3) gap, so it
    // clears the Resume button sitting beneath the tty input.
    expect(document.documentElement.style.getPropertyValue("--session-dock-h")).toBe("100px");
  });

  it("ignores null entries and clears the property with no elements", () => {
    const el = box(30);
    const { rerender, unmount } = renderHook(
      ({ els }: { els: Array<HTMLDivElement | null> }) => usePublishedElementHeight(els, "--session-dock-h"),
      { initialProps: { els: [el, null] } },
    );
    expect(document.documentElement.style.getPropertyValue("--session-dock-h")).toBe("30px");
    rerender({ els: [null, null] });
    expect(document.documentElement.style.getPropertyValue("--session-dock-h")).toBe("");
    unmount();
    expect(document.documentElement.style.getPropertyValue("--session-dock-h")).toBe("");
  });
});
