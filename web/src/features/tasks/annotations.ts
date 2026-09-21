/**
 * Structured annotations as device-local composer drafts (plan task-model
 * task 9, D-050 §7 / design §8 / ui-spec §1.5, §2.9).
 *
 * Two carriers:
 *  - **card**: a note pinned to the task card;
 *  - **anchor (`①`)**: a note on text selected in the task detail body or the
 *    transcript.
 *
 * Annotations ride the NEXT send of the session whose composer holds them:
 * on send they are serialised as a structured prefix of the prompt and the
 * drafts are cleared. There is deliberately NO wire field, NO table and NO
 * schema: storage is localStorage, shaped exactly like lib/drafts.ts
 * (`runtime.draft.<instanceId>` → here `runtime.annotation.<instanceId>`).
 * The serialised prefix is structured feedback, never board-protocol text for
 * the agent to manage (D7 / design §8).
 */

export type AnnotationSurface = "task-detail" | "transcript";

export const ANNOTATION_SURFACE_LABEL: Record<AnnotationSurface, string> = {
  "task-detail": "任务详情",
  transcript: "会话记录",
};

export type AnnotationAnchor = {
  surface: AnnotationSurface;
  /** Transcript row id when anchored to a transcript message. */
  messageId?: string | null;
  /** Selected text, verbatim, whitespace-collapsed and length-bounded. */
  quote: string;
};

export type AnnotationDraft = {
  id: string;
  createdAt: number;
  carrier: "card" | "anchor";
  body: string;
  anchor?: AnnotationAnchor;
  /** Task the card-level annotation is pinned to (snapshot ids for display). */
  taskId?: string | null;
  taskTitle?: string | null;
};

/** Selected quotes longer than this collapse behind an ellipsis. */
export const QUOTE_LIMIT = 160;

let draftSeq = 0;

function nextId(now: number): string {
  return `ann_${now.toString(36)}_${(++draftSeq).toString(36)}`;
}

/** Collapse runs of whitespace and bound the quote; never throw on odd input. */
export function normalizeQuote(raw: string): string {
  const compact = raw.replace(/\s+/g, " ").trim();
  return compact.length > QUOTE_LIMIT ? `${compact.slice(0, QUOTE_LIMIT - 1).trimEnd()}…` : compact;
}

export function createCardDraft(
  body: string,
  ctx: { taskId?: string | null; taskTitle?: string | null; now?: number; id?: string } = {},
): AnnotationDraft {
  const now = ctx.now ?? Date.now();
  return {
    id: ctx.id ?? nextId(now),
    createdAt: now,
    carrier: "card",
    body: body.trim(),
    taskId: ctx.taskId ?? null,
    taskTitle: ctx.taskTitle ?? null,
  };
}

export function createAnchorDraft(
  anchor: AnnotationAnchor,
  body: string,
  ctx: { now?: number; id?: string } = {},
): AnnotationDraft {
  const now = ctx.now ?? Date.now();
  return {
    id: ctx.id ?? nextId(now),
    createdAt: now,
    carrier: "anchor",
    body: body.trim(),
    anchor: {
      surface: anchor.surface,
      messageId: anchor.messageId ?? null,
      quote: normalizeQuote(anchor.quote),
    },
  };
}

export function annotationCount(drafts: readonly AnnotationDraft[]): number {
  return drafts.length;
}

/** 1-based ① numbering among the anchor drafts, in creation order. */
export function anchorNumber(drafts: readonly AnnotationDraft[], id: string): number {
  let n = 0;
  for (const draft of drafts) {
    if (draft.carrier !== "anchor") continue;
    n += 1;
    if (draft.id === id) return n;
  }
  return 0;
}

/** The circled mark for an anchor index (`①`…), falling back past ⑳ to `(n)`. */
export function anchorMark(n: number): string {
  if (n >= 1 && n <= 20) return "①②③④⑤⑥⑦⑧⑨⑩⑪⑫⑬⑭⑮⑯⑰⑱⑲⑳"[n - 1];
  return `(${n})`;
}

function cardLine(n: number, draft: AnnotationDraft): string {
  const where = draft.taskTitle?.trim() ? `（${draft.taskTitle.trim()}）` : "";
  return `${n}. 卡片批注${where}：${draft.body}`;
}

function anchorLine(n: number, draft: AnnotationDraft): string {
  const anchor = draft.anchor!;
  const surface = ANNOTATION_SURFACE_LABEL[anchor.surface];
  return `${n}. 文本标记 · ${surface}「${anchor.quote}」：${draft.body}`;
}

/**
 * The structured prefix block (without the trailing prompt). Drafts are
 * listed in creation order in ONE numbering space; the block is human
 * feedback wording — it carries no protocol instructions. Returns "" when
 * there is nothing to send.
 */
export function serializeAnnotationPrefix(drafts: readonly AnnotationDraft[]): string {
  if (drafts.length === 0) return "";
  const lines = drafts.map((draft, i) =>
    draft.carrier === "card" ? cardLine(i + 1, draft) : anchorLine(i + 1, draft),
  );
  return [`【批注 ×${drafts.length}】`, ...lines].join("\n");
}

/**
 * Fold the drafts into the next prompt as a structured prefix. With no drafts
 * (or only blank bodies) the prompt returns byte-identical — a send without
 * annotations must be indistinguishable from today.
 */
