import { expect, type Page } from "@playwright/test";

/**
 * home.md §2.5 (D-061): on `/sessions` the SpacesPanel starts collapsed behind
 * the host band's 「空间 ▸」 toggle, and the device remembers whether it was
 * expanded. Specs that read the index (space rows, QuickFind's trigger) open
 * it first; a no-op when it is already visible.
 */
export async function openSpacesIndex(page: Page): Promise<void> {
  const panel = page.getByTestId("spaces-panel");
  const toggle = page.getByTestId("spaces-index-open");
  // The panel stays mounted (its QuickFind hotkey is page-wide), so "in the
  // DOM" is not "open". Wait for the desktop header to render, then let the
  // toggle's aria-expanded decide — a visibility probe can race the mount and
  // read a hidden-but-open panel as closed, which would CLOSE it here.
  await expect(toggle).toBeVisible();
  if ((await toggle.getAttribute("aria-expanded")) !== "true") {
    await toggle.click();
  }
  await expect(panel).toBeVisible();
}
