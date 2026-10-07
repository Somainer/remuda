import type { UsagePayload } from "../../types/generated";
import type { Kind } from "../../types/instance";

/**
 * D-056 (2026-10-05): Claude's effort is FIVE native slider stops
 * (low · medium · high · xhigh · max) plus an ORTHOGONAL Ultracode switch.
 * On Claude Code ≥ 2.1.284 the switch stays on at ANY level; on the coupled
 * 2.1.203–2.1.283 builds turning it on moves the slider to xhigh and sliding
 * away turns it off. The legacy word `ultracode` is accepted only as an
 * INPUT alias (stored prefs/drafts/wire) and normalises to {xhigh, on}.
 */

export type EffortKind = Extract<Kind, "claude" | "codex" | "grok" | "agy" | "terminal" | "generic">;

/**
 * One native harness tier row.
 */
export type EffortTier = {
  name: string;
  label?: string;
  description: string;
};

/** A slider stop is a native tier, 1:1 (the ultracode switch is separate). */
export type EffortStop = EffortTier & {
  index: number;
  short?: string;
};

/**
 * A chosen effort. `ultracode` is the Claude-only orthogonal boolean and is
 * session-only; it NEVER implies a tier on ≥2.1.284. For other harnesses the
 * field is absent.
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
 * - `top` — a restrained static accent for Claude `xhigh`/`max`, Codex `max`,
 *   and Grok's native top tier. Same family as the Desktop's max look:
 *   warm label/tint, never the animated ember field.
 * - `ultracode` — the animated ember field. On Claude it follows the
 *   ORTHOGONAL ultracode SWITCH (any level, D-056); on Codex it follows the
 *   native Ultra tier.
 */
export type EffortLook = "plain" | "top" | "ultracode";

/**
 * The real Claude Code levels, in CLI order (`claude --effort`).
 * low · medium · high · xhigh · max. No tier is labelled the universal
 * default any more (D-056 §6: the default is per-model).
 */
const CLAUDE: EffortTier[] = [
  { name: "low", description: "最省 · 最快" },
  { name: "medium", description: "均衡档" },
  { name: "high", description: "综合实现 · 完整测试" },
  { name: "xhigh", description: "跨文件 · 长任务" },
  { name: "max", description: "最高档 · 慢且贵" },
];

/**
 * Codex 0.154.0 picker order and exact English copy. Native names pass 1:1 to
 * `-c model_reasoning_effort=…`; Ultra is a real tier, not the Claude-only
 * ultracode workflow flag. See docs/design/evidence/effort-codex-tiers-1.md.
 */
const CODEX: EffortTier[] = [
  { name: "low", label: "Low", description: "Fast responses with lighter reasoning" },
  { name: "medium", label: "Medium", description: "Balances speed and reasoning depth for everyday tasks" },
  { name: "high", label: "High", description: "Greater reasoning depth for complex problems" },
  { name: "xhigh", label: "Extra high", description: "Extra high reasoning depth for complex problems" },
  { name: "max", label: "Max", description: "For difficult problems when quality matters more than speed · higher usage" },
  { name: "ultra", label: "Ultra", description: "For demanding work using multiple agents · highest usage" },
];

/** Index of the real per-table default tier (codex/grok: `medium`). */
const CODEX_DEFAULT_INDEX = 1;

/**
 * Grok `--reasoning-effort` (visible alias `--effort`), the built-in
 * `/effort` menu order low · medium · high · xhigh.
 *
 * Evidence: the public xAI grok-build repository's sampling-types
 * `ReasoningEffort` enum (see docs/design/evidence/composer-slider-5.md).
 */
const GROK: EffortTier[] = [
  { name: "low", description: "更快 · 更轻的思考" },
  { name: "medium", description: "均衡档" },
  { name: "high", description: "重度思考" },
  { name: "xhigh", description: "延展推理 · 慢且贵" },
];

const GROK_DEFAULT_INDEX = 1;

const AGY: EffortTier[] = [{ name: "default", description: "agy CLI 默认档" }];

const EMPTY: EffortTier[] = [];

/**
 * Legacy fallback when neither a per-model default nor any remembered
 * preference exists: claude `high` (index 2). This is NOT a marked default in
 * the UI — the marker comes from {@link claudeDefaultTier} (D-056 §6).
 */
export const DEFAULT_EFFORT_INDEX = 2;

/** xhigh is the tier the coupled build forces while ultracode is on. */
export const CLAUDE_XHIGH_INDEX = 3;

