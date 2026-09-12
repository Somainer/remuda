export type Delegation = "none" | "gateway" | "direct";

export type ProviderHealth = {
  ok: boolean;
  status?: number;
  latencyMs?: number;
  checkedAt?: string;
};

export type ProviderProfile = {
  profileId: string;
  delegation: Delegation;
  protocol: string;
  baseUrl: string | null;
  health: ProviderHealth | null;
  secretRef: string | null;
  models: string[];
  lastError: string | null;
  rotationOwner: "native" | "gateway" | "runtime";
  available: boolean;
};

export const DELEGATION_COPY: Record<Delegation, { title: string; hint: string }> = {
  none: {
    title: "原生登录态",
    hint: "默认。各 CLI 自带鉴权（Claude 订阅 / Codex ChatGPT / grok xAI）。runtime 不持凭据。",
  },
  gateway: {
    title: "网关",
    hint: "任意 Anthropic-Messages 兼容 endpoint。轮换发生在网关，不在 runtime。无 Artifact / Remote Control。",
  },
  direct: {
    title: "直连",
    hint: "直连上游、多 key 轮换是 v2，本页不实现。",
  },
};

export function redactSecretRef(value: string | null): string {
  if (!value) return "—";
  const prefix = value.slice(0, 4);
  return `${prefix}****`;
}

export function healthLine(health: ProviderHealth | null): string {
  if (!health) return "无 HTTP 探测";
  if (!health.ok) return "不健康";
  const status = health.status ?? 200;
  const ms = health.latencyMs != null ? `  ${health.latencyMs}ms` : "";
  return `健康 ${status}${ms}`;
}

export function shouldAvoidUnhealthy(profile: ProviderProfile): boolean {
  return profile.delegation === "gateway" && profile.health != null && !profile.health.ok;
}
