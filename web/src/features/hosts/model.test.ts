import { describe, expect, it } from "vitest";
import { HOST_FIXTURES } from "./fixtures";
import { carrierOf, hostsMatching, type Placement } from "./model";

describe("host placement and carriers", () => {
  it("maps ssh-dev/ssh-tunnel onto ssh-stdio", () => {
    expect(carrierOf("outbound-wss")).toBe("outbound-wss");
    expect(carrierOf("local")).toBe("local");
    expect(carrierOf("ssh-dev")).toBe("ssh-stdio");
    expect(carrierOf("ssh-tunnel")).toBe("ssh-stdio");
    expect(carrierOf("ssh-stdio")).toBe("ssh-stdio");
  });

  it("covers three transports and an offline host in fixtures", () => {
    const modes = new Set(HOST_FIXTURES.map((h) => h.transport));
    expect(modes.has("ssh-stdio")).toBe(true);
    expect(modes.has("outbound-wss")).toBe(true);
    expect(modes.has("local")).toBe(true);
    expect(HOST_FIXTURES.some((h) => !h.online)).toBe(true);
  });

  it("matches host / labels / any without silent downgrade", () => {
    const sg = HOST_FIXTURES.find((h) => h.label === "devbox-sg")!;
    const host: Placement = { kind: "host", hostId: sg.id };
    expect(hostsMatching(HOST_FIXTURES, host).map((h) => h.label)).toEqual(["devbox-sg"]);
    const labels: Placement = { kind: "labels", labels: ["region:sg", "herdr"] };
    expect(hostsMatching(HOST_FIXTURES, labels).every((h) => h.labels.includes("region:sg"))).toBe(true);
    expect(hostsMatching(HOST_FIXTURES, { kind: "any" }).every((h) => h.online)).toBe(true);
    expect(hostsMatching(HOST_FIXTURES, { kind: "labels", labels: ["no-such-label"] })).toEqual([]);
  });
});
