import { describe, expect, it } from "vitest";
import { PROVIDER_PROFILES } from "./fixtures";
import { DELEGATION_COPY, healthLine, redactSecretRef, shouldAvoidUnhealthy } from "./model";

describe("provider profiles D-012", () => {
  it("defaults to native none and exposes gateway + disabled direct", () => {
    expect(PROVIDER_PROFILES.map((p) => p.delegation)).toEqual(["none", "gateway", "direct"]);
    expect(PROVIDER_PROFILES[0]?.profileId).toBe("none");
    expect(PROVIDER_PROFILES.find((p) => p.delegation === "direct")?.available).toBe(false);
  });

  it("never names a vendor gateway in copy or ids", () => {
    const blob = JSON.stringify({ profiles: PROVIDER_PROFILES, copy: DELEGATION_COPY });
    expect(blob.toLowerCase()).not.toMatch(/astergate/);
  });

  it("redacts secret refs to a 4-char prefix", () => {
    expect(redactSecretRef("sk-ab12cd34ef")).toBe("sk-a****");
    expect(redactSecretRef(null)).toBe("—");
  });

  it("formats health and flags unhealthy gateway for new sessions", () => {
    const gateway = PROVIDER_PROFILES.find((p) => p.profileId === "gateway")!;
    expect(healthLine(gateway.health)).toBe("健康 200  12ms");
    expect(shouldAvoidUnhealthy(gateway)).toBe(false);
    expect(shouldAvoidUnhealthy({ ...gateway, health: { ok: false, status: 503 } })).toBe(true);
    expect(healthLine(null)).toBe("无 HTTP 探测");
  });
});
