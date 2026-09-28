import { act, render, screen } from "@testing-library/react";
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

  it("shows 已恢复 once after an offline spell and removes it 1.5 s later", () => {
    vi.useFakeTimers();
    act(() => setConnection("offline"));
    render(<JournalBanner status="live" />);
    act(() => setConnection("recovering"));
    act(() => setConnection("live"));
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "restored");

    act(() => {
      vi.advanceTimersByTime(1500);
    });
    expect(screen.queryByTestId("journal-banner")).toBeNull();
    vi.useRealTimers();
  });

  it("does not show 已恢复 for a quiet-session stale→live watchdog flap", () => {
    vi.useFakeTimers();
    // The socket's only frame on a quiet session is its open snapshot; the
    // frame watchdog then flaps stale → reopen → live every window. That must
    // never (re)arm the recovery notice.
    render(<JournalBanner status="live" />);
    for (let i = 0; i < 3; i += 1) {
      setConnection("stale");
      act(() => {
        vi.advanceTimersByTime(15_000);
      });
      setConnection("recovering");
      setConnection("live");
      expect(screen.queryByTestId("journal-banner")).toBeNull();
    }
    vi.useRealTimers();
  });

  it("hides a showing 已恢复 notice immediately when the link drops again", () => {
    act(() => setConnection("offline"));
    render(<JournalBanner status="live" />);
    act(() => setConnection("live"));
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "restored");
    act(() => setConnection("offline"));
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "offline");
  });

  it("does not latch 已恢复 when a new mount leaves live within the 1.5 s window", () => {
    vi.useFakeTimers();
    act(() => setConnection("offline"));
    render(<JournalBanner status="live" />);
    act(() => setConnection("live"));
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "restored");

    // 500 ms in (timer still pending) a new mount goes recovering: the cleanup
    // kills the timer and must clear the notice instead of leaving it latched.
    act(() => {
      vi.advanceTimersByTime(500);
    });
    act(() => setConnection("recovering"));
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "recovering");

    // The link returns live WITHOUT another offline spell: no re-arm, and the
    // old notice must not reappear latched — the banner is quiet for good.
    act(() => setConnection("live"));
    expect(screen.queryByTestId("journal-banner")).toBeNull();
    act(() => {
      vi.advanceTimersByTime(5_000);
    });
    expect(screen.queryByTestId("journal-banner")).toBeNull();

    // A genuine new offline spell still re-arms the notice exactly once.
    act(() => setConnection("offline"));
    act(() => setConnection("live"));
    expect(screen.getByTestId("journal-banner")).toHaveAttribute("data-state", "restored");
    act(() => {
      vi.advanceTimersByTime(1500);
    });
    expect(screen.queryByTestId("journal-banner")).toBeNull();
    vi.useRealTimers();
  });
});
