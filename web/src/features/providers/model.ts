export type Delegation = "none" | "gateway" | "direct";

export type ProviderHealth = {
  ok: boolean;
  status?: number | null;
  latencyMs?: number | null;
  checkedAt?: string | null;
  message?: string | null;
};

export type ProviderSecretView = {
  present: boolean;
  last4: string | null;
  fingerprint: string | null;
};

export type ProviderProfile = {
  id: string;
  profileId: string;
  name: string;
  delegation: Delegation;
  kind: "native" | "gateway" | "direct";
  protocol: string;
  baseUrl: string | null;
  health: ProviderHealth | null;
  secret: ProviderSecretView;
  secretRef: string | null;
  models: string[];
  defaultModel: string | null;
  defaultGateway: boolean;
  /** `universal` or `host:<hostId>`. */
  scope: string;
  headers: Record<string, string>;
  lastError: string | null;
  rotationOwner: "native" | "gateway" | "runtime";
  available: boolean;
  revision?: string;
  createdAt?: string;
  updatedAt?: string;
};

export type ProviderCreate = {
  name: string;
  kind: "gateway" | "direct";
  baseUrl: string;
  models: string[];
  defaultModel?: string;
  headers?: Record<string, string>;
  authToken: string;
  defaultGateway?: boolean;
  scope?: string;
};

export type ProviderPatch = {
  name?: string;
  kind?: "gateway" | "direct";
  baseUrl?: string;
  models?: string[];
  defaultModel?: string | null;
  headers?: Record<string, string>;
  authToken?: string;
  defaultGateway?: boolean;
  scope?: string;
};

export type ProviderTestResult = {
  ok: boolean;
  reachable: boolean;
  status?: number | null;
  latencyMs?: number;
  message: string;
  models?: string[];
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
    hint: "直连上游。单 key 可配置；多 key 轮换是 v2。",
  },
};

export const NATIVE_PROFILE: ProviderProfile = {
  id: "none",
  profileId: "none",
  name: "原生登录态",
  delegation: "none",
  kind: "native",
  protocol: "native-cli",
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
  rotationOwner: "native",
  available: true,
};

export function formatSecret(secret: ProviderSecretView): string {
  if (!secret.present) return "—";
  if (secret.last4) return `••••${secret.last4}`;
  return "已保存";
}

/** @deprecated use formatSecret; kept for existing tests that pass a raw prefix. */
export function redactSecretRef(value: string | null): string {
  if (!value) return "—";
  if (value.length <= 4) return `••••${value}`;
  return `••••${value.slice(-4)}`;
}

export function healthLine(health: ProviderHealth | null): string {
  if (!health) return "无 HTTP 探测";
  if (!health.ok) return health.message?.trim() || "不健康";
  const status = health.status ?? 200;
  const ms = health.latencyMs != null ? `  ${health.latencyMs}ms` : "";
  return `健康 ${status}${ms}`;
}

export function shouldAvoidUnhealthy(profile: ProviderProfile): boolean {
  return profile.delegation === "gateway" && profile.health != null && !profile.health.ok;
}

export function defaultGatewayProfile(profiles: ProviderProfile[]): ProviderProfile | undefined {
  return profiles.find((p) => p.delegation === "gateway" && p.defaultGateway && p.available)
    ?? profiles.find((p) => p.delegation === "gateway" && p.available);
}

export function parseModels(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

type HubProvider = {
  id: string;
  name: string;
  kind: "gateway" | "direct" | string;
  baseUrl?: string | null;
  models?: string[];
  defaultModel?: string | null;
  headers?: Record<string, string>;
  defaultGateway?: boolean;
  scope?: string;
  revision?: string;
  secret?: { present?: boolean; last4?: string | null; fingerprint?: string | null };
  health?: ProviderHealth | null;
  createdAt?: string;
  updatedAt?: string;
};

export function fromHub(row: HubProvider): ProviderProfile {
  const kind = row.kind === "direct" ? "direct" : "gateway";
  const last4 = row.secret?.last4 ?? null;
  return {
    id: row.id,
    profileId: row.id,
    name: row.name,
    delegation: kind,
    kind,
    protocol: kind === "direct" ? "provider-native" : "anthropic-messages",
    baseUrl: row.baseUrl ?? null,
    health: row.health ?? null,
    secret: {
      present: Boolean(row.secret?.present),
      last4,
      fingerprint: row.secret?.fingerprint ?? null,
    },
    secretRef: last4,
    models: row.models ?? [],
    defaultModel: row.defaultModel ?? null,
    defaultGateway: Boolean(row.defaultGateway),
    scope: row.scope && row.scope.length ? row.scope : "universal",
    headers: row.headers ?? {},
    lastError: row.health && !row.health.ok ? row.health.message ?? null : null,
    rotationOwner: kind === "direct" ? "runtime" : "gateway",
    available: true,
    revision: row.revision,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}
