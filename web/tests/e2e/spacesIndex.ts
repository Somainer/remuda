import { expect, type Page } from "@playwright/test";

/**
 * home.md §2.5 (D-061): on `/sessions` the SpacesPanel starts collapsed behind
 * the host band's 「空间 ▸」 toggle, and the device remembers whether it was
 * expanded. Specs that read the index (space rows, QuickFind's trigger) open
 * it first; a no-op when it is already visible.
 */
export async function openSpacesIndex(page: Page): Promise<void> {
  const panel = page.getByTestId("spaces-panel");
  // The panel stays mounted (its QuickFind hotkey is page-wide); collapsed
  // means hidden, not absent.
  if (await panel.isVisible().catch(() => false)) return;
  const toggle = page.getByTestId("spaces-index-open");
  await expect(toggle).toBeVisible();
  await toggle.click();
  await expect(panel).toBeVisible();
}
