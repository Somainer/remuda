/**
 * The ① count-row's scroll anchors (home.md §5.2). One place owns the
 * selectors so the phone head and the desktop PageHeader cannot drift.
 *
 * 需要你 scrolls to the queue; until H2 lands the queue, that is the ledger's
 * first blocked row (or, when only a task card is waiting, the task layer).
 */
export const GLANCE_TARGETS = {
  /** Phone: the row's data-blocked follows rank 1, not the UiStatus. */
  needYou: '[data-testid="home-row"][data-blocked="1"]',
  /**
   * Desktop: `/sessions` renders SessionList variant="full", whose rows are
   * `board-card` articles carrying the shared rank — `session-row` is only the
   * inner link (and the compact variant's row), so the rank attribute is the
   * one selector that matches what the page paints.
   */
  needYouDesktop: '[data-testid="board-card"][data-rank="blocked"]',
  tasks: '[data-testid="home-tasks"]',
} as const;

/** Scroll the first match among `selectors` into view (no-op when none). */
export function scrollToFirst(...selectors: readonly string[]): void {
  for (const selector of selectors) {
    const target = typeof document === "undefined" ? null : document.querySelector(selector);
    if (!target) continue;
    target.scrollIntoView({ block: "center", behavior: "smooth" });
    return;
  }
}
