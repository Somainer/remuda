import { useCallback, useEffect, useRef, useState, type RefObject } from "react";

/**
 * How long a hold survives with no activity inside the rows region. A lost
 * `pointerup` (released outside the browser window, or on a row that vanished)
 * would otherwise freeze the list for the rest of the session.
 */
export const HOLD_IDLE_RELEASE_MS = 10_000;

/**
 * Where a hold comes from. They are independent: the list stays frozen while
 * any one of them is active, and releases when the last one ends.
 *
 * - `pointer` — a press inside the rows region; ends on a window pointerup or
 *   pointercancel, and is subject to the idle timeout.
 * - `focus` — keyboard focus inside the rows region; ends when focus leaves
 *   that region, and is subject to the idle timeout.
 * - `sheet` — a row action Sheet or the spaces drawer is open; ends only when
 *   it closes (or the list unmounts). Pointer and timeout activity inside the
 *   sheet must not release it.
 */
export type HoldSource = "pointer" | "focus" | "sheet";

export type HeldOrder<T> = {
  /** True while any source is active. */
  held: boolean;
  /** The structure captured when the first source began, or null. */
  snapshot: T | null;
  /** Begin (or refresh) one source; the snapshot is taken on the first one. */
  begin: (source: HoldSource, build: () => T) => void;
  /** End one source; the snapshot drops when none remain. */
  end: (source: HoldSource) => void;
  /**
   * Attach to the rows region (rows only — never the toolbar or search). The
   * idle timer is re-armed only by activity in here.
   */
  regionRef: RefObject<HTMLDivElement | null>;
};

/**
 * One source of truth for "do not re-rank right now" (home.md §3.3).
 *
 * The caller snapshots the PAINTED structure when the first source begins and
 * renders from that snapshot while held, swapping in live row content by id.
 * Sources are tracked by name, so a lost `pointerup` or a click inside an open
 * Sheet cannot end a hold that another source still owns.
 */
export function useHeldOrder<T>(): HeldOrder<T> {
  const [snapshot, setSnapshot] = useState<T | null>(null);
  const sources = useRef<Set<HoldSource>>(new Set());
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const regionRef = useRef<HTMLDivElement | null>(null);

  const clearTimer = useCallback(() => {
    if (timer.current !== null) {
      clearTimeout(timer.current);
      timer.current = null;
    }
  }, []);

  const arm = useCallback(() => {
    clearTimer();
    timer.current = setTimeout(() => {
      timer.current = null;
      // The idle timeout only ends the interactive sources: an open Sheet keeps
      // the list frozen until the person closes it.
      sources.current.delete("pointer");
      sources.current.delete("focus");
      if (sources.current.size === 0) setSnapshot(null);
    }, HOLD_IDLE_RELEASE_MS);
  }, [clearTimer]);

  const begin = useCallback(
    (source: HoldSource, build: () => T) => {
      const first = sources.current.size === 0;
      sources.current.add(source);
      if (first) setSnapshot(build());
      if (source !== "sheet") arm();
    },
    [arm],
  );

  const end = useCallback(
    (source: HoldSource) => {
      sources.current.delete(source);
      if (sources.current.size === 0) {
        clearTimer();
        setSnapshot(null);
      }
    },
    [clearTimer],
  );

  // A press ends wherever the pointer does; a pointer release is the one end
  // signal that has to come from the window.
  useEffect(() => {
    if (snapshot === null) return;
    const releasePointer = () => end("pointer");
    window.addEventListener("pointerup", releasePointer);
    window.addEventListener("pointercancel", releasePointer);
    return () => {
      window.removeEventListener("pointerup", releasePointer);
      window.removeEventListener("pointercancel", releasePointer);
    };
  }, [snapshot, end]);

  // Idle activity is scoped to the rows region: typing in the search box or
  // moving the mouse over the page must not keep a lost press alive.
  useEffect(() => {
    const region = regionRef.current;
    if (snapshot === null || !region) return;
    const live = () => arm();
    region.addEventListener("pointerdown", live);
    region.addEventListener("pointermove", live);
    region.addEventListener("keydown", live);
    region.addEventListener("focusin", live);
    return () => {
      region.removeEventListener("pointerdown", live);
      region.removeEventListener("pointermove", live);
      region.removeEventListener("keydown", live);
      region.removeEventListener("focusin", live);
    };
  }, [snapshot, arm]);

  // A hold never outlives the list that began it, and the timer never fires
  // into an unmounted component.
  useEffect(
    () => () => {
      clearTimer();
      sources.current.clear();
    },
    [clearTimer],
  );

  return { held: snapshot !== null, snapshot, begin, end, regionRef };
}
