import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { JournalBanner } from "./JournalBanner";
import { hubStore } from "../../lib/store";

function setConnection(connection: "live" | "stale" | "offline" | "recovering" | "reconnecting") {
  hubStore["emit"]({ connection });
}

describe("JournalBanner", () => {
  afterEach(() => setConnection("live"));

  it("hides when live", () => {
    setConnection("live");
    const { container } = render(<JournalBanner status="live" />);
    expect(container).toBeEmptyDOMElement();
  });

  it("renders recovering, gap, and readonly-stale copy", () => {
    // The journal's transient reconnecting label merges with the connection
    // machine into the single 正在恢复… banner.
    const { rerender } = render(<JournalBanner status="reconnecting" />);
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "recovering");
    expect(screen.getByTestId("journal-banner")).toHaveTextContent("正在恢复");
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

  it("renders the offline banner with the queued-message count, and nothing for stale", () => {
    setConnection("offline");
    hubStore["emit"]({ outboxPending: 2 });
    const { rerender, container } = render(<JournalBanner status="live" />);
    const banner = screen.getByTestId("journal-banner");
    expect(banner).toHaveAttribute("data-state", "offline");
    expect(banner.textContent).toContain("离线");
    expect(banner.textContent).toContain("恢复后自动发送");
    expect(banner.textContent).toContain("2 条待发");

    setConnection("stale");
    rerender(<JournalBanner status="live" />);
    // Stale is intentionally quiet (grey dot only, no banner).
    expect(container).toBeEmptyDOMElement();
  });
});
