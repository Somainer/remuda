import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import { SessionPage } from "./SessionPage";

const mockViewportState = { mobile: false };
vi.mock("../lib/viewport", () => ({
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

it("compact renders provenance/promotion exactly once, inside the disclosure", () => {
  
  const withMarks = { ...grokInstance, launchedBy: "remuda" as const };
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [withMarks],
    events: { [withMarks.id]: [] },
  });
  mockViewportState.mobile = true;
  stubMatchMedia(() => true);
  renderPage();

  // One node, in the disclosure — not the CSS-hidden duplicate the old row
  // rendered.
  expect(screen.getAllByTestId("launched-by")).toHaveLength(1);
  expect(screen.getByTestId("run-details")).toContainElement(screen.getByTestId("launched-by"));
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
