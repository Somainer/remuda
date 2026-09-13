import type { UsagePayload } from "../../types/generated";
import type { Kind } from "../../types/instance";

/** Harness-native effort tables. Never a generic fast/standard/deep list. */
export type EffortKind = Extract<Kind, "claude" | "codex" | "grok" | "agy" | "terminal" | "generic">;

export type EffortTier = {
  name: string;
  description: string;
};

/**
 * A chosen effort. `name`/`index` always address a row of the harness table.
 * `ultracode` is Claude-only and is NOT a tier: when it is on Claude forces
 * the `xhigh` tier plus workflow orchestration, so the slider parks and locks
 * on `xhigh`. Other harnesses leave it `undefined`.
 */
export type EffortSelection = {
  index: number;
  name: string;
  kind: EffortKind;
  ultracode?: boolean;
};

/**
 * The real Claude Code levels, in CLI order (`claude --effort`).
 * low · medium · high (default) · xhigh · max.
 */
const CLAUDE: EffortTier[] = [
  { name: "low", description: "最省 · 最快" },
  { name: "medium", description: "日常档" },
  { name: "high", description: "默认档" },
  { name: "xhigh", description: "跨文件 · 长任务" },
  { name: "max", description: "最高档 · 慢且贵" },
];

/** Claude forces this tier while ultracode is on. */
export const CLAUDE_ULTRACODE_INDEX = 3;

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

/** Settings / New Session default: claude `high` (index 2 of the five levels). */
export const DEFAULT_EFFORT_INDEX = 2;

/**
 * Legacy Claude tier names from before the real `--effort` levels, mapped by
 * NAME onto the new table. `ultracode` was once a tier; it is now the
 * `xhigh` tier plus the ultracode boolean. Anything unrecognised lands on the
 * default `high`.
 */
const CLAUDE_LEGACY_NAMES: Record<string, { tier: string; ultracode?: boolean }> = {
  default: { tier: "low" },
  think: { tier: "high" },
  "think-hard": { tier: "xhigh" },
  ultracode: { tier: "xhigh", ultracode: true },
};

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

/** Resolve a Claude stored/legacy name to a current tier name. Unknown → high. */
export function normalizeClaudeName(
  name: string,
): { tier: string; index: number; ultracode: boolean } {
  const table = CLAUDE;
  const direct = table.findIndex((tier) => tier.name === name);
  if (direct >= 0) {
    return { tier: table[direct].name, index: direct, ultracode: false };
  }
  const legacy = CLAUDE_LEGACY_NAMES[name];
  const targetTier = legacy?.tier ?? "high";
  const index = Math.max(
    0,
    table.findIndex((tier) => tier.name === targetTier),
  );
  return { tier: table[index].name, index, ultracode: legacy?.ultracode === true };
}

/** 0..1 position of a snapped index on a discrete track. A single-tier table sits at the ember end. */
export function effortRatio(index: number, length: number): number {
  if (length <= 1) return length === 1 ? 1 : 0;
  return clampEffortIndex(index, length) / (length - 1);
}

/** Snap a 0..1 track position onto the nearest native tier index. */
export function snapEffortIndex(ratio: number, length: number): number {
  if (length <= 1) return 0;
  if (!Number.isFinite(ratio)) return 0;
  const t = Math.max(0, Math.min(1, ratio));
  return Math.round(t * (length - 1));
}

export function effortIndexFromClientX(
  clientX: number,
  track: { left: number; width: number },
  length: number,
): number {
  if (track.width <= 0) return 0;
  return snapEffortIndex((clientX - track.left) / track.width, length);
}

/** Discrete slider keys. Returns null when the event is not an effort key. */
export function keyboardEffortIndex(current: number, key: string, length: number): number | null {
  if (length <= 0) return 0;
  if (key === "ArrowLeft" || key === "ArrowDown") return clampEffortIndex(current - 1, length);
  if (key === "ArrowRight" || key === "ArrowUp") return clampEffortIndex(current + 1, length);
  if (key === "Home") return 0;
  if (key === "End") return length - 1;
  return null;
}

/**
 * The reset/default stop for a harness. The device setting is a Claude index,
 * so on another table it maps by nearest position (「换 harness 后按新表就近映射」)
 * rather than hard-clamping — Claude `high` at the midpoint lands on grok's
 * middle tier, not on its ember `max`.
 */
