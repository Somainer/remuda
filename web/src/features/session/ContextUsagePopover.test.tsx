import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ContextUsagePopover } from "./ContextUsagePopover";
import type { UsageRollup } from "./contextUsage";

// jsdom has no animation frames; the popover's 1 Hz clock only drives the
// relative last-turn wording, so a never-firing frame keeps tests stable.
afterEach(() => vi.unstubAllGlobals());

function stubRaf() {
  vi.stubGlobal("requestAnimationFrame", () => 1);
  vi.stubGlobal("cancelAnimationFrame", () => {});
}

const full: UsageRollup = {
  contextUsedTokens: 35_839,
  contextWindowTokens: 200_000,
  contextPct: 18,
  sessionInputTokens: 7_856,
  sessionOutputTokens: 589,
  cacheReadTokens: 97_704,
  cacheCreationTokens: 0,
  turns: 3,
  tpmIn60s: 1_223,
  tpmOut60s: 144,
  tpmIn5m: 612,
  tpmOut5m: 66,
  lastTurnAt: new Date(Date.now() - 30_000).toISOString(),
};

describe("ContextUsagePopover", () => {
  it("renders every rollup figure and the context bar percentage", () => {
    stubRaf();
    render(<ContextUsagePopover rollup={full} mobile={false} onClose={() => {}} />);
    expect(screen.getByTestId("context-usage-headline")).toHaveTextContent(
      "上下文 35.8k/200.0k (18%)",
    );
    expect(screen.getByTestId("context-usage-bar")).toHaveAttribute("data-pct", "18");
    expect(screen.getByTestId("context-usage-turns")).toHaveTextContent("3");
    // Session cells, compact form.
    expect(screen.getByTestId("context-usage-cell-入")).toHaveTextContent("7.9k");
    expect(screen.getByTestId("context-usage-cell-出")).toHaveTextContent("589");
    expect(screen.getByTestId("context-usage-cell-缓存读")).toHaveTextContent("97.7k");
    expect(screen.getByTestId("context-usage-cell-缓存写")).toHaveTextContent("0");
    // TPM rows (60 s window + 5 min average).
    const tpm = screen.getByText("TPM");
    expect(tpm).toBeInTheDocument();
    expect(screen.getByText("1.2k")).toBeInTheDocument();
    expect(screen.getByText("612")).toBeInTheDocument();
    // Last turn within the last minute renders 刚刚.
    expect(screen.getByText("最近一回合")).toBeInTheDocument();
  });

  it("renders unknown channels as em dash with the channel tooltip", () => {
    stubRaf();
    // Grok-like: output only.
    const partial: UsageRollup = {
      contextUsedTokens: null,
      contextWindowTokens: 128_000,
      contextPct: null,
      sessionInputTokens: null,
      sessionOutputTokens: 420,
      cacheReadTokens: null,
      cacheCreationTokens: null,
      turns: 1,
      tpmIn60s: null,
      tpmOut60s: 420,
      tpmIn5m: null,
      tpmOut5m: 84,
      lastTurnAt: null,
    };
    render(<ContextUsagePopover rollup={partial} mobile={false} onClose={() => {}} />);
    expect(screen.getByTestId("context-usage-headline")).toHaveTextContent("上下文 —/128.0k (—%)");
    expect(screen.getByTestId("context-usage-bar")).toHaveAttribute("data-pct", "0");
    const inputCell = screen.getByTestId("context-usage-cell-入");
    expect(inputCell).toHaveTextContent("—");
    // The dash carries the missing-channel explanation on hover.
    const dash = inputCell.querySelector("[title]") as HTMLElement;
    expect(dash.getAttribute("title")).toMatch(/inputTokens/);
  });

  it("becomes a fixed sheet on touch widths and closes via the affordance", async () => {
    stubRaf();
    const user = userEvent.setup();
    const onClose = vi.fn();
    const { rerender } = render(
      <ContextUsagePopover rollup={full} mobile={false} onClose={onClose} />,
    );
    const card = screen.getByTestId("context-usage-popover");
    expect(card).toHaveAttribute("data-mobile", "0");
    rerender(<ContextUsagePopover rollup={full} mobile onClose={onClose} />);
    expect(card).toHaveAttribute("data-mobile", "1");
    await user.click(screen.getByTestId("context-usage-close"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
