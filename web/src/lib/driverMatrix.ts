/**
 * D-028 §5.1 kind/driver matrix, as *read from the Node* (search breadth: medium).
 * New Session is a prefill: picking claude/codex/grok means the shell runs
 * that kind, and launch command in the New Session form must match. When the
 * host reports no inventory we fall back to CLI presence alone because the
 * Node is older than the matrix and nothing on the host proves a pairing is
 * launchable. Nothing here is hardcoded per host.
 */
export type AgentKindId = "claude" | "codex" | "grok" | "agy" | "terminal";

/** Host matrix shape the New Session page consumes. */
export type HostMatrix = {
  cli?: Pick<import("../types/instance").HostCli, "kind" | "installed" | "version">[];
  capabilities?: Record<string, unknown> | null;
};

/** Subset of the host CLI entry used for the version-gated preview. */
type CliEntry = { kind?: string; version?: string; installed?: boolean };

/**
 * Read-only preview of what the PTY will run (D-028 §5.1).
 *
 * D-056: Claude ultracode at launch renders the REAL argv for the chosen
 * version:
 * - ≥2.1.284: `--effort <level>` plus `--settings` carrying
 *   `"ultracode":true` in the one existing overlay (two `--settings` would
 *   drop the first, so the preview names the same overlay once);
 * - 2.1.203–2.1.283: `--effort ultracode` (the single coupled spelling),
 *   which implies xhigh;
 * - unknown/older: the plain `--effort <level>`.
 */
export function launchPreview(
  opts:
    | { kind: "terminal" }
    | {
        kind: Exclude<AgentKindId, "terminal">;
        effortName?: string | null;
        /** D-056 orthogonal launch switch. */
        ultracode?: boolean;
        /** Selected Claude binary version (host CLI inventory); pre-pin. */
        claudeVersion?: string | null;
        yolo?: boolean;
      },
): string {
  if (opts.kind === "terminal") return "$SHELL --login";

  if (opts.kind === "claude") {
    // Always shown: the per-session overlay and its settings keys.
    const argv = [
      "claude",
      "--settings <per-session overlay>",
      "--setting-sources user,project,local",
    ];
    if (opts.yolo || false) {
      argv.push("--dangerously-skip-permissions");
    }
    const on = opts.ultracode === true;
    const gate = versionGate(opts.claudeVersion);
    if (on && (gate === "coupled" || gate === "legacy")) {
      // Coupled: the single spelling forces xhigh. For a legacy (<2.1.203)
      // binary this would be rejected at create time, but the preview still
      // names what the materializer would attempt rather than hiding it.
      argv.push("--effort ultracode");
    } else if (opts.effortName) {
      argv.push(`--effort ${opts.effortName}`);
    }
    if (on && gate === "decoupled") {
      argv.push('# overlay: {"ultracode": true}');
    }
    if (on && gate === "unknown") {
      argv.push(opts.effortName ? "--effort ultracode" : '--settings {"ultracode": true}');
    }
    return argv.join(" ");
  }

  if (opts.kind === "codex") {
    const argv = [
      "codex",
      "(-c model_reasoning_effort=<level> for non-default effort)"
    ];
    if (opts.yolo) argv.push("--dangerously-bypass-approvals-and-sandbox");
    return argv.join(" ");
  }

  if (opts.kind === "grok") {
    return ["grok", "--reasoning-effort <level>", opts.yolo ? "--always-approve" : ""].filter(Boolean).join(" ");
  }

  return ["agy", opts.yolo ? "--yolo" : ""].filter(Boolean).join(" ");
}

/** Classify a Claude version for the launch argv (mirrors effort.ts). */
function versionGate(version: string | null | undefined): "decoupled" | "coupled" | "legacy" | "unknown" {
  const parsed = parse(version);
  if (!parsed) return "unknown";
  const cmp = (to: [number, number, number]) => {
    for (let i = 0; i < 3; i++) {
      if (parsed[i] !== to[i]) return parsed[i] < to[i] ? -1 : 1;
    }
    return 0;
  };
  if (cmp([2, 1, 284]) >= 0) return "decoupled";
  if (cmp([2, 1, 203]) >= 0) return "coupled";
  return "legacy";
}

