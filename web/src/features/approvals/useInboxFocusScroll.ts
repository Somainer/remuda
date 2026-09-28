import { useEffect, type RefObject } from "react";

/**
 * Deep-link scroll for `?focus=<interaction-id>` on the inbox shell.
 *
 * The focused row mounts progressively: each tier reveals another slice per
 * animation frame (useIncrementalLimit), so the scroll must re-run whenever
 * ANY tier's slice limit grows, not only when the target first commits.
 *
 * Desktop ordering matters (c-inboxfu round 2): 待你处理 / 进行中 · 最近 /
 * 已离队. A focused DEPARTED row scrolls into view, then a later recent slice
 * mounts ABOVE it and pushes it back below the fold — omitting the recent
 * limit from the deps left it off-screen. Compact has two tiers and passes
 * just its pending limit.
 */
export function useInboxFocusScroll(
  focus: string | null,
  focusedKey: string | null,
  scrollRef: RefObject<HTMLElement | null>,
  /** Progressive-mount limit of every tier rendered above/around the target. */
  limits: ReadonlyArray<number>,
): void {
  // A primitive key: a fresh array every render must not by itself retrigger
  // the effect, but a grown slice must.
  const limitsKey = limits.join(",");
  useEffect(() => {
    if (!focus || !focusedKey) return;
    const el = scrollRef.current?.querySelector<HTMLElement>(
      `[data-interaction-id="${CSS.escape(focus)}"]`,
    );
    el?.scrollIntoView({ block: "center", behavior: "auto" });
    // limitsKey is the joined projection of the tier limits above.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focus, focusedKey, scrollRef, limitsKey]);
}
