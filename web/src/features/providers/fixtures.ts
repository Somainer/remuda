import { NATIVE_PROFILE, type ProviderProfile } from "./model";

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
