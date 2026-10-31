/**
 * The ① count-row's scroll anchors (home.md §5.2). One place owns the
 * selectors so the phone head and the desktop PageHeader cannot drift.
 *
 * 需要你 scrolls to the queue; until H2 lands the queue, that is the ledger's
 * first blocked row (or, when only a task card is waiting, the task layer).
 */
export const GLANCE_TARGETS = {
  needYou: '[data-testid="home-row"][data-blocked="1"]',
  needYouDesktop: '[data-testid="session-row"][data-status="blocked"]',
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
