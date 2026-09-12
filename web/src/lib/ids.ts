import type { Id } from "../types/wire";

let n = 1;

export function id(prefix: string): Id {
  const hex = n.toString(16).padStart(12, "0");
  n += 1;
  return `${prefix}01993ab0-0000-7000-8000-${hex}` as Id;
}

export function now(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, ".000Z");
}

export function digestPlaceholder(): string {
  return `sha256:${"ab".repeat(32)}`;
}
