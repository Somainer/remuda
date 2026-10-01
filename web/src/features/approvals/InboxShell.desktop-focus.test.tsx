import { act } from "react";
import { render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { Workspace } from "../../types/workspace";
import { known } from "../../types/wire";
import { InboxShell } from "./InboxShell";

/**
 * c-inboxfu round 3 — the item-1 regression through the REAL desktop caller.
 *
 * Feeding synthetic limit arrays to the hook cannot prove that DesktopInbox
 * actually includes the 进行中 · 最近 limit in its focus-scroll call: deleting
 * `recentLimit` from the call leaves every hook unit test green, while a
 * focused DEPARTED row scrolls in once and is pushed below the fold as the
 * later recent slices (this tier renders ABOVE 已离队) mount over rAF.
 *
 * This test renders the actual InboxShell desktop path with ?focus= on an
 * expired interaction and 30 live recent instances, drains the REAL
 * useIncrementalLimit rAF slices, and asserts scrollIntoView runs on the
 * target again AFTER the final (30-row) recent slice has committed.
 */

const NOW = Date.parse("2026-09-28T12:00:00.000Z");
const HOST_ID = "hst_desk";
const WORKSPACE_ID = "wsp_desk";

vi.mock("../../lib/store", () => ({
  useHub: () => hub,
  hubStore: {
    subscribe: () => () => undefined,
    getSnapshot: () => hub,
    titleOf: (id: string) => `title ${id}`,
    hostName: (id: string) => (id === HOST_ID ? "devbox" : "other"),
    respond: vi.fn(),
    hydrateRowSummaries: vi.fn().mockResolvedValue(undefined),
  },
}));

// A frozen instant: no deadline-crossing re-derives, no clock interference.
vi.mock("../mobile/useInboxPendingCount", () => ({
  useInboxClockNow: () => NOW,
}));

// Deterministic rAF queue (the same stub useIncrementalLimit.survival uses).
const pendingFrames = new Map<number, FrameRequestCallback>();
let nextFrameHandle = 1;
vi.stubGlobal(
  "requestAnimationFrame",
  vi.fn((cb: FrameRequestCallback) => {
    const handle = nextFrameHandle++;
    pendingFrames.set(handle, cb);
    return handle;
  }),
);
vi.stubGlobal(
  "cancelAnimationFrame",
  vi.fn((handle: number) => {
    pendingFrames.delete(handle);
  }),
);

function drainFrames() {
  while (pendingFrames.size > 0) {
    const handle = pendingFrames.keys().next().value as number;
    const cb = pendingFrames.get(handle)!;
    pendingFrames.delete(handle);
    act(() => cb(0));
  }
}

const hub: {
  hosts: Host[];
  workspaces: Workspace[];
  instances: Instance[];
  interactions: Interaction[];
  answering: Record<string, true>;
  summaries: Record<string, string>;
  usageRollup: Record<string, unknown>;
} = {
  hosts: [],
  workspaces: [],
  instances: [],
  interactions: [],
  answering: {},
  summaries: {},
  usageRollup: {},
};

const instanceTemplate = mockDb.instances[0] as Instance;
const expiredTemplate = mockDb.interactions.find((item) => item.state === "expired") as Interaction;

function liveInstance(id: string, updatedAt: string): Instance {
  return {
    ...instanceTemplate,
    id: id as Instance["id"],
    hostId: HOST_ID as Instance["hostId"],
    workspaceId: WORKSPACE_ID as Instance["workspaceId"],
    lifecycle: "ready",
    activity: known("working"),
    connectivity: "connected",
    updatedAt,
  };
}

function buildHub(): void {
  hub.hosts = [
    {
      ...(mockDb.hosts?.[0] ?? {}),
      id: HOST_ID as Host["id"],
      label: "devbox",
      state: "online",
    } as Host,
  ];
  hub.workspaces = [
    {
      id: WORKSPACE_ID as Workspace["id"],
      hostId: HOST_ID as Workspace["hostId"],
      label: "demo-root",
      rootPath: "/srv/demo-root",
      writePolicy: "workspace-write",
      canonicalRoot: known("/srv/demo-root"),
    } as Workspace,
  ];
  // The expired card's own instance is ended: it anchors the 已离队 row but
  // must NOT add a 31st recent row.
  const expiredInstance: Instance = {
    ...instanceTemplate,
    id: "ins_expired" as Instance["id"],
    hostId: HOST_ID as Instance["hostId"],
    workspaceId: WORKSPACE_ID as Instance["workspaceId"],
    lifecycle: "exited",
    activity: known("idle"),
    connectivity: "connected",
  };
  const lives = Array.from({ length: 30 }, (_, i) =>
    liveInstance(`ins_live_${String(i).padStart(2, "0")}`, `2026-09-28T11:${String(i).padStart(2, "0")}:00.000Z`),
  );
  hub.instances = [expiredInstance, ...lives];

  const departed: Interaction = {
    ...expiredTemplate,
    id: "itx_departed" as Interaction["id"],
    instanceId: expiredInstance.id,
    hostId: expiredInstance.hostId,
    state: "expired",
  };
  hub.interactions = [departed];
  hub.answering = {};
  hub.summaries = {};
  hub.usageRollup = {};
}

describe("desktop inbox focus scroll vs later progressive slices", () => {
  let scrollSpy: ReturnType<typeof vi.fn>;
  /** Mounted recent-row count observed at each scrollIntoView call. */
  const depths: number[] = [];

  beforeEach(() => {
    buildHub();
    depths.length = 0;
    pendingFrames.clear();
    nextFrameHandle = 1;
    scrollSpy = vi.fn((..._args: unknown[]) => {
      depths.push(document.querySelectorAll('[data-testid="approvals-recent-row"]').length);
    });
    Element.prototype.scrollIntoView = scrollSpy as unknown as Element["scrollIntoView"];
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("re-scrolls a focused departed row after the final recent slice mounts", () => {
    render(
      <MemoryRouter initialEntries={["/approvals?focus=itx_departed"]}>
        <InboxShell mode="desktop" />
      </MemoryRouter>,
    );

    // The focused departed row is present from the first commit (departed is
    // a single row, within its first slice); 30 recent instances mount over
    // three rAF slices: 12 -> 24 -> 30.
    expect(document.querySelector('[data-interaction-id="itx_departed"]')).toBeTruthy();
    expect(document.querySelectorAll('[data-testid="approvals-recent-row"]').length).toBe(12);

    drainFrames();

    expect(document.querySelectorAll('[data-testid="approvals-recent-row"]').length).toBe(30);
    // The real call includes recentLimit, so the scroll repeats at every
    // grown recent slice — and the LAST call fires only after all 30 rows
    // mounted. Removing recentLimit from DesktopInbox's call leaves a single
    // call at depth 12 and this assertion fails (verified round 3).
    expect(scrollSpy).toHaveBeenCalled();
    expect(depths[depths.length - 1]).toBe(30);
  });
});
