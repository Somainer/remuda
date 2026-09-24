import { useState } from "react";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import css from "./decision.module.css";

type ElicitationAction = "accept" | "decline" | "cancel";

/**
 * Plain hook `Elicitation` (MCP form / url). The hook reply shape is
 * `{hookSpecificOutput:{hookEventName:"Elicitation",action,content}}`; this
 * card offers the three distinct actions the server distinguishes, with a
 * free-form JSON payload for accept (the schema itself is not carried in
 * tier A, so an opaque JSON/JSONL text entry is the honest option).
 */
export function ElicitationCard({
  interaction,
  busy,
  onRespond,
}: {
  interaction: Interaction;
  busy?: boolean;
  onRespond: (answer: InteractionAnswer) => void;
}) {
  const [raw, setRaw] = useState("");
  const [error, setError] = useState<string | null>(null);
  const req = interaction.request;
  if (req.kind !== "elicitation") return null;
  const disabled = busy || !interaction.answerable || interaction.state !== "pending";

  const send = (action: ElicitationAction) => {
    if (action !== "accept") {
      onRespond({ kind: "elicitation", action, content: null });
      return;
    }
    const text = raw.trim();
    if (!text) {
      onRespond({ kind: "elicitation", action, content: null });
      return;
    }
    try {
      onRespond({ kind: "elicitation", action, content: JSON.parse(text) });
      setError(null);
    } catch {
      setError("内容必须是合法 JSON");
    }
  };

  return (
    <section className={css.question} data-testid="elicitation-card">
      <div className={css.approvalHead} style={{ padding: 0, borderBottom: 0 }}>
        <span className={css.dustDot} />
        <span className={css.approvalTitle}>表单请求 · {req.title}</span>
        <span className={css.spacer} />
        <span className={css.approvalHint}>{interaction.id.slice(0, 12)}</span>
      </div>
      {req.mode === "url" && req.url ? (
        <p className={css.qTitle}>
          <a href={req.url} target="_blank" rel="noreferrer">
            {req.url}
          </a>
        </p>
      ) : null}
      <textarea
        className={css.input}
        rows={4}
        disabled={disabled}
        placeholder='应答内容（JSON，可留空），例如 {"account":"ada"}'
        value={raw}
        onChange={(event) => setRaw(event.target.value)}
      />
      {error ? <span className={css.approvalHint}>{error}</span> : null}
      <div className={css.qRow}>
        <button type="button" className={css.allowBtn} disabled={disabled} onClick={() => send("accept")}>
          Accept
        </button>
        <span className={css.spacer} />
        <button type="button" className={css.quietBtn} disabled={disabled} onClick={() => send("decline")}>
          Decline
        </button>
        <button type="button" className={css.quietBtn} disabled={disabled} onClick={() => send("cancel")}>
          Cancel
        </button>
      </div>
    </section>
  );
}
