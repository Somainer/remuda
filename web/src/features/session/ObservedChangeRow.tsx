import { jsonPreview } from "../../lib/format";
import css from "./toolCard.module.css";

/**
 * c-mfix: `effort` and `model` are known observation kinds (protocol §5.1,
 * ObservationPayload::Effort/Model), not opaque events — the transcript used
 * to render them as 「未识别事件 · effort」. They render as a one-line change
 * record; the raw payload stays one disclosure away for provenance, exactly
 * like OpaqueRow. Genuinely unknown kinds still go through OpaqueRow (D-052).
 */
export function ObservedChangeRow({ kind, raw }: { kind: "model" | "effort"; raw: unknown }) {
  const isModel = kind === "model";
  const value = isModel ? modelValue(raw) : effortValue(raw);
  return (
    <div className={css.changeRow} data-testid="observed-change-row" data-kind={kind}>
      <span>
        ▸ {isModel ? "模型" : "档位"} → {value}
      </span>
      <details>
        <summary>原始事件</summary>
        <pre className={css.stdout} data-testid="observed-change-json">
          {jsonPreview(raw)}
        </pre>
      </details>
    </div>
  );
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null ? (value as Record<string, unknown>) : null;
}

function effectiveId(payload: unknown): string | null {
  const effective = asRecord(asRecord(payload)?.effective);
  const id = effective?.id;
  return typeof id === "string" && id ? id : null;
}

function modelValue(payload: unknown): string {
  const record = asRecord(payload);
  return effectiveId(record) ?? (typeof record?.raw === "string" ? record.raw : "—");
}

function effortValue(payload: unknown): string {
  const record = asRecord(payload);
  const effective = asRecord(record?.effective);
  const name = effective?.name;
  if (typeof name !== "string" || !name) return "—";
  // Claude reports ultracode sessions at level xhigh with the flag; the flag
  // is the part that distinguishes the row, so carry it on the line.
  return effective?.ultracode === true ? `${name} · ultracode` : name;
}
