export const PRIMARY_NAV = [
  { id: "sessions", to: "/sessions", label: "会话" },
  { id: "approvals", to: "/approvals", label: "审批" },
  { id: "new", to: "/sessions/new", label: "新建" },
] as const;

export const MORE_NAV = [
  { id: "hosts", to: "/hosts", label: "主机" },
  { id: "projects", to: "/projects", label: "项目" },
  { id: "providers", to: "/providers", label: "Provider" },
  { id: "bots", to: "/bots", label: "Bot" },
  { id: "settings", to: "/settings", label: "设置" },
] as const;

/**
 * Phone bottom bar (D-049 / ui-spec §4.7): 会话 · 收件箱(n) · 新建 · 更多.
 * Mounted only by the `/m*` phone shell; the shared `/s/:id` route renders no
 * app bottom bar in compact. `more` has no destination — it opens MORE_NAV.
 */
export const PHONE_NAV = [
  { id: "home", to: "/m", label: "会话", glyph: "▤" },
  { id: "inbox", to: "/m/inbox", label: "收件箱", glyph: "◆" },
  { id: "new", to: "/sessions/new", label: "新建", glyph: "＋" },
  { id: "more", to: null, label: "更多", glyph: "⋯" },
] as const;

export function isSessionRoute(pathname: string): boolean {
  return pathname === "/sessions" || pathname.startsWith("/sessions/") || pathname.startsWith("/s/");
}
