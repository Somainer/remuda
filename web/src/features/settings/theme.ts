/* ------------------------------------------------------------------ */
/* Theme — the single source of truth for the night/ledger choice.    */
/* tokens.css carries both palettes; this module only chooses one and */
/* stamps <html data-theme>. main.tsx applies it once before React    */
/* mounts, so every route paints the stored choice on its first frame.*/
/* ------------------------------------------------------------------ */

export const THEME_KEY = "runtime.theme.v1";

export type ThemeChoice = "night" | "ledger";

/** Read the stored choice. Missing storage, a thrown getItem, or a value
 *  outside the known set all fall back to "night". */
export function readTheme(): ThemeChoice {
  try {
    return localStorage.getItem(THEME_KEY) === "ledger" ? "ledger" : "night";
  } catch {
    return "night";
  }
}

/** Stamp the choice onto <html>. Idempotent: applying the same value twice
 *  is a no-op observable state change. */
export function applyTheme(theme: ThemeChoice): void {
  document.documentElement.dataset.theme = theme;
}

/** Persist the choice and apply it. A denied write throws (so the settings
 *  save flow reports failure and rolls back) and leaves the DOM untouched —
 *  the caller reapplies the last committed value. */
export function writeTheme(theme: ThemeChoice): void {
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    throw new Error("本地存储不可用");
  }
  applyTheme(theme);
}