export function withAnnotationPrefix(
  drafts: readonly AnnotationDraft[],
  prompt: string,
): string {
  const live = drafts.filter((draft) => draft.body.trim() !== "");
  const prefix = serializeAnnotationPrefix(live);
  if (!prefix) return prompt;
  return prompt ? `${prefix}\n\n${prompt}` : prefix;
}

// ── device-local storage (lib/drafts.ts shape) ────────────────────────────

const keyPrefix = "runtime.annotation.";

function storageKey(instanceId: string): string {
  return keyPrefix + instanceId;
}

export function readAnnotations(instanceId: string): AnnotationDraft[] {
  try {
    const raw = localStorage.getItem(storageKey(instanceId));
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(isDraft);
  } catch {
    return [];
  }
}

function isDraft(value: unknown): value is AnnotationDraft {
  if (typeof value !== "object" || value === null) return false;
  const rec = value as Record<string, unknown>;
  return (
    typeof rec.id === "string" &&
    typeof rec.body === "string" &&
    (rec.carrier === "card" || rec.carrier === "anchor")
  );
}

export function writeAnnotations(instanceId: string, drafts: readonly AnnotationDraft[]): void {
  try {
    if (drafts.length === 0) localStorage.removeItem(storageKey(instanceId));
    else localStorage.setItem(storageKey(instanceId), JSON.stringify(drafts));
  } catch {
    /* ignore quota, mirroring lib/drafts.ts */
  }
  notify();
}

export function addAnnotation(instanceId: string, draft: AnnotationDraft): AnnotationDraft[] {
  if (!draft.body.trim()) return readAnnotations(instanceId);
  const next = [...readAnnotations(instanceId), draft].sort((a, b) => a.createdAt - b.createdAt);
  writeAnnotations(instanceId, next);
  return next;
}

export function removeAnnotation(instanceId: string, id: string): AnnotationDraft[] {
  const next = readAnnotations(instanceId).filter((draft) => draft.id !== id);
  writeAnnotations(instanceId, next);
  return next;
}

/** Drafts clear immediately after a send/hold consumed them. */
export function clearAnnotations(instanceId: string): void {
  writeAnnotations(instanceId, []);
}

/**
 * Fold this instance's drafts into an outgoing prompt. The caller clears the
 * drafts only once the send landed (store.send resolves true), mirroring the
 * composer's 状态待确认 semantics: a failed POST keeps the drafts for retry.
 */
export function composeWithAnnotations(
  instanceId: string,
  prompt: string,
): { text: string; count: number } {
  const drafts = readAnnotations(instanceId).filter((draft) => draft.body.trim() !== "");
  return { text: withAnnotationPrefix(drafts, prompt), count: drafts.length };
}

// ── external-store subscription for useSyncExternalStore ─────────────────

type StoreListener = () => void;
const listeners = new Set<StoreListener>();
const versions = new Map<string, number>();
let globalVersion = 0;

function notify(): void {
  globalVersion += 1;
  for (const key of versions.keys()) versions.set(key, globalVersion);
  for (const listener of listeners) listener();
}

export function subscribeAnnotations(listener: StoreListener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Changes per instance; readAnnotations() re-reads only when this moves. */
export function annotationsVersion(instanceId: string | null | undefined): number {
  if (!instanceId) return 0;
  let version = versions.get(instanceId);
  if (version === undefined) {
    version = globalVersion;
    versions.set(instanceId, version);
  }
  return version;
}

// ── DOM selection capture for the anchor affordance ──────────────────────

/**
 * What the selection 批注 affordance needs from the current selection.
 *
 * Anchor surfaces mark themselves with `data-anchor-surface` (the task detail
 * body and transcript messages). The owning session and read-only state are
 * inherited from the nearest `data-annotation-instance` /
 * `data-annotation-readonly` ancestor when the surface itself does not carry
 * them (the board detail panel puts them on the surface directly).
 */
export type AnnotationSelection = {
  instanceId: string;
  surface: AnnotationSurface;
  messageId: string | null;
  quote: string;
  readonly: boolean;
};

function dataAttr(el: Element | null, name: string): string | null {
  return el?.getAttribute(name) ?? null;
}

export function readAnnotationSelection(
  root: Document | null | undefined = typeof document !== "undefined" ? document : null,
): AnnotationSelection | null {
  const selection = root?.getSelection?.();
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const node = selection.getRangeAt(0).commonAncestorContainer;
  const element = node.nodeType === 1 ? (node as Element) : node.parentElement;
  const surfaceEl = element?.closest("[data-anchor-surface]") ?? null;
  if (!surfaceEl) return null;
  const surface = dataAttr(surfaceEl, "data-anchor-surface");
  if (surface !== "task-detail" && surface !== "transcript") return null;

  const instanceId =
    dataAttr(surfaceEl, "data-annotation-instance") ??
    dataAttr(surfaceEl.closest("[data-annotation-instance]"), "data-annotation-instance");
  if (!instanceId) return null;

  const readonlyAttr =
    dataAttr(surfaceEl, "data-annotation-readonly") ??
    dataAttr(surfaceEl.closest("[data-annotation-readonly]"), "data-annotation-readonly");
  const quote = normalizeQuote(selection.toString());
  if (!quote) return null;
  return {
    instanceId,
    surface,
    messageId: dataAttr(surfaceEl, "data-anchor-message"),
    quote,
    readonly: readonlyAttr === "1",
  };
}
