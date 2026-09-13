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
    expect(screen.getByTestId("model-effort-chip")).toHaveTextContent(/opus think/);
    expect(screen.getByTestId("context-chip")).toHaveTextContent("74%");
    expect(screen.getByTestId("permission-chip")).toHaveTextContent(/询问/);
    await user.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("effort-menu")).toHaveTextContent("切换只影响后续回合，不重写已发出的 prompt");
    expect(screen.queryByTestId("effort-tier-default")).toBeNull();
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
    expect(slider).toHaveAttribute("data-name", "think");
    expect(screen.getByTestId("effort-title")).toHaveTextContent("think");
    expect(screen.getByTestId("effort-hint")).toHaveTextContent("think · 默认档");
    slider.focus();
    await user.keyboard("{End}");
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "ultracode", kind: "claude" });
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
    const inner = slider.querySelector("div");
    if (!(inner instanceof HTMLDivElement)) throw new Error("missing track");
    vi.spyOn(inner, "getBoundingClientRect").mockReturnValue({
      x: 11,
      y: 18,
      left: 11,
      top: 18,
      right: 289,
      bottom: 26,
      width: 278,
      height: 8,
      toJSON() {
        return {};
      },
    });
    fireEvent.pointerDown(slider, { clientX: 280, pointerId: 1, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 280, pointerId: 1, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "ultracode", kind: "claude" });
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
