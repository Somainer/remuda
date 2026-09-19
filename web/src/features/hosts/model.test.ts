import { describe, expect, it } from "vitest";
import { HOST_FIXTURES } from "./fixtures";
import {
  COMPUTER_USE_KIND,
  STALE_OFFLINE_MS,
  absentCli,
  carrierOf,
  cliSummary,
  compactCliVersion,
  computerUseState,
  hostsMatching,
  installedCli,
  isStaleOffline,
  sortHostsOnlineFirst,
  supportedHarnessKinds,
  type HostCli,
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

describe("computer-use host capability row (D-045 §3.4)", () => {
  it("labels an installed row without eating the kind in cliSummary", () => {
    const installed = { kind: COMPUTER_USE_KIND, version: "2.7.0", path: "/home/x/client", auth: "unknown" as const, installed: true };
    // The `<kind>-cli ` prefix strip must leave `computer-use` intact rather
    // than swallowing it: the kind is not a product-name prefix here.
    expect(compactCliVersion(COMPUTER_USE_KIND, "2.7.0")).toBe("computer-use 2.7.0");
    expect(cliSummary([installed])).toBe("computer-use 2.7.0");
    expect(cliSummary([installed])).not.toBe("2.7.0");
  });

  it("keeps 'no', 'not installed' and 'not reported' distinct", () => {
    const installed: HostCli = { kind: COMPUTER_USE_KIND, version: "2.7.0", path: "/home/x/client", auth: "unknown", installed: true };
    const absent: HostCli = { kind: COMPUTER_USE_KIND, auth: "unknown", installed: false };
    const omitted: HostCli[] = [{ kind: "claude", version: "1", path: "/usr/bin/claude", auth: "unknown" }];

    expect(computerUseState([installed])).toEqual({ reported: true, installed: true, version: "2.7.0", path: "/home/x/client" });
    expect(computerUseState([absent])).toEqual({ reported: true, installed: false });
    expect(computerUseState(omitted)).toEqual({ reported: false });
    expect(computerUseState(undefined)).toEqual({ reported: false });
  });

  it("drops a reported-absent row from the installed list but keeps it as absent", () => {
    const absent: HostCli = { kind: COMPUTER_USE_KIND, auth: "unknown", installed: false };
    expect(installedCli([absent])).toEqual([]);
    expect(cliSummary([absent])).toBe("");
    expect(absentCli([absent])).toEqual([absent]);
  });

  it("treats a flagless legacy row by the path/version heuristic", () => {
    // A Node that predates `installed` sends neither flag; the heuristic is
    // what keeps those rows on screen.
    const legacyPresent: HostCli = { kind: "claude", version: "2.1.268", path: "/usr/bin/claude", auth: "logged_in" };
    const legacyBare: HostCli = { kind: "claude", auth: "logged_in" };
    expect(installedCli([legacyPresent])).toEqual([legacyPresent]);
    expect(installedCli([legacyBare])).toEqual([]);
    // Neither is "absent": absence is an explicit claim by the Node.
    expect(absentCli([legacyPresent, legacyBare])).toEqual([]);
  });

  it("lets `installed: true` win over a missing path", () => {
    // The flag is authoritative; a path is not required to be installed.
    const flagged: HostCli = { kind: "computer-use", version: "2.7.0", auth: "unknown", installed: true };
    expect(installedCli([flagged])).toEqual([flagged]);
    expect(absentCli([flagged])).toEqual([]);
  });

  it("covers all three states across the host fixtures", () => {
    const states = HOST_FIXTURES.map((host) => computerUseState(host.cli).reported);
    expect(states).toContain(true);
    expect(states).toContain(false);
    const installedHost = HOST_FIXTURES.find((host) => {
      const state = computerUseState(host.cli);
      return state.reported && state.installed;
    });
    const absentHost = HOST_FIXTURES.find((host) => {
      const state = computerUseState(host.cli);
      return state.reported && !state.installed;
    });
    expect(installedHost, "a fixture reports the client present").toBeTruthy();
    expect(absentHost, "a fixture reports it absent").toBeTruthy();
  });

  it("never turns the capability row into a launchable harness kind", () => {
    // The regression this pins: New Session reads a *non-empty* supported
    // list as "the host told us what it has" and stops falling back to
    // claude. If the capability row counted, a host with the vendor client
    // but no agent CLI on PATH would render every kind disabled.
    const capabilityOnly: HostCli[] = [
      { kind: COMPUTER_USE_KIND, version: "2.7.0", path: "/home/x/client", auth: "unknown", installed: true },
    ];

    expect(supportedHarnessKinds(capabilityOnly)).toEqual([]);
    // The other reported state is the same host shape for this purpose: a Node
    // that looked and found no client still must not contribute a kind.
    const capabilityAbsent: HostCli[] = [
      { kind: COMPUTER_USE_KIND, auth: "unknown", installed: false },
    ];
    expect(supportedHarnessKinds(capabilityAbsent)).toEqual([]);
    expect(supportedHarnessKinds(HOST_FIXTURES.flatMap((host) => host.cli)))
      .not.toContain(COMPUTER_USE_KIND);

    // The two states a real host actually presents, end to end.
    const mixed: HostCli[] = [
      ...capabilityOnly,
      { kind: "claude", version: "2.1.268", path: "/usr/bin/claude", auth: "logged_in" },
    ];
    expect(supportedHarnessKinds(mixed)).toEqual(["claude"]);
    expect(supportedHarnessKinds([])).toEqual([]);
    expect(supportedHarnessKinds(undefined)).toEqual([]);
  });
});
