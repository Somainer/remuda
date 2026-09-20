import { describe, expect, it } from "vitest";
import { apiRouteClause, apiRouteKind, routeDownMessage } from "./apiRoute";
import type { Instance } from "../types/instance";

describe("apiRouteClause — the session strip's D-035 clause", () => {
  it("renders nothing until a Node echo exists, never the requested route", () => {
    expect(apiRouteClause(undefined)).toBeNull();
    expect(apiRouteClause(null)).toBeNull();
  });

  it("renders 直连 for a direct echo", () => {
    expect(apiRouteClause({ mode: "direct" })).toBe("直连");
  });

  it("renders the proxy host label and the resolved kind", () => {
    const route: Instance["apiRoute"] = {
      mode: "via",
      route: "hub-relay",
      viaHostId: "hst_mac",
      viaHostLabel: "mac-relay",
    };
    expect(apiRouteClause(route)).toBe("经 mac-relay Hub 中转");
    expect(apiRouteKind(route)).toBe("hub-relay");

    const net: Instance["apiRoute"] = {
      mode: "via",
      route: "direct-net",
      viaHostId: "hst_mac",
      viaHostLabel: "mac-relay",
    };
    expect(apiRouteClause(net)).toBe("经 mac-relay 直连网络");
    expect(apiRouteKind(net)).toBe("direct-net");
  });

  it("falls back to the host id, then the Hub host, never a blank label", () => {
    expect(
      apiRouteClause({ mode: "via", route: "hub-relay", viaHostId: "hst_sg" }),
    ).toBe("经 hst_sg Hub 中转");
    // apiVia self carries no host id on the echo.
    expect(apiRouteClause({ mode: "via", route: "hub-relay" })).toBe(
      "经 Hub 主机 Hub 中转",
    );
  });
});

describe("routeDownMessage", () => {
  it("is null without an api_route_down diagnostic", () => {
    expect(routeDownMessage(undefined)).toBeNull();
    expect(
      routeDownMessage([
        { kind: "lifecycle", payload: { type: "native", nativeName: "live.status" } },
      ]),
    ).toBeNull();
  });

  it("returns the latest api_route_down message", () => {
    const events = [
      {
        kind: "lifecycle" as const,
        payload: {
          type: "native",
          topic: "diagnostic",
          origin: "hub",
          nativeName: "api_route_down",
          message: "the proxy host hst_mac went away; the API route is down (no reroute)",
        },
      },
    ];
    expect(routeDownMessage(events)).toBe(
      "the proxy host hst_mac went away; the API route is down (no reroute)",
    );
  });
});
