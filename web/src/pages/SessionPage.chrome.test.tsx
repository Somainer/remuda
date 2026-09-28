import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mapInstance } from "../lib/api";
import { mockDb } from "../lib/mock";
import { SessionPage } from "./SessionPage";

const mockViewportState = { mobile: false };
// Keep the module's real exports (e.g. COMPACT_WORKBENCH_QUERY, read by the
// Transcript/ToolCard layout hooks) while pinning the viewport state.
vi.mock("../lib/viewport", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/viewport")>()),
  useWorkbenchViewport: () => ({ mobile: mockViewportState.mobile, offsetTop: 0 }),
  composing: () => false,
}));

const grokInstance =
  mockDb.instances.find((instance) => instance.driver === "generic-pty") ?? mockDb.instances[0];

function renderPage() {
  return render(
    <MemoryRouter initialEntries={[`/s/${grokInstance.id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
        <Route path="/s/:instanceId/files" element={<div data-testid="files-route" />} />
      </Routes>
    </MemoryRouter>,
  );
}

/** Static matchMedia: every query matches iff `matchesFor` says so. */
function stubMatchMedia(matchesFor: (query: string) => boolean) {
  const mql = (query: string): MediaQueryList =>
    ({
      matches: matchesFor(query),
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => true,
      addListener: () => {},
      removeListener: () => {},
    }) as MediaQueryList;
  vi.stubGlobal("matchMedia", mql);
}

beforeEach(() => {
  mockViewportState.mobile = false;
  localStorage.clear();
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [grokInstance],
    events: { [grokInstance.id]: [] },
  });
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it("wide desktop keeps host, cost, switch, 文件, Stop and ⋯ on one row, with run details closed", () => {
  renderPage();

  const header = screen.getByRole("banner");
  expect(header).toHaveAttribute("data-layout", "desktop");
  expect(within(header).getByTestId("session-host")).toBeVisible();
  expect(within(header).getByTestId("session-cost")).toBeVisible();
  expect(within(header).getByTestId("view-switch")).toBeVisible();
  expect(within(header).getByTestId("files-toggle")).toBeVisible();
  expect(within(header).getByRole("button", { name: "Stop" })).toBeVisible();
  expect(within(header).getByTestId("session-more-open")).toBeVisible();
  // Density and raw events live in ⋯ only (D-053).
  expect(screen.queryByTestId("density-toggle")).toBeNull();
  expect(screen.queryByTestId("events-toggle")).toBeNull();
  // Run details takes no row until ⋯ opens it.
  expect(screen.getByTestId("run-details")).not.toBeVisible();
});

it("desktop ⋯ lists the §2.2 items in order and never the switch, 文件 or Stop", () => {
  renderPage();
  fireEvent.click(screen.getByTestId("session-more-open"));

  const menu = within(screen.getByTestId("session-more-popover")).getByRole("menu");
  const ids = Array.from(menu.querySelectorAll("[role^='menuitem']")).map((item) =>
    item.getAttribute("data-testid"),
  );
  expect(ids[0]).toBe("run-details-summary");
  expect(ids).toContain("density-toggle");
  expect(ids.at(-1)).toBe("events-toggle");
  expect(ids).not.toContain("files-toggle");
  expect(within(menu).queryByTestId("view-switch")).toBeNull();
  expect(within(menu).queryByRole("button", { name: "Stop" })).toBeNull();

  fireEvent.keyDown(menu, { key: "Escape" });
  expect(screen.queryByTestId("session-more-popover")).toBeNull();
  expect(screen.getByTestId("session-more-open")).toHaveFocus();
});

it("768–1023 moves 文件 into ⋯ and keeps the title", () => {
  stubMatchMedia(() => false);
  renderPage();

  const header = screen.getByRole("banner");
  expect(header).toHaveAttribute("data-layout", "desktop");
  expect(within(header).getByRole("heading", { level: 1 })).toBeVisible();
  expect(within(header).queryByTestId("files-toggle")).toBeNull();

  fireEvent.click(screen.getByTestId("session-more-open"));
  fireEvent.click(within(screen.getByTestId("session-more-popover")).getByTestId("files-toggle"));
  expect(screen.getByTestId("files-route")).toBeInTheDocument();
});

it("renders a recorded model_pin_mismatch in run details with both ids verbatim", () => {
  // model-pin-1 §5.4: the launch divergence is the Node's authoritative
  // diagnostic, shown in run details — never recomputed into a chip pair.
  const events = [
    {
      eventId: "evt_pin_1",
      instanceId: grokInstance.id,
      journalId: "obj",
      seq: "1",
      kind: "lifecycle",
      observedAt: "2026-09-24T00:00:00Z",
      source: { channel: "runtime" },
      payload: {
        type: "native",
        topic: "diagnostic",
        nativeName: "model_pin_mismatch",
        nativeId: { state: "not-applicable" },
        status: { state: "known", value: "diverged" },
        severity: "warning",
        affectsCompletion: false,
        dataRef: null,
        relatedIds: {
          reason: "model-mismatch",
          requested: "passthrough/ark/seed-evolving",
          observed: "ark/seed-evolving",
        },
      },
    },
  ];
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [grokInstance],
    events: { [grokInstance.id]: events as never[] },
  });
  renderPage();
  const pin = screen.getByTestId("run-details-model-pin");
  expect(pin).toHaveTextContent("请求模型 passthrough/ark/seed-evolving，实际运行 ark/seed-evolving");
  expect(pin).toHaveAttribute("data-requested", "passthrough/ark/seed-evolving");
  expect(pin).toHaveAttribute("data-observed", "ark/seed-evolving");
});

it("renders a projected model_pin_mismatch through the real API mapper even with an empty window", () => {
  // model-pin-1 §5.4 regression: the durable divergence reaches the browser
  // through the public instance JSON -> mapInstance (the actual wire
  // boundary), and run details renders it with an EMPTY event window (the
  // diagnostic has aged out of the bounded tail). Nothing is hand-injected.
  const projected = mapInstance({
    instanceId: grokInstance.id,
    hostId: grokInstance.hostId,
    workspaceId: grokInstance.workspaceId,
    kind: "generic-pty",
    driver: grokInstance.driver,
    lifecycle: "ready",
    activity: "idle",
    connectivity: "connected",
    journalId: grokInstance.journalId,
    durableSeq: "1",
    modelPinMismatches: [
      {
        requested: "model_hub/es1_orange_o50[1m]",
        observed: "model_hub/es1_orange_o48[1m]",
        observedAt: "2026-09-24T00:00:00.000Z",
      },
    ],
  } as never);
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [projected],
    // Deliberately empty window: the diagnostic is older than 2000 events.
    events: { [grokInstance.id]: [] },
  });
  renderPage();
  const pins = screen.getAllByTestId("run-details-model-pin");
  expect(pins).toHaveLength(1);
  expect(pins[0]).toHaveTextContent(
    "请求模型 model_hub/es1_orange_o50[1m]，实际运行 model_hub/es1_orange_o48[1m]",
  );
});

it("deduplicates a projected diagnostic and the same launch event still in the window", () => {
  // The steady state: projected record + the in-tail event for the same
  // divergence must render ONCE (identity is requested+observed).
  const projected = mapInstance({
    instanceId: grokInstance.id,
    hostId: grokInstance.hostId,
    workspaceId: grokInstance.workspaceId,
    kind: "generic-pty",
    driver: grokInstance.driver,
    lifecycle: "ready",
    activity: "idle",
    connectivity: "connected",
    journalId: grokInstance.journalId,
    durableSeq: "2",
    modelPinMismatches: [
      {
        requested: "passthrough/ark/seed-evolving",
        observed: "ark/seed-evolving",
        observedAt: "2026-09-24T00:00:00.000Z",
      },
    ],
  } as never);
  const events = [
    {
      eventId: "evt_pin_live",
      instanceId: grokInstance.id,
      journalId: "obj",
      seq: "1",
      kind: "lifecycle",
      observedAt: "2026-09-24T00:00:00.000Z",
      source: { channel: "runtime" },
      payload: {
        type: "native",
        topic: "diagnostic",
        nativeName: "model_pin_mismatch",
        nativeId: { state: "not-applicable" },
        status: { state: "known", value: "diverged" },
        severity: "warning",
        affectsCompletion: false,
        dataRef: null,
        relatedIds: {
          reason: "model-mismatch",
          requested: "passthrough/ark/seed-evolving",
          observed: "ark/seed-evolving",
        },
      },
    },
  ];
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [projected],
    events: { [grokInstance.id]: events as never[] },
  });
  renderPage();
  expect(screen.getAllByTestId("run-details-model-pin")).toHaveLength(1);
});

it("compact is one row: 返回, the title block, switch, Stop and ⋯, with the toggles in the sheet", () => {
  mockViewportState.mobile = true;
  stubMatchMedia(() => false);
  renderPage();

  const header = screen.getByRole("banner");
  expect(header).toHaveAttribute("data-layout", "compact");
  // No chip strip: exactly one Space name, inside the title block that opens
  // the drawer.
  expect(screen.queryAllByTestId("space-chip")).toHaveLength(0);
  expect(within(header).getAllByTestId("spaces-chips")).toHaveLength(1);
  expect(screen.getByTestId("spaces-drawer-open")).toContainElement(screen.getByTestId("spaces-chips"));

  // The segmented switch and Stop are permanent main-row citizens.
  expect(within(header).getByRole("link", { name: "返回" })).toBeVisible();
  expect(within(header).getByTestId("view-switch")).toBeVisible();
  expect(within(header).getByRole("button", { name: "Stop" })).toBeVisible();
  expect(screen.queryByTestId("session-host")).toBeNull();
  expect(screen.queryByTestId("density-toggle")).toBeNull();
  expect(screen.queryByTestId("files-toggle")).toBeNull();
  expect(screen.queryByTestId("events-toggle")).toBeNull();

  fireEvent.click(screen.getByTestId("session-more-open"));
  const sheet = screen.getByTestId("session-more-sheet");
  expect(within(sheet).getByTestId("density-toggle")).toHaveAttribute("role", "menuitemcheckbox");
  expect(within(sheet).getByTestId("files-toggle")).toHaveAttribute("role", "menuitemcheckbox");
  expect(within(sheet).getByTestId("events-toggle")).toHaveAttribute("role", "menuitemcheckbox");
  // The switch and Stop must never be reachable only through the menu.
  expect(within(sheet).queryByTestId("view-switch")).toBeNull();
  expect(within(sheet).queryByRole("button", { name: "Stop" })).toBeNull();

  fireEvent.click(within(sheet).getByTestId("files-toggle"));
  expect(screen.getByTestId("files-route")).toBeInTheDocument();
});

it("the ⋯ item advertises the exact number of fields run details reveals (desktop and compact)", () => {
  const advertised = () => {
    fireEvent.click(screen.getByTestId("session-more-open"));
    const count = Number(
      screen.getByTestId("run-details-summary").textContent?.match(/运行详情 · (\d+) 项/)?.[1],
    );
    fireEvent.click(screen.getByTestId("session-more-open"));
    return count;
  };
  // The meta body alternates field · separator, so fields = (children+1)/2.
  const renderedFields = () =>
    (screen.getByTestId("session-meta").children.length + 1) / 2;

  // Desktop
  mockViewportState.mobile = false;
  let result = renderPage();
  expect(advertised()).toBe(renderedFields());

  // Compact: host + cost move into the panel, so the honest count rises.
  result.unmount();
  mockViewportState.mobile = true;
  stubMatchMedia(() => true);
  result = renderPage();
  expect(advertised()).toBe(renderedFields());
});

it("desktop renders provenance and promotion once, inside run details", () => {
  const withMarks = { ...grokInstance, launchedBy: "remuda" as const, mode: "promoted" as const };
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [withMarks],
    events: { [withMarks.id]: [] },
  });
  renderPage();

  const details = screen.getByTestId("run-details");
  expect(screen.getAllByTestId("launched-by")).toHaveLength(1);
  expect(screen.getAllByTestId("promoted-badge")).toHaveLength(1);
  expect(details).toContainElement(screen.getByTestId("launched-by"));
  expect(details).toContainElement(screen.getByTestId("promoted-badge"));
});

it.each([
  // Phone (390px): every layout query matches.
  ["phone", () => true],
  // Coarse-pointer compact-but-wide (e.g. 900×600): compact layout, but not
  // every width query matches — the badges must still render exactly once.
  ["coarse compact-but-wide", (query: string) => !query.includes("640")],
])("compact (%s) renders provenance and promotion exactly once, inside the disclosure", (_label, matchesFor) => {
  const withMarks = {
    ...grokInstance,
    launchedBy: "remuda" as const,
    mode: "promoted" as const,
  };
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [withMarks],
    events: { [withMarks.id]: [] },
  });
  mockViewportState.mobile = true;
  stubMatchMedia(matchesFor);
  const result = renderPage();

  // Exactly one of each badge, and it lives in the disclosure — never a
  // CSS-hidden main-row duplicate plus a second disclosure node.
  expect(screen.getAllByTestId("launched-by")).toHaveLength(1);
  expect(screen.getAllByTestId("promoted-badge")).toHaveLength(1);
  const details = screen.getByTestId("run-details");
  expect(details).toContainElement(screen.getByTestId("launched-by"));
  expect(details).toContainElement(screen.getByTestId("promoted-badge"));

  result.unmount();
});

it("run details opens from ⋯ and remembers its state on this device across remounts", async () => {
  const openFromMenu = () => {
    fireEvent.click(screen.getByTestId("session-more-open"));
    fireEvent.click(screen.getByTestId("run-details-summary"));
  };
  const result = renderPage();
  expect(screen.getByTestId("run-details")).not.toBeVisible();

  openFromMenu();
  await waitFor(() => expect(screen.getByTestId("run-details")).toHaveAttribute("open"));
  expect(screen.getByTestId("session-meta")).toBeVisible();
  expect(localStorage.getItem("runtime.run-details.open")).toBe("1");

  result.unmount();
  renderPage();
  expect(screen.getByTestId("run-details")).toHaveAttribute("open");
  fireEvent.click(screen.getByTestId("session-more-open"));
  expect(screen.getByTestId("run-details-summary")).toHaveAttribute("aria-checked", "true");
  fireEvent.click(screen.getByTestId("session-more-open"));

  // The panel heading folds it back too.
  fireEvent.click(screen.getByTestId("run-details-heading"));
  await waitFor(() => expect(screen.getByTestId("run-details")).not.toBeVisible());
  expect(localStorage.getItem("runtime.run-details.open")).toBe("0");
});
