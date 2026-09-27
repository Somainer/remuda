import { InboxShell } from "../features/approvals/InboxShell";

/**
 * Desktop route `/approvals`. The compact route bounces here into
 * `/m/inbox` at the router gate (D-049); both render the single InboxShell.
 */
export function ApprovalsPage() {
  return <InboxShell mode="desktop" />;
}
