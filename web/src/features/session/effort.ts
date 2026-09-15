import type { UsagePayload } from "../../types/generated";
import type { Kind } from "../../types/instance";

/** Harness-native effort tables. Never a generic fast/standard/deep list. */
export type EffortKind = Extract<Kind, "claude" | "codex" | "grok" | "agy" | "terminal" | "generic">;

export type EffortTier = {
  name: string;
  description: string;
};

/**
 * One stop of the slider. Tiers map 1:1 onto a row of the harness table; the
 * Claude-only `ultracode` stop is a sixth stop PAST `max` that selects the
 * `xhigh` tier together with the ultracode workflow flag — the wire shape is
 * still {@link EffortSelection} `{name: "xhigh", ultracode: true}`.
 */
export type EffortStop = {
  name: string;
  description: string;
  /** Tier index addressed in {@link effortTable}; the ultracode stop parks on xhigh. */
  index: number;
  ultracode: boolean;
  /** Short tick label for the cramped composer popover; falls back to `name`. */
  short?: string;
};

/**
 * A chosen effort. `name`/`index` always address a row of the harness table.
 * `ultracode` is Claude-only and is NOT a tier: when it is on Claude forces
 * the `xhigh` tier plus workflow orchestration. In the UI it is the slider's
 * sixth (rightmost) stop rather than a separate switch; other harnesses leave
 * it `undefined`.
 */
export type EffortSelection = {
  index: number;
  name: string;
  kind: EffortKind;
  ultracode?: boolean;
};

/**
 * The three visual treatments of the one slider (composer-slider-5):
 *
 * - `plain` — ordinary tiers: the cold brand fill, no amber.
 * - `top` — a restrained static top-tier accent for `xhigh`/`max` (and the
 *   native top tier of codex/grok). Same family as the Desktop's max look:
 *   warm label/tint, never the animated ember field.
 * - `ultracode` — the Claude ultracode stop ALONE: the full ember glow,
 *   drifting spark layers, its own label colour and thumb state.
 */
export type EffortLook = "plain" | "top" | "ultracode";

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

/**
 * Codex `model_reasoning_effort`, in CLI/enum order (codex-cli 0.147.0
 * `ReasoningEffort::from_str`): minimal · low · medium · high · xhigh.
 *
 * Evidence: the installed binary + upstream enum and the embedded model
 * catalog — `low/medium/high/xhigh` are advertised by every current model,
 * `minimal` is parsed by the enum/config (newest models only), while `max`
 * (5.6-luna and newer) and `ultra` (5.6-sol/terra only) are model-gated
 * behind the advanced picker and never offered here. The slider passes these
 * 1:1 to `-c model_reasoning_effort=…`; see
 * docs/design/evidence/composer-slider-5.md.
 */
const CODEX: EffortTier[] = [
  { name: "minimal", description: "极简思考" },
  { name: "low", description: "不额外思考" },
  { name: "medium", description: "默认档" },
  { name: "high", description: "跨文件重构、长任务" },
  { name: "xhigh", description: "最深推理 · 慢且贵" },
];

/** Index of the real per-table default tier (codex/grok: `medium`). */
const CODEX_DEFAULT_INDEX = 2;

/**
 * Grok `--reasoning-effort` (visible alias `--effort`), the built-in
 * `/effort` menu order low · medium · high · xhigh.
 *
 * Evidence: the public xAI grok-build repository's sampling-types
 * `ReasoningEffort` enum (see docs/design/evidence/composer-slider-5.md).
 */
const GROK: EffortTier[] = [
  { name: "low", description: "更快 · 更轻的思考" },
  { name: "medium", description: "默认档" },
  { name: "high", description: "重度思考" },
  { name: "xhigh", description: "延展推理 · 慢且贵" },
];

const GROK_DEFAULT_INDEX = 1;

const AGY: EffortTier[] = [{ name: "default", description: "agy CLI 默认档" }];

const EMPTY: EffortTier[] = [];

/** Settings / New Session default: claude `high` (index 2 of the five levels). */
export const DEFAULT_EFFORT_INDEX = 2;

/**
 * Legacy Claude tier names from before the real `--effort` levels, mapped by
 * NAME onto the new table. `ultracode` was once a tier; it is now the
 * rightmost slider stop over the `xhigh` tier plus the ultracode boolean.
 * Anything unrecognised lands on the default `high`.
 */
const CLAUDE_LEGACY_NAMES: Record<string, { tier: string; ultracode?: boolean }> = {
  default: { tier: "low" },
  think: { tier: "high" },
  "think-hard": { tier: "xhigh" },
  ultracode: { tier: "xhigh", ultracode: true },
};

