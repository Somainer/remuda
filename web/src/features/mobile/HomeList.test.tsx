import { render, screen, waitFor, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { HomeList } from "./HomeList";
import { SPACES_PREFS_KEY, spaceKey, spaceStore } from "../spaces/store";

const hub = {
  hosts: [{ id: "host-a", label: "alpha" }],
  workspaces: [{ id: "wsp-a", hostId: "host-a", label: "alpha", rootPath: "/srv/alpha" }],
  instances: [] as Instance[],
  interactions: [] as unknown[],
  events: {} as Record<string, unknown>,
  screens: {} as Record<string, unknown>,
  connection: "live",
};

const resumeMock = vi.fn();
const listeners = new Set<() => void>();

vi.mock("../../lib/store", () => ({
  // The home reads the store through the cached external-store seam: the mock
  // offers the same subscribe/getSnapshot surface (no store polling in unit
  // tests) plus the imperative row helpers the component calls.
  hubStore: {
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getSnapshot: () => ({ ...hub, instances: hub.instances.slice() }),
    titleOf: (id: string) => `title ${id}`,
    hostName: () => "alpha",
    usageRollupOf: () => null,
    summaryOf: () => undefined,
    resume: (...args: unknown[]) => resumeMock(...args),
    refreshScreens: vi.fn().mockResolvedValue(undefined),
    hydrateRowSummaries: vi.fn().mockResolvedValue(undefined),
  },
}));

vi.mock("../files/filesApi", () => ({
  fetchChanges: vi.fn(() => Promise.reject(new Error("offline"))),
}));

vi.mock("../tasks/TaskList", () => ({
  // The task layer is a store/network concern, out of scope for these
  // component tests; the session rows under test never depend on it.
  TaskGroups: () => null,
  useTaskLedger: () => ({ tasks: [], projectName: () => null }),
}));

function exited(id: string): Instance {
  return row(id, { lifecycle: "exited" });
}

function row(id: string, patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id,
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    parent: null,
    updatedAt: "2026-09-19T10:00:00.000Z",
    ...patch,
  } as Instance;
}

function renderHome() {
  return render(
    <MemoryRouter>
      <HomeList />
    </MemoryRouter>,
  );
}

/** An exited-only project folds into 「安静的项目」 until the bucket is opened. */
async function openQuiet() {
  await userEvent.click(await screen.findByTestId("home-quiet-fold"));
}

beforeEach(() => {
  resumeMock.mockReset();
  localStorage.removeItem(SPACES_PREFS_KEY);
  spaceStore.reload();
});

describe("HomeList resume", () => {
  it("exited rows offer 恢复, which calls resume(id, structured) and stays put showing an error when it fails", async () => {
    hub.instances = [exited("ins-exited-fail")];
    resumeMock.mockResolvedValue(null);
    renderHome();
    await openQuiet();

    const button = await screen.findByTestId("home-resume");
    expect(button).toHaveTextContent("恢复");
    button.click();

    await waitFor(() => expect(resumeMock).toHaveBeenCalledWith("ins-exited-fail", "structured"));
    await waitFor(() =>
      expect(screen.getByTestId("home-resume-error")).toHaveTextContent("恢复失败"),
    );
    // The row is still there with the button enabled again — no navigation.
    expect(screen.getByTestId("home-resume")).toHaveTextContent("恢复");
  });

  it("on a successful resume the busy state clears for the new navigation", async () => {
    hub.instances = [exited("ins-exited-ok")];
    let resolveResume: (id: Id | null) => void = () => {};
    resumeMock.mockReturnValue(
      new Promise<Id | null>((resolve) => {
        resolveResume = resolve;
      }),
    );
    renderHome();
    await openQuiet();
    const button = await screen.findByTestId("home-resume");
    button.click();
    await waitFor(() => expect(button).toBeDisabled());
    expect(button).toHaveTextContent("恢复中…");
    resolveResume("ins-child-new");
    await waitFor(() => expect(screen.queryByTestId("home-resume-error")).toBeNull());
  });
});

