//!
//! On-demand client for one subagent's sidechain transcript (drill-in).
//!
//! Subagents are Claude sub-sessions INSIDE one Remuda session, never Remuda
//! instances, so their conversation never enters the live journal. The Hub
//! proxies a bounded read of `agent-<id>.jsonl` to the owning Node, which
//! maps it through the same transcript pipeline as the main session. Kept out
//! of `api.ts`/the live store: the contract is a user-initiated read, no
//! caching, like the workspace-changes client.

import { HubHttpError } from "../../../lib/httpError";
import { readSession } from "../../../lib/session";
import type { Observation } from "../../../types/observation";
import type { Id } from "../../../types/wire";
import { coerceObservationList } from "../../../lib/hubJournal";

function hubBase(): string {
  if (import.meta.env.DEV && import.meta.env.VITE_HUB_URL) return "";
  const raw = import.meta.env.VITE_API_BASE ?? import.meta.env.VITE_HUB_URL ?? "";
  return raw.replace(/\/$/, "");
}

/** Header facts folded from the agent transcript by the Node. */
export type SubagentMeta = {
  agentId: string;
  runId?: string | null;
  prompt?: string | null;
  model?: string | null;
  tokens?: number | null;
  calls?: number | null;
  latestTool?: string | null;
  startedAt?: string | null;
  endedAt?: string | null;
  finalText?: string | null;
};

export type SubagentTranscriptResponse = {
  /** False when the agent exists but its transcript has not landed yet. */
  available: boolean;
  meta?: SubagentMeta;
  events: Observation[];
};

async function getJson(path: string): Promise<unknown> {
  const session = readSession();
  const res = await fetch(`${hubBase()}${path}`, {
    credentials: "include",
    headers: {
      ...(session ? { "X-Remuda-Device-Id": session.deviceId } : {}),
    },
  });
  if (!res.ok) {
    const text = await res.text();
    let code = `HTTP_${res.status}`;
    let message = text || `HTTP ${res.status}`;
    try {
      const body = JSON.parse(text) as { code?: string; error?: string };
      if (body.code) code = body.code;
      if (body.error) message = body.error;
    } catch {
      /* raw */
    }
    throw new HubHttpError(res.status, code, message);
  }
  return res.json();
}

/**
 * Fetch one subagent's transcript. Never rejects on `available:false` — the
 * caller renders 「启动中」. A missing host/route surfaces as an error so the
 * row can distinguish "starting" from "cannot reach the host".
 */
export async function fetchSubagentTranscript(
  instanceId: Id,
  agentId: string,
): Promise<SubagentTranscriptResponse> {
  const raw = await getJson(
    `/v1/instances/${encodeURIComponent(instanceId)}/subagents/${encodeURIComponent(agentId)}/transcript`,
  );
  const rec = (raw ?? {}) as { available?: boolean; meta?: SubagentMeta; events?: unknown[] };
  if (!rec.available) return { available: false, events: [] };
  return {
    available: true,
    meta: rec.meta,
    events: coerceObservationList(rec.events ?? [], instanceId, instanceId),
  };
}
