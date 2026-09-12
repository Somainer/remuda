import { jsonPreview } from "../../lib/format";
import ui from "../../styles/ui.module.css";

export function OpaqueRow({ kind, summary, raw }: { kind: string; summary: string | null; raw: unknown }) {
  return (
    <details className={ui.listMeta} data-testid="opaque-row">
      <summary>
        未识别事件 · {kind}
        {summary ? ` · ${summary}` : ""}
      </summary>
      <pre className={ui.pre} data-testid="opaque-json">
        {jsonPreview(raw)}
      </pre>
    </details>
  );
}
