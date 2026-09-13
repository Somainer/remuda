import { afterEach, expect, it, vi } from "vitest";
import { followWorkspaces } from "./follow";
import { mergeHostWorkspaces } from "./registry";
import { mockDb } from "../../lib/mock";

afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });

it("does not let a late HTTP snapshot replace a newer workspace event", () => {
  const current = { ...mockDb.hosts[0], workspaceRevision: 4, workspaces: [] };
  const stale = { ...current, workspaceRevision: 3, label: "new host name", workspaces: [
    { workspaceId: "wsp_removed", hostId: current.id, root: "/home/dev/removed" },
  ] };
  expect(mergeHostWorkspaces([stale], [current])).toEqual([{ ...current, label: "new host name" }]);
  expect(mergeHostWorkspaces([{ ...stale, workspaceRevision: 5 }], [current])[0].workspaces).toEqual(stale.workspaces);
});

it("applies host.updated, refreshes on gaps and reconnects, and stops after logout", () => {
  vi.useFakeTimers();
  const sockets: FakeSocket[] = [];
  class FakeSocket extends EventTarget {
    constructor(_url: string) { super(); sockets.push(this); }
    close() { this.dispatchEvent(new Event("close")); }
    message(value: unknown) { this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify(value) })); }
  }
  vi.stubGlobal("WebSocket", FakeSocket);
  const update = vi.fn();
  const refresh = vi.fn();
  const stop = followWorkspaces("ws://localhost/v1/follow", update, refresh);
  sockets[0].dispatchEvent(new Event("open"));
  const event = { type: "host.updated", hostId: "hst_node", workspaceRevision: 2,
    workspaces: [{ workspaceId: "wsp_app", hostId: "hst_node", root: "/home/dev/app" }] };
  sockets[0].message({ type: "event", event });
  expect(update).toHaveBeenCalledWith(event);
  sockets[0].message({ type: "event", event: { ...event, workspaceRevision: "invalid" } });
  expect(update).toHaveBeenCalledOnce();
  sockets[0].message({ type: "gap" });
  expect(refresh).toHaveBeenCalledTimes(2);
  sockets[0].close();
  vi.advanceTimersByTime(1000);
  expect(sockets).toHaveLength(2);
  stop();
  vi.advanceTimersByTime(2000);
  expect(sockets).toHaveLength(2);
});
