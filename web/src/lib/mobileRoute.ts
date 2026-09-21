/**
 * D-049 viewport redirect layer (ui-spec.md §1.2 / §4.7), as a pure function
 * so deep-link query preservation is unit-testable without a router.
 *
 * Returns the destination (pathname + verbatim search) for a
 * `<Navigate replace>` element, or null when the current URL stays where it
 * is. The search string is never parsed or rebuilt — a push deep link such as
 * `/approvals?focus=int_x` must land on `/m/inbox?focus=int_x` byte for byte.
 *
 * Rules:
 * - `/` is the manifest start_url: compact lands on `/m`, desktop on
 *   `/sessions` — one manifest, the viewport decides (D-049).
 * - compact: `/sessions` -> `/m`, `/approvals` -> `/m/inbox`, `/board` ->
 *   `/m` (the desktop task board collapses onto the phone home task layer,
 *   D-050 / ui-spec §2.9).
 * - desktop: the whole `/m*` tree bounces back to `/sessions`.
 * - `/s/:id*`, `/sessions/new`, `/login`, `/pair`, `/settings` are shared
 *   routes and return null on BOTH sides; `/s/:id` is the same session
 *   projection under either shell and must never be rewritten.
 */
export function resolveLanding(pathname: string, search: string, compact: boolean): string | null {
  // Treat a single trailing slash as absent ("/sessions/" === "/sessions"),
  // mirroring the /m/ tolerance already in the desktop branch. The bare
  // root keeps its slash.
  const path = pathname.length > 1 && pathname.endsWith("/") ? pathname.slice(0, -1) : pathname;
  // PWA start_url: no query belongs to the bare root.
  if (path === "/") return compact ? "/m" : "/sessions";
  if (compact) {
    if (path === "/sessions" || path === "/board") return `/m${search}`;
    if (path === "/approvals") return `/m/inbox${search}`;
    return null;
  }
  // Desktop never renders the phone tree (including unknown /m/* branches).
  if (path === "/m" || path.startsWith("/m/")) return "/sessions";
  return null;
}