describe("HomeList hold", () => {
  function ids() {
    return screen.getAllByTestId("home-row").map((row) => row.querySelector("a")?.getAttribute("href"));
  }

  it("keeps a held row where it is while another worker changes state, then re-ranks on release", () => {
    hub.instances = [
      row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-other", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();

    // Both are working, so neither folds. The newer one leads, and it is the
    // row a later change could displace.
    expect(ids()).toEqual(["/s/ins-held", "/s/ins-other"]);

    const held = screen.getByText("title ins-held").closest("article")!;
    act(() => {
      held.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(document.querySelector("[data-held='1']")).not.toBeNull();

    // The other worker becomes blocked, which outranks the held row. The finger
    // is still down, so the held row keeps its place.
    act(() => {
      hub.instances = [
        row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
        row("ins-other", { activity: known("waiting-interaction"), updatedAt: "2026-09-19T12:00:00.000Z" }),
      ];
      for (const listener of listeners) listener();
    });
    expect(ids()).toEqual(["/s/ins-held", "/s/ins-other"]);
    // The row's own text does update while it is held.
    expect(screen.getByText("title ins-other").closest("article")).toHaveAttribute("data-rank", "blocked");

    act(() => {
      held.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    });
    expect(document.querySelector("[data-held='0']")).not.toBeNull();
    // Released, blocked outranks working, so the changed worker moves above.
    expect(ids()).toEqual(["/s/ins-other", "/s/ins-held"]);
  });

  it("also freezes while focus sits in a row's control, and re-ranks when it leaves", () => {
    hub.instances = [
      row("ins-focused", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-peer", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();
    expect(ids()).toEqual(["/s/ins-focused", "/s/ins-peer"]);

    const link = screen.getByText("title ins-focused").closest("a")!;
    act(() => {
      link.focus();
    });
    expect(document.querySelector("[data-held='1']")).not.toBeNull();

    act(() => {
      hub.instances = [
        row("ins-focused", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
        row("ins-peer", { activity: known("waiting-interaction"), updatedAt: "2026-09-19T12:00:00.000Z" }),
      ];
      for (const listener of listeners) listener();
    });
    // The peer outranks the focused row, but the frozen order holds.
    expect(ids()).toEqual(["/s/ins-focused", "/s/ins-peer"]);

    act(() => {
      link.blur();
    });
    expect(document.querySelector("[data-held='0']")).not.toBeNull();
    expect(ids()).toEqual(["/s/ins-peer", "/s/ins-focused"]);
  });
});

describe("HomeList folds during a search", () => {
  it("hides both fold toggles while the search box has text", async () => {
    localStorage.setItem(
      SPACES_PREFS_KEY,
      JSON.stringify({
        version: 1,
        idleOpen: { [spaceKey("host-a", "wsp-a")]: true },
        quietOpen: false,
      }),
    );
    spaceStore.reload();
    hub.instances = [
      // A busy project with its idle rows open: the collapse control is shown.
      row("ins-blocked", { activity: known("waiting-interaction") }),
      row("ins-idle", { activity: known("idle") }),
      // A second, quiet project carries the bucket toggle.
      row("ins-beta-idle", {
        hostId: "host-b",
        workspaceId: "wsp-b",
        activity: known("idle"),
      }),
    ];
    hub.workspaces = [
      { id: "wsp-a", hostId: "host-a", label: "alpha", rootPath: "/srv/alpha" },
      { id: "wsp-b", hostId: "host-b", label: "beta", rootPath: "/srv/beta" },
    ] as typeof hub.workspaces;
    hub.hosts = [
      { id: "host-a", label: "alpha" },
      { id: "host-b", label: "beta" },
    ] as typeof hub.hosts;
    renderHome();
    expect(await screen.findByTestId("home-idle-fold")).toBeTruthy();
    expect(await screen.findByTestId("home-quiet-fold")).toBeTruthy();

    await userEvent.type(screen.getByTestId("home-search"), "ins-");
    // Search forces the folds open, so the controls that would close them are
    // gone: a click there could not take effect and would only flip the saved
    // preference for after the search.
    await waitFor(() => expect(screen.queryByTestId("home-idle-fold")).toBeNull());
    expect(screen.queryByTestId("home-quiet-fold")).toBeNull();
  });
});
