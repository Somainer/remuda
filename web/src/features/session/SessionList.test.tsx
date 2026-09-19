import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { detectPlatform, setPlatformForTest } from "../../lib/platform";
import { SessionList } from "./SessionList";

// jsdom has no matchMedia; the workbench viewport hook needs one to pick the
// popover (desktop) or the sheet (phone). Default to desktop; `viewport` flips it.
let mobileViewport = false;
beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: mobileViewport,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }),
  });
});

const hub = {
  hosts: [] as unknown[],
  workspaces: [] as unknown[],
  instances: [] as Instance[],
  interactions: [] as unknown[],
  screens: {} as Record<string, unknown>,
  connection: "live",
};

vi.mock("../../lib/store", () => ({
  useHub: () => hub,
  hubStore: {
    titleOf: (id: string) => titles[id] ?? id,
    summaryOf: (id: string) => summaries[id] ?? "",
    hostName: (id: string) => hostNames[id] ?? id,
    effortOf: () => ({ name: "medium", index: 2, ultracode: false }),
    effortEffectiveOf: () => null,
    modelOf: () => "model_hub/es1_orange_o50[1m]",
    modelEffectiveOf: (id: string) => modelEffective[id] ?? null,
    modelCatalogOf: () => null,
    refreshScreens: vi.fn(),
    hydrateRowSummaries: vi.fn().mockResolvedValue(undefined),
    broadcast: vi.fn(),
    send: vi.fn(),
    sendKeys: vi.fn(),
    close: vi.fn(),
  },
}));

const titles: Record<string, string> = {};
const hostNames: Record<string, string> = { "host-a": "alpha", "host-b": "beta" };
/// Per-instance live phrases projected from journal tails; empty/absent means
/// the working row falls back to its constant sentence.
const summaries: Record<string, string> = {};
/// Per-instance effective-model observations the mocked store hands back
/// (model-pin-1). Empty by default: most rows have not read one back.
const modelEffective: Record<
  string,
  { id: string; source: string; observedAt: string }
> = {};

function host(id: string, label: string) {
  return { id, label, state: "online" };
}

function workspace(id: string, hostId: string, label: string, rootPath: string) {
  return { id, hostId, label, rootPath, canonicalRoot: known(rootPath), writePolicy: "workspace-write" };
}

function session(id: string, patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id,
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    parent: null,
    ...patch,
  } as Instance;
}

/** The Space the list is pinned to in most of these cases. */
const spaceA = { id: "space-a", name: "sfe-root", hostId: "host-a", workspaceId: "wsp-a" };

function renderList(search = "", options: { instances?: Instance[]; space?: typeof spaceA } = {}) {
  return render(
    <MemoryRouter initialEntries={[`/sessions${search}`]}>
      <SessionList
        variant="full"
        instances={options.instances ?? hub.instances}
        title="sfe-root"
        space={"space" in options ? options.space : spaceA}
      />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  mobileViewport = false;
  hub.hosts = [host("host-a", "alpha"), host("host-b", "beta")];
  hub.workspaces = [workspace("wsp-a", "host-a", "sfe-root", "/home/dev/sfe-root")];
  hub.instances = [session("ins_a"), session("ins_b", { activity: known("waiting-interaction") })];
  hub.interactions = [];
  hub.screens = {};
  titles.ins_a = "spill 抖动";
  titles.ins_b = "等待批准";
  for (const key of Object.keys(summaries)) delete summaries[key];
});

