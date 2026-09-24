import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MathExpression } from "./MathBlock";

describe("MathExpression", () => {
  it("shows the TeX source as a neutral placeholder, then KaTeX output for inline math", async () => {
    const { container } = render(<MathExpression source={"a_b + c_d"} display={false} />);

    // First paint, before the lazy KaTeX chunk resolves: the source is there
    // (never blank) in the loading state.
    const loading = screen.getByTestId("math-loading");
    expect(loading.getAttribute("data-state")).toBe("loading");
    expect(loading.textContent).toBe("a_b + c_d");

    // KaTeX settles: real glyph span, html + MathML, underscores untouched.
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

  it("renders KaTeX output with currentColor (no hard-coded color on the markup)", async () => {
    render(<MathExpression source={"x^2"} display={false} />);
    await waitFor(() => expect(screen.getByTestId("math-inline")).toBeTruthy());
    const katex = screen.getByTestId("math-inline").querySelector(".katex") as HTMLElement;
    // KaTeX sets color: currentColor itself; neither the wrapper nor the
    // expression carries an inline colour (the stylesheet owns the theme).
    expect(katex.style.color === "" || katex.style.color === "currentColor").toBe(true);
  });

  it("shows the raw source in the danger role for a broken formula", async () => {
    render(<MathExpression source={"\\frac{"} display={true} />);
    const error = await screen.findByTestId("math-error");
    expect(error.textContent).toBe("\\frac{");
    expect(error.classList.toString()).toContain("error");
    // The message keeps rendering everything else; no KaTeX subtree here.
    expect(error.querySelector(".katex")).toBeNull();
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
});
