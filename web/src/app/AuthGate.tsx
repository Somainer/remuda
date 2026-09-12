import { Navigate, Outlet, useLocation } from "react-router-dom";
import { useHub } from "../lib/store";

export function AuthGate() {
  const hub = useHub();
  const location = useLocation();
  if (!hub.ready) {
    return (
      <p style={{ padding: 24, color: "var(--mute)", background: "var(--ink)", minHeight: "100dvh" }} data-testid="auth-loading">
        加载中…
      </p>
    );
  }
  if (!hub.authed) return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  return <Outlet />;
}
