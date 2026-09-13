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
    expect(onSend).toHaveBeenCalledWith("hello");
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

  it("renders collapsed chips and lists the claude effort table", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    render(
      <Composer
        instanceId="ins_z"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 1)}
        contextLabel="74%"
        onEffort={onEffort}
        onPermission={vi.fn()}
      />,
    );
    expect(screen.getByTestId("harness-chip")).toHaveTextContent(/Claude/);
    expect(screen.getByTestId("model-effort-chip")).toHaveTextContent("think");
    expect(screen.getByTestId("model-effort-chip")).not.toHaveTextContent("opus");
    expect(screen.getByTestId("context-chip")).toHaveTextContent("74%");
    expect(screen.getByTestId("permission-chip")).toHaveTextContent(/询问/);
    await user.click(screen.getByTestId("model-effort-chip"));
    // Row 1 lightning + tier + reset, row 2 model, then the pill. No tier list, no model list.
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
    expect(screen.queryByTestId("effort-tier-default")).toBeNull();
    expect(screen.queryByTestId("model-option-opus")).toBeNull();
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
    expect(slider).toHaveAttribute("data-name", "think");
    expect(slider).toHaveAttribute("aria-valuetext", "think");
    expect(screen.getByTestId("effort-title")).toHaveTextContent("think");
    expect(screen.getByTestId("effort-model")).toHaveTextContent("opus");
    expect(screen.getByTestId("effort-knob")).toBeInTheDocument();
    slider.focus();
    await user.keyboard("{End}");
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "ultracode", kind: "claude" });
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
        effort={effortAt("claude", 1)}
        onEffort={onEffort}
        onModel={onModel}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    await user.click(screen.getByTestId("effort-open-list"));
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "list");
    expect(screen.queryByTestId("effort-slider")).toBeNull();
    expect(screen.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-tier-think")).toHaveAttribute("data-selected", "1");
    expect(screen.getByTestId("effort-list")).toHaveTextContent("跨文件重构、长任务");
    await user.click(screen.getByTestId("model-option-sonnet"));
    expect(onModel).toHaveBeenCalledWith("sonnet");
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");

    await user.click(screen.getByTestId("effort-open-list"));
    await user.click(screen.getByTestId("effort-tier-think-hard"));
    expect(onEffort).toHaveBeenCalledWith({ index: 2, name: "think-hard", kind: "claude" });
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
  });

  it("the top tier turns the pill and the tier name ember", async () => {
    const user = userEvent.setup();
    render(
      <Composer
        instanceId="ins_ember"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3)}
        onEffort={vi.fn()}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("effort-title")).toHaveAttribute("data-ember", "1");
    expect(screen.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
  });

  it("snaps a pointer drag to the nearest native tier", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    render(
      <Composer
        instanceId="ins_drag"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 1)}
        onEffort={onEffort}
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const slider = screen.getByTestId("effort-slider");
    const track = screen.getByTestId("effort-track");
    // 400px pill; the knob centre travels between x=22 and x=378.
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
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "ultracode", kind: "claude" });

    onEffort.mockClear();
    // Dead-centre lands on the middle stop, not on an edge.
    fireEvent.pointerDown(slider, { clientX: 200, pointerId: 2, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 200, pointerId: 2, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 2, name: "think-hard", kind: "claude" });
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
        effort={effortAt("claude", 1)}
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
        effort={effortAt("claude", 1)}
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
