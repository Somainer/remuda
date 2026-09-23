import { useEffect, useState, type ReactNode, type RefObject } from "react";
import { useFocusTrap } from "./useFocusTrap";
import css from "./overlay.module.css";

/**
 * The centered dialog, now on the shared overlay contract (UX plan §2).
 *
 * It keeps its own visual-viewport tracking — on a phone the software keyboard
 * shrinks the viewport and the panel has to follow it — but name, focus and
 * keyboard behaviour come from {@link useFocusTrap}, so Escape closes it, the
 * initial focus lands inside, Tab no longer walks onto the page behind the
 * scrim, and closing returns focus to whatever opened it.
 */
export function Modal({
  open,
  onClose,
  labelledBy,
  initialFocusRef,
  returnFocusRef,
  children,
}: {
  open: boolean;
  onClose: () => void;
  labelledBy?: string;
  initialFocusRef?: RefObject<HTMLElement | null>;
  returnFocusRef?: RefObject<HTMLElement | null>;
  children: ReactNode;
}) {
  const { containerRef, onKeyDown, ...aria } = useFocusTrap({ open, onClose, labelledBy, initialFocusRef, returnFocusRef });
  const [box, setBox] = useState(() => ({
    height: typeof window === "undefined" ? 800 : window.visualViewport?.height || window.innerHeight,
    top: typeof window === "undefined" ? 0 : window.visualViewport?.offsetTop || 0,
  }));

  useEffect(() => {
    if (!open) return;
    const update = () =>
      setBox({
        height: window.visualViewport?.height || window.innerHeight,
        top: window.visualViewport?.offsetTop || 0,
      });
    update();
    window.visualViewport?.addEventListener("resize", update);
    window.visualViewport?.addEventListener("scroll", update);
    window.addEventListener("resize", update);
    return () => {
      window.visualViewport?.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
    };
  }, [open]);

  if (!open) return null;
  return (
    <div className={css.modalScrim} style={{ top: box.top, height: box.height }} onClick={onClose} role="presentation">
      <div
        {...aria}
        ref={containerRef}
        onKeyDown={onKeyDown}
        className={css.modalPanel}
        onClick={(e) => e.stopPropagation()}
      >
        {children}
      </div>
    </div>
  );
}
