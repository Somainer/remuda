import { createElement } from "react";
import { act, render, renderHook, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { known, type Id } from "../../types/wire";
import type { Workspace } from "../../types/workspace";
import { BOT_CHANNELS } from "../bots/channels";
import {
  GLOBAL_PROJECT,
  ProjectSwitcher,
  boardPath,
  createProjectFilterStore,
  filterTasksByProject,
  isGlobalFilter,
  listProjects,
  memberSpaceKey,
  projectFilterStore,
  projectLabel,
  projectMemberHostIds,
  projectMemberRows,
  resolveProjectMember,
  tasksPath,
  useProjectFilter,
  useProjects,
  type Project,
  type ProjectMember,
} from "./ProjectSwitcher";

vi.mock("../../lib/api", () => ({ rest: vi.fn() }));

import { rest } from "../../lib/api";

const restMock = vi.mocked(rest);

function workspace(id: string, hostId: string, label = id, rootPath = `/tmp/${id}`): Workspace {
  return {
    id: id as Id,
    hostId: hostId as Id,
    label,
    rootPath,
    writePolicy: "workspace-write",
    canonicalRoot: known(rootPath),
    revision: "1",
    createdAt: "2026-09-21T00:00:00Z",
    updatedAt: "2026-09-21T00:00:00Z",
  };
}

function project(id: string, name: string, members: ProjectMember[] = []): Project {
  return {
    id,
    name,
    members,
    revision: "1",
    createdAt: "2026-09-21T00:00:00Z",
    updatedAt: "2026-09-21T00:00:00Z",
  } as Project;
}

type TaskRow = { id: string; projectId: string; title: string };
function task(id: string, projectId: string): TaskRow {
  return { id, projectId, title: id };
}

afterEach(() => {
  restMock.mockReset();
  try {
    projectFilterStore.clear();
    localStorage.removeItem("remuda.project-filter.v1");
  } catch {
    /* jsdom storage absent */
  }
});

describe("filterTasksByProject — switcher filtering and the global option", () => {
  const tasks = [
    task("t1", "prj_a"),
    task("t2", "prj_b"),
    task("t3", "prj_a"),
  ];

  it("global returns every task across projects", () => {
    expect(isGlobalFilter(GLOBAL_PROJECT)).toBe(true);
    expect(filterTasksByProject(tasks, GLOBAL_PROJECT).map((t) => t.id)).toEqual(["t1", "t2", "t3"]);
  });

  it("a project id keeps only that project's tasks", () => {
    expect(filterTasksByProject(tasks, "prj_a").map((t) => t.id)).toEqual(["t1", "t3"]);
    expect(filterTasksByProject(tasks, "prj_b").map((t) => t.id)).toEqual(["t2"]);
  });

  it("an unknown scope and an empty list project to no rows", () => {
    expect(filterTasksByProject(tasks, "prj_other")).toEqual([]);
    expect(filterTasksByProject([], "prj_a")).toEqual([]);
    expect(filterTasksByProject([], GLOBAL_PROJECT)).toEqual([]);
  });

  it("the board and task-list reads carry ?project only when scoped", () => {
    expect(tasksPath(GLOBAL_PROJECT)).toBe("/v1/tasks");
    expect(boardPath(GLOBAL_PROJECT)).toBe("/v1/board");
    expect(tasksPath("prj a/1")).toBe("/v1/tasks?project=prj%20a%2F1");
    expect(boardPath("prj_a")).toBe("/v1/board?project=prj_a");
  });
});

describe("project members map to Spaces through the full (hostId, workspaceId) pair (D-024)", () => {
  it("the same workspace id on two hosts stays two distinct Space keys", () => {
    const onA: ProjectMember = { hostId: "hst_a", workspaceId: "wsp_shared", role: "primary" };
    const onB: ProjectMember = { hostId: "hst_b", workspaceId: "wsp_shared" };
    expect(memberSpaceKey(onA)).not.toBe(memberSpaceKey(onB));
    expect(memberSpaceKey(onA)).toBe(JSON.stringify(["hst_a", "wsp_shared"]));
  });

  it("resolves a member only when both halves of the pair are registered", () => {
    const workspaces = [
      workspace("wsp_a", "hst_a", "alpha", "/tmp/alpha"),
      workspace("wsp_shared", "hst_b", "beta", "/tmp/beta"),
    ];
    // wsp_shared is registered on hst_b, not hst_a: the pair must not resolve.
    const cross = resolveProjectMember({ hostId: "hst_a", workspaceId: "wsp_shared" }, workspaces);
    expect(cross.workspace).toBeUndefined();
    expect(cross.space).toBeUndefined();
    expect(cross.key).toBe(JSON.stringify(["hst_a", "wsp_shared"]));

    // The exact pair resolves and builds a Space carrying the same key.
    const exact = resolveProjectMember({ hostId: "hst_b", workspaceId: "wsp_shared" }, workspaces);
    expect(exact.workspace?.label).toBe("beta");
    expect(exact.space?.id).toBe(exact.key);
    expect(exact.space?.rootPath).toBe("/tmp/beta");
  });

  it("a cross-host project yields one row per member spanning both hosts", () => {
    const p = project("prj_x", "X", [
      { hostId: "hst_a", workspaceId: "wsp_a", role: "primary" },
      { hostId: "hst_b", workspaceId: "wsp_b", role: "build" },
      // Unregistered pair: still a row, never merged into another Space.
      { hostId: "hst_b", workspaceId: "wsp_gone" },
    ]);
    const rows = projectMemberRows(p, [
      workspace("wsp_a", "hst_a", "alpha", "/tmp/alpha"),
      workspace("wsp_b", "hst_b", "beta", "/tmp/beta"),
    ]);
    expect(rows).toHaveLength(3);
    expect(rows.map((r) => r.key)).toEqual([
      JSON.stringify(["hst_a", "wsp_a"]),
      JSON.stringify(["hst_b", "wsp_b"]),
      JSON.stringify(["hst_b", "wsp_gone"]),
    ]);
    expect(rows[0].space?.name).toBe("alpha");
    expect(rows[1].space?.name).toBe("beta");
    expect(rows[2].space).toBeUndefined();
    expect(projectMemberHostIds(p)).toEqual(["hst_a", "hst_b"]);
  });
});

describe("bot channel default project reference", () => {
  it("stays a valid display reference when the project directory does not list it", () => {
    for (const channel of BOT_CHANNELS) {
      expect(channel.defaultProject).toBeTruthy();
      // An empty/other-scope directory must not blank or rewrite it: the raw
      // reference renders verbatim (BotsPage prints defaultProject as text).
      expect(projectLabel(channel.defaultProject, [])).toBe(channel.defaultProject);
    }
    const listed = project("sfe-root", "Sense Front End");
    expect(projectLabel("sfe-root", [listed])).toBe("Sense Front End");
  });
});

describe("project filter store", () => {
  it("defaults to global, selects and clears with persistence", () => {
    const backing = new Map<string, string>();
    const storage = {
      getItem: (key: string) => backing.get(key) ?? null,
      setItem: (key: string, value: string) => void backing.set(key, value),
      removeItem: (key: string) => void backing.delete(key),
    };
    const first = createProjectFilterStore(storage);
    expect(first.getSnapshot()).toBe(GLOBAL_PROJECT);
    act(() => first.select("prj_a"));
    expect(first.getSnapshot()).toBe("prj_a");
    expect(backing.get("remuda.project-filter.v1")).toBe("prj_a");

    // A second store (new tab) reads the persisted selection.
    expect(createProjectFilterStore(storage).getSnapshot()).toBe("prj_a");

    act(() => first.clear());
    expect(first.getSnapshot()).toBe(GLOBAL_PROJECT);
    expect(backing.has("remuda.project-filter.v1")).toBe(false);
  });

  it("notifies subscribers and ignores blank/duplicate selections", () => {
    const store = createProjectFilterStore({ getItem: () => null, setItem: () => {}, removeItem: () => {} });
    const seen: (string | null)[] = [];
    const unsubscribe = store.subscribe(() => seen.push(store.getSnapshot()));
    act(() => {
      store.select("prj_a");
      store.select("prj_a");
      store.select("   ");
    });
    expect(seen).toEqual(["prj_a"]);
    unsubscribe();
  });

  it("useProjectFilter tracks the singleton store", () => {
    const { result } = renderHook(() => useProjectFilter());
    expect(result.current).toBe(GLOBAL_PROJECT);
    act(() => projectFilterStore.select("prj_b"));
    expect(result.current).toBe("prj_b");
  });

  it("reconcile keeps a known selection and clears a stale one back to global", () => {
    const store = createProjectFilterStore({ getItem: () => null, setItem: () => {}, removeItem: () => {} });
    act(() => store.select("prj_a"));
    expect(store.getSnapshot()).toBe("prj_a");

    // Still in the directory: untouched, no notification.
    act(() => store.reconcile(["prj_a", "prj_b"]));
    expect(store.getSnapshot()).toBe("prj_a");

    // Vanished/out-of-scope: clears and persists the clear.
    act(() => store.reconcile(["prj_b"]));
    expect(store.getSnapshot()).toBe(GLOBAL_PROJECT);

    // Global always survives reconcile against any directory, even empty.
    act(() => store.reconcile([]));
    expect(store.getSnapshot()).toBe(GLOBAL_PROJECT);
  });
});

describe("useProjects", () => {
  it("reads the ProjectPage envelope through the generated client", async () => {
    restMock.mockResolvedValue({ items: [project("prj_a", "Alpha")], nextCursor: null });
    const { result } = renderHook(() => useProjects());
    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.projects.map((p) => p.id)).toEqual(["prj_a"]);
    expect(restMock).toHaveBeenCalledWith("/v1/projects");
  });

  it("surfaces a load error without throwing", async () => {
    restMock.mockRejectedValue(new Error("boom"));
    const { result } = renderHook(() => useProjects());
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.error).toMatch(/boom/);
    expect(result.current.projects).toEqual([]);
  });

  it("listProjects unwraps the envelope and tolerates a missing items array", async () => {
    restMock.mockResolvedValue({ nextCursor: null });
    expect(await listProjects()).toEqual([]);
  });

  it("clears a stored selection when the project disappears, so consumers and the control agree", async () => {
    // The operator had prj_a selected (persisted from an earlier session);
    // the reloaded directory only contains prj_b.
    act(() => projectFilterStore.select("prj_a"));
    const consumer = renderHook(() => useProjectFilter());
    expect(consumer.result.current).toBe("prj_a");
    restMock.mockResolvedValue({ items: [project("prj_b", "Beta")], nextCursor: null });

    const directory = renderHook(() => useProjects());
    await waitFor(() => expect(directory.result.current.loading).toBe(false));

    // The directory load reconciles the singleton store: every consumer now
    // reads global, and the stale persisted key is gone.
    expect(projectFilterStore.getSnapshot()).toBe(GLOBAL_PROJECT);
    expect(consumer.result.current).toBe(GLOBAL_PROJECT);
    expect(localStorage.getItem("remuda.project-filter.v1")).toBeNull();

    // The control renders the same global value rather than a hidden id.
    const screen2 = render(
      createElement(
        MemoryRouter,
        { initialEntries: ["/projects"] },
        createElement(ProjectSwitcher, { projects: directory.result.current.projects }),
      ),
    );
    expect((screen2.getByTestId("project-switcher") as HTMLSelectElement).value).toBe("");
    screen2.unmount();
    consumer.unmount();
    directory.unmount();
  });
});

