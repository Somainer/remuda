import type { UsagePayload } from "../../types/generated";
import { usageLine } from "./usage";
import css from "./session.module.css";

export function UsageFooter({ payload }: { payload: UsagePayload }) {
  const line = usageLine(payload);
  if (!line) return null;
  return (
    <div className={css.usage} data-testid="usage-row">
      {line}
    </div>
  );
}
