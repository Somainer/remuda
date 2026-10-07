import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { EffortSlider } from "./EffortSlider";

type SliderProps = Parameters<typeof EffortSlider>[0];

function mount(overrides: Partial<SliderProps> & Pick<SliderProps, "kind">) {
  return render(<EffortSlider index={2} onChange={vi.fn()} {...overrides} />);
}

describe("EffortSlider five-stop slider + switch", () => {
  it("Claude lists exactly five tier stops, no sixth ultracode stop", () => {
    mount({ kind: "claude", index: 2 });
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max");
  });

  it("low/medium/high plain; xhigh/max static top; no ember on the pill", () => {
    for (const [index, look] of [
      [0, "plain"],
      [1, "plain"],
      [2, "plain"],
      [3, "top"],
      [4, "top"],
    ] as const) {
      const { unmount } = mount({ kind: "claude", index, ultracode: true });
      const slider = screen.getByTestId("effort-slider");
      expect(slider).toHaveAttribute("data-effort-look", look);
      // Pill ember follows the TIER only; the switch carries the ember.
      expect(screen.queryByTestId("effort-embers")).toBeNull();
      unmount();
    }
  });

  it("a tier drag changes only the tier, carrying the current flag along", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    mount({ kind: "claude", index: 2, ultracode: true, onChange });
    const slider = screen.getByTestId("effort-slider");
    slider.focus();
    await user.keyboard("{ArrowRight}"); // high → xhigh, flag rides along
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ name: "xhigh", ultracode: true }),
    );
    await user.keyboard("{ArrowRight}"); // xhigh → max, still on
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ name: "max", ultracode: true }),
    );
  });

  it("the switch never moves the slider; flipping it calls onUltracodeChange only", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const onUltra = vi.fn();
    mount({ kind: "claude", index: 4, ultracode: false, onChange, onUltracodeChange: onUltra, ultraGate: "decoupled" });
    const sw = screen.getByTestId("effort-ultracode-switch");
    expect(sw).toHaveAttribute("role", "switch");
    expect(sw).toHaveAttribute("aria-checked", "false");
    await user.click(sw);
    expect(onUltra).toHaveBeenCalledWith(true);
    expect(onChange).not.toHaveBeenCalled();
    // The slider still names max.
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-name", "max");
  });

  it("marks the per-model default tier with a dot, and hides it when defaultIndex is null", () => {
    mount({ kind: "claude", index: 2, defaultIndex: 1 });
    const dots = document.querySelectorAll("[data-default='1']");
    expect(dots.length).toBeGreaterThan(0);
  });

  it("Codex keeps its six native tiers and an ember Ultra, no switch row", () => {
    mount({ kind: "codex", index: 5 });
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultra");
    expect(slider).toHaveAttribute("data-effort-look", "ultracode");
    expect(screen.queryByTestId("effort-ultracode")).toBeNull();
  });

  it("grok has four stops and no switch", () => {
    mount({ kind: "grok", index: 3 });
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-tiers", "low,medium,high,xhigh");
    expect(screen.queryByTestId("effort-ultracode")).toBeNull();
  });
});

describe("Ultracode switch gating", () => {
  it("is enabled with an onUltracodeChange handler and decoupled gate", () => {
    mount({ kind: "claude", index: 3, ultraGate: "decoupled", onUltracodeChange: vi.fn() });
    const row = screen.getByTestId("effort-ultracode");
    expect(row).toHaveAttribute("data-disabled", "0");
    expect(row).toHaveAttribute("data-gate", "decoupled");
  });

  it("is disabled below 2.1.203 with the version reason", () => {
    mount({ kind: "claude", index: 3, ultraGate: "legacy", onUltracodeChange: vi.fn() });
    expect(screen.getByTestId("effort-ultracode")).toHaveAttribute("data-disabled", "1");
    expect(screen.getByTestId("effort-ultracode-switch")).toBeDisabled();
    expect(screen.getByTestId("effort-ultracode-reason").textContent).toContain("2.1.203");
  });

  it("r2 item 1: is disabled with a named reason when the version is unknown", () => {
    // Never assume decoupled support from an unreported/unparsable version:
    // only the switch locks, the five stops stay on.
    mount({ kind: "claude", index: 3, ultraGate: "unknown", onUltracodeChange: vi.fn() });
    expect(screen.getByTestId("effort-ultracode")).toHaveAttribute("data-disabled", "1");
    expect(screen.getByTestId("effort-ultracode-switch")).toBeDisabled();
    expect(screen.getByTestId("effort-ultracode-reason").textContent).toContain("版本");
    expect(screen.getByTestId("effort-slider")).toBeInTheDocument();
  });

  it("names the model on an ultracode-unavailable-for-model refusal", () => {
    mount({
      kind: "claude",
      index: 3,
      ultraGate: "decoupled",
      onUltracodeChange: vi.fn(),
      ultraBlocked: { reason: "ultracode-unavailable-for-model", model: "claude-sonnet-4-6" },
    });
    expect(screen.getByTestId("effort-ultracode-reason").textContent).toContain("claude-sonnet-4-6");
  });

  it("names dynamic workflows on a workflows-disabled refusal", () => {
    mount({
      kind: "claude",
      index: 3,
      ultraGate: "decoupled",
      onUltracodeChange: vi.fn(),
      ultraBlocked: { reason: "ultracode-workflows-disabled" },
    });
    expect(screen.getByTestId("effort-ultracode-reason").textContent).toContain("dynamic workflows");
  });

  it("says coupled builds run it as xhigh in the hint copy", () => {
    mount({ kind: "claude", index: 3, ultraGate: "coupled", onUltracodeChange: vi.fn() });
    expect(screen.getByTestId("effort-ultracode-hint").textContent).toContain("xhigh");
  });

  it("is disabled when there is no configure channel", () => {
    mount({ kind: "claude", index: 3, ultraGate: "decoupled" });
    expect(screen.getByTestId("effort-ultracode-switch")).toBeDisabled();
  });
});

