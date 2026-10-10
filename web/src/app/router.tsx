import { Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { AuthGate } from "./AuthGate";
import { Shell } from "./Shell";
import { PhoneShell } from "./PhoneShell";
// Login stays in the initial chunk: it is the unauthed first paint and must
// not wait on a route chunk. Every authed surface is a separate chunk
// (c-perffu UO-11: the eager all-pages bundle made cold navigation parse ~1.5
// MB of JS in one long task), wrapped in a per-page LazyRoute boundary (r2:
// rejected offline/stale chunk keeps the Shell mounted with re-import/
// reload; r3: each page is its OWN component type so sibling navigation
// unmounts the old page instead of re-rendering it).
import { LoginPage } from "../pages/LoginPage";
import {
  ApprovalsRoute,
  BoardRoute,
  BotDetailRoute,
  BotsRoute,
  FleetRoute,
  HomeListRoute,
  HostDetailRoute,
  HostsRoute,
  InboxRoute,
  NewSessionRoute,
  ProjectDetailRoute,
  ProjectsRoute,
  ProviderDetailRoute,
  ProvidersRoute,
  SessionRoute,
  SessionsRoute,
  SettingsRoute,
  SubagentRoute,
} from "./lazyRoutes";
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
            <Route path="/sessions" element={<SessionsRoute />} />
            <Route path="/approvals" element={<ApprovalsRoute />} />
            <Route path="/board" element={<BoardRoute />} />
          </Route>
          <Route path="/sessions/new" element={<NewSessionRoute />} />
          <Route path="/s/:instanceId" element={<SessionRoute />} />
          <Route path="/s/:instanceId/tty" element={<SessionRoute view="tty" />} />
          <Route
            path="/s/:instanceId/structured"
            element={<SessionRoute view="structured" />}
          />
          <Route path="/s/:instanceId/files" element={<SessionRoute view="files" />} />
          <Route path="/s/:instanceId/events" element={<SessionRoute view="events" />} />
          <Route path="/s/:instanceId/agents/:agentId" element={<SubagentRoute />} />
          <Route path="/hosts" element={<HostsRoute />} />
          <Route path="/hosts/:hostId" element={<HostDetailRoute />} />
          <Route path="/fleet" element={<FleetRoute />} />
          <Route path="/projects" element={<ProjectsRoute />} />
          <Route path="/projects/:workspaceId" element={<ProjectDetailRoute />} />
          <Route path="/providers" element={<ProvidersRoute />} />
          <Route path="/providers/:profileId" element={<ProviderDetailRoute />} />
          <Route path="/bots" element={<BotsRoute />} />
          <Route path="/bots/:channelId" element={<BotDetailRoute />} />
          <Route path="/settings" element={<SettingsRoute />} />
        </Route>
        <Route element={<ViewportGate />}>
          <Route path="/m" element={<PhoneShell />}>
            <Route index element={<HomeListRoute />} />
            <Route path="inbox" element={<InboxRoute />} />
            <Route path="*" element={<Navigate to="/m" replace />} />
          </Route>
        </Route>
      </Route>
      <Route path="*" element={<Navigate to="/sessions" replace />} />
    </Routes>
  );
}
