import { describe, expect, it } from "vitest";
import { PROVIDER_PROFILES } from "./fixtures";
import {
  DELEGATION_COPY,
  contextChip,
  deliveryClause,
  deliveryHostOffline,
  effectiveDelivery,
  defaultGatewayProfile,
  enabledModels,
  filterModels,
  formatSecret,
  fromHub,
  groupModels,
  healthLine,
  invertEnabled,
  mergeDiscovered,
  modelGroupKey,
  nextDefaultModel,
  normalizeModels,
  parseModels,
  redactSecretRef,
  resolveGatewayModel,
  setEnabled,
  shouldAvoidUnhealthy,
  splitModelId,
  triState,
} from "./model";

describe("provider profiles D-012", () => {
  it("defaults to native none and exposes gateway + disabled direct", () => {
    // native, the default gateway, the D-047 via-host gateway, and the v2
    // direct placeholder.
    expect(PROVIDER_PROFILES.map((p) => p.delegation)).toEqual([
      "none",
      "gateway",
      "gateway",
      "direct",
    ]);
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

/**
 * New Session must offer exactly the enabled catalog. Both wire shapes reach
 * the picker: the structured entries the Hub stores today and the legacy bare
 * id list a not-yet-rewritten row still serves.
 */
describe("resolveGatewayModel", () => {
  const structured = normalizeModels([
    { id: "e2e/auto", enabled: true, label: "E2E Auto" },
    { id: "e2e/fast", enabled: true },
    { id: "e2e/plain", enabled: false },
  ]);
  const legacy = normalizeModels(["e2e/auto", "e2e/fast"]);

  it("keeps a current model the catalog still exposes", () => {
    expect(resolveGatewayModel(structured, "e2e/auto", "e2e/fast")).toBe("e2e/fast");
    expect(resolveGatewayModel(legacy, "e2e/auto", "e2e/fast")).toBe("e2e/fast");
  });

  it("replaces a disabled model with the profile default", () => {
    expect(resolveGatewayModel(structured, "e2e/auto", "e2e/plain")).toBe("e2e/auto");
  });

  it("replaces a model absent from the catalog with the profile default", () => {
    expect(resolveGatewayModel(structured, "e2e/auto", "passthrough/auto")).toBe("e2e/auto");
    expect(resolveGatewayModel(legacy, "e2e/auto", "passthrough/auto")).toBe("e2e/auto");
  });

  it("falls back to the first enabled model when the default is unusable", () => {
    // A default naming a disabled model must not be resurrected.
    expect(resolveGatewayModel(structured, "e2e/plain", "gone")).toBe("e2e/auto");
    expect(resolveGatewayModel(structured, null, "gone")).toBe("e2e/auto");
  });

  it("returns null when the profile exposes nothing, leaving the free-text box", () => {
    expect(resolveGatewayModel([], "e2e/auto", "gone")).toBeNull();
    expect(
      resolveGatewayModel(normalizeModels([{ id: "e2e/plain", enabled: false }]), null, "x"),
    ).toBeNull();
  });

  it("treats a legacy bare id list as fully enabled", () => {
    expect(enabledModels(legacy).map((m) => m.id)).toEqual(["e2e/auto", "e2e/fast"]);
  });
});

/**
 * astergate serves ~300 models across many prefixes, so the checklist and the
 * New Session picker both group and filter rather than rendering one long list.
 */
describe("catalog grouping and filtering", () => {
  const catalog = normalizeModels([
    { id: "passthrough/ark/seed-evolving", surfaces: ["openai"] },
    { id: "passthrough/auto", label: "Auto", surfaces: ["openai"] },
    { id: "cursor/gpt-5", surfaces: ["openai"] },
    { id: "gemini-2.5-pro", surfaces: ["openai"] },
    { id: "claude-opus-5", label: "Opus 5", surfaces: ["openai", "anthropic"] },
    { id: "claude-haiku-4-5", surfaces: ["anthropic"] },
    { id: "solo", surfaces: ["openai"] },
  ]);

  it("buckets an id by everything up to the first slash or dash", () => {
    expect(modelGroupKey("passthrough/ark/seed-evolving")).toBe("passthrough/");
    expect(modelGroupKey("cursor/gpt-5")).toBe("cursor/");
    expect(modelGroupKey("gemini-2.5-pro")).toBe("gemini-");
    expect(modelGroupKey("claude-opus-5")).toBe("claude-");
    // An id with neither separator is its own bucket rather than vanishing.
    expect(modelGroupKey("solo")).toBe("solo");
  });

  it("groups in first-seen order and keeps every model", () => {
    const groups = groupModels(catalog);
    expect(groups.map((g) => g.key)).toEqual([
      "passthrough/",
      "cursor/",
      "gemini-",
      "claude-",
      "solo",
    ]);
    expect(groups[0].models.map((m) => m.id)).toEqual([
      "passthrough/ark/seed-evolving",
      "passthrough/auto",
    ]);
    expect(groups.flatMap((g) => g.models)).toHaveLength(catalog.length);
  });

  it("filters on id or label, case-insensitively", () => {
    expect(filterModels(catalog, "claude").map((m) => m.id)).toEqual([
      "claude-opus-5",
      "claude-haiku-4-5",
    ]);
    // Matches the human label too, not just the wire id.
    expect(filterModels(catalog, "opus 5").map((m) => m.id)).toEqual(["claude-opus-5"]);
    expect(filterModels(catalog, "SEED").map((m) => m.id)).toEqual([
      "passthrough/ark/seed-evolving",
    ]);
    expect(filterModels(catalog, "  ")).toHaveLength(catalog.length);
    expect(filterModels(catalog, "nothing-matches")).toHaveLength(0);
  });

  it("carries the surfaces the Hub reported and survives a re-probe", () => {
    expect(catalog[4].surfaces).toEqual(["openai", "anthropic"]);
    // A manual id has no surface at all.
    expect(normalizeModels([{ id: "typed" }])[0].surfaces).toBeUndefined();
    // A re-probe restates surfaces rather than accumulating stale ones.
    const { models } = mergeDiscovered(
      [{ id: "claude-opus-5", enabled: false, surfaces: ["openai", "anthropic"] }],
      [{ id: "claude-opus-5", enabled: true, surfaces: ["anthropic"] }],
    );
    expect(models[0].surfaces).toEqual(["anthropic"]);
    expect(models[0].enabled).toBe(false);
  });
});

describe("bulk enable/disable math", () => {
  const catalog = normalizeModels([
    { id: "cursor/gpt-5", enabled: true },
    { id: "cursor/gpt-5-mini", enabled: false },
    { id: "cursor/sonic", enabled: false },
    { id: "openai/o3", enabled: true },
  ]);

  it("reads a group as all, some or none", () => {
    expect(triState(catalog.slice(0, 3))).toBe("some");
    expect(triState(catalog.slice(1, 3))).toBe("none");
    expect(triState([catalog[0], catalog[3]])).toBe("all");
    // An empty group is not "all"; a box for nothing must render unticked.
    expect(triState([])).toBe("none");
  });

  it("enables only the named ids and leaves the rest of the catalog alone", () => {
    const next = setEnabled(catalog, ["cursor/gpt-5-mini", "cursor/sonic"], true);
    expect(next.map((m) => m.enabled)).toEqual([true, true, true, true]);
    // Order is preserved, and untouched entries keep their identity so React
    // does not remount every row of a 300-model list.
    expect(next.map((m) => m.id)).toEqual(catalog.map((m) => m.id));
    expect(next[0]).toBe(catalog[0]);
    expect(next[3]).toBe(catalog[3]);
  });

  it("disables a group without touching a model outside it", () => {
    const cursor = catalog.filter((m) => m.id.startsWith("cursor/")).map((m) => m.id);
    const next = setEnabled(catalog, cursor, false);
    expect(next.filter((m) => m.enabled).map((m) => m.id)).toEqual(["openai/o3"]);
  });

  it("inverts exactly the ids it is given", () => {
    const next = invertEnabled(catalog, ["cursor/gpt-5", "cursor/sonic"]);
    expect(next.map((m) => [m.id, m.enabled])).toEqual([
      ["cursor/gpt-5", false],
      ["cursor/gpt-5-mini", false],
      ["cursor/sonic", true],
      ["openai/o3", true],
    ]);
  });

  it("select-all over a filtered view touches only the matching rows", () => {
    // What the header button does: filter first, then flip that id set.
    const shown = filterModels(catalog, "cursor");
    const next = setEnabled(catalog, shown.map((m) => m.id), true);
    expect(next.filter((m) => m.enabled)).toHaveLength(4);

    const off = setEnabled(catalog, filterModels(catalog, "sonic").map((m) => m.id), false);
    // Only sonic was in view, so gpt-5 and o3 keep their enabled choice.
    expect(off.filter((m) => m.enabled).map((m) => m.id)).toEqual(["cursor/gpt-5", "openai/o3"]);
  });
});

describe("nextDefaultModel", () => {
  const catalog = normalizeModels([
    { id: "a", enabled: false },
    { id: "b", enabled: true },
    { id: "c", enabled: true },
  ]);

  it("keeps a default the catalog still offers", () => {
    expect(nextDefaultModel(catalog, "c")).toBe("c");
  });

  it("moves to the first enabled model when the default is disabled", () => {
    expect(nextDefaultModel(catalog, "a")).toBe("b");
  });

  it("moves on when the default was removed from the catalog outright", () => {
    expect(nextDefaultModel(catalog, "gone")).toBe("b");
  });

  it("empties the default when a bulk disable leaves nothing enabled", () => {
    expect(nextDefaultModel(setEnabled(catalog, ["b", "c"], false), "b")).toBe("");
    expect(nextDefaultModel([], "b")).toBe("");
  });

  it("adopts the first enabled model when there is no default yet", () => {
    expect(nextDefaultModel(catalog, "")).toBe("b");
    expect(nextDefaultModel(catalog, null)).toBe("b");
  });
});

describe("splitModelId", () => {
  it("keeps the tail, where ids in one family actually differ", () => {
    const [head, tail] = splitModelId("passthrough/ark/seed-evolving-250918");
    expect(head + tail).toBe("passthrough/ark/seed-evolving-250918");
    expect(tail).toBe("ing-250918");
  });

  it("leaves a short id whole rather than splitting it for no gain", () => {
    expect(splitModelId("e2e/auto")).toEqual(["e2e/auto", ""]);
  });
});

describe("D-047 provider delivery", () => {
  const hosts = [
    { id: "hst_on", label: "mac-relay", online: true },
    { id: "hst_off", label: "sg-box", online: false },
  ];

  it("treats absent/partial delivery as the direct/auto default", () => {
    expect(effectiveDelivery(undefined)).toEqual({ mode: "direct", route: "auto" });
    expect(effectiveDelivery(null)).toEqual({ mode: "direct", route: "auto" });
    expect(effectiveDelivery({ mode: "via" } as never)).toEqual({
      mode: "via",
      route: "auto",
    });
  });

  it("renders the clause the Provider page prints", () => {
    expect(deliveryClause(undefined, hosts)).toBe("直连");
    expect(deliveryClause({ mode: "direct", route: "auto" }, hosts)).toBe("直连");
    expect(
      deliveryClause({ mode: "via", viaHostId: "hst_on", route: "hub-relay" }, hosts),
    ).toBe("经由 mac-relay · Hub 中转");
    expect(
      deliveryClause({ mode: "via", viaHostId: "hst_on", route: "direct-net" }, hosts),
    ).toBe("经由 mac-relay · 直连网络");
    // A host missing from the inventory shows its id instead of a guessed name.
    expect(deliveryClause({ mode: "via", viaHostId: "hst_x", route: "auto" }, hosts)).toBe(
      "经由 hst_x · 自动",
    );
  });

  it("warns offline only when the named host is known offline", () => {
    expect(
      deliveryHostOffline({ mode: "via", viaHostId: "hst_off", route: "auto" }, hosts),
    ).toBe(true);
    expect(
      deliveryHostOffline({ mode: "via", viaHostId: "hst_on", route: "auto" }, hosts),
    ).toBe(false);
    // Unknown host = Hub refusal territory, not the form's offline guess.
    expect(
      deliveryHostOffline({ mode: "via", viaHostId: "hst_x", route: "auto" }, hosts),
    ).toBe(false);
    expect(deliveryHostOffline({ mode: "direct", route: "auto" }, hosts)).toBe(false);
  });

  it("maps the Hub row's delivery through fromHub", () => {
    const mapped = fromHub({
      id: "pvp_1",
      name: "relay",
      kind: "gateway",
      baseUrl: "http://127.0.0.1:1/v1",
      delivery: { mode: "via", viaHostId: "hst_on", route: "hub-relay" },
    });
    expect(mapped.delivery).toEqual({
      mode: "via",
      viaHostId: "hst_on",
      route: "hub-relay",
    });
  });
});
