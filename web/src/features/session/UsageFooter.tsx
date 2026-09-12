import type { UsagePayload } from "../../types/generated";
import ui from "../../styles/ui.module.css";
import { usageLine } from "./usage";

export function UsageFooter({ payload }: { payload: UsagePayload }) {
  const line = usageLine(payload);
  if (!line) return null;
  return (
    <div className={ui.usage} data-testid="usage-row">
      {line}
    </div>
  );
}