export function defaultEffortIndex(kind: EffortKind | string): number {
  const to = effortTable(kind);
  return mapEffortIndex(DEFAULT_EFFORT_INDEX, effortTable("claude").length, to.length);
}

/** Map a stored index onto another table. Top tier always lands on the new top (ember) tier. */
export function mapEffortIndex(fromIndex: number, fromLen: number, toLen: number): number {
  if (toLen <= 0) return 0;
  if (fromLen <= 1) return clampEffortIndex(fromIndex, toLen);
  const t = clampEffortIndex(fromIndex, fromLen) / (fromLen - 1);
  return Math.round(t * (toLen - 1));
}

export function effortAt(
  kind: EffortKind | string,
  index: number,
  ultracode?: boolean,
): EffortSelection {
  const table = effortTable(kind);
  let i = clampEffortIndex(index, table.length);
  let ultra = kind === "claude" && ultracode === true;
  // Ultracode forces xhigh; the tier index never lingers on a different stop.
  if (ultra) i = clampEffortIndex(CLAUDE_ULTRACODE_INDEX, table.length);
  const name = table[i]?.name ?? "default";
  const selection: EffortSelection = { index: i, name, kind: (kind as EffortKind) || "claude" };
  if (kind === "claude") selection.ultracode = ultra;
  return selection;
}

/** Rebuild a selection from an instance record after reload, mapping legacy names. */
export function effortFromRecord(
  kind: EffortKind | string,
  name?: string | null,
  index?: number | null,
  ultracode?: boolean | null,
): EffortSelection | undefined {
  if (name == null && (index == null || Number.isNaN(index))) return undefined;
  const table = effortTable(kind);
  const harness = (kind as EffortKind) || "claude";
  if (kind === "claude") {
    if (name) {
      const norm = normalizeClaudeName(name);
      return {
        index: norm.index,
        name: norm.tier,
        kind: "claude",
        ultracode: ultracode === true || norm.ultracode,
      };
    }
    return effortAt("claude", index ?? DEFAULT_EFFORT_INDEX, ultracode === true);
  }
  if (name) {
    const found = table.findIndex((tier) => tier.name === name);
    if (found >= 0) return { index: found, name, kind: harness };
    if (index != null) return effortAt(harness, index);
  }
  if (index != null) return effortAt(harness, index);
  return undefined;
}

export function mapEffort(current: EffortSelection, nextKind: EffortKind | string): EffortSelection {
  // ultracode is Claude-only; leaving Claude drops it, and it never enters elsewhere.
  if (nextKind === "claude" && current.kind === "claude" && current.ultracode) {
    return effortAt("claude", CLAUDE_ULTRACODE_INDEX, true);
  }
  const from = effortTable(current.kind);
  const to = effortTable(nextKind);
  const index = mapEffortIndex(current.index, from.length, to.length);
  return effortAt(nextKind, index);
}

/** The top table row (max for Claude). */
export function isEmberTier(kind: EffortKind | string, index: number): boolean {
  const table = effortTable(kind);
  return table.length > 0 && index === table.length - 1;
}

/** Ember plays on the top tier (max) and whenever Claude ultracode is on. */
export function isEmberEffort(
  kind: EffortKind | string,
  index: number,
  ultracode?: boolean,
): boolean {
  if (kind === "claude" && ultracode === true) return true;
  return isEmberTier(kind, index);
}

export function isEmberName(kind: EffortKind | string, name: string): boolean {
  const table = effortTable(kind);
  return table.length > 0 && table[table.length - 1]?.name === name;
}

/**
 * The tier name to put on the wire. The current Hub stores an opaque effort
 * name, so an ultracode selection round-trips as the legacy `"ultracode"`
 * string until x-p1-proto lands the `{name, ultracode}` shape; reads map it
 * back through {@link normalizeClaudeName}.
 */
export function effortWireName(selection: EffortSelection): string {
  return selection.kind === "claude" && selection.ultracode ? "ultracode" : selection.name;
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
  if (kind === "agy") {
    return { harness: true, model: false, effort: true, context: true, permission: true };
  }
  return { harness: true, model: true, effort: true, context: true, permission: true };
}

/** Whether a harness exposes the ultracode workflow toggle (Claude only). */
export function supportsUltracode(kind: EffortKind | string): boolean {
  return kind === "claude";
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
export const ULTRACODE_HINT = "ultracode · 锁定 xhigh，启用多代理工作流编排";
