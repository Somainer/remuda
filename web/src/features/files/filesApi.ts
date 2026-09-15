//! Minimal read-only client for the workspace-changes proxy. Kept separate from
//! the cached store (`store.ts`) because the contract requires every open to
//! re-collect live with no caching (files-view-contract §3.1, §3.6).

import { HubHttpError } from "../../lib/httpError";
import { readSession } from "../../lib/session";
import type { ScmDiff, ScmFile, ScmStatus } from "../../types/scm";

function hubBase(): string {
  if (import.meta.env.DEV && import.meta.env.VITE_HUB_URL) return "";
  const raw = import.meta.env.VITE_API_BASE ?? import.meta.env.VITE_HUB_URL ?? "";
  return raw.replace(/\/$/, "");
}

async function getJson<T>(path: string): Promise<T> {
  const session = readSession();
  const res = await fetch(`${hubBase()}${path}`, {
    credentials: "include",
    headers: {
      "content-type": "application/json",
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
  return (await res.json()) as T;
}

const changesBase = (hostId: string, workspaceId: string) =>
  `/v1/hosts/${encodeURIComponent(hostId)}/workspaces/${encodeURIComponent(workspaceId)}/changes`;

/** Fetch live `workspace.scm.status`. */
export function fetchChanges(hostId: string, workspaceId: string): Promise<ScmStatus> {
  return getJson<ScmStatus>(changesBase(hostId, workspaceId));
}

/** Fetch one entry's unified diff (staged or unstaged). */
export function fetchEntryDiff(
  hostId: string,
  workspaceId: string,
  path: string,
  staged: boolean,
): Promise<ScmDiff> {
  const query = `?path=${encodeURIComponent(path)}&staged=${staged ? "true" : "false"}`;
  return getJson<ScmDiff>(`${changesBase(hostId, workspaceId)}/diff${query}`);
}

/** Fetch restricted current bytes for an untracked/new file. */
export function fetchEntryFile(
  hostId: string,
  workspaceId: string,
  path: string,
): Promise<ScmFile> {
  return getJson<ScmFile>(
    `${changesBase(hostId, workspaceId)}/file?path=${encodeURIComponent(path)}`,
  );
}
