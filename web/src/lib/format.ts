import type { Knowledge, U64 } from "../types/wire";

type KnowledgeLike = { state: string; value?: unknown };

export function knownText(k: Knowledge<string> | KnowledgeLike | undefined, fallback = "—"): string {
  return k?.state === "known" && typeof k.value === "string" ? k.value : fallback;
}

export function formatTokens(k: Knowledge<U64> | KnowledgeLike): string | null {
  if (k.state !== "known") return null;
  const n = Number(k.value);
  if (!Number.isFinite(n)) return null;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

export function knowledgeString(k: KnowledgeLike | undefined): string | undefined {
  return k?.state === "known" && typeof k.value === "string" ? k.value : undefined;
}

export function shortId(value: string, n = 8): string {
  return value.length <= n ? value : value.slice(0, n);
}

export function jsonPreview(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

export function asRecord(value: unknown): Record<string, unknown> | null {
  if (value && typeof value === "object" && !Array.isArray(value)) return value as Record<string, unknown>;
  return null;
}

export function asString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

export function formatClock(iso: string): string {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return "—";
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}

export function formatListTime(iso: string, nowMs = Date.now()): string {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return "—";
  const delta = nowMs - t;
  if (delta < 45_000) return "刚刚";
  if (delta < 3_600_000) return `${Math.max(1, Math.round(delta / 60_000))}m`;
  const date = new Date(t);
  const today = new Date(nowMs);
  if (date.toDateString() === today.toDateString()) return formatClock(iso);
  const yesterday = new Date(nowMs);
  yesterday.setDate(today.getDate() - 1);
  if (date.toDateString() === yesterday.toDateString()) return "昨天";
  return date.toLocaleDateString([], { month: "numeric", day: "numeric" });
}
