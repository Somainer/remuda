/**
 * D-028 §5.1 kind/driver matrix, as *reported by the Node*.
 *
 * New Session is a prefill: picking claude/codex/grok/agy means Remuda opens
 * its own PTY and runs the launch command in it, so the default driver is
 * `shell-pty` whenever the selected host's inventory says that pairing is
 * launchable. Nothing here is hardcoded per host — the matrix comes from
 * `driverInventory` in `node.hello` / heartbeat (stored verbatim on the Hub
 * host view); the CLI presence probes decide whether the harness binary
 * exists. When a Node reports no inventory we fall back to CLI presence
 * alone, because "not reported" is not "unsupported".
 */
import type { HostCli } from "../types/instance";
import type { DriverKind } from "../types/nativeRef";

export type AgentKindId = "claude" | "codex" | "grok" | "agy";

/** Subset of the protocol `DriverDescriptor` this decision needs. */
export type MatrixDriver = {
  kind: string;
  launchable?: boolean;
};

export type HostMatrix = {
  cli?: Pick<HostCli, "kind" | "installed">[];
  /**
   * Raw `capabilities` object from the Hub host view. Newer Nodes put the
   * protocol `DriverDescriptor[]` under `driverInventory`; older shapes may
   * carry the same list under `drivers`.
   */
  capabilities?:
    | { driverInventory?: MatrixDriver[]; drivers?: MatrixDriver[] }
    | null
    | undefined;
};

export function matrixDrivers(host: HostMatrix | undefined): MatrixDriver[] {
  const caps = host?.capabilities;
  const list = caps?.driverInventory ?? caps?.drivers;
  return Array.isArray(list) ? list.filter((d): d is MatrixDriver => Boolean(d && typeof d.kind === "string")) : [];
}

function cliHasKind(host: HostMatrix | undefined, kind: string): boolean {
  const cli = host?.cli;
  if (!cli || cli.length === 0) {
    // No CLI inventory at all (older Node): do not claim the binary is
    // missing — claude stays selectable by long-standing default.
    return kind === "claude";
  }
  const entry = cli.find((item) => item.kind === kind);
  return Boolean(entry && entry.installed !== false);
}

/**
 * Whether the host can run `kind` directly inside a Remuda-owned PTY.
 *
 * True when the Node advertises a launchable `shell-pty` driver AND the
 * harness binary is present. If the Node advertises an explicit matrix that
 * refuses shell-pty, that answer is honoured even when the CLI exists.
 */
export function shellPtyAllowed(host: HostMatrix | undefined, kind: AgentKindId | "terminal"): boolean {
  if (kind === "terminal") return true;
  if (!cliHasKind(host, kind)) return false;
  const inventory = matrixDrivers(host);
  // An unreported matrix means the Node cannot prove it launches agents inside its own PTY
  // (D-028 P2 core not enabled) — fall back to the legacy carriers instead of typing the
  // first prompt into a bare shell. Only a reported `shell-pty` row with launchable=true opts in.
  if (inventory.length === 0) return false;
  const shell = inventory.find((d) => d.kind === "shell-pty");
  return Boolean(shell && shell.launchable !== false);
}

/** Legacy carriers still offered, secondary to the native PTY. */
export function legacyDrivers(kind: AgentKindId): DriverKind[] {
  // claude-pty first: it keeps a multi-turn TUI alive. claude-print is offered
  // last and never defaulted — a print session ends after one turn and needs a
  // manual resume, so it is a diagnostic carrier only, chosen explicitly (D-035).
  if (kind === "claude") return ["claude-pty", "generic-pty", "claude-print"];
  return ["generic-pty"];
}

/**
 * Default driver for the New Session sheet: native PTY first, legacy drivers
 * only as a fallback for hosts that have not advertised the matrix.
 */
export function defaultDriver(host: HostMatrix | undefined, kind: AgentKindId | "terminal"): DriverKind {
  if (kind === "terminal") return "shell-pty";
  if (shellPtyAllowed(host, kind)) return "shell-pty";
  if (kind === "claude") return "claude-pty";
  return "generic-pty";
}

export const DRIVER_LABELS: Record<DriverKind, string> = {
  "shell-pty": "原生终端 (shell-pty)",
  "claude-print": "结构化 print (claude-print) · 诊断用，单轮即结束",
  // D-037: same stream-json/stdio transport as print minus `-p`, so stdin stays
  // open and the child serves many turns. No Terminal view on this carrier
  // (stdio is not a PTY). Experimental and never a default — explicit only.
  "claude-sdk": "结构化 stream-json (claude-sdk) · 多轮，无终端视图 · 实验性",
  "claude-pty": "herdr PTY (claude-pty)",
  "generic-pty": "herdr 通用 PTY (generic-pty)",
  "claude-bg": "claude-bg",
  "codex-appserver": "codex app-server",
  "grok-acp": "grok ACP",
  "agy-print": "agy print",
};

export type LaunchPreviewInput = {
  kind: AgentKindId | "terminal";
  effortName?: string | null;
  yolo?: boolean;
};

/**
 * Read-only preview of what the PTY will run.
 *
 * The materialized recipe is Node-side (`flags.rs` whitelist + per-kind
 * recipe) and has no Hub GET yet; until it does, this is the honest
 * kind + flags summary of D-028 §5.1 — never user-supplied argv.
 */
export function launchPreview({ kind, effortName, yolo }: LaunchPreviewInput): string {
  if (kind === "terminal") return "$SHELL --login";
  if (kind === "claude") {
    const argv = [
      "claude",
      "--settings <per-session overlay>",
      "--setting-sources user,project,local",
      ...(effortName ? [`--effort ${effortName}`] : []),
      ...(yolo ? ["--dangerously-skip-permissions"] : []),
    ];
    return argv.join(" ");
  }
  if (kind === "codex") {
    return ["codex", yolo ? "--dangerously-bypass-approvals-and-sandbox" : "", "(CODEX_HOME → 影子目录)"]
      .filter(Boolean)
      .join(" ");
  }
  if (kind === "grok") {
    return ["grok", yolo ? "--always-approve" : "", "(GROK_HOME → 影子目录)"].filter(Boolean).join(" ");
  }
  return ["agy", yolo ? "--yolo" : ""].filter(Boolean).join(" ");
}
