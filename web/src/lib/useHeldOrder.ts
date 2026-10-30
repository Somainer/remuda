import { useCallback, useEffect, useRef, useState } from "react";

/**
 * How long a hold survives with no pointer or focus activity at all. A lost
 * `pointerup` (released over the header, another window, or a removed row)
 * would otherwise freeze the list for the rest of the session.
 */
export const HOLD_IDLE_RELEASE_MS = 10_000;

export type HeldOrder<T> = {
  /** True while the list must not re-rank. */
  held: boolean;
  /** The structure captured when the hold began, or null. */
  snapshot: T | null;
  /** Begin a hold; a second call while held only refreshes the idle timer. */
  start: (build: () => T) => void;
  /** End the hold; the list re-ranks once, on the next render. */
  release: () => void;
};

/**
 * One source of truth for "do not re-rank right now" (home.md §3.3).
 *
 * The caller snapshots the PAINTED structure when the hold begins and renders
 * from that snapshot while held, swapping in live row content by id. This hook
 * only owns the lifetime: it stays held until the release conditions below, so
 * there is no counter that can drift.
 *
 * Release happens on a window-level `pointerup`/`pointercancel` (wherever the
 * finger or mouse ends up), on an explicit `release()` (a Sheet closing, focus
 * leaving the list), on unmount, and after {@link HOLD_IDLE_RELEASE_MS} with
 * no pointer or keyboard activity.
 */
export function useHeldOrder<T>(): HeldOrder<T> {
  const [snapshot, setSnapshot] = useState<T | null>(null);
  const heldRef = useRef(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearTimer = useCallback(() => {
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
    }
  }, []);

  const release = useCallback(() => {
    clearTimer();
    heldRef.current = false;
    setSnapshot(null);
  }, [clearTimer]);

  const arm = useCallback(() => {
    clearTimer();
    timer.current = setTimeout(() => {
      timer.current = null;
      heldRef.current = false;
      setSnapshot(null);
    }, HOLD_IDLE_RELEASE_MS);
  }, [clearTimer]);

  const start = useCallback(
    (build: () => T) => {
      if (!heldRef.current) {
        heldRef.current = true;
        setSnapshot(build());
      }
      arm();
    },
    [arm],
  );

  useEffect(() => {
    if (snapshot === null) return;
    const end = () => release();
    const live = () => arm();
    window.addEventListener("pointerup", end);
    window.addEventListener("pointercancel", end);
    // A keyboard activation (Enter on a fold or row control) ends a focus hold.
    window.addEventListener("click", end);
    window.addEventListener("pointerdown", live);
    window.addEventListener("pointermove", live);
    window.addEventListener("keydown", live);
    window.addEventListener("focusin", live);
    return () => {
      window.removeEventListener("pointerup", end);
      window.removeEventListener("pointercancel", end);
      window.removeEventListener("click", end);
      window.removeEventListener("pointerdown", live);
      window.removeEventListener("pointermove", live);
      window.removeEventListener("keydown", live);
      window.removeEventListener("focusin", live);
    };
  }, [snapshot, release, arm]);

  // A hold never outlives the list that began it.
  useEffect(() => release, [release]);

  return { held: snapshot !== null, snapshot, start, release };
}
