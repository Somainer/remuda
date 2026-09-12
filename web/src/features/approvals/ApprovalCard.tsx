import type { Interaction, InteractionAnswer } from "../../types/interaction";
import css from "../session/session.module.css";

export function ApprovalCard({
  interaction,
  busy,
  onRespond,
}: {
  interaction: Interaction;
  busy?: boolean;
  onRespond: (answer: InteractionAnswer) => void;
}) {
  const req = interaction.request;
  if (req.kind !== "approval") return null;
  const disabled = busy || !interaction.answerable || interaction.state !== "pending";
  return (
    <section className={css.approval} data-testid="approval-card">
      <div className={css.approvalHead}>
        <span className={css.dustDot} />
        <span className={css.approvalTitle}>审批 · {req.title}</span>
        <span className={css.approvalHint}>{interaction.id.slice(0, 12)}</span>
        <span className={css.spacer} />
        <span className={css.approvalHint}>多台设备同时点，只记第一次</span>
      </div>
      <div className={css.approvalBody}>
        <p className={css.preview}>{req.description}</p>
        {req.options.map((opt) => {
          const kind = opt.effect === "deny" ? css.denyBtn : opt.effect === "allow-once" ? css.allowBtn : css.quietBtn;
          return (
            <button
              key={opt.id}
              type="button"
              className={kind}
              disabled={disabled}
              onClick={() => onRespond({ kind: "approval", optionId: opt.id, inputDigest: req.inputDigest })}
            >
              {opt.label}
            </button>
          );
        })}
      </div>
    </section>
  );
}
