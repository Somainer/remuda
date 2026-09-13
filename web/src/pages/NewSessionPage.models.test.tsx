import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { api, type HubProviderRow } from "../lib/api";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import { NewSessionPage } from "./NewSessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false }), composing: () => false,
}));

const host = { ...mockDb.hosts[0], state: "online" as const };
const workspace = { ...mockDb.workspaces[0], id: "wsp_project", hostId: host.id, rootPath: "/home/dev/projects/app" };

/**
 * The list endpoint's structured shape: two of three models exposed, matching
 * what `providers-discovery.spec.ts` saves before opening New Session.
 */
const structured: HubProviderRow = {
  id: "pvp_e2e",
  name: "e2e-fake-upstream",
  kind: "gateway",
  baseUrl: "http://127.0.0.1:58881/v1",
  models: [
    { id: "e2e/auto", enabled: true, label: "E2E Auto", contextWindow: 1_048_576, tags: ["1m"] },
    { id: "e2e/fast", enabled: true, label: "E2E Fast", contextWindow: 200_000 },
    { id: "e2e/plain", enabled: false },
  ],
  defaultModel: "e2e/auto",
  defaultGateway: true,
  scope: "universal",
  revision: "2",
  secret: { present: true, last4: "qqqq" },
};

/** The legacy `["id", …]` shape a migrated row can still serve. */
const legacy: HubProviderRow = {
  ...structured,
  models: ["e2e/auto", "e2e/fast"],
};

function optionValues() {
  return Array.from(screen.getByTestId("new-session-model").querySelectorAll("option")).map(
    (o) => (o as HTMLOptionElement).value,
  );
}

beforeEach(() => {
  localStorage.clear();
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [host], workspaces: [workspace], instances: [],
  });
});
afterEach(() => vi.restoreAllMocks());

it("offers only the enabled models of the structured profile shape", async () => {
  vi.spyOn(api, "providerList").mockResolvedValue({ items: [structured] });
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
  await waitFor(() => expect(screen.getByTestId("new-session-delegation-gateway")).toBeEnabled());

  fireEvent.click(screen.getByTestId("new-session-delegation-gateway"));

  await waitFor(() => expect(optionValues()).toEqual(["e2e/auto", "e2e/fast"]));
  expect(screen.getByTestId("new-session-model")).toHaveValue("e2e/auto");
});

it("offers every model of the legacy bare-id shape, which carries no enabled flag", async () => {
  vi.spyOn(api, "providerList").mockResolvedValue({ items: [legacy] });
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
  await waitFor(() => expect(screen.getByTestId("new-session-delegation-gateway")).toBeEnabled());

  fireEvent.click(screen.getByTestId("new-session-delegation-gateway"));

  await waitFor(() => expect(optionValues()).toEqual(["e2e/auto", "e2e/fast"]));
});

/**
 * The regression CI caught: a remembered model that the catalog does not expose
 * (here the disabled `e2e/plain`) must not be re-added as a third option.
 */
it("drops a remembered model the catalog no longer exposes instead of offering it", async () => {
  localStorage.setItem(
    "runtime.new-session",
    JSON.stringify({ model: "e2e/plain", delegation: "gateway" }),
  );
  vi.spyOn(api, "providerList").mockResolvedValue({ items: [structured] });
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);

  await waitFor(() => expect(optionValues()).toEqual(["e2e/auto", "e2e/fast"]));
  expect(screen.getByTestId("new-session-model")).toHaveValue("e2e/auto");
});

it("keeps the picker on the enabled catalog when the profile arrives after first paint", async () => {
  let release: (page: { items: HubProviderRow[] }) => void = () => undefined;
  vi.spyOn(api, "providerList").mockReturnValue(
    new Promise((resolve) => {
      release = resolve;
    }),
  );
  localStorage.setItem(
    "runtime.new-session",
    JSON.stringify({ model: "e2e/plain", delegation: "gateway" }),
  );
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);

  // Before the profile lands there is no catalog to constrain the box.
  expect(screen.getByTestId("new-session-model")).toBeInstanceOf(HTMLInputElement);

  release({ items: [structured] });

  await waitFor(() => expect(optionValues()).toEqual(["e2e/auto", "e2e/fast"]));
});
