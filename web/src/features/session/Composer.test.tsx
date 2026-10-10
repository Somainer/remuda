import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Composer, HOVER_CLOSE_DELAY_MS } from "./Composer";
import { effortAt } from "./effort";
import { printCapabilities } from "../../lib/capabilities";
import type { Capability, CapabilityProvision, CapabilitySnapshot } from "../../types/nativeRef";

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

  it("renders collapsed chips, five tier stops and the ultracode switch", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    render(
      <Composer
        instanceId="ins_z"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        launchModel="opus"
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
    // The panel shows the pill, and the switch row sits below it.
    expect(screen.getByTestId("effort-slider-panel")).toHaveAttribute("data-view", "slider");
    expect(screen.queryByTestId("effort-tier-low")).toBeNull();
    expect(screen.queryByTestId("model-option-opus")).toBeNull();
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max");
    expect(slider).toHaveAttribute("aria-valuemax", "4");
    expect(slider).toHaveAttribute("data-name", "high");
    expect(slider).toHaveAttribute("data-ultracode", "0");
    expect(slider).toHaveAttribute("aria-valuetext", "high");
    expect(screen.getByTestId("effort-title")).toHaveTextContent("high");
    expect(screen.getByTestId("effort-model")).toHaveTextContent("opus");
    expect(screen.getByTestId("effort-knob")).toBeInTheDocument();
    // The orthogonal switch exists below the pill and starts off.
    const sw = screen.getByTestId("effort-ultracode-switch");
    expect(sw).toHaveAttribute("role", "switch");
    expect(sw).toHaveAttribute("aria-checked", "false");
    slider.focus();
    // End lands on the fifth native stop, max — the flag is a separate row.
    await user.keyboard("{End}");
    expect(onEffort).toHaveBeenCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });
  });

  it("the tier name opens a list of five stops and models, and picking one closes it", async () => {
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
    // Five tiers; max carries the restrained top accent; there is NO ultracode
    // tier row (it is the switch on the slider view).
    expect(screen.getByTestId("effort-tier-max")).toHaveAttribute("data-effort-look", "top");
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

  it("max carries the restrained top accent, not the ember (the switch alone ember)", async () => {
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
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-effort-look", "top");
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-ember", "0");
    expect(screen.getByTestId("effort-title")).toHaveAttribute("data-effort-look", "top");
    // The collapsed chip never animates on a plain max.
    expect(screen.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "0");
  });

  it("plain xhigh is not ember; flipping the switch plays ember at the tier and never moves the slider", async () => {
    const user = userEvent.setup();
    const onEffort = vi.fn();
    const onUltracode = vi.fn();
    const { rerender } = render(
      <Composer
        instanceId="ins_ultra"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3)}
        onEffort={onEffort}
        onUltracode={onUltracode}
        ultraGate="decoupled"
      />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const slider = screen.getByTestId("effort-slider");
    expect(slider).toHaveAttribute("data-name", "xhigh");
    expect(slider).toHaveAttribute("data-index", "3");
    expect(slider).toHaveAttribute("data-ember", "0");
    expect(slider).toHaveAttribute("data-ultracode", "0");

    // The arrow keys walk the five tiers only; they never toggle the switch.
    slider.focus();
    await user.keyboard("{ArrowRight}");
    expect(onEffort).toHaveBeenLastCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });
    await user.keyboard("{End}");
    expect(onEffort).toHaveBeenLastCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });

    // The switch flips independently and keeps the slider on xhigh.
    await user.click(screen.getByTestId("effort-ultracode-switch"));
    expect(onUltracode).toHaveBeenCalledWith(true);
    expect(onEffort).not.toHaveBeenCalledWith(expect.objectContaining({ ultracode: true }));

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
        onUltracode={onUltracode}
        ultraGate="decoupled"
      />,
    );
    // Slider stays on xhigh; ember follows the switch state.
    expect(slider).toHaveAttribute("data-name", "xhigh");
    expect(slider).toHaveAttribute("data-index", "3");
    expect(slider).toHaveAttribute("data-ultracode", "1");
    expect(screen.getByTestId("effort-ultracode-switch")).toHaveAttribute("aria-checked", "true");
    // The chip reads the level plus the separate ultracode marker.
    expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("xhigh");
    expect(screen.getByTestId("model-effort-ultracode")).toHaveTextContent(/ultracode/);
    expect(screen.getByTestId("model-effort-chip")).toHaveAttribute("data-ultracode-effective", "on");
    expect(screen.getByTestId("model-effort-chip")).toHaveAttribute("data-ember", "1");
    // Flipping the switch off leaves the tier on xhigh (orthogonal).
    await user.click(screen.getByTestId("effort-ultracode-switch"));
    expect(onUltracode).toHaveBeenLastCalledWith(false);
  });

  it("snaps a pointer drag to the nearest of the five Claude stops", async () => {
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
    // across five stops (4 intervals).
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
    // All the way right = the fifth stop, max (the flag is a separate row).
    fireEvent.pointerDown(slider, { clientX: 396, pointerId: 2, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 396, pointerId: 2, button: 0 });
    expect(onEffort).toHaveBeenCalledWith({ index: 4, name: "max", kind: "claude", ultracode: false });

    onEffort.mockClear();
    // Three quarters lands on xhigh (stop 3), a plain tier.
    fireEvent.pointerDown(slider, { clientX: 291, pointerId: 3, button: 0 });
    fireEvent.pointerUp(slider, { clientX: 291, pointerId: 3, button: 0 });
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
    expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh");
    slider.focus();
    await user.keyboard("{Home}");
    expect(onEffort).toHaveBeenCalledWith({ index: 0, name: "low", kind: "grok" });
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
    // The grok CLI default is `medium` (index 1).
    expect(onEffort).toHaveBeenCalledWith({ index: 1, name: "medium", kind: "grok" });
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
    expect(screen.getByTestId("effort-slider")).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultra");
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

  it("abbreviates the harness label on mobile but keeps it static, inside the options sheet", () => {
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
    // D-042: the read-only harness chip rides inside the options sheet, not
    // the collapsed bar.
    expect(screen.queryByTestId("harness-chip")).toBeNull();
    fireEvent.click(screen.getByTestId("model-effort-chip"));
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

  it("item 7: a read-only codex `never` chip keeps the danger treatment", () => {
    render(
      <Composer
        instanceId="ins_codex_never"
        mobile={false}
        onSend={vi.fn()}
        kind="codex"
        model="gpt"
        effort={{ index: 1, name: "medium", kind: "codex" }}
        permissionMode="never"
      />,
    );
    const chip = screen.getByTestId("permission-chip");
    expect(chip).toHaveAttribute("data-readonly", "1");
    expect(chip).toHaveAttribute("data-permission-danger", "1");
    expect(chip).toHaveTextContent("never");
  });
  it("item 5: the live wheel walks the CLI order and keeps dontAsk out", () => {
    render(
      <Composer
        instanceId="ins_wheel"
        mobile={false}
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2, false)}
        permissionMode="manual"
        launchPermissionMode="manual"
        onPermission={vi.fn()}
      />,
    );
    fireEvent.click(screen.getByTestId("permission-chip"));
    expect(["manual", "acceptEdits", "plan", "auto"].map((id) =>
      screen.getByTestId(`permission-option-${id}`).textContent,
    ).length).toBe(4);
    expect(screen.queryByTestId("permission-option-dontAsk")).toBeNull();
    expect(screen.queryByTestId("permission-option-bypassPermissions")).toBeNull();
  });

});

