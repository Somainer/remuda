import ui from "../styles/ui.module.css";
import type { ConnectionUi } from "../lib/store";
import { CommitProbe } from "./CommitProbe";

/**
 * Live is calm (a muted dot), not a success colour; reconnecting is the only
 * moving state and keeps moving under reduced motion, since the motion is the
 * message.
 */
export function ConnectionIndicator({ status }: { status: ConnectionUi | "gap-backfill" | "readonly-stale" | "live" | "reconnecting" }) {
  return (
    <CommitProbe name="ConnectionIndicator">
      <ConnectionBody status={status} />
    </CommitProbe>
  );
}

function ConnectionBody({ status }: { status: ConnectionUi | "gap-backfill" | "readonly-stale" | "live" | "reconnecting" }) {
  if (status === "live") {
    return (
      <span className={ui.conn}>
        <span className={ui.connLive} />
        live
      </span>
    );
  }
  if (status === "reconnecting" || status === "gap-backfill") {
    return (
      <span className={ui.conn}>
        <span className={ui.connDots} aria-hidden>
          <span className={`${ui.connDot} conn-dot`} data-motion="essential" />
          <span className={`${ui.connDot} conn-dot`} data-motion="essential" />
          <span className={`${ui.connDot} conn-dot`} data-motion="essential" />
        </span>
        {status === "gap-backfill" ? "补事件" : "重连"}
      </span>
    );
  }
  return (
    <span className={ui.conn}>
      <span className={ui.connOff} />
      {status === "readonly-stale" ? "只读" : "离线"}
    </span>
  );
}
