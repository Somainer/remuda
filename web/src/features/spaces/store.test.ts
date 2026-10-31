import { afterEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import type { Workspace } from "../../types/workspace";
import { known } from "../../types/wire";
import {
  buildSpaces, createSpaceStore, defaultSpacePrefs, newSessionPath, OTHER_SPACE, parseSpacePrefs,
  selectedSpace, selectedTab, spaceKey, spaceSessions, SPACES_PREFS_KEY, visibleTabs,
} from "./store";

const alpha = spaceKey("host-a", "workspace-a");
const beta = spaceKey("host-a", "workspace-b");

function workspace(id = "workspace-a", hostId = "host-a", rootPath = "/home/dev/projects/alpha/"): Workspace {
  return { ...mockDb.workspaces[0], id, hostId, rootPath, label: "Ignored registration label" };
}

function instance(id: string, patch: Partial<Instance> = {}): Instance {
  return { ...mockDb.instances[0], id, hostId: "host-a", workspaceId: "workspace-a", parent: null,
    createdAt: "2026-09-13T00:00:00Z", lifecycle: "ready", connectivity: "connected", activity: known("idle"), ...patch };
}

afterEach(() => {
  localStorage.removeItem(SPACES_PREFS_KEY);
  vi.restoreAllMocks();
});

describe("space grouping and order", () => {
  it("groups every session by both host and workspace, retaining children and unmatched sessions exactly once", () => {
    const sessions = [instance("a"), instance("child", { parent: { instanceId: "a", runId: "run", commandId: "cmd" } }),
      instance("other-host", { hostId: "host-b" }), instance("missing", { workspaceId: "missing" }), instance("a")];
    const spaces = buildSpaces([workspace(), workspace(), workspace("workspace-a", "host-b")], sessions, defaultSpacePrefs());
    expect(spaces).toHaveLength(3);
    expect(spaces.find((space) => space.id === alpha)?.instances.map((item) => item.id)).toEqual(["a", "child"]);
    expect(spaces.find((space) => space.id === spaceKey("host-b", "workspace-a"))?.instances.map((item) => item.id)).toEqual(["other-host"]);
    expect(spaces.at(-1)).toMatchObject({ id: OTHER_SPACE, name: "其他", instances: [expect.objectContaining({ id: "missing" })] });
    expect(spaces.flatMap((space) => space.instances)).toHaveLength(4);
    expect(spaceKey("a:b", "c")).not.toBe(spaceKey("a", "b:c"));
  });

  it("uses root basenames and retains empty registered projects without an empty other group", () => {
    const spaces = buildSpaces([workspace(), workspace("workspace-b", "host-a", "C:\\projects\\beta\\")], [], defaultSpacePrefs());
    expect(spaces.map((space) => space.name)).toEqual(["alpha", "beta"]);
    expect(spaces.every((space) => space.liveCount === 0 && space.instances.length === 0)).toBe(true);
    expect(spaces.some((space) => space.id === OTHER_SPACE)).toBe(false);
  });

  it("counts confirmed live and blocked sessions without treating disconnected or exited sessions as live", () => {
    const sessions = [instance("idle"), instance("working", { activity: known("working") }),
      instance("blocked", { activity: known("waiting-interaction") }), instance("starting", { lifecycle: "starting" }),
      instance("exited", { lifecycle: "exited" }), instance("offline", { connectivity: "disconnected" })];
    expect(buildSpaces([workspace()], sessions, defaultSpacePrefs())[0]).toMatchObject({ liveCount: 4, blockedCount: 1 });
  });

  it("applies names and manual space order, ignores stale ids and keeps tab order stable when activity changes", () => {
    const prefs = { ...defaultSpacePrefs(), names: { [alpha]: "Alpha team" }, order: ["removed", beta] };
    const workspaces = [workspace(), workspace("workspace-b", "host-a", "/home/dev/projects/beta")];
    const sessions = [instance("z"), instance("a"), instance("old", { createdAt: "2026-09-12T00:00:00Z" })];
    const spaces = buildSpaces(workspaces, sessions, prefs);
    expect(spaces.map((space) => space.id)).toEqual([beta, alpha]);
    expect(spaces[1].name).toBe("Alpha team");
    expect(spaces[1].instances.map((item) => item.id)).toEqual(["old", "a", "z"]);
    sessions[0].updatedAt = "2026-09-14T00:00:00Z";
    sessions[0].activity = known("waiting-interaction");
    expect(buildSpaces(workspaces, sessions.reverse(), prefs)[1].instances.map((item) => item.id)).toEqual(["old", "a", "z"]);
  });
});

describe("device space preferences", () => {
  it("persists collapse, group, rename, ordering and independent selected tabs across store recreation", () => {
    const store = createSpaceStore(localStorage);
    const listener = vi.fn();
    const unsubscribe = store.subscribe(listener);
    store.setCollapsed(true);
    store.toggleGroup(alpha);
    store.rename(alpha, "  Study  ");
    store.moveSpace(beta, -1, [alpha, beta, OTHER_SPACE]);
    store.selectTab(alpha, "a2");
    store.selectTab(beta, "b1");
    store.selectSpace(alpha);
    expect(createSpaceStore(localStorage).getSnapshot()).toEqual(store.getSnapshot());
    expect(store.getSnapshot()).toMatchObject({ collapsed: true, groupCollapsed: { [alpha]: true }, names: { [alpha]: "Study" },
      order: [beta, alpha], selectedSpaceId: alpha, selectedTabs: { [alpha]: "a2", [beta]: "b1" } });
    expect(listener).toHaveBeenCalledTimes(7);
    unsubscribe();
    store.setCollapsed(false);
    expect(listener).toHaveBeenCalledTimes(7);
  });

  it("keeps the snapshot stable for no-op selection and prevents order moves past either boundary", () => {
    const store = createSpaceStore(localStorage);
    store.selectTab(alpha, "a");
    const snapshot = store.getSnapshot();
    store.selectTab(alpha, "a");
    store.moveSpace(alpha, -1, [alpha, beta]);
    store.moveSpace(beta, 1, [alpha, beta]);
    store.moveSpace("removed", 1, [alpha, beta]);
    expect(store.getSnapshot()).toBe(snapshot);
    store.setOrder([alpha, alpha, OTHER_SPACE, beta]);
    expect(store.getSnapshot().order).toEqual([alpha, beta]);
    store.rename(alpha, "Alias");
    store.rename(alpha, " ");
    expect(store.getSnapshot().names[alpha]).toBeUndefined();
  });

  it("rejects malformed or unsupported versions and validates each persisted field", () => {
    for (const raw of [null, "{bad", "null", "[]", '{"version":2,"collapsed":true}']) {
      expect(parseSpacePrefs(raw)).toEqual(defaultSpacePrefs());
    }
    const prefs = parseSpacePrefs(JSON.stringify({ version: 1, collapsed: "yes", groupCollapsed: { a: true, b: "false" },
      exitedOpen: { a: true, b: 1 }, names: { a: " A ", b: 4 }, order: ["a", "a", null, 4, "b"], selectedSpaceId: 5,
      selectedTabs: { a: "one", b: {} }, closedTabs: { a: ["one", "one", false], b: null } }));
    expect(prefs).toEqual({ version: 1, collapsed: false, collapsedSet: false, spacesOpen: false,
      groupCollapsed: { a: true }, exitedOpen: { a: true },
      idleOpen: {}, quietOpen: false,
      names: { a: "A" }, order: ["a", "b"], selectedSpaceId: undefined, selectedTabs: { a: "one" },
      closedTabs: { a: [{ id: "one", resurface: true }], b: [] } });
    // A device that closed tabs before this release keeps them closed, and its
    // plain ids adopt the resurface-on-blocked default.
    const legacy = parseSpacePrefs(JSON.stringify({ version: 1, closedTabs: { a: ["kept"] } }));
    expect(legacy.closedTabs).toEqual({ a: [{ id: "kept", resurface: true }] });
    expect(parseSpacePrefs(JSON.stringify({ version: 1, closedTabs: { a: [{ id: "quiet", resurface: false }] } })).closedTabs)
      .toEqual({ a: [{ id: "quiet", resurface: false }] });
  });

  it("continues working if local storage access is denied or quota is exhausted", () => {
    const storage = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("quota"); } };
    const store = createSpaceStore(storage);
    expect(store.getSnapshot()).toEqual(defaultSpacePrefs());
    store.setCollapsed(true);
    store.selectTab(alpha, "a");
    expect(store.getSnapshot()).toMatchObject({ collapsed: true, selectedTabs: { [alpha]: "a" } });
  });

  it("remembers the sidebar fold as an explicit choice and the 空间 index state per device (D-061 §2.4/§2.5)", () => {
    const store = createSpaceStore(localStorage);
    // No choice yet: the fold is the default, not a stored preference.
    expect(store.getSnapshot()).toMatchObject({ collapsed: false, collapsedSet: false, spacesOpen: false });
    // The index starts collapsed behind 「空间 ▸」; expanding is remembered.
    store.setSpacesOpen(true);
    expect(store.getSnapshot().spacesOpen).toBe(true);
    expect(createSpaceStore(localStorage).getSnapshot().spacesOpen).toBe(true);
    // Flipping the sidebar fold marks the preference explicit in both directions.
    store.setCollapsed(false);
    expect(createSpaceStore(localStorage).getSnapshot()).toMatchObject({ collapsed: false, collapsedSet: true });
    store.setCollapsed(true);
    expect(createSpaceStore(localStorage).getSnapshot()).toMatchObject({ collapsed: true, collapsedSet: true });
    store.setSpacesOpen(false);
    expect(createSpaceStore(localStorage).getSnapshot().spacesOpen).toBe(false);
    // A device folded before this release keeps that fold as an explicit one.
    expect(parseSpacePrefs(JSON.stringify({ version: 1, collapsed: true })))
      .toMatchObject({ collapsed: true, collapsedSet: true });
    // No explicit fold recorded: the device gets the width's initial state.
    expect(parseSpacePrefs(JSON.stringify({ version: 1 })))
      .toMatchObject({ collapsed: false, collapsedSet: false, spacesOpen: false });
  });

  it("remembers which project's idle rows are open and whether the quiet bucket is open", () => {
    const store = createSpaceStore(localStorage);
    expect(store.getSnapshot().quietOpen).toBe(false);
    expect(store.getSnapshot().idleOpen).toEqual({});
    store.toggleIdle(alpha);
    store.toggleQuiet();
    // A second toggle of the same project folds it again.
    store.toggleIdle(beta);
    store.toggleIdle(beta);
    const again = createSpaceStore(localStorage).getSnapshot();
    expect(again.idleOpen).toEqual({ [alpha]: true, [beta]: false });
    expect(again.quietOpen).toBe(true);
    // A stored record from before the fold fields parses as folded.
    expect(parseSpacePrefs(JSON.stringify({ version: 1 }))).toMatchObject({ idleOpen: {}, quietOpen: false });
    expect(parseSpacePrefs(JSON.stringify({ version: 1, idleOpen: { [alpha]: true, bad: "yes" }, quietOpen: true })))
      .toMatchObject({ idleOpen: { [alpha]: true }, quietOpen: true });
  });
});

