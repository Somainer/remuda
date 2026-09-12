import type { UiStatus } from "../types/instance";
import ui from "../styles/ui.module.css";

const CLASS: Record<UiStatus, string> = {
  blocked: ui.dotBlocked,
  working: ui.dotWorking,
  starting: ui.dotStarting,
  idle: ui.dotIdle,
  exited: ui.dotExited,
  unknown: ui.dotUnknown,
};

export function StateDot({ status, title }: { status: UiStatus; title?: string }) {
  if (status === "exited") {
    return (
      <span className={`${ui.dot} ${ui.dotExited}`} title={title ?? status} aria-label={status}>
        ×
      </span>
    );
  }
  return <span className={`${ui.dot} ${CLASS[status]}`} title={title ?? status} aria-label={status} />;
}
