import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Composer, type HeldItem } from "./Composer";
import { printCapabilities } from "../../lib/capabilities";
import type { Capability, CapabilityProvision, CapabilitySnapshot } from "../../types/nativeRef";

/**
 * c-steer 插队发送: every queued (Remuda-held) composer row carries its own
 * button to interrupt the running turn and send that row now. The native
 * mirror rows never do (the wire owns them), and the control is disabled with
 * a visible reason wherever 插队 is not honest.
 */
function caps(over: Partial<Record<"steer" | "queue" | "interrupt", Capability>> = {}): CapabilitySnapshot {
  const base = printCapabilities();
  return {
    ...base,
    capabilities: { ...base.capabilities, ...over } as CapabilitySnapshot["capabilities"],
  };
}

function cap(state: Capability["state"] = "supported", p: CapabilityProvision = "native"): Capability {
  return { state, provision: p, scope: [], reasonCode: "test", prerequisites: [], evidence: [] };
}

const held: HeldItem[] = [
  { id: "local_a", text: "queued alpha", reason: "turn", holder: "remuda" },
  { id: "local_b", text: "queued beta", reason: "turn", holder: "remuda" },
  { id: "mirror_1", text: "native row", reason: "turn", holder: "native" },
];

function renderComposer(phase: "idle" | "working" | "blocked", over = {}, onSteerHeld = vi.fn()) {
  render(
    <Composer
      instanceId="ins_steerq"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      phase={phase}
      capabilities={caps(over)}
      held={held}
      onSteerHeld={onSteerHeld}
      onRetractHeld={vi.fn()}
    />,
  );
  return onSteerHeld;
}

describe("Composer 插队发送 per-queued-row control", () => {
  it("renders one button per remuda row with its aria-label; none on the native mirror", () => {
    renderComposer("working", { steer: cap(), interrupt: cap() });
    const buttons = screen.getAllByTestId("composer-queued-steer");
    // Two remuda rows → two buttons; the native mirror row has none.
    expect(buttons).toHaveLength(2);
    for (const button of buttons) {
      expect(button).toHaveAccessibleName("插队发送这条排队消息");
      expect(button.tagName).toBe("BUTTON");
      expect(button).toHaveAttribute("type", "button");
      expect(button).toBeEnabled();
    }
  });

  it("a click calls onSteerHeld once with that row's id", async () => {
    const user = userEvent.setup();
    const onSteerHeld = renderComposer("working", { steer: cap(), interrupt: cap() });
    await user.click(screen.getAllByTestId("composer-queued-steer")[1]);
    expect(onSteerHeld).toHaveBeenCalledTimes(1);
    expect(onSteerHeld).toHaveBeenCalledWith("local_b");
  });

  it("two fast clicks still call onSteerHeld once (in-flight guard)", async () => {
    const user = userEvent.setup();
    const onSteerHeld = renderComposer("working", { steer: cap(), interrupt: cap() });
    const button = screen.getAllByTestId("composer-queued-steer")[0];
    await user.click(button);
    await user.click(button);
    expect(onSteerHeld).toHaveBeenCalledTimes(1);
    expect(onSteerHeld).toHaveBeenCalledWith("local_a");
  });

  it("is reachable by Tab and activates on Enter/Space", async () => {
    const user = userEvent.setup();
    const onSteerHeld = renderComposer("working", { steer: cap(), interrupt: cap() });
    const first = screen.getAllByTestId("composer-queued-steer")[0];
    first.focus();
    expect(first).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(onSteerHeld).toHaveBeenCalledWith("local_a");
  });

  it("disabled with the phase-blocked reason (Esc must not hit an open question)", () => {
    renderComposer("blocked", { steer: cap(), interrupt: cap() });
    const button = screen.getAllByTestId("composer-queued-steer")[0];
    expect(button).toBeDisabled();
    expect(button).toHaveAccessibleName(/问题处理中，回答后送出/);
  });

  it("disabled with the idle reason (no running turn to interrupt)", () => {
    renderComposer("idle", { steer: cap(), interrupt: cap() });
    const button = screen.getAllByTestId("composer-queued-steer")[0];
    expect(button).toBeDisabled();
    expect(button).toHaveAccessibleName(/无进行中的回合，回车即送出/);
  });

  it("disabled with the capability reason when interrupt is unsupported", () => {
    renderComposer("working", { steer: cap("unsupported"), interrupt: cap("unsupported") });
    const button = screen.getAllByTestId("composer-queued-steer")[0];
    expect(button).toBeDisabled();
    expect(button).toHaveAccessibleName(/该载体未提供打断能力/);
  });

  it("keeps every row's ordinal text while offering the control", () => {
    renderComposer("working", { steer: cap(), interrupt: cap() });
    const chips = screen.getAllByTestId("composer-queued-chip");
    // The two remuda turn rows keep their 1-based ordinals; the native row is 原生.
    expect(chips[0]).toHaveTextContent("第 1 条");
    expect(chips[0]).toHaveTextContent("queued alpha");
    expect(chips[1]).toHaveTextContent("第 2 条");
    expect(chips[1]).toHaveTextContent("queued beta");
    expect(chips[2]).toHaveTextContent("原生");
  });
});
