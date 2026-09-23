import type { UiStatus } from "../types/instance";
import ui from "../styles/ui.module.css";
import { CommitProbe } from "./CommitProbe";

const CLASS: Record<UiStatus, string> = {
  blocked: ui.dotBlocked,
  working: ui.dotWorking,
  starting: ui.dotStarting,
  idle: ui.dotIdle,
  exited: ui.dotExited,
  unknown: ui.dotUnknown,
};

/**
 * Status shapes read without colour: ⚠ blocked, ● working, ○ idle, ■ exited,
 * dashed ring unknown. None of them is an ×, which in this UI only ever means
 * "close tab" (D-024 addendum).
 */
const LABEL: Record<UiStatus, string> = {
  blocked: "待处理",
  working: "运行中",
  starting: "启动中",
  idle: "空闲",
  exited: "已退出",
  unknown: "状态未知",
};

export function StateDot({ status, title }: { status: UiStatus; title?: string }) {
  return (
    <CommitProbe name="StateDot">
      <span
        className={`${ui.dot} ${CLASS[status]}`}
        data-status={status}
        title={title ?? LABEL[status]}
        aria-label={LABEL[status]}
        role="img"
      >
        {status === "blocked" ? "⚠" : null}
      </span>
    </CommitProbe>
  );
}
