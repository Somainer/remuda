import { useState } from "react";
import { formatListTime } from "../../lib/format";
import { nowMs } from "../../lib/clock";
import { noticeVoice, type OwnerNotice } from "../home/notices";
import css from "./ownerNotice.module.css";

/**
 * D-060 §11.5 「告诉你」 in Main's transcript (cb-owner-notify): one card per
 * `owner notify` notice under the reply of the turn that wrote it. It speaks
 * in Main's voice — a labelled card, never the owner's bubble — with the
 * whole body. 知道了 acknowledges it on every device (the home ④ card leaves
 * too); an acknowledged card stays in the conversation, marked 已确认.
 */
export function OwnerNoticeCards({
  notices,
  onAck,
}: {
  notices: readonly OwnerNotice[];
  onAck?: (noticeId: string) => Promise<void>;
}) {
  return (
    <ul className={css.notices} data-testid="transcript-notices">
      {notices.map((notice) => (
        <li key={notice.noticeId}>
          <NoticeCard notice={notice} onAck={onAck} />
        </li>
      ))}
    </ul>
  );
}

function NoticeCard({ notice, onAck }: { notice: OwnerNotice; onAck?: (noticeId: string) => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const acked = Boolean(notice.ackedAt);
  return (
    <article
      className={css.notice}
      data-testid="transcript-notice"
      data-notice-id={notice.noticeId}
      data-acked={acked ? "1" : "0"}
      aria-label={`${noticeVoice(notice)} 告诉你：${notice.title}`}
    >
      <header className={css.head}>
        <span className={css.voice}>{noticeVoice(notice)} 告诉你</span>
        <span className={css.time}>{formatListTime(notice.createdAt, nowMs())}</span>
      </header>
      <p className={css.title}>{notice.title}</p>
      <p className={css.body}>{notice.body}</p>
      <div className={css.actions}>
        {acked ? (
          <span className={css.done} data-testid="transcript-notice-acked">
            已确认
          </span>
        ) : onAck ? (
          <button
            type="button"
            className={css.ack}
            data-testid="transcript-notice-ack"
            disabled={busy}
            onClick={() => {
              setBusy(true);
              setError(null);
              onAck(notice.noticeId).then(
                () => setBusy(false),
                (reason: unknown) => {
                  setBusy(false);
                  setError(reason instanceof Error ? reason.message : String(reason));
                },
              );
            }}
          >
            知道了
          </button>
        ) : null}
        {error ? (
          <span className={css.error} role="alert">
            {error}
          </span>
        ) : null}
      </div>
    </article>
  );
}
