import { fireEvent, render, screen, waitFor, act } from "@testing-library/react";

// The r3 rename moved the list attribute to `data-order-held`; the tolerant
// selector lets the same tests run against the previous build for the RED proof.
// jsdom has no matchMedia; the Spaces drawer's viewport hook needs one.
beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    }),
  });
});

const HELD = "[data-order-held='1']";
const RELEASED = "[data-order-held='0']";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { uiMode } from "../../lib/status";
import { known, type Id } from "../../types/wire";
import { HomeList } from "./HomeList";
import { LONG_PRESS_MS } from "../home/RowActions";
import { GLANCE_TARGETS } from "../home/glanceNav";
import { interaction as queueInteraction } from "../home/queueTestData";
import { SPACES_PREFS_KEY, spaceKey, spaceStore } from "../spaces/store";
import { setClock } from "../../lib/clock";

const hub = {
  hosts: [{ id: "host-a", label: "alpha" }],
  workspaces: [{ id: "wsp-a", hostId: "host-a", label: "alpha", rootPath: "/srv/alpha" }],
  instances: [] as Instance[],
  interactions: [] as unknown[],
  events: {} as Record<string, unknown>,
  screens: {} as Record<string, unknown>,
  answering: {} as Record<string, true>,
  connection: "live",
  // home-glance-d060: ① reads this, never the instance list.
  glance: null as {
    counts: { needYou: number; doneUnseen: number; working: number; doneToday: number };
    doneUnseenIds?: string[];
  } | null,
};

/** The owner glance the row's ① and ⑤ read (the Hub's numbers, verbatim). */
function setGlance(
  counts: { needYou?: number; doneUnseen?: number; working?: number } = {},
  doneUnseenIds: string[] = [],
) {
  hub.glance = {
    counts: { needYou: 0, doneUnseen: 0, working: 0, doneToday: 0, ...counts },
    doneUnseenIds,
  };
}

const resumeMock = vi.fn();
const listeners = new Set<() => void>();
/** The per-row reads the home used to issue; home-glance-d060 retires the journal one. */
/** The glance's per-row phrase and headline (r6 item 2). */
const glanceRows = vi.hoisted(() => ({
  phrases: {} as Record<string, string>,
  headlines: {} as Record<string, string>,
}));
const rowReads = vi.hoisted(() => ({
  refreshScreens: vi.fn(() => Promise.resolve(undefined)),
  hydrateRowSummaries: vi.fn(() => Promise.resolve(undefined)),
}));
/** The store's freshness projection (home.md §8) as the component sees it. */
const health = { lastOkAt: Date.now() as number | null, failingSince: null as number | null, settledSeq: 0 };
const healthListeners = new Set<() => void>();
const refreshMock = vi.fn(async () => undefined);

function publishHealth(patch: Partial<typeof health>) {
  Object.assign(health, patch);
  for (const listener of healthListeners) listener();
}

vi.mock("../../lib/store", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/store")>();
  return {
    useHub: () => hub,
    resolveMain: actual.resolveMain,
    // The home reads the store through the cached external-store seam: the mock
    // offers the same subscribe/getSnapshot surface (no store polling in unit
    // tests) plus the imperative row helpers the component calls.
    // No address-owner Main in these fixtures: the Main card renders nothing.
    useMainEntry: () => null,
    hubStore: {
      subscribe: (listener: () => void) => {
        listeners.add(listener);
        return () => listeners.delete(listener);
      },
      getSnapshot: () => ({ ...hub, instances: hub.instances.slice() }),
      getRefreshHealth: () => health,
      subscribeRefreshHealth: (listener: () => void) => {
        healthListeners.add(listener);
        return () => healthListeners.delete(listener);
      },
      refresh: () => refreshMock(),
      titleOf: (id: string) => `title ${id}`,
      hostName: () => "alpha",
      usageRollupOf: () => null,
      summaryOf: () => undefined,
      // home-glance-d060: the row body's phrase is the glance's structured
      // phrase (r6: never its title-falling headline), and the ① row owns the
      // shared glance poll (§5.3).
      glanceHeadlineOf: (id: string) => glanceRows.headlines[id],
      glancePhraseOf: (id: string) => glanceRows.phrases[id],
      refreshGlance: () => Promise.resolve(undefined),
      resume: (...args: unknown[]) => resumeMock(...args),
      refreshScreens: rowReads.refreshScreens,
      hydrateRowSummaries: rowReads.hydrateRowSummaries,
    },
  };
});

