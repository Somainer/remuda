/** New-session permission and provider/delegation (decisions.md D-011 / D-012). */

export const PERMISSION_OPTIONS = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "bypassPermissions", label: "全自动" },
] as const;

export type PermissionModeId = (typeof PERMISSION_OPTIONS)[number]["id"];

export const DELEGATION_OPTIONS = [
  { id: "none", label: "原生登录态 (none)" },
  { id: "gateway", label: "网关 (gateway)" },
] as const;

export type DelegationId = (typeof DELEGATION_OPTIONS)[number]["id"];

export const YOLO_HINT =
  "全自动（yolo）会跳过工具批准并允许危险权限（bypassPermissions）。仅限你本人发起的会话；不要用于 bot，也不要在生产目录上用。";

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
  if (value === "bypassPermissions" || value === "dontAsk") return "bypassPermissions";
  if (value === "acceptEdits") return "acceptEdits";
  return "manual";
}

export function normalizeDelegation(value: string | undefined): DelegationId {
  return value === "gateway" ? "gateway" : "none";
}

/** Profile id sent on create. Wire uses none | gateway; never an astergate-specific name. */
export function providerProfileForDelegation(delegation: DelegationId): "none" | "gateway" {
  return delegation;
}
