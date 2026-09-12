import type { UsagePayload } from "../../types/generated";
import { formatTokens } from "../../lib/format";

/** Hide the whole row when input or output is not known (ui-spec §2.2). */
export function usageLine(payload: UsagePayload): string | null {
  const input = formatTokens(payload.inputTokens);
  const output = formatTokens(payload.outputTokens);
  if (!input || !output) return null;
  const cost = payload.cost.state === "known" ? ` · $${payload.cost.value.amount}` : "";
  return `usage · in ${input} / out ${output}${cost}`;
}
