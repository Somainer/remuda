import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { EffortSlider } from "./EffortSlider";

type SliderProps = Parameters<typeof EffortSlider>[0];

function mount(overrides: Partial<SliderProps> & Pick<SliderProps, "kind">) {
  return render(<EffortSlider index={2} onChange={vi.fn()} {...overrides} />);
}

describe("EffortSlider visual ladder", () => {
  it("Claude low/medium/high are plain: cold fill, no ember field", () => {
    for (const index of [0, 1, 2]) {
      const { unmount } = mount({ kind: "claude", index });
      const slider = screen.getByTestId("effort-slider");
      expect(slider).toHaveAttribute("data-effort-look", "plain");
      expect(slider).toHaveAttribute("data-ember", "0");
      expect(screen.queryByTestId("effort-embers")).toBeNull();
      unmount();
    }
  });

  it("Claude xhigh and max carry the restrained top accent without the ember", () => {
    for (const index of [3, 4]) {
      const { unmount } = mount({ kind: "claude", index });
      const slider = screen.getByTestId("effort-slider");
      expect(slider).toHaveAttribute("data-effort-look", "top");
      // The animated ember field exists at neither top tier.
      expect(screen.queryByTestId("effort-embers")).toBeNull();
      expect(slider).toHaveAttribute("data-ember", "0");
      unmount();
    }
  });

  it("the Claude ultracode stop gets the strongest look: embers, its own label and thumb", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    mount({ kind: "claude", index: 3, ultracode: true, onChange });
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-effort-look", "ultracode");
    expect(slider).toHaveAttribute("data-ember", "1");
    expect(slider).toHaveAttribute("data-name", "ultracode");
    // Full field: glow + three spark layers + the dotted layer.
    const embers = screen.getByTestId("effort-embers");
    expect(embers).toHaveAttribute("data-intensity", "ultra");
    expect(embers.querySelectorAll("span")).toHaveLength(5);
    // Its own title colour, distinct from the top-tier dust.
    const title = screen.getByTestId("effort-title");
    expect(title).toHaveAttribute("data-effort-look", "ultracode");
    // Stepping one stop left drops to the restrained `max` look, visibly.
    slider.focus();
    await user.keyboard("{ArrowLeft}");
    expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ name: "max", ultracode: false }));
  });

  it("walks plain → top → ultracode across the rightmost stops", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    mount({ kind: "claude", index: 2, onChange });
    const slider = screen.getByTestId("effort-slider");
    slider.focus();
    await user.keyboard("{ArrowRight}"); // xhigh: top
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ name: "xhigh", ultracode: false }),
    );
    await user.keyboard("{ArrowRight}"); // max: still top, no ember
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ name: "max", ultracode: false }),
    );
    await user.keyboard("{ArrowRight}"); // ultracode: strongest
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ name: "xhigh", ultracode: true }),
    );
  });

  it("Codex enumerates six native stops and Max matches Claude's restrained accent", () => {
    mount({ kind: "codex", index: 4 });
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute(
      "data-tiers",
      "low,medium,high,xhigh,max,ultra",
    );
    expect(slider).toHaveAttribute("data-effort-look", "top");
    expect(slider).toHaveAttribute("data-ember", "0");
    expect(screen.queryByTestId("effort-embers")).toBeNull();
    expect(screen.getByTestId("effort-title")).toHaveTextContent("Max");
    expect(slider).toHaveAttribute("aria-valuetext", "Max");
  });

  it("Codex Low through Extra high are plain and the flag is never ultracode", () => {
    for (const index of [0, 1, 2, 3]) {
      const { unmount } = mount({ kind: "codex", index });
      const slider = screen.getByTestId("effort-slider");
      expect(slider).toHaveAttribute("data-effort-look", "plain");
      expect(slider).toHaveAttribute("data-ultracode", "0");
      unmount();
    }
  });

  it("Codex Ultra gets the strongest field while remaining a native tier without the workflow flag", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    mount({ kind: "codex", index: 5, onChange });
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-name", "ultra");
    expect(slider).toHaveAttribute("aria-valuetext", "Ultra");
    expect(slider).toHaveAttribute("data-effort-look", "ultracode");
    expect(slider).toHaveAttribute("data-ultracode", "0");
    expect(slider).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-embers").querySelectorAll("span")).toHaveLength(5);
    expect(screen.getByTestId("effort-title")).toHaveTextContent("Ultra");
    expect(screen.getByTestId("effort-model")).toHaveTextContent(
      "For demanding work using multiple agents · highest usage",
    );
    slider.focus();
    await user.keyboard("{ArrowLeft}");
    expect(onChange).toHaveBeenLastCalledWith({ kind: "codex", name: "max", index: 4 });
    expect(slider).toHaveAttribute("data-effort-look", "top");
    await user.keyboard("{ArrowRight}");
    expect(slider).toHaveAttribute("data-name", "ultra");
    expect(slider).toHaveAttribute("data-effort-look", "ultracode");
    expect(slider).toHaveAttribute("data-ultracode", "0");
  });

  it("lists all six exact Codex labels and descriptions, preserving the native values on selection", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    mount({ kind: "codex", index: 1, onChange });
    await user.click(screen.getByTestId("effort-open-list"));
    const tiers = [
      ["low", "Low", "Fast responses with lighter reasoning", "plain"],
      ["medium", "Medium", "Balances speed and reasoning depth for everyday tasks", "plain"],
      ["high", "High", "Greater reasoning depth for complex problems", "plain"],
      ["xhigh", "Extra high", "Extra high reasoning depth for complex problems", "plain"],
      ["max", "Max", "For difficult problems when quality matters more than speed · higher usage", "top"],
      ["ultra", "Ultra", "For demanding work using multiple agents · highest usage", "ultracode"],
    ];
    for (const [name, label, description, look] of tiers) {
      const row = screen.getByTestId(`effort-tier-${name}`);
      expect(row).toHaveTextContent(label);
      expect(row).toHaveTextContent(description);
      expect(row).toHaveAttribute("title", description);
      expect(row).toHaveAttribute("data-effort-look", look);
      expect(row).toHaveAttribute("data-ultracode", "0");
    }
    expect(screen.queryByTestId("effort-tier-minimal")).toBeNull();
    expect(screen.queryByTestId("effort-tier-ultracode")).toBeNull();
    await user.click(screen.getByTestId("effort-tier-ultra"));
    expect(onChange).toHaveBeenLastCalledWith({ kind: "codex", name: "ultra", index: 5 });
    await user.click(screen.getByTestId("effort-open-list"));
    await user.click(screen.getByTestId("effort-tier-max"));
    expect(onChange).toHaveBeenLastCalledWith({ kind: "codex", name: "max", index: 4 });
  });

  it("grok enumerates the verified four stops with no ember anywhere", () => {
    const { unmount } = mount({ kind: "grok", index: 3 });
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh");
    expect(slider).toHaveAttribute("data-effort-look", "top");
    expect(slider).toHaveAttribute("data-ember", "0");
    expect(screen.queryByTestId("effort-embers")).toBeNull();
    unmount();
  });

  it("marks every tier row in the list with its look", async () => {
    const user = userEvent.setup();
    mount({ kind: "claude", index: 2 });
    await user.click(screen.getByTestId("effort-open-list"));
    const rows = ["low", "medium", "high", "xhigh", "max", "ultracode"] as const;
    const expected: Record<(typeof rows)[number], string> = {
      low: "plain",
      medium: "plain",
      high: "plain",
      xhigh: "top",
      max: "top",
      ultracode: "ultracode",
    };
    for (const name of rows) {
      expect(screen.getByTestId(`effort-tier-${name}`)).toHaveAttribute(
        "data-effort-look",
        expected[name],
      );
    }
    // Only the ultracode row carries the flag.
    expect(screen.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ultracode", "1");
  });
});