describe("Composer context usage chip", () => {
  const rollup = {
    contextUsedTokens: 35_839,
    contextWindowTokens: 200_000,
    contextPct: 18,
    sessionInputTokens: 7_856,
    sessionOutputTokens: 589,
    cacheReadTokens: 97_704,
    cacheCreationTokens: 0,
    turns: 3,
    tpmIn60s: 1_223,
    tpmOut60s: 144,
    tpmIn5m: 612,
    tpmOut5m: 66,
    lastTurnAt: new Date().toISOString(),
  };

  beforeEach(() => {
    vi.stubGlobal("requestAnimationFrame", () => 1);
    vi.stubGlobal("cancelAnimationFrame", () => {});
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("drives the ring percentage from the rollup and opens the popover on click", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_rollup" mobile={false} onSend={vi.fn()} usageRollup={rollup} />,
    );
    const chip = screen.getByTestId("context-chip");
    expect(chip).toHaveTextContent("18%");
    expect(chip).toHaveAttribute("data-has-popover", "1");
    expect(chip).toHaveAttribute("aria-expanded", "false");
    const ring = chip.querySelector("[class*='contextRing']") as HTMLElement;
    expect(ring.style.getPropertyValue("--ctx-pct")).toBe("18%");

    await user.click(chip);
    expect(chip).toHaveAttribute("aria-expanded", "true");
    const popover = screen.getByTestId("context-usage-popover");
    expect(popover).toHaveAttribute("data-mobile", "0");
    expect(screen.getByTestId("context-usage-headline")).toHaveTextContent("35.8k/200.0k (18%)");

    // The close affordance dismisses.
    await user.click(screen.getByTestId("context-usage-close"));
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(chip).toHaveAttribute("aria-expanded", "false");
  });

  it("opens the popover on hover for precise pointers and closes on leave", async () => {
    const user = userEvent.setup();
    const matchMedia = vi.fn((query: string) => ({
      matches: query.includes("hover: hover"),
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }));
    vi.stubGlobal("matchMedia", matchMedia);
    render(
      <Composer instanceId="ins_hover" mobile={false} onSend={vi.fn()} usageRollup={rollup} />,
    );
    const chip = screen.getByTestId("context-chip");
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    await user.hover(chip);
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    await user.unhover(chip);
    // Close is debounced 140 ms so the cursor can cross into the panel.
    await vi.waitFor(() =>
      expect(screen.queryByTestId("context-usage-popover")).toBeNull(),
    );
  });

  it("click-pins the panel so pointer leave keeps it open until dismissed", async () => {
    const user = userEvent.setup();
    const matchMedia = vi.fn((query: string) => ({
      matches: query.includes("hover: hover"),
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }));
    vi.stubGlobal("matchMedia", matchMedia);
    render(
      <Composer instanceId="ins_pin" mobile={false} onSend={vi.fn()} usageRollup={rollup} />,
    );
    const chip = screen.getByTestId("context-chip");
    await user.click(chip);
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    await user.unhover(chip);
    // Pinned: a hover leave must not close a panel the click opened.
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    // Pointerdown outside the composer un-pins and closes.
    fireEvent.pointerDown(document.body, { bubbles: true });
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
  });

  // RC2: the context chip's leave timer must never close an open
  // effort/permission menu, and entering the context chip must not steal one.
  function stubFinePointer() {
    vi.stubGlobal(
      "matchMedia",
      vi.fn((query: string) => ({
        matches: query.includes("hover: hover"),
        media: query,
        onchange: null,
        addEventListener: () => {},
        removeEventListener: () => {},
        addListener: () => {},
        removeListener: () => {},
        dispatchEvent: () => false,
      })),
    );
  }

  it("a no-rollup context-chip leave does not close the effort menu (RC2)", async () => {
    // Time is driven explicitly (r3 item 4): fake timers, never wall-clock.
    // Fake only setTimeout/setInterval: the beforeEach rAF stub (noop) must
    // survive, and user-event's pointer choreography waits on the faked
    // clock, so the gestures go through plain fireEvent.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    stubFinePointer();
    render(<Composer instanceId="ins_rc2_e" mobile={false} onSend={vi.fn()} kind="claude" />);
    fireEvent.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("effort-slider-panel")).toBeInTheDocument();
    // Cross/leave the context chip and advance beyond HOVER_CLOSE_DELAY_MS:
    // the scoped leave timer must not dismiss the effort menu.
    fireEvent.mouseLeave(screen.getByTestId("context-chip"));
    act(() => {
      vi.advanceTimersByTime(HOVER_CLOSE_DELAY_MS + 60);
    });
    expect(screen.getByTestId("effort-slider-panel")).toBeInTheDocument();
  });

  it("a no-rollup context-chip leave does not close the permission menu (RC2)", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    stubFinePointer();
    render(
      <Composer
        instanceId="ins_rc2_p"
        mobile={false}
        onSend={vi.fn()}
        onPermission={vi.fn()}
        permissionMode="manual"
      />,
    );
    fireEvent.click(screen.getByTestId("permission-chip"));
    expect(screen.getByTestId("permission-menu")).toBeInTheDocument();
    fireEvent.mouseLeave(screen.getByTestId("context-chip"));
    act(() => {
      vi.advanceTimersByTime(HOVER_CLOSE_DELAY_MS + 60);
    });
    expect(screen.getByTestId("permission-menu")).toBeInTheDocument();
  });

  it("with a rollup, entering the context chip does not steal the effort menu (RC2)", () => {
    stubFinePointer();
    render(
      <Composer
        instanceId="ins_rc2_steal"
        mobile={false}
        onSend={vi.fn()}
        usageRollup={rollup}
        kind="claude"
      />,
    );
    fireEvent.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("effort-slider-panel")).toBeInTheDocument();
    // Hover the context chip: it must not replace the effort menu.
    fireEvent.mouseEnter(screen.getByTestId("context-chip"));
    expect(screen.getByTestId("effort-slider-panel")).toBeInTheDocument();
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
  });

  it("opens an empty-state card with a description when there is no rollup (RC3)", async () => {
    const user = userEvent.setup();
    render(<Composer instanceId="ins_none" mobile={false} onSend={vi.fn()} />);
    const chip = screen.getByTestId("context-chip");
    expect(chip).toHaveTextContent("—");
    // The chip is never dead: it advertises and opens the card.
    expect(chip).toHaveAttribute("data-has-popover", "1");
    expect(chip.getAttribute("aria-label")).toContain("查看明细");
    expect(chip).toHaveAttribute("aria-haspopup", "dialog");
    await user.click(chip);
    const card = await screen.findByTestId("context-usage-popover");
    expect(card).toBeInTheDocument();
    expect(screen.getByTestId("context-usage-empty-note").textContent).toContain("未上报 usage");
  });

  it("renders a sheet on touch widths", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_touch" mobile onSend={vi.fn()} usageRollup={rollup} />,
    );
    // D-042: context usage lives inside the options sheet at touch widths.
    await user.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("context-chip")).toBeInTheDocument();
    await user.click(screen.getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toHaveAttribute("data-mobile", "1");
  });

  it("scrim mousedown on the options sheet closes the stacked usage sheet with its parent (RC4)", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_sheet_usage" mobile onSend={vi.fn()} usageRollup={rollup} />,
    );
    // On mobile the collapsed effort chip opens the options sheet.
    await user.click(screen.getByTestId("model-effort-chip"));
    expect(screen.getByTestId("composer-options-sheet")).toBeVisible();
    // Stack the usage sheet inside.
    await user.click(within(screen.getByTestId("composer-options-sheet")).getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toHaveAttribute("data-mobile", "1");
    // One scrim mousedown dismisses usage WITH ITS PARENT: no orphaned usage
    // popover is left floating after the options sheet closes.
    fireEvent.mouseDown(document.querySelector("[data-variant='sheet']")!);
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();
  });

  it("a single Escape closes the stacked usage sheet with the options sheet (RC4)", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_sheet_usage_esc" mobile onSend={vi.fn()} usageRollup={rollup} />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    await user.click(within(screen.getByTestId("composer-options-sheet")).getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();
  });

  it("the usage panel's own close button returns focus to the context chip and leaves the sheet open (RC4)", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_sheet_usage_x" mobile onSend={vi.fn()} usageRollup={rollup} />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const chip = within(screen.getByTestId("composer-options-sheet")).getByTestId("context-chip");
    await user.click(chip);
    await user.click(screen.getByTestId("context-usage-close"));
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.getByTestId("composer-options-sheet")).toBeVisible();
    expect(chip).toHaveFocus();
  });

  // r2 item 1: EVERY parent-close path must funnel through dismissOptions, so
  // a stacked usage card can never be orphaned.
  it("attach/paste from the sheet closes a stacked usage card with the sheet", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_sheet_paste" mobile onSend={vi.fn()} usageRollup={rollup} />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const sheet = screen.getByTestId("composer-options-sheet");
    await user.click(within(sheet).getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    // The sheet's 粘贴附件 button runs the same close-before-insert path as
    // the file picker (sheetAttachHandlers). Clipboard read rejects in jsdom;
    // the dismiss happens first and must still win.
    await user.click(within(sheet).getByTestId("attach-paste"));
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();
  });

  it("picking a file from the sheet's native picker closes a stacked usage card with the sheet", async () => {
    // r3 item 3: the onFiles half of sheetAttachHandlers is a separate close
    // path from onPasteClick — cover it directly. AttachButtons builds the
    // <input type=file> itself, so stand in for the native chooser by
    // resolving its click() with a file and the change event the handler
    // listens for.
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_sheet_files" mobile onSend={vi.fn()} usageRollup={rollup} />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const sheet = screen.getByTestId("composer-options-sheet");
    await user.click(within(sheet).getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();

    const picked = new File(["hello"], "note.txt", { type: "text/plain" });
    const openPicker = vi
      .spyOn(HTMLInputElement.prototype, "click")
      .mockImplementation(function mockPick(this: HTMLInputElement) {
        Object.defineProperty(
          this,
          "files",
          { value: { 0: picked, length: 1, item: () => picked }, configurable: true },
        );
        this.dispatchEvent(new Event("change", { bubbles: true }));
      });
    try {
      await user.click(within(sheet).getByTestId("attach-file"));
    } finally {
      openPicker.mockRestore();
    }
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();
  });

  it("picking an in-sheet permission clears the usage pin and closes both layers, so a later desktop hover-leave can close the card", () => {
    // r3 item 3: the pin must be made OBSERVABLE. The sheet half closes both
    // layers; the desktop half proves usagePinned was really cleared — a
    // leftover pin makes scheduleHoverClose bail and the hovered card stays.
    // Fake only setTimeout/setInterval: the beforeEach rAF stub (noop) must
    // survive, and user-event's pointer choreography waits on the faked
    // clock, so the gestures go through plain fireEvent (which still runs
    // the real onClick handlers).
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
    stubFinePointer();
    const { rerender } = render(
      <Composer
        instanceId="ins_sheet_perm"
        mobile
        onSend={vi.fn()}
        onPermission={vi.fn()}
        permissionMode="manual"
        usageRollup={rollup}
      />,
    );
    fireEvent.click(screen.getByTestId("model-effort-chip"));
    const sheet = screen.getByTestId("composer-options-sheet");
    // The orphan this path must close: stack the usage card first.
    fireEvent.click(within(sheet).getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    // Pick a reachable permission row (manual is live-reachable for claude)
    // through the real row handler.
    const row = within(sheet)
      .getAllByTestId(/^permission-option-/)
      .find((el) => !el.hasAttribute("disabled"))!;
    expect(row).toBeTruthy();
    fireEvent.click(row);
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();

    // Same instance, now a desktop with a precise pointer: hover-open the
    // usage card unpinned, then leave and run the close delay. If the sheet
    // pick left usagePinned=true behind, scheduleHoverClose bails and the
    // card is still on screen after the delay.
    rerender(
      <Composer
        instanceId="ins_sheet_perm"
        mobile={false}
        onSend={vi.fn()}
        onPermission={vi.fn()}
        permissionMode="manual"
        usageRollup={rollup}
      />,
    );
    const desktopChip = screen.getByTestId("context-chip");
    fireEvent.mouseEnter(desktopChip);
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    fireEvent.mouseLeave(desktopChip);
    act(() => {
      vi.advanceTimersByTime(HOVER_CLOSE_DELAY_MS + 10);
    });
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
  });

  it("the in-sheet effort slider's Escape (model list) closes a stacked usage card with the sheet via the same funnel", async () => {
    // r3 item 3: the orphan must be present for the test to mean anything.
    // The old version never opened the usage layer, and the pre-fix Escape
    // already closed the sheet — so it passed against round 1.
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_sheet_effort_close" mobile onSend={vi.fn()} usageRollup={rollup} kind="claude" />,
    );
    await user.click(screen.getByTestId("model-effort-chip"));
    const sheet = screen.getByTestId("composer-options-sheet");
    // Stack the usage card first, then open the slider's model/tier list.
    await user.click(within(sheet).getByTestId("context-chip"));
    expect(screen.getByTestId("context-usage-popover")).toBeInTheDocument();
    await user.click(within(sheet).getByTestId("effort-open-list"));
    expect(within(sheet).getByTestId("effort-list")).toBeInTheDocument();
    // Its Escape fires the slider's own onClose — the real handler — which
    // must funnel through dismissOptions: popover AND sheet both gone.
    fireEvent.keyDown(within(sheet).getByTestId("effort-list"), { key: "Escape" });
    expect(screen.queryByTestId("context-usage-popover")).toBeNull();
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();
  });
});

