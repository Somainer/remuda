/** New-session permission and provider/delegation (decisions.md D-011 / D-012). */

export const TUI_OPTIONS = [
  { id: "fullscreen", label: "全屏渲染（推荐）" },
  { id: "default", label: "行内渲染" },
] as const;

export const TUI_LAUNCH_HINT = "启动时使用此渲染方式，会话内可用 /tui 切换";

export const PERMISSION_OPTIONS = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "dontAsk", label: "全自动" },
  { id: "bypassPermissions", label: "绕过全部" },
] as const;

export type PermissionModeId = (typeof PERMISSION_OPTIONS)[number]["id"];

export const DELEGATION_OPTIONS = [
  { id: "host", label: "跟随主机" },
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

/** Short chip label for the read-only pty permission control. */
export function ptyYoloChipLabel(kind: string): string {
  if (kind === "grok") return "always-approve";
  if (kind === "codex") return "bypass";
  if (kind === "agy") return "bypass";
  return "skip-permissions";
}

export function normalizePermissionMode(value: string | undefined): PermissionModeId {
  if (value === "bypassPermissions") return "bypassPermissions";
  if (value === "dontAsk") return "dontAsk";
  if (value === "acceptEdits") return "acceptEdits";
  return "manual";
}

export function normalizeDelegation(value: string | undefined): DelegationId {
  if (value === "gateway") return "gateway";
  if (value === "none") return "none";
  return "host";
}

/** Profile id sent on create. Omitted when following the host binding. */
export function providerProfileForDelegation(delegation: DelegationId, defaultGatewayId?: string): string | undefined {
  if (delegation === "host") return undefined;
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

export type ProviderHintProfile = {
  id: string;
  name: string;
  scope: string;
  defaultGateway: boolean;
  kind?: string;
};

/** Hub-side waterfall preview for New Session. */
export function providerLaunchHint(input: {
  kind: string;
  binding?: string;
  cli?: Array<{ kind?: string; auth?: string; nativeGateway?: boolean }>;
  profiles: ProviderHintProfile[];
  delegation: DelegationId;
  explicitProfileId?: string;
}): string | null {
  if (input.kind !== "claude") return null;
  const realId = input.explicitProfileId?.trim();
  if (
    realId &&
    !["none", "native", "native-login", "gateway", "direct", "auto", "host"].includes(realId)
  ) {
    const profile = input.profiles.find((p) => p.id === realId);
    return profileHint(profile) ?? `将使用 ${realId}`;
  }
  if (input.delegation === "none") return "使用主机原生登录";
  const skipNative = input.delegation === "gateway";
  if (!skipNative) {
    const binding = (input.binding ?? "auto").trim();
    if (binding === "native") return "使用主机原生登录";
    if (binding.startsWith("profile:")) {
      const id = binding.slice("profile:".length);
      const profile = input.profiles.find((p) => p.id === id);
      return profileHint(profile) ?? `将使用 ${id}`;
    }
  }
  const hostScoped = input.profiles.find((p) => p.defaultGateway && p.scope.startsWith("host:"));
  if (hostScoped) return profileHint(hostScoped);
  const universal = input.profiles.find((p) => p.defaultGateway && (p.scope === "universal" || !p.scope));
  if (universal) return profileHint(universal);
  if (!skipNative && (claudeHostAuth(input.cli) === "logged_in" || claudeHostAuth(input.cli) === "gateway-native")) {
    return "使用主机原生登录";
  }
  return claudeProviderHint(input.kind, input.cli);
}

function profileHint(profile: ProviderHintProfile | undefined): string | null {
  if (!profile) return null;
  const label = profile.scope.startsWith("host:") ? "host" : "universal";
  return `将使用 ${profile.name} (${label})`;
}
