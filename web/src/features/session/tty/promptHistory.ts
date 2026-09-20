import type { Observation } from "../../../types/observation";
import { readDraft } from "../../../lib/drafts";
import { assembleTranscript } from "../assemble";

/**
 * c-mkeybar (mobile-ui plan B.3.2 slot 7): the previous prompts of one
 * instance, newest first, for the 史 key. Sources are exactly the ones the
 * plan names: the unsent local draft (`lib/drafts.ts`) and the journal's
 * human `user` messages projected through the SAME assembler the transcript
 * uses — no second derivation. Skill bodies and hook-context records are
 * filed by producers as `user` with a non-human origin, and the assembler
 * already marks those (`node.origin !== "human"`), so they never look like
 * prompts the operator typed.
 */
export function promptHistory(instanceId: string, events: Observation[]): string[] {
  const prompts: string[] = [];
  const seen = new Set<string>();
  const push = (raw: string | undefined | null) => {
    const text = raw?.trim();
    if (!text || seen.has(text)) return;
    seen.add(text);
    prompts.push(text);
  };

  for (const node of assembleTranscript(events, [])) {
    if (node.type !== "message" || node.role !== "user" || node.origin !== "human") continue;
    push(node.text);
  }
  prompts.reverse();

  // The unsent composer draft is the most recent typed text, if present.
  const draft = readDraft(instanceId).trim();
  if (draft) prompts.unshift(draft);

  return prompts;
}
