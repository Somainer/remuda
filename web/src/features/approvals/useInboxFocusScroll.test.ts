import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useInboxFocusScroll } from "./useInboxFocusScroll";

/**
 * c-inboxfu round 2, item 1: the desktop ?focus= effect must re-scroll as the
 * 进行中 · 最近 slice grows too — that tier renders ABOVE 已离队, so a
 * focused departed row otherwise scrolls in once and gets pushed below the
 * fold by later recent slices.
 */
describe("useInboxFocusScroll", () => {
  let scrollIntoView: ReturnType<typeof vi.fn>;
  let container: HTMLDivElement;

  beforeEach(() => {
    scrollIntoView = vi.fn();
    // jsdom does not implement scrollIntoView.
    Element.prototype.scrollIntoView = scrollIntoView as unknown as Element["scrollIntoView"];
    container = document.createElement("div");
    const row = document.createElement("article");
    row.dataset.interactionId = "itx_departed";
    container.appendChild(row);
    document.body.appendChild(container);
  });

  afterEach(() => {
    container.remove();
  });

  it("scrolls once the focused row exists", () => {
    const scrollRef = { current: container };
    renderHook(() =>
      useInboxFocusScroll("itx_departed", "itx_departed", scrollRef, [12, 12, 12]),
    );
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
  });

  it("re-scrolls when a later recent slice mounts above a focused departed row", () => {
    const scrollRef = { current: container };
    const { rerender } = renderHook(
      ({ limits }) => useInboxFocusScroll("itx_departed", "itx_departed", scrollRef, limits),
      { initialProps: { limits: [12, 12, 12] } },
    );
    expect(scrollIntoView).toHaveBeenCalledTimes(1);

    // The 进行中 · 最近 tier reveals its second rAF slice (12 -> 24); the
    // departed target shifts down and must be re-centered.
    rerender({ limits: [12, 24, 12] });
    expect(scrollIntoView).toHaveBeenCalledTimes(2);

    // A grown departed slice re-scrolls too (target itself mounts late).
    rerender({ limits: [12, 24, 24] });
    expect(scrollIntoView).toHaveBeenCalledTimes(3);
  });

  it("does not re-scroll when no limit actually changed", () => {
    const scrollRef = { current: container };
    const { rerender } = renderHook(
      ({ limits }) => useInboxFocusScroll("itx_departed", "itx_departed", scrollRef, limits),
      { initialProps: { limits: [12, 12, 12] } },
    );
    rerender({ limits: [12, 12, 12] });
    expect(scrollIntoView).toHaveBeenCalledTimes(1);
  });

  it("never scrolls without a focus target", () => {
    const scrollRef = { current: container };
    const { rerender } = renderHook(({ limits }) => useInboxFocusScroll(null, null, scrollRef, limits), {
      initialProps: { limits: [12, 12, 12] },
    });
    expect(scrollIntoView).not.toHaveBeenCalled();
    rerender({ limits: [12, 24, 12] });
    expect(scrollIntoView).not.toHaveBeenCalled();
  });
});
