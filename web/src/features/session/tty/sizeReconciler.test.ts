import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  createSizeReconciler,
  type Grid,
  type SizeReconciler,
} from "./sizeReconciler";

/**
 * UO-10 round-6: the reconciler invariant.
 *
 * A fake terminal (the grid xterm renders), a fake PTY (the grid last sent),
 * and a container/mode model. After ANY sequence of layout signals settles (the
 * ~80ms debounce flushed):
 *
 *     xterm.cols/rows === lastSent === desired
 *
 * Nothing cancels a needed send and nothing sends a stale target.
 */

type Mode = "fit" | "fixed" | "responsive";

const CELL: Record<Mode, { w: number; h: number }> = {
  responsive: { w: 8, h: 16 },
  fit: { w: 7, h: 14 },
  // Fixed caps the grid (font never shrinks past the base).
  fixed: { w: 8, h: 16 },
};
const FIXED_CAP = { cols: 100, rows: 40 };

type Harness = {
  reconciler: SizeReconciler;
  xterm: Grid;
  pty: Grid | null;
  sends: Grid[];
  renders: Grid[];
  setSize: (w: number, h: number) => void;
  setMode: (mode: Mode) => void;
  size: () => { w: number; h: number };
  keyboardOpen: () => void;
  keyboardClose: () => void;
  settle: () => Promise<void>;
};

const DEBOUNCE = 80;
const KEYBOARD_PX = 320;

function makeHarness(initial: { w: number; h: number }): Harness {
  let w = initial.w;
  let h = initial.h;
  let keyboard = false;
  let mode: Mode = "responsive";

  const xterm = { cols: 80, rows: 24 };
  const pty: { current: Grid | null } = { current: null };
  const sends: Grid[] = [];
  const renders: Grid[] = [];

  // Mirrors TerminalView.measureDesired: the RAW grid for the current
  // container + mode. It knows nothing about the keyboard hold — the reconciler
  // pins rows; this only ever reads real width/height.
  const measureDesired = (): Grid => {
    const cell = CELL[mode];
    const viewH = keyboard ? h - KEYBOARD_PX : h;
    const raw = {
      cols: Math.max(10, Math.floor(w / cell.w)),
      rows: Math.max(3, Math.floor(viewH / cell.h)),
    };
    return mode === "fixed"
      ? {
          cols: Math.min(FIXED_CAP.cols, raw.cols),
          rows: Math.min(FIXED_CAP.rows, raw.rows),
        }
      : raw;
  };

  const reconciler = createSizeReconciler({
    currentGrid: () => ({ ...xterm }),
    measureDesired,
    applyGrid: (grid) => {
      xterm.cols = grid.cols;
      xterm.rows = grid.rows;
      renders.push({ ...grid });
    },
    sendResize: (grid) => {
      pty.current = { ...grid };
      sends.push({ ...grid });
    },
    debounceMs: DEBOUNCE,
  });

  // Mount: TerminalView calls applyFit() once synchronously on mount.
  reconciler.layoutChanged();

  return {
    reconciler,
    xterm,
    get pty() {
      return pty.current;
    },
    sends,
    renders,
    setSize: (nextW, nextH) => {
      w = nextW;
      h = nextH;
      reconciler.layoutChanged();
    },
    setMode: (next) => {
      mode = next;
      reconciler.layoutChanged();
    },
    size: () => ({ w, h }),
    keyboardOpen: () => {
      keyboard = true;
      reconciler.keyboardOpened();
    },
    keyboardClose: () => {
      keyboard = false;
      reconciler.keyboardClosed();
    },
    async settle() {
      await vi.advanceTimersByTimeAsync(DEBOUNCE + 10);
    },
  };
}

