import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  computeAnchored,
  useAnchoredPopover,
  POPOVER_DOWN_USABLE_PX,
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

  it("flips below when an approval card blocks above and below is usable", () => {
    // Trigger 500–530 in a 900 viewport: card bottom 460 leaves only 24px
    // cleared above, but 354px below (>= 160 floor) → dodge down.
    const down = computeAnchored(
      { top: 500, bottom: 530, left: 100, right: 200, width: 100 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 460 },
    );
    expect(down.placement).toBe("down");
    expect(down.top).toBe(538);
  });

  it("opens above when the panel clears a card that only reaches partway", () => {
    // Trigger 700–730 in a 900 viewport: 684px above, 154px below. A card
    // ending at 450 leaves 234px of cleared room above — enough for the 190px
    // panel, so it opens up and clears the card.
    const up = computeAnchored(
      { top: 700, bottom: 730, left: 100, right: 200, width: 100 },
      { preferredHeight: 190, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 450 },
    );
    expect(up.placement).toBe("up");
    expect(up.top).toBe(700 - 8 - 190);
    expect(up.top).toBeGreaterThanOrEqual(458);
  });

  it("stays up when a card blocks up and below cannot host a usable panel", () => {
    // Composer docked at the bottom (no usable room below) with a parked
    // card: opening down would push controls off-screen, so stay up. The
    // card-cleared strip here (71px) is UNDER the 120px usable minimum, so the
    // panel falls back to the bounded plain above-room cap (it may overlap the
    // dismissible card) rather than clipping its controls out of reach.
    const measured = computeAnchored(
      { top: 857, bottom: 887, left: 590, right: 699, width: 109 },
      { preferredHeight: 131, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 770 },
    );
    expect(measured.placement).toBe("up");
    expect(measured.top).toBeLessThan(857);
    expect(measured.maxHeight).toBe(540); // min(841 plain above-room, 60vh)
  });

  it("r10 item 10: the cleared strip counts BOTH gaps; a one-gap 112px is the narrow fallback, not a usable 120", () => {
    // The composer-effort e2e used to compute trigger.y − card.bottom − 8 and
    // call 112px "usable" (857 − 737 − 8 = 112... with its gap bookkeeping it
    // compared 857 − 729 − 8 = 120). Production keeps gap on BOTH sides:
    // upperBoundary = card.bottom + 8, strip = trigger.y − 8 − that, i.e.
    // 857 − 8 − (729 + 8) = 112, UNDER the 120 minimum → bounded fallback.
    const measured = computeAnchored(
      { top: 857, bottom: 887, left: 590, right: 699, width: 109 },
      { preferredHeight: 131, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 729 },
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBe(540);
    // Boundary one millimetre taller clears: 857 − 8 − (721 + 8) = 120.
    const atMinimum = computeAnchored(
      { top: 857, bottom: 887, left: 590, right: 699, width: 109 },
      { preferredHeight: 131, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 721 },
    );
    expect(atMinimum.maxHeight).toBe(POPOVER_DOWN_USABLE_PX);
  });

  it("item 5(a): a card-cleared strip AT the usable minimum keeps the clearance", () => {
    // A 120px cleared strip is usable, so the panel caps EXACTLY to it and its
    // bottom edge clears the card (non-overlap geometry, composer-effort e2e
    // class). Trigger 857, card bottom 721: 857 − 8 − (721+8) = 120.
    const measured = computeAnchored(
      { top: 857, bottom: 887, left: 590, right: 699, width: 109 },
      { preferredHeight: 131, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 721 },
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBe(120);
    expect(measured.top).toBe(857 - 8 - 120);
  });

  it("item 12b: a literal 0 cleared strip is bounded, never rendered uncapped", () => {
    // A card reaches the trigger itself (no cleared room above) with no room
    // below either: stay up, but the 0 must NOT become an absent maxHeight
    // (which painted a full-vh panel); it falls back to the bounded plain
    // above-room (and the 60vh absolute cap).
    const measured = computeAnchored(
      { top: 857, bottom: 887, left: 100, right: 200, width: 100 },
      { preferredHeight: 206, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 857 },
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBeDefined();
    expect(measured.maxHeight).toBe(540); // min(841 plain above-room, 60vh)
  });

  it("r9 item 12: a tiny plain (no-card) up strip shrinks to the room and clears the trigger", () => {
    // Dock near the bottom (dodging DOWN is not an option) in a short viewport
    // with NO parked card: plain above-room is only ~40px. The panel must
    // SHRINK to that room (never grow to a usable floor that covers the
    // trigger); its bottom edge stays above the trigger top.
    const measured = computeAnchored(
      { top: 56, bottom: 86, left: 100, right: 200, width: 100 },
      { preferredHeight: 300, width: 300 },
      { width: 600, height: 100 },
      OPTS,
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBe(40); // trigger.top − gap − margin
    const panelHeight = Math.min(300, measured.maxHeight ?? 300);
    expect(measured.top + panelHeight).toBeLessThanOrEqual(56);
  });

  it("r9 item 12: a sub-120 plain strip in a tall viewport also stays bounded by the room", () => {
    // Dock near the bottom of a 900px viewport (below ~5px, unusable): the
    // plain above-room is 100px. The panel caps to 100px and its bottom edge
    // clears the trigger rather than growing to a 120px floor over it.
    const measured = computeAnchored(
      { top: 116, bottom: 892, left: 100, right: 200, width: 100 },
      { preferredHeight: 300, width: 300 },
      { width: 1440, height: 900 },
      OPTS,
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBe(100);
    const panelHeight = Math.min(300, measured.maxHeight ?? 300);
    expect(measured.top + panelHeight).toBeLessThanOrEqual(116);
  });

  it("item 5(a): a 40–120px plain strip is bounded by the 60vh cap when smaller", () => {
    // Trigger low with below unusable (12px): the plain above-room is 100px;
    // the 60vh cap (99px in a 166px viewport) is the binding bound.
    const measured = computeAnchored(
      { top: 116, bottom: 146, left: 100, right: 200, width: 100 },
      { preferredHeight: 300, width: 300 },
      { width: 1440, height: 166 },
      OPTS,
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBe(99); // 60vh of 166 = 99
  });

  it("r5 item 5: a short up panel is bounded within the above room and stays on-screen", () => {
    // Trigger top BELOW 136 and below unusable (only 5px) so it cannot flip
    // down: the panel shrinks to the above room; its top stays at/above the
    // margin and it never covers the trigger.
    const measured = computeAnchored(
      { top: 100, bottom: 130, left: 100, right: 200, width: 100 },
      { preferredHeight: 300, width: 300 },
      { width: 1440, height: 143 },
      OPTS,
    );
    expect(measured.placement).toBe("up");
    expect(measured.top).toBeGreaterThanOrEqual(8);
    const panelHeight = Math.min(300, measured.maxHeight ?? 300);
    expect(measured.top + panelHeight).toBeLessThanOrEqual(100);
  });

  it("r5 item 5: a trigger with room below flips down instead of clamping an off-screen up panel", () => {
    // Same low trigger but room BELOW the trigger: placement flips down.
    const measured = computeAnchored(
      { top: 100, bottom: 130, left: 100, right: 200, width: 100 },
      { preferredHeight: 300, width: 300 },
      // 300px below vs 92px above.
      { width: 1440, height: 600 },
      OPTS,
    );
    expect(measured.placement).toBe("down");
  });

  it("item 5(a): a sub-minimum card-cleared strip falls back to plain above-room", () => {
    // Dock with a narrow 71px cleared strip: below the usable minimum, so the
    // panel uses the bounded plain above-room (covering the dismissible card
    // is the documented lesser evil) and its controls stay reachable.
    const measured = computeAnchored(
      { top: 857, bottom: 887, left: 590, right: 699, width: 109 },
      { preferredHeight: 131, width: 300 },
      { width: 1440, height: 900 },
      { ...OPTS, avoidBottom: 770 },
    );
    expect(measured.placement).toBe("up");
    expect(measured.maxHeight).toBe(540);
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

function HarnessAvoid({
  open,
  avoid,
  onAnchor,
}: {
  open: boolean;
  avoid: { current: HTMLElement };
  onAnchor: (anchor: AnchoredStyle) => void;
}) {
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const anchor = useAnchoredPopover(triggerRef, panelRef, open, {
    preferUp: true,
    avoidElements: [avoid],
  });
  onAnchor(anchor);
  return (
    <div>
      <button ref={triggerRef} data-testid="trigger" style={{ position: "fixed" }}>
        open
      </button>
      {open ? (
        <div ref={panelRef} data-testid="panel" data-placement={anchor.placement} style={anchor.style}>
          <div data-popover-scroll="1" data-testid="panel-body">
            card
          </div>
        </div>
      ) : null}
    </div>
  );
}

// Panel > flex-column scroll wrapper > a nested scroll REGION (the effort
// popover: popover > [data-popover-scroll] > slider card > effort-list body).
function HarnessNestedRegion({
  open,
  avoid,
  onAnchor,
}: {
  open: boolean;
  avoid: { current: HTMLElement };
  onAnchor: (anchor: AnchoredStyle) => void;
}) {
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const anchor = useAnchoredPopover(triggerRef, panelRef, open, {
    preferUp: true,
    avoidElements: [avoid],
  });
  onAnchor(anchor);
  return (
    <div>
      <button ref={triggerRef} data-testid="trigger" style={{ position: "fixed" }}>
        open
      </button>
      {open ? (
        <div ref={panelRef} data-testid="panel" data-placement={anchor.placement} style={anchor.style}>
          <div data-popover-scroll="1" data-testid="panel-body">
            <div className="list-head">head</div>
            <div data-popover-scroll-region="1" data-testid="panel-region">
              tall catalog
            </div>
          </div>
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

  it("stops scheduling animation frames once the trigger box settles", async () => {
    // Regression: the trigger tracker ran a perpetual requestAnimationFrame
    // loop (and a max-height toggle re-measured on every cycle), keeping the
    // panel moving forever — Playwright actionability saw its buttons as
    // unstable. Assert frame SCHEDULING stops after the settle budget; a
    // constant mocked trigger is not enough, the tracker itself must stop.
    const raf = vi
      .spyOn(window, "requestAnimationFrame")
      .mockImplementation((cb: FrameRequestCallback) => {
        return setTimeout(() => cb(performance.now()), 0) as unknown as number;
      });
    const caf = vi
      .spyOn(window, "cancelAnimationFrame")
      .mockImplementation((id: number) => {
        clearTimeout(id as unknown as ReturnType<typeof setTimeout>);
      });
    const renderResult = render(<Harness panel={panelSize} open onAnchor={() => {}} />);
    // The initial layout effect schedules at least one tracker frame.
    const initial = raf.mock.calls.length;
    expect(initial).toBeGreaterThan(0);
    // Drive ~60 frames worth of timers.
    await new Promise((resolve) => setTimeout(resolve, 60));
    const afterSettled = raf.mock.calls.length;
    // Once settled the tracker cancels itself: a long quiet window must not
    // schedule further frames beyond the opening burst.
    await new Promise((resolve) => setTimeout(resolve, 60));
    const afterQuiet = raf.mock.calls.length;
    expect(afterQuiet).toBe(afterSettled);
    // A scroll re-arms the tracker: it schedules a burst again, then stops.
    window.dispatchEvent(new Event("scroll", { bubbles: true, cancelable: true }));
    await new Promise((resolve) => setTimeout(resolve, 5));
    const onScroll = raf.mock.calls.length;
    expect(onScroll).toBeGreaterThan(afterQuiet);
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(raf.mock.calls.length).toBeLessThan(onScroll + 20);
    // The tracker settles (frame scheduling stops) and unmounting cancels any
    // pending frame via cancelAnimationFrame rather than leaking it.
    const { unmount } = renderResult;
    unmount();
    expect(caf.mock.calls.length).toBeGreaterThan(0);
  });

  it("does not flip placement when the panel is measured again under a height clamp", async () => {
    // Regression for the measurement feedback loop: `.popover` is
    // overflow:hidden and only an inner child scrolls, so once a max-height
    // clamp is applied the panel's own offsetHeight shrinks but the inner
    // body's scrollHeight keeps reporting the full content extent. Reading
    // the panel box directly fed the clamped height back in as `need`; with
    // a card bottom at 580 (104px of cleared up room) natural need 190 is
    // blocked (dodge down) but a clamped 100 would "fit" and flip up, on
    // every ResizeObserver cycle.
    //
    // Geometry: trigger 700-730, card bottom 580, viewport 900 (154 below).
    const avoid = { current: document.createElement("div") };
    Object.defineProperty(avoid.current, "getBoundingClientRect", {
      configurable: true,
      value: () => new DOMRect(100, 480, 1000, 100), // bottom = 580
    });
    let clamped = false;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.dataset.testid === "trigger") return triggerRect;
      if (this.dataset.testid === "panel") {
        const h = clamped ? 100 : 190;
        return {
          ...(triggerRect as unknown as Record<string, number>),
          left: 0, top: 0, right: 300, bottom: h, width: 300, height: h,
          toJSON: () => ({}),
        } as DOMRect;
      }
      return new DOMRect();
    });
    // The inner scroll wrapper: its scrollHeight is the constant CONTENT
    // extent (180 + 10 chrome ≈ 190) regardless of the clamped offsetHeight.
    Object.defineProperty(HTMLElement.prototype, "offsetHeight", {
      configurable: true,
      get(this: HTMLElement) {
        if (this.dataset.testid === "panel") return clamped ? 100 : 190;
        if (this.dataset.testid === "panel-body") return clamped ? 90 : 180;
        return 0;
      },
    });
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) {
        if (this.dataset.testid === "panel-body") return 180;
        return 0;
      },
    });
    const placements: string[] = [];
    render(<HarnessAvoid avoid={avoid} open onAnchor={(a) => placements.push(a.placement)} />);
    await new Promise((resolve) => setTimeout(resolve, 30));
    const first = placements[placements.length - 1];
    // First measure: natural 190, blocked by card (104 cleared), dodges down.
    expect(first).toBe("down");
    // The browser applies the clamp; fire repeated panel resize cycles.
    clamped = true;
    for (let i = 0; i < 4; i++) {
      layout.observerInstances.forEach((o) => o.flush());
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    // Despite the panel box now reporting 100px, need stays the inner content
    // extent: placement does NOT flip to up.
    expect(placements[placements.length - 1]).toBe("down");
    expect(new Set(placements).size).toBe(1);
  });

  it("reads content extent from a nested scroll region so a flex-column wrapper clamping never oscillates", async () => {
    // The effort popover is popover > flex-column [data-popover-scroll]
    // wrapper > slider card > effort-list (the real scrollport). Once the
    // 60vh/strip max-height clamps the card, the overflow lives entirely in
    // the nested region: the wrapper's OWN scrollHeight equals its clamped
    // clientHeight. naturalHeightOf must add the region's hidden overflow
    // back, otherwise `need` collapses to the clamp and the placement math
    // oscillates between the card-cleared strip cap (need = content, blocked)
    // and the plain above-room cap (need = clamp, "fits") every observer
    // cycle, resizing the panel.
    //
    // Review geometry: viewport 900, trigger 857/887, approval bottom 721 →
    // exactly 120px of cleared strip (usable). Below has no room
    // (900 - 887 - 8 - 8 = -3), so the panel stays up and caps to 120.
    const avoid = { current: document.createElement("div") };
    Object.defineProperty(avoid.current, "getBoundingClientRect", {
      configurable: true,
      value: () => new DOMRect(100, 621, 1000, 100), // bottom = 721
    });
    const triggerRect857 = {
      x: 590, y: 857, top: 857, bottom: 887, left: 590, right: 699, width: 109, height: 30,
      toJSON: () => ({}),
    } as unknown as DOMRect;
    Object.defineProperty(window, "innerWidth", { value: 1440, configurable: true });
    Object.defineProperty(window, "innerHeight", { value: 900, configurable: true });
    // Natural content: 2920px (20px head inside the wrapper + 2900px list).
    const CONTENT = 2920;
    const REGION_CONTENT = 2900;
    const HEAD = CONTENT - REGION_CONTENT;
    let clamped = false;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      if (this.dataset.testid === "trigger") return triggerRect857;
      if (this.dataset.testid === "panel") {
        const h = clamped ? 120 : CONTENT;
        return {
          x: 0, y: 0, right: 300, bottom: h, width: 300, height: h, toJSON: () => ({}),
        } as DOMRect;
      }
      return new DOMRect();
    });
    Object.defineProperty(HTMLElement.prototype, "offsetHeight", {
      configurable: true,
      get(this: HTMLElement) {
        if (this.dataset.testid === "panel") return clamped ? 120 : CONTENT;
        if (this.dataset.testid === "panel-body") return clamped ? 120 : CONTENT;
        return 0;
      },
    });
    // The flex wrapper never overflows itself (the nested region absorbs it),
    // so its scrollHeight tracks its clamped offsetHeight.
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get(this: HTMLElement) {
        if (this.dataset.testid === "panel-body") return clamped ? 120 : CONTENT;
        if (this.dataset.testid === "panel-region") return REGION_CONTENT;
        return 0;
      },
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get(this: HTMLElement) {
        if (this.dataset.testid === "panel-region") return clamped ? 120 - HEAD : REGION_CONTENT;
        return 0;
      },
    });
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
    const anchors: AnchoredStyle[] = [];
    render(<HarnessNestedRegion avoid={avoid} open onAnchor={(a) => anchors.push(a)} />);
    await new Promise((resolve) => setTimeout(resolve, 30));
    // First measure: need is the full content (2920) > the 120 strip, blocked
    // up, no room below → stays up capped exactly to the cleared strip.
    expect(anchors[anchors.length - 1].placement).toBe("up");
    expect(anchors[anchors.length - 1].style.maxHeight).toBe(120);

    // The browser applies the 120px clamp; fire repeated panel resize cycles.
    clamped = true;
    for (let i = 0; i < 5; i++) {
      observerInstances.forEach((o) => o.flush());
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    const last = anchors[anchors.length - 1];
    expect(last.placement).toBe("up");
    // need MUST NOT collapse to the clamped 120 (which would "fit" and jump to
    // the plain above-room 540 cap); every post-clamp measure keeps 120.
    expect(last.style.maxHeight).toBe(120);
    const postClamp = anchors.filter((_, i) => i > 0);
    expect(new Set(postClamp.map((a) => a.placement))).toEqual(new Set(["up"]));
    expect(new Set(postClamp.map((a) => a.style.maxHeight))).toEqual(new Set([120]));
    const tops = new Set(postClamp.map((a) => a.style.top));
    expect(tops.size, "panel top must not oscillate between caps").toBe(1);
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
