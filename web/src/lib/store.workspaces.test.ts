import { afterEach, expect, it, vi } from "vitest";
import { api } from "./api";
import { hubStore } from "./store";
import { mockDb } from "./mock";
import type { WorkspaceSnapshot } from "../types/workspace";

afterEach(() => { hubStore.logout(); vi.restoreAllMocks(); });

it("updates the workspace picker store from follow and rejects stale refresh and mutation snapshots", async () => {
  const host = { ...mockDb.hosts[0], workspaceRevision: 1, workspaces: [] };
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [host], nextCursor: null });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  let update: ((snapshot: WorkspaceSnapshot) => void) | undefined;
  const stop = vi.fn();
  vi.spyOn(api, "hostWorkspaceSubscribe").mockImplementation((onSnapshot) => { update = onSnapshot; return stop; });
  await hubStore.bootstrap();
  const workspace = { workspaceId: "wsp_project", hostId: host.id, root: "/home/dev/projects/app" };
  update!({ hostId: host.id, workspaceRevision: 3, workspaces: [workspace] });
  expect(hubStore.getSnapshot().workspaces[0]?.rootPath).toBe(workspace.root);
  await hubStore.refreshHosts();
  expect(hubStore.getSnapshot().workspaces[0]?.id).toBe(workspace.workspaceId);
  vi.spyOn(api, "workspaceUnregister").mockResolvedValue({ items: [], workspaceRevision: 2, nextCursor: null });
  await hubStore.unregisterWorkspace(host.id, workspace.root);
  expect(hubStore.getSnapshot().workspaces).toHaveLength(1);
  update!({ hostId: host.id, workspaceRevision: 4, workspaces: [] });
  expect(hubStore.getSnapshot().workspaces).toEqual([]);
  hubStore.logout();
  expect(stop).toHaveBeenCalledOnce();
});
