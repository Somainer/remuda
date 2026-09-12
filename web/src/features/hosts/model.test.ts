import { describe, expect, it } from "vitest";
import { HOST_FIXTURES } from "./fixtures";
import {
  STALE_OFFLINE_MS,
  carrierOf,
  cliSummary,
  compactCliVersion,
  hostsMatching,
  isStaleOffline,
  sortHostsOnlineFirst,
  type Placement,
} from "./model";

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

  it("sorts online hosts first and hides stale offline", () => {
    const now = Date.parse("2026-09-13T12:00:00.000Z");
    const ordered = sortHostsOnlineFirst(HOST_FIXTURES);
    expect(ordered[0]?.online).toBe(true);
    expect(ordered.find((h) => h.label === "forge-doloris")?.online).toBe(false);
    const stale = HOST_FIXTURES.find((h) => h.label === "forge-doloris")!;
    expect(isStaleOffline(stale, now)).toBe(true);
    expect(isStaleOffline(stale, Date.parse(stale.lastSeenAt!) + STALE_OFFLINE_MS - 1)).toBe(false);
    expect(cliSummary(HOST_FIXTURES[0]?.cli)).toContain("claude");
    expect(cliSummary(HOST_FIXTURES[0]?.cli)).toContain("grok");
    expect(compactCliVersion("claude", "2.1.269 (Claude Code)")).toBe("claude 2.1.269");
    expect(compactCliVersion("codex", "codex-cli 0.154.0")).toBe("codex 0.154.0");
    expect(compactCliVersion("grok", "grok 1.0.30 (04b7ffed98c6)")).toBe("grok 1.0.30");
  });
});
