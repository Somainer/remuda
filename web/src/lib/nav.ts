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

export function isSessionRoute(pathname: string): boolean {
  return pathname === "/sessions" || pathname.startsWith("/sessions/") || pathname.startsWith("/s/");
}
