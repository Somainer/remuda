import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { HomeList } from "./HomeList";

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
    getSnapshot: () => hub,
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
  return {
    ...mockDb.instances[0],
    id,
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "exited",
    connectivity: "connected",
    activity: known("idle"),
    parent: null,
  } as Instance;
}

function renderHome() {
  return render(
    <MemoryRouter>
      <HomeList />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  resumeMock.mockReset();
});

describe("HomeList resume", () => {
  it("exited rows offer 恢复, which calls resume(id, structured) and stays put showing an error when it fails", async () => {
    hub.instances = [exited("ins-exited-fail")];
    resumeMock.mockResolvedValue(null);
    renderHome();

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
    const button = await screen.findByTestId("home-resume");
    button.click();
    await waitFor(() => expect(button).toBeDisabled());
    expect(button).toHaveTextContent("恢复中…");
    resolveResume("ins-child-new");
    await waitFor(() => expect(screen.queryByTestId("home-resume-error")).toBeNull());
  });
});
