import { useEffect, useState, type ReactNode } from "react";
import ui from "../styles/ui.module.css";

export function Modal({ open, onClose, children }: { open: boolean; onClose: () => void; children: ReactNode }) {
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
    <div className={ui.modal} style={{ top: box.top, height: box.height }} onClick={onClose} role="presentation">
      <div
        className={ui.modalPanel}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        {children}
      </div>
    </div>
  );
}
