/**
 * Per-workflow card dismissal (c-wfcard).
 *
 * A dismissed workflow card collapses into its turn's compact fold; an
 * undismissed one always stays a visible live card, running or finished
 * (assemble.ts `compactTranscript`). Dismissal is per workflow id, scoped to
 * the session, and persisted in the existing `runtime.*` localStorage
 * namespace alongside readingPosition.ts (`runtime.reading.v1.`) and the
 * compact preference (`runtime.compact`). Only opaque workflow ids are
 * stored — no prompt text, no journal content.
 */

const KEY_PREFIX = "runtime.workflow-dismiss.v1.";

/** Read the dismissed-workflow id set saved for one session instance. */
export function readDismissedWorkflows(instanceId: string): Set<string> {
  try {
    const raw = localStorage.getItem(KEY_PREFIX + instanceId);
    if (!raw) return new Set();
    const value = JSON.parse(raw) as unknown;
    if (!Array.isArray(value)) return new Set();
    return new Set(value.filter((id): id is string => typeof id === "string"));
  } catch {
    return new Set();
  }
}

function write(instanceId: string, ids: Set<string>): void {
  try {
    if (ids.size === 0) localStorage.removeItem(KEY_PREFIX + instanceId);
    else localStorage.setItem(KEY_PREFIX + instanceId, JSON.stringify([...ids]));
  } catch {
    /* private mode / quota: dismissal simply does not persist */
  }
}

export function isWorkflowDismissed(instanceId: string, workflowId: string): boolean {
  return readDismissedWorkflows(instanceId).has(workflowId);
}

/** Persist one workflow's dismissal; returns the resulting set. */
export function dismissWorkflow(instanceId: string, workflowId: string): Set<string> {
  const ids = readDismissedWorkflows(instanceId);
  ids.add(workflowId);
  write(instanceId, ids);
  return ids;
}

/** Lift one workflow's dismissal; returns the resulting set. */
export function undismissWorkflow(instanceId: string, workflowId: string): Set<string> {
  const ids = readDismissedWorkflows(instanceId);
  ids.delete(workflowId);
  write(instanceId, ids);
  return ids;
}
