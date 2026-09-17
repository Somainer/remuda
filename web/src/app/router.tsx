import { Navigate, Route, Routes } from "react-router-dom";
import { AuthGate } from "./AuthGate";
import { Shell } from "./Shell";
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

export function AppRouter() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route path="/pair" element={<LoginPage mode="pair" />} />
      <Route element={<AuthGate />}>
        <Route element={<Shell />}>
          <Route path="/" element={<Navigate to="/sessions" replace />} />
          <Route path="/sessions" element={<SessionsPage />} />
          <Route path="/sessions/new" element={<NewSessionPage />} />
          <Route path="/s/:instanceId" element={<SessionPage />} />
          <Route path="/s/:instanceId/tty" element={<SessionPage view="tty" />} />
          <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
          <Route path="/s/:instanceId/files" element={<SessionPage view="files" />} />
          <Route path="/s/:instanceId/events" element={<SessionPage view="events" />} />
          <Route path="/s/:instanceId/agents/:agentId" element={<SubagentView />} />
          <Route path="/approvals" element={<ApprovalsPage />} />
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
      </Route>
      <Route path="*" element={<Navigate to="/sessions" replace />} />
    </Routes>
  );
}
