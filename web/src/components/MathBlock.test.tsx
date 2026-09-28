import { render, screen, waitFor } from "@testing-library/react";
import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MathExpression } from "./MathBlock";
import {
  __setMathImporterForTest,
  getMathState,
  resetMathEngineForTest,
} from "../lib/mathRender";

beforeEach(() => {
  resetMathEngineForTest();
});

/**
 * jsdom has no ResizeObserver and does zero layout; the promotion logic is
 * driven by an observer callback, so install one we can fire by hand and
 * stub the inner-KaTeX intrinsic width and the box clientWidth per step.
 */
class ResizeObserverMock {
  private callback: ResizeObserverCallback;
  readonly observed: Element[] = [];
  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
  }
  observe(target: Element): void {
    if (!this.observed.includes(target)) this.observed.push(target);
  }
  unobserve(target: Element): void {
    const i = this.observed.indexOf(target);
    if (i >= 0) this.observed.splice(i, 1);
  }
  disconnect(): void {
    this.observed.splice(0);
  }
  /** Deliver one entry for `target`; a target that is not observed is a no-op. */
  trigger(target?: Element): void {
    if (target && !this.observed.includes(target)) return;
    const targets = target ? [target] : [...this.observed];
    this.callback(
      targets.map((t) => ({ target: t })) as ResizeObserverEntry[],
      this as unknown as ResizeObserver,
    );
  }
}
let observers: ResizeObserverMock[] = [];

