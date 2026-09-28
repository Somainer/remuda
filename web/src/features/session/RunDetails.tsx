import { useEffect, useState, type ReactNode } from "react";
import session from "./runDetails.module.css";
import css from "./runDetails.module.css";

/**
 * The §2.2 "运行详情" disclosure: the session header's main row answers
 * "can this run keep going" (host, cost, status, view switch, Stop); every
 * diagnostic field lives one fold below. The disclosure is collapsed by
 * default and the open state is remembered on THIS device only (D-040, same
 * local-storage口径 as §1.4 space prefs — never synced).
 *
 * D-053 moved the trigger from a second header row into the ⋯ menu. When
 * `open` / `onClose` are supplied the disclosure is controlled: the menu owns
 * the trigger (so no <summary> is rendered here) and persistence is the
 * caller's responsibility. With no props the component keeps its original
 * self-triggering behaviour byte-for-byte.
 */
const KEY = "runtime.run-details.open";

function readOpen(): boolean {
  try {
    return localStorage.getItem(KEY) === "1";
  } catch {
    return false;
  }
}

function writeOpen(open: boolean): void {
  try {
    localStorage.setItem(KEY, open ? "1" : "0");
  } catch {
    /* storage unavailable: state just does not persist */
  }
}

export function RunDetails({
  count,
  children,
  open,
  onClose,
}: {
  count: number;
  children: ReactNode;
  /** Controlled open state (the ⋯ menu item owns the trigger). */
  open?: boolean;
  /** Request close; only meaningful while controlled (e.g. Escape). */
  onClose?: () => void;
}) {
  const [internalOpen, setInternalOpen] = useState(readOpen);
  const controlled = open !== undefined;
  const isOpen = controlled ? open : internalOpen;
  useEffect(() => {
    if (!controlled) writeOpen(internalOpen);
  }, [controlled, internalOpen]);

  // Controlled Escape-to-close; the panel itself is plain document flow, not
  // a modal, so the rest of the keyboard story stays with the menu.
  useEffect(() => {
    if (!controlled || !open || !onClose) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [controlled, open, onClose]);

  return (
    <details className={css.runDetails} data-testid="run-details" open={isOpen}>
      {controlled ? null : (
        <summary
          data-testid="run-details-summary"
          onClick={(event) => {
            event.preventDefault();
            setInternalOpen((value) => !value);
          }}
        >
          <span className={css.chev} aria-hidden="true" />
          运行详情
          <span className={css.count}>{count} 项运行信息</span>
        </summary>
      )}
      <div className={`${session.meta} ${css.metaBody}`} data-testid="session-meta">
        {children}
      </div>
    </details>
  );
}