/**
 * Legacy Claude tier names from before the real `--effort` levels, mapped by
 * NAME onto the new table. `ultracode` was once a tier; it is now an INPUT
 * alias for the `xhigh` tier plus the orthogonal ultracode boolean (legacy
 * stored prefs/drafts load as {xhigh, on}, D-056). Anything unrecognised
 * lands on the legacy fallback `high`.
 */
const CLAUDE_LEGACY_NAMES: Record<string, { tier: string; ultracode?: boolean }> = {
  default: { tier: "low" },
  think: { tier: "high" },
  "think-hard": { tier: "xhigh" },
  ultracode: { tier: "xhigh", ultracode: true },
};

/**
 * Codex's retired `minimal` stop migrates to Low. Max and Ultra resolve
 * directly as current native tiers.
 */
const CODEX_LEGACY_NAMES: Record<string, string> = {
  minimal: "low",
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

/** Real default-tier index per harness table (legacy CLI fallback only). */
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

/** Index of the harness's real CLI default tier (legacy fallback). */
export function effortDefaultIndex(kind: EffortKind | string): number {
  const table = effortTable(kind);
  if (table.length === 0) return 0;
  return TABLE_DEFAULT[kind] ?? clampEffortIndex(Math.floor((table.length - 1) / 2), table.length);
}

/**
 * The slider stops for a harness: its native tiers 1:1. Claude gets FIVE
 * stops — ultracode is the separate switch below the pill.
 */
export function effortStops(kind: EffortKind | string): EffortStop[] {
  const table = effortTable(kind);
  // The ~280px composer popover cannot fit six full tick labels; New Session's
  // inline field is wider and renders the full names (see EffortSlider).
  const popoverShort: Record<string, string> = { medium: "med", minimal: "min" };
  return table.map((tier, index) => ({
    ...tier,
    index,
    short: kind === "codex" ? undefined : popoverShort[tier.name],
  }));
}

/** Clamped tier index for a harness — the switch is a separate axis. */
export function effortStopIndex(kind: EffortKind | string, index: number): number {
  const stops = effortStops(kind);
  return clampEffortIndex(index, Math.max(1, stops.length));
}

/** Build a selection from a slider position (tier only; the flag is separate). */
export function effortAtStop(kind: EffortKind | string, stopIndex: number): EffortSelection {
  const stops = effortStops(kind);
  const stop = stops[clampEffortIndex(stopIndex, Math.max(1, stops.length))];
  return effortAt(kind, stop?.index ?? 0, false);
}

/** The tier display name (the switch's on/off is rendered by the switch). */
export function effortStopName(kind: EffortKind | string, name: string): string {
  return effortTable(kind).find((tier) => tier.name === name)?.label ?? name;
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
 * Legacy words migrate (codex `minimal`; grok `quick/standard/max`); an
 * unrecognised word lands on that harness's real CLI default.
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

/** 0..1 position of a snapped index on a discrete track. */
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
 * (codex/grok `medium`), rather than a ratio-mapped position. For Claude the
 * marked default is per-model ({@link claudeDefaultTier}); this remains the
 * generic fallback.
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

/**
 * Build a selection. D-056: on Claude the ultracode boolean is orthogonal and
 * stays on at the requested tier (≥2.1.284 semantics); the COUPLED-build
 * linkage lives in the UI layer ({@link coupledSelection}), not here.
 */
export function effortAt(
  kind: EffortKind | string,
  index: number,
  ultracode?: boolean,
): EffortSelection {
  const table = effortTable(kind);
  const i = clampEffortIndex(index, table.length);
  const name = table[i]?.name ?? "default";
  const selection: EffortSelection = { index: i, name, kind: (kind as EffortKind) || "claude" };
  if (kind === "claude") selection.ultracode = ultracode === true;
  return selection;
}

/**
 * Apply the COUPLED-build (2.1.203–2.1.283) rule: ultracode on forces the
 * slider to xhigh. Used by every surface that has a concrete version gate.
 */
export function coupledSelection(selection: EffortSelection): EffortSelection {
  if (selection.kind !== "claude" || selection.ultracode !== true) return selection;
  return { ...selection, index: CLAUDE_XHIGH_INDEX, name: effortTable("claude")[CLAUDE_XHIGH_INDEX].name };
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
        // The legacy `ultracode` NAME implies the flag even when the record
        // carried no explicit boolean; an explicit boolean still rides along
        // independently (D-056: {max, on} stays max).
        ultracode: ultracode === true || norm.ultracode,
      };
    }
    return effortAt("claude", index ?? DEFAULT_EFFORT_INDEX, ultracode === true);
  }
  if (table.length > 0) {
    if (name) {
      // Migrate legacy words (minimal / quick / standard / max) to the real
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
  // ultracode is Claude-only; leaving Claude drops it. Re-entering Claude
  // keeps the FLAG at the ratio-mapped tier (the flag is orthogonal, D-056),
  // rather than forcing xhigh.
  const keepFlag = nextKind === "claude" && current.kind === "claude" && current.ultracode === true;
  const from = effortTable(current.kind);
  const to = effortTable(nextKind);
  const index = mapEffortIndex(current.index, from.length, to.length);
  return effortAt(nextKind, index, keepFlag);
}

/**
 * The visual look for a stop. Claude ember follows the orthogonal ultracode
 * SWITCH (any tier); Codex ember follows the native Ultra tier.
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
    return index >= CLAUDE_XHIGH_INDEX ? "top" : "plain";
  }
  if (kind === "codex") {
    if (table[index]?.name === "ultra") return "ultracode";
    return table[index]?.name === "max" ? "top" : "plain";
  }
  // Other harnesses: their single native top row gets the static accent;
  // a single-tier table (agy) stays plain.
  if (table.length > 1 && index === table.length - 1) return "top";
  return "plain";
}

/** The single top native table row (`max` for Claude, `ultra` for Codex);
 *  a single-tier table (agy) has no ember row. */
export function isEmberTier(kind: EffortKind | string, index: number): boolean {
  const table = effortTable(kind);
  return table.length > 1 && index === table.length - 1;
}

/** The full animated ember plays on the Claude switch or on Codex Ultra. */
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
  return stop ? effortLook(kind, stop.index, false) === "ultracode" : false;
}

