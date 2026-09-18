import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { Instance } from "../../types/instance";
import { known } from "../../types/wire";
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
    summaryOf: () => "",
    hostName: (id: string) => hostNames[id] ?? id,
    effortOf: () => ({ name: "medium", index: 2, ultracode: false }),
    effortEffectiveOf: () => null,
    modelOf: () => "model_hub/es1_orange_o50[1m]",
    modelEffectiveOf: (id: string) => modelEffective[id] ?? null,
    refreshScreens: vi.fn(),
    broadcast: vi.fn(),
    send: vi.fn(),
    sendKeys: vi.fn(),
    close: vi.fn(),
  },
}));

const titles: Record<string, string> = {};
const hostNames: Record<string, string> = { "host-a": "alpha", "host-b": "beta" };
/// Per-instance effective-model observations the mocked store hands back
/// (D-036 / model-pin-1). Empty by default: most rows have not read one back.
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
    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("session-filter-panel")).toHaveAttribute("aria-modal", "true");
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

describe("SessionList effective-model label (D-036 / model-pin-1)", () => {
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

  // The regression: the pin was requested and something else answered. The row
  // must show what answered and mark the divergence, because showing only the
  // request is what made the substituted model invisible.
  it("shows the observed id and marks a divergence when the pin was not honoured", () => {
    for (const id of ["ins_a", "ins_b"]) {
      modelEffective[id] = {
        id: "claude-opus-4-8",
        source: "launch",
        observedAt: "2026-09-18T00:00:00Z",
      };
    }
    renderList();
    const label = screen.getAllByTestId("session-model")[0];
    expect(label).toHaveTextContent("claude-opus-4-8");
    expect(label).toHaveAttribute("data-model-effective", "claude-opus-4-8");
    expect(label).toHaveAttribute("data-model-diverged", "1");
    // Both ids stay legible, so the pin that was asked for is not lost.
    expect(label.getAttribute("title")).toContain("model_hub/es1_orange_o50[1m]");
    expect(label.getAttribute("title")).toContain("claude-opus-4-8");
  });
});

function renderKeyList() {
  return render(
    <MemoryRouter initialEntries={["/sessions"]}>
      <SessionList variant="full" instances={hub.instances} title="sfe-root" space={derivedSpace} />
    </MemoryRouter>,
  );
}

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