// Deterministic PRNG for the property test.
function mulberry32(seed: number) {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const WIDTHS = [390, 393, 500, 700, 844, 1024, 1280, 1440];
const HEIGHTS = [640, 659, 740, 844, 900];
const MODES: Mode[] = ["fit", "fixed", "responsive"];

describe("size reconciler", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  const assertSettled = (h: Harness) => {
    const desired = h.reconciler.desired();
    const lastSent = h.reconciler.lastSent();
    expect(desired, "a desired grid exists").not.toBeNull();
    expect({ cols: h.xterm.cols, rows: h.xterm.rows }).toEqual(desired);
    expect(lastSent).toEqual(desired);
    expect(h.pty).toEqual(desired);
  };

  it("keyboard open/close sends ZERO PTY resizes", async () => {
    const h = makeHarness({ w: 393, h: 659 });
    await h.settle();
    h.reconciler.resetCount();

    h.keyboardOpen();
    await h.settle();
    expect(h.reconciler.resizeCount()).toBe(0);
    expect(h.pty).toEqual({ cols: h.xterm.cols, rows: h.xterm.rows });

    h.keyboardClose();
    await h.settle();
    expect(h.reconciler.resizeCount()).toBe(0);
    assertSettled(h);
  });

  it("rotation with the keyboard open sends exactly 1 resize: new cols, held rows", async () => {
    const h = makeHarness({ w: 393, h: 659 });
    await h.settle();
    const before = { ...h.xterm };
    h.reconciler.resetCount();

    h.keyboardOpen();
    await h.settle();
    expect(h.reconciler.resizeCount()).toBe(0);

    // Rotate: width changes while the keyboard stays open.
    h.setSize(700, 659);
    await h.settle();
    expect(h.reconciler.resizeCount()).toBe(1);
    expect(h.xterm.cols).toBeGreaterThan(before.cols);
    expect(h.xterm.rows).toBe(before.rows);
    expect(h.pty).toEqual({ cols: h.xterm.cols, rows: before.rows });

    // A later same-width refire commits nothing more.
    h.setSize(700, 659);
    await h.settle();
    expect(h.reconciler.resizeCount()).toBe(1);
    assertSettled(h);
  });

  it("fit→fixed→responsive with a frame between clicks ends PTY at the final xterm grid", async () => {
    const h = makeHarness({ w: 1440, h: 900 });
    await h.settle();
    h.reconciler.resetCount();

    // A height-only change lands inside the same burst.
    h.setSize(1440, 640);
    h.setMode("fit");
    // "Intermediate render actually happens": the grid is applied to xterm
    // synchronously on each signal even before the debounce fires.
    const afterFit = { ...h.xterm };
    await vi.advanceTimersByTimeAsync(16); // one frame, inside the debounce
    expect(h.renders).toContainEqual(afterFit);
    h.setMode("fixed");
    const afterFixed = { ...h.xterm };
    expect(afterFixed).not.toEqual(afterFit);
    await vi.advanceTimersByTimeAsync(16);
    h.setMode("responsive");

    await h.settle();
    expect(h.reconciler.resizeCount()).toBe(1);
    expect(h.pty).toEqual({ cols: h.xterm.cols, rows: h.xterm.rows });
    assertSettled(h);
  });

  it("W0→W1→W0 with a same-width event inside the debounce ends the PTY at W0", async () => {
    const h = makeHarness({ w: 393, h: 659 });
    await h.settle();
    h.keyboardOpen();
    await h.settle();
    h.reconciler.resetCount();
    const w0 = { ...h.xterm };

    h.setSize(700, 659); // W1 — arms a send
    await vi.advanceTimersByTimeAsync(40); // inside the 80ms debounce
    h.setSize(393, 659); // W0 — back to what the PTY already has
    // A same-width event (duplicate ResizeObserver + window resize pair).
    h.setSize(393, 659);
    await h.settle();

    expect(h.reconciler.resizeCount()).toBe(0);
    expect(h.pty).toEqual(w0);
    expect({ cols: h.xterm.cols, rows: h.xterm.rows }).toEqual(w0);
    assertSettled(h);
  });

  it("sub-threshold keyboard frames settle to ZERO resizes and the original grid", async () => {
    // m-realdevice raiseKeyboardAnimated: a 40px loss frame (data-keyboard
    // not yet stamped) lands 16ms BEFORE the full keyboard stamps. The early frame
    // may resize xterm locally and arm a send, but by settle the hold has
    // captured the PTY's rows, xterm is rolled back, and nothing is sent.
    const h = makeHarness({ w: 393, h: 659 });
    await h.settle();
    const before = { ...h.xterm };
    h.reconciler.resetCount();

    h.setSize(393, 619); // 40px loss, keyboard not yet "open" to the hold
    await vi.advanceTimersByTimeAsync(16); // one frame later: keyboard stamps
    h.keyboardOpen();
    await h.settle();

    expect(h.reconciler.resizeCount()).toBe(0);
    expect({ cols: h.xterm.cols, rows: h.xterm.rows }).toEqual(before);
    expect(h.pty).toEqual(before);
  });

  it("property: after any random signal burst settles, xterm === lastSent === desired", () => {
    const ITERATIONS = 500;
    const rand = mulberry32(0xc0ffeed0);
    const pick = <T,>(xs: readonly T[]) => xs[Math.floor(rand() * xs.length)];

    for (let i = 0; i < ITERATIONS; i++) {
      const h = makeHarness({
        w: pick(WIDTHS),
        h: pick(HEIGHTS),
      });
      // Some iterations start mid-keyboard.
      if (rand() < 0.3) {
        h.keyboardOpen();
      }
      const steps = 5 + Math.floor(rand() * 16);
      for (let s = 0; s < steps; s++) {
        switch (Math.floor(rand() * 6)) {
          case 0:
            h.setSize(pick(WIDTHS), h.size().h);
            break;
          case 1:
            h.setSize(h.size().w, pick(HEIGHTS));
            break;
          case 2:
            h.keyboardOpen();
            break;
          case 3:
            h.keyboardClose();
            break;
          case 4:
            // Rotation: new width with keyboard state held.
            h.setSize(pick(WIDTHS), pick(HEIGHTS));
            break;
          default:
            h.setMode(pick(MODES));
        }
        // Signals can land while the previous debounce is pending (the
        // reconciler must only re-arm, never cancel a needed send).
        if (rand() < 0.5) vi.advanceTimersByTime(20 + Math.floor(rand() * 100));
      }
      vi.advanceTimersByTime(DEBOUNCE + 50);
      assertSettled(h);
    }
  });

  it("property: xterm tracks desired immediately, even mid-burst", () => {
    const ITERATIONS = 200;
    const rand = mulberry32(0xbadc0de);
    const pick = <T,>(xs: readonly T[]) => xs[Math.floor(rand() * xs.length)];

    for (let i = 0; i < ITERATIONS; i++) {
      const h = makeHarness({ w: pick(WIDTHS), h: pick(HEIGHTS) });
      for (let s = 0; s < 10; s++) {
        switch (Math.floor(rand() * 5)) {
          case 0:
            h.setSize(pick(WIDTHS), h.size().h);
            break;
          case 1:
            h.setSize(h.size().w, pick(HEIGHTS));
            break;
          case 2:
            h.keyboardOpen();
            break;
          case 3:
            h.keyboardClose();
            break;
          default:
            h.setMode(pick(MODES));
        }
        // Apply is synchronous; the debounced send is what may lag.
        expect({ cols: h.xterm.cols, rows: h.xterm.rows }).toEqual(
          h.reconciler.desired(),
        );
      }
    }
  });
});
