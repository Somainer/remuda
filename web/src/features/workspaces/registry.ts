import type { Host } from "../../types/instance";
import { known } from "../../types/wire";
import type { RegisteredWorkspace, Workspace } from "../../types/workspace";

export function mapWorkspace(row: RegisteredWorkspace): Workspace {
  return {
    id: row.workspaceId, hostId: row.hostId, rootPath: row.root,
    label: row.root.split("/").filter(Boolean).at(-1) ?? row.root,
    revision: "1", createdAt: "", updatedAt: "",
    writePolicy: "default", canonicalRoot: known(row.root),
  };
}

/** An in-flight HTTP snapshot must not undo a newer journal event. */
export function mergeHostWorkspaces(incoming: Host[], current: Host[]): Host[] {
  return incoming.map((host) => {
    const previous = current.find((row) => row.id === host.id);
    return previous && (previous.workspaceRevision ?? 0) > (host.workspaceRevision ?? 0)
      ? { ...host, workspaceRevision: previous.workspaceRevision, workspaces: previous.workspaces }
      : host;
  });
}
