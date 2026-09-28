import type { Id } from "../types/wire";

let n = 1;

export function id(prefix: string): Id {
  const hex = n.toString(16).padStart(12, "0");
  n += 1;
  return `${prefix}01993ab0-0000-7000-8000-${hex}` as Id;
}

/**
 * Random id for a client-local bubble (clientRequestId). Unlike {@link id}
 * this does NOT reset on reload: the counter in id() restarts at 1 whenever
 * the module reloads, so an unsent bubble restored from the durable outbox
 * and a bubble sent after reload would share the same clientRequestId — the
 * bubble merge/React key then hides one row. Random v4 keeps every send
 * distinct across tab lifetimes.
 */
export function localId(): Id {
  const rand =
    typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
      ? crypto.randomUUID()
      : `${Date.now().toString(16)}-${Math.random().toString(16).slice(2)}-${Math.random()
          .toString(16)
          .slice(2)}`;
  return `local_${rand}` as Id;
}

export function now(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, ".000Z");
}

export function digestPlaceholder(): string {
  return `sha256:${"ab".repeat(32)}`;
}
