import type { DecisionOption, Interaction, InteractionAnswer } from "../../types/interaction";

/**
 * Build the {@link InteractionAnswer} an approval / plan-review option tap
 * submits. The single place that maps an option id to its wire answer, shared
 * by the desktop ApprovalCard and the phone inbox row — the payload shape
 * (inputDigest for approvals; planRevision/planDigest + null feedback for
 * plan reviews) must never be rebuilt differently in two surfaces.
 *
 * Returns null for interactions that have no options (questions /
 * elicitations); callers render those through their own cards.
 */
export function optionAnswerFor(
  interaction: Interaction,
  optionId: DecisionOption["id"],
): InteractionAnswer | null {
  const request = interaction.request;
  if (request.kind === "approval") {
    return { kind: "approval", optionId, inputDigest: request.inputDigest };
  }
  if (request.kind === "plan-review") {
    return {
      kind: "plan-review",
      optionId,
      planRevision: request.planRevision,
      planDigest: request.planDigest,
      feedback: null,
    };
  }
  return null;
}