describe("Composer held-queue flush (turn-end boundary)", () => {
  it("flushes a Remuda-held row exactly once when a screen-decided end maps working→idle", () => {
    // The multi-channel turn reducer can end a turn from the screen even with
    // no Stop hook; SessionPage maps that decision to composer phase idle. The
    // held prompt must flush on that single edge and never again when a late
    // hook Stop arrives (the decision stays ended, so there is no second edge).
    const onFlushHeld = vi.fn();
    const held = [{ id: "b1", text: "queued prompt", reason: "turn" as const, holder: "remuda" as const }];
    const { rerender } = render(
      <Composer
        instanceId="ins_flush"
        mobile={false}
        onSend={vi.fn()}
        phase="working"
        held={held}
        onFlushHeld={onFlushHeld}
      />,
    );
    expect(onFlushHeld).not.toHaveBeenCalled();

    // The screen decides the end: phase goes idle and the held row flushes.
    rerender(
      <Composer
        instanceId="ins_flush"
        mobile={false}
        onSend={vi.fn()}
        phase="idle"
        held={held}
        onFlushHeld={onFlushHeld}
      />,
    );
    expect(onFlushHeld).toHaveBeenCalledTimes(1);

    // A late hook Stop / repeated render while still idle must not re-flush.
    rerender(
      <Composer
        instanceId="ins_flush"
        mobile={false}
        onSend={vi.fn()}
        phase="idle"
        held={held}
        onFlushHeld={onFlushHeld}
      />,
    );
    expect(onFlushHeld).toHaveBeenCalledTimes(1);
  });
});

