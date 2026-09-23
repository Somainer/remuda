import type { Page } from "@playwright/test";

/**
 * The one way e2e specs switch light/dark (visual-system.md §1).
 *
 * Sets the explicit `data-appearance` attribute the runtime would set for a
 * stored dark/light choice, plus the transition `data-theme` mirror that
 * un-migrated `[data-theme=…]` branches still key on. The old names are
 * accepted so screenshot names built from night/ledger keep working.
 *
 * Specs no longer poke `data-theme` directly: when UO-1b drops the mirror,
 * only this file changes.
 */
export type AppearanceMode = "dark" | "light" | "night" | "ledger";

export function resolveMode(mode: AppearanceMode): "dark" | "light" {
  return mode === "light" || mode === "ledger" ? "light" : "dark";
}

export async function setMode(page: Page, mode: AppearanceMode): Promise<void> {
  await page.evaluate((resolved) => {
    const html = document.documentElement;
    html.dataset.appearance = resolved;
    html.dataset.theme = resolved === "light" ? "ledger" : "night";
  }, resolveMode(mode));
}

/** Seed the stored preference before any page script runs (boot path). */
export async function seedMode(page: Page, stored: string): Promise<void> {
  await page.addInitScript((raw) => {
    try {
      localStorage.setItem("runtime.theme.v1", raw);
    } catch {
      /* storage unavailable: boot takes its default */
    }
  }, stored);
}
