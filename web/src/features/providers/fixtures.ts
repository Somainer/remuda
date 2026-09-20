import { NATIVE_PROFILE, type ProviderProfile } from "./model";

/** Synthetic proxy host the `gateway-via` fixture routes through (D-047). */
export const FIXTURE_VIA_HOST_ID = "hst_fixture_via_00000000000000000000aa";
export const FIXTURE_VIA_HOST_LABEL = "fixture-mac-relay";

/** Mock ProviderProfile rows. Wire ids are none | gateway | direct (D-012). */
export const PROVIDER_PROFILES: ProviderProfile[] = [
  NATIVE_PROFILE,
  {
    id: "gateway",
    profileId: "gateway",
    name: "示例网关",
    delegation: "gateway",
    kind: "gateway",
    protocol: "anthropic-messages",
    baseUrl: "https://gateway.example/v1",
    health: { ok: true, status: 200, latencyMs: 12, checkedAt: "2026-09-12T00:00:00.000Z" },
    secret: { present: true, last4: "34ef", fingerprint: "0123456789abcdef" },
    secretRef: "34ef",
    models: [
      { id: "passthrough/auto", enabled: true, label: "Auto", contextWindow: 1_048_576, tags: ["1m"] },
      { id: "passthrough/auto_model", enabled: true, label: "Auto model" },
    ],
    defaultModel: "passthrough/auto",
    defaultGateway: true,
    scope: "universal",
    headers: {},
    lastError: null,
    rotationOwner: "gateway",
    available: true,
  },
  {
    // D-047: a gateway profile whose sessions egress on a named proxy host
    // over the in-band hub-relay route.
    id: "gateway-via",
    profileId: "gateway-via",
    name: "经主机网关",
    delegation: "gateway",
    kind: "gateway",
    protocol: "anthropic-messages",
    baseUrl: "https://relay.example/v1",
    health: { ok: true, status: 200, latencyMs: 18, checkedAt: "2026-09-19T00:00:00.000Z" },
    secret: { present: true, last4: "0001", fingerprint: "abcdef0123456789" },
    secretRef: "0001",
    models: [{ id: "passthrough/relay/auto", enabled: true, label: "Relay Auto" }],
    defaultModel: "passthrough/relay/auto",
    defaultGateway: false,
    scope: "universal",
    headers: {},
    lastError: null,
    rotationOwner: "gateway",
    available: true,
    delivery: { mode: "via", viaHostId: FIXTURE_VIA_HOST_ID, route: "hub-relay" },
  },
  {
    id: "direct",
    profileId: "direct",
    name: "Direct 多 key",
    delegation: "direct",
    kind: "direct",
    protocol: "provider-native",
    baseUrl: null,
    health: null,
    secret: { present: false, last4: null, fingerprint: null },
    secretRef: null,
    models: [],
    defaultModel: null,
    defaultGateway: false,
    scope: "universal",
    headers: {},
    lastError: null,
    rotationOwner: "runtime",
    available: false,
  },
];

export function profileById(id: string | undefined): ProviderProfile | undefined {
  return PROVIDER_PROFILES.find((p) => p.profileId === id || p.id === id);
}