/**
 * Legacy Codex names from before the verified vocabulary. The old top stop
 * `ultra` is NOT a codex value — it migrates to the real top `xhigh`;
 * anything else lands on the codex default `medium`.
 */
const CODEX_LEGACY_NAMES: Record<string, string> = {
  ultra: "xhigh",
};

/**
 * Legacy Grok names from the invented quick/standard/max table. They migrate
 * by name onto the real low/medium/high/xhigh menu; unknown → `medium`.
 */
const GROK_LEGACY_NAMES: Record<string, string> = {
  quick: "low",
  standard: "medium",
  max: "xhigh",
};

/** Real default-tier index per harness table. */
const TABLE_DEFAULT: Partial<Record<string, number>> = {
  claude: DEFAULT_EFFORT_INDEX,
  codex: CODEX_DEFAULT_INDEX,
  grok: GROK_DEFAULT_INDEX,
  agy: 0,
};

export function effortTable(kind: EffortKind | string): EffortTier[] {
  if (kind === "claude") return CLAUDE;
  if (kind === "codex") return CODEX;
  if (kind === "grok") return GROK;
  if (kind === "agy") return AGY;
  return EMPTY;
}

/** Index of the harness's real CLI default tier. */
export function effortDefaultIndex(kind: EffortKind | string): number {
  const table = effortTable(kind);
  if (table.length === 0) return 0;
  return TABLE_DEFAULT[kind] ?? clampEffortIndex(Math.floor((table.length - 1) / 2), table.length);
}

/**
 * The slider stops for a harness. Claude gets a sixth stop past `max` —
 * ultracode — which selects `xhigh` plus the workflow flag. Every other
 * harness exposes its native tiers one-to-one.
 */
export function effortStops(kind: EffortKind | string): EffortStop[] {
  const table = effortTable(kind);
  // The ~280px composer popover cannot fit six full tick labels; New Session's
  // inline field is wider and renders the full names (see EffortSlider).
  const popoverShort: Record<string, string> = { medium: "med", minimal: "min" };
  const stops: EffortStop[] = table.map((tier, index) => ({
    ...tier,
    index,
    ultracode: false,
    short: popoverShort[tier.name],
  }));
  if (kind === "claude") stops.push({ ...CLAUDE_ULTRACODE_STOP_DEF });
  return stops;
}

/** Claude forces this tier while ultracode is on. */
export const CLAUDE_ULTRACODE_INDEX = 3;

/** Position of the ultracode stop on the six-stop Claude slider (past `max`). */
export const CLAUDE_ULTRACODE_STOP = 5;

/**
 * The ultracode stop, in the Desktop's slot: the rightmost stop past `max`,
 * still the `xhigh` tier plus the workflow flag on the wire.
 */
const CLAUDE_ULTRACODE_STOP_DEF: EffortStop = {
  name: "ultracode",
  description: "多代理工作流 · 锁 xhigh",
  index: CLAUDE_ULTRACODE_INDEX,
  ultracode: true,
  short: "ultra",
};

/** Slider position (0..stops-1) of a Claude tier/flag pair. Ultracode is the last stop. */
export function effortStopIndex(
  kind: EffortKind | string,
  index: number,
  ultracode?: boolean,
): number {
  if (kind === "claude" && ultracode === true) return CLAUDE_ULTRACODE_STOP;
  const stops = effortStops(kind);
  return clampEffortIndex(index, Math.max(1, stops.length));
}

/** Build a selection from a slider position. The last Claude stop is xhigh + ultracode. */
export function effortAtStop(kind: EffortKind | string, stopIndex: number): EffortSelection {
  const stops = effortStops(kind);
  const stop = stops[clampEffortIndex(stopIndex, Math.max(1, stops.length))];
  return effortAt(kind, stop?.index ?? 0, stop?.ultracode === true);
}

