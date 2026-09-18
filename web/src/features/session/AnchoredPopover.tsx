import {
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type RefObject,
} from "react";

/**
 * One anchored, viewport-aware popover primitive for the composer menus
 * (effort card, permission wheel, context-usage card).
 *
 * The old menus were `position: absolute; left: 0` against the whole composer,
 * so every card pinned to the composer's left edge whatever chip opened it,
 * flipped up from a one-shot measurement taken before the panel's own
 * slider/list flip, and had no height bound — a tall catalog ran off the top
 * of the window. This primitive instead:
 *
 * - computes `left`/`top` from the **trigger** rect (`position: fixed`, so no
 *   containing-block surprises);
 * - flips up/down against the room actually available;
 * - shifts horizontally to stay inside the viewport;
 * - bounds the panel to `min(maxHeightVh, available room)`;
 * - re-measures on window resize/scroll and, crucially, on **panel resize**
 *   (ResizeObserver) — that is the signal the slider→list flip produces.
 *
 * jsdom has no layout: every measurement degrades to zeros and the panel
 * renders unpositioned (left/top 0, down), which keeps unit tests working.
 */

export type PopoverPlacement = "up" | "down";

export interface AnchoredStyle {
  placement: PopoverPlacement;
  /** Inline style to put on the positioned panel element. */
  style: CSSProperties;
  /** Re-measure on demand (rare; the automatic observers cover normal use). */
  remeasure: () => void;
}

export interface AnchoredOptions {
  /** Horizontal edge of the trigger the panel aligns to. Default "start". */
  align?: "start" | "end" | "center";
  /** Gap between the trigger and the panel, in px. */
  gap?: number;
  /** Inset kept from the viewport edges, in px. */
  margin?: number;
  /** Height cap as a fraction of the viewport height. Default 0.6. */
  maxHeightVh?: number;
  /** Prefer opening upward when the panel fits above the trigger (composer
   *  menus historically open up). When false, downward is preferred. */
  preferUp?: boolean;
  /** Touch-width bottom sheet: the panel keeps its own fixed
   *  left/right/bottom CSS and the hook skips trigger anchoring. */
  sheet?: boolean;
  /** Elements the panel must not overlap when opening above the trigger
   *  (e.g. the approval card parked over the composer). Only rects that sit
   *  above the trigger and overlap the panel's prospective horizontal span
   *  shrink the available room. */
  avoidElements?: ReadonlyArray<RefObject<HTMLElement | null>>;
}

interface Measured {
  placement: PopoverPlacement;
  left: number;
  top: number;
  /** Undefined when no cap applies (natural height clips the viewport edge). */
  maxHeight?: number;
}

const VIEWPORT_FALLBACK = { width: 1024, height: 768 };

function viewportSize() {
  if (typeof window === "undefined") return VIEWPORT_FALLBACK;
  return {
    width: window.innerWidth || VIEWPORT_FALLBACK.width,
    height: window.innerHeight || VIEWPORT_FALLBACK.height,
  };
}