beforeEach(() => {
  observers = [];
  vi.stubGlobal(
    "ResizeObserver",
    class extends ResizeObserverMock {
      constructor(callback: ResizeObserverCallback) {
        super(callback);
        observers.push(this);
      }
    },
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

/**
 * Stub one physically possible geometry (all real numbers a shrink-to-fit
 * inline box / its inner KaTeX node / its nearest block container can have):
 *  - scrollW: content the box can scroll (or clip), including bearings
 *  - nodeW:   the inner .katex border-box width
 *  - clientW: the box's current content width (clamped to the block when
 *             the formula is wider, else shrink-to-fit ≈ the formula)
 *  - blockW:  nearest non-inline ancestor's content width (its clientWidth
 *             minus padL/padR); inline ancestors keep clientWidth 0
 */
const mockGeom = (
  el: Element,
  g: { scrollW: number; nodeW: number; clientW: number; blockW: number; padL?: number; padR?: number },
): void => {
  Object.defineProperty(el, "scrollWidth", { configurable: true, get: () => g.scrollW });
  Object.defineProperty(el, "clientWidth", { configurable: true, get: () => g.clientW });
  const katexEl = el.querySelector(".katex");
  if (katexEl) {
    katexEl.getBoundingClientRect = () =>
      ({
        width: g.nodeW, x: 0, y: 0, top: 0, left: 0,
        right: g.nodeW, bottom: 0, height: 0, toJSON() {},
      }) as DOMRect;
  }
  // Same ancestor walk as the component: skip inline wrappers (strong/em/a).
  let block: Element | null = el.parentElement;
  while (block && getComputedStyle(block).display === "inline") block = block.parentElement;
  if (block) {
    block.setAttribute("style", `padding-left:${g.padL ?? 0}px;padding-right:${g.padR ?? 0}px`);
    Object.defineProperty(block, "clientWidth", {
      configurable: true,
      get: () => g.blockW + (g.padL ?? 0) + (g.padR ?? 0),
    });
  }
};

describe("MathExpression", () => {
  it("shows the TeX source as a neutral placeholder, then KaTeX output for inline math", async () => {
    const { container } = render(<MathExpression source={"a_b + c_d"} display={false} />);

    const loading = screen.getByTestId("math-loading");
    expect(loading.getAttribute("data-state")).toBe("loading");
    expect(loading.textContent).toBe("a_b + c_d");

    await waitFor(() => expect(screen.getByTestId("math-inline")).toBeTruthy());
    const inline = screen.getByTestId("math-inline");
    expect(inline.getAttribute("data-state")).toBe("ready");
    expect(inline.querySelector(".katex")).toBeTruthy();
    expect(inline.querySelector(".katex-mathml")).toBeTruthy();
    expect(container.textContent).toContain("a_b + c_d");
  });

  it("renders display math in its own block", async () => {
    render(
      <MathExpression
        source={"\\sigma(\\mathbf{z})_i = \\frac{e^{z_i}}{\\sum_{j=1}^{K} e^{z_j}}"}
        display={true}
      />,
    );
    await waitFor(() => expect(screen.getByTestId("math-display")).toBeTruthy());
    const block = screen.getByTestId("math-display");
    expect(block.tagName).toBe("DIV");
    expect(block.querySelector(".katex-display")).toBeTruthy();
    expect(block.querySelector(".mfrac")).toBeTruthy();
  });

  it("renders KaTeX output with currentColor (no hard-coded inline colour)", async () => {
    render(<MathExpression source={"x^2"} display={false} />);
    await waitFor(() => expect(screen.getByTestId("math-inline")).toBeTruthy());
    const katex = screen.getByTestId("math-inline").querySelector(".katex") as HTMLElement;
    expect(katex.style.color === "" || katex.style.color === "currentColor").toBe(true);
  });

  it("shows the raw source in the danger role for a broken formula", async () => {
    render(<MathExpression source={"\\frac{"} display={true} />);
    const error = await screen.findByTestId("math-error");
    expect(error.textContent).toBe("\\frac{");
    expect(error.classList.toString()).toContain("error");
    expect(error.querySelector(".katex")).toBeNull();
  });

  it("shows raw source in a neutral style (not danger) when over the size cap", async () => {
    const big = "x".repeat(5000);
    render(<MathExpression source={big} display={true} />);
    const skip = await screen.findByTestId("math-skip");
    expect(skip.textContent).toBe(big);
    expect(skip.classList.toString()).not.toContain("error");
    expect(screen.queryByTestId("math-display")).toBeNull();
  });

  it("renders a tall nested fraction inline without a height cap (round-3 J)", async () => {
    render(<MathExpression source={"\\dfrac{1}{\\dfrac{1}{x}}"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    // Explicitly tall inline formulas are allowed to grow their line: the
    // node is a plain inline `.inline` (no max-height cap). The ordinary-formula
    // paragraph-line geometry is asserted in e2e (jsdom does no layout).
    expect(inline.className).toMatch(/inline/);
    expect(inline.querySelector(".mfrac")).toBeTruthy();
  });

  it("copies the TeX source when the selection is wholly inside the formula", async () => {
    render(<MathExpression source={"E = mc^2"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    const range = document.createRange();
    range.selectNodeContents(inline);
    const selection = window.getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);

    const event = new Event("copy", { bubbles: true, cancelable: true }) as Event & {
      clipboardData: { setData: ReturnType<typeof vi.fn> };
    };
    event.clipboardData = { setData: vi.fn() };
    inline.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
    expect(event.clipboardData.setData).toHaveBeenCalledWith("text/plain", "E = mc^2");
  });

  it("does not hijack a copy that reaches outside the formula", async () => {
    render(
      <div>
        <MathExpression source={"x"} display={false} />
        <span data-testid="outside">prose</span>
      </div>,
    );
    const inline = await screen.findByTestId("math-inline");
    const outside = screen.getByTestId("outside");
    const range = document.createRange();
    range.setStartBefore(inline);
    range.setEndAfter(outside);
    const selection = window.getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);

    const event = new Event("copy", { bubbles: true, cancelable: true }) as Event & {
      clipboardData: { setData: ReturnType<typeof vi.fn> };
    };
    event.clipboardData = { setData: vi.fn() };
    inline.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(false);
    expect(event.clipboardData.setData).not.toHaveBeenCalled();
  });

  it("promotes on exactly 1px of clipped ink and stays stable (c-mathfu r3.3)", async () => {
    render(<MathExpression source={"\\sum_i x_i"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    const fire = (g: Parameters<typeof mockGeom>[1]): void => {
      mockGeom(inline, g);
      act(() => observers[observers.length - 1]!.trigger());
    };
    const promoted = (): boolean => inline.className.includes("inlineScroll");
    expect(promoted()).toBe(false);
    // Exactly one pixel of real ink beyond the shrink-to-fit box.
    fire({ scrollW: 35, nodeW: 34, clientW: 34, blockW: 700 });
    expect(promoted()).toBe(true);
    // Repeated measurements with the same 1px geometry must stay promoted.
    fire({ scrollW: 35, nodeW: 34, clientW: 34, blockW: 700 });
    expect(promoted()).toBe(true);
    fire({ scrollW: 35, nodeW: 34, clientW: 34, blockW: 700 });
    expect(promoted()).toBe(true);
  });

  it("promotes on 2px KaTeX bearings and never flips at the block-width boundary (c-mathfu 2/r2)", async () => {
    render(<MathExpression source={"\\sum_i x_i"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    expect(observers).toHaveLength(1);
    const fire = (g: Parameters<typeof mockGeom>[1]): void => {
      mockGeom(inline, g);
      act(() => observers[observers.length - 1]!.trigger());
    };
    const promoted = (): boolean => inline.className.includes("inlineScroll");

    // KaTeX bearings make scrollWidth 36 in a 34px shrink-to-fit box: 2px of
    // real ink is clipped, so it promotes even though the formula fits the
    // 700px column with huge room.
    expect(promoted()).toBe(false);
    fire({ scrollW: 36, nodeW: 34, clientW: 34, blockW: 700 });
    expect(promoted()).toBe(true);

    // While promoted the bearings remain scrollable (scrollW 36 > 34): the
    // box must NOT demote back to clip, and repeated callbacks settle.
    fire({ scrollW: 36, nodeW: 34, clientW: 34, blockW: 700 });
    expect(promoted()).toBe(true);
    fire({ scrollW: 36, nodeW: 34, clientW: 34, blockW: 700 });
    expect(promoted()).toBe(true);

    // Boundary band: a clean formula that fills the available width to
    // within 8px stays promoted (no sub-pixel flip); with 8px real slack
    // and nothing scrollable it demotes, and does not re-promote.
    fire({ scrollW: 696, nodeW: 696, clientW: 696, blockW: 700 });
    expect(promoted()).toBe(true);
    fire({ scrollW: 693, nodeW: 693, clientW: 693, blockW: 700 });
    expect(promoted()).toBe(true);
    fire({ scrollW: 692, nodeW: 692, clientW: 692, blockW: 700 });
    expect(promoted()).toBe(false);
    // Same geometry measured from the demoted box stays demoted.
    fire({ scrollW: 692, nodeW: 692, clientW: 692, blockW: 700 });
    expect(promoted()).toBe(false);
  });

  it("demotes when the block widens (narrow -> wide resize) (c-mathfu r2.2)", async () => {
    render(<MathExpression source={"x_1+...+x_20"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    const fire = (g: Parameters<typeof mockGeom>[1]): void => {
      mockGeom(inline, g);
      act(() => observers[observers.length - 1]!.trigger());
    };
    // Narrow column: 900px formula clamped to a 180px shrink-to-fit box.
    fire({ scrollW: 900, nodeW: 900, clientW: 180, blockW: 180 });
    expect(inline.className).toMatch(/inlineScroll/);
    // Column widens to 1000: promoted box re-lays out shrink-to-fit at
    // 900, nothing scrollable, 900 <= 1000-8 -> demote.
    fire({ scrollW: 900, nodeW: 900, clientW: 900, blockW: 1000 });
    expect(inline.className).not.toMatch(/inlineScroll/);
    // Shrinking the column again re-promotes on real overflow.
    fire({ scrollW: 900, nodeW: 900, clientW: 180, blockW: 180 });
    expect(inline.className).toMatch(/inlineScroll/);
  });

  it("demotes when the source gets shorter (long -> short) (c-mathfu r2.2)", async () => {
    const { rerender } = render(<MathExpression source={"x_1+...+x_20"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    const fire = (g: Parameters<typeof mockGeom>[1]): void => {
      mockGeom(inline, g);
      act(() => observers[observers.length - 1]!.trigger());
    };
    fire({ scrollW: 900, nodeW: 900, clientW: 180, blockW: 700 });
    expect(inline.className).toMatch(/inlineScroll/);
    // Streaming edit replaces the formula with a 40px one: the box
    // shrink-to-fits, nothing scrolls, it fits the line with slack.
    rerender(<MathExpression source={"x"} display={false} />);
    const inline2 = await screen.findByTestId("math-inline");
    fire({ scrollW: 40, nodeW: 40, clientW: 40, blockW: 700 });
    expect(inline2.className).not.toMatch(/inlineScroll/);
  });

  it("demotes through inline bold/em/link ancestors (their clientWidth is 0) (c-mathfu r3.1)", async () => {
    const Nested = ({ expr }: { expr: string }) => (
      <p data-testid="para">
        <strong>
          <em>
            <a href="#">
              <MathExpression source={expr} display={false} />
            </a>
          </em>
        </strong>
      </p>
    );
    const { rerender } = render(<Nested expr={"x_1+...+x_20"} />);
    const inline = await screen.findByTestId("math-inline");
    // The immediate parents really are inline boxes of width 0.
    expect(getComputedStyle(inline.parentElement!).display).toBe("inline");
    expect(inline.parentElement!.clientWidth).toBe(0);
    const block = screen.getByTestId("para");
    const ro = (): ResizeObserverMock => observers[observers.length - 1]!;
    // `fire` installs geometry then delivers an entry for ONE element:
    // "el" = wrapper measured, "block" = the containing block only.
    const fire = (which: "el" | "block", g: Parameters<typeof mockGeom>[1]): void => {
      mockGeom(inline, g);
      act(() => ro().trigger(which === "el" ? inline : block));
    };
    const promoted = (): boolean => inline.className.includes("inlineScroll");

    // Narrow block: promoted even though every ancestor up to <p> is inline.
    fire("el", { scrollW: 900, nodeW: 900, clientW: 180, blockW: 180 });
    expect(promoted()).toBe(true);

    // Padded block, content width 200 (224 client - 12px padding each side).
    // A 195px formula stays inside the 8px band of the CONTENT width (if
    // padding were not subtracted, 195 <= 224-8 would wrongly demote).
    fire("el", { scrollW: 195, nodeW: 195, clientW: 195, blockW: 200, padL: 12, padR: 12 });
    expect(promoted()).toBe(true);
    // With real content-width slack it demotes.
    fire("block", { scrollW: 190, nodeW: 190, clientW: 190, blockW: 200, padL: 12, padR: 12 });
    expect(promoted()).toBe(false);

    // Intrinsic width stays 900; ONLY the nested container widens
    // (180 -> 1000). Get promoted again, then deliver the BLOCK's entry
    // alone — the wrapper geometry is unchanged — and it demotes.
    fire("el", { scrollW: 900, nodeW: 900, clientW: 180, blockW: 180 });
    expect(promoted()).toBe(true);
    fire("block", { scrollW: 900, nodeW: 900, clientW: 900, blockW: 1000 });
    expect(promoted()).toBe(false);

    // Long -> short source through the same inline ancestry: get back into
    // the promoted state and prove it is promoted IMMEDIATELY before the
    // rerender (otherwise the later demote assertion would be vacuous).
    fire("el", { scrollW: 900, nodeW: 900, clientW: 180, blockW: 180 });
    expect(promoted()).toBe(true);
    // The narrow geometry stays installed across the streaming rerender;
    // the source change replaces only the inner KaTeX node.
    rerender(<Nested expr={"x"} />);
    const inline2 = await screen.findByTestId("math-inline");
    expect(inline2.className).toMatch(/inlineScroll/);
    // Measuring the now-short formula in the wide block demotes it.
    act(() => {
      mockGeom(inline2, { scrollW: 40, nodeW: 40, clientW: 40, blockW: 700 });
      observers[observers.length - 1]!.trigger(block);
    });
    expect(inline2.className).not.toMatch(/inlineScroll/);
  });

  it("observes the containing block and demotes on a container-only resize (c-mathfu r3.2)", async () => {
    const { unmount } = render(<MathExpression source={"x_1+...+x_20"} display={false} />);
    const inline = await screen.findByTestId("math-inline");
    const ro = observers[observers.length - 1]!;
    const block = inline.parentElement!;
    // Both the wrapper and its available-width block are observed.
    expect(ro.observed).toContain(inline);
    expect(ro.observed).toContain(block);
    // Deliver a real per-target entry for exactly one observed element.
    const fire = (which: "el" | "block", g: Parameters<typeof mockGeom>[1]): void => {
      mockGeom(inline, g);
      act(() => ro.trigger(which === "el" ? inline : block));
    };

    // Promotes with 899px available (1px clipped) — wrapper entry only.
    fire("el", { scrollW: 900, nodeW: 900, clientW: 899, blockW: 899 });
    expect(inline.className).toMatch(/inlineScroll/);
    // Block widens to 904: wrapper shrink-to-fits to 900 and nothing scrolls,
    // but 900 > 904-8, so it stays promoted (first stage changes the wrapper).
    fire("el", { scrollW: 900, nodeW: 900, clientW: 900, blockW: 904 });
    expect(inline.className).toMatch(/inlineScroll/);
    // SECOND STAGE changes only the container: the wrapper geometry is
    // untouched (still 900/900), only the block's clientWidth goes to 1000,
    // and the entry delivered is the BLOCK's alone. The 100px slack demotes.
    act(() => {
      Object.defineProperty(block, "clientWidth", { configurable: true, get: () => 1000 });
      ro.trigger(block);
    });
    expect(inline.className).not.toMatch(/inlineScroll/);

    // An entry for an element that is NOT observed must be a no-op (this is
    // what makes the container observation real, not a shared callback).
    act(() => ro.trigger(document.createElement("div")));

    // Cleanup disconnects both observations.
    unmount();
    expect(ro.observed).toHaveLength(0);
  });

  it("renders a SECOND mounted component after the first import rejects (K)", async () => {
    resetMathEngineForTest();
    let attempts = 0;
    __setMathImporterForTest(() => {
      attempts += 1;
      return attempts === 1
        ? Promise.reject(new Error("chunk fetch failed"))
        : Promise.resolve({
            default: {
              renderToString: () => '<span class="katex"><span>r2</span></span>',
            },
          });
    });

    // First mounted component: import rejects, it shows its raw source (load
    // error state), never KaTeX.
    const first = render(<MathExpression source={"fail^2"} display={false} />);
    await waitFor(() => expect(getMathState().status).toBe("error"));
    expect(screen.queryByTestId("math-inline")).toBeNull();
    expect(first.getByTestId("math-loading").textContent).toBe("fail^2");
    expect(attempts).toBe(1);

    // A SECOND component mounts later: loadMath() must retry a fresh import
    // and this one renders KaTeX.
    first.unmount();
    render(<MathExpression source={"ok^2"} display={false} />);
    await waitFor(() => expect(getMathState().status).toBe("ready"));
    const inline = await screen.findByTestId("math-inline");
    expect(inline.textContent).toContain("r2");
    expect(attempts).toBe(2);

    resetMathEngineForTest();
  });
});
