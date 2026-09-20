import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import { ttyLabInstance } from "../features/session/tty/fixture";
import { SessionPage } from "./SessionPage";

function routeInstance(apiRoute: Instance["apiRoute"]): Instance {
  return { ...ttyLabInstance(), apiRoute };
}

beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
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

async function renderSession(instance: Instance, events: unknown[] = []) {
  const store = await import("../lib/store");
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "titleOf").mockReturnValue("routed");
  vi.spyOn(store.hubStore, "hostName").mockReturnValue("local");
  vi.spyOn(store.hubStore, "workspaceOf").mockReturnValue(undefined);
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [instance],
    events: { [instance.id]: events },
    interactions: [],
    bubbles: [],
    hosts: [],
    journalStatus: {},
  } as ReturnType<typeof store.useHub>);
  return render(
    <MemoryRouter initialEntries={[`/s/${instance.id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("Session strip API route clause (D-047 / D-035)", () => {
  it("renders the echoed hub-relay clause naming the proxy host", async () => {
    await renderSession(
      routeInstance({
        mode: "via",
        route: "hub-relay",
        viaHostId: "hst_mac",
        viaHostLabel: "mac-relay",
      }),
    );
    const clause = screen.getByTestId("session-api-route");
    expect(clause).toHaveTextContent("经 mac-relay Hub 中转");
    expect(clause).toHaveAttribute("data-mode", "via");
    expect(clause).toHaveAttribute("data-route", "hub-relay");
    expect(screen.queryByTestId("session-api-route-down")).toBeNull();
  });

  it("renders 直连 for a direct echo", async () => {
    await renderSession(routeInstance({ mode: "direct" }));
    expect(screen.getByTestId("session-api-route")).toHaveTextContent("直连");
  });

  it("renders nothing when the instance carries no echo (intent is not fact)", async () => {
    await renderSession(routeInstance(null));
    expect(screen.queryByTestId("session-api-route")).toBeNull();
  });

  it("shows api-route-down as an error while still naming the via route (no reroute)", async () => {
    await renderSession(
      routeInstance({
        mode: "via",
        route: "hub-relay",
        viaHostId: "hst_mac",
        viaHostLabel: "mac-relay",
      }),
      [
        {
          kind: "lifecycle",
          payload: {
            type: "native",
            topic: "diagnostic",
            origin: "hub",
            nativeName: "api_route_down",
            message: "the proxy host hst_mac went away; the API route is down (no reroute)",
          },
        },
      ],
    );
    // The route clause stays a via route — the session did not reroute.
    expect(screen.getByTestId("session-api-route")).toHaveTextContent(
      "经 mac-relay Hub 中转",
    );
    const down = screen.getByTestId("session-api-route-down");
    expect(down).toBeTruthy();
    expect(down).toHaveAttribute("role", "alert");
    expect(down).toHaveTextContent("api-route-down");
    expect(down).toHaveTextContent("经 mac-relay Hub 中转");
  });
});
