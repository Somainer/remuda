import type { WorkflowMemberPayload } from "../../types/generated";

export function memberChildInstanceId(member: WorkflowMemberPayload): string | null {
  const value = member.childInstanceId;
  return typeof value === "string" && value.length > 0 ? value : null;
}
