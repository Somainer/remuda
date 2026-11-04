/**
 * The ① count-row's scroll anchor (home.md §5.2): 需要你 scrolls to the ④
 * queue (H2). One place owns the id so the phone head, the desktop PageHeader
 * and the queue itself cannot drift.
 */
export const GLANCE_TARGETS = {
  /** The ④ 需要你 queue's element id (home.md §4; the push/① scroll anchor). */
  queueId: "home-queue",
  /**
   * ⑥'s 「+N ›」: the board filtered by the SAME §3.1 rank the 工作中 count
   * uses, across every Space. A rank filter keeps dispatched workers and
   * sessions on a dropped host (rank reads lifecycle and activity, never
   * connectivity); `status=working` filtered on UiStatus and dropped both.
   */
  workingHref: "/sessions?rank=working&scope=all",
} as const;

/**
 * Bring the ④ queue's top into view. Its scroll-margin clears the sticky
 * head, so the header line 「需要你 · N」 is the first thing under it.
 */
export function scrollToQueue(): void {
  const target = typeof document === "undefined" ? null : document.getElementById(GLANCE_TARGETS.queueId);
  target?.scrollIntoView({ block: "start", behavior: "smooth" });
}