function PathProbe() {
  const location = useLocation();
  return createElement("span", { "data-testid": "path" }, location.pathname);
}

function renderSwitcher(navigateOnSelect: boolean, initialPath = "/projects") {
  const projects = [project("prj_a", "Alpha"), project("prj_b", "Beta")];
  return render(
    createElement(
      MemoryRouter,
      { initialEntries: [initialPath] },
      createElement(PathProbe),
      createElement(ProjectSwitcher, { projects, navigateOnSelect }),
      createElement(
        Routes,
        null,
        createElement(Route, { path: "/projects", element: createElement("div", null, "list") }),
        createElement(Route, { path: "/projects/:id", element: createElement("div", null, "detail") }),
      ),
    ),
  );
}

describe("<ProjectSwitcher>", () => {
  beforeEach(() => {
    act(() => projectFilterStore.clear());
  });

  it("renders 全局 first and every project as options", () => {
    renderSwitcher(false);
    const select = screen.getByTestId("project-switcher") as HTMLSelectElement;
    expect(select.value).toBe("");
    expect(select.options[0]).toHaveTextContent("全局");
    expect([...select.options].map((o) => o.textContent)).toEqual(["全局", "Alpha", "Beta"]);
  });

  it("updates the shared filter when a project is chosen and clears back to global", () => {
    renderSwitcher(false);
    const select = screen.getByTestId("project-switcher");
    act(() => {
      (select as HTMLSelectElement).value = "prj_a";
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(projectFilterStore.getSnapshot()).toBe("prj_a");
    expect(screen.getByTestId("path").textContent).toBe("/projects");

    act(() => {
      (screen.getByTestId("project-switcher") as HTMLSelectElement).value = "";
      screen.getByTestId("project-switcher").dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(projectFilterStore.getSnapshot()).toBe(GLOBAL_PROJECT);
  });

  it("navigates to the scoped project page only when navigateOnSelect", () => {
    renderSwitcher(true);
    act(() => {
      (screen.getByTestId("project-switcher") as HTMLSelectElement).value = "prj_b";
      screen.getByTestId("project-switcher").dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(screen.getByTestId("path").textContent).toBe("/projects/prj_b");
    act(() => {
      (screen.getByTestId("project-switcher") as HTMLSelectElement).value = "";
      screen.getByTestId("project-switcher").dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(screen.getByTestId("path").textContent).toBe("/projects");
  });

  it("does not navigate from non-projects surfaces", () => {
    renderSwitcher(false, "/sessions");
    act(() => {
      (screen.getByTestId("project-switcher") as HTMLSelectElement).value = "prj_a";
      screen.getByTestId("project-switcher").dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(screen.getByTestId("path").textContent).toBe("/sessions");
    expect(projectFilterStore.getSnapshot()).toBe("prj_a");
  });
});
