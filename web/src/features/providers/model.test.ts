import { describe, expect, it } from "vitest";
import { PROVIDER_PROFILES } from "./fixtures";
import {
  DELEGATION_COPY,
  contextChip,
  defaultGatewayProfile,
  enabledModels,
  formatSecret,
  fromHub,
  healthLine,
  mergeDiscovered,
  normalizeModels,
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
      models: [{ id: "m", enabled: true }],
      defaultModel: "m",
      defaultGateway: true,
      secret: { present: true, last4: "t0k1", fingerprint: "0123456789abcdef" },
    });
    expect(JSON.stringify(mapped)).not.toMatch(/authToken|sk-/);
    expect(mapped.secret.last4).toBe("t0k1");
    expect(parseModels("a, b\nc")).toEqual([
      { id: "a", enabled: true },
      { id: "b", enabled: true },
      { id: "c", enabled: true },
    ]);
  });
});

describe("structured model catalog", () => {
  it("migrates a legacy string list from the Hub into enabled entries", () => {
    const mapped = fromHub({
      id: "pvp_legacy",
      name: "legacy",
      kind: "gateway",
      baseUrl: "https://gw.example/v1",
      models: ["passthrough/auto", "passthrough/auto_model"],
      defaultGateway: false,
    });
    expect(mapped.models).toEqual([
      { id: "passthrough/auto", enabled: true },
      { id: "passthrough/auto_model", enabled: true },
    ]);
    expect(enabledModels(mapped.models)).toHaveLength(2);
  });

  it("keeps metadata, drops blanks and duplicates, and honours enabled=false", () => {
    expect(
      normalizeModels([
        { id: " gw/a ", enabled: false },
        { id: "gw/a" },
        { id: "", enabled: true },
        { id: "gw/b", label: "B", contextWindow: 1_048_576, tags: ["1m"] },
      ]),
    ).toEqual([
      { id: "gw/a", enabled: false },
      { id: "gw/b", enabled: true, label: "B", contextWindow: 1_048_576, tags: ["1m"] },
    ]);
    expect(normalizeModels(undefined)).toEqual([]);
  });

  it("merges discovery: keeps choices, adds metadata, flags new ids, keeps manual ones", () => {
    const current = [
      { id: "gw/keep", enabled: false },
      { id: "gw/manual", enabled: true },
    ];
    const discovered = [
      { id: "gw/keep", enabled: true, label: "Keep", contextWindow: 200_000 },
      { id: "gw/fresh", enabled: true, label: "Fresh" },
    ];
    const { models, added } = mergeDiscovered(current, discovered);
    // A saved model keeps the operator's enabled choice but gains metadata.
    expect(models[0]).toEqual({
      id: "gw/keep",
      enabled: false,
      label: "Keep",
      contextWindow: 200_000,
    });
    // A manual id the gateway does not list survives the merge.
    expect(models[1]).toEqual({ id: "gw/manual", enabled: true });
    expect(models[2]).toEqual({ id: "gw/fresh", enabled: true, label: "Fresh" });
    expect(added).toEqual(["gw/fresh"]);
  });

  it("formats context windows as chips", () => {
    expect(contextChip(1_048_576)).toBe("1m");
    expect(contextChip(2_000_000)).toBe("2m");
    expect(contextChip(200_000)).toBe("200k");
    expect(contextChip(512)).toBe("512");
    expect(contextChip(null)).toBeNull();
    expect(contextChip(0)).toBeNull();
  });
});