/**
 * The tier name to put on the wire. D-056: the web always sends a native
 * level word; ultracode rides as its own boolean (`{name, ultracode, index}`),
 * never as the legacy `"ultracode"` name. Legacy names migrate via
 * {@link effortFromRecord} before reaching this point.
 */
export function effortWireName(selection: EffortSelection): string {
  return nativeEffortWord(selection.kind, selection.name);
}

// ── D-056 Claude Code version gating ─────────────────────────────────────

export type ClaudeVersionGate =
  /** ≥ 2.1.284: five stops + orthogonal switch at any level. */
  | "decoupled"
  /** 2.1.203–2.1.283: switch on forces xhigh; single `/effort ultracode`. */
  | "coupled"
  /** < 2.1.203: no ultracode at all; the switch is disabled with the reason. */
  | "legacy"
  /** Version unknown/unread/unparsable: the ultracode switch is DISABLED with
   *  a named reason (never assume decoupled support). The five stops stay on. */
  | "unknown";

/** First build that offered `--effort ultracode`. */
export const CLAUDE_ULTRACODE_MIN_VERSION = "2.1.203";
/** First build with the orthogonal toggle (`/effort ultracode on|off`). */
export const CLAUDE_DECOUPLED_MIN_VERSION = "2.1.284";

/** Parse a `major.minor.patch` (extra suffix tolerated); null when unreadable. */
export function parseClaudeVersion(version: string | null | undefined): [number, number, number] | null {
  if (!version) return null;
  const match = /(\d+)\.(\d+)\.(\d+)/.exec(version.trim());
  if (!match) return null;
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

function cmpVersion(a: [number, number, number], b: [number, number, number]): number {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return 0;
}

/** Classify a Claude Code binary version for the ultracode switch rules. */
export function claudeVersionGate(version: string | null | undefined): ClaudeVersionGate {
  const parsed = parseClaudeVersion(version);
  if (!parsed) return "unknown";
  if (cmpVersion(parsed, parseClaudeVersion(CLAUDE_DECOUPLED_MIN_VERSION)!) >= 0) return "decoupled";
  if (cmpVersion(parsed, parseClaudeVersion(CLAUDE_ULTRACODE_MIN_VERSION)!) >= 0) return "coupled";
  return "legacy";
}

// ── D-056 §6 per-model default effort ────────────────────────────────────

/** One row of the Hub's GET /v1/supply/catalog, narrowed to what the slider
 *  needs (the rest of the row is ignored). */
export type ModelEffortCatalogRow = {
  id: string;
  aliases?: string[];
  defaultEffort?: string | null;
  ultracodeCapable?: boolean | null;
};

/**
 * Built-in fallback for offline/mock sessions; mirrors the Hub catalog's
 * Claude rows. Longest id/alias wins, a dated gateway id matches its family
 * by the `family-` / `family` prefix (same rule as model_catalog::lookup).
 */
const CLAUDE_MODEL_FALLBACK: ModelEffortCatalogRow[] = [
  { id: "claude-opus-5", aliases: ["opus"], defaultEffort: "medium", ultracodeCapable: true },
  { id: "claude-sonnet-5", aliases: ["sonnet"], defaultEffort: "medium", ultracodeCapable: true },
  { id: "claude-fable-5", aliases: ["fable"], defaultEffort: "high", ultracodeCapable: true },
  { id: "claude-haiku-4-5", aliases: ["haiku"], defaultEffort: "high", ultracodeCapable: true },
  // Opus 4.7 (D-056 §6): older frontier, default xhigh; dated/[1m] spellings
  // resolve via longest-match.
  { id: "claude-opus-4-7", aliases: ["opus-4-7"], defaultEffort: "xhigh", ultracodeCapable: true },
];

function normalizeModelId(modelId: string): string {
  let id = modelId.trim().toLowerCase();
  if (id.endsWith("[1m]")) id = id.slice(0, -4);
  // Strip a dated snapshot suffix (`-20251001`).
  const bytes = id.split("");
  if (
    bytes.length > 9
    && bytes[bytes.length - 9] === "-"
    && bytes.slice(bytes.length - 8).every((c) => c >= "0" && c <= "9")
  ) {
    id = id.slice(0, -9);
  }
  return id;
}

/**
 * Resolve one catalog row for a model id (exact id, alias, gateway
 * `profile/family-…` spelling, or a dated snapshot), longest candidate wins.
 * `rows` are the Hub catalog rows when reachable; the built-in fallback fills
 * in for demo/offline mode.
 */
export function lookupModelEffortRow(
  modelId: string | null | undefined,
  rows?: ModelEffortCatalogRow[] | null,
): ModelEffortCatalogRow | null {
  if (!modelId) return null;
  const candidates = new Set<string>();
  for (const raw of [modelId, modelId.split("/").pop() ?? ""]) {
    let id = normalizeModelId(raw);
    if (id) candidates.add(id);
    // A gateway profile id may itself carry a `/` after normalization.
    const tail = id.split("/").pop();
    if (tail && tail !== id) candidates.add(normalizeModelId(tail));
  }
  let best: { len: number; row: ModelEffortCatalogRow } | null = null;
  for (const row of rows && rows.length ? rows : CLAUDE_MODEL_FALLBACK) {
    for (const candidateName of [row.id, ...(row.aliases ?? [])]) {
      const candidate = candidateName.toLowerCase();
      for (const id of candidates) {
        if ((id === candidate || id.startsWith(`${candidate}-`)) && (!best || candidate.length > best.len)) {
          best = { len: candidate.length, row };
        }
      }
    }
  }
  return best?.row ?? null;
}

/**
 * The per-model DEFAULT Claude tier to mark on the slider, or null when the
 * model is unknown (no marker; an unpinned draft says "follow the model
 * default" without guessing the tier — D-056 §6).
 */
export function claudeDefaultTier(
  modelId: string | null | undefined,
  rows?: ModelEffortCatalogRow[] | null,
): { name: string; index: number } | null {
  const row = lookupModelEffortRow(modelId, rows);
  const name = row?.defaultEffort;
  if (!name) return null;
  const index = CLAUDE.findIndex((tier) => tier.name === name);
  return index >= 0 ? { name, index } : null;
}

/** Whether the static catalog says this model supports the ultracode toggle. */
export function modelUltracodeCapable(
  modelId: string | null | undefined,
  rows?: ModelEffortCatalogRow[] | null,
): boolean {
  return lookupModelEffortRow(modelId, rows)?.ultracodeCapable !== false;
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
  const out: string[] = [];
  for (const name of [...extra, ...baseModels(kind)]) {
    if (name && !out.includes(name)) out.push(name);
  }
  return out;
}

function baseModels(kind: string): string[] {
  return DEFAULT_MODELS[kind] ?? ["passthrough/auto"];
}

const CONTEXT_WINDOWS: Record<string, number> = {
  claude: 200_000,
  codex: 200_000,
  grok: 128_000,
  agy: 200_000,
};

function tokenCount(k: UsagePayload["inputTokens"]): number | null {
  if (!k || k.state !== "known") return null;
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

/** Switch row copy (D-056 ui-spec §2.2). */
export const ULTRACODE_SWITCH_LABEL = "Ultracode";
export const ULTRACODE_SWITCH_DESC = "每个任务编排 dynamic workflow · 仅本会话";
export const ULTRACODE_SWITCH_SPACE = "Ultracode 开关在档位下方独立存在，不移动滑杆";
export const ULTRACODE_COUPLED_DESC = "开启后以 xhigh 运行（Claude Code 2.1.203–2.1.283）";
