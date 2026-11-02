/** Deep-link a Hub Web Push payload to a client route. */

export function resolvePushDeepLink(payload: { tag?: string; data?: { url?: string } }): string {
  const raw = payload.data?.url?.trim();
  if (raw) {
    if (raw.startsWith("/")) return raw;
    try {
      const parsed = new URL(raw);
      if (typeof location !== "undefined" && parsed.origin === location.origin) {
        return `${parsed.pathname}${parsed.search}${parsed.hash}`;
      }
    } catch {
      /* fall through to tag */
    }
  }
  const tag = payload.tag ?? "";
  // home.md §9: an interaction lands on the home with its ④ card focused
  // (a phone keeps the query through the /sessions → /m redirect).
  if (tag.startsWith("interaction:")) {
    return `/sessions?focus=${encodeURIComponent(tag.slice("interaction:".length))}`;
  }
  if (tag.startsWith("instance:")) {
    return `/s/${encodeURIComponent(tag.slice("instance:".length))}`;
  }
  return "/sessions";
}