describe("SessionList empty states", () => {
  it("reports no hosts, pointing at the place that adds one", () => {
    hub.hosts = [];
    renderList();
    const list = screen.getByTestId("session-list");
    expect(list).toHaveAttribute("data-empty", "no-hosts");
    expect(list).toHaveTextContent("无主机");
    expect(screen.getByRole("link", { name: "添加主机" })).toHaveAttribute("href", "/hosts");
  });

  it("separates a connected host with no registered directory from having no host at all", () => {
    hub.workspaces = [];
    renderList();
    const list = screen.getByTestId("session-list");
    expect(list).toHaveAttribute("data-empty", "no-workspaces");
    expect(list).toHaveTextContent("还没有注册工作目录");
    expect(screen.getByRole("link", { name: "注册工作目录" })).toHaveAttribute("href", "/hosts");
  });

  it("reports an empty Space as having no sessions, and offers to create one", () => {
    renderList("", { instances: [] });
    const list = screen.getByTestId("session-list");
    expect(list).toHaveAttribute("data-empty", "no-sessions");
    expect(list).toHaveTextContent("还没有会话");
    expect(screen.getByRole("link", { name: "新建会话" })).toBeInTheDocument();
  });

  it("distinguishes filtered-to-zero from an empty Space, and says the sessions are still there", () => {
    renderList("?q=nothing-matches-this");
    const zero = screen.getByTestId("session-no-matches");
    expect(zero).toHaveAttribute("data-empty", "no-matches");
    expect(zero).toHaveTextContent("没有会话符合当前筛选条件");
    // The count of what exists behind the filter is what tells the user this is
    // a filter, not data loss.
    expect(zero).toHaveTextContent("2 个会话");
    expect(screen.queryByTestId("session-list")).toHaveAttribute("data-testid", "session-list");
  });

  it("offers both recovery paths from zero results and no destructive action", () => {
    renderList("?q=nothing-matches-this");
    const zero = screen.getByTestId("session-no-matches");
    expect(screen.getByTestId("session-no-matches-clear")).toHaveTextContent("清除筛选");
    expect(screen.getByTestId("session-no-matches-all")).toHaveTextContent("搜索所有空间");
    // Clearing a filter changes the view, never the sessions themselves.
    expect(zero).toHaveTextContent("不会创建或关闭任何会话");
    expect(zero.textContent).not.toMatch(/删除|停止|关闭会话/);
  });

  it("restores the list when the conditions are cleared, without touching any session", async () => {
    const user = userEvent.setup();
    const store = await import("../../lib/store");
    renderList("?q=nothing-matches-this");
    await user.click(screen.getByTestId("session-no-matches-clear"));
    expect(screen.queryByTestId("session-no-matches")).toBeNull();
    expect(screen.getAllByTestId("board-card")).toHaveLength(2);
    expect(store.hubStore.close).not.toHaveBeenCalled();
    expect(store.hubStore.send).not.toHaveBeenCalled();
  });
});

describe("SessionList scope and conditions", () => {
  it("shows the fixed Space scope with its host, and the match count", () => {
    renderList();
    const scope = screen.getByTestId("session-scope");
    expect(scope).toHaveAttribute("data-scope", "space");
    expect(scope).toHaveTextContent("当前 Space 固定范围");
    expect(scope).toHaveTextContent("alpha");
    expect(screen.getByTestId("session-match-count")).toHaveTextContent("2 / 2");
  });

  it("reports global scope when the URL says so", () => {
    renderList("?scope=all");
    expect(screen.getByTestId("session-scope")).toHaveAttribute("data-scope", "all");
    expect(screen.getByTestId("session-scope")).toHaveTextContent("所有空间");
  });

  it("withholds host and directory conditions inside a fixed Space, and explains why", async () => {
    const user = userEvent.setup();
    renderList();
    await user.click(screen.getByTestId("session-filter-open"));
    expect(screen.queryByTestId("session-filter-hosts")).toBeNull();
    expect(screen.queryByTestId("session-filter-workspaces")).toBeNull();
    expect(screen.getByTestId("session-filter-scope-note")).toHaveTextContent("已固定主机与工作目录");
  });

  it("offers host and directory conditions once the search is global", async () => {
    const user = userEvent.setup();
    renderList("?scope=all");
    await user.click(screen.getByTestId("session-filter-open"));
    expect(screen.getByTestId("session-filter-hosts")).toBeInTheDocument();
    expect(screen.getByTestId("session-filter-workspaces")).toBeInTheDocument();
  });

  it("opens and closes the filter panel with aria-expanded tracking it", async () => {
    const user = userEvent.setup();
    renderList();
    const trigger = screen.getByTestId("session-filter-open");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger.getAttribute("aria-controls")).toBeNull();
    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    const filterPanel = screen.getByTestId("session-filter-panel");
    expect(filterPanel).toHaveAttribute("aria-modal", "true");
    // aria-controls resolves to the panel's real id.
    expect(trigger.getAttribute("aria-controls")).toBe("session-filter-panel");
    expect(filterPanel.getAttribute("id")).toBe("session-filter-panel");
    await user.keyboard("{Escape}");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger).toHaveFocus();
  });

  it("renders active conditions as removable chips", async () => {
    const user = userEvent.setup();
    renderList("?q=spill&status=idle");
    const chips = screen.getAllByTestId("session-chip");
    expect(chips.map((chip) => chip.getAttribute("data-chip-key"))).toEqual(["q", "status"]);
    await user.click(screen.getByRole("button", { name: /移除条件 搜索：spill/ }));
    expect(screen.queryByRole("button", { name: /移除条件 搜索：spill/ })).toBeNull();
  });

  it("filters the list by the search text in the URL", () => {
    renderList("?q=spill");
    expect(screen.getAllByTestId("board-card")).toHaveLength(1);
    expect(screen.getByTestId("session-match-count")).toHaveTextContent("1 / 2");
  });

  it("uses the popover on desktop and the sheet on a phone", async () => {
    const user = userEvent.setup();
    const desktop = renderList();
    await user.click(screen.getByTestId("session-filter-open"));
    expect(screen.getByTestId("session-filter-panel")).toHaveAttribute("data-variant", "popover");
    desktop.unmount();

    mobileViewport = true;
    renderList();
    await user.click(screen.getByTestId("session-filter-open"));
    expect(screen.getByTestId("session-filter-panel")).toHaveAttribute("data-variant", "sheet");
  });
});

