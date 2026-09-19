import type { Observation } from "../../types/observation";
import type { Id } from "../../types/wire";
import { observationText } from "../../lib/api";
import { shortId } from "../../lib/format";

/**
 * The live-run phrase for a working list row (ui-spec.md D-038, §2.1
 * wireframe: `Workflow wf_ab12 · phase compile`). It is a *projection* of
 * journal observations already on the client — no new state, no inference:
 *
 *  - the newest running `workflow.run` plus the newest running phase label of
 *    that same workflow reads as `Workflow <native run id> · phase <label>`;
 *    the native run id is shown in full (the row's sentence ellipsizes, and
 *    the 8-char short-code rule applies to `ins_` instance ids, not to the
 *    phrase), falling back to a short workflow id when it was not observed;
 *  - otherwise the newest assistant message becomes the phrase (one line);
 *  - with neither, the row falls back to the constant `运行中…`.
 *
 * Events are expected most-recent-last (the journal ordering).
 */
export type LiveRun = { workflowId: string; nativeRunId?: string } | undefined;

export function liveSummary(events: Observation[]): string | undefined {
  let run: LiveRun;
  let phaseLabel: string | undefined;

  for (const event of events) {
    if (event.kind === "workflow.run") {
      const payload = event.payload as {
        workflowId?: string;
        state?: string;
        nativeRunId?: { state?: string; value?: string };
      };
      // A workflow that already finished no longer describes what the row is
      // doing right now; drop it so an assistant tail can take over.
      const state = payload.state;
      if (state === "completed" || state === "failed" || state === "cancelled") {
        run = undefined;
        phaseLabel = undefined;
        continue;
      }
      run = {
        workflowId: payload.workflowId ?? "",
        nativeRunId: payload.nativeRunId?.state === "known" ? payload.nativeRunId.value : undefined,
      };
      continue;
    }
    if (event.kind === "workflow.phase") {
      const payload = event.payload as {
        workflowId?: string;
        state?: string;
        label?: { state?: string; value?: string };
      };
      // The phrase names the phase the run is *currently in*: a queued phase
      // that follows the running one is what is next, not what is happening.
      if (run && payload.workflowId === run.workflowId && payload.state === "running") {
        phaseLabel = payload.label?.state === "known" ? payload.label.value : undefined;
      }
    }
  }

  if (run) {
    const id = run.nativeRunId || shortId(run.workflowId as Id, 8);
    return phaseLabel ? `Workflow ${id} · phase ${phaseLabel}` : `Workflow ${id}`;
  }

  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    if (event.kind !== "message") continue;
    const payload = event.payload as { role?: string };
    if (payload.role !== "assistant") continue;
    const text = observationText(event).replace(/\s+/g, " ").trim();
    if (text) return text;
  }

  return undefined;
}
