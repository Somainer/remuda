import { useEffect, useRef, type KeyboardEvent as ReactKeyboardEvent, type RefObject } from "react";
import { isTerminalTarget } from "../lib/keyboardScope";

/**
 * The shared overlay keyboard/focus contract (UX plan §2).
 *
 * Every overlay in the workbench — the session filter popover, the new-session
 * sheet, the cross-space finder — takes its name, initial focus, Tab cycling,
 * Escape and focus return from here rather than re-implementing them. Batches
 * B, D and F consume this hook; they must not write their own scrim.
 *
 * Two rules make this safe next to an attached terminal (UX plan §4 risk 1):
 *
 * 1. The keydown listener is attached to the **overlay container**, never to
 *    `window`. A global listener would swallow Escape and Tab for every
 *    terminal on the page, not just the one under the overlay.
 * 2. It returns immediately for events originating inside `.xterm`. A terminal
 *    rendered within an overlay still owns Escape and Tab as bytes for the
 *    native process; the overlay must not close or re-aim focus on them.
 */
export type FocusTrapOptions = {
  open: boolean;
  onClose: () => void;
  /** Element the overlay container is `aria-labelledby`. */
  labelledBy?: string;
  /** Focused when the overlay opens; the container itself if omitted. */
  initialFocusRef?: RefObject<HTMLElement | null>;
  /** Focused when the overlay closes; the previously focused element if omitted. */
  returnFocusRef?: RefObject<HTMLElement | null>;
};

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled]):not([type='hidden'])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(", ");

/**
 * Tab order within the overlay.
 *
 * Hidden controls are skipped so an overlay that collapses a section does not
 * strand focus on something invisible. Visibility is read from `hidden`/`inert`
 * and computed styles rather than box size: layout-free environments report
 * every element as zero-sized, and a size test there would empty the trap.
 */
export function focusableIn(container: HTMLElement): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((element) => {
    if (element.hasAttribute("inert") || element.closest("[inert]")) return false;
    if (element.hidden || element.closest("[hidden]")) return false;
    const style = getComputedStyle(element);
    return style.display !== "none" && style.visibility !== "hidden";
  });
}

export type FocusTrap = {
  /** Spread onto the overlay container element. */
  containerRef: RefObject<HTMLDivElement | null>;
  onKeyDown: (event: ReactKeyboardEvent<HTMLElement>) => void;
  "aria-labelledby": string | undefined;
  "aria-modal": true;
  role: "dialog";
  tabIndex: -1;
};

export function useFocusTrap({ open, onClose, labelledBy, initialFocusRef, returnFocusRef }: FocusTrapOptions): FocusTrap {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const restoreRef = useRef<HTMLElement | null>(null);
  // The open/close effect keys on `open` alone: a caller passing inline arrows
  // must not re-run it and re-steal focus on every render. It reads the current
  // callbacks through this mirror, which the effect below refreshes after each
  // render — declared first, so it is already current when the effect runs.
  const latest = useRef({ onClose, initialFocusRef, returnFocusRef });
  useEffect(() => {
    latest.current = { onClose, initialFocusRef, returnFocusRef };
  });

  useEffect(() => {
    if (!open) return;
    // Remember the trigger before moving focus; on close we put it back here
    // unless the caller named somewhere else.
    restoreRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const container = containerRef.current;
    const opened = latest.current;
    const target = opened.initialFocusRef?.current ?? (container ? (focusableIn(container)[0] ?? container) : null);
    target?.focus();
    const trigger = restoreRef.current;
    // Resolve the return target now, while the overlay is opening and the
    // trigger is certainly still mounted, rather than at close time.
    const back = opened.returnFocusRef?.current ?? trigger;
    return () => {
      // Only pull focus back if it is still inside the overlay we are closing.
      // If the user already clicked elsewhere, respect where they went.
      const moved = document.activeElement;
      const inside = !moved || moved === document.body || (container?.contains(moved) ?? false);
      if (inside && back?.isConnected) back.focus();
    };
  }, [open]);

  const onKeyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    // An attached terminal owns Escape and Tab as bytes for the native process.
    if (isTerminalTarget(event.target)) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      onClose();
      return;
    }
    if (event.key !== "Tab") return;
    const container = containerRef.current;
    if (!container) return;
    const items = focusableIn(container);
    if (!items.length) {
      // Nothing to cycle between: keep focus on the container rather than
      // letting Tab escape to the page behind the overlay.
      event.preventDefault();
      container.focus();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement as HTMLElement | null;
    if (event.shiftKey && (active === first || active === container || !container.contains(active))) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  };

  return { containerRef, onKeyDown, "aria-labelledby": labelledBy, "aria-modal": true, role: "dialog", tabIndex: -1 };
}
