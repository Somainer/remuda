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

it("desktop keeps host, cost, switch, Stop and the toggles on the main row, with run details folded", () => {
  renderPage();

  const details = screen.getByTestId("run-details");
  expect(details).not.toHaveAttribute("open");
  expect(screen.getByTestId("session-host")).toBeVisible();
  expect(screen.getByTestId("session-cost")).toBeVisible();
  expect(screen.getByTestId("view-switch")).toBeVisible();
  expect(screen.getByRole("button", { name: "Stop" })).toBeVisible();
  expect(screen.getByTestId("density-toggle")).toBeVisible();
  expect(screen.getByTestId("files-toggle")).toBeVisible();
  expect(screen.getByTestId("events-toggle")).toBeVisible();
  expect(screen.queryByTestId("session-more-open")).toBeNull();
  // The diagnostic row is the disclosure body, not a second visible row.
  expect(screen.getByTestId("session-meta")).not.toBeVisible();
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

it("mobile folds the chips strip into one header chip and moves the toggles into the ⋯ sheet", () => {
  mockViewportState.mobile = true;
  stubMatchMedia((query) => query.includes("640"));
  renderPage();

  // The full strip's space-chip buttons are gone; the one current-space chip
  // opens the same drawer.
  expect(screen.queryAllByTestId("space-chip")).toHaveLength(0);
  expect(screen.getByTestId("spaces-drawer-open")).toBeVisible();

  // The segmented switch and Stop are permanent main-row citizens.
  expect(screen.getByTestId("view-switch")).toBeVisible();
  expect(screen.getByRole("button", { name: "Stop" })).toBeVisible();
  expect(screen.queryByTestId("session-host")).toBeNull();
  expect(screen.queryByTestId("density-toggle")).toBeNull();
  expect(screen.queryByTestId("files-toggle")).toBeNull();
  expect(screen.queryByTestId("events-toggle")).toBeNull();

  fireEvent.click(screen.getByTestId("session-more-open"));
  const sheet = screen.getByTestId("session-more-sheet");
  expect(within(sheet).getByTestId("density-toggle")).toHaveAttribute("role", "menuitem");
  expect(within(sheet).getByTestId("files-toggle")).toHaveAttribute("role", "menuitem");
  expect(within(sheet).getByTestId("events-toggle")).toHaveAttribute("role", "menuitem");
  // The switch and Stop must never be reachable only through the menu.
  expect(within(sheet).queryByTestId("view-switch")).toBeNull();
  expect(within(sheet).queryByRole("button", { name: "Stop" })).toBeNull();

  fireEvent.click(within(sheet).getByTestId("files-toggle"));
  expect(screen.getByTestId("files-route")).toBeInTheDocument();
});

it("a compact-but-wide (767px, no touch) window keeps the toggles inline", () => {
  mockViewportState.mobile = true;
  stubMatchMedia(() => false);
  renderPage();

  expect(screen.queryByTestId("session-more-open")).toBeNull();
  expect(screen.getByTestId("density-toggle")).toBeVisible();
  expect(screen.getByTestId("files-toggle")).toBeVisible();
  expect(screen.getByTestId("events-toggle")).toBeVisible();
});

it("the collapsed trigger advertises the exact number of fields it reveals (desktop and compact)", () => {
  const advertised = () =>
    Number(screen.getByTestId("run-details-summary").textContent?.match(/(\d+) 项运行信息/)?.[1]);
  // The meta body alternates field · separator, so fields = (children+1)/2.
  const renderedFields = () =>
    (screen.getByTestId("session-meta").children.length + 1) / 2;

  // Desktop
  mockViewportState.mobile = false;
  let result = renderPage();
  expect(advertised()).toBe(renderedFields());

  // Compact: host + cost move into the disclosure, so the honest count rises.
  result.unmount();
  mockViewportState.mobile = true;
  stubMatchMedia(() => true);
  result = renderPage();
  expect(advertised()).toBe(renderedFields());
});

it.each([
  // Crowded phone (390px): every layout query matches.
  ["crowded phone", () => true],
  // Coarse-pointer compact-but-wide (e.g. 900×600): the workbench compact
  // query matches but the 640px crowded one does not — mobile is true,
  // crowded is false, and the badges must still render once in the
  // disclosure rather than twice.
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

it("remembers the run-details open state on this device across remounts", async () => {
  const result = renderPage();
  expect(screen.getByTestId("run-details")).not.toHaveAttribute("open");

  fireEvent.click(screen.getByTestId("run-details-summary"));
  await waitFor(() => expect(screen.getByTestId("run-details")).toHaveAttribute("open"));
  expect(screen.getByTestId("session-meta")).toBeVisible();
  expect(localStorage.getItem("runtime.run-details.open")).toBe("1");

  result.unmount();
  renderPage();
  expect(screen.getByTestId("run-details")).toHaveAttribute("open");

  fireEvent.click(screen.getByTestId("run-details-summary"));
  await waitFor(() => expect(screen.getByTestId("run-details")).not.toHaveAttribute("open"));
  expect(localStorage.getItem("runtime.run-details.open")).toBe("0");
});
