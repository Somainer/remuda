import type { UsagePayload } from "../../types/generated";
import type { Kind } from "../../types/instance";

/** Harness-native effort tables. Never a generic fast/standard/deep list. */
export type EffortKind = Extract<Kind, "claude" | "codex" | "grok" | "agy" | "terminal" | "generic">;

export type EffortTier = {
  name: string;
  description: string;
};

export type EffortSelection = {
  index: number;
  name: string;
  kind: EffortKind;
};

const CLAUDE: EffortTier[] = [
  { name: "default", description: "不额外思考" },
  { name: "think", description: "默认档" },
  { name: "think-hard", description: "跨文件重构、长任务" },
  { name: "ultracode", description: "最高档 · 慢且贵" },
];

const CODEX: EffortTier[] = [
  { name: "low", description: "不额外思考" },
  { name: "medium", description: "默认档" },
  { name: "high", description: "跨文件重构、长任务" },
  { name: "ultra", description: "最高档 · 慢且贵" },
];

const GROK: EffortTier[] = [
  { name: "quick", description: "不额外思考" },
  { name: "standard", description: "默认档" },
  { name: "max", description: "最高档 · 慢且贵" },
];

const AGY: EffortTier[] = [{ name: "default", description: "agy CLI 默认档" }];

const EMPTY: EffortTier[] = [];

/** Settings / New Session default: claude `think` (index 1). */
export const DEFAULT_EFFORT_INDEX = 1;

export function effortTable(kind: EffortKind | string): EffortTier[] {
  if (kind === "claude") return CLAUDE;
  if (kind === "codex") return CODEX;
  if (kind === "grok") return GROK;
  if (kind === "agy") return AGY;
  return EMPTY;
}

export function clampEffortIndex(index: number, length: number): number {
  if (length <= 0) return 0;
  if (!Number.isFinite(index)) return 0;
  return Math.max(0, Math.min(Math.round(index), length - 1));
}

/** Map a stored index onto another table. Top tier always lands on the new top (ember) tier. */
export function mapEffortIndex(fromIndex: number, fromLen: number, toLen: number): number {
  if (toLen <= 0) return 0;
  if (fromLen <= 1) return clampEffortIndex(fromIndex, toLen);
  const t = clampEffortIndex(fromIndex, fromLen) / (fromLen - 1);
  return Math.round(t * (toLen - 1));
}

export function effortAt(kind: EffortKind | string, index: number): EffortSelection {
  const table = effortTable(kind);
  const i = clampEffortIndex(index, table.length);
  const name = table[i]?.name ?? "default";
  return { index: i, name, kind: (kind as EffortKind) || "claude" };
}

export function mapEffort(current: EffortSelection, nextKind: EffortKind | string): EffortSelection {
  const from = effortTable(current.kind);
  const to = effortTable(nextKind);
  const index = mapEffortIndex(current.index, from.length, to.length);
  return effortAt(nextKind, index);
}

export function isEmberTier(kind: EffortKind | string, index: number): boolean {
  const table = effortTable(kind);
  return table.length > 0 && index === table.length - 1;
}

export function isEmberName(kind: EffortKind | string, name: string): boolean {
  const table = effortTable(kind);
  return table.length > 0 && table[table.length - 1]?.name === name;
}

export function effortCaps(kind: EffortKind | string): {
  harness: boolean;
  model: boolean;
  effort: boolean;
  context: boolean;
  permission: boolean;
} {
  if (kind === "terminal" || kind === "generic") {
    return { harness: true, model: false, effort: false, context: false, permission: false };
  }
  if (kind === "claude") {
    return { harness: true, model: true, effort: true, context: true, permission: true };
  }
  if (kind === "agy") {
    return { harness: true, model: false, effort: true, context: true, permission: false };
  }
  return { harness: true, model: true, effort: true, context: true, permission: false };
}

export const HARNESS_META: { id: EffortKind; label: string; mark: string }[] = [
  { id: "claude", label: "Claude Code", mark: "C" },
  { id: "codex", label: "Codex", mark: "X" },
  { id: "grok", label: "Grok", mark: "G" },
  { id: "agy", label: "agy", mark: "A" },
  { id: "terminal", label: "Terminal", mark: "$" },
];

export function harnessMeta(kind: string): { id: string; label: string; mark: string } {
  return HARNESS_META.find((h) => h.id === kind) ?? { id: kind, label: kind, mark: kind.slice(0, 1).toUpperCase() };
}

export function shortModel(model: string | undefined): string {
  const raw = (model ?? "").trim();
  if (!raw) return "auto";
  const tail = raw.split("/").pop() ?? raw;
  if (tail === "auto" || tail === "auto_model") return "auto";
  return tail;
}

export const DEFAULT_MODELS: Record<string, string[]> = {
  claude: ["opus", "sonnet", "haiku", "passthrough/auto"],
  codex: ["gpt-5", "o3", "passthrough/auto"],
  grok: ["grok-4", "grok-3", "passthrough/auto"],
  agy: ["default"],
  terminal: [],
};

export function modelsFor(kind: string, extra: string[] = []): string[] {
  const base = DEFAULT_MODELS[kind] ?? ["passthrough/auto"];
  const out: string[] = [];
  for (const name of [...extra, ...base]) {
    if (name && !out.includes(name)) out.push(name);
  }
  return out;
}

const CONTEXT_WINDOWS: Record<string, number> = {
  claude: 200_000,
  codex: 200_000,
  grok: 128_000,
  agy: 200_000,
};

function tokenCount(k: UsagePayload["inputTokens"]): number | null {
  if (k.state !== "known") return null;
  const n = Number(k.value);
  return Number.isFinite(n) ? n : null;
}

/** Context-window usage from journal usage events. Null when tokens are unknown. */
export function contextPercent(payload: UsagePayload | undefined, kind: string = "claude"): number | null {
  if (!payload) return null;
  const total = tokenCount(payload.totalTokens);
  const input = tokenCount(payload.inputTokens);
  const output = tokenCount(payload.outputTokens);
  const used = total ?? (input != null && output != null ? input + output : (input ?? output));
  if (used == null) return null;
  const window = CONTEXT_WINDOWS[kind] ?? 200_000;
  return Math.max(0, Math.min(100, Math.round((used / window) * 100)));
}

export const EFFORT_MENU_HEADER = "EFFORT · 本回合生效，发 Command 不只改本地";
export const EFFORT_MENU_FOOTER = "切换只影响后续回合，不重写已发出的 prompt";
