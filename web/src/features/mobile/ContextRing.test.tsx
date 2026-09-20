import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ContextRing } from "./ContextRing";

describe("ContextRing", () => {
  it("renders nothing at all when contextPct is null (no ring, no number)", () => {
    const { container } = render(<ContextRing pct={null} />);
    expect(container).toBeEmptyDOMElement();
    expect(screen.queryByTestId("context-ring")).toBeNull();
    expect(screen.queryByTestId("context-ring-pct")).toBeNull();
  });

  it("treats undefined and non-finite values as unknown", () => {
    const { container, rerender } = render(<ContextRing pct={undefined} />);
    expect(container).toBeEmptyDOMElement();
    rerender(<ContextRing pct={Number.NaN} />);
    expect(container).toBeEmptyDOMElement();
  });

  it("renders the 0 edge as a 0% ring, not as unknown", () => {
    render(<ContextRing pct={0} />);
    const ring = screen.getByTestId("context-ring");
    expect(ring).toHaveAttribute("data-pct", "0");
    expect(screen.getByTestId("context-ring-pct")).toHaveTextContent("0%");
    const arc = ring.querySelector("circle:last-of-type") as SVGCircleElement;
    expect(arc.getAttribute("stroke-dasharray")).toBe("0 100");
  });

  it("renders the 100 edge as a full 100% ring", () => {
    render(<ContextRing pct={100} />);
    const ring = screen.getByTestId("context-ring");
    expect(ring).toHaveAttribute("data-pct", "100");
    expect(screen.getByTestId("context-ring-pct")).toHaveTextContent("100%");
    const arc = ring.querySelector("circle:last-of-type") as SVGCircleElement;
    expect(arc.getAttribute("stroke-dasharray")).toBe("100 100");
  });

  it("clamps out-of-band percentages to the 0..100 arc instead of bending it", () => {
    const { rerender } = render(<ContextRing pct={142} />);
    expect(screen.getByTestId("context-ring")).toHaveAttribute("data-pct", "100");
    rerender(<ContextRing pct={-8} />);
    expect(screen.getByTestId("context-ring")).toHaveAttribute("data-pct", "0");
  });
});
