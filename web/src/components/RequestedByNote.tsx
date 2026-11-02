import type { RequestedBy } from "../types/generated";
import {
  requestedByExcerpt,
  requestedByTagText,
  requestedByTitle,
} from "./requestedBy";

/** The owner's own words behind an Agent request; wording lives in `requestedBy.ts`. */
export function RequestedByNote({
  requestedBy,
}: {
  requestedBy?: RequestedBy | null;
}) {
  const excerpt = requestedByExcerpt(requestedBy);
  if (!requestedBy || !excerpt) return null;
  const command = requestedBy.commandId ? `（${requestedBy.commandId}）` : "";
  return (
    <p
      data-testid="requested-by"
      title={`你在这条输入里说的话${command}。仅作记录，不影响授权。`}
      style={{
        margin: "4px 0 0",
        fontSize: "var(--text-meta)",
        lineHeight: "var(--lh-meta)",
        color: "var(--fg-muted)",
        whiteSpace: "pre-wrap",
        overflowWrap: "anywhere",
      }}
    >
      你说：{excerpt}
    </p>
  );
}

/**
 * The "agent 自发" label for a document that records `requestedBy` but no
 * `rootTurn` (a task). `/approvals` keeps using `RootTurnTag`.
 */
export function RequestedByTag({
  requestedBy,
}: {
  requestedBy?: RequestedBy | null;
}) {
  const text = requestedByTagText(requestedBy);
  if (!text || !requestedBy) return null;
  return (
    <span
      data-testid="requested-by-tag"
      data-origin={requestedBy.origin ?? undefined}
      title={requestedByTitle(requestedBy)}
      style={{
        fontSize: "var(--text-meta)",
        lineHeight: "var(--lh-meta)",
        color: "var(--fg-muted)",
        border: "1px solid var(--border)",
        borderRadius: 999,
        padding: "0 6px",
        whiteSpace: "nowrap",
        flex: "none",
      }}
    >
      {text}
    </span>
  );
}
