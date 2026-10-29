import { useEffect, useSyncExternalStore } from "react";
import { hubStore, type MainEntry } from "../../lib/store";
import { COMPACT_WORKBENCH_QUERY } from "../../lib/viewport";
import type { Id } from "../../types/wire";

/** Lineage projection refresh while a Main surface is mounted. */
const LINEAGE_POLL_MS = 15_000;

const ASIDE_QUERY = "(min-width: 1440px)";

function subscribeAside(onChange: () => void) {
  if (typeof window.matchMedia !== "function") return () => undefined;
  const media = window.matchMedia(ASIDE_QUERY);
  media.addEventListener("change", onChange);
  return () => media.removeEventListener("change", onChange);
}

/**
 * ≥1440 the desktop home has a right column and the Main card heads it;
 * 1280–1439 (and below) the card heads the main column instead.
 */
export function useMainAside(): boolean {
  return useSyncExternalStore(
    subscribeAside,
    () => typeof window.matchMedia === "function" && window.matchMedia(ASIDE_QUERY).matches,
    () => false,
  );
}

/**
 * Keep `lineages[lineageId]` fresh while a Main surface is mounted: on mount,
 * whenever the resolved chapter moves (resume, lifecycle edge), and on a slow
 * poll. A failed read keeps the last record (store.refreshLineage).
 */
export function useLineageSync(entry: MainEntry | null) {
  const lineageId = entry?.lineageId;
  const chapterKey = entry
    ? `${entry.instance.id}:${entry.instance.lifecycle}:${entry.live ? 1 : 0}:${entry.instance.connectivity}`
    : "";
  useEffect(() => {
    if (!lineageId) return;
    // The view is the resolved chapter and its lifecycle edge: a read issued
    // after a chapter switch (or a pause landing) never joins one that started
    // before it (store.refreshLineage).
    void hubStore.refreshLineage(lineageId as Id, chapterKey);
    const timer = window.setInterval(() => void hubStore.refreshLineage(lineageId as Id, chapterKey), LINEAGE_POLL_MS);
    return () => window.clearInterval(timer);
  }, [lineageId, chapterKey]);
}


function subscribeCompact(onChange: () => void) {
  if (typeof window.matchMedia !== "function") return () => undefined;
  const media = window.matchMedia(COMPACT_WORKBENCH_QUERY);
  media.addEventListener("change", onChange);
  return () => media.removeEventListener("change", onChange);
}

/**
 * The compact layout query, read-only. The Main surfaces must not call
 * useWorkbenchViewport(): its cleanup clears the shell's visual-band
 * variables and the keyboard flag, and a Main surface unmounting while the
 * soft keyboard is up would collapse the /m band.
 */
export function useCompactLayout(): boolean {
  return useSyncExternalStore(
    subscribeCompact,
    () => typeof window.matchMedia === "function" && window.matchMedia(COMPACT_WORKBENCH_QUERY).matches,
    () => false,
  );
}
