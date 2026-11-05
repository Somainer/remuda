/**
 * The Hub's seat-profile routes (cb-seat-profile, D-063; protocol.md §8.4).
 *
 * Every edit carries `expectedRevision`; a 409 PROFILE_REVISION_CONFLICT
 * means another device saved in between, and the panel asks to reload
 * instead of overwriting. An edit takes effect at the next chapter.
 */
import { rest } from "../../../lib/api";
import type { components } from "../../../lib/api.generated";
import { HubHttpError } from "../../../lib/httpError";

export type SeatProfileView = components["schemas"]["SeatProfileView"];
export type SeatProfile = components["schemas"]["SeatProfile"];
export type SeatProfileUpdate = components["schemas"]["SeatProfileUpdate"];
export type SeatProfilePutResult = components["schemas"]["SeatProfilePutResult"];
export type LaunchCapabilities = components["schemas"]["LaunchCapabilities"];

function path(lineageId: string): string {
  return `/v1/lineages/${encodeURIComponent(lineageId)}/profile`;
}

export async function getSeatProfile(lineageId: string): Promise<SeatProfileView> {
  return rest<SeatProfileView>(path(lineageId));
}

export async function putSeatProfile(lineageId: string, body: SeatProfileUpdate): Promise<SeatProfilePutResult> {
  return rest<SeatProfilePutResult>(path(lineageId), { method: "PUT", body: JSON.stringify(body) });
}

/** Whether `error` is the profile's stale-revision 409. */
export function isProfileConflict(error: unknown): error is HubHttpError {
  return error instanceof HubHttpError && error.code === "PROFILE_REVISION_CONFLICT";
}

/** The current revision a 409's message names ("…it is at revision N"). */
export function conflictRevision(error: HubHttpError): number | null {
  const match = /revision (\d+)/.exec(error.message);
  return match ? Number(match[1]) : null;
}

/** Wire categories of an unavailable launch item, in words. */
export const UNAVAILABLE_LABEL: Record<string, string> = {
  unreadable: "无法读取",
  missing: "不存在",
  "not-a-directory": "不是目录",
  "not-a-file": "不是文件",
  "too-large": "过大",
  "invalid-name": "名称无效",
  "unsupported-by-harness": "该 harness 不支持",
  "not-permitted": "不允许",
};

/** Item classes, in words. */
export const CLASS_LABEL: Record<string, string> = {
  skill: "技能",
  capability: "能力",
  "mcp-server": "MCP 服务",
  "mcp-config": "MCP 配置",
  grant: "授权",
};

/** Effort levels the editor offers; empty means the harness decides. */
export const EFFORT_LEVELS = ["low", "medium", "high", "xhigh", "max"] as const;
