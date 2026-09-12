import type { ProviderProfile } from "./model";

/** Mock ProviderProfile rows. Wire ids are none | gateway | direct (D-012). */
export const PROVIDER_PROFILES: ProviderProfile[] = [
  {
    profileId: "none",
    delegation: "none",
    protocol: "native-cli",
    baseUrl: null,
    health: null,
    secretRef: null,
    models: [],
    lastError: null,
    rotationOwner: "native",
    available: true,
  },
  {
    profileId: "gateway",
    delegation: "gateway",
    protocol: "anthropic-messages",
    baseUrl: "https://gateway.example/v1",
    health: { ok: true, status: 200, latencyMs: 12, checkedAt: "2026-09-12T00:00:00.000Z" },
    secretRef: "sk-ab12cd34ef",
    models: ["passthrough/auto", "passthrough/auto_model"],
    lastError: null,
    rotationOwner: "gateway",
    available: true,
  },
  {
    profileId: "direct",
    delegation: "direct",
    protocol: "provider-native",
    baseUrl: null,
    health: null,
    secretRef: null,
    models: [],
    lastError: null,
    rotationOwner: "runtime",
    available: false,
  },
];

export function profileById(id: string | undefined): ProviderProfile | undefined {
  return PROVIDER_PROFILES.find((p) => p.profileId === id);
}
