/* ------------------------------------------------------------------ */
/* Appearance — the single source of truth for system/dark/light.     */
/* tokens.css resolves "system" in pure CSS (prefers-color-scheme),   */
/* so this module only stamps an explicit choice onto <html           */
/* data-appearance>. main.tsx applies it once before React mounts;    */
/* switching modes only touches attributes, never React state.        */
/* ------------------------------------------------------------------ */

export const APPEARANCE_KEY = "runtime.theme.v1";

export type Appearance = "system" | "dark" | "light";
export type ResolvedAppearance = "dark" | "light";

const LIGHT_QUERY = "(prefers-color-scheme: light)";

/** Read the stored choice. Legacy values map across (night → dark,
 *  ledger → light); a missing key, an unknown value or a thrown getItem
 *  all mean "system". */
export function readAppearance(): Appearance {
  let raw: string | null = null;
  try {
    raw = localStorage.getItem(APPEARANCE_KEY);
  } catch {
    return "system";
  }
  if (raw === "dark" || raw === "night") return "dark";
  if (raw === "light" || raw === "ledger") return "light";
  return "system";
}

function systemIsLight(): boolean {
  return typeof window.matchMedia === "function" && window.matchMedia(LIGHT_QUERY).matches;
}

/** The mode actually on screen for a choice. */
export function resolveAppearance(choice: Appearance): ResolvedAppearance {
  if (choice !== "system") return choice;
  return systemIsLight() ? "light" : "dark";
}

// index.html ships one theme-color meta per scheme; their original content
// is snapshotted on first touch so "system" can hand them back untouched.
const metaDefaults = new WeakMap<HTMLMetaElement, string>();

function themeColorMetas(): HTMLMetaElement[] {
  const metas = [...document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')];
  for (const meta of metas) {
    if (!metaDefaults.has(meta)) metaDefaults.set(meta, meta.content);
  }
  return metas;
}

// Transition mirror (removed by UO-1b): unmigrated [data-theme] selectors
// keep working off data-theme="night|ledger". In system mode a media
// listener keeps only this mirror in step; new code never reads it.
let systemQuery: MediaQueryList | null = null;

function mirror(resolved: ResolvedAppearance): void {
  document.documentElement.dataset.theme = resolved === "light" ? "ledger" : "night";
}

function onSystemChange(event: MediaQueryListEvent): void {
  holdTransitions();
  mirror(event.matches ? "light" : "dark");
}

// A mode switch is instant (visual-system.md §7.4): controls that transition
// colour on hover must not fade into the new palette. tokens.css turns every
// transition off while <html data-mode-switch> is set; it is cleared two
// frames later, after the frame that painted the new mode.
let holdFrame = 0;

function holdTransitions(): void {
  if (typeof requestAnimationFrame !== "function") return;
  const root = document.documentElement;
  root.setAttribute("data-mode-switch", "");
  cancelAnimationFrame(holdFrame);
  holdFrame = requestAnimationFrame(() => {
    holdFrame = requestAnimationFrame(() => root.removeAttribute("data-mode-switch"));
  });
}

function followSystem(on: boolean): void {
  if (on && !systemQuery && typeof window.matchMedia === "function") {
    systemQuery = window.matchMedia(LIGHT_QUERY);
    systemQuery.addEventListener("change", onSystemChange);
  } else if (!on && systemQuery) {
    systemQuery.removeEventListener("change", onSystemChange);
    systemQuery = null;
  }
}

/** Stamp the choice onto <html>. Idempotent. */
export function applyAppearance(choice: Appearance): void {
  const root = document.documentElement;
  const metas = themeColorMetas();
  holdTransitions();
  if (choice === "system") {
    root.removeAttribute("data-appearance");
    for (const meta of metas) meta.content = metaDefaults.get(meta) ?? meta.content;
  } else {
    root.dataset.appearance = choice;
    const canvas = getComputedStyle(root).getPropertyValue("--bg-canvas").trim();
    if (canvas) for (const meta of metas) meta.content = canvas;
  }
  followSystem(choice === "system");
  mirror(resolveAppearance(choice));
}

/** Persist the choice and apply it. A denied write throws (so the settings
 *  save flow reports failure and rolls back) and leaves the DOM untouched —
 *  the caller reapplies the last committed value. */
export function writeAppearance(choice: Appearance): void {
  try {
    localStorage.setItem(APPEARANCE_KEY, choice);
  } catch {
    throw new Error("本地存储不可用");
  }
  applyAppearance(choice);
}
