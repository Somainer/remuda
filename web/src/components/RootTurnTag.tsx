import type { RootTurn } from "../types/generated";
import { type RootTurnSurface, rootTurnTagText, rootTurnTitle } from "./rootTurnTag";

/** The cb-rootturn provenance tag; wording and rule live in `rootTurnTag.ts`. */
export function RootTurnTag({
  rootTurn,
  surface,
  testId = "root-turn-tag",
}: {
  rootTurn?: RootTurn | null;
  surface: RootTurnSurface;
  testId?: string;
}) {
  const text = rootTurnTagText(rootTurn, surface);
  if (!text || !rootTurn) return null;
  return (
    <span
      data-testid={testId}
      data-origin={rootTurn.origin}
      data-continuation={rootTurn.continuation}
      title={rootTurnTitle(rootTurn)}
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