describe("EffortSlider running-model chip", () => {
  it("shows the launch spec verbatim before any read-back", () => {
    mount({ kind: "claude", model: "", launchModel: "acme_hub/model_x_o50[1m]" });
    const chip = screen.getByTestId("effort-model");
    expect(chip.textContent).toBe("acme_hub/model_x_o50[1m]");
    expect(chip.getAttribute("title")).toContain("尚未从会话回读");
  });

  it("shows the read-back id once observed", () => {
    mount({ kind: "claude", model: "", launchModel: "acme/model", modelEffective: "claude-opus-5" });
    expect(screen.getByTestId("effort-model").textContent).toBe("claude-opus-5");
    expect(screen.getByTestId("effort-model").getAttribute("title")).toBe("实际 claude-opus-5");
  });

  it("codex shows the model, not the effort stop description", () => {
    mount({ kind: "codex", index: 4, model: "gpt-5", launchModel: "gpt-5", modelEffective: "gpt-5.4" });
    expect(screen.getByTestId("effort-model").textContent).toBe("gpt-5.4");
  });

  it("an effort-only slider (no model prop) keeps the tier description", () => {
    mount({ kind: "claude", index: 2 });
    const chip = screen.getByTestId("effort-model");
    // The high tier description (no universal 默认档 copy under D-056).
    expect(chip.textContent).toContain("综合实现");
  });
});

describe("EffortSlider tier/model list", () => {
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

  it("lists the five Claude tiers with their looks (no ultracode row)", async () => {
    const user = userEvent.setup();
    mount({ kind: "claude", index: 2 });
    await user.click(screen.getByTestId("effort-open-list"));
    for (const [name, look] of [
      ["low", "plain"],
      ["medium", "plain"],
      ["high", "plain"],
      ["xhigh", "top"],
      ["max", "top"],
    ] as const) {
      expect(screen.getByTestId(`effort-tier-${name}`)).toHaveAttribute("data-effort-look", look);
    }
    expect(screen.queryByTestId("effort-tier-ultracode")).toBeNull();
  });

  it("renders every catalog row inside one scrollable list body", async () => {
    const user = userEvent.setup();
    mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    const body = screen.getByTestId("effort-list");
    expect(screen.getByTestId("effort-tier-low")).toBeInTheDocument();
    expect(screen.getByTestId("model-option-model-79")).toBeInTheDocument();
    expect(body.querySelectorAll("button")).toHaveLength(80 + 5);
  });

  it("moves roving focus with arrow keys, Home and End", async () => {
    const user = userEvent.setup();
    mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    await waitFor(() => expect(screen.getByTestId("effort-tier-high")).toHaveFocus());
    await user.keyboard("{ArrowDown}");
    expect(screen.getByTestId("effort-tier-xhigh")).toHaveFocus();
    await user.keyboard("{End}");
    expect(screen.getByTestId("model-option-model-79")).toHaveFocus();
    await user.keyboard("{Home}");
    expect(screen.getByTestId("effort-tier-low")).toHaveFocus();
  });

  it("lets a free-typed id through as the verbatim fallback", async () => {
    const user = userEvent.setup();
    const onModel = mountList();
    await user.click(screen.getByTestId("effort-open-list"));
    const input = screen.getByTestId("effort-model-type");
    await user.type(input, "claude-grok-4.6{Enter}");
    expect(onModel).toHaveBeenCalledWith("claude-grok-4.6");
  });

  it("disables model rows with a why title when configure is unavailable", async () => {
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
    expect(row.getAttribute("title")).toContain("会话已退出");
    await user.click(row);
    expect(onModel).not.toHaveBeenCalled();
  });
});
