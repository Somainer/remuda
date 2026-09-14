import { type ReactNode, type RefObject } from "react";
import { useFocusTrap } from "./useFocusTrap";
import css from "./overlay.module.css";

/**
 * The one overlay in the workbench (UX plan §2).
 *
 * `popover` anchors to its trigger on desktop; `sheet` rises from the bottom
 * edge on phones. Both get the same contract from {@link useFocusTrap}: name,
 * `aria-modal`, initial focus, Tab cycling, Escape and focus return. Callers
 * pick the variant — usually from `useWorkbenchViewport().mobile` — and never
 * hand-roll a scrim.
 */
export type SheetProps = {
  open: boolean;
  onClose: () => void;
  /** id of the element naming this overlay; render it inside `children`. */
  labelledBy?: string;
  initialFocusRef?: RefObject<HTMLElement | null>;
  returnFocusRef?: RefObject<HTMLElement | null>;
  variant?: "popover" | "sheet";
  /** Applied to the panel, for per-caller width/placement. */
  className?: string;
  testId?: string;
  children: ReactNode;
};

export function Sheet({
  open,
  onClose,
  labelledBy,
  initialFocusRef,
  returnFocusRef,
  variant = "popover",
  className = "",
  testId,
  children,
}: SheetProps) {
  const trap = useFocusTrap({ open, onClose, labelledBy, initialFocusRef, returnFocusRef });
  const { containerRef, onKeyDown, ...aria } = trap;
  if (!open) return null;
  return (
    <div className={css.scrim} data-variant={variant} onMouseDown={onClose} role="presentation">
      <div
        {...aria}
        ref={containerRef}
        onKeyDown={onKeyDown}
        onMouseDown={(event) => event.stopPropagation()}
        className={`${css.panel} ${variant === "sheet" ? css.panelSheet : css.panelPopover} ${className}`}
        data-testid={testId}
        data-variant={variant}
      >
        {children}
      </div>
    </div>
  );
}