/** Space id the real buildSpaces() derives for the fixture host/workspace. */
const derivedSpace = { id: '["host-a","wsp-a"]', name: "sfe-root", hostId: "host-a", workspaceId: "wsp-a" };

describe("SessionList effective-model label (model-pin-1)", () => {
  afterEach(() => {
    for (const key of Object.keys(modelEffective)) delete modelEffective[key];
  });

  it("labels the row with the requested id until one is read back", () => {
    renderList();
    const label = screen.getAllByTestId("session-model")[0];
    expect(label).toHaveTextContent("model_hub/es1_orange_o50[1m]");
    expect(label).toHaveAttribute("data-model-effective", "unknown");
    expect(label).toHaveAttribute("data-model-diverged", "0");
  });

  it("labels the row with the observed id once the session reports one", () => {
    for (const id of ["ins_a", "ins_b"]) {
      modelEffective[id] = {
        id: "model_hub/es1_orange_o50[1m]",
        source: "launch",
        observedAt: "2026-09-18T00:00:00Z",
      };
    }
    renderList();
    const label = screen.getAllByTestId("session-model")[0];
    expect(label).toHaveTextContent("model_hub/es1_orange_o50[1m]");
    expect(label).toHaveAttribute("data-model-diverged", "0");
  });

  // The regression: the pin was requested and a DIFFERENT model in the same
  // vocabulary answered. The row must show what answered and mark the
  // divergence, because showing only the request is what made the substituted
  // model invisible.
  it("marks a divergence when a different model in the pin's namespace answered", () => {
    for (const id of ["ins_a", "ins_b"]) {
      modelEffective[id] = {
        id: "model_hub/es1_orange_o48[1m]",
        source: "launch",
        observedAt: "2026-09-18T00:00:00Z",
      };
    }
    renderList();
    const label = screen.getAllByTestId("session-model")[0];
    expect(label).toHaveTextContent("model_hub/es1_orange_o48[1m]");
    expect(label).toHaveAttribute("data-model-effective", "model_hub/es1_orange_o48[1m]");
    expect(label).toHaveAttribute("data-model-diverged", "1");
    // Both ids stay legible, so the pin that was asked for is not lost.
    expect(label.getAttribute("title")).toContain("model_hub/es1_orange_o50[1m]");
    expect(label.getAttribute("title")).toContain("model_hub/es1_orange_o48[1m]");
  });

  // The measured false positive (model-pin-1 §3): a gateway resolves a catalog
  // id to an upstream vendor name. This is a correct launch and must NOT be
  // flagged as diverged, even though the two strings differ.
  it("does not flag a gateway resolving the pin to an upstream vendor name", () => {
    for (const id of ["ins_a", "ins_b"]) {
      modelEffective[id] = {
        id: "claude-opus-5",
        source: "launch",
        observedAt: "2026-09-18T00:00:00Z",
      };
    }
    renderList();
    const label = screen.getAllByTestId("session-model")[0];
    expect(label).toHaveTextContent("claude-opus-5");
    expect(label).toHaveAttribute("data-model-effective", "claude-opus-5");
    expect(label).toHaveAttribute("data-model-diverged", "0");
  });
});

