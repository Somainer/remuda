import { useState } from "react";
import type { QueuedCommand } from "../../lib/api";
import { HubHttpError } from "../../lib/httpError";
import css from "./queuedSends.module.css";
import { queuedText } from "./queuedText";

/**
 * cb-command-queue 「排队中」 (protocol.md §8.5): the caller's own sends to this
 * session that have not reached the harness — the Hub holds them (host
 * offline) or the Node does (the session is busy). Each can be edited (same
 * commandId, the Hub's revision check) or withdrawn (never delivered). A send
 * that went out meanwhile answers 409 and simply leaves the list.
 */
export function QueuedSends({
  rows,
  onEdit,
  onWithdraw,
}: {
  rows: readonly QueuedCommand[];
  onEdit: (row: QueuedCommand, text: string) => Promise<void>;
  onWithdraw: (row: QueuedCommand) => Promise<void>;
}) {
  if (rows.length === 0) return null;
  return (
    <section className={css.panel} data-testid="queued-sends" aria-label="排队中">
      <h3 className={css.head}>排队中 · {rows.length}</h3>
      <ul className={css.rows}>
        {rows.map((row) => (
          <li key={row.commandId}>
            <QueuedRow row={row} onEdit={onEdit} onWithdraw={onWithdraw} />
          </li>
        ))}
      </ul>
    </section>
  );
}

function failure(error: unknown): string {
  if (error instanceof HubHttpError) {
    if (error.code === "COMMAND_NOT_QUEUED") return "已送达，无法再修改";
    if (error.code === "COMMAND_REVISION_CONFLICT") return "已在别处被修改，已刷新";
  }
  return error instanceof Error ? error.message : String(error);
}

function QueuedRow({
  row,
  onEdit,
  onWithdraw,
}: {
  row: QueuedCommand;
  onEdit: (row: QueuedCommand, text: string) => Promise<void>;
  onWithdraw: (row: QueuedCommand) => Promise<void>;
}) {
  const text = queuedText(row.payload);
  const [draft, setDraft] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const where = row.held === "node" ? "等待本回合结束" : "主机离线，等待送达";
  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
      setDraft(null);
    } catch (err) {
      setError(failure(err));
    } finally {
      setBusy(false);
    }
  };
  if (draft !== null) {
    return (
      <div className={css.editor} data-testid="queued-send-editor">
        <textarea
          className={css.input}
          aria-label="编辑排队中的消息"
          value={draft}
          disabled={busy}
          onChange={(event) => setDraft(event.target.value)}
        />
        <div className={css.actions}>
          <button type="button" className={css.action} disabled={busy} onClick={() => setDraft(null)}>
            取消
          </button>
          <button
            type="button"
            className={css.save}
            disabled={busy || draft.trim() === "" || draft === text}
            onClick={() => void run(() => onEdit(row, draft))}
          >
            保存
          </button>
        </div>
        {error ? <span className={css.error} role="alert">{error}</span> : null}
      </div>
    );
  }
  return (
    <div className={css.row} data-testid="queued-send" data-command-id={row.commandId} data-held={row.held}>
      <span className={css.text} title={text}>
        {text}
      </span>
      <span className={css.where}>{where}</span>
      <button type="button" className={css.action} disabled={busy} onClick={() => setDraft(text)}>
        编辑
      </button>
      <button
        type="button"
        className={css.action}
        disabled={busy}
        onClick={() => void run(() => onWithdraw(row))}
      >
        撤回
      </button>
      {error ? <span className={css.error} role="alert">{error}</span> : null}
    </div>
  );
}
