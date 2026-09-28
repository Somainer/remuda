import { lazy, Suspense } from "react";
import { Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { AuthGate } from "./AuthGate";
import { Shell } from "./Shell";
import { PhoneShell } from "./PhoneShell";
// Login stays in the initial chunk: it is the unauthed first paint and must
// not wait on a route chunk. Every authed surface is a separate chunk
// (c-perffu UO-11: the eager all-pages bundle made cold navigation parse ~1.5
// MB of JS in one long task).
import { LoginPage } from "../pages/LoginPage";
import { resolveLanding } from "../lib/mobileRoute";
import { useWorkbenchViewport } from "../lib/viewport";

const SessionsPage = lazy(() =>
  import("../pages/SessionsPage").then((m) => ({ default: m.SessionsPage })),
);
const NewSessionPage = lazy(() =>
  import("../pages/NewSessionPage").then((m) => ({ default: m.NewSessionPage })),
);
const SessionPage = lazy(() =>
  import("../pages/SessionPage").then((m) => ({ default: m.SessionPage })),
);
const ApprovalsPage = lazy(() =>
  import("../pages/ApprovalsPage").then((m) => ({ default: m.ApprovalsPage })),
);
const HostsPage = lazy(() =>
  import("../pages/HostsPage").then((m) => ({ default: m.HostsPage })),
);
const HostDetailPage = lazy(() =>
  import("../pages/HostsPage").then((m) => ({ default: m.HostDetailPage })),
);
const FleetPage = lazy(() =>
  import("../pages/FleetPage").then((m) => ({ default: m.FleetPage })),
);
const ProjectsPage = lazy(() =>
  import("../pages/ProjectsPage").then((m) => ({ default: m.ProjectsPage })),
);
const ProjectDetailPage = lazy(() =>
  import("../pages/ProjectsPage").then((m) => ({ default: m.ProjectDetailPage })),
);
const ProvidersPage = lazy(() =>
  import("../pages/ProvidersPage").then((m) => ({ default: m.ProvidersPage })),
);
const ProviderDetailPage = lazy(() =>
  import("../pages/ProvidersPage").then((m) => ({ default: m.ProviderDetailPage })),
);
const BotsPage = lazy(() =>
  import("../pages/BotsPage").then((m) => ({ default: m.BotsPage })),
);
const BotDetailPage = lazy(() =>
  import("../pages/BotsPage").then((m) => ({ default: m.BotDetailPage })),
);
const SettingsPage = lazy(() =>
  import("../pages/SettingsPage").then((m) => ({ default: m.SettingsPage })),
);
const SubagentView = lazy(() =>
  import("../features/session/subagent/SubagentView").then((m) => ({ default: m.SubagentView })),
);
const Inbox = lazy(() =>
  import("../features/mobile/Inbox").then((m) => ({ default: m.Inbox })),
);
const HomeList = lazy(() =>
  import("../features/mobile/HomeList").then((m) => ({ default: m.HomeList })),
);
const BoardPage = lazy(() =>
  import("../features/tasks/Board").then((m) => ({ default: m.BoardPage })),
);

/** Minimal fallback while a route chunk loads; AuthGate owns its own state. */
function RouteFallback() {
  return (
    <p
      style={{
        margin: 0,
        minHeight: "100dvh",
        padding: "var(--space-5)",
        background: "var(--bg-canvas)",
        color: "var(--fg-muted)",
      }}
    >
      加载中…
    </p>
  );
}

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
                <Suspense fallback={<RouteFallback />}>
                  <SessionsPage />
                </Suspense>
              }
            />
            <Route
              path="/approvals"
              element={
                <Suspense fallback={<RouteFallback />}>
                  <ApprovalsPage />
                </Suspense>
              }
            />
            <Route
              path="/board"
              element={
                <Suspense fallback={<RouteFallback />}>
                  <BoardPage />
                </Suspense>
              }
            />
          </Route>
          <Route
            path="/sessions/new"
            element={
              <Suspense fallback={<RouteFallback />}>
                <NewSessionPage />
              </Suspense>
            }
          />
          <Route
            path="/s/:instanceId"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SessionPage />
              </Suspense>
            }
          />
          <Route
            path="/s/:instanceId/tty"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SessionPage view="tty" />
              </Suspense>
            }
          />
          <Route
            path="/s/:instanceId/structured"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SessionPage view="structured" />
              </Suspense>
            }
          />
          <Route
            path="/s/:instanceId/files"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SessionPage view="files" />
              </Suspense>
            }
          />
          <Route
            path="/s/:instanceId/events"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SessionPage view="events" />
              </Suspense>
            }
          />
          <Route
            path="/s/:instanceId/agents/:agentId"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SubagentView />
              </Suspense>
            }
          />
          <Route
            path="/hosts"
            element={
              <Suspense fallback={<RouteFallback />}>
                <HostsPage />
              </Suspense>
            }
          />
          <Route
            path="/hosts/:hostId"
            element={
              <Suspense fallback={<RouteFallback />}>
                <HostDetailPage />
              </Suspense>
            }
          />
          <Route
            path="/fleet"
            element={
              <Suspense fallback={<RouteFallback />}>
                <FleetPage />
              </Suspense>
            }
          />
          <Route
            path="/projects"
            element={
              <Suspense fallback={<RouteFallback />}>
                <ProjectsPage />
              </Suspense>
            }
          />
          <Route
            path="/projects/:workspaceId"
            element={
              <Suspense fallback={<RouteFallback />}>
                <ProjectDetailPage />
              </Suspense>
            }
          />
          <Route
            path="/providers"
            element={
              <Suspense fallback={<RouteFallback />}>
                <ProvidersPage />
              </Suspense>
            }
          />
          <Route
            path="/providers/:profileId"
            element={
              <Suspense fallback={<RouteFallback />}>
                <ProviderDetailPage />
              </Suspense>
            }
          />
          <Route
            path="/bots"
            element={
              <Suspense fallback={<RouteFallback />}>
                <BotsPage />
              </Suspense>
            }
          />
          <Route
            path="/bots/:channelId"
            element={
              <Suspense fallback={<RouteFallback />}>
                <BotDetailPage />
              </Suspense>
            }
          />
          <Route
            path="/settings"
            element={
              <Suspense fallback={<RouteFallback />}>
                <SettingsPage />
              </Suspense>
            }
          />
        </Route>
        <Route element={<ViewportGate />}>
          <Route
            path="/m"
            element={<PhoneShell />}
          >
            <Route
              index
              element={
                <Suspense fallback={<RouteFallback />}>
                  <HomeList />
                </Suspense>
              }
            />
            <Route
              path="inbox"
              element={
                <Suspense fallback={<RouteFallback />}>
                  <Inbox />
                </Suspense>
              }
            />
            <Route path="*" element={<Navigate to="/m" replace />} />
          </Route>
        </Route>
      </Route>
      <Route path="*" element={<Navigate to="/sessions" replace />} />
    </Routes>
  );
}
