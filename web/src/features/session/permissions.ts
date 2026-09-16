/**
 * Per-harness permission-mode tables — the real native vocabulary, never a
 * generic flat list.
 *
 * Claude (measured on claude 2.1.273, see
 * docs/design/evidence/permission-modes-1.md):
 *
 * - the six `--permission-mode` values are manual(default) · acceptEdits ·
 *   plan · auto · bypassPermissions · dontAsk;
 * - shift+tab in a live session only walks manual → acceptEdits → plan →
 *   auto → manual; bypassPermissions joins the wheel only when the launch
 *   carried the bypass allowance, and dontAsk never joins (one press exits
 *   it). So {@link runtimePermissionTable} excludes dontAsk and marks bypass
 *   conditionally unreachable; {@link launchPermissionTable} offers all six.
 *
 * Other harnesses keep their own native sets: Codex is an approval policy
 * plus a sandbox mode, Grok and agy have their three/four native modes.
 */
import type { Kind } from "../../types/instance";

export type PermissionKind = Extract<
  Kind,
  "claude" | "codex" | "grok" | "agy" | "terminal" | "generic"
>;

export type PermissionOption = {
  /** Wire word sent in `instance.configure` / create `permissionMode`. */
  id: string;
  /** Honest Chinese primary label. */
  label: string;
  /** The harness's own secondary name (`acceptEdits`, `on-request`, …). */
  native: string;
  /** One-line description of what the mode really does. */
  description: string;
  /**
   * Cannot be selected in a live session (launch argv only). Greyed with a
   * 仅启动时 tag in the composer menu.
   */
  launchOnly?: boolean;
  /** Danger treatment for yolo-class modes. */
  danger?: boolean;
};

/**
 * Claude's six real modes, in the order the CLI help lists them. Chinese
 * primaries are honest about behavior; the English native word rides as the
 * secondary label so the picker matches the TUI status line and `/permissions`.
 */
const CLAUDE_PERMS: PermissionOption[] = [
  {
    id: "manual",
    label: "询问",
    native: "default",
    description: "危险操作逐项询问 · shift+tab 轮盘的起点（默认）",
  },
  {
    id: "acceptEdits",
    label: "可改文件",
    native: "acceptEdits",
    description: "自动批准文件编辑与常见文件命令",
  },
  {
    id: "plan",
    label: "计划",
    native: "plan",
    description: "只研究与提方案，不做任何改动",
  },
  {
    id: "auto",
    label: "自动判断",
    native: "auto",
    description: "无例行弹窗 · 审查模型先筛一遍动作（模型门控关闭时不可切）",
  },
  {
    id: "bypassPermissions",
    label: "绕过全部",
    native: "bypassPermissions",
    description: "不再弹窗，动作全部放行 · 需启动时带 bypass 允许",
    danger: true,
  },
  {
    id: "dontAsk",
    label: "拒绝未授权",
    native: "dontAsk",
    description: "任何会触发询问的动作一律拒绝 · 非交互启动专用",
    launchOnly: true,
  },
];

/**
 * Codex's two real axes: an approval policy and a sandbox mode. A launch
 * selection is one of each; the wire ids are `<policy>` / `sandbox:<mode>`.
 */
const CODEX_POLICIES: PermissionOption[] = [
  { id: "untrusted", label: "逐次询问", native: "untrusted", description: "沙箱内执行，执行前等待批准（默认）" },
  { id: "on-request", label: "按需批准", native: "on-request", description: "仅在需要提权时询问" },
  { id: "never", label: "不再询问", native: "never", description: "自动批准，不再弹窗（yolo）", danger: true },
];

const CODEX_SANDBOX: PermissionOption[] = [
  { id: "sandbox:read-only", label: "只读沙箱", native: "read-only", description: "禁止写盘与命令执行" },
  { id: "sandbox:workspace-write", label: "工作区可写", native: "workspace-write", description: "可写工作区，网络受限（默认）" },
  { id: "sandbox:danger-full-access", label: "完全访问", native: "danger-full-access", description: "无沙箱 · 可写任意位置并联网", danger: true },
];

