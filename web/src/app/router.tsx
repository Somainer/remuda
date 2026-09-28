import { Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { AuthGate } from "./AuthGate";
import { Shell } from "./Shell";
import { PhoneShell } from "./PhoneShell";
// Login stays in the initial chunk: it is the unauthed first paint and must
// not wait on a route chunk. Every authed surface is a separate chunk
// (c-perffu UO-11: the eager all-pages bundle made cold navigation parse ~1.5
// MB of JS in one long task), wrapped in a LazyRoute boundary (r2: a rejected
// offline/stale chunk keeps the Shell mounted and offers re-import/reload).
import { LoginPage } from "../pages/LoginPage";
import { LazyRoute } from "./routeBoundary";
import { resolveLanding } from "../lib/mobileRoute";
import { useWorkbenchViewport } from "../lib/viewport";

/**
 * D-049 viewport redirect layer (ui-spec §1.2 / §4.7). A pathless layout
 * element that either renders its subtree or a `<Navigate replace>` derived
 * from the pure resolveLanding(). Mounted around both the desktop index
 * routes (compact bounces /sessions and /approvals into /m) and the /m tree
 * (desktop bounces back to /sessions). Re-renders live on viewport changes.
 */
function ViewportGate() {
  const { mobile } = useWorkbenchViewport();
  const location = useLocation();
  const target = resolveLanding(location.pathname, location.search, mobile);
  if (target) return <Navigate to={target} replace />;
  return <Outlet />;
}

/**
 * PWA start_url "/": the same pure resolver picks /m (compact) or
 * /sessions, so "/" carries no decision of its own.
 */
function RootLanding() {
  const { mobile } = useWorkbenchViewport();
  return <Navigate to={resolveLanding("/", "", mobile) ?? "/sessions"} replace />;
}

export function AppRouter() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route path="/pair" element={<LoginPage mode="pair" />} />
      <Route element={<AuthGate />}>
        <Route element={<Shell />}>
          <Route path="/" element={<RootLanding />} />
          <Route element={<ViewportGate />}>
            <Route
              path="/sessions"
              element={
                <LazyRoute
                  loader={() => import("../pages/SessionsPage")}
                  named="SessionsPage"
                />
              }
            />
            <Route
              path="/approvals"
              element={
                <LazyRoute
                  loader={() => import("../pages/ApprovalsPage")}
                  named="ApprovalsPage"
                />
              }
            />
            <Route
              path="/board"
              element={<LazyRoute loader={() => import("../features/tasks/Board")} named="BoardPage" />}
            />
          </Route>
          <Route
            path="/sessions/new"
            element={
              <LazyRoute
                loader={() => import("../pages/NewSessionPage")}
                named="NewSessionPage"
              />
            }
          />
          <Route
            path="/s/:instanceId"
            element={<LazyRoute loader={() => import("../pages/SessionPage")} named="SessionPage" />}
          />
          <Route
            path="/s/:instanceId/tty"
            element={
              <LazyRoute
                loader={() => import("../pages/SessionPage")}
                named="SessionPage"
                componentProps={{ view: "tty" }}
              />
            }
          />
          <Route
            path="/s/:instanceId/structured"
            element={
              <LazyRoute
                loader={() => import("../pages/SessionPage")}
                named="SessionPage"
                componentProps={{ view: "structured" }}
              />
            }
          />
          <Route
            path="/s/:instanceId/files"
            element={
              <LazyRoute
                loader={() => import("../pages/SessionPage")}
                named="SessionPage"
                componentProps={{ view: "files" }}
              />
            }
          />
          <Route
            path="/s/:instanceId/events"
            element={
              <LazyRoute
                loader={() => import("../pages/SessionPage")}
                named="SessionPage"
                componentProps={{ view: "events" }}
              />
            }
          />
          <Route
            path="/s/:instanceId/agents/:agentId"
            element={
              <LazyRoute
                loader={() => import("../features/session/subagent/SubagentView")}
                named="SubagentView"
              />
            }
          />
          <Route
            path="/hosts"
            element={<LazyRoute loader={() => import("../pages/HostsPage")} named="HostsPage" />}
          />
          <Route
            path="/hosts/:hostId"
            element={<LazyRoute loader={() => import("../pages/HostsPage")} named="HostDetailPage" />}
          />
          <Route
            path="/fleet"
            element={<LazyRoute loader={() => import("../pages/FleetPage")} named="FleetPage" />}
          />
          <Route
            path="/projects"
            element={
              <LazyRoute loader={() => import("../pages/ProjectsPage")} named="ProjectsPage" />
            }
          />
          <Route
            path="/projects/:workspaceId"
            element={
              <LazyRoute loader={() => import("../pages/ProjectsPage")} named="ProjectDetailPage" />
            }
          />
          <Route
            path="/providers"
            element={
              <LazyRoute loader={() => import("../pages/ProvidersPage")} named="ProvidersPage" />
            }
          />
          <Route
            path="/providers/:profileId"
            element={
              <LazyRoute
                loader={() => import("../pages/ProvidersPage")}
                named="ProviderDetailPage"
              />
            }
          />
          <Route
            path="/bots"
            element={<LazyRoute loader={() => import("../pages/BotsPage")} named="BotsPage" />}
          />
          <Route
            path="/bots/:channelId"
            element={<LazyRoute loader={() => import("../pages/BotsPage")} named="BotDetailPage" />}
          />
          <Route
            path="/settings"
            element={
              <LazyRoute loader={() => import("../pages/SettingsPage")} named="SettingsPage" />
            }
          />
        </Route>
        <Route element={<ViewportGate />}>
          <Route path="/m" element={<PhoneShell />}>
            <Route
              index
              element={<LazyRoute loader={() => import("../features/mobile/HomeList")} named="HomeList" />}
            />
            <Route
              path="inbox"
              element={<LazyRoute loader={() => import("../features/mobile/Inbox")} named="Inbox" />}
            />
            <Route path="*" element={<Navigate to="/m" replace />} />
          </Route>
        </Route>
      </Route>
      <Route path="*" element={<Navigate to="/sessions" replace />} />
    </Routes>
  );
}
