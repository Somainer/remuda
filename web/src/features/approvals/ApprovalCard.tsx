import type { Interaction, InteractionAnswer } from "../../types/interaction";
import { Button } from "../../components/Button";
import ui from "../../styles/ui.module.css";

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
    <section className={ui.approval}>
      <div className={ui.cardHead}>
        <strong>审批 · {req.title}</strong>
        <span>{interaction.state}</span>
      </div>
      <p style={{ margin: "0 0 8px" }}>{req.description}</p>
      <p className={ui.listMeta}>多台设备同时点，只记第一次。</p>
      <div className={ui.row}>
        {req.options.map((opt) => (
          <Button
            key={opt.id}
            variant={opt.effect === "deny" ? "danger" : "primary"}
            disabled={disabled}
            onClick={() => onRespond({ kind: "approval", optionId: opt.id, inputDigest: req.inputDigest })}
          >
            {opt.label}
          </Button>
        ))}
      </div>
    </section>
  );
}
