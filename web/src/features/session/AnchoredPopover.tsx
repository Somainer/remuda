import {
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
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
  maxHeight: number;
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
  // The panel top must stay above the trigger by the gap and never cross the
  // top margin or an obstruction (approval card) plus the gap. Obstruction
  // bottoms below the trigger do not constrain room above (they are part of
  // the composer region the panel already avoids); clamp to the trigger edge.
  const obstruction = Math.min(triggerRect.top, Math.max(0, options.avoidBottom ?? 0));
  const upperBoundary = obstruction ? Math.max(margin, obstruction + gap) : margin;
  const roomAbove = Math.max(0, triggerRect.top - gap - upperBoundary);
  const roomBelow = Math.max(0, viewport.height - triggerRect.bottom - margin - gap);
  const vhCap = Math.max(80, Math.floor(viewport.height * maxHeightVh));

  // Pick the side: honour the preferred direction when the panel's natural
  // height fits there, otherwise take whichever side has more room.
  let placement: PopoverPlacement;
  if (preferUp) {
    placement = roomAbove >= panel.preferredHeight || roomAbove >= roomBelow ? "up" : "down";
  } else {
    placement = roomBelow >= panel.preferredHeight || roomBelow >= roomAbove ? "down" : "up";
  }
  const room = placement === "up" ? roomAbove : roomBelow;
  const maxHeight = Math.max(80, Math.min(vhCap, Math.floor(room)));
  const panelHeight = Math.min(panel.preferredHeight, maxHeight);

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
      Math.abs(current.maxHeight - next.maxHeight) < 1
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
    scheduleMeasure();
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
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", scheduleMeasure);
      window.removeEventListener("scroll", scheduleMeasure, true);
      if (frame.current != null) window.cancelAnimationFrame(frame.current);
    };
  }, [open, panelRef, resolved.sheet, scheduleMeasure]);

  const style: CSSProperties =
    resolved.sheet ?
      { position: "fixed" }
    : measured ?
      {
        position: "fixed",
        left: Math.round(measured.left),
        top: Math.round(measured.top),
        maxHeight: measured.maxHeight,
        margin: 0,
      }
    : { position: "fixed", left: 0, top: 0, maxHeight: "none" };

  return {
    placement: resolved.sheet ? "down" : (measured?.placement ?? "down"),
    style,
    remeasure: scheduleMeasure,
  };
}

/**
 * Declarative wrapper: renders `children` in a positioned `div` anchored to
 * `triggerRef` while `open`. The caller owns open/close state so its own
 * outside-pointerdown / hover logic stays untouched.
 */
export function AnchoredPopover({
  triggerRef,
  open,
  options,
  className,
  testId,
  role = "dialog",
  ariaLabel,
  data,
  onMouseEnter,
  onMouseLeave,
  onKeyDown,
  children,
}: {
  triggerRef: RefObject<HTMLElement | null>;
  open: boolean;
  options?: AnchoredOptions;
  className?: string;
  testId?: string;
  role?: "dialog" | "menu" | undefined;
  ariaLabel?: string;
  data?: Record<string, string | number | boolean | undefined>;
  onMouseEnter?: () => void;
  onMouseLeave?: () => void;
  onKeyDown?: (event: React.KeyboardEvent<HTMLDivElement>) => void;
  children: ReactNode;
}) {
  const panelRef = useRef<HTMLDivElement>(null);
  const anchored = useAnchoredPopover(triggerRef, panelRef, open, options);
  if (!open) return null;
  return (
    <div
      ref={panelRef}
      className={className}
      style={anchored.style}
      data-testid={testId}
      data-placement={anchored.placement}
      role={role}
      aria-label={ariaLabel}
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
      onKeyDown={onKeyDown}
      {...data}
    >
      {children}
    </div>
  );
}
