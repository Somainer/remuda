/**
 * UO-10 round-6: the ONE terminal-size reconciler.
 *
 * Five rounds of special cases (freeze/defer/fit gates, rotation flags, frozen
 * snapshots, per-event timer cancellation) kept leaving xterm and the PTY at
 * different grids. Every input — ResizeObserver, visualViewport resize, rotation,
 * fit/fixed/responsive mode change, web-font load, soft-keyboard open/close —
 * is just another layout signal to this single state machine:
 *
 *   lastSent     cols/rows actually sent to the PTY (null before the first).
 *   keyboardHold rows of the grid ON SCREEN when the keyboard started opening
 *                — xterm tracks desired synchronously, so a resize still in
 *                the debounce window is captured as the hold (its pending
 *                target still goes out once); null otherwise.
 *
 * On every signal it recomputes `desired` — the grid the current container and
 * mode measure to — with ONE override: while a keyboard is held, rows stay on
 * `keyboardHold` (cols always follow the real width, so a rotation while the
 * keyboard is open still refits cols). `desired` is applied to xterm
 * immediately, and exactly ONE debounced timer is (re-)armed: when it fires it
 * sends `desired` to the PTY only if it differs from `lastSent`.
 *
 * Nothing ever cancels a needed send (a later signal only re-arms the same
 * timer), and nothing can send a stale target (the timer always sends the
 * current `desired`, never the grid it was armed with).
 *
 * The cursor crop lives entirely in the view layer and is not modelled here.
 */

export type Grid = { cols: number; rows: number };

export const SIZE_SEND_DEBOUNCE_MS = 80;

export type SizeReconciler = {
  /** Any layout signal: container resize, viewport resize, rotation, mode, font. */
  layoutChanged: () => void;
  /**
   * Soft keyboard started opening: pin rows to the grid on screen at onset.
   * MUST be invoked BEFORE the onset event's own shrunken layout is measured
   * (TerminalView syncs the hold at the top of its viewport handler) —
   * otherwise the captured grid is the keyboard-band grid, not the pending one.
   */
  keyboardOpened: () => void;
  /** Soft keyboard closed: rows follow the container again. */
  keyboardClosed: () => void;
  /** The grid the most recent reconcile decided on. */
  desired: () => Grid | null;
  /** The grid last actually sent to the PTY. */
  lastSent: () => Grid | null;
  /** Rows held while the keyboard is open (null otherwise). */
  holdRows: () => number | null;
  /** PTY resizes since mount/reset. */
  resizeCount: () => number;
  /** Test support: zero the counter and drop a pending (post-settle) timer. */
  resetCount: () => void;
  dispose: () => void;
};

export type SizeReconcilerOptions = {
  /** The grid xterm currently renders. */
  currentGrid: () => Grid;
  /**
   * Measure the raw grid the container/mode wants RIGHT NOW. May return null
   * while the box is unmeasurable (collapsed container); the signal is then
   * ignored and a later one recomputes.
   */
  measureDesired: () => Grid | null;
  /** Push a grid into xterm (and any mirroring React state). */
  applyGrid: (grid: Grid) => void;
  /** Send a resize to the PTY. */
  sendResize: (grid: Grid) => void;
  /** Debounce delay (tests pin it); defaults to 80ms. */
  debounceMs?: number;
};

const sameGrid = (a: Grid | null, b: Grid | null): boolean =>
  !!a && !!b && a.cols === b.cols && a.rows === b.rows;

export function createSizeReconciler(
  options: SizeReconcilerOptions,
): SizeReconciler {
  const debounceMs = options.debounceMs ?? SIZE_SEND_DEBOUNCE_MS;
  let desiredGrid: Grid | null = null;
  let sentGrid: Grid | null = null;
  let hold: number | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let count = 0;

  const clearTimer = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };

  /**
   * (Re-)arm the single debounced send. If desired already equals what the PTY
   * has, there is nothing to send — drop any timer an intermediate grid armed
   * (this is what lets a W0→W1→W0 bounce emit nothing).
   */
  const arm = () => {
    if (!desiredGrid) return;
    if (sameGrid(desiredGrid, sentGrid)) {
      clearTimer();
      return;
    }
    clearTimer();
    timer = setTimeout(() => {
      timer = null;
      const target = desiredGrid;
      if (!target || sameGrid(target, sentGrid)) return;
      options.sendResize(target);
      sentGrid = target;
      count += 1;
    }, debounceMs);
  };

  const recompute = () => {
    const measured = options.measureDesired();
    if (!measured) return;
    // While a keyboard is held, rows are pinned; cols follow the real width.
    desiredGrid =
      hold === null ? measured : { cols: measured.cols, rows: hold };
    const current = options.currentGrid();
    if (
      current.cols !== desiredGrid.cols ||
      current.rows !== desiredGrid.rows
    ) {
      options.applyGrid(desiredGrid);
    }
    arm();
  };

  return {
    layoutChanged: recompute,
    keyboardOpened: () => {
      if (hold !== null) return;
      // Capture rows from the grid ON SCREEN at onset. xterm tracks desired
      // synchronously, so a legitimate resize still inside the debounce
      // window is already the current grid: pinning to it lets that pending
      // target (cols AND rows) go out exactly once, and only height changes
      // arriving AFTER onset are pinned. lastSent must not be used here — it
      // is still the pre-pending grid, so it would pair brand-new cols with
      // stale rows, or cancel a height-only pending resize outright.
      hold = options.currentGrid().rows;
      recompute();
    },
    keyboardClosed: () => {
      if (hold === null) return;
      hold = null;
      recompute();
    },
    desired: () => desiredGrid,
    lastSent: () => sentGrid,
    holdRows: () => hold,
    resizeCount: () => count,
    resetCount: () => {
      count = 0;
      clearTimer();
    },
    dispose: clearTimer,
  };
}
