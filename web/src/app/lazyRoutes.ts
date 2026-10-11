import { createLazyRoute } from "./routeBoundary";

// Module scope, one DISTINCT component type per page. Never create these
// inside render: a shared inline element type was the r3 navigation bug
// (React reuses one <LazyRoute> instance across sibling routes). Both
// router.tsx and Shell.tsx import these — keeping them out of router.tsx
// avoids a Shell <-> router import cycle.
// Module scope, one DISTINCT component type per page. Never create these
// inside render: a shared inline element type was the r3 navigation bug.
export const SessionsRoute = createLazyRoute<{ dimmed?: boolean }>(
  () => import("../pages/SessionsPage"),
  "SessionsPage",
);
export const NewSessionRoute = createLazyRoute(
  () => import("../pages/NewSessionPage"),
  "NewSessionPage",
);
export const SessionRoute = createLazyRoute<{ view?: "auto" | "structured" | "tty" | "files" | "events" }>(
  () => import("../pages/SessionPage"),
  "SessionPage",
);
export const ApprovalsRoute = createLazyRoute(() => import("../pages/ApprovalsPage"), "ApprovalsPage");
export const HostsRoute = createLazyRoute(() => import("../pages/HostsPage"), "HostsPage");
export const HostDetailRoute = createLazyRoute(() => import("../pages/HostsPage"), "HostDetailPage");
export const FleetRoute = createLazyRoute(() => import("../pages/FleetPage"), "FleetPage");
export const ProjectsRoute = createLazyRoute(() => import("../pages/ProjectsPage"), "ProjectsPage");
export const ProjectDetailRoute = createLazyRoute(
  () => import("../pages/ProjectsPage"),
  "ProjectDetailPage",
);
export const ProvidersRoute = createLazyRoute(
  () => import("../pages/ProvidersPage"),
  "ProvidersPage",
);
export const ProviderDetailRoute = createLazyRoute(
  () => import("../pages/ProvidersPage"),
  "ProviderDetailPage",
);
export const BotsRoute = createLazyRoute(() => import("../pages/BotsPage"), "BotsPage");
export const BotDetailRoute = createLazyRoute(() => import("../pages/BotsPage"), "BotDetailPage");
export const SettingsRoute = createLazyRoute(() => import("../pages/SettingsPage"), "SettingsPage");
export const SubagentRoute = createLazyRoute(
  () => import("../features/session/subagent/SubagentView"),
  "SubagentView",
);
export const HomeListRoute = createLazyRoute(
  () => import("../features/mobile/HomeList"),
  "HomeList",
);
export const InboxRoute = createLazyRoute(() => import("../features/mobile/Inbox"), "Inbox");
export const BoardRoute = createLazyRoute(
  () => import("../features/tasks/Board"),
  "BoardPage",
);

