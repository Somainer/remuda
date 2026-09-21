import { Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { AuthGate } from "./AuthGate";
import { Shell } from "./Shell";
import { PhoneShell } from "./PhoneShell";
import { LoginPage } from "../pages/LoginPage";
import { SessionsPage } from "../pages/SessionsPage";
import { NewSessionPage } from "../pages/NewSessionPage";
import { SessionPage } from "../pages/SessionPage";
import { ApprovalsPage } from "../pages/ApprovalsPage";
import { HostsPage, HostDetailPage } from "../pages/HostsPage";
import { FleetPage } from "../pages/FleetPage";
import { ProjectsPage, ProjectDetailPage } from "../pages/ProjectsPage";
import { ProvidersPage, ProviderDetailPage } from "../pages/ProvidersPage";
import { BotsPage, BotDetailPage } from "../pages/BotsPage";
import { SettingsPage } from "../pages/SettingsPage";
import { SubagentView } from "../features/session/subagent/SubagentView";
import { Inbox } from "../features/mobile/Inbox";
import { HomeList } from "../features/mobile/HomeList";
import { TaskListPage } from "../features/tasks/TaskList";
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
            <Route path="/sessions" element={<SessionsPage />} />
            <Route path="/approvals" element={<ApprovalsPage />} />
            <Route path="/board" element={<TaskListPage />} />
          </Route>
          <Route path="/sessions/new" element={<NewSessionPage />} />
          <Route path="/s/:instanceId" element={<SessionPage />} />
          <Route path="/s/:instanceId/tty" element={<SessionPage view="tty" />} />
          <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
          <Route path="/s/:instanceId/files" element={<SessionPage view="files" />} />
          <Route path="/s/:instanceId/events" element={<SessionPage view="events" />} />
          <Route path="/s/:instanceId/agents/:agentId" element={<SubagentView />} />
          <Route path="/hosts" element={<HostsPage />} />
          <Route path="/hosts/:hostId" element={<HostDetailPage />} />
          <Route path="/fleet" element={<FleetPage />} />
          <Route path="/projects" element={<ProjectsPage />} />
          <Route path="/projects/:workspaceId" element={<ProjectDetailPage />} />
          <Route path="/providers" element={<ProvidersPage />} />
          <Route path="/providers/:profileId" element={<ProviderDetailPage />} />
          <Route path="/bots" element={<BotsPage />} />
          <Route path="/bots/:channelId" element={<BotDetailPage />} />
          <Route path="/settings" element={<SettingsPage />} />
        </Route>
        <Route element={<ViewportGate />}>
          <Route path="/m" element={<PhoneShell />}>
            <Route index element={<HomeList />} />
            <Route path="inbox" element={<Inbox />} />
            <Route path="*" element={<Navigate to="/m" replace />} />
          </Route>
        </Route>
      </Route>
      <Route path="*" element={<Navigate to="/sessions" replace />} />
    </Routes>
  );
}
