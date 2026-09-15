import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import { effortAt } from "./effort";

describe("Composer shortcuts", () => {
  it("sends on plain Enter (primary) on desktop; Shift+Enter is a newline", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    render(<Composer instanceId="ins_x" mobile={false} onSend={onSend} />);
    const area = screen.getByTestId("composer-input");
    await user.click(area);
    await user.type(area, "hello");
    await user.keyboard("{Enter}");
    // D-028 §6: Enter runs the primary control; the idle mode is new-turn.
    expect(onSend).toHaveBeenCalledTimes(1);
    expect(onSend.mock.calls[0][0]).toBe("hello");
    expect(onSend.mock.calls[0][3]).toBe("new-turn");
    onSend.mockClear();
    await user.type(area, "line1{Shift>}{Enter}{/Shift}line2");
    expect(onSend).not.toHaveBeenCalled();
    expect((area as HTMLTextAreaElement).value).toContain("\n");
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

  it("renders collapsed chips and lists the six Claude stops", async () => {
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
        effortEffective={{ name: "high", ultracode: null, source: "launch", observedAt: "2026-09-14T00:00:00Z" }}
        contextLabel="74%"
        onEffort={onEffort}
        onPermission={vi.fn()}
      />,
    );
    expect(screen.getByTestId("harness-chip")).toHaveTextContent(/Claude/);
    expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("high");
    expect(screen.getByTestId("model-effort-chip")).not.toHaveTextContent("opus");
    expect(screen.getByTestId("context-chip")).toHaveTextContent("74%");
    expect(screen.getByTestId("permission-chip")).toHaveTextContent(/询问/);
    await user.click(screen.getByTestId("model-effort-chip"));
    // Row 1 lightning + tier + reset, row 2 model, then the pill with ticks.
    // No standalone ultracode chip and no tier/model list until the chevron is tapped.
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
    expect(screen.queryByTestId("effort-tier-low")).toBeNull();
    expect(screen.queryByTestId("model-option-opus")).toBeNull();
    expect(screen.queryByTestId("effort-ultracode")).toBeNull();
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultracode");
    expect(slider).toHaveAttribute("aria-valuemax", "5");
    expect(slider).toHaveAttribute("data-name", "high");
    expect(slider).toHaveAttribute("data-ultracode", "0");
    expect(slider).toHaveAttribute("aria-valuetext", "high");
    expect(screen.getByTestId("effort-title")).toHaveTextContent("high");
    expect(screen.getByTestId("effort-model")).toHaveTextContent("opus");
    expect(screen.getByTestId("effort-knob")).toBeInTheDocument();
    slider.focus();
    // End now lands on the sixth stop — ultracode = xhigh tier + workflow flag.
    await user.keyboard("{End}");
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "xhigh", kind: "claude", ultracode: true });
  });

  it("the tier name opens a list of stops and models, and picking one closes it", async () => {
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
    // Both ember stops — max and ultracode — sit at the bottom of the list.
    expect(screen.getByTestId("effort-tier-max")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ultracode", "1");
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

  it("plain xhigh is not ember; the ultracode stop plays ember and names itself ultracode", async () => {
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
    expect(slider).toHaveAttribute("data-index", "3");
    expect(slider).toHaveAttribute("data-ember", "0");
    expect(slider).toHaveAttribute("data-ultracode", "0");

    // Walk xhigh → max → ultracode on the one slider (there is no separate chip).
    slider.focus();
    await user.keyboard("{ArrowRight}");
    expect(onEffort).toHaveBeenLastCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });
    await user.keyboard("{ArrowRight}");
    expect(onEffort).toHaveBeenLastCalledWith({ index: 3, name: "xhigh", kind: "claude", ultracode: true });

    rerender(
      <Composer
        instanceId="ins_ultra"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3, true)}
        effortEffective={{ name: "xhigh", ultracode: true, source: "remuda", observedAt: "2026-09-14T00:00:00Z" }}
        onEffort={onEffort}
      />,
    );
    expect(slider).toHaveAttribute("data-name", "ultracode");
    expect(slider).toHaveAttribute("data-index", "5");
    expect(slider).toHaveAttribute("data-tier-index", "3");
    expect(slider).toHaveAttribute("data-ultracode", "1");
    expect(slider).toHaveAttribute("data-ember", "1");
    // The stop is a normal slider stop: the track stays enabled...
    expect(slider).toHaveAttribute("aria-disabled", "false");
    // §9.1: the chip reads the EFFECTIVE transcript level ("xhigh", the level
    // ultracode runs at); the requested "ultracode" shows in the open slider.
    expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("xhigh");
    expect(screen.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
    // One arrow left returns to max — no locked track.
    await user.keyboard("{ArrowLeft}");
    expect(onEffort).toHaveBeenLastCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });
  });

  it("snaps a pointer drag to the nearest of the six Claude stops", async () => {
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
    // 400px pill; the knob centre travels between x=18 and x=382 (KNOB_INSET),
    // now across six stops (5 intervals).
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
    // Four fifths across = the fifth stop, max (not yet ultracode).
    fireEvent.pointerDown(slider, { clientX: 309, pointerId: 1, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 309, pointerId: 1, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });

    onEffort.mockClear();
    // The far-right stop is ultracode: xhigh tier plus the workflow flag.
    fireEvent.pointerDown(slider, { clientX: 396, pointerId: 2, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 396, pointerId: 2, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "xhigh", kind: "claude", ultracode: true });

    onEffort.mockClear();
    // Three fifths lands on xhigh (stop 3), a plain tier.
    fireEvent.pointerDown(slider, { clientX: 236, pointerId: 3, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 236, pointerId: 3, button: 0 });
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
