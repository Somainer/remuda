import { jsonPreview } from "../../lib/format";
import css from "./toolCard.module.css";

export function OpaqueRow({ kind, summary, raw }: { kind: string; summary: string | null; raw: unknown }) {
  return (
    <details className={css.opaque} data-testid="opaque-row">
      <summary>
        未识别事件 · {kind}
        {summary ? ` · ${summary}` : ""}
      </summary>
      <pre className={css.stdout} data-testid="opaque-json">
        {jsonPreview(raw)}
      </pre>
    </details>
  );
}
