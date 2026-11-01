import { describe, expect, it } from "vitest";
import { hostStrip, offlineAge, type HostStripHost } from "./hostStrip";

/**
 * ② 主机条 (home.md §1 ②): offline ordering, the 「离线 Nm」 age and the
 * all-online collapse. Rendering is covered by the hub spec.
 */

const NOW = Date.parse("2026-10-31T12:00:00.000Z");

function host(overrides: Partial<HostStripHost> & { id: string }): HostStripHost {
  return {
    label: overrides.id,
    state: "online",
    online: true,
    ...overrides,
  } as HostStripHost;
}

function ago(ms: number): string {
  return new Date(NOW - ms).toISOString();
}

describe("offlineAge", () => {
  it("spells minutes, hours and days", () => {
    expect(offlineAge(ago(20_000), NOW)).toBe("刚刚");
    expect(offlineAge(ago(2 * 60_000), NOW)).toBe("2m");
    expect(offlineAge(ago(59 * 60_000 + 30_000), NOW)).toBe("59m");
    expect(offlineAge(ago(2 * 3_600_000), NOW)).toBe("2h");
    expect(offlineAge(ago(3 * 86_400_000), NOW)).toBe("3d");
  });

  it("is null when there is no usable heartbeat", () => {
    expect(offlineAge(undefined, NOW)).toBeNull();
    expect(offlineAge("not-a-date", NOW)).toBeNull();
  });
});

describe("hostStrip", () => {
  it("puts offline hosts first, longest away first, and gives them an age", () => {
    const model = hostStrip(
      [
        host({ id: "forge", label: "forge" }),
        host({ id: "laptop", label: "laptop", online: false, state: "offline", lastSeenAt: ago(2 * 3_600_000) }),
        host({ id: "studio", label: "studio" }),
        host({ id: "nas", label: "nas", online: false, state: "offline", lastSeenAt: ago(5 * 60_000) }),
      ],
      NOW,
    );
    expect(model.collapsed).toBe(false);
    expect(model.total).toBe(4);
    expect(model.online).toBe(2);
    expect(model.chips.map((chip) => [chip.label, chip.offline, chip.age])).toEqual([
      ["laptop", true, "2h"],
      ["nas", true, "5m"],
      ["forge", false, null],
      ["studio", false, null],
    ]);
  });

  it("treats a host without an online flag by its state", () => {
    const model = hostStrip(
      [
        host({ id: "a", label: "a", online: undefined, state: "enrolled" }),
        host({ id: "b", label: "b", online: undefined, state: "offline", lastSeenAt: ago(60_000) }),
        host({ id: "c", label: "c", online: undefined, state: "retired", lastSeenAt: ago(86_400_000) }),
      ],
      NOW,
    );
    expect(model.chips.map((chip) => chip.label)).toEqual(["c", "b", "a"]);
    expect(model.chips.map((chip) => chip.age)).toEqual(["1d", "1m", null]);
  });

  it("collapses to the summary when every host is online", () => {
    const model = hostStrip(
      [host({ id: "a", label: "a" }), host({ id: "b", label: "b" }), host({ id: "c", label: "c" })],
      NOW,
    );
    expect(model).toMatchObject({ total: 3, online: 3, collapsed: true, chips: [] });
  });

  it("does not collapse an empty list: the strip offers 添加主机 instead", () => {
    expect(hostStrip([], NOW)).toEqual({ total: 0, online: 0, collapsed: false, chips: [] });
  });

  it("collapses a single-host strip too (one chip, not one row)", () => {
    const model = hostStrip([host({ id: "a" })], NOW);
    expect(model.collapsed).toBe(true);
    expect(model.online).toBe(1);
  });
});
