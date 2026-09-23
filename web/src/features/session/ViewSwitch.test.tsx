import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import css from "./ViewSwitch.module.css";
import { ViewSwitch } from "./ViewSwitch";

describe("ViewSwitch", () => {
  it("renders one segmented control with two exclusive states", () => {
    render(<ViewSwitch value="tty" onChange={vi.fn()} />);
    const group = screen.getByTestId("view-switch");
    expect(group).toHaveAttribute("role", "radiogroup");
    expect(group).toHaveAttribute("data-view", "tty");
    expect(screen.getAllByRole("radio")).toHaveLength(2);
    expect(screen.getByTestId("view-switch-tty")).toHaveAttribute("aria-checked", "true");
    expect(screen.getByTestId("view-switch-structured")).toHaveAttribute("aria-checked", "false");
  });

  it("reports the other state on click and stays quiet when already selected", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<ViewSwitch value="tty" onChange={onChange} />);
    await user.click(screen.getByTestId("view-switch-tty"));
    expect(onChange).not.toHaveBeenCalled();
    await user.click(screen.getByTestId("view-switch-structured"));
    expect(onChange).toHaveBeenCalledWith("structured");
  });

  it("moves between states with the arrow keys", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<ViewSwitch value="tty" onChange={onChange} />);
    const selected = screen.getByTestId("view-switch-tty");
    expect(selected).toHaveAttribute("tabindex", "0");
    expect(screen.getByTestId("view-switch-structured")).toHaveAttribute("tabindex", "-1");
    selected.focus();
    await user.keyboard("{ArrowRight}");
    expect(onChange).toHaveBeenCalledWith("structured");
    onChange.mockClear();
    await user.keyboard("{ArrowLeft}");
    expect(onChange).toHaveBeenCalledWith("structured");
  });

  it("keeps the hit-area-bearing viewSeg class on both radios regardless of state", () => {
    // Not a hit-box assertion: jsdom performs no layout and the stylesheet is
    // not applied, so a class-string check cannot prove the ::after exists,
    // is centred, or is unobstructed — the geometry binding lives in
    // ux-touchhit.hub.spec.ts.
    // This only guards the TSX invariant the hot zone relies on: the on-state
    // class is additive, so toggling state never drops `.viewSeg` (the class
    // the mobile ::after is keyed to) from a segment.
    render(<ViewSwitch value="tty" onChange={vi.fn()} />);
    const selected = screen.getByTestId("view-switch-tty");
    const other = screen.getByTestId("view-switch-structured");
    expect(selected.className.split(" ")).toContain(css.viewSeg);
    expect(other.className.split(" ")).toContain(css.viewSeg);
    expect(selected.className.split(" ")).toContain(css.viewSegOn);
    expect(other.className.split(" ")).not.toContain(css.viewSegOn);
  });
});
