/**
 * Explicit opt-in for test-only `window.__*Lab` seams (c-composerpop r2
 * item 4). The seams must not exist in a production session: they let a page
 * post standing notifications and inject Hub-computed usage rollups. Hub e2e
 * specs turn them on with `addInitScript(() => { window.__remudaE2E = true })`
 * (see tests/e2e/hub-auth.ts) before the app boots; production never sets the
 * marker, so the seams stay uninstalled and inert.
 */
export const E2E_SEAM_FLAG = "__remudaE2E" as const;

declare global {
  interface Window {
    __remudaE2E?: boolean;
  }
}

export function e2eSeamsEnabled(): boolean {
  return typeof window !== "undefined" && window[E2E_SEAM_FLAG] === true;
}
