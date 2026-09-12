import type { Knowledge, U64 } from "../types/wire";

export function knownText(k: Knowledge<string> | undefined, fallback = "—"): string {
  return k?.state === "known" ? k.value : fallback;
}

export function formatTokens(k: Knowledge<U64>): string | null {
  if (k.state !== "known") return null;
  const n = Number(k.value);
  if (!Number.isFinite(n)) return null;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
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
