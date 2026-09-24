import { InboxShell } from "../approvals/InboxShell";

/**
 * Compact route `/m/inbox` (D-049, ui-spec §2.5/§4.7). PhoneShell only mounts
 * it behind the viewport gate; the card list, kind radiogroup and derivation
 * are the single InboxShell shared with `/approvals`.
 */
export function Inbox() {
  return <InboxShell mode="compact" />;
}