/** Grok's native permission modes. */
const GROK_PERMS: PermissionOption[] = [
  { id: "native-prompt", label: "逐项询问", native: "native-prompt", description: "每个动作按原生提示询问（默认）" },
  { id: "auto", label: "自动", native: "auto", description: "自动批准安全动作" },
  { id: "always-approve", label: "全部批准", native: "always-approve", description: "不再弹窗，动作全部放行（--always-approve）", danger: true },
];

/** agy's native permission modes. */
const AGY_PERMS: PermissionOption[] = [
  { id: "native", label: "原生询问", native: "native", description: "按 agy 原生提示询问（默认）" },
  { id: "accept-edits", label: "可改文件", native: "accept-edits", description: "自动接受文件编辑" },
  { id: "plan", label: "计划", native: "plan", description: "只研究与提方案" },
  { id: "always-proceed", label: "全部放行", native: "always-proceed", description: "不再弹窗，动作全部放行（--yolo）", danger: true },
];

const EMPTY: PermissionOption[] = [];

/** Launch-time table for a harness: every mode the CLI accepts. */
export function launchPermissionTable(kind: PermissionKind | string): PermissionOption[] {
  if (kind === "claude") return CLAUDE_PERMS;
  if (kind === "grok") return GROK_PERMS;
  if (kind === "agy") return AGY_PERMS;
  if (kind === "codex") return [...CODEX_POLICIES, ...CODEX_SANDBOX];
  return EMPTY;
}

/** Codex policy rows (New Session renders policy + sandbox separately). */
export function codexPolicyTable(): PermissionOption[] {
  return CODEX_POLICIES;
}

/** Codex sandbox rows. */
export function codexSandboxTable(): PermissionOption[] {
  return CODEX_SANDBOX;
}

/**
 * The live wheel for a harness: what a runtime `instance.configure` can
 * reach. Only Claude PTY implements the push-down today; other harnesses get
 * a read-only chip. `bypassAllowed` says whether THIS session's launch argv
 * carried the bypass allowance (bypass joins the wheel then).
 */
export function runtimePermissionTable(
  kind: PermissionKind | string,
  opts: { bypassAllowed?: boolean } = {},
): PermissionOption[] {
  if (kind !== "claude") return EMPTY;
  return CLAUDE_PERMS.filter((option) => {
    if (option.launchOnly) return false;
    if (option.id === "bypassPermissions" && !opts.bypassAllowed) return false;
    return true;
  });
}

/** Device-default table (Settings): safe settable defaults, no bypass. */
export function defaultPermissionTable(): PermissionOption[] {
  return CLAUDE_PERMS.filter(
    (option) =>
      !option.danger && !option.launchOnly && option.id !== "dontAsk",
  );
}

export function findPermissionOption(
  kind: PermissionKind | string,
  id: string,
): PermissionOption | undefined {
  return launchPermissionTable(kind).find((option) => option.id === id);
}

/** Normalize a stored/arriving mode id onto a harness's launch table. */
export function normalizePermissionMode(
  kind: PermissionKind | string | undefined,
  value: string | undefined | null,
  fallback = "manual",
): string {
  const harness = kind ?? "claude";
  if (value) {
    const table = launchPermissionTable(harness);
    if (table.some((option) => option.id === value)) return value;
    // Legacy/native aliases. `default` is the TUI spelling of manual;
    // `bypass` is what operators and scripts send for bypassPermissions.
    if (harness === "claude" && (value === "default" || value === "bypass")) {
      return value === "default" ? "manual" : "bypassPermissions";
    }
  }
  return fallback;
}

/** Whether a mode is runtime-reachable for a session given its launch mode. */
export function isLiveReachable(
  kind: PermissionKind | string,
  id: string,
  launchMode: string | undefined,
): boolean {
  if (kind !== "claude") return false;
  if (id === "dontAsk") return false;
  if (id === "bypassPermissions") {
    // Reachable when the session launched with the bypass allowance or
    // already in bypass.
    return launchMode === "bypassPermissions";
  }
  return true;
}

/** Safe native default mode id for each harness's launch table. */
export function defaultPermissionForKind(kind: PermissionKind | string): string {
  switch (kind) {
    case "codex":
      return "untrusted";
    case "grok":
      return "native-prompt";
    case "agy":
      return "native";
    default:
      return "manual";
  }
}