/**
 * D-042 (ui-spec.md §2.2 compact composer 边界): on phones the bar collapses
 * to one trigger naming the permission word + effort tier, options ride in a
 * bottom sheet, and both window.confirm calls are Sheet dialogs.
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

describe("Composer mobile options trigger (D-042)", () => {
  it("names the permission word and the effort tier on the collapsed trigger", () => {
    render(
      <Composer
        instanceId="ins_m1"
        mobile
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2)}
        effortEffective={{ name: "high", ultracode: null, source: "remuda", observedAt: "2026-09-19T00:00:00Z" }}
        onPermission={vi.fn()}
      />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    expect(trigger).toHaveAttribute("data-options-trigger", "1");
    expect(screen.getByTestId("composer-trigger-permission")).toHaveTextContent("询问");
    expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("high");
    expect(trigger).toHaveTextContent(/询问.*high/);
    expect(trigger).toHaveAttribute("data-permission", "manual");
    expect(trigger).toHaveAttribute("data-permission-danger", "0");
  });

  it("marks the danger mode on the collapsed trigger, not only inside the sheet", () => {
    render(
      <Composer
        instanceId="ins_m2"
        mobile
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 0)}
        effortEffective={{ name: "low", ultracode: null, source: "remuda", observedAt: "2026-09-19T00:00:00Z" }}
        permissionMode="bypassPermissions"
        launchPermissionMode="bypassPermissions"
        permissionEffective={{ mode: "bypassPermissions", source: "launch", observedAt: "2026-09-19T00:00:00Z" }}
        onPermission={vi.fn()}
      />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    expect(trigger).toHaveAttribute("data-permission", "bypassPermissions");
    expect(trigger).toHaveAttribute("data-permission-danger", "1");
    // The danger word is readable without opening the sheet.
    expect(screen.getByTestId("composer-trigger-permission")).toHaveTextContent("绕过全部");
  });

  it("r2 item 9: collapses only the ultracode suffix when the fused trigger does not fit", () => {
    const stubWidths = (client: number, scroll: number) => {
      const btn = screen.getByTestId("model-effort-chip");
      Object.defineProperty(btn, "clientWidth", { configurable: true, get: () => client });
      Object.defineProperty(btn, "scrollWidth", { configurable: true, get: () => scroll });
      fireEvent(window, new Event("resize"));
    };
    render(
      <Composer
        instanceId="ins_m_ultra_narrow"
        mobile
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3, true)}
        effortEffective={{ name: "xhigh", ultracode: true, source: "remuda", observedAt: "2026-10-06T00:00:00Z" }}
        onPermission={vi.fn()}
      />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    // 132 px box, 150 px content: overflows.
    stubWidths(132, 150);
    expect(trigger).toHaveAttribute("data-collapsed", "1");
    // The permission word and tier are intact; only the suffix collapses.
    expect(screen.getByTestId("composer-trigger-permission")).toHaveTextContent("询问");
    expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("xhigh");
    expect(screen.getByTestId("model-effort-ultracode")).toBeInTheDocument();

    // Room returns: the suffix comes back.
    stubWidths(200, 150);
    expect(trigger).toHaveAttribute("data-collapsed", "0");
  });

  it("r2 item 9: keeps the ultracode suffix when the full content fits at 132 px", () => {
    render(
      <Composer
        instanceId="ins_m_ultra_fit"
        mobile
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 3, true)}
        effortEffective={{ name: "xhigh", ultracode: true, source: "remuda", observedAt: "2026-10-06T00:00:00Z" }}
        onPermission={vi.fn()}
      />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    Object.defineProperty(trigger, "clientWidth", { configurable: true, get: () => 132 });
    Object.defineProperty(trigger, "scrollWidth", { configurable: true, get: () => 128 });
    fireEvent(window, new Event("resize"));
    expect(trigger).toHaveAttribute("data-collapsed", "0");
    expect(screen.getByTestId("composer-trigger-permission")).toHaveTextContent("询问");
    expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("xhigh");
  });

  it("keeps the option-only controls in the sheet and the primary control outside", async () => {
    const user = userEvent.setup();
    render(
      <Composer
        instanceId="ins_m3"
        mobile
        onSend={vi.fn()}
        kind="claude"
        model="opus"
        effort={effortAt("claude", 2)}
        onPermission={vi.fn()}
      />,
    );
    // Collapsed: no attach/harness/context/permission picker/effort slider in
    // the bar (context usage belongs in the sheet per dispatch plan §C-4).
    expect(screen.queryByTestId("composer-options-sheet")).toBeNull();
    expect(screen.queryByTestId("attach-file")).toBeNull();
    expect(screen.queryByTestId("harness-chip")).toBeNull();
    expect(screen.queryByTestId("context-chip")).toBeNull();
    expect(screen.queryByTestId("permission-option-manual")).toBeNull();
    expect(screen.queryByTestId("effort-slider")).toBeNull();
    // The three-state primary stays outside (D-028a).
    expect(screen.getByTestId("composer-send")).toBeVisible();

    await user.click(screen.getByTestId("model-effort-chip"));
    const sheet = screen.getByTestId("composer-options-sheet");
    expect(sheet).toHaveAttribute("data-variant", "sheet");
    const sheetBody = within(sheet);
    expect(sheetBody.getByTestId("attach-file")).toBeVisible();
    expect(sheetBody.getByTestId("attach-camera")).toBeVisible();
    expect(sheetBody.getByTestId("attach-paste")).toBeVisible();
    expect(sheetBody.getByTestId("harness-chip")).toBeVisible();
    expect(sheetBody.getByTestId("context-chip")).toBeVisible();
    expect(sheetBody.getByTestId("permission-option-manual")).toBeVisible();
    expect(sheetBody.getByTestId("effort-slider")).toBeVisible();
    // The primary control is not duplicated into the sheet.
    expect(sheet.querySelector("[data-testid='composer-send']")).toBeNull();
  });

  it("closing the options sheet returns focus to the collapsed trigger", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_focus" mobile onSend={vi.fn()} kind="claude" effort={effortAt("claude", 2)} onPermission={vi.fn()} />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    await user.click(trigger);
    const sheet = screen.getByTestId("composer-options-sheet");
    await user.click(screen.getByTestId("composer-options-close"));
    expect(sheet).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("attaching a file from the sheet closes it first and returns focus to the trigger", async () => {
    const user = userEvent.setup();
    render(
      <Composer instanceId="ins_attachfocus" mobile onSend={vi.fn()} kind="claude" effort={effortAt("claude", 2)} />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    await user.click(trigger);
    const sheet = screen.getByTestId("composer-options-sheet");
    // The sheet attach buttons are non-file buttons (paste); picking one
    // exercises the close-first handler without a real file chooser.
    await user.click(within(sheet).getByTestId("attach-paste"));
    // Paste with an empty clipboard adds nothing, but the sheet must close
    // before the (no-op) handler runs so focus never hits the textarea
    // behind the aria-modal scrim.
    expect(sheet).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("names the permission word and a danger marker in the trigger aria-label", async () => {
    render(
      <Composer
        instanceId="ins_aria"
        mobile
        onSend={vi.fn()}
        kind="claude"
        effort={effortAt("claude", 2)}
        permissionMode="bypassPermissions"
        launchPermissionMode="bypassPermissions"
        permissionEffective={{ mode: "bypassPermissions", source: "launch", observedAt: "2026-09-19T00:00:00Z" }}
        onPermission={vi.fn()}
      />,
    );
    const trigger = screen.getByTestId("model-effort-chip");
    expect(trigger.getAttribute("aria-label")).toMatch(/绕过全部/);
    expect(trigger.getAttribute("aria-label")).toMatch(/危险/);
    expect(screen.getByTestId("composer-trigger-permission")).toHaveTextContent("绕过全部");
  });

  it("dismisses an open confirm when the turn ends before the user confirms", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    const { rerender } = render(
      <Composer
        instanceId="ins_turnends"
        mobile={false}
        onSend={onSend}
        kind="claude"
        effort={effortAt("claude", 2)}
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    await user.type(screen.getByTestId("composer-input"), "jump now");
    // Open the steer confirm via the visible 插队 button but do not confirm.
    await user.click(screen.getByTestId("composer-steer"));
    expect(screen.getByTestId("composer-confirm-title")).toHaveTextContent("插队发送");
    // The turn ends while the dialog is open.
    rerender(
      <Composer
        instanceId="ins_turnends"
        mobile={false}
        onSend={onSend}
        kind="claude"
        effort={effortAt("claude", 2)}
        phase="idle"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    expect(screen.queryByTestId("composer-confirm")).toBeNull();
    expect(onSend).not.toHaveBeenCalled();
  });

  it("does not post a steer when ok is clicked after live controls lose steer", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    const { rerender } = render(
      <Composer
        instanceId="ins_late"
        mobile={false}
        onSend={onSend}
        kind="claude"
        effort={effortAt("claude", 2)}
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    await user.type(screen.getByTestId("composer-input"), "jump now");
    await user.click(screen.getByTestId("composer-steer"));
    const dialog = screen.getByTestId("composer-confirm");
    // The dialog stays mounted (phase is still "working"), but the live
    // controls snapshot loses the interrupt/steer capability (the turn's
    // state moved underneath the open dialog — steer derives from the
    // interrupt key). Clicking ok must re-check the ref and refuse to post.
    rerender(
      <Composer
        instanceId="ins_late"
        mobile={false}
        onSend={onSend}
        kind="claude"
        effort={effortAt("claude", 2)}
        phase="working"
        capabilities={caps({ interrupt: cap("unsupported"), queue: cap() })}
      />,
    );
    expect(dialog).toBeInTheDocument();
    await user.click(screen.getByTestId("composer-confirm-ok"));
    expect(onSend).not.toHaveBeenCalled();
  });

  it("does not call onInterrupt when ok is clicked after interrupt becomes unsupported", async () => {
    const user = userEvent.setup();
    const onInterrupt = vi.fn();
    const { rerender } = render(
      <Composer
        instanceId="ins_intlate"
        mobile={false}
        onSend={vi.fn()}
        onInterrupt={onInterrupt}
        kind="claude"
        effort={effortAt("claude", 2)}
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    // Open the Esc 打断 confirm.
    await user.click(screen.getByTestId("composer-input"));
    await user.keyboard("{Escape}");
    const dialog = screen.getByTestId("composer-confirm");
    expect(dialog).toBeInTheDocument();
    expect(screen.getByTestId("composer-confirm-title")).toHaveTextContent("打断当前 turn");
    // Phase stays working (so the auto-dismiss effect does NOT fire), but the
    // live snapshot loses the interrupt capability. Clicking ok must re-check
    // the ref and not cancel.
    rerender(
      <Composer
        instanceId="ins_intlate"
        mobile={false}
        onSend={vi.fn()}
        onInterrupt={onInterrupt}
        kind="claude"
        effort={effortAt("claude", 2)}
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap("unsupported"), queue: cap() })}
      />,
    );
    expect(dialog).toBeInTheDocument();
    await user.click(screen.getByTestId("composer-confirm-ok"));
    expect(onInterrupt).not.toHaveBeenCalled();
  });

  it("uses a plain placeholder on phones and keeps the shortcut hint on desktop", () => {
    const { rerender } = render(
      <Composer instanceId="ins_m4" mobile onSend={vi.fn()} />,
    );
    expect(screen.getByTestId("composer-input")).toHaveAttribute("placeholder", "输入提示词…");
    rerender(<Composer instanceId="ins_m4" mobile={false} onSend={vi.fn()} />);
    expect(screen.getByTestId("composer-input").getAttribute("placeholder")).toContain("Enter 排队");
  });
});

describe("Composer Sheet confirms replace window.confirm (D-042)", () => {
  it("steer: cancel keeps the draft and sends nothing; ok POSTs mode=steer", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn();
    const onInterrupt = vi.fn();
    render(
      <Composer
        instanceId="ins_c1"
        mobile={false}
        onSend={onSend}
        onInterrupt={onInterrupt}
        kind="claude"
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    await user.click(screen.getByTestId("composer-input"));
    await user.keyboard("jump now");
    await user.click(screen.getByTestId("composer-steer"));
    const dialog = screen.getByTestId("composer-confirm");
    expect(dialog).toHaveAttribute("data-variant", "popover");
    expect(screen.getByTestId("composer-confirm-title")).toHaveTextContent("插队发送");
    expect(onSend).not.toHaveBeenCalled();

    await user.click(screen.getByTestId("composer-confirm-cancel"));
    expect(screen.queryByTestId("composer-confirm")).toBeNull();
    expect(onSend).not.toHaveBeenCalled();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("jump now");

    await user.click(screen.getByTestId("composer-steer"));
    await user.click(screen.getByTestId("composer-confirm-ok"));
    expect(onSend).toHaveBeenCalledTimes(1);
    expect(onSend.mock.calls[0][0]).toBe("jump now");
    expect(onSend.mock.calls[0][3]).toBe("steer");
    expect(onInterrupt).not.toHaveBeenCalled();
  });

  it("Esc interrupt: cancel leaves the turn running; ok calls onInterrupt", async () => {
    const user = userEvent.setup();
    const onInterrupt = vi.fn();
    render(
      <Composer
        instanceId="ins_c2"
        mobile={false}
        onSend={vi.fn()}
        onInterrupt={onInterrupt}
        kind="claude"
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    await user.click(screen.getByTestId("composer-input"));
    await user.keyboard("{Escape}");
    expect(screen.getByTestId("composer-confirm-title")).toHaveTextContent("打断当前 turn");
    // Esc inside the dialog is cancel (focus trap).
    await user.keyboard("{Escape}");
    expect(screen.queryByTestId("composer-confirm")).toBeNull();
    expect(onInterrupt).not.toHaveBeenCalled();

    await user.keyboard("{Escape}");
    await user.click(screen.getByTestId("composer-confirm-ok"));
    expect(onInterrupt).toHaveBeenCalledTimes(1);
  });

  it("uses the sheet variant on phones and returns focus to the steer trigger", async () => {
    const user = userEvent.setup();
    render(
      <Composer
        instanceId="ins_c3"
        mobile
        onSend={vi.fn()}
        kind="claude"
        phase="working"
        capabilities={caps({ steer: cap(), interrupt: cap(), queue: cap() })}
      />,
    );
    await user.click(screen.getByTestId("composer-input"));
    await user.keyboard("jump now");
    await user.click(screen.getByTestId("composer-steer"));
    expect(screen.getByTestId("composer-confirm")).toHaveAttribute("data-variant", "sheet");
    await user.click(screen.getByTestId("composer-confirm-cancel"));
    expect(screen.getByTestId("composer-steer")).toHaveFocus();
  });
});

it("shows ? while an effort switch is pending and read-back has been withdrawn", () => {
  render(
    <Composer
      instanceId="ins_withdrawn_pending"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      model="opus"
      effort={effortAt("claude", 4, false)}
      effortEffective={null}
      effortReadbackWithdrawn
      effortPending={{ word: "max", queued: false }}
      onEffort={vi.fn()}
    />,
  );
  const chip = screen.getByTestId("model-effort-chip");
  expect(chip).toHaveAttribute("data-effort-effective", "unknown");
  expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("?");
  // The withdrawn title is specific (not the generic waiting title).
  expect(chip).toHaveAttribute("title", expect.stringContaining("回读不可用"));
  // The pending tag is still rendered (the switch outcome is awaited).
  expect(screen.getByTestId("model-effort-pending")).toHaveTextContent("切换中");
});

it("shows the pending word when a pending switch has simply never read back", () => {
  // r7 item 4(a): a null effective with NO readbackAvailable:false edge is
  // "not read back yet", not "withdrawn": the chip keeps the pending word
  // and must not show the withdrawn title.
  render(
    <Composer
      instanceId="ins_never_read_pending"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      model="opus"
      effort={effortAt("claude", 4, false)}
      effortEffective={null}
      effortPending={{ word: "max", queued: false }}
      onEffort={vi.fn()}
    />,
  );
  const chip = screen.getByTestId("model-effort-chip");
  expect(chip).toHaveAttribute("data-effort-effective", "pending");
  expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("max");
  expect(chip).not.toHaveAttribute("title", expect.stringContaining("回读不可用"));
});

it("shows the pending word when read-back is still available during a switch", () => {
  render(
    <Composer
      instanceId="ins_normal_pending"
      mobile={false}
      onSend={vi.fn()}
      kind="claude"
      model="opus"
      effort={effortAt("claude", 4, false)}
      effortEffective={{ name: "high", ultracode: false, source: "remuda", observedAt: "2026-10-08T00:00:00Z" }}
      effortPending={{ word: "max", queued: false }}
      onEffort={vi.fn()}
    />,
  );
  const chip = screen.getByTestId("model-effort-chip");
  expect(chip).toHaveAttribute("data-effort-effective", "pending");
  expect(screen.getByTestId("model-effort-chip-label")).toHaveTextContent("max");
});
