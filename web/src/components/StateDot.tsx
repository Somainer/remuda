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
  return <span className={`${ui.dot} ${CLASS[status]}`} title={title ?? status} aria-label={status} />;
}
