import { render, screen, waitFor } from "@testing-library/react";
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

describe("EffortSlider model read-back display", () => {
  // The attribute lives on the expanded tier/model panel.
  async function openPanel(overrides: Partial<SliderProps> & Pick<SliderProps, "kind">) {
    mount(overrides);
    await userEvent.setup().click(screen.getByTestId("effort-open-list"));
    return screen.getByTestId("effort-slider-panel");
  }

  it("shows both strings when a gateway resolves the catalog pin to an upstream name", async () => {
    // Measured: es1_orange_o50 answers as claude-opus-5 on a real gateway.
    // The strings differ, so both are shown verbatim — no verdict.
    const panel = await openPanel({
      kind: "claude",
      model: "model_hub/es1_orange_o50[1m]",
      modelEffective: "claude-opus-5",
    });
    expect(panel).toHaveAttribute("data-model-different", "1");
    const note = screen.getByTestId("model-option-different");
    expect(note).toHaveTextContent("model_hub/es1_orange_o50[1m]");
    expect(note).toHaveTextContent("claude-opus-5");
  });

  it("shows both strings for a different id in the pin's own namespace", async () => {
    const panel = await openPanel({
      kind: "claude",
      model: "model_hub/es1_orange_o50[1m]",
      modelEffective: "model_hub/es1_orange_o48[1m]",
    });
    expect(panel).toHaveAttribute("data-model-different", "1");
    const note = screen.getByTestId("model-option-different");
    expect(note).toHaveTextContent("model_hub/es1_orange_o50[1m]");
    expect(note).toHaveTextContent("model_hub/es1_orange_o48[1m]");
  });

  it("shows both strings for a [1m] context-suffix spelling difference", async () => {
    const panel = await openPanel({
      kind: "claude",
      model: "ark/seed-evolving[1m]",
      modelEffective: "ark/seed-evolving",
    });
    expect(panel).toHaveAttribute("data-model-different", "1");
  });

  it("does not show a difference while a switch is pending even if the id differs", async () => {
    const panel = await openPanel({
      kind: "claude",
      model: "model_hub/es1_orange_o50[1m]",
      modelEffective: "model_hub/es1_orange_o48[1m]",
      modelPending: { id: "model_hub/es1_orange_o48[1m]", queued: false },
    });
    expect(panel).toHaveAttribute("data-model-different", "0");
  });

  it("shows no difference for identical strings", async () => {
    const panel = await openPanel({
      kind: "claude",
      model: "ark/seed-evolving",
      modelEffective: "ark/seed-evolving",
    });
    expect(panel).toHaveAttribute("data-model-different", "0");
    expect(screen.queryByTestId("model-option-different")).toBeNull();
  });
});

describe("EffortSlider tall catalog", () => {
  const tallCatalog = Array.from({ length: 80 }, (_, i) => `gateway/model-${i}`);

  function mountList() {
    const onModel = vi.fn();
    mount({
      kind: "claude",
      index: 2,
      model: "gateway/model-0",
      models: tallCatalog,
      modelEffective: "gateway/model-0",
      onModel,
    });
    return onModel;
  }

  it("renders every catalog row inside one scrollable list body", async () => {
    const user = userEvent.setup();
    mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    const body = screen.getByTestId("effort-list");
    expect(body).toHaveAttribute("data-popover-scroll", "1");
    // The first tier row is reachable even though 80 model rows follow.
    expect(screen.getByTestId("effort-tier-low")).toBeInTheDocument();
    expect(screen.getByTestId("model-option-model-79")).toBeInTheDocument();
    expect(body.querySelectorAll("button")).toHaveLength(80 + 6);
  });

  it("moves roving focus with arrow keys, Home and End", async () => {
    const user = userEvent.setup();
    mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    // The selected tier row (high, index 2) opens focused; the initial focus
    // ride is scheduled on an animation frame.
    await waitFor(() => expect(screen.getByTestId("effort-tier-high")).toHaveFocus());
    await user.keyboard("{ArrowDown}");
    expect(screen.getByTestId("effort-tier-xhigh")).toHaveFocus();
    await user.keyboard("{End}");
    expect(screen.getByTestId("model-option-model-79")).toHaveFocus();
    await user.keyboard("{Home}");
    expect(screen.getByTestId("effort-tier-low")).toHaveFocus();
    await user.keyboard("{ArrowUp}");
    // Clamps at the first row.
    expect(screen.getByTestId("effort-tier-low")).toHaveFocus();
  });

  it("Enter picks the focused model row; Escape closes back to the slider", async () => {
    const user = userEvent.setup();
    const onModel = mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    // The open-list focus ride is scheduled on an animation frame; pressing
    // End before it lands retargets body (the listbox keydown never runs),
    // and the late focus ride leaves Enter to activate the tier row instead
    // of the model row. Wait for the documented focus state after each key.
    await waitFor(() => expect(screen.getByTestId("effort-tier-high")).toHaveFocus());
    await user.keyboard("{End}");
    await waitFor(() => expect(screen.getByTestId("model-option-model-79")).toHaveFocus());
    await user.keyboard("{Enter}");
    expect(onModel).toHaveBeenCalledWith("gateway/model-79");
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
  });

  it("lets a free-typed id through as the verbatim fallback", async () => {
    const user = userEvent.setup();
    const onModel = mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    const input = screen.getByTestId("effort-model-type");
    await user.type(input, "claude-grok-4.6{Enter}");
    expect(onModel).toHaveBeenCalledWith("claude-grok-4.6");
  });

  it("renders model rows disabled with a why title when configure is unavailable", async () => {
    const user = userEvent.setup();
    const onModel = vi.fn();
    mount({
      kind: "claude",
      index: 2,
      model: "opus",
      models: ["sonnet"],
      onModel,
      modelLockedReason: "会话已退出",
    });
    await user.click(screen.getByTestId("effort-open-list"));
    const row = screen.getByTestId("model-option-sonnet");
    expect(row).toBeDisabled();
    expect(row).toHaveAttribute("aria-disabled", "true");
    expect(row.getAttribute("title")).toContain("会话已退出");
    await user.click(row);
    expect(onModel).not.toHaveBeenCalled();
    expect(screen.getByTestId("effort-model-type")).toBeDisabled();
  });

  it("flags the host-fallback catalog with a one-line diagnostic", async () => {
    const user = userEvent.setup();
    mount({
      kind: "claude",
      index: 2,
      model: "opus",
      models: ["claude-grok-4.6"],
      onModel: vi.fn(),
      modelCatalog: {
        models: ["claude-grok-4.6"],
        source: "gateway-discovery",
        observedAt: "2026-09-18T10:00:00Z",
        cache: {
          scope: "host-fallback",
          baseUrl: "https://relay.example.invalid/v1",
          fetchedAt: "2026-09-18T10:00:00Z",
        },
        discoveryEnv: true,
      },
    });
    await user.click(screen.getByTestId("effort-open-list"));
    const note = screen.getByTestId("effort-catalog-note");
    expect(note).toHaveAttribute("data-reason", "host-fallback");
    expect(note.textContent).toContain("主机缓存");
  });
});
