import { useEffect, useState } from "react";
import type { Interaction } from "../../types/interaction";
import { formatQuestionCountdown } from "./questionAlerts";

/**
 * Live deadline countdown for a pending question / elicitation / plan review
 * (c-question-alert). Renders "还剩 N 分钟，超时将自动拒绝" and ticks every
 * second; disappears at zero (the row then shows its expired state). Uses
 * `setInterval` rather than the shared inbox deadline clock so it updates the
 * second/minute text continuously; the expiry instant itself is the same
 * `interaction.deadline` the inbox projection uses.
 */
export function InteractionDeadline({
  interaction,
  testid = "interaction-deadline",
}: {
  interaction: Interaction;
  testid?: string;
}) {
  const [, force] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => force((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, []);
  const deadline =
    interaction.deadline.state === "known" ? interaction.deadline.value : null;
  const text = formatQuestionCountdown(deadline);
  if (!text) return null;
  return (
    <div data-testid={testid} data-deadline={deadline ?? ""}>
      {text}
    </div>
  );
}
