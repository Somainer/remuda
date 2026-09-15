import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { TuiModeIndicator } from "./TuiModeIndicator";

describe("actual terminal renderer", () => {
  it("keeps mode unknown until reported even when fullscreen was requested", () => {
    render(<TuiModeIndicator altScreen={undefined} requestedTui="fullscreen" hasEngagedAltScreen={false} />);
    expect(screen.getByTestId("tty-alt-screen")).toHaveAttribute("data-alt-screen", "unknown");
    expect(screen.getByTestId("tty-alt-screen")).toHaveTextContent("渲染方式待检测");
    expect(screen.queryByTestId("tty-tui-mismatch")).not.toBeInTheDocument();
  });

  it("uses the reported state for both renderer labels regardless of the requested mode", () => {
    const { rerender } = render(<TuiModeIndicator altScreen={true} requestedTui="default" hasEngagedAltScreen />);
    expect(screen.getByTestId("tty-alt-screen")).toHaveAttribute("data-alt-screen", "true");
    expect(screen.getByTestId("tty-alt-screen")).toHaveTextContent("全屏渲染");
    rerender(<TuiModeIndicator altScreen={false} requestedTui="fullscreen" hasEngagedAltScreen />);
    expect(screen.getByTestId("tty-alt-screen")).toHaveAttribute("data-alt-screen", "false");
    expect(screen.getByTestId("tty-alt-screen")).toHaveTextContent("行内渲染");
    // Returning from the alternate screen (including /tui default) is not evidence
    // that fullscreen failed to engage at launch.
    expect(screen.queryByTestId("tty-tui-mismatch")).not.toBeInTheDocument();
  });

  it("discreetly reports an unobserved fullscreen request without inventing a cause", () => {
    const { rerender } = render(<TuiModeIndicator altScreen={false} requestedTui="fullscreen" hasEngagedAltScreen={false} />);
    expect(screen.getByTestId("tty-tui-mismatch")).toHaveTextContent("已请求全屏，尚未检测到全屏画面");
    rerender(<TuiModeIndicator altScreen={true} requestedTui="fullscreen" hasEngagedAltScreen />);
    expect(screen.queryByTestId("tty-tui-mismatch")).not.toBeInTheDocument();
  });
});
