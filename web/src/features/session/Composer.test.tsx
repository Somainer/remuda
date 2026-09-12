import { render, screen } from "@testing-library/react";
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
    expect(screen.getByTestId("effort-menu")).toHaveTextContent("EFFORT · 本回合生效，发 Command 不只改本地");
    expect(screen.getByTestId("effort-tier-default")).toBeVisible();
    expect(screen.getByTestId("effort-tier-think")).toBeVisible();
    expect(screen.getByTestId("effort-tier-think-hard")).toBeVisible();
    expect(screen.getByTestId("effort-tier-ultracode")).toHaveAttribute("data-ember", "1");
    await user.click(screen.getByTestId("effort-tier-ultracode"));
    expect(onEffort).toHaveBeenCalledWith({ index: 3, name: "ultracode", kind: "claude" });
  });
});
