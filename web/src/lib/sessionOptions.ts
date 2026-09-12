/** New-session permission and provider/delegation (decisions.md D-011 / D-012). */

export const PERMISSION_OPTIONS = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "dontAsk", label: "全自动" },
  { id: "bypassPermissions", label: "绕过全部" },
] as const;

export type PermissionModeId = (typeof PERMISSION_OPTIONS)[number]["id"];

export const DELEGATION_OPTIONS = [
  { id: "none", label: "原生登录态 (none)" },
  { id: "gateway", label: "网关 (gateway)" },
] as const;

export type DelegationId = (typeof DELEGATION_OPTIONS)[number]["id"];

export const YOLO_HINT =
  "审批中心不会再出现这台实例的条目，手机也收不到推送 —— 你只能靠 transcript 事后看它做了什么。仅建议用在一次性沙箱 cwd。";

export const YOLO_ACK = "我明白，开始时再确认一次";

/** Node `generic-pty` yolo argv applied server-side per kind. */
export const PTY_YOLO_FLAGS: Record<"claude" | "codex" | "grok" | "agy", string> = {
  claude: "--dangerously-skip-permissions",
  codex: "--dangerously-bypass-approvals-and-sandbox",
  grok: "--always-approve",
  agy: "--dangerously-skip-permissions",
};

export function ptyYoloHint(kind: keyof typeof PTY_YOLO_FLAGS): string {
  return `generic-pty · Node applies ${PTY_YOLO_FLAGS[kind]} (server-side yolo preset)`;
}

export function normalizePermissionMode(value: string | undefined): PermissionModeId {
  if (value === "bypassPermissions") return "bypassPermissions";
  if (value === "dontAsk") return "dontAsk";
  if (value === "acceptEdits") return "acceptEdits";
  return "manual";
}

export function normalizeDelegation(value: string | undefined): DelegationId {
  return value === "gateway" ? "gateway" : "none";
}

/** Profile id sent on create. Wire uses none | gateway; never an astergate-specific name. */
export function providerProfileForDelegation(delegation: DelegationId, defaultGatewayId?: string): string {
  if (delegation === "gateway") return defaultGatewayId || "gateway";
  return "none";
}

export type ClaudeHostAuth = "gateway-native" | "logged_in" | "none";

export function claudeHostAuth(
  cli: Array<{ kind?: string; auth?: string; nativeGateway?: boolean }> | undefined,
): ClaudeHostAuth {
  const entry = (cli ?? []).find((item) => item.kind === "claude");
  if (!entry) return "none";
  if (entry.nativeGateway || entry.auth === "gateway-native") return "gateway-native";
  if (entry.auth === "logged_in") return "logged_in";
  return "none";
}

/** Informational New Session hint. Does not change Provider defaults. */
export function claudeProviderHint(
  kind: string,
  cli: Array<{ kind?: string; auth?: string; nativeGateway?: boolean }> | undefined,
): string | null {
  if (kind !== "claude") return null;
  const auth = claudeHostAuth(cli);
  if (auth === "gateway-native" || auth === "logged_in") return null;
  return "此主机未配置 Claude 登录/网关，请选择 Provider";
}
