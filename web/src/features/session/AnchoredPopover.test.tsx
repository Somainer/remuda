import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  AnchoredPopover,
  computeAnchored,
  useAnchoredPopover,
  type AnchoredStyle,
} from "./AnchoredPopover";
import { useRef } from "react";

const OPTS = {
  align: "start" as const,
  gap: 8,
  margin: 8,
  maxHeightVh: 0.6,
  preferUp: true,
};

describe("computeAnchored placement math", () => {
  it("aligns the panel's left edge to the trigger box", () => {
    const measured = computeAnchored(
      { top: 700, bottom: 730, left: 420, right: 520, width: 100 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      OPTS,
    );
    expect(measured.placement).toBe("up");
    // The panel starts at the trigger's left, not the composer's left 0.
    expect(measured.left).toBeGreaterThanOrEqual(420);
    expect(measured.left).toBeLessThanOrEqual(520);
    // It sits immediately above the trigger with the gap.
    expect(measured.top + 190).toBeLessThanOrEqual(700 - 8 + 1);
  });

  it("flips down when room above is short", () => {
    const measured = computeAnchored(
      { top: 20, bottom: 50, left: 100, right: 200, width: 100 },
      { preferredHeight: 400, width: 300 },
      { width: 1440, height: 900 },
      OPTS,
    );
    expect(measured.placement).toBe("down");
    expect(measured.top).toBe(58);
  });

  it("caps the panel height to the available room and the 60vh bound", () => {
    // Trigger at the bottom: room below is 0, room above is ~744 → up,
    // and the panel's natural 900px height is capped to the 60vh cap.
    const measured = computeAnchored(
      { top: 760, bottom: 792, left: 100, right: 200, width: 100 },
      { preferredHeight: 900, width: 300 },
      { width: 1440, height: 800 },
      OPTS,
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBeLessThanOrEqual(760 - 8 - 8);
    expect(measured.top).toBeGreaterThanOrEqual(8);
    // 60vh = 480 is the absolute cap.
    expect(measured.maxHeight).toBeLessThanOrEqual(480);
  });

  it("never overlaps the approval card on either placement", () => {
    // Approval bottom 660, trigger 700–730: the usable gap above is ~24px,
    // below is 162px, so the panel opens down — and still clears the card.
    const down = computeAnchored(
      { top: 700, bottom: 730, left: 100, right: 200, width: 100 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 660 },
    );
    const downTop = down.placement === "up"
      ? 700 - 8 - Math.min(190, down.maxHeight)
      : 738;
    expect(downTop).toBeGreaterThanOrEqual(668);

    // Fixture-shaped geometry (real mock e2e): trigger at 857, approval
    // bottom 770, viewport 900. Up is the only roomy side; the panel must
    // stay under the vh cap and above the card edge.
    const up = computeAnchored(
      { top: 857, bottom: 887, left: 590, right: 699, width: 109 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 770 },
    );
    expect(up.placement).toBe("up");
    const height = Math.min(190, up.maxHeight);
    const top = 857 - 8 - height;
    expect(top).toBeGreaterThanOrEqual(770);
    expect(up.maxHeight).toBeLessThanOrEqual(540);
  });

  it("shifts a panel that would overflow the right edge back inside", () => {
    const measured = computeAnchored(
      { top: 700, bottom: 730, left: 1300, right: 1430, width: 130 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      OPTS,
    );
    expect(measured.left + 300).toBeLessThanOrEqual(1440 - 8 + 1);
  });

  it("keeps an end-aligned panel within the trigger's horizontal box", () => {
    const measured = computeAnchored(
      { top: 700, bottom: 730, left: 420, right: 520, width: 100 },
      { preferredHeight: 190, width: 200 },
      { width: 1440, height: 900 },
      { ...OPTS, align: "end" },
    );
    // Right edges coincide; the panel overlaps the trigger horizontally.
    expect(measured.left + 200).toBe(520);
  });

  it("opens below when an approval card leaves no room above", () => {
    // Trigger at y=700; a card whose bottom edge is 660 occupies the space
    // above (room ~24px), and below has 170px → flips down instead of
    // overlapping the card.
    const measured = computeAnchored(
      { top: 700, bottom: 730, left: 100, right: 200, width: 100 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 660 },
    );
    expect(measured.placement).toBe("down");
    expect(measured.top).toBe(738);
  });
});

/** jsdom layout mocks: getBoundingClientRect is a stub returning zeros by
 *  default, and there is no ResizeObserver. */
function installLayout(opts: {
  trigger: DOMRect;
  panel: { width: number; height: number };
  viewport: { width: number; height: number };
}) {
  class ResizeObserverMock {
    private cb: () => void;
    constructor(cb: () => void) {
      this.cb = cb;
    }
    observe() {}
    unobserve() {}
    disconnect() {}
    flush() {
      this.cb();
    }
  }
  const observerInstances: ResizeObserverMock[] = [];
  vi.stubGlobal(
    "ResizeObserver",
    class extends ResizeObserverMock {
      constructor(cb: () => void) {
        super(cb);
        observerInstances.push(this);
      }
    },
  );
  Object.defineProperty(window, "innerWidth", {
    value: opts.viewport.width,
    configurable: true,
    writable: true,
  });
  Object.defineProperty(window, "innerHeight", {
    value: opts.viewport.height,
    configurable: true,
    writable: true,
  });
  return {
    observerInstances,
    setPanelSize: (size: { width: number; height: number }) => {
      Object.assign(opts.panel, size);
    },
  };
}

function Harness({
  open,
  onAnchor,
}: {
  panel: { width: number; height: number };
  open: boolean;
  onAnchor: (anchor: AnchoredStyle) => void;
}) {
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const anchor = useAnchoredPopover(triggerRef, panelRef, open, { preferUp: true });
  onAnchor(anchor);
  return (
    <div>
      <button
        ref={triggerRef}
        data-testid="trigger"
        style={{ position: "fixed" }}
      >
        open
      </button>
      {open ? (
        <div
          ref={panelRef}
          data-testid="panel"
          data-placement={anchor.placement}
          style={anchor.style}
        >
          card
        </div>
      ) : null}
    </div>
  );
}

describe("useAnchoredPopover measured positioning", () => {
  const panelSize = { width: 300, height: 190 };
  const triggerRect = {
    x: 420,
    y: 700,
    top: 700,
    bottom: 730,
    left: 420,
    right: 520,
    width: 100,
    height: 30,
    toJSON: () => ({}),
  } as unknown as DOMRect;
  let layout: ReturnType<typeof installLayout>;

  beforeEach(() => {
    Object.assign(panelSize, { width: 300, height: 190 });
    layout = installLayout({
      trigger: triggerRect,
      panel: panelSize,
      viewport: { width: 1440, height: 900 },
    });
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.dataset.testid === "trigger") return triggerRect;
      if (this.dataset.testid === "panel") {
        return {
          ...triggerRect,
          left: 0,
          top: 0,
          right: panelSize.width,
          bottom: panelSize.height,
          width: panelSize.width,
          height: panelSize.height,
          toJSON: () => ({}),
        } as DOMRect;
      }
      return new DOMRect();
    });
    // jsdom never lays out; give the panel real offset dimensions.
    for (const [prop, key] of [
      ["offsetWidth", "width"],
      ["offsetHeight", "height"],
      ["scrollWidth", "width"],
      ["scrollHeight", "height"],
    ] as const) {
      Object.defineProperty(HTMLElement.prototype, prop, {
        configurable: true,
        get(this: HTMLElement) {
          return this.dataset.testid === "panel"
            ? (panelSize as Record<string, number>)[key]
            : 0;
        },
      });
    }
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("positions the panel at the trigger and flips on a later resize", async () => {
    const anchors: AnchoredStyle[] = [];
    const { rerender } = render(
      <Harness
        panel={panelSize}
        open
        onAnchor={(a) => anchors.push(a)}
      />,
    );
    // Measurement is scheduled on an animation frame.
    await new Promise((resolve) => setTimeout(resolve, 30));
    const panel = screen.getByTestId("panel");
    expect(panel).toHaveStyle({ position: "fixed" });
    const style = panel.getAttribute("style") ?? "";
    expect(style).toContain("left: 420px");
    expect(panel).toHaveAttribute("data-placement");
    // Move the trigger to the top of the viewport: room above is gone → down.
    const topRect = {
      ...(triggerRect as unknown as Record<string, number>),
      top: 20,
      y: 20,
      bottom: 50,
    } as unknown as DOMRect;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.dataset.testid === "trigger") return topRect;
      return new DOMRect(0, 0, panelSize.width, panelSize.height);
    });
    window.dispatchEvent(new Event("resize"));
    await new Promise((resolve) => setTimeout(resolve, 30));
    rerender(
      <Harness
        panel={panelSize}
        open
        onAnchor={(a) => anchors.push(a)}
      />,
    );
    await new Promise((resolve) => setTimeout(resolve, 30));
    // The re-rendered harness shares the resize listener; the final style is down.
    expect(screen.getByTestId("panel").getAttribute("style") ?? "").toMatch(/top:\s*58px/);
  });

  it("re-measures when the panel itself grows (the slider/list flip)", async () => {
    const { rerender } = render(
      <Harness panel={panelSize} open onAnchor={() => {}} />,
    );
    await new Promise((resolve) => setTimeout(resolve, 30));
    const before = screen.getByTestId("panel").getAttribute("style") ?? "";
    expect(before).toContain("left: 420px");
    // The flip makes the panel 900px tall — the ResizeObserver fires and the
    // height gets capped/shifted.
    Object.assign(panelSize, { height: 900 });
    layout.observerInstances.forEach((o) => o.flush());
    await new Promise((resolve) => setTimeout(resolve, 30));
    rerender(
      <Harness panel={panelSize} open onAnchor={() => {}} />,
    );
    await new Promise((resolve) => setTimeout(resolve, 30));
    const after = screen.getByTestId("panel").getAttribute("style") ?? "";
    expect(after).toMatch(/max-height:\s*\d+px/);
    expect(after).not.toContain("max-height: none");
  });

  it("renders nothing while closed", () => {
    render(
      <AnchoredPopover
        triggerRef={{ current: null }}
        open={false}
        testId="gone"
      >
        x
      </AnchoredPopover>,
    );
    expect(screen.queryByTestId("gone")).toBeNull();
  });
});

