import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MathExpression } from "./MathBlock";
import {
  __setMathImporterForTest,
  getMathState,
  resetMathEngineForTest,
} from "../lib/mathRender";

beforeEach(() => {
  resetMathEngineForTest();
});

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