/** Pure placement math, exported for unit tests (jsdom cannot measure). */
export function computeAnchored(
  triggerRect: Pick<DOMRect, "top" | "bottom" | "left" | "right" | "width">,
  panel: { preferredHeight: number; width: number },
  viewport: { width: number; height: number },
  options: AnchoredOptions & {
    preferUp?: boolean;
    /** Bottom edge (viewport-relative) of an obstruction the panel opening
     *  above the trigger must not overlap, such as an approval card parked
     *  over the composer. 0 when nothing is in the way. */
    avoidBottom?: number;
  },
): Measured {
  const gap = options.gap ?? 8;
  const margin = options.margin ?? 8;
  const align = options.align ?? "start";
  const maxHeightVh = options.maxHeightVh ?? 0.6;
  const preferUp = options.preferUp ?? true;
  const obstruction = Math.min(triggerRect.top, Math.max(0, options.avoidBottom ?? 0));
  const upperBoundary = obstruction ? Math.max(margin, obstruction + gap) : margin;
  // Plain viewport room on each side of the trigger.
  const roomAbove = Math.max(0, triggerRect.top - gap - margin);
  const roomBelow = Math.max(0, viewport.height - triggerRect.bottom - margin - gap);
  // Room above that ALSO clears the obstruction (the approval card).
  const roomAboveCleared = Math.max(0, triggerRect.top - gap - upperBoundary);
  const vhCap = Math.floor(viewport.height * maxHeightVh);
  const need = panel.preferredHeight;
  // A panel opening up must stay below a parked obstruction (approval card);
  // this is how much up room clears it.
  const upRoomCleared = obstruction > 0 ? roomAboveCleared : roomAbove;
  const upFitsCleared = upRoomCleared >= need;
  const blockedUpByCard = obstruction > 0 && !upFitsCleared;

  // Side choice:
  // - a parked card that blocks a full-height up panel → open below; the
  //   panel then starts at the trigger and can never cover the card (it may
  //   extend past the viewport bottom when the composer is docked low, where
  //   covering a dismissible card is the only alternative);
  // - otherwise the preferred side wins a tie, flip only when the other side
  //   is strictly roomier.
  let placement: PopoverPlacement;
  if (preferUp) {
    placement = blockedUpByCard || roomBelow > roomAbove ? "down" : "up";
  } else {
    placement = blockedUpByCard ? "down" : roomAbove > roomBelow ? "up" : "down";
  }
  const room = placement === "up" ? roomAbove : roomBelow;
  const cap = Math.min(vhCap, room);
  const maxHeight = cap > 0 ? cap : undefined;
  const panelHeight = Math.min(need, maxHeight ?? need);

  // Horizontal alignment against the trigger, then shift inside the viewport.
  let left: number;
  if (align === "end") {
    left = triggerRect.right - panel.width;
  } else if (align === "center") {
    left = triggerRect.left + (triggerRect.width - panel.width) / 2;
  } else {
    left = triggerRect.left;
  }
  const maxLeft = Math.max(margin, viewport.width - margin - panel.width);
  left = Math.min(Math.max(margin, left), maxLeft);

  const top =
    placement === "up"
      ? triggerRect.top - gap - panelHeight
      : triggerRect.bottom + gap;
  return { placement, left, top, maxHeight };
}

/**
 * Measure a mounted open panel against its trigger. Returns null when layout
 * is unavailable (jsdom) or either element is missing.
 */