vi.mock("../files/filesApi", () => ({
  fetchChanges: vi.fn(() => Promise.reject(new Error("offline"))),
}));

const ledgerScopes = vi.hoisted(() => [] as (string | null)[]);
vi.mock("../tasks/taskLedger", () => ({
  useTaskLedger: (scope?: string | null) => {
    ledgerScopes.push(scope ?? null);
    return { tasks: [], projectName: () => null };
  },
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
  refreshMock.mockClear();
  rowReads.refreshScreens.mockClear();
  rowReads.hydrateRowSummaries.mockClear();
  glanceRows.phrases = {};
  glanceRows.headlines = {};
  hub.glance = null;
  health.lastOkAt = Date.now();
  health.failingSince = null;
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
    expect(document.querySelector(HELD)).not.toBeNull();

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
    expect(document.querySelector(RELEASED)).not.toBeNull();
    // Released, blocked outranks working, so the changed worker moves above.
    expect(ids()).toEqual(["/s/ins-other", "/s/ins-held"]);
  });

  it("freezes the order while the row menu is open and re-ranks when it closes", async () => {
    hub.instances = [
      row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-other", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();
    expect(ids()).toEqual(["/s/ins-held", "/s/ins-other"]);

    const menu = screen.getAllByTestId("row-actions-more")[0];
    await userEvent.click(menu);
    expect(screen.getByTestId("row-actions-menu")).toBeTruthy();
    expect(document.querySelector(HELD)).not.toBeNull();

    act(() => {
      hub.instances = [
        row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
        row("ins-other", { activity: known("waiting-interaction"), updatedAt: "2026-09-19T12:00:00.000Z" }),
      ];
      for (const listener of listeners) listener();
    });
    expect(ids()).toEqual(["/s/ins-held", "/s/ins-other"]);

    await userEvent.keyboard("{Escape}");
    expect(document.querySelector(RELEASED)).not.toBeNull();
    expect(ids()).toEqual(["/s/ins-other", "/s/ins-held"]);
  });

  it("releases the hold when a rank filter drops the row whose menu was open", async () => {
    hub.instances = [
      row("ins-work", { activity: known("working") }),
      row("ins-done", { activity: known("idle"), unseen: true }),
    ];
    renderHome();
    const workingArticle = (await screen.findByText("title ins-work")).closest("article")!;
    await userEvent.click(workingArticle.querySelector("[data-testid='row-actions-more']") as HTMLElement);
    expect(screen.getByTestId("row-actions-menu")).toBeTruthy();
    expect(document.querySelector(HELD)).not.toBeNull();

    // A filter tap drops the row even while the hold freezes the rest; its
    // menu id must not keep the order frozen with no menu to close it.
    fireEvent.click(screen.getByTestId("glance-done-unseen"));
    await waitFor(() => expect(document.querySelector(RELEASED)).not.toBeNull());
    expect(screen.queryByTestId("row-actions-menu")).toBeNull();
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
    expect(document.querySelector(HELD)).not.toBeNull();

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
    expect(document.querySelector(RELEASED)).not.toBeNull();
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

describe("HomeList hold against the painted structure", () => {
  function ids() {
    return screen.queryAllByTestId("home-row").map((row) => row.querySelector("a")?.getAttribute("href"));
  }
  function setInstances(instances: Instance[]) {
    act(() => {
      hub.instances = instances;
      for (const listener of listeners) listener();
    });
  }

  it("keeps a held row on screen when it turns idle and its project would fold away", () => {
    const working = row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" });
    const idle = row("ins-idle", { activity: known("idle"), updatedAt: "2026-09-19T08:00:00.000Z" });
    hub.instances = [working, idle];
    renderHome();
    // The idle row is folded; the working one is the only painted row.
    expect(ids()).toEqual(["/s/ins-held"]);
    expect(screen.getByTestId("home-idle-fold")).toHaveTextContent("还有 1 个空闲");

    const held = screen.getByText("title ins-held").closest("article")!;
    act(() => {
      held.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(document.querySelector(HELD)).not.toBeNull();

    // The turn ends: every row in the project is idle now, so the project
    // would fold into 安静的项目 and the held row would disappear.
    setInstances([
      row("ins-held", { activity: known("idle"), updatedAt: "2026-09-19T11:00:00.000Z" }),
      row("ins-idle", { activity: known("idle"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ]);
    expect(ids()).toEqual(["/s/ins-held"]);
    expect(screen.getByText("title ins-held").closest("article")).toHaveAttribute("data-rank", "idle");

    act(() => {
      held.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    });
    // Released: the project is quiet, so both rows move into the bucket.
    expect(ids()).toEqual([]);
    expect(screen.getByTestId("home-quiet-fold")).toHaveTextContent("安静的项目 · 1");
  });

  it("keeps a held row when another project becomes the freshest one", () => {
    hub.workspaces = [
      { id: "wsp-a", hostId: "host-a", label: "alpha", rootPath: "/srv/alpha" },
      { id: "wsp-b", hostId: "host-b", label: "beta", rootPath: "/srv/beta" },
    ] as typeof hub.workspaces;
    hub.hosts = [
      { id: "host-a", label: "alpha" },
      { id: "host-b", label: "beta" },
    ] as typeof hub.hosts;
    const alpha = row("ins-alpha", { activity: known("working"), updatedAt: "2026-09-19T10:00:00.000Z" });
    const beta = row("ins-beta", {
      hostId: "host-b",
      workspaceId: "wsp-b",
      activity: known("working"),
      updatedAt: "2026-09-19T09:00:00.000Z",
    });
    hub.instances = [alpha, beta];
    renderHome();
    const projects = () =>
      screen.getAllByTestId("home-group").map((group) => group.querySelector("span")?.textContent);
    expect(projects()).toEqual(["alpha", "beta"]);

    const held = screen.getByText("title ins-alpha").closest("article")!;
    act(() => {
      held.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    // Beta's worker reports a change: clock order would put beta first.
    setInstances([
      row("ins-alpha", { activity: known("working"), updatedAt: "2026-09-19T10:00:00.000Z" }),
      row("ins-beta", {
        hostId: "host-b",
        workspaceId: "wsp-b",
        activity: known("working"),
        updatedAt: "2026-09-19T12:00:00.000Z",
      }),
    ]);
    expect(projects()).toEqual(["alpha", "beta"]);

    act(() => {
      held.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    });
    expect(projects()).toEqual(["beta", "alpha"]);
  });

  it("lets a fold toggle take effect while a row holds focus", async () => {
    hub.instances = [
      row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-idle", { activity: known("idle"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();
    act(() => {
      screen.getByText("title ins-held").closest("a")!.focus();
    });
    expect(document.querySelector(HELD)).not.toBeNull();
    expect(screen.queryByText("title ins-idle")).toBeNull();

    // Focus moves onto the fold toggle: that is not a row control, so the hold
    // ends and the toggle's own render (not the frozen one) applies.
    await userEvent.click(screen.getByTestId("home-idle-fold"));
    expect(await screen.findByText("title ins-idle")).toBeTruthy();
    expect(document.querySelector(RELEASED)).not.toBeNull();
  });

  it("keeps the list frozen while the spaces drawer is open, through taps inside it", async () => {
    hub.instances = [
      row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-peer", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();
    const before = ids();
    await userEvent.click(screen.getByTestId("spaces-drawer-open"));
    expect(document.querySelector(HELD)).not.toBeNull();

    // Press, release and click inside the drawer: none of it may release.
    const drawer = screen.getByTestId("spaces-drawer");
    fireEvent.pointerDown(drawer);
    fireEvent.pointerUp(drawer);
    fireEvent.click(drawer);
    expect(document.querySelector(HELD)).not.toBeNull();

    setInstances([
      row("ins-held", { activity: known("working"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-peer", { activity: known("waiting-interaction"), updatedAt: "2026-09-19T12:00:00.000Z" }),
    ]);
    expect(ids()).toEqual(before);

    await userEvent.click(screen.getByLabelText("关闭空间面板"));
    expect(document.querySelector(RELEASED)).not.toBeNull();
    expect(ids()).toEqual(["/s/ins-peer", "/s/ins-held"]);
  });


  it("keeps a vanished held row as an inert placeholder until release", () => {
    hub.instances = [
      exited("ins-gone"),
      row("ins-peer", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();
    const gone = screen.getByText("title ins-gone").closest("article")!;
    act(() => {
      fireEvent.pointerDown(gone);
    });
    expect(document.querySelector(HELD)).not.toBeNull();

    // Another device deletes the instance while the finger is down.
    setInstances([row("ins-peer", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" })]);
    expect(gone).toHaveAttribute("data-stale", "1");
    const link = gone.querySelector('[data-testid="home-row-link"]')!;
    expect(link).toHaveAttribute("aria-disabled", "true");
    expect(gone.querySelector('[data-testid="home-resume"]')).toBeDisabled();

    act(() => {
      fireEvent.pointerUp(document.body);
    });
    expect(screen.queryByText("title ins-gone")).toBeNull();
  });

  it("releases when the pointerup lands outside the list", async () => {
    hub.instances = [row("ins-held", { activity: known("working") })];
    renderHome();
    const held = screen.getByText("title ins-held").closest("article")!;
    act(() => {
      held.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(document.querySelector(HELD)).not.toBeNull();

    act(() => {
      document.body.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    });
    expect(document.querySelector(RELEASED)).not.toBeNull();
  });

  it("times a lost press out even while the rest of the page is used", () => {
    vi.useFakeTimers();
    try {
      hub.instances = [row("ins-held", { activity: known("working") })];
      renderHome();
      const held = screen.getByText("title ins-held").closest("article")!;
      act(() => {
        fireEvent.pointerDown(held);
      });
      expect(document.querySelector(HELD)).not.toBeNull();

      // No pointerup ever arrives. Moving the pointer over the page and typing
      // in the search box are NOT list activity and must not postpone release.
      act(() => {
        document.body.dispatchEvent(new PointerEvent("pointermove", { bubbles: true }));
        fireEvent.keyDown(screen.getByTestId("home-search"));
        vi.advanceTimersByTime(9_000);
        document.body.dispatchEvent(new PointerEvent("pointermove", { bubbles: true }));
        fireEvent.keyDown(screen.getByTestId("home-search"));
        vi.advanceTimersByTime(9_000);
      });
      expect(document.querySelector(RELEASED)).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not stay held when the focused row disappears", () => {
    vi.useFakeTimers();
    try {
      hub.instances = [row("ins-held", { activity: known("working") })];
      renderHome();
      act(() => {
        screen.getByText("title ins-held").closest("a")!.focus();
      });
      expect(document.querySelector(HELD)).not.toBeNull();

      // The row is filtered away while it holds focus: no blur ever fires.
      act(() => {
        hub.instances = [row("ins-other", { activity: known("working") })];
        for (const listener of listeners) listener();
      });
      act(() => {
        vi.advanceTimersByTime(10_000);
      });
      expect(document.querySelector(RELEASED)).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("HomeList §8 freshness states", () => {
  // §8 tasks are asserted on the injected clock: the stale window is a
  // product constant, never a wall-clock threshold.
  const NOW = 5_000_000;
  let restoreClock: () => void;
  beforeEach(() => {
    restoreClock = setClock(() => NOW);
    health.lastOkAt = NOW;
  });
  afterEach(() => {
    restoreClock();
  });

  it("paints three skeleton rows before the first snapshot and never an empty state", async () => {
    health.lastOkAt = null;
    hub.instances = [];
    renderHome();
    const skeleton = await screen.findByTestId("home-skeleton");
    expect(skeleton).toHaveAttribute("aria-busy", "true");
    expect(skeleton.children).toHaveLength(3);
    // A cold start must never flash the designed empty state.
    expect(screen.queryByTestId("home-empty")).toBeNull();
    expect(screen.queryByText("还没有会话")).toBeNull();
    expect(screen.queryByText("暂无会话")).toBeNull();
  });

  it("offers 新建会话 when the Space really has no sessions", async () => {
    hub.instances = [];
    renderHome();
    const empty = await screen.findByTestId("home-empty");
    expect(empty).toHaveTextContent("还没有会话");
    expect(screen.getByTestId("home-empty-new")).toHaveTextContent("新建会话");
    expect(screen.queryByTestId("home-skeleton")).toBeNull();
  });

  it("separates a search with no matches: 没有匹配的会话 with 清除搜索", async () => {
    hub.instances = [row("ins-a")];
    renderHome();
    fireEvent.change(await screen.findByTestId("home-search"), {
      target: { value: "nothing-matches-this" },
    });
    const empty = await screen.findByTestId("home-empty");
    expect(empty).toHaveTextContent("没有匹配的会话");
    fireEvent.click(screen.getByTestId("home-empty-clear"));
    expect(screen.getByTestId("home-search")).toHaveValue("");
    await waitFor(() => expect(screen.queryByTestId("home-empty")).toBeNull());
  });

  it("disables a row's ⋯ menu while stale (home.md §8 controls grey)", async () => {
    health.lastOkAt = NOW - 20_000;
    hub.instances = [row("ins-a", { activity: known("working") })];
    renderHome();
    await screen.findByTestId("home-row");
    expect(screen.getAllByTestId("row-actions-more")[0]).toBeDisabled();
  });

  it("greys the cached rows while stale, and a foreground grace lifts the grey until the first refresh settles", async () => {
    health.lastOkAt = NOW - 20_000;
    hub.instances = [row("ins-a", { activity: known("waiting-interaction") })];
    renderHome();
    await screen.findByTestId("home-row");
    const region = () =>
      screen.getByTestId("home-list").querySelector<HTMLElement>("[data-freshness]");
    expect(region()).toHaveAttribute("data-dimmed", "1");
    // Back to the foreground: §8 forbids a stale verdict until the refresh
    // that the shell fires on resume has settled, so the rows stay lit.
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(region()).toHaveAttribute("data-freshness", "grace");
    expect(region()).not.toHaveAttribute("data-dimmed");
    act(() => {
      publishHealth({ failingSince: NOW });
    });
    expect(region()).toHaveAttribute("data-freshness", "stale");
    expect(region()).toHaveAttribute("data-dimmed", "1");
  });
});

describe("① 需要你 on the phone home", () => {
  it("marks the row by rank 1, so a dropped host still carries the anchor", async () => {
    hub.instances = [
      row("ins-dropped", {
        activity: known("waiting-interaction"),
        connectivity: "disconnected",
      }),
    ];
    const { container } = renderHome();
    const article = await screen.findByTestId("home-row");
    // UiStatus projects unknown on a dropped host; the rank stays blocked.
    expect(article).toHaveAttribute("data-status", "unknown");
    expect(article).toHaveAttribute("data-rank", "blocked");
    expect(article).toHaveAttribute("data-blocked", "1");
    expect(container.querySelector('[data-testid="home-row"][data-blocked="1"]')).toBe(article);
  });

  it("scrolls to the ④ queue anchor when 需要你 is tapped (home.md §5.2)", async () => {
    hub.instances = [row("ins-wait", { activity: known("waiting-interaction") })];
    hub.interactions = [queueInteraction({ id: "int_wait", instanceId: "ins-wait", hostId: "host-a" })];
    const scroll = vi.fn();
    const original = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = scroll;
    try {
      renderHome();
      const queue = await screen.findByTestId("home-queue");
      expect(queue).toHaveAttribute("id", GLANCE_TARGETS.queueId);
      fireEvent.click(await screen.findByTestId("glance-need-you"));
      expect(scroll).toHaveBeenCalledWith(expect.objectContaining({ block: "start" }));
      expect(scroll.mock.contexts.at(-1)).toBe(queue);
    } finally {
      hub.interactions = [];
      Element.prototype.scrollIntoView = original;
    }
  });
});

describe("phone home order (§2.1)", () => {
  it("puts ② directly after the sticky head and the search/order row after it", async () => {
    hub.instances = [row("ins-work", { activity: known("working") })];
    renderHome();
    const head = await screen.findByTestId("home-head");
    const strip = await screen.findByTestId("host-strip");
    const toolbar = await screen.findByTestId("home-search");
    // Document order is the contract the CSS orders follow: head → ② → toolbar.
    expect(head.compareDocumentPosition(strip) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(strip.compareDocumentPosition(toolbar) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});

describe("① 工作中 filter and the group header", () => {
  it("recomputes the dots from the filtered rows and clears on the second tap", async () => {
    hub.instances = [
      row("ins-blocked", { activity: known("waiting-interaction") }),
      row("ins-work", { activity: known("working") }),
    ];
    renderHome();
    const dots = await screen.findByTestId("home-group-dots");
    expect(dots).toHaveTextContent("⚠1 ●1");
    const segment = await screen.findByTestId("glance-working");
    await userEvent.click(segment);
    expect(screen.getAllByTestId("home-row").map((el) => el.getAttribute("data-rank"))).toEqual(["working"]);
    expect(screen.getByTestId("home-group-dots")).toHaveTextContent("●1");
    expect(screen.getByTestId("home-group-dots")).not.toHaveTextContent("⚠");
    await userEvent.click(screen.getByTestId("glance-working"));
    expect(screen.getAllByTestId("home-row")).toHaveLength(2);
    expect(screen.getByTestId("home-group-dots")).toHaveTextContent("⚠1 ●1");
  });
});

describe("⑤ 刚完成", () => {
  it("lists the unseen finished rows under the queue", async () => {
    hub.instances = [row("ins-done", { activity: known("idle"), unseen: true })];
    // ⑤ lists the glance's ◉ set, not a re-derivation over the instances.
    setGlance({ doneUnseen: 1 }, ["ins-done"]);
    renderHome();
    const section = await screen.findByTestId("home-just-done");
    expect(section).toHaveTextContent("title ins-done");
    const queue = screen.getByTestId("home-queue");
    expect(queue.compareDocumentPosition(section) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});

describe("① 完成未看 filter", () => {
  it("narrows the ledger to the done-unseen rank and clears it on the second tap", async () => {
    hub.instances = [
      row("ins-done", { activity: known("idle"), unseen: true }),
      row("ins-work", { activity: known("working") }),
    ];
    setGlance({ doneUnseen: 1, working: 1 });
    renderHome();
    const segment = await screen.findByTestId("glance-done-unseen");
    expect(segment).toHaveTextContent("1 完成未看");
    await userEvent.click(segment);
    expect(screen.getAllByTestId("home-row").map((el) => el.getAttribute("data-rank"))).toEqual(["done-unseen"]);
    expect(screen.getByTestId("glance-done-unseen")).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(screen.getByTestId("glance-done-unseen"));
    expect(screen.getAllByTestId("home-row")).toHaveLength(2);
    expect(screen.getByTestId("glance-done-unseen")).toHaveAttribute("aria-pressed", "false");
  });
});

describe("① filter under an active hold", () => {
  it("recomputes the dots from the rendered rows and drops groups the filter empties", async () => {
    hub.instances = [
      row("ins-blocked", { activity: known("waiting-interaction"), updatedAt: "2026-09-19T09:00:00.000Z" }),
      row("ins-work", { activity: known("working"), updatedAt: "2026-09-19T08:00:00.000Z" }),
    ];
    renderHome();
    expect(await screen.findByTestId("home-group-dots")).toHaveTextContent("⚠1 ●1");

    // A press on the row starts a pointer hold; a plain click on the count row
    // does not dispatch pointerup, so the hold is still active when the rank
    // filter changes (the reviewer's repro).
    act(() => {
      screen.getByText("title ins-blocked").closest("article")!
        .dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(document.querySelector(HELD)).not.toBeNull();
    fireEvent.click(screen.getByTestId("glance-working"));
    expect(document.querySelector(HELD)).not.toBeNull();

    // The header counts what is on screen, and the filtered-out row does not
    // linger as a stale placeholder.
    expect(screen.getByTestId("home-group-dots")).toHaveTextContent("●1");
    expect(screen.queryByText("title ins-blocked")).toBeNull();
    expect(screen.getAllByTestId("home-row")).toHaveLength(1);

    act(() => {
      window.dispatchEvent(new PointerEvent("pointerup"));
    });
    expect(document.querySelector(RELEASED)).not.toBeNull();
    expect(screen.getAllByTestId("home-row")).toHaveLength(1);
  });
});

describe("① count vs the phone ledger's set", () => {
  it("includes a working child session, which /m does list as a row", async () => {
    hub.instances = [
      row("ins-parent", { activity: known("working") }),
      row("ins-child", {
        activity: known("working"),
        parent: { instanceId: "ins-parent", runId: "run_1", commandId: "cmd_1" },
      }),
    ];
    // The Hub's owner-scope count includes the child; the ledger lists it too.
    setGlance({ working: 2 });
    renderHome();
    expect(await screen.findByTestId("glance-working")).toHaveTextContent("2 工作中");
    expect(screen.getAllByTestId("home-row")).toHaveLength(2);
  });
});

describe("the home poll", () => {
  it("reads no journal per row — one glance carries every headline", async () => {
    hub.instances = [
      row("ins-a", { activity: known("working") }),
      row("ins-b", { activity: known("working") }),
    ];
    setGlance({ working: 2 });
    renderHome();
    await screen.findAllByTestId("home-row");
    await screen.findByTestId("glance-working");
    // The mount tick is the poll's first iteration. The per-row journal read
    // that used to feed each body is gone — one glance carries every headline
    // (home.md §5.3); the tty screen read is a different source and stays.
    expect(rowReads.hydrateRowSummaries).not.toHaveBeenCalled();
  });
});

describe("the phone row's next step", () => {
  /**
   * [r6 item 2] The same precedence as /sessions: a pending interaction's own
   * summary, a turn error verbatim, then a working row's glance phrase (the
   * workflow phase), else the constant. The title-falling headline never
   * becomes the body.
   */
  it("keeps the structured sources ahead of the glance phrase, and never paints the headline", async () => {
    hub.instances = [
      row("ins-wf", { activity: known("working") }),
      row("ins-appr", { activity: known("waiting-interaction") }),
      row("ins-fail", { lastTurnError: { at: "2026-09-19T10:00:00.000Z", text: "overloaded" } }),
      row("ins-work", { activity: known("working") }),
    ];
    hub.interactions = [
      queueInteraction({ id: "int_appr" as Id, instanceId: "ins-appr" as Id, hostId: "host-a" }),
    ];
    for (const instance of hub.instances) {
      glanceRows.headlines[instance.id] = `title ${instance.id}`;
      glanceRows.phrases[instance.id] = `assistant line ${instance.id}`;
    }
    glanceRows.phrases["ins-wf"] = "Workflow wf-native-demo · phase Review";
    delete glanceRows.phrases["ins-work"];
    renderHome();
    await screen.findAllByTestId("home-row");
    // The turn-failed row is idle: open the idle fold it sits behind.
    const idleFold = screen.queryByTestId("home-idle-fold");
    if (idleFold) await userEvent.click(idleFold);
    const bodyOf = (id: string) =>
      screen
        .getAllByTestId("home-row")
        .find((el) => el.querySelector(`a[href="/s/${id}"]`))
        ?.querySelector("[data-testid='home-row-body']")?.textContent;
    expect(bodyOf("ins-wf")).toBe("Workflow wf-native-demo · phase Review");
    expect(bodyOf("ins-appr")).not.toContain("assistant line");
    expect(bodyOf("ins-appr")).not.toContain("title ins-appr");
    expect(bodyOf("ins-fail")).toBe("overloaded");
    expect(bodyOf("ins-work")).toBe("运行中…");
  });
});

describe("the home poll's screen reads", () => {
  /**
   * [r4 item 4] The ledger renders child rows (nested under their parent), and
   * their next-step sentence reads the tty screen like any other row. Once
   * mapInstance carried `parent`, a top-level-only filter starved them.
   */
  it("hydrates a child row's /screen like its parent's", async () => {
    const tty = mockDb.instances.find((instance) => uiMode(instance) === "tty-attachable")!;
    const ttyRow = (id: string, patch: Partial<Instance> = {}) =>
      row(id, {
        driver: tty.driver,
        kind: tty.kind,
        capabilities: tty.capabilities,
        activity: known("working"),
        ...patch,
      });
    hub.instances = [
      ttyRow("ins-parent"),
      ttyRow("ins-child", {
        parent: { instanceId: "ins-parent", runId: null, commandId: null },
        creatorOrigin: "agent",
      }),
    ];
    expect(hub.instances.every((instance) => uiMode(instance) === "tty-attachable")).toBe(true);
    renderHome();
    await screen.findAllByTestId("home-row");
    await waitFor(() => expect(rowReads.refreshScreens).toHaveBeenCalled());
    const lastCall = rowReads.refreshScreens.mock.calls.at(-1) as unknown[] | undefined;
    expect(lastCall?.[0]).toEqual(["ins-parent", "ins-child"]);
  });
});

describe("① stays owner-wide on a project-filtered /m", () => {
  it("does not scope the count row's task ledger to ?project=", async () => {
    ledgerScopes.length = 0;
    hub.instances = [row("ins-work", { activity: known("working") })];
    render(
      <MemoryRouter initialEntries={["/m?project=prj_1"]}>
        <HomeList />
      </MemoryRouter>,
    );
    await screen.findByTestId("home-list");
    // The ledger keeps its project scope; the ① count must stay global (§5.1:
    // N is the owner-wide queue that matches the badge).
    expect(ledgerScopes).toContain("prj_1");
    expect(ledgerScopes).toContain(null);
  });
});

describe("empty and skeleton states sit in the ledger slot (§2.1)", () => {
  it("paints a no-match empty state after the toolbar", async () => {
    hub.instances = [row("ins-work", { activity: known("working") })];
    renderHome();
    await screen.findByTestId("home-row");
    await userEvent.type(screen.getByTestId("home-search"), "no-such-session-zzz");
    const slot = await waitFor(() => document.querySelector('[data-slot="ledger"]'));
    expect(slot).not.toBeNull();
    expect(slot!.querySelector('[data-testid="home-empty"]')).not.toBeNull();
  });

  it("paints the pre-snapshot skeleton in the ledger slot", async () => {
    publishHealth({ lastOkAt: null });
    try {
      hub.instances = [];
      renderHome();
      const slot = await waitFor(() => document.querySelector('[data-slot="ledger"]'));
      expect(slot!.querySelector('[data-testid="home-skeleton"]')).not.toBeNull();
    } finally {
      publishHealth({ lastOkAt: Date.now() });
    }
  });
});

describe("⑦ row menu long-press", () => {
  it("opens the menu on a body long-press", async () => {
    hub.instances = [row("ins-menu", { activity: known("working") })];
    renderHome();
    const article = (await screen.findByText("title ins-menu")).closest("article")!;
    act(() => {
      article.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, clientX: 10, clientY: 10 }));
    });
    await waitFor(() => expect(screen.getByTestId("row-actions-menu")).toBeTruthy());
    act(() => {
      window.dispatchEvent(new PointerEvent("pointerup"));
    });
  });

  it("does not re-open when the open menu's backdrop is held past the press threshold", async () => {
    hub.instances = [row("ins-menu", { activity: known("working") })];
    renderHome();
    const article = (await screen.findByText("title ins-menu")).closest("article")!;
    await userEvent.click(article.querySelector("[data-testid='row-actions-more']") as HTMLElement);
    const scrim = screen.getByTestId("row-actions-panel").parentElement!;
    // The scrim closes on mousedown; the pointerdown that precedes it lands
    // inside the row article too (the Sheet is not portaled).
    act(() => {
      scrim.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, clientX: 10, clientY: 10 }));
      scrim.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    });
    expect(screen.queryByTestId("row-actions-panel")).toBeNull();
    // A HELD press: no pointerup until after the row's threshold.
    await act(() => new Promise((resolve) => setTimeout(resolve, LONG_PRESS_MS + 120)));
    expect(screen.queryByTestId("row-actions-panel")).toBeNull();
    act(() => {
      window.dispatchEvent(new PointerEvent("pointerup"));
    });
  });
});