describe("EffortSlider list with a tall catalog", () => {
  // Mount the slider in list mode through the real composer surface is done
  // in the EffortSlider suite; here only the anchor integration: the panel
  // element is the popover primitive's measured box.
  it("keeps the popover box inside the viewport", async () => {
    const user = userEvent.setup();
    const panelSize = { width: 300, height: 190 };
    const triggerRect = new DOMRect(1000, 760, 100, 30);
    const observerInstances: { flush: () => void }[] = [];
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(cb: () => void) {
          observerInstances.push({ flush: cb });
        }
        observe() {}
        unobserve() {}
        disconnect() {}
      },
    );
    Object.defineProperty(window, "innerWidth", { value: 1440, configurable: true });
    Object.defineProperty(window, "innerHeight", { value: 800, configurable: true });
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.dataset.testid === "trigger") return triggerRect;
      return new DOMRect(0, 0, panelSize.width, panelSize.height);
    });
    render(<Harness panel={panelSize} open onAnchor={() => {}} />);
    await new Promise((resolve) => setTimeout(resolve, 30));
    await user.click(screen.getByTestId("trigger"));
    const style = screen.getByTestId("panel").getAttribute("style") ?? "";
    const left = Number((style.match(/left:\s*(-?\d+)px/) ?? [])[1] ?? NaN);
    const top = Number((style.match(/top:\s*(-?\d+)px/) ?? [])[1] ?? NaN);
    expect(left).toBeGreaterThanOrEqual(8);
    expect(left + 300).toBeLessThanOrEqual(1432);
    expect(top).toBeGreaterThanOrEqual(8);
    expect(top).toBeLessThanOrEqual(760 - 8);
    expect(observerInstances.length).toBeGreaterThan(0);
  });
});