function renderKeyList() {
  return render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionList variant="full" instances={hub.instances} title="sfe-root" space={derivedSpace} />
    </MemoryRouter>,
  );
}

function approvalInteraction(id: string, instanceId: string, description: string) {
  return {
    id,
    revision: "1",
    createdAt: "2026-09-19T00:00:00.000Z",
    updatedAt: "2026-09-19T00:00:00.000Z",
    instanceId,
    runId: null,
    hostId: "host-a",
    kind: "approval",
    state: "pending",
    blocking: true,
    answerable: true,
    carrier: "claude-control",
    request: {
      kind: "approval",
      title: "Bash",
      description,
      toolCallId: id,
      actionRef: id,
      options: [],
      requestedPermissionsRef: null,
      inputDigest: "sha256:00",
    },
    requestKey: {
      native: { type: "rpc", valueType: "string", value: "ask" },
      processGeneration: "1",
      runGeneration: "1",
      connectionEpoch: "host-a",
    },
    requestVersion: "1",
    deadline: { state: "unknown", reason: "none", evidenceEventIds: [] },
    deadlineSource: "none",
    answer: { state: "not-applicable" },
    delivery: "not-sent",
    resolution: { state: "not-applicable" },
  };
}

describe("SessionList rows: next step, wire disclosure and overflow sheet", () => {
  it("renders the headline first and hides the wire triple inside a closed disclosure", () => {
    renderList();
    const row = screen.getAllByTestId("session-row")[0];
    // The dot/title headline is the first child line; the next-step line follows.
    const headline = row.firstElementChild!;
    expect(headline.querySelector("[data-status]")).not.toBeNull();
    expect(headline.textContent).toContain("等待批准");
    expect(row.children[1].getAttribute("data-testid")).toBe("session-next-step");

    const wire = screen.getAllByTestId("session-wire")[0] as HTMLDetailsElement;
    expect(wire.open).toBe(false);
    // lifecycle/activity/connectivity are present in the DOM but not in the
    // default viewport: the closed details hides them.
    expect(screen.getAllByTestId("session-lifecycle")[0]).not.toBeVisible();
    expect(row.textContent).not.toContain("waiting-interaction");

    // The row-side wire toggle's tooltip carries the wire triple and the
    // relative timestamp (the time is hidden from the mobile row itself).
    const card = screen.getAllByTestId("board-card")[0];
    const wireToggle = card.querySelector("[data-testid='session-wire-toggle']") as HTMLElement;
    expect(wireToggle).toBeTruthy();
    const summaryTip = wireToggle.getAttribute("title") ?? "";
    expect(summaryTip).toContain("ready");
    expect(summaryTip).toContain("connected");
    expect(summaryTip).toContain("alpha/sfe-root");
    // Relative timestamp (mock timestamps are "now"-ish, so formatListTime
    // yields either "刚刚" or a clock string).
    expect(summaryTip).toMatch(/刚刚|^\d{1,2}:\d{2}$/m);
    // The toggle's aria-expanded/aria-controls describe the closed details.
    expect(wireToggle.getAttribute("aria-expanded")).toBe("false");
    expect(wireToggle.getAttribute("aria-controls")).toBe(wire.id);
  });
  it("renders no 'undefined' hole when the instance's workspace is absent from the snapshot", () => {
    hub.instances = [session("ins_a", { workspaceId: "wsp-missing" as Id })];    renderList();
    const card = screen.getAllByTestId("board-card")[0];
    expect(card.textContent).not.toContain("undefined");
    const tip = card.querySelector("[data-testid='session-wire-toggle']")?.getAttribute("title") ?? "";
    expect(tip).not.toContain("undefined");
    expect(tip).not.toContain("/ ");
  });

  it("projects the pending approval as the next step and keeps one go handle", () => {
    hub.interactions = [approvalInteraction("int_b", "ins_b", "rm -rf /tmp/coord-media")];
    renderList();
    const blockedCard = screen
      .getAllByTestId("board-card")
      .find((card) => card.getAttribute("data-status") === "blocked")!;
    const step = blockedCard.querySelector("[data-testid='session-next-step']");
    expect(step?.textContent).toContain("rm -rf /tmp/coord-media");
    expect(step?.textContent).not.toContain("waiting-interaction");
    const go = blockedCard.querySelector("[data-testid='board-go-handle']");
    expect(go?.getAttribute("href")).toBe("/approvals?focus=int_b");
    expect(go?.textContent).toContain("去处理");
  });

  it("projects a working row's journal phrase and falls back to the constant", () => {
    summaries.ins_a = "Workflow wf_9f3 · phase compile";
    hub.instances = [session("ins_a", { activity: known("working") }), session("ins_b", { activity: known("waiting-interaction") })];
    renderList();
    const workingCard = screen
      .getAllByTestId("board-card")
      .find((card) => card.querySelector(`a[href="/s/ins_a"]`))!;
    expect(workingCard.querySelector("[data-testid='session-next-step']")?.textContent).toBe(
      "Workflow wf_9f3 · phase compile",
    );

    delete summaries.ins_a;
  });

  it("uses the constant working sentence when no phrase is known", () => {
    hub.instances = [session("ins_a", { activity: known("working") }), session("ins_b", { activity: known("waiting-interaction") })];
    renderList();
    const workingCard = screen
      .getAllByTestId("board-card")
      .find((card) => card.querySelector(`a[href="/s/ins_a"]`))!;
    expect(workingCard.querySelector("[data-testid='session-next-step']")?.textContent).toBe("运行中…");
  });

  it("does not render the go handle when the blocked row has no pending interaction", () => {
    renderList();
    const blockedCard = screen
      .getAllByTestId("board-card")
      .find((card) => card.getAttribute("data-status") === "blocked")!;
    expect(blockedCard.querySelector("[data-testid='session-next-step']")?.textContent).toContain(
      "等待处理交互",
    );
    expect(blockedCard.querySelector("[data-testid='board-go-handle']")).toBeNull();
  });

  it("keeps send/keys/stop testids inside the overflow sheet, which opens and closes", async () => {
    const user = userEvent.setup();
    renderList();
    const card = screen.getAllByTestId("board-card")[0];
    // The inline remote controls are gone; only the ⋯ trigger sits on the row.
    expect(card.querySelector("[data-testid='board-prompt']")).toBeNull();
    expect(card.querySelector("[data-testid='board-key-esc']")).toBeNull();

    const trigger = card.querySelector("[data-testid='board-more']") as HTMLButtonElement;
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(trigger.getAttribute("aria-controls")).toBeNull();
    await user.click(trigger);
    const panel = screen.getByTestId("board-actions-panel");
    expect(panel).toBeVisible();
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    // aria-controls resolves to the panel's real id.
    expect(trigger.getAttribute("aria-controls")).toBe("board-actions-panel");
    expect(panel.getAttribute("id")).toBe("board-actions-panel");
    expect(panel).toHaveAttribute("data-variant", "popover");
    expect(panel.querySelector("[data-testid='board-prompt']")).not.toBeNull();
    expect(panel.querySelector("[data-testid='board-send']")).not.toBeNull();
    expect(panel.querySelector("[data-testid='board-key-enter']")).not.toBeNull();
    expect(panel.querySelector("[data-testid='board-key-esc']")).not.toBeNull();
    expect(panel.querySelector("[data-testid='board-key-ctrl-c']")).not.toBeNull();
    expect(panel.querySelector("[data-testid='board-stop']")).not.toBeNull();

    // Keys stay in the open sheet for rapid presses and still reach the store.
    await user.click(panel.querySelector("[data-testid='board-key-esc']") as HTMLElement);
    const store = await import("../../lib/store");
    expect(store.hubStore.sendKeys).toHaveBeenCalledWith("ins_b", "esc");
    expect(screen.getByTestId("board-actions-panel")).toBeVisible();

    // Escape closes and focus returns to the row trigger.
    await user.keyboard("{Escape}");
    expect(screen.queryByTestId("board-actions-panel")).toBeNull();
    expect(trigger).toHaveFocus();
  });

  it("sends the prompt through the sheet and then closes it", async () => {
    const user = userEvent.setup();
    renderList();
    const card = screen.getAllByTestId("board-card")[0];
    await user.click(card.querySelector("[data-testid='board-more']") as HTMLElement);
    const panel = screen.getByTestId("board-actions-panel");
    await user.type(panel.querySelector("[data-testid='board-prompt']") as HTMLElement, "PAUSE");
    await user.click(panel.querySelector("[data-testid='board-send']") as HTMLElement);
    const store = await import("../../lib/store");
    expect(store.hubStore.send).toHaveBeenCalledWith("ins_b", "PAUSE");
    expect(screen.queryByTestId("board-actions-panel")).toBeNull();
  });

  it("uses the sheet variant under the mobile viewport", async () => {
    const user = userEvent.setup();
    mobileViewport = true;
    renderList();
    await user.click(screen.getAllByTestId("board-more")[0]);
    expect(screen.getByTestId("board-actions-panel")).toHaveAttribute("data-variant", "sheet");
  });
});

