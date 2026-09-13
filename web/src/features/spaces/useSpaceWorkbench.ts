import { useLocation, useNavigate } from "react-router-dom";
import { useHub } from "../../lib/store";
import { buildSpaces, newSessionPath, selectedSpace, selectedTab, spaceKey, spaceStore, useSpacesPrefs, visibleTabs, type Space } from "./store";

export function useSpaceWorkbench() {
  const hub = useHub();
  const prefs = useSpacesPrefs();
  const location = useLocation();
  const navigate = useNavigate();
  const instanceId = location.pathname.startsWith("/s/") ? location.pathname.split("/")[2] : undefined;
  const spaces = buildSpaces(hub.workspaces, hub.instances, prefs);
  const params = new URLSearchParams(location.search);
  const newSpace = location.pathname === "/sessions/new" && params.has("host") && params.has("workspace")
    ? spaces.find((space) => space.id === spaceKey(params.get("host")!, params.get("workspace")!)) : undefined;
  const active = newSpace ?? selectedSpace(spaces, prefs, instanceId);
  const tabs = active ? visibleTabs(active, prefs) : [];
  const select = (space: Space) => {
    spaceStore.selectSpace(space.id);
    const tab = selectedTab(space, prefs);
    navigate(tab ? `/s/${tab.id}` : "/sessions");
  };
  return { spaces, active, tabs, prefs, instanceId, select, newHref: newSessionPath(active) };
}
