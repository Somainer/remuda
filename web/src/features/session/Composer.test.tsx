import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import { effortAt } from "./effort";

describe("Composer shortcuts", () => {
  it("sends on Cmd/Ctrl+Enter on desktop, not on plain Enter", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    render(<Composer instanceId="ins_x" mobile={false} onSend={onSend} />);
    const area = screen.getByTestId("composer-input");
    await user.click(area);
    await user.type(area, "hello");
    await user.keyboard("{Enter}");
    expect(onSend).not.toHaveBeenCalled();
    await user.keyboard("{Meta>}{Enter}{/Meta}");
    // The shortcut is what this asserts; a send now also carries its (empty)
    // attachment lists.
    expect(onSend.mock.calls[0][0]).toBe("hello");
  });

  it("does not send Cmd+Enter on mobile", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    render(<Composer instanceId="ins_y" mobile onSend={onSend} />);
    const area = screen.getByTestId("composer-input");
    await user.click(area);
    await user.type(area, "hello");
    await user.keyboard("{Meta>}{Enter}{/Meta}");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("renders collapsed chips and lists the five real Claude levels", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    render(
      <Composer
        instanceId="ins_z"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2)}
        contextLabel="74%"
        onEffort={onEffort}
        onPermission={vi.fn()}
      />,
    );
    expect(screen.getByTestId("harness-chip")).toHaveTextContent(/Claude/);
    expect(screen.getByTestId("model-effort-chip")).toHaveTextContent("high");
    expect(screen.getByTestId("model-effort-chip")).not.toHaveTextContent("opus");
    expect(screen.getByTestId("context-chip")).toHaveTextContent("74%");
    expect(screen.getByTestId("permission-chip")).toHaveTextContent(/询问/);
    await user.click(screen.getByTestId("model-effort-chip"));
    // Row 1 lightning + tier + ultracode + reset, row 2 model, then the pill. No tier list, no model list.
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
    expect(screen.queryByTestId("effort-tier-low")).toBeNull();
    expect(screen.queryByTestId("model-option-opus")).toBeNull();
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max");
    expect(slider).toHaveAttribute("data-name", "high");
    expect(slider).toHaveAttribute("data-ultracode", "0");
    expect(slider).toHaveAttribute("aria-valuetext", "high");
    expect(screen.getByTestId("effort-title")).toHaveTextContent("high");
    expect(screen.getByTestId("effort-model")).toHaveTextContent("opus");
    expect(screen.getByTestId("effort-knob")).toBeInTheDocument();
    // The ultracode toggle is present in the card header and starts off.
    const ultra = screen.getByTestId("effort-ultracode");
    expect(ultra).toHaveAttribute("data-on", "0");
    expect(ultra).toHaveAttribute("aria-pressed", "false");
    slider.focus();
    await user.keyboard("{End}");
    expect(onEffort).toHaveBeenCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });
  });

  it("the tier name opens a list of tiers and models, and picking one closes it", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    const onModel = vi.fn();
    render(
      <Composer
        instanceId="ins_list"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2)}
        onEffort={onEffort}
        onModel={onModel}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    await user.click(screen.getByTestId("effort-open-list"));
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "list");
    expect(screen.queryByTestId("effort-slider")).toBeNull();
    // max is the ember row now; ultracode is not in the tier list.
    expect(screen.getByTestId("effort-tier-max")).toHaveAttribute("data-ember", "1");
    expect(screen.queryByTestId("effort-tier-ultracode")).toBeNull();
    expect(screen.getByTestId("effort-tier-high")).toHaveAttribute("data-selected", "1");
    expect(screen.getByTestId("effort-list")).toHaveTextContent("跨文件 · 长任务");
    await user.click(screen.getByTestId("model-option-sonnet"));
    expect(onModel).toHaveBeenCalledWith("sonnet");
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");

    await user.click(screen.getByTestId("effort-open-list"));
    await user.click(screen.getByTestId("effort-tier-xhigh"));
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "xhigh", kind: "claude", ultracode: false });
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
  });

  it("turns the pill and the tier name ember on the max tier", async () => {
    const user = userEvent.setup();
    render(
      <Composer
        instanceId="ins_ember"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 4)}
        onEffort={vi.fn()}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-title")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
  });

  it("plain xhigh is not ember; ultracode locks xhigh and plays ember", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    const { rerender } = render(
      <Composer
        instanceId="ins_ultra"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3)}
        onEffort={onEffort}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-name", "xhigh");
    expect(slider).toHaveAttribute("data-ember", "0");
    expect(slider).toHaveAttribute("data-ultracode", "0");

    // Toggle ultracode on: selection locks to xhigh + ultracode and ember plays.
    await user.click(screen.getByTestId("effort-ultracode"));
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "xhigh", kind: "claude", ultracode: true });

    rerender(
      <Composer
        instanceId="ins_ultra"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3, true)}
        onEffort={onEffort}
      />,
    );
    expect(slider).toHaveAttribute("data-name", "xhigh");
    expect(slider).toHaveAttribute("data-ultracode", "1");
    expect(slider).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-ultracode")).toHaveAttribute("data-on", "1");
    // The track no longer takes tier input while locked.
    expect(slider).toHaveAttribute("aria-disabled", "true");
    slider.focus();
    await user.keyboard("{End}");
    // No new tier event from the locked track; the last call is the toggle's.
    expect(onEffort).toHaveBeenCalledTimes(1);
  });

  it("snaps a pointer drag to the nearest of the five native tiers", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    render(
      <Composer
        instanceId="ins_drag"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2)}
        onEffort={onEffort}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const slider = screen.getByTestId("effort-slider");
    const track = screen.getByTestId("effort-track");
    // 400px pill; the knob centre travels between x=18 and x=382 (KNOB_INSET).
    vi.spyOn(track, "getBoundingClientRect").mockReturnValue({
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 400,
      bottom: 44,
      width: 400,
      height: 44,
      toJSON() {
        return {};
      },
    });
    fireEvent.pointerDown(slider, { clientX: 396, pointerId: 1, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 396, pointerId: 1, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });

    onEffort.mockClear();
    // Three-quarters in lands on the fourth stop (xhigh), not on an edge.
    fireEvent.pointerDown(slider, { clientX: 291, pointerId: 2, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 291, pointerId: 2, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "xhigh", kind: "claude", ultracode: false });
  });

  it("Home/End and reset land on the table edges and default", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    const { rerender } = render(
      <Composer
        instanceId="ins_keys"
        mobile={false}
        onSend={vi.fn()}
        kind="grok"
        model="grok-4"
        effort={effortAt("grok", 1)}
        onEffort={onEffort}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "quick,standard,max");
    slider.focus();
    await user.keyboard("{Home}");
    expect(onEffort).toHaveBeenCalledWith({ index: 0, name: "quick", kind: "grok" });
    rerender(
      <Composer
        instanceId="ins_keys"
        mobile={false}
        onSend={vi.fn()}
        kind="grok"
        model="grok-4"
        effort={effortAt("grok", 0)}
        onEffort={onEffort}
      />,
    );
    await user.click(screen.getByTestId("effort-reset"));
    // Claude high (midpoint) maps by nearest position onto grok's middle tier.
    expect(onEffort).toHaveBeenCalledWith({ index: 1, name: "standard", kind: "grok" });
  });

  it("locks the slider when the session is not configurable", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    render(
      <Composer
        instanceId="ins_lock"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        effort={effortAt("claude", 2)}
        onEffort={onEffort}
        effortDisabled
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("aria-disabled", "true");
    slider.focus();
    await user.keyboard("{End}");
    expect(onEffort).not.toHaveBeenCalled();
    expect(screen.getByTestId("effort-reset")).toBeDisabled();
  });

  it("lists the native codex table for a codex session and hides ultracode", async () => {
    const user = userEvent.setup();
    render(
      <Composer
        instanceId="ins_codex"
        mobile={false}
        onSend={vi.fn()}
        kind="codex"
        model="gpt-5"
        effort={effortAt("codex", 1)}
        onEffort={vi.fn()}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-tiers", "low,medium,high,ultra");
    expect(screen.queryByTestId("effort-ultracode")).toBeNull();
  });

  it("shows the harness as a static label inside a session, with no menu", async () => {
    const user = userEvent.setup();
    render(
      <Composer
        instanceId="ins_locked"
        mobile={false}
        onSend={vi.fn()}
        kind="codex"
        model="gpt-5"
        effort={effortAt("codex", 1)}
      />,
    );
    const chip = screen.getByTestId("harness-chip");
    expect(chip).toHaveAttribute("data-readonly", "1");
    expect(chip).toHaveTextContent("Codex");
    expect(chip).toHaveTextContent("X");
    expect(chip.tagName).toBe("SPAN");
    expect(chip).not.toHaveAttribute("aria-expanded");
    await user.click(chip);
    expect(screen.queryByTestId("harness-menu")).toBeNull();
    expect(screen.queryByTestId("harness-option-claude")).toBeNull();
    expect(screen.queryByTestId("harness-option-terminal")).toBeNull();
  });

  it("abbreviates the harness label on mobile but keeps it static", () => {
    render(
      <Composer
        instanceId="ins_locked_m"
        mobile
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2)}
      />,
    );
    const chip = screen.getByTestId("harness-chip");
    expect(chip).toHaveTextContent("Claude");
    expect(chip).not.toHaveTextContent("Claude Code");
    expect(chip.querySelector("button")).toBeNull();
  });

  it("shows a read-only yolo permission chip for grok pty", () => {
    render(
      <Composer
        instanceId="ins_grok"
        mobile={false}
        onSend={vi.fn()}
        kind="grok"
        model="grok-4"
        effort={effortAt("grok", 1)}
        permissionMode="always-approve"
      />,
    );
    const chip = screen.getByTestId("permission-chip");
    expect(chip).toHaveAttribute("data-readonly", "1");
    expect(chip).toHaveTextContent("always-approve");
    expect(screen.getByTestId("harness-chip")).toHaveTextContent(/Grok/i);
    expect(screen.getByTestId("model-effort-chip")).toBeVisible();
    expect(screen.getByTestId("context-chip")).toBeVisible();
  });
});
