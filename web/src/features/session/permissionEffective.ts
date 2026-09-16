/**
 * Effective permission-mode view model: the transcript/TUI-read-back mode,
 * the permission counterpart of effortEffective. The UI chip renders from
 * this — never from the requested mode.
 */
import type { Observation } from "../../types/observation";
import type { Instance } from "../../types/instance";
import { normalizePermissionMode } from "./permissions";

export type PermissionEffectiveView = {
  /** Protocol wire mode (`manual`/`acceptEdits`/`plan`/`auto`/…). */
  mode: string;
  source: "launch" | "slash" | "remuda" | "unknown";
  observedAt: string;
};

function view(payload: unknown): PermissionEffectiveView | null {
  const effective = (payload as { effective?: unknown } | null)?.effective as
    | { mode?: unknown; source?: unknown; observedAt?: unknown }
    | undefined;
  const mode = typeof effective?.mode === "string" ? effective.mode : null;
  const source = effective?.source;
  const observedAt = typeof effective?.observedAt === "string" ? effective.observedAt : null;
  if (!mode || !observedAt) return null;
  if (source !== "launch" && source !== "slash" && source !== "remuda" && source !== "unknown") {
    return null;
  }
  return {
    mode: normalizePermissionMode("claude", mode),
    source,
    observedAt,
  };
}

export function effectivePermissionFromObservation(
  observation: Observation,
): { effective: PermissionEffectiveView } | null {
  if (observation.kind !== "permission") return null;
  const effective = view(observation.payload);
  return effective ? { effective } : null;
}

export function effectivePermissionFromRecord(
  record: Instance["permissionEffective"],
): PermissionEffectiveView | null {
  if (!record) return null;
  return view({ effective: record });
}
