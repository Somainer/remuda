import { describe, expect, it } from "vitest";
import { PROVIDER_PROFILES } from "./fixtures";
import {
  DELEGATION_COPY,
  defaultGatewayProfile,
  formatSecret,
  fromHub,
  healthLine,
  parseModels,
  redactSecretRef,
  shouldAvoidUnhealthy,
} from "./model";

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

  it("shows last4 not the token", () => {
    expect(formatSecret({ present: true, last4: "34ef", fingerprint: "aa" })).toBe("••••34ef");
    expect(formatSecret({ present: false, last4: null, fingerprint: null })).toBe("—");
    expect(redactSecretRef("sk-ab12cd34ef")).toBe("••••34ef");
    expect(redactSecretRef(null)).toBe("—");
  });

  it("formats health and flags unhealthy gateway for new sessions", () => {
    const gateway = PROVIDER_PROFILES.find((p) => p.profileId === "gateway")!;
    expect(healthLine(gateway.health)).toBe("健康 200  12ms");
    expect(shouldAvoidUnhealthy(gateway)).toBe(false);
    expect(shouldAvoidUnhealthy({ ...gateway, health: { ok: false, status: 503, message: "unreachable: connection refused" } })).toBe(true);
    expect(healthLine(null)).toBe("无 HTTP 探测");
    expect(healthLine({ ok: false, message: "unreachable: connection refused" })).toBe("unreachable: connection refused");
  });

  it("picks the default gateway and maps hub rows without a token field", () => {
    expect(defaultGatewayProfile(PROVIDER_PROFILES)?.name).toBe("示例网关");
    const mapped = fromHub({
      id: "pvp_01993ab0-0000-7000-8000-000000000010",
      name: "mine",
      kind: "gateway",
      baseUrl: "https://gw.example/v1",
      models: ["m"],
      defaultModel: "m",
      defaultGateway: true,
      secret: { present: true, last4: "t0k1", fingerprint: "0123456789abcdef" },
    });
    expect(JSON.stringify(mapped)).not.toMatch(/authToken|sk-/);
    expect(mapped.secret.last4).toBe("t0k1");
    expect(parseModels("a, b\nc")).toEqual(["a", "b", "c"]);
  });
});
