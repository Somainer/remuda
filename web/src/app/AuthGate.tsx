import { Fragment } from "react";
import { Navigate, Outlet, useLocation } from "react-router-dom";
import { useHub } from "../lib/store";
import { QuestionAlertHost } from "../features/approvals/QuestionAlertHost";

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
  // c-question-alert r2: mounted in the SHARED authenticated root, not in
  // Shell — /m and /m/inbox render PhoneShell instead, so a fresh phone entry
  // never started the watcher. The Fragment keeps the route outlet layout
  // identical; the watcher itself is a singleton that survives the
  // Shell <-> PhoneShell switch without re-baselining.
  return (
    <Fragment>
      <QuestionAlertHost />
      <Outlet />
    </Fragment>
  );
}
