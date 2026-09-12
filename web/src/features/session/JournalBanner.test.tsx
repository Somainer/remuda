import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { JournalBanner } from "./JournalBanner";

describe("JournalBanner", () => {
  it("hides when live", () => {
    const { container } = render(<JournalBanner status="live" />);
    expect(container).toBeEmptyDOMElement();
  });

  it("renders reconnecting, gap, and readonly-stale copy", () => {
    const { rerender } = render(<JournalBanner status="reconnecting" />);
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "reconnecting");
    expect(screen.getByTestId("journal-banner")).toHaveTextContent("不会自动重发");
    rerender(<JournalBanner status="gap-backfill" />);
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "gap-backfill");
    expect(screen.getByText(/正在补事件/)).toBeTruthy();
    const onRetry = vi.fn();
    rerender(<JournalBanner status="readonly-stale" onRetry={onRetry} />);
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "readonly-stale");
    expect(screen.getByText(/只读/)).toBeTruthy();
    screen.getByRole("button", { name: "重试" }).click();
    expect(onRetry).toHaveBeenCalled();
  });
});