/** The name the UI shows for the current stop: "ultracode" while the flag is on. */
export function effortStopName(
  kind: EffortKind | string,
  name: string,
  ultracode?: boolean,
): string {
  return kind === "claude" && ultracode === true ? "ultracode" : name;
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

/**
 * Resolve a stored/legacy NAME on a non-Claude harness table to a current row.
 * Legacy words migrate (codex `ultra→xhigh`; grok `quick/standard/max`); an
 * unrecognised word lands on that harness's real CLI default. Returns the
 * table index and tier name.
 */
export function normalizeHarnessName(
  kind: EffortKind | string,
  name: string,
): { tier: string; index: number } {
  const table = effortTable(kind);
  if (table.length === 0) return { tier: name, index: 0 };
  const direct = table.findIndex((tier) => tier.name === name);
  if (direct >= 0) return { tier: table[direct].name, index: direct };
  const legacyMap = kind === "codex" ? CODEX_LEGACY_NAMES : kind === "grok" ? GROK_LEGACY_NAMES : {};
  const target = legacyMap[name] ?? table[effortDefaultIndex(kind)].name;
  const index = Math.max(0, table.findIndex((tier) => tier.name === target));
  return { tier: table[index].name, index };
}

/**
 * The exact native word to put on a harness's effort channel, or throw.
 *
 * Unlike the record reader (which migrates legacy words) this is the guard for
 * values about to be persisted/sent: only a CURRENT tier row of that harness
 * is accepted, so an unknown word can never be passed straight to the CLI.
 * The driver enforces the same set again in Rust.
 */
export class UnknownEffortError extends Error {
  constructor(
    kind: string,
    name: string,
  ) {
    super(`unknown ${kind} effort tier ${JSON.stringify(name)}`);
    this.name = "UnknownEffortError";
  }
}

export function nativeEffortWord(kind: EffortKind | string, name: string): string {
  const table = effortTable(kind);
  // Kinds with no effort axis (terminal/generic) carry the Hub's opaque
  // string; there is no native vocabulary to validate against.
  if (table.length === 0) return name;
  if (!table.some((tier) => tier.name === name)) {
    throw new UnknownEffortError(String(kind), name);
  }
  return name;
}

/** 0..1 position of a snapped index on a discrete track. A single-tier table sits at the top end. */
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
 * The reset/default stop for a harness: the CLI's own real default
 * (claude `high`, codex/grok `medium`), rather than a ratio-mapped position.
 */
export function defaultEffortIndex(kind: EffortKind | string): number {
  return effortDefaultIndex(kind);
}

/** Map a stored index onto another table. Top tier always lands on the new top tier. */
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
  if (table.length > 0) {
    if (name) {
      // Migrate legacy words (ultra / quick / standard / max) to the real
      // table; unknown words land on the CLI default — never passed through.
      const norm = normalizeHarnessName(harness, name);
      return { index: norm.index, name: norm.tier, kind: harness };
    }
    return effortAt(harness, index ?? effortDefaultIndex(harness));
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

/**
 * The visual look for a stop. The animated ember field exists ONLY on the
 * Claude ultracode stop; `xhigh`/`max` (and each harness's native top tier)
 * carry the restrained static `top` accent; everything else is plain.
 */
export function effortLook(
  kind: EffortKind | string,
  index: number,
  ultracode?: boolean,
): EffortLook {
  if (kind === "claude" && ultracode === true) return "ultracode";
  const table = effortTable(kind);
  if (kind === "claude") {
    // xhigh and max share the restrained top-tier accent.
    return index >= CLAUDE_ULTRACODE_INDEX ? "top" : "plain";
  }
  // Other harnesses: their single native top row gets the same static accent;
  // a single-tier table (agy) stays plain.
  if (table.length > 1 && index === table.length - 1) return "top";
  return "plain";
}

/** The single top table row only (`max` for Claude): the strongest non-ultra tier. */
export function isEmberTier(kind: EffortKind | string, index: number): boolean {
  const table = effortTable(kind);
  return table.length > 0 && index === table.length - 1;
}

/**
 * The full animated ember plays ONLY on the Claude ultracode stop. Kept under
 * this name for the existing chip/list call sites; a top-tier selection
 * (xhigh/max) is the restrained `top` look, not the ember.
 */
export function isEmberEffort(
  kind: EffortKind | string,
  index: number,
  ultracode?: boolean,
): boolean {
  return effortLook(kind, index, ultracode) === "ultracode";
}

export function isEmberName(kind: EffortKind | string, name: string): boolean {
  const stops = effortStops(kind);
  const stop = stops.find((s) => s.name === name);
  return stop ? effortLook(kind, stop.index, stop.ultracode) === "ultracode" : false;
}

/**
 * The tier name to put on the wire. The current Hub stores an opaque effort
 * name, so an ultracode selection round-trips as the legacy `"ultracode"`
 * string until x-p1-proto lands the `{name, ultracode}` shape; reads map it
 * back through {@link normalizeClaudeName}. Non-Claude names must be current
 * tier words — legacy words migrate before this point, never pass through.
 */
export function effortWireName(selection: EffortSelection): string {
  if (selection.kind === "claude" && selection.ultracode) return "ultracode";
  return nativeEffortWord(selection.kind, selection.name);
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
