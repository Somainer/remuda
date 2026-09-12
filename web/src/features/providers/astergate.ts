export const ASTERGATE_DEFAULT = {
  profileId: "astergate-default",
  protocol: "anthropic-messages",
  baseUrl: "https://astergate.example/v1",
  health: { ok: true, latencyMs: 12, checkedAt: "2026-09-12T00:00:00.000Z" },
  secretRef: "sk-a***",
  models: ["passthrough/auto", "passthrough/auto_model"],
  lastError: null as string | null,
};