export function useAnchoredPopover(
  triggerRef: RefObject<HTMLElement | null>,
  panelRef: RefObject<HTMLElement | null>,
  open: boolean,
  options: AnchoredOptions = {},
): AnchoredStyle {
  const resolved = {
    align: options.align ?? "start",
    gap: options.gap ?? 8,
    margin: options.margin ?? 8,
    maxHeightVh: options.maxHeightVh ?? 0.6,
    preferUp: options.preferUp ?? true,
    sheet: options.sheet ?? false,
    avoidElements: options.avoidElements ?? [],
  };
  const [measured, setMeasured] = useState<Measured | null>(null);
  const frame = useRef<number | null>(null);
  const optionsRef = useRef(resolved);
  optionsRef.current = resolved;

  const measure = useCallback(() => {
    const trigger = triggerRef.current;
    const panel = panelRef.current;
    if (!trigger || !panel || typeof window === "undefined") {
      setMeasured(null);
      return;
    }
    const triggerRect = trigger.getBoundingClientRect();
    if (triggerRect.width === 0 && triggerRect.height === 0) {
      setMeasured(null);
      return;
    }
    // The panel's natural size, ignoring the height cap the previous
    // measurement imposed — otherwise a clamped panel can never grow again
    // when the list view flips in.
    const prevMaxHeight = panel.style.maxHeight;
    panel.style.maxHeight = "none";
    const preferredHeight = panel.offsetHeight || panel.scrollHeight || 0;
    panel.style.maxHeight = prevMaxHeight;
    const width = panel.offsetWidth || panel.scrollWidth || 0;

    // The prospective horizontal span of the panel at the trigger; only
    // obstructions overlapping it matter (the approval card spans the
    // composer, a far-away chip elsewhere does not).
    const widthClamped = Math.min(width, viewportSize().width - optionsRef.current.margin * 2);
    let leftGuess: number;
    const alignNow = optionsRef.current.align;
    if (alignNow === "end") {
      leftGuess = triggerRect.right - widthClamped;
    } else if (alignNow === "center") {
      leftGuess = triggerRect.left + (triggerRect.width - widthClamped) / 2;
    } else {
      leftGuess = triggerRect.left;
    }
    const marginNow = optionsRef.current.margin;
    const maxLeft = Math.max(marginNow, viewportSize().width - marginNow - widthClamped);
    leftGuess = Math.min(Math.max(marginNow, leftGuess), maxLeft);
    const rightGuess = leftGuess + widthClamped;
    let avoidBottom = 0;
    for (const ref of optionsRef.current.avoidElements ?? []) {
      const rect = ref.current?.getBoundingClientRect();
      if (!rect) continue;
      if (
        rect.bottom <= triggerRect.top &&
        rect.right > leftGuess &&
        rect.left < rightGuess
      ) {
        avoidBottom = Math.max(avoidBottom, rect.bottom);
      }
    }

    const next = computeAnchored(
      triggerRect,
      { preferredHeight, width },
      viewportSize(),
      { ...optionsRef.current, avoidBottom },
    );
    setMeasured((current) =>
      current &&
      current.placement === next.placement &&
      Math.abs(current.left - next.left) < 1 &&
      Math.abs(current.top - next.top) < 1 &&
      Math.abs((current.maxHeight ?? -1) - (next.maxHeight ?? -1)) < 1
        ? current
        : next,
    );
  }, [panelRef, triggerRef]);

  const scheduleMeasure = useCallback(() => {
    if (typeof window === "undefined" || typeof window.requestAnimationFrame !== "function") {
      measure();
      return;
    }
    if (frame.current != null) window.cancelAnimationFrame(frame.current);
    frame.current = window.requestAnimationFrame(measure);
  }, [measure]);

  useLayoutEffect(() => {
    if (!open) {
      setMeasured(null);
      return;
    }
    if (resolved.sheet) {
      // The CSS bottom sheet owns its geometry; do not position it.
      setMeasured(null);
      return;
    }
    // Measure synchronously in the layout effect so the FIRST paint already
    // carries the computed box — a rAF-deferred first measurement paints one
    // frame at the fallback left:0/top:0 viewport corner.
    measure();
    const panel = panelRef.current;
    let observer: ResizeObserver | undefined;
    if (typeof ResizeObserver !== "undefined" && panel) {
      observer = new ResizeObserver(() => scheduleMeasure());
      observer.observe(panel);
    }
    window.addEventListener("resize", scheduleMeasure);
    // The trigger moves on any ancestor scroll (the panel is fixed, the
    // trigger is not), so listen in capture mode for scrolls everywhere.
    window.addEventListener("scroll", scheduleMeasure, true);
    // Layout shifts that move the trigger without resize/scroll events
    // (an approval card unmounts, turn events stream in, images load): a
    // fixed panel does not follow the trigger automatically, so after the
    // opening measurement settles, compare its box each animation frame and
    // re-measure on change.
    let raf = 0;
    let lastBox = "";
    let armed = false;
    const track = () => {
      if (!armed) {
        // Skip the opening burst (panel growth itself nudges layout once).
        armed = true;
        raf = window.requestAnimationFrame(track);
        return;
      }
      const rect = triggerRef.current?.getBoundingClientRect();
      if (rect) {
        const box = [rect.left, rect.top, rect.width, rect.height]
          .map((n) => Math.round(n * 10) / 10)
          .join(",");
        if (box !== lastBox) {
          lastBox = box;
          scheduleMeasure();
        }
      }
      raf = window.requestAnimationFrame(track);
    };
    if (typeof window !== "undefined" && !resolved.sheet) {
      raf = window.requestAnimationFrame(track);
    }
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", scheduleMeasure);
      window.removeEventListener("scroll", scheduleMeasure, true);
      if (raf) window.cancelAnimationFrame(raf);
      if (frame.current != null) window.cancelAnimationFrame(frame.current);
    };
  }, [open, panelRef, triggerRef, resolved.sheet, scheduleMeasure]);

  const style: CSSProperties =
    resolved.sheet ?
      { position: "fixed" }
    : measured ?
      {
        position: "fixed",
        left: Math.round(measured.left),
        top: Math.round(measured.top),
        ...(measured.maxHeight != null ? { maxHeight: measured.maxHeight } : {}),
        margin: 0,
      }
    : { position: "fixed", left: 0, top: 0, maxHeight: "none" };

  return {
    placement: resolved.sheet ? "down" : (measured?.placement ?? "down"),
    style,
    remeasure: scheduleMeasure,
  };
}
