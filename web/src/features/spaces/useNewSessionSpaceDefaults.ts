import { useHub } from "../../lib/store";
import { spaceKey, useSpacesPrefs } from "./store";

/** Query parameters in NewSessionPage retain precedence over the current project. */
export function useNewSessionSpaceDefaults() {
  const { workspaces } = useHub();
  const prefs = useSpacesPrefs();
  const workspace = workspaces.find((row) => spaceKey(row.hostId, row.id) === prefs.selectedSpaceId);
  return workspace ? { hostId: workspace.hostId, workspaceId: workspace.id } : {};
}
