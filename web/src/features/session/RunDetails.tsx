import { useEffect, useState, type ReactNode } from "react";
import session from "./runDetails.module.css";
import css from "./runDetails.module.css";

/**
 * The §2.2 "运行详情" disclosure: the session header's main row answers
 * "can this run keep going" (host, cost, status, view switch, Stop); every
 * diagnostic field lives one fold below. The disclosure is collapsed by
 * default and the open state is remembered on THIS device only (D-040, same
 * local-storage口径 as §1.4 space prefs — never synced).
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

export function RunDetails({ count, children }: { count: number; children: ReactNode }) {
  const [open, setOpen] = useState(readOpen);
  useEffect(() => writeOpen(open), [open]);
  return (
    <details className={css.runDetails} data-testid="run-details" open={open}>
      <summary
        data-testid="run-details-summary"
        onClick={(event) => {
          event.preventDefault();
          setOpen((value) => !value);
        }}
      >
        <span className={css.chev} aria-hidden="true" />
        运行详情
        <span className={css.count}>{count} 项运行信息</span>
      </summary>
      <div className={`${session.meta} ${css.metaBody}`} data-testid="session-meta">
        {children}
      </div>
    </details>
  );
}
