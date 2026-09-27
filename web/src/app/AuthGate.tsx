import { Navigate, Outlet, useLocation } from "react-router-dom";
import { useHub } from "../lib/store";

export function AuthGate() {
  const hub = useHub();
  const location = useLocation();
  if (!hub.ready) {
    return (
      <p
        style={{
          margin: 0,
          minHeight: "100dvh",
          padding: "var(--space-5)",
          background: "var(--bg-canvas)",
          color: "var(--fg-muted)",
        }}
        data-testid="auth-loading"
      >
        加载中…
      </p>
    );
  }
  if (!hub.authed) return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  return <Outlet />;
}