describe("space and tab selection", () => {
  const workspaces = [workspace(), workspace("workspace-b", "host-a", "/home/dev/projects/beta")];
  const sessions = [instance("a1"), instance("a2"), instance("b1", { workspaceId: "workspace-b" })];

  it("restores each project's selected tab and follows deep links into the correct project", () => {
    const store = createSpaceStore(localStorage);
    store.selectTab(alpha, "a2");
    store.selectTab(beta, "b1");
    store.selectSpace(alpha);
    const prefs = store.getSnapshot();
    const spaces = buildSpaces(workspaces, sessions, prefs);
    expect(selectedSpace(spaces, prefs)?.id).toBe(alpha);
    expect(selectedTab(spaces[0], prefs)?.id).toBe("a2");
    expect(selectedTab(spaces[1], prefs)?.id).toBe("b1");
    expect(selectedSpace(spaces, prefs, "b1")?.id).toBe(beta);
    expect(selectedSpace(spaces, { ...prefs, selectedSpaceId: "removed" })?.id).toBe(alpha);
    expect(selectedTab(spaces[0], { ...prefs, selectedTabs: { [alpha]: "removed" } })?.id).toBe("a1");
  });

  it("hides a closed tab durably, falls back to a remaining tab, and reopens from its deep link", () => {
    const store = createSpaceStore(localStorage);
    const spaces = buildSpaces(workspaces, sessions, store.getSnapshot());
    store.selectTab(alpha, "a2");
    store.closeTab(alpha, "a2");
    expect(visibleTabs(spaces[0], createSpaceStore(localStorage).getSnapshot()).map((item) => item.id)).toEqual(["a1"]);
    expect(selectedTab(spaces[0], store.getSnapshot())?.id).toBe("a1");
    store.closeTab(alpha, "a1");
    expect(selectedTab(spaces[0], store.getSnapshot())).toBeUndefined();
    expect(selectedSpace(spaces, store.getSnapshot(), "a2")?.id).toBe(alpha);
    store.selectTab(alpha, "a2");
    expect(visibleTabs(spaces[0], store.getSnapshot()).map((item) => item.id)).toEqual(["a2"]);
    expect(selectedTab(spaces[0], store.getSnapshot())?.id).toBe("a2");
    expect(visibleTabs(spaces[1], store.getSnapshot()).map((item) => item.id)).toEqual(["b1"]);
  });

  it("keeps a dismissed running session alive, resurfaces it when blocked and re-arms after the episode", () => {
    const store = createSpaceStore(localStorage);
    const running = [instance("a1"), instance("a2", { activity: known("working") })];
    const spaceOf = (rows: Instance[]) => buildSpaces([workspace()], rows, store.getSnapshot())[0];

    // Dismissing a tab hides it without touching the session itself.
    store.closeTab(alpha, "a2", true);
    expect(visibleTabs(spaceOf(running), store.getSnapshot()).map((item) => item.id)).toEqual(["a1"]);
    expect(spaceSessions(spaceOf(running)).live.map((item) => item.id)).toEqual(["a1", "a2"]);

    // It comes back on its own once that session needs a human.
    const blocked = [running[0], instance("a2", { activity: known("waiting-interaction") })];
    expect(visibleTabs(spaceOf(blocked), store.getSnapshot()).map((item) => item.id)).toEqual(["a1", "a2"]);

    // Dismissing it again suppresses only this episode, so it does not bounce back.
    store.closeTab(alpha, "a2", false);
    expect(store.getSnapshot().closedTabs[alpha]).toEqual([{ id: "a2", resurface: false }]);
    expect(visibleTabs(spaceOf(blocked), store.getSnapshot()).map((item) => item.id)).toEqual(["a1"]);
    store.rearmDismissed(["a2"]);
    expect(visibleTabs(spaceOf(blocked), store.getSnapshot()).map((item) => item.id)).toEqual(["a1"]);

    // Once the episode ends the next one may re-open the tab again.
    store.rearmDismissed([]);
    expect(store.getSnapshot().closedTabs[alpha]).toEqual([{ id: "a2", resurface: true }]);
    expect(visibleTabs(spaceOf(running), store.getSnapshot()).map((item) => item.id)).toEqual(["a1"]);
    expect(visibleTabs(spaceOf(blocked), store.getSnapshot()).map((item) => item.id)).toEqual(["a1", "a2"]);

    // Stopping a session dismisses it for good: an exited tab never resurfaces.
    store.closeTab(alpha, "a2", false);
    store.rearmDismissed([]);
    const stopped = [running[0], instance("a2", { lifecycle: "exited" })];
    expect(visibleTabs(spaceOf(stopped), store.getSnapshot()).map((item) => item.id)).toEqual(["a1"]);
    // Clicking it in the sidebar re-opens the tab.
    store.selectTab(alpha, "a2");
    expect(store.getSnapshot().closedTabs[alpha]).toEqual([]);
    expect(visibleTabs(spaceOf(stopped), store.getSnapshot()).map((item) => item.id)).toEqual(["a1", "a2"]);
  });

  it("splits exited sessions into their own group, collapsed until the device opens it", () => {
    const store = createSpaceStore(localStorage);
    const sessions = [instance("live"), instance("gone", { lifecycle: "exited" }),
      instance("failed", { lifecycle: "failed" }), instance("offline", { connectivity: "disconnected" })];
    const spaces = buildSpaces([workspace()], sessions, store.getSnapshot());
    const grouped = spaceSessions(spaces[0]);
    // `unknown` is not a confirmed exit, so a disconnected session stays live.
    expect(grouped.live.map((item) => item.id)).toEqual(["live", "offline"]);
    expect(grouped.exited.map((item) => item.id)).toEqual(["failed", "gone"]);
    // Every session still has a tab; grouping is a sidebar concern only.
    expect(visibleTabs(spaces[0], store.getSnapshot()).map((item) => item.id)).toEqual(["failed", "gone", "live", "offline"]);
    expect(store.getSnapshot().exitedOpen[alpha]).toBeUndefined();
    store.toggleExited(alpha);
    expect(createSpaceStore(localStorage).getSnapshot().exitedOpen[alpha]).toBe(true);
    store.toggleExited(alpha);
    expect(createSpaceStore(localStorage).getSnapshot().exitedOpen[alpha]).toBe(false);
  });

  it("prefills host, workspace and cwd from the selected space without leaking another project's defaults", () => {
    const spaces = buildSpaces(workspaces, sessions, defaultSpacePrefs());
    const params = new URL(newSessionPath(spaces[1]), "http://remuda.test").searchParams;
    expect(Object.fromEntries(params)).toEqual({ host: "host-a", workspace: "workspace-b", cwd: "/home/dev/projects/beta" });
    expect(newSessionPath()).toBe("/sessions/new");
    expect(newSessionPath({ ...spaces[0], id: OTHER_SPACE, hostId: undefined, workspaceId: undefined })).toBe("/sessions/new");
  });
});