describe("SessionList hold-modifier badges (⌘1–9)", () => {
  beforeEach(() => {
    setPlatformForTest(detectPlatform({ platform: "MacIntel" }));
    localStorage.removeItem("remuda.spaces.v1");
  });

  afterEach(() => {
    setPlatformForTest(null);
    mobileViewport = false;
  });

  it("numbers the rows in tab order and advertises the shortcut permanently", () => {
    renderKeyList();
    const rows = screen.getAllByTestId("session-row");
    expect(rows).toHaveLength(2);
    // The blocked group paints before the idle one, so the first *visible*
    // row is the second tab: numbers follow the shared tab ordering Shell's
    // digit handler uses, not the screen position of status groups.
    expect(rows[0]).toHaveAttribute("aria-keyshortcuts", "Meta+2");
    expect(rows[1]).toHaveAttribute("aria-keyshortcuts", "Meta+1");

    const badges = screen.getAllByText(/⌘ [12]/);
    expect(badges).toHaveLength(2);
    // Hidden until held, invisible to assistive tech at all times.
    for (const badge of badges) {
      expect(badge).toHaveAttribute("aria-hidden", "true");
      expect(badge).toHaveAttribute("data-held", "0");
    }
  });

  it("renders Ctrl glyphs and Control+ shortcuts on Windows/Linux", () => {
    setPlatformForTest(detectPlatform({ platform: "Win32" }));
    renderKeyList();
    expect(screen.getAllByText(/Ctrl [12]/)).toHaveLength(2);
    expect(screen.getAllByTestId("session-row")[0]).toHaveAttribute("aria-keyshortcuts", "Control+2");
    expect(screen.getByTestId("session-switch-hint")).toHaveTextContent("按住 Ctrl 快捷切换");
  });

  it("reveals badges only while the modifier is held", async () => {
    renderKeyList();
    const user = userEvent.setup();
    const badge = screen.getAllByText(/⌘ [12]/)[0];
    expect(badge).toHaveAttribute("data-held", "0");

    await user.keyboard("{Meta>}");
    expect(badge).toHaveAttribute("data-held", "1");

    await user.keyboard("{/Meta}");
    expect(badge).toHaveAttribute("data-held", "0");
  });

  it("hides badges again when focus moves into a text field while held", async () => {
    renderKeyList();
    const user = userEvent.setup();
    await user.keyboard("{Meta>}");
    const badge = screen.getAllByText(/⌘ [12]/)[0];
    expect(badge).toHaveAttribute("data-held", "1");

    await user.click(screen.getByTestId("session-search"));
    expect(badge).toHaveAttribute("data-held", "0");
  });

  it("shows the discreet hold hint on desktop but neither hint nor badges on a touch platform", () => {
    const desktop = renderKeyList();
    expect(screen.getByTestId("session-switch-hint")).toHaveTextContent("按住 ⌘ 快捷切换");
    desktop.unmount();

    setPlatformForTest(detectPlatform({ platform: "iPad", maxTouchPoints: 5 }));
    renderKeyList();
    expect(screen.queryByTestId("session-switch-hint")).toBeNull();
    expect(screen.queryByText(/⌘ /)).toBeNull();
    expect(screen.getAllByTestId("session-row")[0]).not.toHaveAttribute("aria-keyshortcuts");
  });

  it("renders no badges or hint in the mobile layout", () => {
    mobileViewport = true;
    renderKeyList();
    expect(screen.queryByTestId("session-switch-hint")).toBeNull();
    expect(screen.queryByText(/⌘ /)).toBeNull();
  });
});