function parse(version: string | null | undefined): [number, number, number] | null {
  if (!version) return null;
  const match = /(\d+)\.(\d+)\.(\d+)/.exec(version);
  if (!match) return null;
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

export type MatrixDriver = {
  id: import("../types/nativeRef").DriverKind;
  /** True when the Node reports launchable; false is a proof, not a guess. */
  launchable: boolean;
  reason?: string;
};

/**
 * Whether a kind/driver pairing is launchable on a host.
 *
 * True ONLY when the Node reports `shell-pty` with launchable=true AND the
 * inventory says the harness binary is present. If the Node advertises the
 * driver but the operator is on a host where the harness binary is absent,
 * shellPtyAllowed() returns false and the New Session sheet falls back to
 * legacy drivers. An unreported matrix (older Node, capabilities null/empty)
 * falls back on the CLI presence probe alone: claude stays selectable by
 * long-standing default, grok/agy visibility is driven by the CLI list.
 * Nothing is hardcoded about a host's launch capability.
 */
export function shellPtyAllowed(host: HostMatrix | undefined, kind: AgentKindId): boolean {
  if (kind === "terminal") return true;
  if (!cliHasKind(host, kind)) return false;
  const inventory = matrixDrivers(host);
  // An unreported matrix means the Node cannot prove native-PTY launch; fall
  // back to legacy carriers. Only a reported launchable shell-pty row opts in.
  if (inventory.length === 0) return false;
  const shell = inventory.find((d) => d.id === "shell-pty");
  return Boolean(shell && shell.launchable);
}

/**
 * Default driver for New Session: native `shell-pty` whenever the Node says
 * so AND a launchable pairing is proved. Otherwise offer the legacy carriers.
 */
export function defaultDriver(
  host: HostMatrix | undefined,
  kind: AgentKindId,
): import("../types/nativeRef").DriverKind {
  if (kind === "terminal") return "shell-pty";
  if (shellPtyAllowed(host, kind)) return "shell-pty";
  if (kind === "claude") return "claude-pty";
  return "generic-pty";
}

export const DRIVER_LABELS: Record<import("../types/nativeRef").DriverKind, string> = {
  "shell-pty": "原生终端 (shell-pty)",
  "claude-print": "结构化 print (claude-print) · 诊断用，单轮即结束",
  "claude-sdk": "结构化 stream (claude-sdk) · 多轮，无终端视图 · 实验性",
  "claude-pty": "PTY (claude-pty)",
  "claude-bg": "claude-bg",
  "codex-appserver": "codex app-server",
  "grok-acp": "grok ACP",
  "agy-print": "agy print",
  "generic-pty": "通用 PTY (generic-pty)",
};

/**
 * Legacy carriers still offered when native PTY is not reported.
 * claude-print is offered in New Session for diagnostics (single-turn ends).
 */
export function legacyDrivers(kind: AgentKindId): import("../types/nativeRef").DriverKind[] {
  // claude-pty first (keeps a multi-turn TUI alive); generic-pty as the
  // fallback; claude-print last (diagnostic, single-turn — never default).
  if (kind === "claude") return ["claude-pty", "generic-pty", "claude-print"];
  return ["generic-pty"];
}

function matrixDrivers(host: HostMatrix | undefined): MatrixDriver[] {
  const caps = host?.capabilities;
  const list: unknown = caps && typeof caps === "object" && "driverInventory" in caps
    ? (caps as { driverInventory?: unknown }).driverInventory
    : undefined;
  if (!Array.isArray(list)) return [];
  const drivers: MatrixDriver[] = [];
  for (const entry of list) {
    if (!entry || typeof entry !== "object") continue;
    const record = entry as Record<string, unknown>;
    if (typeof record.kind !== "string") continue;
    drivers.push({
      id: record.kind as MatrixDriver["id"],
      launchable: record.launchable === true,
      reason: typeof record.reasonCode === "string" ? record.reasonCode : undefined,
    });
  }
  return drivers;
}

function cliHasKind(host: HostMatrix | undefined, kind: string): boolean {
  if (!host || !Array.isArray(host.cli)) return false;
  // An explicitly installed:false CLI entry is an ABSENT binary; matching the
  // kind alone would make a missing CLI count as installed (c-r2 item 8).
  return host.cli.some((entry: CliEntry) => entry.kind === kind && entry.installed !== false);
}

/** The host's reported Claude CLI version, if any (D-056 launch gate). */
export function hostClaudeVersion(host: HostMatrix | undefined): string | null {
  if (!host || !Array.isArray(host.cli)) return null;
  const entry = host.cli.find((e) => e.kind === "claude");
  return entry?.version ?? null;
}
